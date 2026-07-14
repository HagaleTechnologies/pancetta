# Onboarding Phase 1: Unbreak the Funnel — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A stranger following README.md verbatim reaches a running, decoding TUI with the real C decoder — or is told loudly at every divergence point what went wrong and how to fix it. No dead ends, no silent degradation, no brickable wizard.

**Architecture:** Six independent, individually shippable tasks: two doc-truth passes (README, CONTRIBUTING), one build-system fix (workspace default-members), one build.rs + pipeline observability fix (loud stub), and two wizard/startup hardening changes in `pancetta/src/main.rs` (prompt validation, de-brick on load failure). No new crates, no new dependencies.

**Tech Stack:** Rust stable, existing workspace. TUI surfacing reuses the existing `MessageType::DiagnosticEvent` bus variant (see `pancetta/src/message_bus.rs:160`) and the `emit_diagnostic` pattern (`pancetta/src/coordinator/tx.rs:358-380`).

## Global Constraints

- **Never make the ft8lib stub fallback fatal** — CI worktrees and research flows build without the submodule by design. Loud, not fatal.
- **Safe-by-default posture must not regress:** rig interface stays default-disabled, autonomous stays default-off, N0CALL CQ refusal untouched.
- The wizard remains TTY-gated exactly as today (`main.rs:674-679`); `--headless`, `--wav`, and piped-stdin invocations must never prompt.
- Existing behavior of `pancetta-config` validation is the contract — do NOT loosen `validate_callsign` / `validate_grid_square` (`pancetta-config/src/station.rs:275-362`); normalize input to satisfy them instead.
- All commits run the standard local gate: `cargo fmt --all` + `cargo clippy --workspace --features transmit` clean before each commit.
- Commit messages follow repo convention (`fix:`/`feat:`/`docs:` prefixes, imperative).

---

### Task 1: README quickstart made literally executable

**Files:**
- Modify: `README.md` (lines cited below from current main)
- Modify: `CLAUDE.md` (crate table)

**Interfaces:**
- Consumes: nothing.
- Produces: README commands that Task 5 makes even shorter (Task 5 enables bare `cargo run`; this task documents the `-p pancetta` form, which works both before and after Task 5 — keep `-p pancetta` in the final text for robustness).

- [ ] **Step 1: Fix the clone command (README.md:68-72)**

Replace the clone block with:

```bash
git clone --recursive https://github.com/HagaleTechnologies/pancetta.git
cd pancetta
cargo build --release
```

Immediately below it add:

```markdown
> Already cloned without `--recursive`? Run `git submodule update --init`
> before building — otherwise the build silently falls back to the pure-Rust
> decoder and you lose the C `ft8_lib` decode pass (the build will warn).
```

- [ ] **Step 2: Fix both run commands (README.md:87-93 and 120-133)**

In "3. Bootstrap your config", replace the bare `pancetta` command block with:

```bash
# First-run wizard (runs automatically on first launch)
./target/release/pancetta

# Or, equivalently:
cargo run --release -p pancetta

# Optional: put `pancetta` on your PATH
cargo install --path pancetta
```

In "4. Run", replace both `cargo run --release` occurrences with `cargo run --release -p pancetta`.

- [ ] **Step 3: Mark hamlib as optional-at-runtime (README.md:32-39)**

In the prerequisites table, change the Hamlib row label to `Hamlib (CAT control — optional; runtime-only)` and add under the table:

```markdown
Hamlib is **not** needed to build, or to run decode-only. Pancetta talks to
`rigctld` over TCP at runtime, and only when `[rig.interface].enabled = true`.
Install it when you're ready to key a radio.
```

- [ ] **Step 4: Fix crate counts (README.md:74, README.md:243-258, CLAUDE.md crate table)**

- README.md:74: "workspace is 12 crates" → "workspace is 14 crates".
- README.md:243: "11-crate Cargo workspace" → "14-crate Cargo workspace"; add the three missing rows to the table:

```markdown
| `pancetta-agent` | Remote-TX security: ArmState, session gating | 
| `pancetta-protocol` | Remote-operation wire protocol (no bus internals) |
| `pancetta-research` | Local-only decoder-iteration harness (excluded from CI) |
```

- README.md:285-287: fix the CI sentence to match `.github/workflows/ci.yml` reality: cross-platform `cargo check` runs on **macOS** (Windows lane was dropped 2026-05-23), and advisories are covered by `cargo deny` (there is no `cargo audit` step). Suggested text: "CI runs all of the above on every PR, plus a `cargo check` lane on macOS. `cargo deny check` runs on every push to catch security advisories and license drift."
- CLAUDE.md workspace table: add `pancetta-agent` (Remote-TX security: arm gating, session binding) and `pancetta-protocol` (remote-operation wire protocol) rows, and change "12-crate Cargo workspace" to "14-crate Cargo workspace". (`pancetta-research` is already listed there.)

- [ ] **Step 5: Verify every command in the README quickstart actually runs**

```bash
cargo run --release -p pancetta -- --help
./target/release/pancetta --help
```

Expected: both print the CLI help, exit 0. (Do NOT run the bare binary without flags — it would launch the TUI/wizard.)

- [ ] **Step 6: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs: make README quickstart literally executable (--recursive, -p pancetta, hamlib optional, 14 crates)"
```

---

### Task 2: Loud ft8lib-stub fallback (build-time + runtime + TUI)

**Files:**
- Modify: `pancetta-ft8/build.rs:32-36`
- Modify: `pancetta/src/coordinator/pipeline.rs:39-46`
- Test: manual build verification (build-script output is not unit-testable); existing `ft8lib_is_available()` API is the runtime seam.

**Interfaces:**
- Consumes: `pancetta_ft8::ft8lib_is_available() -> bool` (existing).
- Produces: a `DiagnosticEvent` with `target: "decode.engine"` — Phase 4's doctor command and health panel may later read the same target string; keep it exactly `"decode.engine"`.

- [ ] **Step 1: Emit cargo warnings in the stub fallback branch (build.rs)**

Replace the `else` branch body (`pancetta-ft8/build.rs:32-36`) with:

```rust
    } else {
        // ft8_lib vendor sources not found — using pure-Rust decoder instead.
        // Signal to the Rust code that stubs should be used instead of real FFI.
        // Loud but non-fatal: CI worktrees and research builds rely on this path.
        println!(
            "cargo:warning=ft8_lib C sources not found at pancetta-ft8/vendor/ft8_lib \
             — building WITHOUT the C decoder (ft8lib_stub, degraded decode recall)."
        );
        println!(
            "cargo:warning=Fix: git submodule update --init  (then rebuild)"
        );
        println!("cargo:rustc-cfg=ft8lib_stub");
    }
```

- [ ] **Step 2: Verify the warning fires**

```bash
mv pancetta-ft8/vendor/ft8_lib/ft8 /tmp/ft8_lib_ft8_backup 2>/dev/null || true
cargo build -p pancetta-ft8 2>&1 | grep "cargo:warning\|warning: pancetta-ft8"
mv /tmp/ft8_lib_ft8_backup pancetta-ft8/vendor/ft8_lib/ft8
cargo build -p pancetta-ft8 2>&1 | tail -2
```

Expected: first build prints both warnings; after restore, builds clean with no warnings. (If the submodule is not initialized in your worktree, skip the `mv` dance — the warning fires on a plain build.)

- [ ] **Step 3: Elevate the pipeline startup log and emit a DiagnosticEvent**

In `pancetta/src/coordinator/pipeline.rs`, the startup log at line 39 currently reports stub state at `info!`. Change to log at `warn!` when stubbed, and emit a retained diagnostic. Replace the `info!(...)` block (lines ~39-46) with:

```rust
        let ft8lib_native = pancetta_ft8::ft8lib_is_available();
        info!(
            "Pipeline starting: ft8_lib={}, audio_device={}",
            if ft8lib_native { "native-C" } else { "stub (pure-Rust only)" },
            if self.headless { "stub" } else { "real" },
        );
        if !ft8lib_native {
            warn!(
                "ft8_lib C decoder NOT compiled in (ft8lib_stub) — decode recall is degraded. \
                 Fix: git submodule update --init && cargo build --release"
            );
        }
```

Then, after the TUI bus channel is created (the `create_channel(ComponentId::Tui)` call around line 56), send the retained diagnostic so it lands in the Shift+D overlay. Follow the exact construction pattern of `emit_diagnostic` in `pancetta/src/coordinator/tx.rs:358-380` (a `ComponentMessage::new(<source component>, ComponentId::Tui, MessageType::DiagnosticEvent { .. }, Instant::now())` sent via `self.message_bus.send_message(...)`):

```rust
        if !ft8lib_native {
            let msg = ComponentMessage::new(
                ComponentId::Ft8Decoder,
                ComponentId::Tui,
                MessageType::DiagnosticEvent {
                    target: "decode.engine",
                    level: pancetta_core::DiagnosticLevel::Warn,
                    text: "ft8_lib C decoder not compiled in (stub build) — decode recall degraded. \
                           Fix: git submodule update --init, then rebuild."
                        .to_string(),
                    qso_id: None,
                    callsign: None,
                },
                Instant::now(),
            );
            if let Err(e) = self.message_bus.send_message(msg).await {
                tracing::debug!("stub-build DiagnosticEvent relay failed (no TUI?): {e}");
            }
        }
```

Adjust imports to match the file's existing `use` set (it already uses `ComponentId` and `MessageType`; add `pancetta_core::DiagnosticLevel` and `ComponentMessage`/`Instant` only if not already imported).

- [ ] **Step 4: Verify compile + existing tests**

```bash
cargo clippy -p pancetta --features transmit 2>&1 | tail -3
cargo test -p pancetta --test loopback_qso 2>&1 | tail -3
```

Expected: clippy clean, loopback tests pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/build.rs pancetta/src/coordinator/pipeline.rs
git commit -m "feat(build): make ft8lib_stub fallback loud — cargo warnings, startup warn, TUI diagnostic"
```

---

### Task 3: Wizard validates and normalizes callsign + grid at the prompt

**Files:**
- Modify: `pancetta/src/main.rs:770-798` (`setup_station`), plus new helpers + tests in the same file
- Test: `#[cfg(test)]` unit tests in `pancetta/src/main.rs` for the pure helpers

**Interfaces:**
- Consumes: `pancetta_config::Config::validate()` (pub, `pancetta-config/src/lib.rs:235`) — field-level validators are private, so validation goes through a whole-`Config` clone.
- Produces: `fn normalize_grid(raw: &str) -> String` and `fn try_set_station_field(config: &Config, field: StationField, value: &str) -> Result<Config, String>` used only within main.rs. `enum StationField { Callsign, Grid }`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)]` module in `pancetta/src/main.rs` (create one at the bottom if absent):

```rust
#[cfg(test)]
mod wizard_validation_tests {
    use super::*;
    use pancetta_config::Config;

    #[test]
    fn normalize_grid_uppercases_field_lowercases_subsquare() {
        assert_eq!(normalize_grid("fn42"), "FN42");
        assert_eq!(normalize_grid("fn42AB"), "FN42ab");
        assert_eq!(normalize_grid("FN42ab"), "FN42ab");
        assert_eq!(normalize_grid("fn42ab19"), "FN42ab19");
    }

    #[test]
    fn try_set_station_field_accepts_valid_and_rejects_invalid() {
        let cfg = Config::default();
        // Valid callsign, any case
        let ok = try_set_station_field(&cfg, StationField::Callsign, "k5arh").unwrap();
        assert_eq!(ok.station.callsign, "K5ARH");
        // Letters-only callsign must be rejected (validator requires a digit)
        assert!(try_set_station_field(&cfg, StationField::Callsign, "KARH").is_err());
        // Lowercase grid input must be normalized then accepted
        let ok = try_set_station_field(&cfg, StationField::Grid, "fn42").unwrap();
        assert_eq!(ok.station.grid_square, "FN42");
        // Garbage grid must be rejected, not stored
        assert!(try_set_station_field(&cfg, StationField::Grid, "12ab").is_err());
        assert!(try_set_station_field(&cfg, StationField::Grid, "FN4").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p pancetta --bin pancetta wizard_validation 2>&1 | tail -5
```

Expected: FAIL — `normalize_grid` / `try_set_station_field` not found.

- [ ] **Step 3: Implement the helpers**

Add above `setup_station` in `pancetta/src/main.rs`:

```rust
/// Maidenhead normalization: field+square uppercase (chars 0-3), subsquare
/// lowercase (chars 4-5), extended digits (6-7) untouched. Matches what
/// `pancetta-config`'s `validate_grid_square` requires.
fn normalize_grid(raw: &str) -> String {
    raw.trim()
        .char_indices()
        .map(|(i, c)| {
            if i < 4 {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum StationField {
    Callsign,
    Grid,
}

/// Apply a station-field edit on a clone and run full config validation.
/// Returns the updated Config on success, a user-facing message on failure.
/// Field-level validators in pancetta-config are private by design; whole-
/// config validate() is the public contract.
fn try_set_station_field(
    config: &Config,
    field: StationField,
    value: &str,
) -> std::result::Result<Config, String> {
    let mut candidate = config.clone();
    match field {
        StationField::Callsign => {
            candidate.station.callsign = value.trim().to_uppercase();
        }
        StationField::Grid => {
            candidate.station.grid_square = normalize_grid(value);
        }
    }
    candidate
        .validate()
        .map_err(|e| format!("{e}"))
        .map(|()| candidate)
}
```

- [ ] **Step 4: Rewire `setup_station` to loop-until-valid**

Replace the callsign and grid prompt blocks in `setup_station` (`main.rs:774-782`) with:

```rust
    loop {
        let input = prompt_line(&format!("  Callsign [{}]: ", config.station.callsign))?;
        if input.is_empty() {
            break;
        }
        match try_set_station_field(config, StationField::Callsign, &input) {
            Ok(updated) => {
                *config = updated;
                break;
            }
            Err(e) => println!("  Invalid callsign ({e}). Example: K5ARH — try again or Enter to skip."),
        }
    }

    loop {
        let input = prompt_line(&format!("  Grid square [{}]: ", config.station.grid_square))?;
        if input.is_empty() {
            break;
        }
        match try_set_station_field(config, StationField::Grid, &input) {
            Ok(updated) => {
                *config = updated;
                break;
            }
            Err(e) => println!("  Invalid grid ({e}). Example: FN42 or FN42ab — try again or Enter to skip."),
        }
    }
```

Note `setup_station` takes `config: &mut Config` — the `*config = updated` assignment replaces the whole struct; the power-watts block below stays unchanged.

- [ ] **Step 5: Belt-and-braces — validate before save in `run_first_time_setup`**

In `run_first_time_setup` (`main.rs:707-721`), immediately before the `prompt_yes_no("Save configuration to …")` block, add:

```rust
    if let Err(e) = new_config.validate() {
        println!();
        println!("WARNING: configuration is still invalid ({e}).");
        println!("Not saving — fix the value and re-run, or edit the file by hand.");
        return Ok(Some(new_config));
    }
```

(Returning `Some(new_config)` keeps this session running with the in-memory values, same as declining the save prompt today; nothing invalid reaches disk.)

- [ ] **Step 6: Run tests to verify they pass**

```bash
cargo test -p pancetta --bin pancetta wizard_validation 2>&1 | tail -5
cargo clippy -p pancetta --features transmit 2>&1 | tail -3
```

Expected: PASS ×2, clippy clean.

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/main.rs
git commit -m "fix(wizard): validate + normalize callsign/grid at the prompt; never save an invalid config"
```

---

### Task 4: Startup de-brick — offer the wizard when the saved config fails to load

**Files:**
- Modify: `pancetta/src/main.rs:653-682` (`load_configuration_with_warnings`)
- Test: manual scenario (interactive stdin cannot be unit-tested here); pure gating logic extracted and unit-tested

**Interfaces:**
- Consumes: `run_first_time_setup(&Config) -> Result<Option<Config>>` (existing, Task 3 hardened it), `Config::default()`, `prompt_yes_no` (existing).
- Produces: `fn offer_wizard_on_load_failure(headless: bool, wav: bool, interactive: bool) -> bool` (pure gate, tested).

- [ ] **Step 1: Write the failing test for the gate**

In the `wizard_validation_tests` module from Task 3, add:

```rust
    #[test]
    fn debrick_gate_only_fires_interactive_tui_runs() {
        assert!(offer_wizard_on_load_failure(false, false, true));
        assert!(!offer_wizard_on_load_failure(true, false, true)); // headless
        assert!(!offer_wizard_on_load_failure(false, true, true)); // --wav
        assert!(!offer_wizard_on_load_failure(false, false, false)); // piped stdin
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p pancetta --bin pancetta debrick_gate 2>&1 | tail -3
```

Expected: FAIL — function not defined.

- [ ] **Step 3: Implement gate + de-brick flow**

Add near the wizard helpers:

```rust
/// The de-brick wizard offer fires only for interactive TUI launches —
/// exactly the same gate as the first-run wizard (main.rs:675).
fn offer_wizard_on_load_failure(headless: bool, wav: bool, interactive: bool) -> bool {
    !headless && !wav && interactive
}
```

In `load_configuration_with_warnings`, replace the default-path load (`main.rs:658-661`):

```rust
    } else {
        match Config::load_default_with_warnings() {
            Ok(pair) => pair,
            Err(e)
                if offer_wizard_on_load_failure(
                    cli.headless,
                    cli.wav.is_some(),
                    std::io::stdin().is_terminal(),
                ) =>
            {
                eprintln!();
                eprintln!("ERROR: your saved configuration failed to load:");
                eprintln!("  {e}");
                eprintln!("  (file: ~/.pancetta/pancetta.toml)");
                eprintln!();
                if prompt_yes_no(
                    "Re-run first-time setup? (overwrites the broken config on save)",
                    false,
                )? {
                    let defaults = Config::default();
                    match run_first_time_setup(&defaults)? {
                        Some(fixed) => (fixed, Vec::new()),
                        None => {
                            return Err(anyhow::anyhow!(e))
                                .context("Failed to load default configuration")
                        }
                    }
                } else {
                    eprintln!("Edit the file by hand, or delete it to start fresh.");
                    return Err(anyhow::anyhow!(e)).context("Failed to load default configuration");
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e)).context("Failed to load default configuration")
            }
        }
    };
```

(If `ConfigError` already implements `std::error::Error + Send + Sync` — it does, it's a `thiserror` enum — `anyhow::anyhow!(e)` is fine; if the existing code used `?` with auto-conversion, match that idiom instead.)

Note: the first-run-wizard call further down (`main.rs:675-679`) stays unchanged; when the de-brick wizard already ran, `config.station.callsign != "N0CALL"` so it won't double-fire (and if the user skipped every field, re-offering the wizard is correct behavior anyway).

- [ ] **Step 4: Run tests + clippy**

```bash
cargo test -p pancetta --bin pancetta 2>&1 | tail -5
cargo clippy -p pancetta --features transmit 2>&1 | tail -3
```

Expected: all bin tests pass, clippy clean.

- [ ] **Step 5: Manual scenario verification**

```bash
SCRATCH=$(mktemp -d)
mkdir -p "$SCRATCH/.pancetta"
printf '[station]\ncallsign = "K5ARH"\ngrid_square = "fn42"\n' > "$SCRATCH/.pancetta/pancetta.toml"
HOME="$SCRATCH" ./target/debug/pancetta --headless 2>&1 | head -5   # headless: must NOT prompt, must error cleanly
printf 'n\n' | HOME="$SCRATCH" ./target/debug/pancetta 2>&1 | head -12  # piped stdin: not a TTY, must NOT prompt either
rm -rf "$SCRATCH"
```

Expected: headless run exits with the load error and no prompt; piped run likewise (gate requires a real TTY). Then verify interactively in a terminal if available: same broken config, run `./target/debug/pancetta`, confirm the "Re-run first-time setup?" prompt appears and completing it overwrites the file.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/main.rs
git commit -m "fix(startup): offer the setup wizard instead of exiting when the saved config fails validation"
```

---

### Task 5: Bare `cargo run` disambiguation

**Files:**
- Modify: `Cargo.toml:18-32` (workspace `default-members`)

**Interfaces:**
- Consumes: nothing. Produces: bare `cargo run`/`cargo build` at the workspace root target the main binary set without `pancetta-audio`'s helper bin colliding.

- [ ] **Step 1: Confirm which packages expose binaries**

```bash
grep -l "\[\[bin\]\]\|src/main.rs" pancetta-audio/Cargo.toml; ls pancetta-audio/src/main.rs 2>/dev/null
```

Expected: `pancetta-audio` has a binary (this is the collision source). If it does NOT, stop — re-diagnose the `cargo run` ambiguity before touching anything.

- [ ] **Step 2: Remove `pancetta-audio` from `default-members`**

In the root `Cargo.toml`, delete the `"pancetta-audio",` line from `default-members` (NOT from `members`). Result: `cargo run` at the root has exactly one binary candidate; `cargo build --workspace` / `cargo test --workspace` still cover pancetta-audio (the `--workspace` flag overrides default-members).

- [ ] **Step 3: Verify**

```bash
cargo run --release -- --help 2>&1 | head -3        # must now work bare
cargo build 2>&1 | tail -1                           # default build OK
cargo test -p pancetta-audio --lib 2>&1 | tail -2    # audio crate still tests fine explicitly
```

Expected: `--help` prints; builds green. Also confirm CI is unaffected: `.github/workflows/ci.yml` uses `--workspace` flags (verify with `grep -n "workspace" .github/workflows/ci.yml`).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git commit -m "fix(build): drop pancetta-audio bin from default-members so bare 'cargo run' works"
```

---

### Task 6: CONTRIBUTING.md truth pass

**Files:**
- Modify: `CONTRIBUTING.md` (bottom half; keep lines ~143-361 — the clean-room, check.sh, and CI sections are accurate and good)

**Interfaces:** none.

- [ ] **Step 1: Fix every fabricated/wrong item**

Read the current file, then apply ALL of the following (line numbers from current main):

1. Lines 62, 115, 540: `github.com/pancetta-project/pancetta` → `github.com/HagaleTechnologies/pancetta` (the `upstream` remote instructions and any other org references).
2. Line ~538: delete the Discord link (`discord.gg/pancetta` does not exist). Replace the "Getting help" contact list with: GitHub Issues (bugs/features) and GitHub Discussions if enabled — verify with `gh api repos/HagaleTechnologies/pancetta --jq .has_discussions`; if false, Issues only.
3. Lines ~545-547: delete the placeholder maintainer list (`[@username](https://github.com/username)` ×3). Replace with: "Maintained by Hagale Technologies (K5ARH)."
4. Remove references to nonexistent `CONTRIBUTORS.md` and `./scripts/pre-submit.sh` (the real gate is `scripts/check.sh` — point to it).
5. Line ~511: "Two approvals required for merge" → "PRs are reviewed and merged by the maintainer. CI (fmt, clippy, full test suite, cargo-deny) must be green."
6. Line ~558: "licensed under the same license as the project (MIT)" → "dual-licensed under MIT OR Apache-2.0, the same terms as the project (see README §License)."

- [ ] **Step 2: Verify no dead references remain**

```bash
grep -n "pancetta-project\|discord.gg\|CONTRIBUTORS.md\|pre-submit.sh\|@username" CONTRIBUTING.md
```

Expected: no matches.

- [ ] **Step 3: Commit**

```bash
git add CONTRIBUTING.md
git commit -m "docs: replace fabricated CONTRIBUTING boilerplate with real project facts"
```

---

## Final gate (after all tasks)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test --workspace --features transmit 2>&1 | tail -5
```

Expected: all green. Then push the branch and open a PR titled "Onboarding Phase 1: unbreak the clone→run funnel" referencing `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md`.
