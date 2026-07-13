# Onboarding Phase 3: Releases, Discoverability, and Presentation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A stranger lands on `github.com/HagaleTechnologies/pancetta` and sees a real project — badges, a screenshot slot, topics, a working license readout — and can download a prebuilt binary for macOS/Linux/Windows that provably embeds the real `ft8_lib` C decoder, tagged `v0.9.5`. This is the "true 5-minute install" leg of the onboarding spec (`docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md`, Phase 3).

**Architecture:** Eight tasks. Tasks 1–6 are repo changes on one branch → one PR (a small `pancetta info` code change, a new `release.yml` workflow, CHANGELOG/README/license/metadata/hygiene edits). Task 7 is GitHub-metadata-only (`gh api`, no commits). Task 8 is **operator-gated**: tag `v0.9.5` on the post-merge main tip, watch the release build, publish the draft release. `cargo-dist` was evaluated and rejected (verified alive and maintained as of 2026 — v0.32, releases through Feb 2026 — but pancetta's Windows binary **must** build with the MinGW/GNU toolchain because MSVC cannot compile `ft8_lib`'s VLAs, per the 2026-05-23 note in `ci.yml:174-181`, and we need a custom non-stub smoke gate; a ~150-line hand-rolled matrix in the existing house style keeps both under direct control).

**Tech Stack:** GitHub Actions (house style from `.github/workflows/ci.yml`: `actions/checkout@v7` + `submodules: recursive`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`, explicit apt deps, heavy comments on engineering decisions), `gh` CLI, shields.io.

## Global Constraints

- **The release workflow must never build `pancetta-research`.** Achieved structurally: it builds `-p pancetta` only (never `--workspace`). Verify with `grep -c "workspace" .github/workflows/release.yml` → only in comments, if at all.
- **Binaries must embed the REAL `ft8_lib`.** Three independent gates in the workflow: `submodules: recursive` checkout, a hard file-presence guard on `pancetta-ft8/vendor/ft8_lib/ft8/constants.c` (the exact file `pancetta-ft8/build.rs:9` keys the stub fallback on), and a smoke step that runs the built binary's `info` subcommand and fails unless it prints `ft8_lib C decoder: native-C` (Task 1 adds that line; today `pancetta info` doesn't reveal stub state — verified in `pancetta/src/main.rs` `info_command()` ~line 528).
- **Tagging and publishing are OPERATOR-GATED** (Task 8). The workflow creates a *draft* release; only the operator publishes. No tag is pushed without explicit operator approval.
- **No crates.io publishing.** `[workspace.package] publish = false` (root `Cargo.toml:42`) stays untouched; Task 5 fills metadata so it remains a choice, not a blocker.
- Windows release target is `x86_64-pc-windows-gnu`, never MSVC (VLA incompatibility, `ci.yml:174-181`; the on-rig MiniPC builds with MinGW already).
- K5ARH → N0CALL swaps only in test fixtures/plan files; docs describing real on-air sessions (`docs/qso-engine-bugs.md`, `docs/operations/*`, security reviews, `docs/fcc-part97-compliance.md`) are left alone.
- All commits run the standard local gate: `cargo fmt --all` + `cargo clippy --workspace --features transmit` clean before each commit. Repo commit-message conventions (`feat:`/`fix:`/`docs:`/`chore:`). Main moves only by PR merge.
- Line numbers below are from current `origin/main`; if Phase 1's README edits have landed, locate anchors by heading, not line number.

---

### Task 1: `pancetta info` reveals decoder-engine state + the release workflow

**Files:**
- Modify: `pancetta/src/main.rs` (`info_command()`, ~line 528)
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `pancetta_ft8::ft8lib_is_available() -> bool` (re-exported at `pancetta-ft8/src/lib.rs:131`; already a dependency of the `pancetta` package). Same runtime seam Phase 1 Task 2 uses.
- Produces: a stable, greppable line `ft8_lib C decoder: native-C` in `pancetta info` output — the release smoke step, and later Phase 4's `pancetta doctor`, key on this exact string. `info` runs no audio/config init (verified: it only prints), so it is CI-safe on a bare runner.

- [ ] **Step 1: Add the decoder-engine line to `info_command()`**

In `pancetta/src/main.rs`, `info_command()` currently prints a `Components:` block with only `pancetta-dsp`. Extend it:

```rust
    // Component versions
    println!("Components:");
    println!("  pancetta-dsp: {}", pancetta_dsp::VERSION);
    println!(
        "  ft8_lib C decoder: {}",
        if pancetta_ft8::ft8lib_is_available() {
            "native-C"
        } else {
            "STUB (pure-Rust only — degraded decode recall; fix: git submodule update --init, then rebuild)"
        }
    );
```

- [ ] **Step 2: Verify locally**

```bash
cargo run --release -p pancetta -- info | grep "ft8_lib C decoder"
```

Expected (this worktree has the submodule initialized — `pancetta-ft8/vendor/ft8_lib/ft8/constants.c` exists): `  ft8_lib C decoder: native-C`.

- [ ] **Step 3: Confirm the files the packaging step will ship all exist**

```bash
ls README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md
```

Expected: all five listed. If `THIRD-PARTY-NOTICES.md` is missing, drop it from the `cp` line in the workflow below.

- [ ] **Step 4: Write `.github/workflows/release.yml`**

Full file content:

```yaml
name: Release

# Builds prebuilt binaries on every v* tag push and attaches them to a DRAFT
# GitHub Release. The operator publishes the draft — tags and releases are
# operator-gated (docs/superpowers/plans/2026-07-13-onboarding-phase3-releases.md).
#
# Engineering notes (mirrors ci.yml house policy):
#   - ft8_lib submodule: checkout MUST be `submodules: recursive`, and a hard
#     guard + a run-the-binary smoke step verify the real C decoder is baked
#     in. Without the submodule, pancetta-ft8/build.rs silently falls back to
#     ft8lib_stub (degraded decode recall) — a stub binary must NEVER ship.
#   - pancetta-research is a local-only iteration harness (CLAUDE.md): never
#     built in CI. This workflow builds `-p pancetta` only, so it is excluded
#     structurally — do not change this to a --workspace build.
#   - Windows uses the GNU (MinGW) toolchain, never MSVC: MSVC cannot compile
#     the VLAs in vendor/ft8_lib/ft8/decode.c (error C2057; see the
#     cross-platform-check note in ci.yml, 2026-05-23). The on-rig Windows
#     MiniPC builds with MinGW already, so gnu is also the proven toolchain.
#   - crates.io publishing is OUT of scope ([workspace.package] publish=false).
#     This workflow ships binaries only.

on:
  push:
    tags: ['v*']
  # Dry run: builds + smoke-tests artifacts (named after the branch), but the
  # release job is tag-gated and will not run.
  workflow_dispatch:

permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always

jobs:
  build:
    name: Build (${{ matrix.target }})
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: aarch64-apple-darwin
            os: macos-latest
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
          - target: x86_64-pc-windows-gnu
            os: windows-latest
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v7
        with:
          # REQUIRED: without the ft8_lib submodule the binary ships the
          # degraded ft8lib_stub decoder. Two more gates below back this up.
          submodules: recursive

      - name: Guard — ft8_lib submodule sources present
        shell: bash
        # Exact file pancetta-ft8/build.rs keys the stub fallback on.
        run: |
          test -f pancetta-ft8/vendor/ft8_lib/ft8/constants.c \
            || { echo "::error::ft8_lib submodule not checked out — refusing to build a stub release"; exit 1; }

      - name: Install system dependencies (Linux)
        if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev libudev-dev libssl-dev pkg-config

      - name: Assert MinGW gcc available (Windows)
        if: runner.os == 'Windows'
        shell: bash
        # windows-latest images ship MinGW gcc on PATH; the cc crate uses it to
        # compile ft8_lib for the gnu target. If a future image drops it, add:
        #   choco install mingw -y
        run: gcc --version

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: Swatinem/rust-cache@v2
        with:
          key: release-${{ matrix.target }}

      - name: Build release binary
        # -p pancetta only: excludes pancetta-research (local-only, never in
        # CI) and every helper binary. Default features (metrics + hamlib
        # rigctld-TCP client) are the production build; no system hamlib is
        # needed at build time (runtime-only, via rigctld over TCP).
        run: cargo build --release -p pancetta --target ${{ matrix.target }}

      - name: Smoke test — binary must embed the real C decoder
        shell: bash
        run: |
          BIN="target/${{ matrix.target }}/release/pancetta"
          if [ "${{ runner.os }}" = "Windows" ]; then BIN="$BIN.exe"; fi
          "$BIN" info
          "$BIN" info | grep -q "ft8_lib C decoder: native-C" \
            || { echo "::error::binary was built with ft8lib_stub — degraded decoder must not ship"; exit 1; }

      - name: Package
        shell: bash
        run: |
          set -euo pipefail
          NAME="pancetta-${GITHUB_REF_NAME}-${{ matrix.target }}"
          mkdir "$NAME"
          if [ "${{ runner.os }}" = "Windows" ]; then
            cp "target/${{ matrix.target }}/release/pancetta.exe" "$NAME/"
          else
            cp "target/${{ matrix.target }}/release/pancetta" "$NAME/"
          fi
          cp README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md "$NAME/"
          if [ "${{ runner.os }}" = "Windows" ]; then
            7z a "${NAME}.zip" "$NAME"
            ARCHIVE="${NAME}.zip"
          else
            tar czf "${NAME}.tar.gz" "$NAME"
            ARCHIVE="${NAME}.tar.gz"
          fi
          # sha256sum on Linux + Git-Bash/Windows; shasum on macOS.
          if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$ARCHIVE" > "${ARCHIVE}.sha256"
          else
            shasum -a 256 "$ARCHIVE" > "${ARCHIVE}.sha256"
          fi

      - uses: actions/upload-artifact@v7
        with:
          name: release-${{ matrix.target }}
          path: |
            pancetta-*.tar.gz*
            pancetta-*.zip*
          if-no-files-found: error

  release:
    name: Create draft release
    # Tag pushes only — workflow_dispatch dry runs stop after build+smoke.
    if: startsWith(github.ref, 'refs/tags/v')
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7   # CHANGELOG.md only; no submodule needed

      - uses: actions/download-artifact@v7
        with:
          path: dist
          merge-multiple: true

      - name: Extract release notes from CHANGELOG
        run: |
          set -euo pipefail
          VERSION="${GITHUB_REF_NAME#v}"
          awk -v ver="$VERSION" \
            'index($0, "## [" ver "]") == 1 {flag=1; next} /^## /{flag=0} flag' \
            CHANGELOG.md > notes.md
          test -s notes.md || { echo "::error::CHANGELOG.md has no section for ${VERSION}"; exit 1; }
          {
            echo ""
            echo "---"
            echo "Tagged at \`${GITHUB_SHA}\`. Prebuilt binaries: macOS (Apple Silicon),"
            echo "Linux x86_64 (gnu), Windows x86_64 (MinGW). Every binary is CI-verified"
            echo "to embed the real \`ft8_lib\` C decoder (\`pancetta info\`)."
          } >> notes.md

      - name: Create draft release (operator publishes)
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh release create "$GITHUB_REF_NAME" dist/* \
            --draft \
            --verify-tag \
            --title "Pancetta $GITHUB_REF_NAME" \
            --notes-file notes.md
```

- [ ] **Step 5: Validate the workflow file**

```bash
# YAML parses:
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))" && echo YAML-OK
# The awk notes-extraction works against the real CHANGELOG:
VERSION=0.9.5; awk -v ver="$VERSION" 'index($0, "## [" ver "]") == 1 {flag=1; next} /^## /{flag=0} flag' CHANGELOG.md | head -4
# pancetta-research can never build here:
grep -n "workspace\|pancetta-research" .github/workflows/release.yml
```

Expected: `YAML-OK`; the awk output starts with the `[0.9.5]` section content; the grep hits comments only, no `--workspace` build flag.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --features transmit 2>&1 | tail -3
git add pancetta/src/main.rs .github/workflows/release.yml
git commit -m "feat(release): v* tag workflow with prebuilt binaries + pancetta-info stub gate"
```

---

### Task 2: CHANGELOG — fix the malformed link footer, log this work, record the release-infra decision

**Files:**
- Modify: `CHANGELOG.md` (footer at line 98; `[Unreleased]` section at line 8)
- Modify: `docs/DECISIONS/config-and-platform.md` (append dated entry, per CLAUDE.md doc policy)

**Interfaces:**
- Consumes: nothing. Produces: `[0.9.5]` link target the Task 1 workflow's notes footer complements; Keep-a-Changelog-conformant compare links once the `v0.9.5` tag exists (Task 8).

- [ ] **Step 1: Replace the malformed footer (CHANGELOG.md:98)**

Current (broken — a compare link with only one ref):

```
[Unreleased]: https://github.com/HagaleTechnologies/pancetta/compare/HEAD
```

Replace with:

```
[Unreleased]: https://github.com/HagaleTechnologies/pancetta/compare/v0.9.5...HEAD
[0.9.5]: https://github.com/HagaleTechnologies/pancetta/releases/tag/v0.9.5
```

(Both URLs go live when Task 8 pushes the tag; until then they 404, which is normal Keep-a-Changelog practice for a pending release.)

- [ ] **Step 2: Fill the empty `[Unreleased]` section (CHANGELOG.md:8)**

```markdown
## [Unreleased]

### Added

- GitHub release workflow: pushing a `v*` tag builds prebuilt binaries for
  macOS (Apple Silicon), Linux x86_64, and Windows x86_64 (MinGW) and attaches
  them to a draft release. CI refuses to ship any binary built without the
  real `ft8_lib` C decoder.
- `pancetta info` now reports the decode engine: `ft8_lib C decoder: native-C`
  or a loud `STUB` line with the fix command.
- README: CI / release / license badges, a prebuilt-binary install section,
  and a screenshot slot (`docs/assets/`).

### Fixed

- `LICENSE-APACHE` restored to the canonical Apache-2.0 text — the previous
  file paraphrased §6 and §9 and carried a corrupted appendix, which is both
  a legal-hygiene problem and the reason GitHub reported the repo license as
  `NOASSERTION`.
- `CHANGELOG.md` link footer (the `[Unreleased]` compare URL was malformed).

### Removed

- `.env.example`, which described a Docker/Grafana deployment that has never
  existed in this repository (`git ls-files | grep -i docker` is empty).
```

- [ ] **Step 3: Append the decision digest**

Append to `docs/DECISIONS/config-and-platform.md`:

```markdown
## 2026-07-13 — v0.9.5 release infrastructure (Onboarding Phase 3)

- **Hand-rolled release workflow over cargo-dist.** cargo-dist is alive and
  maintained (v0.32, 2026) and was genuinely considered, but rejected on two
  hard requirements: (1) Windows binaries must build with the MinGW/GNU
  toolchain — MSVC cannot compile ft8_lib's VLAs (ci.yml cross-platform note,
  2026-05-23) and windows-gnu is off cargo-dist's happy path; (2) the release
  gate must run the built binary and fail on `ft8lib_stub` (custom smoke step).
  A ~150-line matrix workflow in the existing ci.yml house style keeps both
  under direct control. Revisit cargo-dist if installers/updaters are wanted.
- Releases are draft-first and tags are operator-gated: the tag push builds
  and uploads; only the operator publishes.
- LICENSE-APACHE was discovered to be a *paraphrase* of the Apache-2.0 text
  (§6 truncated, §9 retitled, appendix garbled); restored verbatim. This also
  fixes GitHub's NOASSERTION license detection.
- GitHub community-profile API `documentation` field hardcodes
  `tree/master/docs` (GitHub-side link generation; no repository setting
  controls it). Not fixable repo-side without creating a decoy `master`
  branch — rejected. Mitigation: repo `homepage` now points at
  `tree/main/docs`.
- Repo-wide K5ARH→N0CALL fixture sweep deferred: ~800 occurrences across ~85
  Rust files, several of which encode callsign *semantics* (near-miss
  K5ARG/K5ARH tests, compound-call bases, CTY prefix expectations) where a
  blind sed changes test meaning. The remote-TX security crate
  (pancetta-agent) was swept now; the rest is its own pass.
```

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md docs/DECISIONS/config-and-platform.md
git commit -m "docs(changelog): fix link footer, log Phase-3 release work; record release-infra decision"
```

---

### Task 3: README presentation — badges, Download section, screenshot slot

**Files:**
- Modify: `README.md` (top of file; before `## Prerequisites`)
- Create: `docs/assets/README.md` (capture instructions; also makes `docs/assets/` exist in git)

**Interfaces:**
- Consumes: the Task 1 workflow's artifact naming scheme (`pancetta-v0.9.5-<target>.tar.gz|zip`) — keep them in sync.
- Produces: an image slot at `docs/assets/tui-main.png` + GIF slot at `docs/assets/decode-to-qso.gif` that the **operator** fills (a plan cannot fake a screenshot); README tolerates their absence via an HTML comment block.

- [ ] **Step 1: Badges under the H1 (README.md:1)**

Immediately after `# Pancetta`, insert:

```markdown
[![CI](https://github.com/HagaleTechnologies/pancetta/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/HagaleTechnologies/pancetta/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/HagaleTechnologies/pancetta?include_prereleases&label=release)](https://github.com/HagaleTechnologies/pancetta/releases/latest)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
```

(Workflow file name verified: `.github/workflows/ci.yml`, `name: CI`. The release badge renders "no releases" until Task 8 publishes — acceptable for the few days between merge and tag.)

- [ ] **Step 2: Screenshot slot after the intro paragraphs (README.md:~15, before the Status blockquote)**

```markdown
<!-- TODO(operator): capture per docs/assets/README.md, commit the images,
     then un-comment this block. README must render cleanly until then.

![Pancetta TUI — decodes, waterfall, DX hunter, and QSO ladder on 20 m](docs/assets/tui-main.png)

*A full CQ→RR73 exchange, hands-off:*

![Decode → QSO in one 15-second cadence](docs/assets/decode-to-qso.gif)
-->
```

- [ ] **Step 3: Download section ABOVE build-from-source (insert between the Status blockquote and `## Prerequisites`)**

`````markdown
## Install

### Option A — prebuilt binary (fastest: ~5 minutes to decoding)

No Rust toolchain needed. Download the archive for your platform from the
[latest release](https://github.com/HagaleTechnologies/pancetta/releases/latest):

| Platform | Artifact |
|---|---|
| macOS (Apple Silicon) | `pancetta-v0.9.5-aarch64-apple-darwin.tar.gz` |
| Linux x86_64 | `pancetta-v0.9.5-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `pancetta-v0.9.5-x86_64-pc-windows-gnu.zip` |

```bash
tar xzf pancetta-v0.9.5-aarch64-apple-darwin.tar.gz
cd pancetta-v0.9.5-aarch64-apple-darwin

# macOS only: the binaries are not notarized yet — clear the quarantine bit:
xattr -d com.apple.quarantine ./pancetta 2>/dev/null || true

./pancetta        # first run: setup wizard, then the decoding TUI
./pancetta info   # sanity check — must print "ft8_lib C decoder: native-C"
```

Linux needs the ALSA runtime only: `sudo apt install libasound2`
(`libasound2t64` on Ubuntu 24.04+). Windows and macOS need nothing extra.
Verify downloads against the `.sha256` file published next to each archive.

### Option B — build from source

Follow the Quick Start below (15-minute path; needs the Rust toolchain).
`````

- [ ] **Step 4: Create `docs/assets/README.md` with EXACT operator capture instructions**

```markdown
# docs/assets — README media

Two files are referenced (commented out until they exist) by the top-level
README: `tui-main.png` and `decode-to-qso.gif`. Pancetta is a ratatui TUI, so
these must be captured from a live terminal by the operator — they cannot be
generated.

## Capturing `tui-main.png` (screenshot)

1. Run the real station on a busy band (20 m mid-afternoon is reliable):
   `./target/release/pancetta`. Let it run 2–3 slot cycles so the decode list,
   waterfall, and DX Hunter are all populated. A dark terminal theme at
   ≥140×40 cells reads best; make the window ≥1600 px wide on screen.
2. Capture:
   - **macOS (recommended):** `Cmd+Shift+4`, then `Space`, then click the
     terminal window — produces a clean window-cropped PNG with shadow.
     (`Cmd+Shift+5` → "Capture Selected Window" is equivalent.)
   - **Linux:** `gnome-screenshot -w` or `spectacle -a` (window capture).
   - **Windows:** `Win+Alt+PrtScn` (Game Bar window capture).
3. Save as `docs/assets/tui-main.png`. FT8 decodes are public broadcasts —
   no redaction needed.

## Capturing `decode-to-qso.gif` (animated)

1. `brew install asciinema agg` (or cargo install agg).
2. `asciinema rec pancetta.cast`, run `./target/release/pancetta`, work one
   QSO (Space on a caller → watch the ladder to RR73), quit, `Ctrl+D`.
3. Render: `agg --font-size 16 --speed 2.5 pancetta.cast decode-to-qso.gif`
   Keep it under ~10 MB (trim the cast or re-record a tighter session).
   (A static alternative for single frames is `termshot`, but window capture
   above is better for a full-screen TUI.)
4. Save as `docs/assets/decode-to-qso.gif`.

## After capturing

Un-comment the image block near the top of `README.md`, then:

    git add docs/assets/tui-main.png docs/assets/decode-to-qso.gif README.md
    git commit -m "docs: add TUI screenshot and decode-to-QSO GIF"
```

- [ ] **Step 5: Verify**

```bash
grep -n "badge.svg\|img.shields.io\|docs/assets" README.md
ls docs/assets/
# Confirm the commented block really is inert (no bare image link outside the comment):
grep -n "tui-main.png" README.md
```

Expected: badges present; `docs/assets/README.md` exists; every `tui-main.png` reference sits inside the `<!-- ... -->` block.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/assets/README.md
git commit -m "docs(readme): badges, prebuilt-binary install section, screenshot slot with operator capture guide"
```

---

### Task 4: Restore canonical `LICENSE-APACHE` (fixes GitHub NOASSERTION)

**Files:**
- Modify: `LICENSE-APACHE`

**Interfaces:** none.

**Investigation result (verified 2026-07-13):** `gh api repos/HagaleTechnologies/pancetta/license` returns `NOASSERTION` with path `LICENSE-APACHE`. A word-level diff against the canonical `https://www.apache.org/licenses/LICENSE-2.0.txt` shows the tracked file is a **paraphrase, not the license**: §6 drops "reasonable and customary use in"; §9 is retitled "Accepting Warranty or Support" with rewritten sentences; the appendix boilerplate is garbled; the closing paragraph inserts `(the "Work")`. That is why licensee fails to match it. **The LICENSE.md-pointer idea is evaluated and REJECTED:** licensee prefers a file named `LICENSE.md` over `LICENSE-APACHE`, and a one-paragraph pointer matches no canonical license text — adding it would *guarantee* NOASSERTION. Restoring the canonical text is the actual fix; GitHub detects rust-style dual licensing (`LICENSE-MIT` + `LICENSE-APACHE`, both canonical) as "Apache-2.0, MIT licenses found". `LICENSE-MIT` already matches canonical MIT — leave it alone.

- [ ] **Step 1: Replace with canonical text, preserving the copyright attribution**

```bash
curl -s https://www.apache.org/licenses/LICENSE-2.0.txt -o LICENSE-APACHE
sed -i '' 's/Copyright \[yyyy\] \[name of copyright owner\]/Copyright 2025 Hagale Technologies, LLC/' LICENSE-APACHE
```

- [ ] **Step 2: Verify it now matches canonical (modulo the one copyright line)**

```bash
curl -s https://www.apache.org/licenses/LICENSE-2.0.txt | diff - LICENSE-APACHE
```

Expected: exactly one hunk — the `Copyright [yyyy]...` → `Copyright 2025 Hagale Technologies, LLC` line. (Licensee ignores copyright lines when matching.)

- [ ] **Step 3: Commit**

```bash
git add LICENSE-APACHE
git commit -m "fix(license): restore canonical Apache-2.0 text (previous file was a paraphrase; broke GitHub license detection)"
```

Post-merge check (belongs to Task 7 verification): `gh api repos/HagaleTechnologies/pancetta/license --jq '.license.spdx_id'` → expect `Apache-2.0` (detection re-runs on push to the default branch; allow a few minutes). If it still reads NOASSERTION after a day, the residual cause is GitHub-side caching — the canonical text is correct regardless; do not add LICENSE.md.

---

### Task 5: Crate metadata (`pancetta-agent`, `pancetta-protocol`) + delete stale `.env.example`

**Files:**
- Modify: `pancetta-agent/Cargo.toml`
- Modify: `pancetta-protocol/Cargo.toml`
- Delete: `.env.example`

**Interfaces:**
- Consumes: `[workspace.package]` (root `Cargo.toml:35-42`: `version`, `edition`, `authors`, `license`, `repository`, `publish = false` all defined).
- Produces: complete `[package]` metadata so `publish = false` stays a choice, not a blocker (spec Phase 3 bullet 4). Verified current state: both crates inherit `version`/`edition`/`license` but have **no `description`, no `repository`, no `authors`**.

- [ ] **Step 1: `pancetta-agent/Cargo.toml` — replace the `[package]` block**

```toml
[package]
name = "pancetta-agent"
description = "Remote-TX security agent for pancetta: arm-state gating, capability verification, device pairing, and audit trail"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true
publish = false
```

- [ ] **Step 2: `pancetta-protocol/Cargo.toml` — replace the `[package]` block**

```toml
[package]
name = "pancetta-protocol"
description = "Remote-operation wire protocol for pancetta: serde DTOs shared between the station and remote clients (no bus internals)"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true
publish = false
```

- [ ] **Step 3: Delete `.env.example`**

Verified stale: it configures `GRAFANA_ADMIN_PASSWORD` and a "Pancetta Docker Environment" — but `git ls-files | grep -i docker` is empty and nothing references it. It documents an environment that doesn't exist.

```bash
git rm .env.example
```

- [ ] **Step 4: Verify**

```bash
cargo metadata --format-version 1 --no-deps \
  | python3 -c "import json,sys; [print(p['name'], '|', p.get('description'), '|', p.get('repository')) for p in json.load(sys.stdin)['packages'] if p['name'] in ('pancetta-agent','pancetta-protocol')]"
cargo check -p pancetta-agent -p pancetta-protocol 2>&1 | tail -2
```

Expected: both crates print a description and `https://github.com/HagaleTechnologies/pancetta`; check green.

- [ ] **Step 5: Commit**

```bash
git add pancetta-agent/Cargo.toml pancetta-protocol/Cargo.toml
git commit -m "chore(metadata): fill description/repository/authors on pancetta-agent + pancetta-protocol; delete stale Docker-era .env.example"
```

---

### Task 6: Callsign hygiene — K5ARH → N0CALL in pancetta-agent fixtures; relocate `PLAN-priority-scoring.md`

**Files:**
- Modify: `pancetta-agent/src/arm.rs`, `pancetta-agent/src/audit.rs`, `pancetta-agent/src/control.rs`, `pancetta-agent/src/pairing.rs`, `pancetta-agent/tests/capability_verification.rs`
- Move: `PLAN-priority-scoring.md` → `docs/superpowers/plans/2026-07-06-priority-scoring-cqdx-contract-hardening.md`

**Interfaces:**
- Consumes: the repo's own convention (CHANGELOG `[0.9.5]`: example default callsign changed to `N0CALL`).
- Produces: a security crate free of the operator's real callsign in fixtures; a root directory free of stray plan files.

**Safety analysis (performed — read before swapping):**
- `arm.rs` (10 sites): all are `VerifiedArmGrant { operator_callsign: "K5ARH"... }` constructions plus two string assertions (`operator_callsign(), Some("K5ARH")` at :525; audit-event assert at :856). `VerifiedArmGrant` is *post-verification* plain data; arm/heartbeat/TTL logic keys on `jti`/`seq`/timestamps, never the callsign. **Plain swap safe** as long as constructions and assertions change together (a single `sed` per file guarantees that).
- `audit.rs` (4), `control.rs` (3), `pairing.rs` (1 — the string is `"K5ARH Rig"`, a device label): plain data / JSON-mapping equality tests; `control.rs`'s txArm test uses a placeholder `"clientSig": "sig-base64url"` that is never cryptographically verified in the mapping layer. **Plain swap safe.**
- `capability_verification.rs` (`const OPERATOR: &str = "K5ARH"`): grants are **signed at runtime** with deterministic seeded keys (`SigningKey::from_bytes(&[seed; 32])`); there are no precomputed signature/hash literals in the file (verified: no string literals ≥40 chars). Changing the callsign changes the payload and its runtime signature together. **Plain swap safe — no fixture regeneration needed.** If any test fails after the swap, STOP and re-read that test before forcing it; that would falsify this analysis.
- **Out of scope, deliberately:** the repo-wide sweep. `grep -rc "K5ARH" --include="*.rs" . --exclude-dir=target` finds ~800 occurrences across ~85 files (qso/tui/ft8/coordinator tests, research examples). Several encode callsign *semantics* (near-miss `K5ARG`/`K5ARH` disambiguation, compound-call base extraction, CTY prefix expectations) where blind substitution changes test meaning. That sweep is its own future task (recorded in the Task 2 decision digest). Docs describing real on-air sessions are never swapped.

- [ ] **Step 1: Swap in pancetta-agent**

```bash
sed -i '' 's/K5ARH/N0CALL/g' \
  pancetta-agent/src/arm.rs \
  pancetta-agent/src/audit.rs \
  pancetta-agent/src/control.rs \
  pancetta-agent/src/pairing.rs \
  pancetta-agent/tests/capability_verification.rs
grep -rn "K5ARH" pancetta-agent/ ; echo "grep-exit=$? (want 1 = no matches)"
```

- [ ] **Step 2: Run the crate's full test suite**

```bash
cargo test -p pancetta-agent 2>&1 | tail -5
```

Expected: all green (unit + integration + property tests). Any failure here means the safety analysis above was wrong — investigate, don't patch assertions blindly.

- [ ] **Step 3: Relocate + sanitize the stray root plan file**

The file's own header says `Date: 2026-07-06`; its 3 K5ARH sites are CTY-resolution examples where N0CALL (a US-format call) preserves the semantics exactly (`F/N0CALL` → France via prefix, `N0CALL/P` → entity 291).

```bash
git mv PLAN-priority-scoring.md docs/superpowers/plans/2026-07-06-priority-scoring-cqdx-contract-hardening.md
sed -i '' 's/K5ARH/N0CALL/g' docs/superpowers/plans/2026-07-06-priority-scoring-cqdx-contract-hardening.md
ls PLAN-*.md 2>/dev/null; echo "root-plans-exit=$? (want 1)"
```

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add -A pancetta-agent docs/superpowers/plans/2026-07-06-priority-scoring-cqdx-contract-hardening.md
git commit -m "chore(hygiene): swap operator callsign to N0CALL in pancetta-agent fixtures; move root plan file into docs/superpowers/plans"
```

---

### Task 7: GitHub repo metadata via `gh` (no commits)

**Files:** none (GitHub API state only). Run these against the live repo; they are idempotent.

**Interfaces:**
- Consumes: `gh` authenticated as the operator (repo admin).
- Produces: topics/description/homepage that make the repo findable; documented outcomes for the two investigation items.

- [ ] **Step 1: Description + homepage**

Current state (verified): description = "Modern Ham Radio Digital Modes client written in Rust", homepage = null, topics = `[]`.

```bash
gh api -X PATCH repos/HagaleTechnologies/pancetta \
  -f description='Autonomous FT8 ham radio station in Rust — decode, priority-score, and work QSOs from a terminal UI; multi-stream TX, CAT control, optional hands-off operation' \
  -f homepage='https://github.com/HagaleTechnologies/pancetta/tree/main/docs'
```

- [ ] **Step 2: Topics**

Syntax verified: the topics endpoint is GA — the historical `application/vnd.github.mercy-preview+json` accept header is **no longer required**; `gh api`'s default accept works. PUT replaces the full set:

```bash
gh api -X PUT repos/HagaleTechnologies/pancetta/topics \
  -f 'names[]=ham-radio' -f 'names[]=amateur-radio' -f 'names[]=ft8' \
  -f 'names[]=rust' -f 'names[]=tui' -f 'names[]=sdr' \
  -f 'names[]=digital-modes' -f 'names[]=wsjt-x'
```

- [ ] **Step 3: Verify all metadata**

```bash
gh api repos/HagaleTechnologies/pancetta --jq '{description, homepage, topics}'
```

Expected: the new description, the `tree/main/docs` homepage, and all 8 topics.

- [ ] **Step 4: Community-profile docs-link 404 — investigation outcome (no command can fix it)**

Verified: `gh api repos/HagaleTechnologies/pancetta/community/profile --jq .documentation` returns `https://github.com/HagaleTechnologies/pancetta/tree/master/docs`, which 404s (default branch is `main`; no `master` branch exists). Root cause: GitHub **auto-generates** this link from the presence of a `docs/` directory and hardcodes `master` — no repository setting controls the field. The only repo-side "fix" would be creating a decoy `master` branch — rejected. Mitigation is the homepage set in Step 1 (a working docs link in the About box). Re-check after Step 1 in case GitHub regenerates:

```bash
gh api repos/HagaleTechnologies/pancetta/community/profile --jq '{health_percentage, documentation}'
```

- [ ] **Step 5: License detection — verify Task 4's fix landed (run after the PR merges)**

```bash
gh api repos/HagaleTechnologies/pancetta/license --jq '.license.spdx_id'
gh api repos/HagaleTechnologies/pancetta --jq .license
```

Expected: `Apache-2.0` (GitHub reports the "preferred" file; the repo page shows both licenses found). Detection re-runs on default-branch pushes; allow a few minutes after merge. If still `NOASSERTION` after 24 h, document and move on — the canonical text is correct either way, and per the Task 4 evaluation, do **not** add a `LICENSE.md` pointer.

---

### Task 8: OPERATOR-GATED — merge, tag `v0.9.5`, verify the release

**Files:** none (git tag + GitHub release state).

**Interfaces:**
- Consumes: the merged PR from Tasks 1–6 (the tag MUST contain `release.yml` — tag-push workflows run from the tagged commit, so tagging pre-merge main would run nothing).
- Produces: the repo's first tag (verified: `git tag -l` and `git ls-remote --tags origin` are both empty today) and a published v0.9.5 release with 6 assets (3 archives + 3 `.sha256`).

> **GATE: Do not run Step 2 onward without explicit operator approval.** SECURITY.md promises tagged releases "once they exist" — this creates the first one, and the tag is permanent public API.

- [ ] **Step 1 (optional, pre-merge): dry-run the workflow**

After the PR branch is pushed, trigger `workflow_dispatch` on the branch to shake out runner issues without a tag:

```bash
gh workflow run release.yml --repo HagaleTechnologies/pancetta --ref <pr-branch-name>
gh run watch --repo HagaleTechnologies/pancetta \
  "$(gh run list --repo HagaleTechnologies/pancetta --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Expected: all three `build` jobs green (incl. the `native-C` smoke line in each log); the `release` job is skipped (tag-gated).

- [ ] **Step 2 (OPERATOR-GATED): tag the post-merge main tip**

The tag target is the **merge commit of this plan's PR** — i.e., the main tip *after* merge (tag-push workflows run from the tagged commit, which must contain `release.yml`). Note that the tagged tree contains post-2026-06-24 fixes beyond the CHANGELOG `[0.9.5]` section (health panel, FP-filter fix, silent-TX-failure fix, dependency bumps); the workflow's notes footer states the tag SHA for exactly this reason. Operator accepts this or requests a CHANGELOG amendment before tagging.

```bash
git fetch origin main
TIP=$(git rev-parse origin/main)
git log -1 "$TIP" --format='about to tag: %H %s'   # eyeball: is this the Phase-3 merge commit?

git tag -a v0.9.5 "$TIP" -m "Pancetta v0.9.5 — first tagged release

First public tag. Release notes: CHANGELOG.md section [0.9.5] (2026-06-24).
Ships prebuilt binaries for macOS (Apple Silicon), Linux x86_64, and
Windows x86_64 (MinGW), each CI-verified to embed the real ft8_lib C decoder."

git push origin v0.9.5
```

- [ ] **Step 3: Watch the release build**

```bash
gh run watch --repo HagaleTechnologies/pancetta \
  "$(gh run list --repo HagaleTechnologies/pancetta --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Expected: 3 build jobs + the `release` job all green.

- [ ] **Step 4: Verify the draft release end-to-end**

```bash
gh release view v0.9.5 --repo HagaleTechnologies/pancetta --json isDraft,assets \
  --jq '{draft: .isDraft, assets: [.assets[].name]}'
# Expect: draft=true, 6 assets.

# Download + verify the macOS artifact locally (Apple Silicon Mac):
cd "$(mktemp -d)"
gh release download v0.9.5 --repo HagaleTechnologies/pancetta -p 'pancetta-v0.9.5-aarch64-apple-darwin*'
shasum -a 256 -c pancetta-v0.9.5-aarch64-apple-darwin.tar.gz.sha256
tar xzf pancetta-v0.9.5-aarch64-apple-darwin.tar.gz
./pancetta-v0.9.5-aarch64-apple-darwin/pancetta info | grep "ft8_lib C decoder: native-C"
```

Expected: checksum `OK`; the grep prints the native-C line. (The Windows artifact gets its real-hardware validation on the MiniPC — add to the meatspace list.)

- [ ] **Step 5 (OPERATOR-GATED): publish**

The operator publishes from the GitHub UI, or:

```bash
gh release edit v0.9.5 --repo HagaleTechnologies/pancetta --draft=false --latest
```

Then confirm the README badges resolve: the Release badge now shows `v0.9.5`, and `releases/latest` serves the Download table's links.

---

## Final gate (after Tasks 1–6, before the PR)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test --workspace --features transmit 2>&1 | tail -5
cargo test -p pancetta-agent 2>&1 | tail -3
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo YAML-OK
git grep -n "K5ARH" -- pancetta-agent 'docs/superpowers/plans/2026-07-06-*' ; echo "want no matches"
```

Expected: all green. Push the branch and open a PR titled "Onboarding Phase 3: releases, discoverability, and presentation" referencing `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md`. After merge: run Task 7, then hand Task 8 to the operator. Post-publish operator follow-ups (tracked in `docs/assets/README.md` and the meatspace list): capture the TUI screenshot/GIF, and smoke-test the Windows artifact on the MiniPC.
