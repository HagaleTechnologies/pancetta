# Upgrade and rollback

Use this guide for archive installations on Linux, macOS and Windows. No Rust or source checkout is required. First installs can use README's Quick start. Upgrades require a verified offline backup before replacing or starting a binary.

## 1. Stop and inventory the station

Stop Pancetta, logbook imports and every other writer to its files. Disable any service or scheduled task that restarts it. Confirm it has stopped before copying. Filesystem copying is not an online SQLite backup.

Privately record the old version, extracted installation directory, startup command, working directory, environment settings and every effective configuration source. Run the installed binary's `config --validate` if available and note its loaded-file report. If unavailable, consult [CONFIG's search table](CONFIG.md#where-pancetta-looks-for-its-config). Include explicit `--config` paths and external station-agent key directories where configured. Config discovery depends on the working directory. Preserve the source paths and load order; do not silently collapse layered config into one file. Do not publish config dumps, credentials or this inventory.

## 2. Make and verify a private backup

Choose a new backup directory outside both the installation and data directories. Allow space for the complete installation and data, plus a second snapshot if rollback becomes necessary. Copy the entire extracted old version directory, all of `~/.pancetta` and every external effective config file and key directory. Include hidden files, original ADIF, legacy database, SQLite WAL/SHM sidecars, import receipts and backups. An exported ADIF or automatic database backup alone is insufficient.

The following examples assume ordinary files and directories, with one explicit config. For multiple config sources, repeat the external config copy and comparison using distinct backup names such as `config-2.toml`, and record each source-to-backup mapping and load order in the private inventory. For external key directories, repeat the whole-directory copy and comparison. Inventory symbolic links, Windows junctions and their targets first. Preserve external targets separately, or copy their contents and record their original paths. A matching link alone does not back up its target. Do not call the snapshot verified until all targets are covered.

### Linux and macOS

Set actual absolute paths first, then run the whole block. The subshell stops on the first failed operation.

```sh
# Set these to your actual absolute paths before running.
OLD_DIR="$HOME/pancetta-old-version"
DATA_DIR="$HOME/.pancetta"
CONFIG_FILE="$HOME/station-config/pancetta.toml"
BACKUP="$HOME/pancetta-before-upgrade"
(
  set -eu
  umask 077
  mkdir "$BACKUP"
  cp -Rp "$OLD_DIR" "$BACKUP/install"
  cp -Rp "$DATA_DIR" "$BACKUP/data"
  cp -p "$CONFIG_FILE" "$BACKUP/config.toml"
  diff -r "$OLD_DIR" "$BACKUP/install"
  diff -r "$DATA_DIR" "$BACKUP/data"
  cmp "$CONFIG_FILE" "$BACKUP/config.toml"
)
```

Success produces no output and exits 0. Any missing source, read error, failed comparison or existing destination means stop. A partial directory is not a verified backup. Do not launch the new binary after a failed backup. The private directory is created with mode 0700 by `umask 077`.

The normal upgrade recipe expects existing data and fails if it is missing. Only for a genuinely unused installation, confirm that the directory does not exist and that there is no logbook at another configured location. Record that fact in the private inventory and omit the data copy and data comparison together. Permission errors or unreadable data are not evidence of absence.

### Windows PowerShell

Use an NTFS destination where a private ACL can be applied. Run the whole block, with your actual absolute paths. Errors terminate the recipe; do not proceed after an error. It removes inherited access and grants only your user and SYSTEM access to the backup directory. Check the destination's access before adding files.

```powershell
$OldDir = "$HOME\pancetta-old-version"
$DataDir = "$HOME\.pancetta"
$ConfigFile = "$HOME\station-config\pancetta.toml"
$Backup = "$HOME\pancetta-before-upgrade"
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $Backup) { throw 'Backup destination already exists' }
New-Item -ItemType Directory -Path $Backup | Out-Null
$Sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
& icacls.exe $Backup /inheritance:r /grant:r "*${Sid}:(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not protect backup directory' }
Copy-Item -LiteralPath $OldDir -Destination "$Backup\install" -Recurse -Force
Copy-Item -LiteralPath $DataDir -Destination "$Backup\data" -Recurse -Force
Copy-Item -LiteralPath $ConfigFile -Destination "$Backup\config.toml" -Force
function Compare-Tree($Source, $Copy) {
    $Root = (Get-Item -LiteralPath $Source).FullName.TrimEnd('\')
    $Files = @(Get-ChildItem -LiteralPath $Root -Recurse -Force -File)
    $Copied = @(Get-ChildItem -LiteralPath $Copy -Recurse -Force -File)
    if ($Files.Count -ne $Copied.Count) { throw 'Backup file count differs' }
    foreach ($File in $Files) {
        $Relative = $File.FullName.Substring($Root.Length + 1)
        $Other = Join-Path $Copy $Relative
        if (!(Test-Path -LiteralPath $Other -PathType Leaf)) { throw "Missing copy: $Relative" }
        if ($File.Length -ne (Get-Item -LiteralPath $Other).Length) { throw "Size differs: $Relative" }
        $A = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash
        $B = (Get-FileHash -LiteralPath $Other -Algorithm SHA256).Hash
        if ($A -ne $B) { throw "Hash differs: $Relative" }
    }
}
Compare-Tree $OldDir "$Backup\install"
Compare-Tree $DataDir "$Backup\data"
$A = (Get-FileHash -LiteralPath $ConfigFile -Algorithm SHA256).Hash
$B = (Get-FileHash -LiteralPath "$Backup\config.toml" -Algorithm SHA256).Hash
if ($A -ne $B) { throw 'Config hash differs' }
```

Success produces no output. Any thrown error means stop. A partially copied directory is not verified. The missing-data exception and multi-config/link inventory rules above apply here too. Protect the later recovery snapshot with the same private ACL.

## 3. Verify and extract the new release

Download the archive and its matching `.sha256` sidecar from the same GitHub release. Keep the old installation directory. Extract to a separate, version-named directory only after the backup passes. Do not overwrite the old directory.

In Linux, replace the example filename with the exact downloaded archive name:

```sh
sha256sum -c pancetta-vVERSION-x86_64-unknown-linux-gnu.tar.gz.sha256
```

On macOS:

```sh
shasum -a 256 -c pancetta-vVERSION-aarch64-apple-darwin.tar.gz.sha256
```

In PowerShell, verify the ZIP before extracting:

```powershell
$ErrorActionPreference = 'Stop'
$Archive = 'pancetta-vVERSION-x86_64-pc-windows-gnu.zip'
$Expected = ((Get-Content -LiteralPath "$Archive.sha256" -Raw).Trim() -split '\s+')[0]
if ($Expected -notmatch '^[0-9a-fA-F]{64}$') { throw 'Invalid SHA-256 sidecar' }
$Actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash
if ($Actual -ne $Expected) { throw 'Archive checksum mismatch' }
```

Every release also carries a single `SHA256SUMS` manifest listing all platform
archives. Download it from the same release. To verify a partial download with
only your platform's archive present, add `--ignore-missing`:

```sh
sha256sum -c --ignore-missing SHA256SUMS        # Linux
shasum -a 256 -c --ignore-missing SHA256SUMS    # macOS
```

Each archive also carries a GitHub build-provenance attestation, verifiable
with the GitHub CLI:

```sh
gh attestation verify pancetta-vVERSION-x86_64-unknown-linux-gnu.tar.gz --repo HagaleTechnologies/pancetta
```

For crash triage, every release attaches the split debug symbols for each
target (`*.debug` for Linux and Windows, `*.dSYM.tar.gz` for macOS), matching
the stripped binary inside the archive.

A failed checksum stops the upgrade. Checksums detect a mismatched download; they are not code signing. macOS archives remain unnotarized. For a trusted browser download, macOS may require `xattr -d com.apple.quarantine /absolute/path/to/new/pancetta` before launch, as in README. Linux ARM64 requires glibc >= 2.35, such as Raspberry Pi OS Bookworm or newer.

Extract a tar archive with `tar xzf <archive.tar.gz>`, or use Windows Extract All for the ZIP, into a location that does not contain the old version. Keep the original startup working directory and config sources. For an explicit config, run the new executable by its full path with `--config /absolute/path/to/pancetta.toml config --validate`. For layered discovery, run its full path with `config --validate` from the recorded working directory with the recorded environment. An explicit config is suitable only if it reproduces the intended effective configuration.

## 4. Check the upgrade before transmitting

Validate with the new binary before starting the station. Stop on validation errors. On startup, read migration messages and compare the old and new logbook, including known callsigns, dates and contact details. A migration warning means stop and follow recovery. Avoid transmitting until station settings and logbook checks pass.

Keep a private recovery checklist:

- Old/new versions and installation paths.
- Startup working directory, command, environment and ordered config paths.
- Backup location, copied external targets and successful comparison results.
- Old/new config validation results and loaded sources.
- Migration messages or warnings and logbook comparison results.
- Whether any contacts were logged after upgrade, and whether reconciliation is complete.

This is an operator record, not an automatic migration report.

## Rollback

> Never overwrite your current data directory with an old backup. First stop all writers and make a separate, verified post-upgrade snapshot. Contacts made after the upgrade are absent from the pre-upgrade snapshot.

1. Stop Pancetta, services, imports and all other writers again. Repeat the complete backup procedure with the current installation and changed configs, using a **new** private destination, for example `pancetta-after-upgrade`. Verify it before restoring anything. Preserve original ADIF, database, SQLite sidecars, receipts and backups. Do not substitute `pancetta export`; export regenerates data from the index.
2. Restore the prior version directory from the verified pre-upgrade backup into an unoccupied destination. Move the current data directory aside without deleting it, then copy the pre-upgrade `data` directory back to its original path. Restore every prior config source and external key directory using the inventory. Preserve current external configs in the post-upgrade snapshot before replacing them. Use `cp -Rp` and `diff -r` on POSIX, or `Copy-Item -Recurse -Force` and `Compare-Tree` from the PowerShell recipe, to copy and verify each restored directory. Use `cmp` or SHA-256 to verify restored config files. Refuse occupied restore destinations; never merge a snapshot into live data.
3. Use the prior startup working directory, environment and arguments so discovery does not change. With no post-upgrade contacts, verified restoration returns the prior binary, config and logbook. Validate before restarting. Retain both snapshots and the moved current directory.
4. If contacts were made after upgrade, mark reconciliation incomplete. Do not resume logging with the older binary until those contacts are accounted for. Restoring the old binary does not itself make later contacts visible in its index. No automatic reverse migration is promised.

### Reconcile later contacts

v0.9.6 has **no `import` command**. An older database-only release is not known to rebuild its index from a newer ADIF. Never delete its database to force a rebuild.

Only for a version that actually provides import, inspect `pancetta import --help`. Preview the preserved post-upgrade ADIF with `pancetta import <file> --dry-run`, review unreadable-record and duplicate counts, then import and inspect the result. Whole-file import can suppress nearby contacts through duplicate rules. Record counts alone do not prove completeness; check the individual later contacts against their original records.

For v0.9.6 or an unverified format boundary, retain both complete snapshots. Use a compatible logger or a version with supported import to reconcile portable contacts before resuming station logging. Keep database-only contacts in their original snapshot until a compatible version can export them. A rollback is not fully reconciled while later contacts exist only in a recovery copy. Never prune either snapshot automatically.
