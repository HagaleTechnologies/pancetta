# Lane 2 — Install → first run → CLI → config → docs → packaging

Reviewed at `main` = `c90cb34` (v0.9.6-3), binary `target/release/pancetta` built 2026-09-05 (3m55s, M4 Pro). All runs used isolated `HOME`s; no rig enabled, no TX; audio hardware touched only via `test-audio --list` / `doctor`.

## 1. Executive summary

Pancetta's onboarding *skeleton* is genuinely good — safe-by-default posture, a wizard that validates at the prompt, a real `doctor`, CI-verified binaries for 4 targets, an honest README — but the **config layer under it is unreliable in a way that defeats the ≤5-minute goal for anyone who edits TOML by hand, which every doc tells them to do**. Scores (1–10): **install 6 · first-run 6 · CLI 5 · docs 5 · packaging 6.**

1. **P0:** ~50 config structs lack `#[serde(default)]`, so the partial blocks every doc shows — `[autonomous] enabled = true`, GUIDE's GridTracker `[network.wsjtx_udp]` recipe, CONFIG.md's own "minimum viable config" `[rig.interface]` — fail with `missing field …`. On the default path the loader then **silently discards the whole file and runs as N0CALL/AA00aa**, and `pancetta config --validate` prints **PASS, exit 0** on that same file. This is the "silently ignored knob" class the spec's Phase 2 set out to kill, one level down.
2. **P1 docs-truth:** config hot-reload is documented in CONFIG.md/RUNBOOK/crate docs and **is not wired into the binary at all** (`ConfigManager`/watcher never instantiated; `classify_config_reload` imported, never called).
3. **P1:** a WAV recorder writes every 15-s window to `~/.pancetta/recordings` unconditionally (hard-coded 10 GB cap, no knob, undocumented) — hostile to the Pi/SD-card story the release notes advertise.
4. **P1:** `doctor` prints "Result: ready … you should see decodes" with no config file (4 WARNs). The ADIF "index rebuilt when missing" migration path never fires on a fresh drop-in (logger creates `qso.db` ~1 ms before the seed checks for it).
5. CLI fossils: `--help` marketing (`<1ms latency`, `>95% at −20 dB`) contradicting the README; dead flags (`--log-format`, `run --callsign/--frequency/--power`); two "not yet implemented → exit 1" subcommands; 15 global flags (incl. `--test-tx`) echoed into every subcommand's help; `config --show` = 5-line summary; a 962-line config dump ~85 % runtime-inert GUI scaffolding (`[ui.window] width = 1200`, `secret_encrypted`).
6. Packaging honest but minimal: ad-hoc-signed macOS binary (no notarization), unsigned Windows exe (SmartScreen), per-file `.sha256` only (no signing/SBOM), no install script / brew tap / binstall / winget / deb / nix / `pancetta upgrade`, no Intel-mac or armv7. `scripts/build_release.sh` is a fabricated fossil.
7. Excellent: wizard validation+normalization, de-brick offer on *validation* failure, owner-only atomic config writes, `--wav`/`--replay` zero-hardware smoke paths, `doctor`'s hand-rolled SNTP, honest README caveats, release gate refusing stub-decoder binaries, secrets never reach logs (verified with `-v`).

## 2. Measured first-run timeline

| Step | Wall clock | Notes |
|---|---|---|
| `cargo build --release -p pancetta` | 3 m 55 s | warm registry, cold target |
| `--help` / `--version` | < 10 ms | |
| First-run wizard (scripted: 2 bad inputs, audio picker, rig skipped, save) | 7 s | human pace ≈ 1.5–2 min |
| `doctor` (no config) | 0.07 s | **falsely "ready"** |
| `doctor` (with config) | ≈ 2.5 s | 2-s level capture |
| `--wav assets/demo-wav/live_now.wav` | 1.06 s | 26 decodes, exit 0 |
| `--headless --replay assets/demo-wav` | 102 s | 5.5 s slot-align + 75 s audio + 20 s grace; exit 0; 28 decodes |
| `test-audio --list`, `info`, `config --*`, `export`, `identity` | ≤ 0.08 s | |

Wizard → doctor → first decode ≈ **2 min** on the happy path. The budget is blown the moment the operator follows any doc that says "add this block to `pancetta.toml`" (P0-1).

## 3. Ranked hit list

### P0
1. **[P0][M] Partial config sections fail to parse; file silently replaced by defaults.** `[station] callsign="W5AU" grid_square="EM13"` + `[autonomous] enabled = true` at `~/.pancetta/pancetta.toml` → `config --show`: `WARNING: … failed to parse — using defaults … missing field 'slot_parity'` → `Station: N0CALL (AA00aa)`. Same for `[network.wsjtx_udp]` (GUIDE.md:228-232 verbatim), `[rig.interface]` (CONFIG.md:34-37 verbatim), `[rig.ptt]`, `[rig.frequency]`, `[autonomous.priorities]`, `[duplicate_checking]`, `[audio.levels]`, `[audio.processing]`. Root cause: 50+ structs without struct-level `#[serde(default)]` (`autonomous.rs:200`, `rig.rs:208`, `network.rs:240`, `duplicate_check.rs:14`, …). Fix: `#[serde(default)]` on every config struct + CI test that deserializes every TOML snippet in `docs/**/*.md`. Why: every "enable X" instruction in the docs currently reverts the station to N0CALL.
2. **[P0][S] `config --validate` returns PASS/exit 0 for a file that failed to parse** (`main.rs:760-773` validates the merged defaults, ignores `load_warnings`). Fix: any load warning ⇒ exit 1 with path + TOML line/col.
3. **[P0][S] Loader swallows parse errors into a warning and continues with defaults** (`loader.rs:~232-250`). For a transmitting station a typo silently reverts callsign → N0CALL. Fix: parse failure of the primary file is fatal for TUI/headless (offer the de-brick wizard interactively — the prompt exists but is unreachable for parse errors, #12).

### P1
4. **[P1][S] `doctor` says "Result: ready" with no config file** — 4×`[WARN] not configured`, exit 0 (`doctor.rs:100-109`, `main.rs:1540-1545`). Fix: distinct `NOT CONFIGURED` verdict, non-zero exit; color on TTY.
5. **[P1][M] Hot-reload documented, not wired.** CONFIG.md:3-5, 511-525 (incl. nonexistent `Config reloaded: 12 keys updated`), RUNBOOK.md:296-299. `grep ConfigManager|start_watching|notify:: pancetta/src pancetta-tui/src` → nothing; `classify_config_reload` (`health.rs:112`) never called. Fix: wire it or delete every claim. Why: "suspend autonomous by editing config" per RUNBOOK never takes effect while the station keeps transmitting.
6. **[P1][S] WAV recorder always on, 10 GB, undocumented, unconfigurable.** `dsp.rs:32` const; `dsp.rs:395-401` unconditional; runs under `--no-audio` and `--replay`; 1.8 MB per 100 s ≈ 1.5 GB/day. `[audio.recording]` schema (`auto_record = false`, `directory = "~/Documents/Pancetta/Recordings"`) read by nothing. GH #265 covers only `--replay`. Fix: honor `auto_record` (default off in release), document, `doctor` disk row.
7. **[P1][S] ADIF-index rebuild never fires on fresh drop-in.** WSJT-X-style `qsos.adi` into empty `~/.pancetta` → logger creates `qso.db` at T+0.9928 s, seed check at T+0.9937 s sees DB newer → no replay → `seeded 0 worked grid(s)`; `export` → 0 QSOs. Touching the ADIF + restart *does* replay (`Replayed 3 records … seeded 2 worked station(s)`), so parser is fine; ordering (`coordinator/qso.rs:2190-2215`) is wrong. Why: CONFIG.md:540-542 / TROUBLESHOOTING.md:49-52 promise it; it is *the* WSJT-X migration path.
8. **[P1][S] RUNBOOK.md:151 says press `T` to auto-pick TX offset — `T` = Shift+T = TUNE (12-s carrier, PTT).** KEYBINDINGS.md:38/42. Fix: `t`.
9. **[P1][S] RUNBOOK cites non-existent commands**: `pancetta --list-audio` (:88, :103), `config --show --path` (:177); `--show` can't confirm the keys RUNBOOK :95-108 asks about.
10. **[P1][S] `setup` on unparseable config silently starts from defaults and overwrites on save** (`main.rs:1461-1465` `unwrap_or_default()`; E7: `Callsign [N0CALL]`, no warning). `doctor`'s fix line recommends exactly this ("run `pancetta setup` (rewrites the file)").
11. **[P1][M] `setup`/wizard/`--generate` write the full 962-line schema, destroying comments.** E1: 966 lines; E6: 10-line commented config → 956 lines, 0 user comments, incl. `[ui.window] width = 1200`, `[network.web_api.authentication.jwt] secret_encrypted = ""` (SECURITY.md:36-37 claims `*_encrypted` removed). Fix: `toml_edit` non-default-only writes (as `set_audio_devices_in_file` already does).
12. **[P1][S] De-brick offer unreachable for parse errors.** Fires only on validation `Err` (E5b OK); parse error returns `Ok(defaults)` → E5a shows `No station configuration found` (false). config-and-platform.md:105-115 records it as shipped.
13. **[P1][S] `--help` long_about is marketing fiction** (`main.rs:49-71`: `<1ms latency`, `>95% at -20dB`, `<100MB`, `<25% CPU`) vs README.md:40-47 (~2.1 dB behind jt9). FEATURES.md:5 repeats `>95%`, "~200 tests" (README ~295), typo "Ordererd".
14. **[P1][S] Headless with invalid config exits with anyhow trace, no path/fix** (E5c). Under systemd `Restart=always` → crash-loop 5×/min.
15. **[P1][S] TROUBLESHOOTING.md never mentions `pancetta doctor`** (spec §Phase 4: "first line of every troubleshooting doc").

### P2
16. **[P2][S] Unknown-key warning: top-level only, default path only.** `[station] callsgn = …` → PASS silently; `--config file` never sweeps (`[autonomous_operator]` via `--config` → PASS, no warning).
17. **[P2][S] `identity` says "read-only, no network calls" (`main.rs:186-188`) but generates keys** — fresh HOME grew `~/.pancetta/agent/{identity.key,agreement.key}` (`load_or_generate`, `main.rs:513`).
18. **[P2][S] Wizard saves N0CALL config on Ctrl-D/Enter-through** (E2: save default yes → `callsign = "N0CALL"`, "Setup complete!").
19. **[P2][S] `prompt_choice` invalid selection keeps `default`** (E1: `Select input device: 99` → stays `"default"`; doctor then WARNs). Loop like callsign/grid.
20. **[P2][S] Rig model free-text; validated only by `doctor`** (E4: `FT-DX3000` saved → `FAIL unknown rig model`). `hamlib_model_id` knows 14 rigs (`hamlib.rs:1003-1021`); default `"Generic"` isn't one. Fix: numbered picker + rigctld-number escape.
21. **[P2][S] Serial picker lists `debug-console`/`Bluetooth-Incoming-Port` as "PCI"; `test-rig` reports FOUND/OK on `/dev/cu.debug-console`** (E4).
22. **[P2][S] `test-rig` exits 0 on every failure; CAT PTT "not yet implemented"** while README.md:263 says `--ptt` keys TX 1 s; opens serial port directly (collides with rigctld).
23. **[P2][S] `config --show` is a 5-line summary**; shows `Rig: Generic @ /dev/ttyUSB0` with rig disabled. Add effective-TOML dump (redacted), `--path`, `--diff`.
24. **[P2][S] Dead CLI surface**: `--log-format json` never read (headless output stays text); `run --callsign/--frequency/--power` ignored (`Commands::Run(_args)`); `benchmark` and `test-audio` (sans `--list`) → "not yet implemented", exit 1.
25. **[P2][S] 15 global flags echoed into every subcommand help**, incl. `--test-tx` under `export --help`. Make only `--config/-d/-v` global.
26. **[P2][S] Headless console log always ANSI** (`^[[2m…^[[32m INFO` when piped) — journald/NSSM/launchd logs fill with escapes. `.with_ansi(is_terminal())`.
27. **[P2][S] Every subcommand creates `~/.pancetta/logs/`** and writes a log line (`init_logging` before `handle_command`).
28. **[P2][S] Logs 0644; hand-written 0644 config never tightened** (only `save_to_file` chmods). `doctor` should WARN.
29. **[P2][M] Headless with no config runs as N0CALL with no banner** — only `WARN AP decoding: could not encode 'N0CALL'`.
30. **[P2][S] CONFIG.md defaults wrong**: `[rig].model` `""` vs `"Generic"`; `baud_rate` `38400` vs `9600`; `port` `""` vs `"/dev/ttyUSB0"`; `[ui].theme` "dark"/"light" vs `"default"` (any string validates — `"solarized"` → PASS).
31. **[P2][S] CONFIG.md env table wrong/incomplete** (:496-507): documents `--no-rig` (doesn't exist); omits real `PANCETTA_CALLSIGN/GRID_SQUARE/POWER_WATTS/CAT_PORT/CAT_BAUD/AUDIO_INPUT/AUDIO_OUTPUT/SAMPLE_RATE/THEME` (`loader.rs:460-510`), `PANCETTA_WORKER_THREADS`, `PANCETTA_HOUND`, `PANCETTA_QSO_FILTER_OFF`, `PANCETTA_SCOPED_FAST_PATH`.
32. **[P2][S] Config search path undocumented and surprising** (`loader.rs:150-181`): **cwd first** (`./pancetta.toml|config.toml|pancetta.json|config.json`), then `~/Library/Application Support/pancetta`, `/etc/pancetta`, `/usr/local/etc/pancetta`, `~/.pancetta`, `~/.config/pancetta`, all merged. Drop cwd.
33. **[P2][S] ~600 schema lines runtime-inert; decisions digest calls them "live".** Outside `pancetta-config`: `network.web_api/wspr/tls/proxy/rate_limiting/reliability` 0 refs, `audio.recording` 0, `ui.*` only `theme`/`show_coordinates`. config-and-platform.md:190-193 overstates. `[ui.keyboard.shortcuts.quit] keys = "Ctrl+Q"` advertises a remap that does nothing.
34. **[P2][S] `[rig.ptt]`/`[rig.frequency]` — sections the wizard writes — undocumented in CONFIG.md.**
35. **[P2][S] GUIDE.md:204 `Shift+M` FT8→FT4→FT2 vs generated KEYBINDINGS.md:55 FT8→FT4** — GUIDE bypassed the SSOT.
36. **[P2][S] SECURITY.md stale ×3**: "releases once they exist" (v0.9.5/0.9.6 exist); "credentials via env vars" (none exist); `bindings.rs` "slated for removal" (already gone).
37. **[P2][S] CONTRIBUTING.md boilerplate survives**: "Rust 1.70+" (no `rust-version`; PAN-56), "Discord (optional)" (:51), "review within 48 hours" (:370), `cargo build --all` (:74-80).
38. **[P2][S] CHANGELOG `[Unreleased]` empty with 3 commits since v0.9.6** incl. #325 (ClubLog key bake — user-visible config behaviour). Release notes are generated from this section.
39. **[P2][S] `scripts/build_release.sh` is a fossil**: fake config keys (`[hamlib] use_mock`, `[ft8] decode_depth`), nonexistent `docs/INSTALL.md`/`USER_GUIDE.md`/`LICENSE`, MSVC target (can't build ft8_lib), "73 de Pancetta Team". `git rm`.
40. **[P2][S] launchd plist self-contradicts**: header says `~/.pancetta/launchd.*.log`, keys set `/tmp/pancetta.launchd.*.log`; binary `/usr/local/bin/pancetta` vs systemd `%h/.cargo/bin/pancetta`; README installs neither.
41. **[P2][M] No "send me your logs" bundle**: template asks for `~/.pancetta/logs/pancetta.log` (real: `pancetta.log.YYYY-MM-DD`), `git rev-parse`, `rustc --version`. Add `doctor --json` + `support-bundle` (redacts `password|token|api_key`).
42. **[P2][S] `--version` = `pancetta 0.9.6`** — no SHA/date/target/decoder/features.
43. **[P2][S] No shell completions / man page** (clap_complete, ~20 lines).

### P3
44. `doctor` rig-check "to go on-air" hint never prints (fix only shown for non-Pass; `main.rs:1531-1535`).
45. `doctor` submodule check cwd-relative (`doctor.rs:514`).
46. `doctor` lacks output-device, `tqsl`-when-LoTW, disk-space, `rigctl --list` checks; level check passes on `rms −91 dBFS, peak 0.000`.
47. `doctor` clock needs UDP 123 to `pool.ntp.org`; no local fallback (`chronyc`/`w32tm`), no `PANCETTA_NTP_SERVER`.
48. Wizard ignores `--config` (`main.rs:1060-1063`, `1457-1460`) while `doctor` honours it.
49. `pancetta config` (no flags) prints "Use --help", exit 0 → should be usage, exit 2.
50. JSON config half-supported (search path yes, `--config` no, undocumented). Drop.
51. `export` reads `qso.db`, exits 1 when only ADIF exists — contradicts ADIF-is-truth.
52. No `import` subcommand though `pancetta_qso::import_adif` exists (`lib.rs:460`).
53. Archive dir named by target triple; add friendly alias table in notes.
54. Release notes lack install snippet/checksum block.
55. README Windows path: no SmartScreen, console-closes-on-exit, `%USERPROFILE%\.pancetta`, `rigctld.exe` on PATH guidance (GH #275 adjacent).
56. `RIGCTLD_HOST/PORT` env-only; no config key.
57. Log retention time-based only; `-v` ≈ 350 MB/day.
58. Two redundant `ft8_lib` doctor rows on release builds.
59. GUIDE/README say wizard writes "callsign, grid, audio, rig" — omit `[rig.frequency]`/`[rig.ptt]` and the 960-line dump.

## 4. Docs-vs-code discrepancy table

| Doc | Claim | Observed | Verdict |
|---|---|---|---|
| CONFIG.md:3-5, 511-525; RUNBOOK.md:296-299 | hot-reload; `Config reloaded: N keys updated` | no watcher in binary; string absent | **False** |
| CONFIG.md:25-41 | "minimum viable config" loads | `missing field data_bits` | **False** |
| GUIDE.md:228-273 | `[network.wsjtx_udp]` partial blocks | parse error → file discarded | **False** |
| README:107, GUIDE:142, RUNBOOK:171, CONFIG:44 | `[autonomous] enabled = true` | `missing field slot_parity` | **False** |
| CONFIG.md:284-290, 336-340 | psk_reporter/cqdx partials OK | PASS | True |
| CONFIG.md:162-163 | warns on unknown sections | default path only; nested never | Partial |
| CONFIG.md:116/118/119 | rig defaults `""`/`""`/`38400` | `"Generic"`/`"/dev/ttyUSB0"`/`9600` | **False** |
| CONFIG.md:480-481 | theme dark/light | default `"default"`; any string | Misleading |
| CONFIG.md:496-507 | env table; `--no-rig` | 13 undocumented vars; no `--no-rig` | **False** |
| CONFIG.md:540-542; TROUBLESHOOTING:49-52 | index rebuilt when missing | not on fresh drop-in | Partial |
| RUNBOOK:88,103 / :177 / :151 / :95-108 | `--list-audio` / `--show --path` / press `T` / verify via `--show` | none exist / `T` = tune / summary only | **False** |
| README:263 | `test-rig --ptt` keys TX | Serial only; CAT "not implemented" | Partial |
| README:165-173, 186-187 | 4 targets; not notarized | true (v0.9.5 lacked arm-linux, disclosed) | True |
| GUIDE.md:204 | Shift+M → FT2 | keymap: FT8→FT4 | Drift |
| bug_report.md | `logs/pancetta.log` | `pancetta.log.YYYY-MM-DD` | Wrong |
| SECURITY.md:24 / :48-49 / :36-37 / :74-75 | releases pending / env creds / `*_encrypted` gone / `bindings.rs` | exist / none / `secret_encrypted` remains / file gone | Stale ×4 |
| FEATURES.md:5 | >95% @ −20 dB, ~200 tests | README: −2.1 dB vs jt9, ~295 | Contradiction |
| `main.rs:49-71` | `<1ms`, `>95%`, `<100MB`, `<25%` | README disclaims | Fiction |
| `main.rs:186-188` | `identity` read-only | writes keypair | **False** |
| `--log-format`, `run --callsign…` | options | never read | Dead |
| CONTRIBUTING:48,51,74-80,370 | 1.70+, Discord, `--all`, 48 h | none true | Stale |
| config-and-platform.md:190-193 / :105-115 | inert sections "live" / de-brick on load failure | 0 runtime refs / validation-only | Overstated |
| launchd header | `~/.pancetta/launchd.*.log` | `/tmp/…` | Self-contradicting |
| CHANGELOG Unreleased | empty | 3 commits | Stale |
| README/GUIDE/FEATURES keybindings vs KEYBINDINGS.md | all keys | all match (except FT2) | True — SSOT works |

## 5. Five "world-class" moves

1. **Un-brickable then small config**: `#[serde(default)]` everywhere; per-section unknown-key sweep in both load paths; fatal parse error for the primary file with de-brick offer; `config --validate` non-zero on any warning; CI test executing every TOML snippet in `docs/`; `toml_edit` writes of non-default keys only (~15 lines, comments kept); `config --show` = effective diff. Collapses P0-1/2/3, P1-10/11/12, P2-16/23/30/33 into one promise.
2. **One-line install everywhere, Pi first-class**: `curl … | sh` (sha-verified → `~/.local/bin` → runs `doctor`), Homebrew tap auto-generated in `release.yml`, `cargo binstall` metadata, winget/Scoop, `.deb` for Pi OS (depends `libasound2 libhamlib-utils`, installs unit + completions), `pancetta upgrade`; Apple Developer ID + notarytool, cosign `sign-blob`, single `SHA256SUMS` + `.sig`, SBOM. Delete `build_release.sh`.
3. **`doctor` as nervous system**: verdicts `NOT CONFIGURED/NOT READY/DECODE-ONLY/ON-AIR READY` with distinct exit codes; `--json`; `--fix` for mechanical fixes; new checks (output device, `tqsl`, disk/recordings, rig model ∈ table, perms, port/CODEC held by another process); same checks at headless startup and in `Shift+S`; `support-bundle` = doctor json + info + redacted config + tail of log.
4. **Migration + data-files page**: `pancetta import <adif> [--from wsjtx|jtdx|…]` with dedupe; fix rebuild ordering; `export --since/--band/--mode`; `docs/DATA.md` inventory of `~/.pancetta` (what to back up, what's safe to delete); recorder off by default.
5. **CLI hygiene to `gh` standard**: global flags trimmed; dead flags gone; one true `about`; `completions`, man page, rich `--version`; color on TTY / `NO_COLOR`; ANSI off when piped; exit-code contract (0/1/2/3); every error ends with the fixing command; `trycmd`/`insta` snapshot suite that also executes the docs' command examples.

## 6. What is already excellent

Wizard validation/normalization (`karh`→rejected with example, `em13`→`EM13`, out-of-range power caught); real safe-by-default (rig→mock, autonomous off, N0CALL CQ refused, rig prompt defaults No, `--replay` refuses Hamlib/PSKReporter/UDP/agent and says so); de-brick offer on validation failure (clear, `[y/N]`, exit 1); atomic 0600/0700 config writes verified on disk; **no secret leakage** to logs or `--show` under `-v`; `--wav`/`--replay` as zero-hardware verification (self-terminating, exit 0, 26 real decodes) with the load-bearing `conflicts_with_all` comment; `doctor`'s checks-as-data, 40-line RFC 4330 SNTP, platform-specific fix hints, "startup would silently fall back" wording; release gate refusing stub-decoder binaries + real ARM fixture decode + written-down glibc rationale; `shasum -c`-compatible checksums; archive with README/CHANGELOG/licenses/notices; macOS binary links only system frameworks; README honesty (decoder gap, "Why not (yet)", `xattr` caveat, Bullseye caveat); keybinding SSOT actually holds across README/GUIDE/FEATURES; unknown-section warning on default path; WSJT-X ADIF parses cleanly once replay triggers (3 records → 2 bands, DXCC resolved); PR template's security checklist.

## Cross-references

- **Spec phases**: P0-1/2/3, P2-16 are the Phase 2 "silently ignored knob" class one level down (success metric "zero config keys documented-but-ignored"). P1-4/13/14/15, P2-22/24 are Phase 1/4 exit criteria ("no dead ends", "every failure visible with a printed fix"). Move #2 exceeds Phase 3's deliberately-deferred scope (config-and-platform.md:267-275).
- **`docs/DECISIONS/config-and-platform.md`** overstates in two places: inert sections "live" (P2-33) and de-brick on load failure (P1-12). `2026-07-development-phases-and-gaps.md` tracks nothing in this lane.
- **GitHub issues**: #265 ⊂ P1-6; #264 adjacent to P1-7; #275 adjacent to P2-32/P3-55; #141, #140 fixed.
- **Linear PAN** (100 recent): PAN-56 (MSRV) ↔ P2-37; PAN-60 (serial re-enumeration) ↔ P2-21. No PAN ticket covers install/first-run/CLI/docs/packaging — **every P0/P1 above is untracked.**