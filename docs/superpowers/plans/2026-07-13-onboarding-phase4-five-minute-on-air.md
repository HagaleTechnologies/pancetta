# Onboarding Phase 4: The Five-Minute On-Air Path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new user goes from "build finished" to **on the air** — configured, rig connected, able to TX a QSO — in under 5 minutes, and every failure on that path (wrong input device, dead clock, missing hamlib, unknown rig model, stub decoder) is visible with a printed fix instead of a silent log line. This phase implements the operator's clarified goal: **5 minutes measured from build-complete to on-air.**

**Architecture:** Eight tasks in three groups. (1) *Configure fast:* the first-run wizard gains a skippable rig step by reusing the three rig helpers that already exist for `pancetta setup` (`setup_rig` / `setup_ptt` / `setup_frequency` in `pancetta/src/main.rs` — they are free functions taking `&mut Config`, directly callable; no refactor needed). (2) *Diagnose fast:* a new `pancetta doctor` subcommand (new module `pancetta/src/doctor.rs`) runs seven independent checks as a `Vec<DoctorCheck>` of data — Phase-5 additions are one-liners; plus symmetric failure surfacing so the TUI's AUDIO and RIG badges always carry a reason (input-device fallback mirrors the existing output-side mechanism; rigctld spawn/model/connect failures route through `MessageType::Error` + a retained `DiagnosticEvent`). (3) *Learn fast:* `docs/GUIDE.md` (task-oriented stranger guide, full content in Task 6), the hidden CLI surface documented in README, and a stopwatch drill script defining the acceptance criterion.

**Tech Stack:** Rust stable, existing workspace. **No new dependencies** — SNTP is ~40 lines over `std::net::UdpSocket`; TCP probe via `std::net::TcpStream::connect_timeout`; serial enumeration via the already-present `serialport` crate; audio via `pancetta_audio::{AudioManager, AudioManagerConfig}` (re-exported at crate root, `pancetta-audio/src/lib.rs:42`). TUI surfacing reuses `MessageType::Error` (relayed at `pancetta/src/coordinator/tui_relay.rs:521-537`) and `MessageType::DiagnosticEvent` (`pancetta/src/message_bus.rs:160-170`, `target: &'static str`).

## Global Constraints

- **TX-safety invariants untouched:** rig interface stays default-disabled (`[rig.interface] enabled = false`), N0CALL CQ refusal untouched, armed-TX gate untouched. The wizard's rig prompt defaults to **No**.
- **Wizard remains TTY-gated** exactly as today (`main.rs:674-679`): `--headless`, `--wav`, and piped-stdin invocations never prompt. The new rig step lives *inside* `run_first_time_setup`, behind the existing gate.
- **`pancetta doctor` must work with NO config file** — every config-dependent check degrades to `WARN … not configured — run the wizard`, never panics, never prompts.
- **Never make the ft8lib stub fatal** — doctor reports it as `WARN` (soft), not `FAIL`.
- **Build on Phase 1, don't duplicate it:** Phase 1 (`docs/superpowers/plans/2026-07-12-onboarding-phase1-funnel.md`) owns callsign/grid prompt validation (`normalize_grid`, `try_set_station_field`, `StationField`) and the validate-before-save guard in `run_first_time_setup`, plus startup de-brick. This plan adds no callsign/grid logic and assumes those helpers exist (nothing here calls them, so the tasks also apply cleanly if Phase 1 lands later; `main.rs` line numbers cited below are from the current branch — anchor by function name if Phase 1 shifted them).
- Every commit passes the standard local gate: `cargo fmt --all` + `cargo clippy --workspace --features transmit` clean.
- New code in `main.rs`/`doctor.rs` follows the existing helper style (`prompt_line` / `prompt_yes_no` / `prompt_choice`, `main.rs:738-768`).
- Commit messages follow repo convention (`feat:`/`fix:`/`docs:` prefixes, imperative).

---

### Task 1: Wizard rig step — the wizard finally covers what README says it covers

**Files:**
- Modify: `pancetta/src/main.rs` (`run_first_time_setup`, currently lines 686-732)

**Interfaces:**
- Consumes: `setup_rig(&mut Config) -> Result<()>` (`main.rs:882-967` — rig enable, model, **serial-port enumeration via `serialport::available_ports()` with USB product/manufacturer detail**, baud picker), `setup_ptt(&mut Config) -> Result<()>` (`main.rs:969-996` — None/CAT/Serial/VOX picker), `setup_frequency(&mut Config) -> Result<()>` (`main.rs:998-1014`), `prompt_yes_no` (`main.rs:747`). All three setup fns are already free functions used by `setup_command` (`main.rs:1016-1076`) — **reuse them directly; do not copy any prompt logic.**
- Produces: a first-run wizard whose flow is station → audio → **rig (skippable)** → save, with a "To set up CAT later: pancetta setup" closing hint on the skip path.

- [ ] **Step 1: Insert the rig step after the audio block**

In `run_first_time_setup`, immediately after the `setup_audio` block (the `if prompt_yes_no("Configure audio input/output devices now? (recommended)", true)? { setup_audio(&mut new_config)?; }` block at `main.rs:700-705`), insert:

```rust
    // Rig / CAT control — the other half of the on-air path. Skippable:
    // decode-only needs no rig, and the rig interface stays disabled by
    // default (safe-by-default posture). Reuses the exact same helpers as
    // `pancetta setup`, including serial-port enumeration.
    if prompt_yes_no(
        "Configure rig CAT control now? (skip = decode-only for now)",
        false,
    )? {
        setup_rig(&mut new_config)?;
        if new_config.rig.interface.enabled {
            setup_ptt(&mut new_config)?;
            setup_frequency(&mut new_config)?;
        }
    }
```

Note the default is **No** — pressing Enter through the whole wizard still produces a decode-only, never-transmits config.

- [ ] **Step 2: Make the closing summary state the rig outcome (and the later-setup hint)**

Replace the closing `println!` block of `run_first_time_setup` (currently `main.rs:723-729`, the `Station: … / Setup complete!` lines) with:

```rust
    println!();
    println!(
        "Station: {} / {} / {}W",
        new_config.station.callsign, new_config.station.grid_square, new_config.station.power_watts
    );
    if new_config.rig.interface.enabled {
        println!(
            "Rig:     {} on {} @ {} (PTT: {:?})",
            new_config.rig.model,
            new_config.rig.interface.port,
            new_config.rig.interface.baud_rate,
            new_config.rig.ptt.method
        );
        println!("         Verify the link any time with: pancetta test-rig");
    } else {
        println!("Rig:     not configured — decode-only (no PTT).");
        println!("         To set up CAT later: pancetta setup");
    }
    println!("Setup complete! Starting Pancetta...");
    println!();
```

- [ ] **Step 3: Build + non-interactive safety verification**

```bash
cargo build -p pancetta 2>&1 | tail -2
SCRATCH=$(mktemp -d)
HOME="$SCRATCH" ./target/debug/pancetta --headless 2>&1 | head -5   # must NOT prompt (headless)
printf '' | HOME="$SCRATCH" ./target/debug/pancetta 2>&1 | head -5   # piped stdin: not a TTY, must NOT prompt
rm -rf "$SCRATCH"
```

Expected: builds clean; neither invocation shows any wizard prompt (the TTY gate is untouched).

- [ ] **Step 4: Manual interactive verification (real terminal)**

```bash
SCRATCH=$(mktemp -d); HOME="$SCRATCH" ./target/debug/pancetta
```

Walk the wizard twice: (a) answer `n` at the rig prompt → summary shows `Rig: not configured` + the `pancetta setup` hint; (b) re-run (delete `$SCRATCH/.pancetta/pancetta.toml` first), answer `y` → the `--- Rig Control ---` section from `pancetta setup` appears, including the serial-port list. Confirm `[rig]` / `[rig.interface]` / `[rig.ptt]` land in the saved TOML. Confirm README.md:82-85 ("…walks you through writing a `~/.pancetta/pancetta.toml` containing your callsign, grid square, audio device names, and rig model") is now literally true — no README edit needed.

- [ ] **Step 5: Gate + commit**

```bash
cargo fmt --all && cargo clippy --workspace --features transmit 2>&1 | tail -3
git add pancetta/src/main.rs
git commit -m "feat(wizard): add skippable rig/CAT step to first-run setup (reuses pancetta-setup helpers)"
```

---

### Task 2: `pancetta doctor` — framework + the four offline checks

**Files:**
- Create: `pancetta/src/doctor.rs`
- Modify: `pancetta/src/main.rs` (`Commands` enum at 125-145, `handle_command` at 364-379, `mod doctor;` declaration)
- Test: `#[cfg(test)]` module inside `pancetta/src/doctor.rs`

**Interfaces:**
- Consumes: `pancetta_config::Config::{load_from_file, validate}` (`pancetta-config/src/lib.rs:229,235`), `pancetta_audio::device::list_input_devices() -> Vec<(String, bool)>` (`pancetta-audio/src/device.rs:662`, same fn `test-audio --list` uses via `list_audio_devices`, `main.rs:460-490`), `pancetta_ft8::ft8lib_is_available() -> bool` (`pancetta-ft8/src/lib.rs:131`), `dirs::home_dir()`.
- Produces (Task 3 and Phase 5 build on these exact names): `pub enum CheckStatus { Pass, Warn, Fail }`, `pub struct CheckOutcome { status, detail: String, fix: Option<String> }`, `pub struct DoctorCheck { name: &'static str, hard: bool, run: Box<dyn Fn(&DoctorCtx) -> CheckOutcome> }`, `pub struct DoctorCtx { config_path: PathBuf, config: Option<Config>, config_error: Option<String> }`, `pub fn build_ctx(Option<PathBuf>) -> DoctorCtx`, `pub fn build_checks() -> Vec<DoctorCheck>`, `pub fn doctor_exit_code(&[(bool, CheckStatus)]) -> i32`, `pub fn status_label(CheckStatus) -> &'static str`.

- [ ] **Step 1: Write the failing tests**

Create `pancetta/src/doctor.rs` containing only the test module for now:

```rust
//! `pancetta doctor` — fast, independent station-health checks, each with a
//! one-line fix. The universal first answer to "it doesn't work" and the
//! first line of every troubleshooting doc. Checks are data
//! ([`DoctorCheck`] entries in [`build_checks`]) so adding one is a
//! one-liner for later phases.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exit_code_is_nonzero_only_for_hard_failures() {
        use CheckStatus::*;
        assert_eq!(doctor_exit_code(&[(true, Pass), (false, Fail)]), 0); // soft fail ok
        assert_eq!(doctor_exit_code(&[(true, Warn), (false, Warn)]), 0); // warns ok
        assert_eq!(doctor_exit_code(&[(true, Fail)]), 1); // hard fail
        assert_eq!(doctor_exit_code(&[]), 0);
    }

    #[test]
    fn config_check_degrades_when_no_config_file() {
        let ctx = build_ctx(Some(PathBuf::from("/nonexistent/dir/pancetta.toml")));
        let outcome = (check_config().run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Warn);
        assert!(outcome.detail.contains("not configured"));
        assert!(outcome.fix.unwrap().contains("wizard"));
    }

    #[test]
    fn config_check_fails_hard_on_unparseable_file() {
        let dir = std::env::temp_dir().join(format!("pancetta-doctor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        std::fs::write(&path, "[station\nthis is not toml").unwrap();
        let ctx = build_ctx(Some(path));
        let outcome = (check_config().run)(&ctx);
        assert_eq!(outcome.status, CheckStatus::Fail);
        assert!(check_config().hard);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Add `mod doctor;` to `main.rs` (immediately after the `use pancetta_lib::coordinator::ApplicationCoordinator;` line), then:

```bash
cargo test -p pancetta --bin pancetta doctor 2>&1 | tail -5
```

Expected: FAIL — `doctor_exit_code` / `build_ctx` / `check_config` not found.

- [ ] **Step 3: Implement the framework and the four offline checks**

Fill in `pancetta/src/doctor.rs` above the test module:

```rust
use pancetta_config::Config;
use std::path::PathBuf;

/// Outcome status of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// Ready.
    Pass,
    /// Not OK, but not fatal to a decode-only station (or unverifiable here).
    Warn,
    /// Broken. If the check is `hard`, doctor exits non-zero.
    Fail,
}

/// Result of running one check: one line of detail, one line of fix.
pub struct CheckOutcome {
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

/// One doctor check, kept as data so later phases add checks as one-liners
/// in [`build_checks`]. `hard` checks gate the exit code; soft checks print.
pub struct DoctorCheck {
    pub name: &'static str,
    pub hard: bool,
    pub run: Box<dyn Fn(&DoctorCtx) -> CheckOutcome>,
}

/// Shared context: the config is loaded ONCE here so each check stays fast
/// and independent. `config` is `Some` whenever the file parsed, even if
/// validation failed (`config_error` carries the parse/validate error text)
/// so later checks can still use e.g. the device names.
pub struct DoctorCtx {
    pub config_path: PathBuf,
    pub config: Option<Config>,
    pub config_error: Option<String>,
}

pub fn build_ctx(config_override: Option<PathBuf>) -> DoctorCtx {
    let config_path = config_override.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".pancetta")
            .join("pancetta.toml")
    });
    if !config_path.exists() {
        return DoctorCtx { config_path, config: None, config_error: None };
    }
    match Config::load_from_file(&config_path) {
        Ok(cfg) => {
            let config_error = cfg.validate().err().map(|e| e.to_string());
            DoctorCtx { config_path, config: Some(cfg), config_error }
        }
        Err(e) => DoctorCtx { config_path, config: None, config_error: Some(e.to_string()) },
    }
}

/// The check registry. Order = print order. Phase-5 additions: append here.
pub fn build_checks() -> Vec<DoctorCheck> {
    vec![
        check_config(),
        check_audio_device(),
        check_decoder(),
        check_submodule(),
    ]
}

pub fn status_label(s: CheckStatus) -> &'static str {
    match s {
        CheckStatus::Pass => "PASS",
        CheckStatus::Warn => "WARN",
        CheckStatus::Fail => "FAIL",
    }
}

/// Non-zero iff any HARD check FAILed. Warns and soft fails never gate.
pub fn doctor_exit_code(results: &[(bool, CheckStatus)]) -> i32 {
    if results.iter().any(|(hard, s)| *hard && *s == CheckStatus::Fail) {
        1
    } else {
        0
    }
}

fn not_configured() -> CheckOutcome {
    CheckOutcome {
        status: CheckStatus::Warn,
        detail: "not configured — run the wizard".to_string(),
        fix: Some("run `pancetta` (first-run wizard) or `pancetta setup`".to_string()),
    }
}

pub(crate) fn check_config() -> DoctorCheck {
    DoctorCheck {
        name: "config",
        hard: true,
        run: Box::new(|ctx| {
            if !ctx.config_path.exists() {
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!(
                        "not configured — {} does not exist",
                        ctx.config_path.display()
                    ),
                    fix: Some("run `pancetta` (first-run wizard) or `pancetta setup`".to_string()),
                };
            }
            if let Some(e) = &ctx.config_error {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("does not load/validate: {e}"),
                    fix: Some(
                        "run `pancetta setup` (rewrites the file) or fix the TOML by hand"
                            .to_string(),
                    ),
                };
            }
            let cfg = ctx.config.as_ref().expect("config present when no error recorded");
            if cfg.station.callsign == "N0CALL" {
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "valid, but callsign is still N0CALL (CQ will be refused)".to_string(),
                    fix: Some("run `pancetta setup` and set your callsign".to_string()),
                };
            }
            CheckOutcome {
                status: CheckStatus::Pass,
                detail: format!(
                    "{} / {} — loads and validates",
                    cfg.station.callsign, cfg.station.grid_square
                ),
                fix: None,
            }
        }),
    }
}

pub(crate) fn check_audio_device() -> DoctorCheck {
    DoctorCheck {
        name: "audio input device",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else { return not_configured() };
            let wanted = &cfg.audio.input_device;
            let inputs = pancetta_audio::device::list_input_devices();
            if inputs.is_empty() {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: "no audio input devices found on this host".to_string(),
                    fix: Some("plug in the rig's USB CODEC, then re-run".to_string()),
                };
            }
            if wanted.eq_ignore_ascii_case("default") {
                let name = inputs
                    .iter()
                    .find(|(_, is_default)| *is_default)
                    .map(|(n, _)| n.as_str())
                    .unwrap_or("<none>");
                return CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!(
                        "using system default input ('{name}') — works, but fragile for a rig"
                    ),
                    fix: Some(
                        "set [audio] input_device to the rig CODEC name \
                         (run `pancetta test-audio --list`)"
                            .to_string(),
                    ),
                };
            }
            // Same resolution semantics as the live audio stream: case-
            // insensitive SUBSTRING match (find_input_device_by_name).
            let found = inputs
                .iter()
                .any(|(n, _)| n.to_lowercase().contains(&wanted.to_lowercase()));
            if found {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: format!("'{wanted}' present"),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("'{wanted}' NOT present (startup would silently fall back)"),
                    fix: Some(
                        "run `pancetta test-audio --list` and copy the exact name into \
                         [audio] input_device"
                            .to_string(),
                    ),
                }
            }
        }),
    }
}

pub(crate) fn check_decoder() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib decoder",
        // Soft by invariant: the stub build is degraded, never fatal.
        hard: false,
        run: Box::new(|_ctx| {
            if pancetta_ft8::ft8lib_is_available() {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "native C decoder compiled in".to_string(),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "STUB build — pure-Rust decoder only (degraded decode recall)"
                        .to_string(),
                    fix: Some("git submodule update --init && cargo build --release".to_string()),
                }
            }
        }),
    }
}

pub(crate) fn check_submodule() -> DoctorCheck {
    DoctorCheck {
        name: "ft8_lib submodule",
        hard: false,
        run: Box::new(|_ctx| {
            // Only meaningful when run from a source-checkout root — probes
            // the same path pancetta-ft8/build.rs uses at compile time
            // (vendor/ft8_lib/ft8/constants.c).
            if !std::path::Path::new("pancetta-ft8").exists() {
                return CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "not run from a source checkout (skipped)".to_string(),
                    fix: None,
                };
            }
            if std::path::Path::new("pancetta-ft8/vendor/ft8_lib/ft8/constants.c").exists() {
                CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "vendor sources present".to_string(),
                    fix: None,
                }
            } else {
                CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: "submodule not initialized — the NEXT build will be a stub build"
                        .to_string(),
                    fix: Some("git submodule update --init".to_string()),
                }
            }
        }),
    }
}
```

- [ ] **Step 4: Wire the subcommand into clap**

In `main.rs`, add to the `Commands` enum (after `Setup` at line ~140):

```rust
    /// Check station health: config, clock, audio, decoder, rig — with a
    /// printed fix for every failure. Run this before your first session.
    Doctor,
```

Add the arm in `handle_command` (after `Commands::Setup => …` at line ~375):

```rust
        Commands::Doctor => doctor_command(cli).await,
```

And add the command fn near `setup_command`:

```rust
async fn doctor_command(cli: &Cli) -> Result<()> {
    let ctx = doctor::build_ctx(cli.config.clone());
    let checks = doctor::build_checks();
    println!();
    println!("pancetta doctor — {} checks (config: {})", checks.len(), ctx.config_path.display());
    println!();
    let mut results = Vec::with_capacity(checks.len());
    for check in &checks {
        let outcome = (check.run)(&ctx);
        println!(
            "[{}] {:<20} {}",
            doctor::status_label(outcome.status),
            check.name,
            outcome.detail
        );
        if outcome.status != doctor::CheckStatus::Pass {
            if let Some(fix) = &outcome.fix {
                println!("       fix: {fix}");
            }
        }
        results.push((check.hard, outcome.status));
    }
    println!();
    if doctor::doctor_exit_code(&results) != 0 {
        println!("Result: NOT READY — fix the FAIL lines above, then re-run `pancetta doctor`.");
        std::process::exit(1);
    }
    println!("Result: ready. Start `pancetta` — you should see decodes within ~30 s.");
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass, plus a live no-config run**

```bash
cargo test -p pancetta --bin pancetta doctor 2>&1 | tail -5
SCRATCH=$(mktemp -d)
HOME="$SCRATCH" ./target/debug/pancetta doctor; echo "exit=$?"
rm -rf "$SCRATCH"
```

Expected: 3 tests PASS. Live run prints WARN `not configured — run the wizard` for config + audio (never a panic, never a prompt), `exit=0` (warns don't gate).

- [ ] **Step 6: Gate + commit**

```bash
cargo fmt --all && cargo clippy --workspace --features transmit 2>&1 | tail -3
git add pancetta/src/doctor.rs pancetta/src/main.rs
git commit -m "feat(doctor): add 'pancetta doctor' subcommand — check framework + config/audio-device/decoder/submodule checks"
```

---

### Task 3: `pancetta doctor` — clock (hand-rolled SNTP), audio level, rigctld

**Files:**
- Modify: `pancetta/src/doctor.rs` (three new checks + SNTP helpers)
- Modify: `pancetta/src/coordinator/hamlib.rs:60` (one-word visibility change)
- Test: same `#[cfg(test)]` module in `doctor.rs`

**Interfaces:**
- Consumes: `std::net::UdpSocket` / `TcpStream` (std only, no new crate); `pancetta_audio::{AudioManager, AudioManagerConfig}` (`with_config`/`start`/`process_audio`/`stop`, `pancetta-audio/src/manager.rs:130,201,244,223`); the coordinator's rigctld conventions — env `RIGCTLD_HOST` (default `127.0.0.1`) / `RIGCTLD_PORT` (default `4532`) and the `rigctld` binary spawn (`pancetta/src/coordinator/hamlib.rs:150-155,222-251`); `ApplicationCoordinator::hamlib_model_id` (`hamlib.rs:60-78`) — **currently `pub(crate)`; make it `pub` so the doctor (in the bin, which links `pancetta_lib` as an external crate) can call it** (same targeted-exposure pattern as the `panic_count` re-export).
- Produces: `pub(crate) fn sntp_clock_offset(server: &str, timeout: Duration) -> anyhow::Result<f64>`, pure helpers `ntp_ts_to_unix_f64(&[u8]) -> f64` and `sntp_offset(t1, t2, t3, t4) -> f64` (tested), checks `check_clock()`, `check_audio_level()`, `check_rigctld()` registered in `build_checks`.

- [ ] **Step 1: Write the failing tests for the pure SNTP math**

Add to the `tests` module in `doctor.rs`:

```rust
    #[test]
    fn ntp_timestamp_conversion_handles_epoch_and_fraction() {
        // NTP epoch is 1900-01-01; Unix is 1970-01-01; delta 2_208_988_800 s.
        let unix_zero: [u8; 8] = [0x83, 0xAA, 0x7E, 0x80, 0, 0, 0, 0]; // 2_208_988_800.0
        assert_eq!(ntp_ts_to_unix_f64(&unix_zero), 0.0);
        // +1 second and a half-fraction (0x8000_0000 / 2^32 = 0.5) → 1.5.
        let one_and_half: [u8; 8] = [0x83, 0xAA, 0x7E, 0x81, 0x80, 0, 0, 0];
        assert!((ntp_ts_to_unix_f64(&one_and_half) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn sntp_offset_recovers_skew_independent_of_symmetric_delay() {
        // Local clock 5 s slow, 200 ms symmetric round trip:
        // T1=100.0 (local send), T2=T3=105.1 (server), T4=100.2 (local recv).
        let offset = sntp_offset(100.0, 105.1, 105.1, 100.2);
        assert!((offset - 5.0).abs() < 1e-9);
        // Zero skew, only delay → offset 0.
        assert!(sntp_offset(100.0, 100.1, 100.1, 100.2).abs() < 1e-9);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p pancetta --bin pancetta sntp 2>&1 | tail -3
cargo test -p pancetta --bin pancetta ntp_timestamp 2>&1 | tail -3
```

Expected: FAIL — functions not defined.

- [ ] **Step 3: Implement the SNTP client (hand-rolled, ~40 lines, std-only)**

Add to `doctor.rs`:

```rust
use std::time::Duration;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_EPOCH_DELTA: f64 = 2_208_988_800.0;

/// Convert an 8-byte NTP timestamp (32.32 fixed point, seconds since 1900)
/// to Unix seconds as f64.
pub(crate) fn ntp_ts_to_unix_f64(b: &[u8]) -> f64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as f64 / 4_294_967_296.0;
    secs + frac - NTP_UNIX_EPOCH_DELTA
}

/// RFC 4330 clock offset from the four timestamps:
/// T1 local send, T2 server receive, T3 server transmit, T4 local receive.
/// Positive = the local clock is BEHIND the server.
pub(crate) fn sntp_offset(t1: f64, t2: f64, t3: f64, t4: f64) -> f64 {
    ((t2 - t1) + (t3 - t4)) / 2.0
}

fn unix_now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// One-shot SNTP (RFC 4330) query over UDP. Returns the clock offset in
/// seconds. Hand-rolled on purpose: a 48-byte packet is not worth a crate.
pub(crate) fn sntp_clock_offset(server: &str, timeout: Duration) -> anyhow::Result<f64> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.connect(server)?;

    // 48-byte client request: LI=0, VN=4, Mode=3 (client) → first byte 0x23.
    let mut req = [0u8; 48];
    req[0] = 0x23;
    let t1 = unix_now_f64();
    sock.send(&req)?;
    let mut resp = [0u8; 48];
    let n = sock.recv(&mut resp)?;
    let t4 = unix_now_f64();
    anyhow::ensure!(n >= 48, "short SNTP response ({n} bytes)");
    let mode = resp[0] & 0x07;
    anyhow::ensure!(mode == 4 || mode == 5, "not an SNTP server response (mode {mode})");
    let t2 = ntp_ts_to_unix_f64(&resp[32..40]); // Receive timestamp
    let t3 = ntp_ts_to_unix_f64(&resp[40..48]); // Transmit timestamp
    anyhow::ensure!(t3 > 0.0, "SNTP transmit timestamp is zero");
    Ok(sntp_offset(t1, t2, t3, t4))
}
```

- [ ] **Step 4: Implement the three checks and register them**

Add to `doctor.rs` (and change `build_checks` to `vec![check_config(), check_clock(), check_audio_device(), check_audio_level(), check_decoder(), check_rigctld(), check_submodule()]`):

```rust
fn clock_fix_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "System Settings → General → Date & Time → 'Set time automatically'"
    } else if cfg!(target_os = "windows") {
        "run `w32tm /resync` in an admin prompt (or Settings → Time & Language → Sync now)"
    } else {
        "enable an NTP daemon: `sudo apt install chrony` (or systemd-timesyncd)"
    }
}

pub(crate) fn check_clock() -> DoctorCheck {
    DoctorCheck {
        name: "system clock",
        hard: true,
        run: Box::new(|_ctx| {
            match sntp_clock_offset("pool.ntp.org:123", Duration::from_secs(2)) {
                // FT8 slots are UTC-aligned; past ~1 s decodes fail systematically.
                Ok(offset) if offset.abs() < 1.0 => CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: format!("offset {offset:+.3} s vs pool.ntp.org"),
                    fix: None,
                },
                Ok(offset) => CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("offset {offset:+.2} s — FT8 needs < ~1 s from UTC"),
                    fix: Some(clock_fix_hint().to_string()),
                },
                Err(e) => CheckOutcome {
                    status: CheckStatus::Warn,
                    detail: format!("could not reach pool.ntp.org ({e}) — clock unverified"),
                    fix: Some("check network; FT8 needs the clock within ~1 s of UTC".to_string()),
                },
            }
        }),
    }
}

/// Capture `dur` of input audio and return (rms, peak) of the samples.
fn capture_rms(cfg: &Config, dur: Duration) -> anyhow::Result<(f32, f32)> {
    use pancetta_audio::{AudioManager, AudioManagerConfig};
    let mut mgr = AudioManager::with_config(AudioManagerConfig {
        input_device: Some(cfg.audio.input_device.clone()),
        output_device: Some(cfg.audio.output_device.clone()),
        sample_rate: cfg.audio.sample_rate,
        buffer_size: cfg.audio.buffer_size as usize,
        channels: cfg.audio.input_channels as u16,
        ..Default::default()
    })?;
    mgr.start()?;
    let deadline = std::time::Instant::now() + dur;
    let (mut sum_sq, mut n, mut peak) = (0f64, 0usize, 0f32);
    while std::time::Instant::now() < deadline {
        match mgr.process_audio()? {
            Some(samples) => {
                for s in &samples {
                    sum_sq += (*s as f64) * (*s as f64);
                    peak = peak.max(s.abs());
                }
                n += samples.len();
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let _ = mgr.stop();
    anyhow::ensure!(n > 0, "no samples captured in {dur:?}");
    Ok(((sum_sq / n as f64).sqrt() as f32, peak))
}

pub(crate) fn check_audio_level() -> DoctorCheck {
    DoctorCheck {
        name: "audio level (2 s)",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else { return not_configured() };
            match capture_rms(cfg, Duration::from_secs(2)) {
                Ok((rms, peak)) => {
                    let dbfs = 20.0 * rms.max(1e-9).log10();
                    if peak < 1e-6 {
                        CheckOutcome {
                            status: CheckStatus::Fail,
                            detail: "FLAT LINE — no signal reaching the input at all".to_string(),
                            fix: Some(
                                "wrong input device, OS-muted, or rig AF off — \
                                 `pancetta test-audio --list`, check OS input level, \
                                 turn up the rig's USB audio out"
                                    .to_string(),
                            ),
                        }
                    } else {
                        CheckOutcome {
                            status: CheckStatus::Pass,
                            detail: format!("rms {dbfs:.0} dBFS, peak {peak:.3}"),
                            fix: None,
                        }
                    }
                }
                Err(e) => CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!("could not capture: {e}"),
                    fix: Some(
                        "run `pancetta test-audio --list`; close other apps holding the device \
                         (including a running pancetta)"
                            .to_string(),
                    ),
                },
            }
        }),
    }
}

fn hamlib_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install hamlib"
    } else if cfg!(target_os = "windows") {
        "install hamlib from https://hamlib.github.io and put rigctld.exe on PATH"
    } else {
        "sudo apt install libhamlib-utils"
    }
}

pub(crate) fn check_rigctld() -> DoctorCheck {
    DoctorCheck {
        name: "rig / rigctld",
        hard: true,
        run: Box::new(|ctx| {
            let Some(cfg) = &ctx.config else { return not_configured() };
            if !cfg.rig.interface.enabled {
                return CheckOutcome {
                    status: CheckStatus::Pass,
                    detail: "rig disabled — decode-only (mock rig, no PTT)".to_string(),
                    fix: Some("to go on-air: `pancetta setup` and enable rig control".to_string()),
                };
            }
            // Model must map to a hamlib ID or the coordinator can't spawn rigctld
            // (hamlib.rs:252-258 warns and gives up).
            if pancetta_lib::coordinator::ApplicationCoordinator::hamlib_model_id(&cfg.rig.model)
                .is_none()
            {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: format!(
                        "unknown rig model '{}' — pancetta cannot spawn rigctld for it",
                        cfg.rig.model
                    ),
                    fix: Some(
                        "run `pancetta setup` and set a supported model (FTdx10, IC-7300, …), \
                         or run an external rigctld and set RIGCTLD_HOST/RIGCTLD_PORT"
                            .to_string(),
                    ),
                };
            }
            // Binary on PATH — pancetta spawns `rigctld` at startup (hamlib.rs:222).
            let on_path = std::process::Command::new("rigctld")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok();
            if !on_path {
                return CheckOutcome {
                    status: CheckStatus::Fail,
                    detail: "rig enabled but `rigctld` is not on PATH".to_string(),
                    fix: Some(hamlib_install_hint().to_string()),
                };
            }
            // Same host/port conventions as the coordinator (hamlib.rs:150-155).
            let host = std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
            let port: u16 = std::env::var("RIGCTLD_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(4532);
            use std::net::ToSocketAddrs;
            let reachable = format!("{host}:{port}")
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .map(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok())
                .unwrap_or(false);
            let detail = if reachable {
                format!("rigctld on PATH; already listening on {host}:{port}")
            } else {
                format!("rigctld on PATH; nothing on {host}:{port} yet (pancetta spawns it at startup)")
            };
            CheckOutcome { status: CheckStatus::Pass, detail, fix: None }
        }),
    }
}
```

- [ ] **Step 5: Make `hamlib_model_id` public**

In `pancetta/src/coordinator/hamlib.rs:60`, change `pub(crate) fn hamlib_model_id` to `pub fn hamlib_model_id` and extend its doc comment: `/// Public so `pancetta doctor` (in the bin crate) can pre-validate the configured model against the same table the spawner uses.`

- [ ] **Step 6: Run tests + live run**

```bash
cargo test -p pancetta --bin pancetta doctor 2>&1 | tail -5
cargo build -p pancetta && ./target/debug/pancetta doctor; echo "exit=$?"
```

Expected: 5 doctor tests PASS. Live run (dev machine has a config): clock check PASSes with a small offset, audio-level check prints a real dBFS figure, all 7 checks print one line each. Also verify degradation: `HOME=$(mktemp -d) ./target/debug/pancetta doctor` → config/audio/level/rig all WARN `not configured`, exit 0.

- [ ] **Step 7: Gate + commit**

```bash
cargo fmt --all && cargo clippy --workspace --features transmit 2>&1 | tail -3
git add pancetta/src/doctor.rs pancetta/src/coordinator/hamlib.rs
git commit -m "feat(doctor): clock (hand-rolled SNTP), 2s audio-level RMS/flat-line, and rigctld/model checks"
```

---

### Task 4: Input-device fallback gets the output side's TUI error + badge

**Files:**
- Modify: `pancetta-audio/src/stream.rs` (field near line 106, getter near 142, resolution at 300-319)
- Modify: `pancetta-audio/src/manager.rs` (passthrough near line 184)
- Modify: `pancetta/src/coordinator/mod.rs` (field near 800, init near 1230)
- Modify: `pancetta/src/coordinator/audio.rs` (clone near 170, check after 229, reopen re-eval after 322)
- Modify: `pancetta/src/coordinator/tui_relay.rs` (clone near 155, push near 657)
- Modify: `pancetta-tui/src/tui_runner.rs` (variant near 252, handler near 789)
- Modify: `pancetta-tui/src/app.rs` (field near 946, init near 1120)
- Modify: `pancetta-tui/src/ui/station_info.rs` (badge after 268)

**Interfaces:**
- Consumes: the output-side template end-to-end: `output_is_system_default` (`stream.rs:106/142/507` → `manager.rs:184` → `coordinator/audio.rs:219-229` + reopen re-eval at 316-322 → `audio_output_default: Arc<AtomicBool>` in `coordinator/mod.rs:800` → relay push `tui_relay.rs:648-657` → `TuiMessage::AudioOutputDefault` → `app.tx_output_default` → badge `station_info.rs:260-268`). `report_audio_error` closure (`coordinator/audio.rs:173-192`) for the visible one-shot error.
- Produces: `AudioStreamManager::input_fallback(&self) -> Option<(String, String)>` (requested, resolved), `AudioManager::input_device_fallback(&self) -> Option<(String, String)>`, `TuiMessage::AudioInputFallback { active: bool }`, `app.input_fallback: bool`.

- [ ] **Step 1: Record the fallback in `pancetta-audio/src/stream.rs`**

Add a field after `output_is_system_default` (line ~106):

```rust
    /// Set when the configured INPUT device name did not match any present
    /// device and capture silently fell back to the best available input
    /// (`(requested, resolved)` names). Mirror of `output_is_system_default`
    /// for the RX side; recomputed each time the input stream is (re)created.
    input_fallback: Option<(String, String)>,
```

Initialize `input_fallback: None,` in `new()` (next to `output_is_system_default: false,` line ~133). Add the getter after `output_is_system_default()` (line ~144):

```rust
    /// If the configured input device was not found and capture fell back,
    /// returns `(requested_name, resolved_name)`. Only meaningful after the
    /// input stream has been created.
    pub fn input_fallback(&self) -> Option<(String, String)> {
        self.input_fallback.clone()
    }
```

Then in `create_input_stream`, replace the device-resolution block (lines 299-319) with:

```rust
        // Get input device — recording whether we silently fell back so the
        // coordinator can surface it (previously warn!-only; the classic
        // "decoding the built-in mic instead of the rig" trap).
        let mut fallback_from: Option<String> = None;
        let input_device = if let Some(ref device_name) = self.config.input_device_name {
            if device_name.eq_ignore_ascii_case("default") {
                self.device_manager.get_best_ft8_input_device()?
            } else {
                // Find device by name substring match (case-insensitive).
                // Falls back to best available if no match found.
                match self.device_manager.find_input_device_by_name(device_name) {
                    Ok(device) => device,
                    Err(_) => {
                        tracing::warn!(
                            "Input device matching '{}' not found, falling back to best available",
                            device_name
                        );
                        fallback_from = Some(device_name.clone());
                        self.device_manager.get_best_ft8_input_device()?
                    }
                }
            }
        } else {
            self.device_manager.get_best_ft8_input_device()?
        };
        let resolved_input_name = input_device
            .name()
            .unwrap_or_else(|_| "<unknown>".to_string());
        self.input_fallback = fallback_from.map(|req| (req, resolved_input_name));
```

(Field-disjoint borrows: `input_device` borrows only `self.device_manager`; assigning `self.input_fallback` is a different field — same pattern the existing `self.shared = …` lines already rely on.)

- [ ] **Step 2: Passthrough in `pancetta-audio/src/manager.rs`**

After `output_is_system_default()` (line ~189):

```rust
    /// If the configured INPUT device was not found and capture fell back to
    /// the best available input, returns `(requested, resolved)` device
    /// names. Only meaningful after [`start`](Self::start). RX-side mirror
    /// of [`output_is_system_default`](Self::output_is_system_default).
    pub fn input_device_fallback(&self) -> Option<(String, String)> {
        self.stream.as_ref().and_then(|s| s.input_fallback())
    }
```

- [ ] **Step 3: Coordinator flag + error, mirroring the output block**

`pancetta/src/coordinator/mod.rs`: next to `audio_output_default` (line ~800) add:

```rust
    /// Latched true when the configured audio INPUT device was not found and
    /// capture fell back to another device — RX-side mirror of
    /// `audio_output_default`. Drives a persistent TUI badge via the relay.
    audio_input_fallback: Arc<AtomicBool>,
```

and in the constructor (line ~1230): `audio_input_fallback: Arc::new(AtomicBool::new(false)),`

`pancetta/src/coordinator/audio.rs`: next to line 170 add `let audio_input_fallback = self.audio_input_fallback.clone();`. Immediately after the output-misconfig `if/else` (after line 229) add:

```rust
                // RX-input fallback: the configured input device wasn't found
                // and capture silently fell back (pancetta-audio warns to the
                // log only — stream.rs). Mirror the output-side surfacing:
                // one-shot TUI error + latched badge flag.
                if let Some((requested, resolved)) = audio_manager.input_device_fallback() {
                    audio_input_fallback.store(true, Ordering::Relaxed);
                    report_audio_error(format!(
                        "RX audio input '{requested}' NOT FOUND — capturing from '{resolved}' \
                         instead. You may be decoding the wrong device (e.g. the built-in \
                         mic). Fix [audio] input_device (run `pancetta test-audio --list`)."
                    ));
                } else {
                    audio_input_fallback.store(false, Ordering::Relaxed);
                }
```

And in the live-reopen success arm, right after the existing `audio_output_default.store(…)` (lines 319-322), add:

```rust
                                audio_input_fallback.store(
                                    audio_manager.input_device_fallback().is_some(),
                                    Ordering::Relaxed,
                                );
```

- [ ] **Step 4: Relay + TUI plumbing**

`pancetta/src/coordinator/tui_relay.rs`: next to line 155 add `let audio_input_fallback_relay = self.audio_input_fallback.clone();`; next to the `last_audio_default` declaration (grep `let mut last_audio_default`) add `let mut last_input_fallback: Option<bool> = None;`; after the output-badge push block (line ~657) add:

```rust
                    // RX-input fallback badge — push only when it changes.
                    let input_fb = audio_input_fallback_relay.load(Ordering::Relaxed);
                    if last_input_fallback != Some(input_fb) {
                        last_input_fallback = Some(input_fb);
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::AudioInputFallback {
                                active: input_fb,
                            },
                        );
                    }
```

`pancetta-tui/src/tui_runner.rs`: add the variant next to `AudioOutputDefault` (line ~252):

```rust
    /// The configured `[audio] input_device` was not found and RX capture is
    /// running on a fallback device (RX mirror of `AudioOutputDefault`).
    AudioInputFallback { active: bool },
```

and the handler next to the `AudioOutputDefault` arm (line ~789):

```rust
            TuiMessage::AudioInputFallback { active } => {
                app.input_fallback = active;
            }
```

`pancetta-tui/src/app.rs`: next to `tx_output_default` (line ~946) add `/// RX capture fell back to a device other than the configured one.` `pub input_fallback: bool,` and init `input_fallback: false,` next to line 1120.

`pancetta-tui/src/ui/station_info.rs`: after the `tx_output_default` badge block (line ~268):

```rust
    // Persistent warning badge when the configured input device was not
    // found and RX capture fell back (the "decoding the built-in mic" trap).
    if app.input_fallback {
        device_first_line.push(Span::raw("  "));
        device_first_line.push(Span::styled(
            "⚠ RX→fallback device",
            Style::default()
                .fg(app.theme.error_color())
                .add_modifier(Modifier::BOLD),
        ));
    }
```

- [ ] **Step 5: Verify — build, tests, manual fault injection**

```bash
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test -p pancetta-audio --lib 2>&1 | tail -3
cargo test -p pancetta --test loopback_qso 2>&1 | tail -3
```

Then manual (real terminal): scratch config with `[audio] input_device = "NoSuchDevice2026"`, run `./target/debug/pancetta` → expect the `RX audio input 'NoSuchDevice2026' NOT FOUND…` error line, the `⚠ RX→fallback device` badge in Station Info, and decodes still flowing from the fallback device. Fix the name via the `d` device picker → badge clears (reopen re-eval path).

- [ ] **Step 6: Commit**

```bash
git add pancetta-audio/src/stream.rs pancetta-audio/src/manager.rs pancetta/src/coordinator/mod.rs pancetta/src/coordinator/audio.rs pancetta/src/coordinator/tui_relay.rs pancetta-tui/src/tui_runner.rs pancetta-tui/src/app.rs pancetta-tui/src/ui/station_info.rs
git commit -m "feat(audio): surface RX input-device fallback as TUI error + persistent badge (mirrors TX-output misconfig path)"
```

---

### Task 5: rigctld failures reach the TUI — the RIG ✗ badge always has a reason

**Files:**
- Modify: `pancetta/src/coordinator/hamlib.rs` (helper + four call sites: ~188-197, ~245-251, ~252-258, ~322-325)

**Interfaces:**
- Consumes: `MessageBus` (Clone, `message_bus.rs:923`; a clone is already moved into the connect task at `hamlib.rs:133` and remains in scope at the connect-failure site), `MessageType::Error { component_id, error_message, error_code }` (relayed to the TUI error log at `tui_relay.rs:521-537`), `MessageType::DiagnosticEvent` (retained; Shift+D overlay), `hamlib_install_hint()` — **duplicated here from doctor.rs deliberately** (3-line cfg chain; the two crates' modules don't share a private helper without a new public surface). Start order is safe: `start_pipeline()` (which creates the TUI channel) runs before `start_hamlib_component()` (`coordinator/mod.rs:1276-1280`).
- Produces: `async fn report_rig_error(&MessageBus, String)` in `hamlib.rs`, emitting both an ephemeral `Error` and a retained `DiagnosticEvent` with `target: "rig.cat"`.

- [ ] **Step 1: Add the helper (bottom of `hamlib.rs`, outside the `impl`)**

Extend the existing import line 16 to `use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};`, then add:

```rust
/// Platform-appropriate hamlib install hint (mirrors `pancetta doctor`).
fn hamlib_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install hamlib"
    } else if cfg!(target_os = "windows") {
        "install hamlib from https://hamlib.github.io and put rigctld.exe on PATH"
    } else {
        "sudo apt install libhamlib-utils"
    }
}

/// Surface a rig-setup failure to the operator: an ephemeral TUI error line
/// (`MessageType::Error` → error log/status) PLUS a retained
/// `DiagnosticEvent` (target `rig.cat`, Shift+D overlay) so the reason for a
/// RIG ✗ badge survives after the status line scrolls away. Previously these
/// failures were `warn!`-to-file only — invisible under the TUI's alternate
/// screen. Sends are best-effort (`let _ =`): headless runs have no TUI channel.
async fn report_rig_error(message_bus: &MessageBus, text: String) {
    let err = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::Error {
            component_id: ComponentId::Hamlib,
            error_message: text.clone(),
            error_code: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(err).await;
    let diag = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target: "rig.cat",
            level: pancetta_core::DiagnosticLevel::Warn,
            text,
            qso_id: None,
            callsign: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(diag).await;
}
```

- [ ] **Step 2: Call it at all four failure sites**

(1) Suspicious port refusal — inside the `if !port_field.is_empty() && !Self::device_path_looks_safe(port_field)` block (lines 188-197), after the existing `warn!` and before `return Ok(());`:

```rust
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Rig CAT disabled: suspicious [rig.interface].port '{port_field}' — \
                         run `pancetta setup` to pick a real serial port."
                    ),
                )
                .await;
```

(2) Spawn failure — in the `Err(e)` arm of the `std::process::Command::new("rigctld")…spawn()` match (lines 245-251), replace the body with:

```rust
                    Err(e) => {
                        warn!(
                            "Failed to spawn rigctld: {}. Install hamlib: {}",
                            e,
                            hamlib_install_hint()
                        );
                        report_rig_error(
                            &self.message_bus,
                            format!(
                                "rigctld failed to start ({e}) — no CAT/PTT this session. \
                                 Fix: {}",
                                hamlib_install_hint()
                            ),
                        )
                        .await;
                    }
```

(3) Unknown model — in the trailing `else` arm (lines 252-258), after the existing `warn!`:

```rust
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Unknown rig model '{}' — cannot spawn rigctld (no CAT/PTT). \
                         Run `pancetta setup` to pick a supported model, or run an \
                         external rigctld and set RIGCTLD_HOST/RIGCTLD_PORT.",
                        rig_config.model
                    ),
                )
                .await;
```

(4) Connect failure — in the spawned task's `Err(e)` arm of `rig.connect()` (lines 322-325; the `message_bus` clone from line 133 is in scope), after the `rig_conn_state.store(…)`:

```rust
                        if rig_enabled {
                            report_rig_error(
                                &message_bus,
                                format!(
                                    "Rig connect failed ({e}) — RIG badge is ✗. Check the radio \
                                     is powered on and the USB cable; verify with \
                                     `pancetta test-rig`, then restart pancetta."
                                ),
                            )
                            .await;
                        }
```

(The `rig_enabled` guard keeps the mock-rig path silent — a disabled rig is not an error.)

- [ ] **Step 3: Verify — build + manual fault injection**

```bash
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test -p pancetta --lib 2>&1 | tail -3
```

Manual (real terminal): scratch config with `[rig.interface] enabled = true`, `port = "/dev/cu.nonexistent"`, `model = "FluxCapacitor9000"` → run → expect the "Unknown rig model" error in the TUI error log AND a `rig.cat` entry in the Shift+D diagnostics overlay, with RIG showing ✗. Then set `model = "FTdx10"` with hamlib temporarily off PATH (`PATH=/usr/bin:/bin ./target/debug/pancetta`) → expect the spawn-failure message with the platform hint.

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/hamlib.rs
git commit -m "feat(rig): surface rigctld spawn/model/connect failures to the TUI with install hints — RIG ✗ always has a reason"
```

---

### Task 6: `docs/GUIDE.md` — the task-oriented stranger guide

**Files:**
- Create: `docs/GUIDE.md`

**Interfaces:** consumes everything shipped above (`doctor`, wizard rig step). Every keybinding below is verified against the handlers in `pancetta-tui/src/tui_runner.rs:1320-1680` and the `?` overlay (`tui_runner.rs:1952-2010`); config keys against the real config structs (note: the guide uses the REAL `[autonomous]` schema — CONFIG.md's `[autonomous_operator]` block is a known Phase-2 defect and must not be copied); compliance language against `docs/fcc-part97-compliance.md`.

- [ ] **Step 1: Write the file with exactly this content**

`````markdown
# Pancetta User Guide

For the licensed ham who just built pancetta and wants to get on the air.
This guide is task-oriented: your first 5 minutes, your first QSO, then
"how do I…" answers. (Owner-operator procedures live in `docs/RUNBOOK.md`;
every config key lives in `docs/CONFIG.md`.)

---

## Your first 5 minutes

You've run `cargo build --release` (with `--recursive` on the clone — the
build warns if the C decoder is missing). Now:

### 1. Run the wizard (~2 minutes)

```bash
./target/release/pancetta
```

On first run (no config yet) pancetta walks you through:

- **Station** — callsign, Maidenhead grid (e.g. `FN42`), TX power.
- **Audio** — pick your rig's USB CODEC from a numbered list for **both**
  input and output. This is the #1 thing to get right: the wrong input
  device means zero decodes; the wrong output device means PTT keys the
  rig while your FT8 tones play through the laptop speakers.
- **Rig CAT control** — answer `y` if your radio is connected by USB now.
  Pancetta lists your serial ports (with USB product names), asks for the
  rig model (e.g. `FTdx10`, `IC-7300`), baud rate (38400 for a Yaesu
  FTdx10), and PTT method (**CAT** is right for most modern rigs).
  Answer `n` to stay decode-only; you can add the rig any time with
  `pancetta setup`.

Everything is saved to `~/.pancetta/pancetta.toml`.

### 2. Run the doctor (~10 seconds)

```bash
pancetta doctor
```

Seven independent checks — config, system clock vs NTP, audio input
device, a 2-second audio-level capture, the C decoder, rigctld, and the
git submodule — each with a one-line fix when it fails. **Green doctor =
you will decode.** The two classic first-run killers it catches:

- **Clock offset ≥ 1 s.** FT8 slots are aligned to UTC; a drifted clock
  fails every decode while looking otherwise healthy.
- **Flat-line audio.** The input device exists but no signal reaches it
  (OS-muted, wrong device, rig AF at zero).

### 3. Start and watch the first decode (~30 seconds)

```bash
./target/release/pancetta
```

Tune the rig to an FT8 dial frequency (20 m: **14.074 MHz**, USB). Decodes
arrive in bursts at the end of each 15-second slot — within two slots the
**Band Activity** panel should fill. If it doesn't: `pancetta doctor`
again, then `Shift+D` (diagnostics history) and `Shift+S` (station health)
inside the TUI.

That's the whole path. With a rig configured you are now one keypress from
transmitting.

---

## Your first QSO

FT8 exchanges are fixed-format and take ~1 minute. Pancetta runs the
sequence for you; you pick the station.

### Reading the Band Activity panel

Each row is one decoded transmission: UTC time, SNR (dB), time offset,
audio frequency, and the message. `CQ` rows are stations asking to be
called. Move with `Up`/`Down` (or jump panels with `1`–`5`:
Band Activity / QSO Status / Callers / DX Hunter / TX Placement).

### Calling: Space

Select a CQ row and press **Space**. Pancetta answers with your grid in
the next appropriate slot and then runs the standard exchange (your
callsign here shown as K1ABC):

```
CQ W1AW FN31          ← they call CQ
W1AW K1ABC EM13       ← you answer with your grid       (pancetta sends)
K1ABC W1AW -07        ← they send your report
W1AW K1ABC R-12       ← you roger + their report        (pancetta sends)
K1ABC W1AW RR73       ← they confirm; QSO is complete
W1AW K1ABC 73         ← courtesy sign-off               (pancetta sends)
```

Watch it in the **QSO Status** panel (`2`). The QSO logs automatically to
`~/.pancetta/qsos.adi` (+ the query index `qso.db`). Space is
context-aware: if the selected station is already mid-exchange with you,
it re-sends the *correct next message*, not your grid.

If someone answers **your** CQ, they appear in the **Callers** panel
(`3`) — select and press **Enter** to reply at the right step.

### Keys you must know before transmitting

| Key | Effect |
|---|---|
| `h` | **Halt current TX** — drops PTT within ~150 ms |
| `Shift+Q` | **EMERGENCY STOP** — abort TX, autonomous off, TX policy → Disabled |
| `g` | Cycle TX policy: **Full → Respond-only → Disabled** |
| `k` | Abort the selected QSO (QSO Status panel only) |
| `r` | Re-send your last message in the selected QSO (QSO Status panel only) |
| `Esc` | Clear the stop banner / dismiss any overlay |

Pancetta refuses to CQ as `N0CALL`, the rig interface is **disabled by
default**, and autonomous mode is **off by default** — nothing transmits
until you configured a rig and pressed a key that means "transmit".

---

## How do I…

### …work a specific DX station?

The **DX Hunter** panel (`4`) scores every decoded station by what *you*
need (new DXCC, grid, POTA/SOTA, rarity). Select and press **Space** — same
exchange as above, but pancetta places your TX and picks the slot parity to
match theirs. `c` starts a repeating CQ of your own; `s` stops it.

### …hold my TX frequency vs. letting pancetta pick?

- `f` toggles **HOLD** (your offset is pinned) vs **AUTO** (pancetta picks
  a clear one).
- `t` auto-finds a clear 25 Hz-aligned offset and moves your cursor there.
- `←`/`→` or `[`/`]` nudge the TX offset ±50 Hz; `o` types an exact offset
  in Hz (200–2900; blank = back to Auto; setting one implies Hold).
- `Shift+F` sets the **dial** frequency (and optional split TX dial) via CAT.

### …enable autonomous operation (the supervised way)?

Press `a` to toggle autonomous mode (or set `[autonomous] enabled = true`
in `~/.pancetta/pancetta.toml`; `Shift+P` pauses/resumes). Pancetta then
hunts, calls, completes, and logs QSOs using the priority weights under
`[autonomous.priorities]` (`needed_dxcc`, `needed_grid`, `pota_sota`,
`rarity`, `signal_strength`, penalties — see `docs/CONFIG.md`).

**Compliance framing (US operators; see
`docs/fcc-part97-compliance.md`):** with you present and able to
intervene — at the keyboard, or watching over SSH/screen share — this is
*local/remote control* under §97.109 and is fully compliant on the normal
FT8 frequencies, including originating CQ. **Unattended** operation is
*automatic control* (§97.109(d)), and the standard FT8 frequencies are
outside §97.221(b)'s automatic-control segments — so an unattended station
must at most **respond** to calls (§97.221(c)), never originate CQ.
Practical rule: **stay present while autonomous runs** (the ARRL's
contemporaneous-initiation posture), and if you must step away, press `g`
until the policy reads **Respond-only** — or `Shift+Q` to stop TX
entirely. The licensee remains responsible either way (§97.103).

### …upload my logs (QRZ / LoTW / ClubLog)?

Every QSO is appended to `~/.pancetta/qsos.adi` (durable ADIF — back it
up). For live per-QSO uploads, enable the blocks in
`~/.pancetta/pancetta.toml` (then `chmod 600` the file — credentials are
plaintext):

```toml
[network.qrz_logbook]
enabled = true
api_key = ""        # logbook Settings → API access key (per-logbook key)

[network.clublog]
enabled  = true
email    = ""       # your ClubLog account email
password = ""       # an Application Password is recommended
callsign = ""       # empty = each QSO's own station call
api_key  = ""       # ClubLog application API key
```

**LoTW:** per-QSO upload is deferred (LoTW requires TQSL signing, not a
raw ADIF POST) — point TQSL/WSJT-X at `~/.pancetta/qsos.adi`. Bulk export
with filters: `pancetta export --output mylog.adi`.

### …switch bands or modes?

- `=` / `-` step the band up/down (CAT moves the dial; active QSOs are
  torn down safely — turning the rig's physical dial does the same).
- `Shift+F` for any arbitrary dial frequency, including split.
- `Shift+M` cycles the station operating mode (FT8/FT4). It can be refused
  while a QSO is in flight — finish or `k` the QSO first.
- `e` cycles decode effort (Eco → Standard → Deep → Max → Auto) if you're
  CPU-bound.

### …use Hound mode (work a DXpedition Fox)?

Select the Fox in the DX Hunter panel (`4`) and press **Shift+H**.
Pancetta engages the WSJT-X Fox/Hound convention: calls above 1000 Hz,
then obeys the Fox's QSY instruction after being called. `Shift+X`
toggles Fox mode itself (running your own pileup — read
`docs/superpowers/specs/` on Fox mode before using this in anger).

### …see why something isn't working?

1. `pancetta doctor` from a shell — config, clock, audio, decoder, rig.
2. `Shift+D` in the TUI — retained diagnostics history (why TX was
   dropped, why a QSO failed, rig errors).
3. `Shift+S` — the station-health panel (one screen: is the station
   healthy right now?).
4. Badges in Station Info: `⚠ TX→system default` (TX audio going to your
   speakers, not the rig) and `⚠ RX→fallback device` (configured input
   device not found).
5. Logs: `~/.pancetta/logs/` (daily rotation, 14 kept).

Press `?` any time for the complete key list.
`````

- [ ] **Step 2: Verify every claim against the source**

```bash
grep -n "KeyCode::Char('g')\|KeyCode::Char('e')\|KeyCode::Char('M')\|KeyCode::Char('H')\|KeyCode::Char('X')\|KeyCode::Char('a')\|KeyCode::Char('P')" pancetta-tui/src/tui_runner.rs
grep -n "qrz_logbook\|clublog\|api_key" docs/CONFIG.md | head
grep -n "pub enabled" pancetta-config/src/autonomous.rs
```

Expected: every key named in the guide has a handler; every TOML key exists in CONFIG.md / the config structs. Fix any drift in the guide, not the code.

- [ ] **Step 3: Commit**

```bash
git add docs/GUIDE.md
git commit -m "docs: add GUIDE.md — task-oriented stranger guide (first 5 minutes, first QSO, how-do-I sections)"
```

---

### Task 7: Document the hidden CLI surface in README (+ link the guide)

**Files:**
- Modify: `README.md` (new section after "How to drive the TUI", ~line 188; Documentation list at 291-301)

**Interfaces:** consumes the clap definitions in `main.rs:125-246` (verified below). **Omit** `benchmark` and `test-audio --device/--duration` — both print "not yet implemented" and exit 1 (`main.rs:446-454, 551-554`); documenting them would create a Phase-1-class dead end.

- [ ] **Step 1: Add the CLI section**

Insert after the "Decode-effort control" section (before "## Troubleshooting"):

```markdown
---

## Command-line tools

Everything below ships in the one `pancetta` binary (`pancetta <cmd> --help`
for details):

| Command | What it does |
|---|---|
| `pancetta` | Run the station (TUI). First run launches the setup wizard. |
| `pancetta doctor` | Check station health — config, clock vs NTP, audio device + level, decoder, rigctld — with a printed fix per failure. Run it whenever something "doesn't work". |
| `pancetta setup` | Interactive wizard for station, audio, rig, PTT, and frequency control. Safe to re-run any time. |
| `pancetta test-audio --list` | List audio input/output devices exactly as pancetta sees them (copy names into `[audio]`). |
| `pancetta test-rig` | Test the rig link: serial port present, opens, data readable. Add `--ptt` to key TX for 1 s (careful!). |
| `pancetta config --validate` | Validate the config file and exit non-zero on errors (also `--show`, `--generate <path>`). |
| `pancetta export --output log.adi` | Export logged QSOs to ADIF (`--callsign` to filter). |
| `pancetta info` | Version and host capabilities. |
| `pancetta benchmark-decode <wav-or-dir>` | Compare the native decoder against ft8_lib on WAV captures. |
| `pancetta --wav <file>` | Decode a 15-s WAV file and exit (no audio hardware needed). |
| `pancetta --headless` | Run without the TUI (logs to `~/.pancetta/logs/`). |
```

- [ ] **Step 2: Link the guide from the Documentation section**

At the top of the Documentation list (README.md:291-301) add:

```markdown
- [`docs/GUIDE.md`](docs/GUIDE.md) — **start here**: your first 5 minutes, your first QSO, and how-do-I recipes.
```

- [ ] **Step 3: Verify each documented command actually runs**

```bash
for c in "doctor" "test-audio --list" "config --validate" "info" "--help"; do
  ./target/debug/pancetta $c >/dev/null 2>&1; echo "$c -> $?"
done
./target/debug/pancetta export --help >/dev/null && ./target/debug/pancetta test-rig --help >/dev/null && ./target/debug/pancetta benchmark-decode --help >/dev/null && echo "help ok"
```

Expected: every listed command exits 0 (or, for `config --validate` with a broken config, a *documented* non-zero) — nothing prints "not implemented".

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(readme): document the full CLI surface (doctor, setup, test-rig, test-audio, config, export, --wav) and link GUIDE.md"
```

---

### Task 8: The five-minute drill — scripted, stopwatch-timed acceptance test

**Files:**
- Create: `scripts/five-minute-drill.sh` (mode 755)

**Interfaces:** consumes the finished Tasks 1-7. This script IS the Phase-4 exit criterion: **build-complete → wizard → doctor green → decoding, with a rig attached and TX-capable, in ≤ 5:00.**

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# five-minute-drill.sh — stopwatch for pancetta's post-build onboarding path.
#
# The operator's 5-minute goal, measured from the moment the build finishes:
# a new user configures the station and is ON THE AIR (rig connected, able
# to TX a QSO) within 5:00. Run this with a stopwatch mindset on a machine
# with a freshly built binary, the rig on USB, and NO ~/.pancetta config
# (the script offers to move an existing one aside).
#
# PASS: wizard + doctor-green + first decode within 300 s.
#       (TX-capable = the rig step was completed and doctor's rig check PASSes.)
# FAIL: any step dead-ends, or total time > 300 s.
set -u

BIN="${PANCETTA_BIN:-./target/release/pancetta}"
LIMIT=300
[ -x "$BIN" ] || { echo "ERROR: $BIN not found — cargo build --release first."; exit 2; }

CFG="$HOME/.pancetta/pancetta.toml"
if [ -f "$CFG" ]; then
  printf "Existing config found. Move it aside for a clean drill? [y/N] "
  read -r ans
  if [ "${ans:-n}" = "y" ]; then
    mv "$CFG" "$CFG.drill-backup.$(date +%s)"
    echo "Moved aside (restore from $CFG.drill-backup.*)."
  else
    echo "Running against the existing config (wizard step will be skipped)."
  fi
fi

START=$(date +%s)
elapsed() { echo $(( $(date +%s) - START )); }
mark() { printf "\n== [T+%03ds] %s ==\n" "$(elapsed)" "$1"; }

mark "STEP 1/4: first-run wizard (station -> audio -> RIG: answer y!)"
echo "Complete the wizard, then QUIT pancetta (q, y). Starting it now..."
"$BIN"

mark "STEP 2/4: pancetta doctor"
if "$BIN" doctor; then
  echo "doctor: GREEN"
else
  mark "RESULT: FAIL — doctor is red. Apply the printed fixes and re-run."
  exit 1
fi

mark "STEP 3/4: decode check"
echo "Starting pancetta. Rig on an FT8 frequency (e.g. 14.074 USB)."
echo "As soon as you SEE A DECODE in Band Activity, quit (q, y)."
"$BIN"

mark "STEP 4/4: operator attestation"
printf "Did you see at least one decode? [y/N] "; read -r saw
printf "Is the RIG badge connected (CAT working, TX-capable)? [y/N] "; read -r rig

TOTAL=$(elapsed)
echo
echo "-------------------------------------------"
echo " Total time: ${TOTAL}s (limit ${LIMIT}s)"
if [ "${saw:-n}" = "y" ] && [ "${rig:-n}" = "y" ] && [ "$TOTAL" -le "$LIMIT" ]; then
  echo " RESULT: PASS — on the air in under 5 minutes."
  exit 0
fi
echo " RESULT: FAIL"
[ "${saw:-n}" != "y" ] && echo "   - no decode observed (doctor + Shift+D to diagnose)"
[ "${rig:-n}" != "y" ] && echo "   - rig not connected/TX-capable (pancetta test-rig)"
[ "$TOTAL" -gt "$LIMIT" ] && echo "   - over the 300 s budget: find the slow step above (T+ marks)"
exit 1
```

- [ ] **Step 2: Verify script mechanics without hardware**

```bash
chmod +x scripts/five-minute-drill.sh
bash -n scripts/five-minute-drill.sh && echo "syntax ok"
PANCETTA_BIN=/nonexistent scripts/five-minute-drill.sh; echo "exit=$?"   # expect the ERROR line, exit=2
```

- [ ] **Step 3: Commit**

```bash
git add scripts/five-minute-drill.sh
git commit -m "feat(scripts): add five-minute-drill.sh — stopwatch acceptance test for the post-build on-air path"
```

- [ ] **Step 4: Hand the drill to the operator (meatspace)**

The real acceptance run needs the FTdx10 attached — file it on the operator's at-rig list: run `scripts/five-minute-drill.sh` on the station machine after this branch builds there, record the T+ marks, and treat any step over budget as a Phase-4 bug.

---

## Final gate (after all tasks)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test --workspace --features transmit 2>&1 | tail -5
cargo test -p pancetta --bin pancetta 2>&1 | tail -5   # doctor + wizard unit tests
```

Expected: all green. Update `docs/DECISIONS/config-and-platform.md` (doctor + wizard rig step) and `docs/DECISIONS/tui.md` (input-fallback badge, rig error surfacing) with dated entries per the repo documentation policy. Then push the branch and open a PR titled "Onboarding Phase 4: the five-minute on-air path" referencing `docs/superpowers/specs/2026-07-12-onboarding-world-class-design.md` — noting that this phase implements the operator's clarified goal: **5 minutes measured from build-complete to on-air**, not from download to decode.
