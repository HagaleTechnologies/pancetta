# Arbitrary-frequency tune + split RX/TX — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator tune the rig to an arbitrary dial frequency and run true rig-level dual-VFO split (RX dial ≠ TX dial), with a TUI modal entry and an interim once-per-session US-out-of-band warning.

**Architecture:** One new shared atomic `split_tx_frequency_hz` (0 = simplex) lives beside the existing `operating_frequency_hz` in the coordinator. The hamlib layer gains `set_split` / `set_split_freq` (rigctld `S` / `I` short-form commands) behind a new `RigControlMessage::SetSplit` bus message. The QSO RF stamp logs the *effective TX dial* (`split != 0 ? split : rx_dial`). A new TUI modal (Shift+`F`) collects RX dial + optional split TX in MHz, forwards `SetFrequency` + `SetSplit`, and shows a required acknowledgment modal the first time a session's TX RF lands outside the ham bands.

**Tech Stack:** Rust workspace (pancetta-hamlib, pancetta/coordinator, pancetta-qso, pancetta-tui, pancetta-core), `async_trait`, ratatui/crossterm TUI, tokio.

**Spec:** `docs/superpowers/specs/2026-06-25-arbitrary-freq-split-design.md`

**Build order context:** Feature 1 of the next-session agenda; on branch `feat/arbitrary-freq-split`.

**Keymap deviation from spec:** spec proposed `f` to open + Shift+`F` to clear split; `f` is already bound (TX-freq-mode toggle, `tui_runner.rs:1045`). This plan uses Shift+`F` to OPEN the modal and clears split by leaving the TX-split field blank. No separate clear key.

**Process discipline (from project memory):**
- The pre-push hook runs `cargo fmt --check` + clippy + `cargo test --workspace` on the WORKING TREE. Never leave uncommitted broken changes while a push runs. Commit per task.
- `cargo fmt` every touched file before committing (the hook's fmt-check rejects otherwise).
- Do NOT push from task subagents. The controller pushes at the end, foreground-verified via `git ls-remote` (no piping through `tail`).
- Run `cargo test --features transmit -p <crate>` for the crate you touched after each implementation step.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `pancetta-hamlib/src/rig.rs` | `RigControl` trait | Add `set_split` + `set_split_freq` with default bodies |
| `pancetta-hamlib/src/rigctld.rs` | rigctld client | Implement both via `S` / `I` short-form commands |
| `pancetta-hamlib/src/mock.rs` | `MockRig` | Split state (`split_enabled`, `split_tx_freq`) + impls + getters |
| `pancetta/src/message_bus.rs` | bus enums | `RigControlMessage::SetSplit` |
| `pancetta/src/coordinator/hamlib.rs` | rig command loop | Handle `SetSplit` |
| `pancetta/src/coordinator/mod.rs` | coordinator state | `split_tx_frequency_hz` atomic + wiring |
| `pancetta/src/coordinator/qso.rs` | QSO component startup | Inject split source into `QsoManager` |
| `pancetta/src/coordinator/tui_relay.rs` | TUI→bus relay | Handle `SetSplit`; clear split on band change |
| `pancetta/src/coordinator/autonomous.rs` | autonomous band-hop | Clear split on `ChangeBand` |
| `pancetta-qso/src/qso_manager.rs` | RF stamp | `effective_tx_dial` + split source + use at both stamp sites |
| `pancetta-tui/src/tui_runner.rs` | TUI keys + commands | `TuiCommand::SetSplit`; modal intercept; Shift+`F` |
| `pancetta-tui/src/app.rs` | TUI state | Freq-modal state + parse/validation helpers + banner |
| `pancetta-tui/src/ui/mod.rs` | rendering | Modal overlay + out-of-band ack overlay + split chip |
| `pancetta/tests/coord_sim.rs` | coord-level test | Split-active QSO logs TX dial; mock keys with split |

---

## Phase 1 — Hamlib split primitive

### Task 1: Add split methods to the `RigControl` trait (default bodies)

**Files:**
- Modify: `pancetta-hamlib/src/rig.rs:200-202` (end of trait, after `get_info`)

Adding methods with **default bodies** means existing `RigControl` impls (real FFI, tests) compile unchanged; only `RigctldClient` and `MockRig` override them.

- [ ] **Step 1: Add the two trait methods with defaults**

In `rig.rs`, immediately before the closing `}` of the `RigControl` trait (after `async fn get_info(&self) -> Result<String>;`, line 201):

```rust
    /// Enable or disable split operation, selecting which VFO transmits.
    /// Default impl is a no-op for rigs/mocks that do not model split.
    async fn set_split(&self, _enabled: bool, _tx_vfo: Vfo) -> Result<()> {
        Ok(())
    }

    /// Set the transmit-VFO frequency (Hz) used while split is enabled.
    /// Default impl is a no-op for rigs/mocks that do not model split.
    async fn set_split_freq(&self, _tx_freq: u64) -> Result<()> {
        Ok(())
    }
```

- [ ] **Step 2: Build the crate to confirm defaults compile everywhere**

Run: `cargo build -p pancetta-hamlib`
Expected: builds clean (no impl is forced to change yet).

- [ ] **Step 3: Commit**

```bash
cargo fmt -p pancetta-hamlib
git add pancetta-hamlib/src/rig.rs
git commit -m "feat(hamlib): add set_split/set_split_freq to RigControl trait (default no-op)"
```

### Task 2: Implement split on `RigctldClient` (short-form `S` / `I`)

**Files:**
- Modify: `pancetta-hamlib/src/rigctld.rs` (in `impl RigControl for RigctldClient`, near `set_frequency` at `:486`)
- Test: same file, `#[cfg(test)]` module (add if none; otherwise append)

rigctld short-form commands: `S <split:0|1> <tx_vfo>` sets split mode + TX VFO; `I <freq>` sets the split TX frequency. We assert the emitted command strings; the live behavior is `OPERATOR-CONFIRM(split)` (verified on the FTdx10).

- [ ] **Step 1: Write the failing test for command-string formatting**

First check whether `rigctld.rs` already has a `#[cfg(test)]` module and a way to capture sent commands. If there is an existing pattern that tests `set_frequency` command strings, mirror it. If commands are only sent over TCP (no capture seam), add a pure formatting helper and test THAT instead:

Add near `vfo_to_string` (`rigctld.rs:378`):

```rust
    /// Format the rigctld short-form command to set split mode + TX VFO.
    fn split_command(enabled: bool, tx_vfo: Vfo) -> String {
        format!("S {} {}", if enabled { 1 } else { 0 }, Self::vfo_to_string(tx_vfo))
    }

    /// Format the rigctld short-form command to set the split TX frequency.
    fn split_freq_command(tx_freq: u64) -> String {
        format!("I {}", tx_freq)
    }
```

Add a test (new or appended `#[cfg(test)] mod split_tests`):

```rust
#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::models::Vfo;

    #[test]
    fn split_command_strings() {
        assert_eq!(RigctldClient::split_command(true, Vfo::B), "S 1 VFOB");
        assert_eq!(RigctldClient::split_command(false, Vfo::A), "S 0 VFOA");
        assert_eq!(RigctldClient::split_freq_command(14_090_000), "I 14090000");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pancetta-hamlib split_command_strings`
Expected: FAIL — `split_command` / `split_freq_command` not found (or, if you wrote the test before the helpers, a compile error). Add the helpers (Step 1 code) and it compiles to FAIL→PASS only once the impl methods exist.

- [ ] **Step 3: Implement the trait methods on `RigctldClient`**

In `impl RigControl for RigctldClient`, after `set_frequency` (ends `:506`):

```rust
    #[instrument(skip(self))]
    async fn set_split(&self, enabled: bool, tx_vfo: Vfo) -> Result<()> {
        // OPERATOR-CONFIRM(split): exact short-form verified on-air (FTdx10).
        self.send_command_with_retry(&Self::split_command(enabled, tx_vfo))
            .await?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn set_split_freq(&self, tx_freq: u64) -> Result<()> {
        // OPERATOR-CONFIRM(split): exact short-form verified on-air (FTdx10).
        self.send_command_with_retry(&Self::split_freq_command(tx_freq))
            .await?;
        Ok(())
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p pancetta-hamlib split_command_strings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p pancetta-hamlib
git add pancetta-hamlib/src/rigctld.rs
git commit -m "feat(hamlib): rigctld set_split/set_split_freq (S/I short-form) + cmd-string tests"
```

### Task 3: Implement split state on `MockRig`

**Files:**
- Modify: `pancetta-hamlib/src/mock.rs` — `MockRigState` (`:57-82`), `Default` (`:84-115`), `impl RigControl for MockRig` (`:344`), inherent getters (near `:312`)
- Test: `pancetta-hamlib/src/mock.rs` `#[cfg(test)] mod tests` (`:851`)

- [ ] **Step 1: Add split fields to `MockRigState`**

In `MockRigState` (after `scanning: bool,` at `:79`):

```rust
    /// Split mode enabled.
    split_enabled: bool,
    /// Split TX-VFO frequency in Hz (meaningful only when `split_enabled`).
    split_tx_freq: u64,
```

In `MockRigState::default()` (in the `Self { ... }` literal, after `scanning: false,` at `:111`):

```rust
            split_enabled: false,
            split_tx_freq: 0,
```

- [ ] **Step 2: Write the failing test**

Append to `mod tests` (`mock.rs:851`):

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn test_mock_rig_split() {
        let rig = MockRig::default();
        rig.connect().await.unwrap();

        // Default: split off.
        assert!(!rig.split_enabled());

        // Enable split with a TX freq.
        rig.set_split_freq(14_090_000).await.unwrap();
        rig.set_split(true, Vfo::B).await.unwrap();
        assert!(rig.split_enabled());
        assert_eq!(rig.split_tx_freq(), 14_090_000);

        // Disable split.
        rig.set_split(false, Vfo::A).await.unwrap();
        assert!(!rig.split_enabled());
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p pancetta-hamlib test_mock_rig_split`
Expected: FAIL — `split_enabled` / `split_tx_freq` methods not found.

- [ ] **Step 4: Implement the inherent getters and trait methods**

Add inherent getters near the other accessors (after `get_operation_count`, `:315`):

```rust
    /// Whether split is currently enabled (test accessor).
    pub fn split_enabled(&self) -> bool {
        self.state.read().split_enabled
    }

    /// Current split TX-VFO frequency in Hz (test accessor).
    pub fn split_tx_freq(&self) -> u64 {
        self.state.read().split_tx_freq
    }
```

Add the trait methods inside `impl RigControl for MockRig` (after `set_frequency`, `:437`):

```rust
    #[instrument(skip(self))]
    async fn set_split(&self, enabled: bool, _tx_vfo: Vfo) -> Result<()> {
        self.simulate_delay().await;
        {
            let mut state = self.state.write();
            state.split_enabled = enabled;
        }
        self.update_operation_time();
        debug!("Mock rig set split: {}", enabled);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn set_split_freq(&self, tx_freq: u64) -> Result<()> {
        self.simulate_delay().await;
        {
            let mut state = self.state.write();
            state.split_tx_freq = tx_freq;
        }
        self.update_operation_time();
        debug!("Mock rig set split TX freq: {} Hz", tx_freq);
        Ok(())
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p pancetta-hamlib test_mock_rig_split`
Expected: PASS.

- [ ] **Step 6: Run the whole hamlib suite (single-threaded per CLAUDE.md)**

Run: `cargo test -p pancetta-hamlib --lib -- --test-threads=1`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
cargo fmt -p pancetta-hamlib
git add pancetta-hamlib/src/mock.rs
git commit -m "feat(hamlib): MockRig split state + getters + test"
```

---

## Phase 2 — Bus message + coordinator handler

### Task 4: Add `RigControlMessage::SetSplit`

**Files:**
- Modify: `pancetta/src/message_bus.rs:344-370` (the `RigControlMessage` enum)

- [ ] **Step 1: Add the variant**

In `RigControlMessage` (after `SwrResponse { swr: f32 },` at `:369`, before the closing `}`):

```rust
    /// Enable/disable rig-level split (RX dial ≠ TX dial). When `enabled`,
    /// the rig transmits on VFO B at `tx_frequency` (Hz) while receiving on
    /// VFO A. When disabled, `tx_frequency` is ignored. Produced by the TUI
    /// SetSplit relay; consumed by the hamlib command loop.
    SetSplit { enabled: bool, tx_frequency: u64 },
```

- [ ] **Step 2: Build**

Run: `cargo build -p pancetta`
Expected: builds (the `_ => {}` arm in `hamlib.rs` already absorbs the new variant; no exhaustiveness break).

- [ ] **Step 3: Commit**

```bash
cargo fmt -p pancetta
git add pancetta/src/message_bus.rs
git commit -m "feat(bus): RigControlMessage::SetSplit"
```

### Task 5: Handle `SetSplit` in the hamlib command loop

**Files:**
- Modify: `pancetta/src/coordinator/hamlib.rs:627` (replace the `_ => {}` arm with a `SetSplit` arm + `_ => {}`)

- [ ] **Step 1: Add the handler arm**

Replace the `_ => {}` at `hamlib.rs:627` with:

```rust
                                    crate::message_bus::RigControlMessage::SetSplit {
                                        enabled,
                                        tx_frequency,
                                    } => {
                                        if *enabled {
                                            if let Err(e) =
                                                rig_poll.set_split_freq(*tx_frequency).await
                                            {
                                                warn!(target: "rig.split", "set_split_freq failed: {}", e);
                                            }
                                            if let Err(e) = rig_poll
                                                .set_split(true, pancetta_hamlib::Vfo::B)
                                                .await
                                            {
                                                warn!(target: "rig.split", "set_split(on) failed: {}", e);
                                            } else {
                                                info!(target: "rig.split", "split ON, TX {} Hz", tx_frequency);
                                            }
                                        } else if let Err(e) = rig_poll
                                            .set_split(false, pancetta_hamlib::Vfo::A)
                                            .await
                                        {
                                            warn!(target: "rig.split", "set_split(off) failed: {}", e);
                                        } else {
                                            info!(target: "rig.split", "split OFF");
                                        }
                                    }
                                    _ => {}
```

Confirm `warn` is imported in `hamlib.rs` (it uses `error!`/`info!`/`debug!` already; add `warn` to the `use tracing::{...}` line if missing).

- [ ] **Step 2: Build**

Run: `cargo build -p pancetta`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
cargo fmt -p pancetta
git add pancetta/src/coordinator/hamlib.rs
git commit -m "feat(coordinator): handle RigControlMessage::SetSplit in hamlib loop"
```

---

## Phase 3 — Split state atomic + QSO RF-stamp correctness

### Task 6: `effective_tx_dial` + split source on `QsoManager`

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` — add field near `dial_frequency_hz` (`:323`), constructor default, `effective_tx_dial` free fn, setter near `set_dial_frequency_source` (`:457`), and use at both stamp sites (`:1205-1207`, `:1555-1557`)
- Test: `qso_manager.rs` test module

- [ ] **Step 1: Write the failing unit test for `effective_tx_dial`**

Add to the `qso_manager.rs` `#[cfg(test)]` module (find it with `rg -n "mod tests" pancetta-qso/src/qso_manager.rs`):

```rust
    #[test]
    fn effective_tx_dial_simplex_and_split() {
        // Simplex: split == 0 → use RX dial.
        assert_eq!(super::effective_tx_dial(14_074_000, 0), 14_074_000);
        // Split: nonzero split → use split TX dial.
        assert_eq!(super::effective_tx_dial(14_074_000, 14_090_000), 14_090_000);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pancetta-qso effective_tx_dial_simplex_and_split`
Expected: FAIL — `effective_tx_dial` not found.

- [ ] **Step 3: Add the free function**

Near the top of `qso_manager.rs` (after the imports / before `impl QsoManager`), add:

```rust
/// The dial the station actually transmits on: the split TX dial when split is
/// active (`split_tx_hz != 0`), otherwise the RX dial. Used to stamp the logged
/// RF frequency of a completed QSO (dial + audio offset).
pub fn effective_tx_dial(rx_dial_hz: u64, split_tx_hz: u64) -> u64 {
    if split_tx_hz != 0 {
        split_tx_hz
    } else {
        rx_dial_hz
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p pancetta-qso effective_tx_dial_simplex_and_split`
Expected: PASS.

- [ ] **Step 5: Add the split-source field, default, and setter**

Add the field after `dial_frequency_hz: Arc<AtomicU64>,` (`:323`):

```rust
    /// Rig split-TX dial in Hz (0 = simplex), shared from the coordinator.
    /// When nonzero, completed-QSO RF is stamped against this TX dial instead
    /// of `dial_frequency_hz` (the RX dial). Defaults to a private `0` atomic
    /// so callers that never inject a source keep simplex (RX==TX) behavior.
    split_tx_frequency_hz: Arc<AtomicU64>,
```

In the constructor(s) where `dial_frequency_hz` is initialized (the `Self { ... }` literal that sets `dial_frequency_hz: Arc::new(AtomicU64::new(0))` — find via `rg -n "dial_frequency_hz:" pancetta-qso/src/qso_manager.rs`), add alongside it:

```rust
            split_tx_frequency_hz: Arc::new(AtomicU64::new(0)),
```

Add the setter after `set_dial_frequency_source` (`:459`):

```rust
    /// Share the coordinator's split-TX dial source so completed QSOs log the
    /// real TX RF during split operation. Pass the same `Arc<AtomicU64>` the
    /// TUI SetSplit relay updates (0 = simplex). If never called, the manager
    /// keeps its private `0` (RX==TX).
    pub fn set_split_tx_frequency_source(&mut self, source: Arc<AtomicU64>) {
        self.split_tx_frequency_hz = source;
    }
```

- [ ] **Step 6: Use the effective dial at both stamp sites**

At `:1205-1207` (the `is_completed_open` branch), replace:

```rust
            let dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            if dial > 0 {
                metadata.frequency += dial as f64;
            }
```

with:

```rust
            let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
            let dial = effective_tx_dial(rx_dial, split);
            if dial > 0 {
                metadata.frequency += dial as f64;
            }
```

At `:1555-1557` (the `Completed` match arm), replace:

```rust
                let dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                if dial > 0 {
                    m.frequency += dial as f64;
                }
```

with:

```rust
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                if dial > 0 {
                    m.frequency += dial as f64;
                }
```

- [ ] **Step 7: Write a stamp test (split overrides RX dial)**

This needs a `QsoManager` driven to completion with both sources set. Find an existing completion test in `qso_manager.rs` (`rg -n "QsoCompleted" pancetta-qso/src/qso_manager.rs` / the sim) and mirror it, OR add a focused test that sets both atomics and asserts `effective_tx_dial` is what gets added. Minimal focused test if a full-completion harness is heavy:

```rust
    #[test]
    fn split_source_overrides_rx_dial_for_stamp() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let rx = Arc::new(AtomicU64::new(14_074_000));
        let split = Arc::new(AtomicU64::new(14_090_000));
        // Mirrors the stamp computation in process_message_for_qso.
        let dial = super::effective_tx_dial(
            rx.load(Ordering::Relaxed),
            split.load(Ordering::Relaxed),
        );
        assert_eq!(dial, 14_090_000);
        split.store(0, Ordering::Relaxed);
        let dial = super::effective_tx_dial(
            rx.load(Ordering::Relaxed),
            split.load(Ordering::Relaxed),
        );
        assert_eq!(dial, 14_074_000);
    }
```

(The authoritative end-to-end assertion lives in coord_sim, Task 11.)

- [ ] **Step 8: Run the qso suite**

Run: `cargo test --features transmit -p pancetta-qso`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
cargo fmt -p pancetta-qso
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): stamp completed-QSO RF against effective TX dial (split-aware)"
```

### Task 7: Add the `split_tx_frequency_hz` atomic to the coordinator and wire the QSO source

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs` — field near `operating_frequency_hz` (`:410`), construction (`:731`), and a `pub` accessor; pass clone into the QSO component
- Modify: `pancetta/src/coordinator/qso.rs:783` — call `set_split_tx_frequency_source`

- [ ] **Step 1: Add the field and construct it**

In the coordinator struct (next to `operating_frequency_hz: Arc<AtomicU64>` at `:410`):

```rust
    /// Rig split-TX dial in Hz (0 = simplex). Written by the TUI SetSplit relay,
    /// read by the QSO RF stamp. RX dial stays `operating_frequency_hz`.
    split_tx_frequency_hz: Arc<AtomicU64>,
```

Where `operating_frequency_hz` is constructed (`:731`, `Arc::new(AtomicU64::new(0))`), add the sibling init in the same `Self { ... }` / builder:

```rust
            split_tx_frequency_hz: Arc::new(AtomicU64::new(0)),
```

Add a `pub(crate)` accessor near the other atomic accessors (so `tui_relay`/tests reach it):

```rust
    /// Shared split-TX dial atomic (0 = simplex).
    pub(crate) fn split_tx_frequency_hz(&self) -> Arc<AtomicU64> {
        self.split_tx_frequency_hz.clone()
    }
```

- [ ] **Step 2: Inject the source into the QSO manager**

In `coordinator/qso.rs` near `:783` where `set_dial_frequency_source(operating_frequency_hz.clone())` is called, you need the split atomic in scope. Thread `split_tx_frequency_hz.clone()` into the QSO component startup the same way `operating_frequency_hz` is passed (follow the existing `with_operating_frequency` / clone-into-task pattern from `mod.rs:792`). Then add, right after the dial source line:

```rust
        qso_manager.set_split_tx_frequency_source(split_tx_frequency_hz.clone());
```

(If `operating_frequency_hz` is moved into the task via a `let cmd_x = x.clone();` capture, add a matching `let` capture for `split_tx_frequency_hz` at the same site.)

- [ ] **Step 3: Build**

Run: `cargo build -p pancetta`
Expected: builds clean.

- [ ] **Step 4: Commit**

```bash
cargo fmt -p pancetta
git add pancetta/src/coordinator/mod.rs pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): split_tx_frequency_hz atomic + inject into QsoManager"
```

---

## Phase 4 — TUI: modal entry, relay, out-of-band warning, banner

### Task 8: Pure TUI helpers — MHz parse + US-band check (TDD first)

**Files:**
- Modify: `pancetta-tui/src/app.rs` — add two free functions + tests
- Test: `pancetta-tui/src/app.rs` test module (`:2650` area)

- [ ] **Step 1: Write the failing tests**

Add to the `app.rs` `#[cfg(test)]` module:

```rust
    #[test]
    fn parse_mhz_to_hz_accepts_and_rejects() {
        assert_eq!(super::parse_mhz_to_hz("14.085"), Some(14_085_000));
        assert_eq!(super::parse_mhz_to_hz("7.074"), Some(7_074_000));
        assert_eq!(super::parse_mhz_to_hz("14"), Some(14_000_000));
        assert_eq!(super::parse_mhz_to_hz(""), None);
        assert_eq!(super::parse_mhz_to_hz("abc"), None);
        assert_eq!(super::parse_mhz_to_hz("14.0.0"), None);
    }

    #[test]
    fn tx_rf_out_of_us_band_flags_only_out_of_band() {
        // In 20m.
        assert!(!super::tx_rf_out_of_us_band(14_074_000 + 1_500));
        // 28.5 MHz is inside 10m.
        assert!(!super::tx_rf_out_of_us_band(28_500_000));
        // 15.000 MHz is between 20m and 17m → out of band.
        assert!(super::tx_rf_out_of_us_band(15_000_000));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-tui parse_mhz_to_hz_accepts_and_rejects tx_rf_out_of_us_band_flags_only_out_of_band`
Expected: FAIL — functions not found.

- [ ] **Step 3: Implement the helpers**

Add to `app.rs` (free functions, near the top or bottom of the module, not inside `impl App`):

```rust
/// Parse an operator-entered frequency in MHz (e.g. "14.085") to Hz. Returns
/// `None` for empty or malformed input. Rounds to the nearest Hz.
pub fn parse_mhz_to_hz(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let mhz: f64 = t.parse().ok()?;
    if !mhz.is_finite() || mhz <= 0.0 {
        return None;
    }
    Some((mhz * 1_000_000.0).round() as u64)
}

/// Interim US-band check: true when `tx_rf_hz` is outside the ham band ranges
/// modeled by `pancetta_core::Band::from_frequency` (used here as the proxy for
/// US bands). Region-aware band plans are a deferred TODO (see the design spec).
pub fn tx_rf_out_of_us_band(tx_rf_hz: u64) -> bool {
    pancetta_core::Band::from_frequency(tx_rf_hz).is_none()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-tui parse_mhz_to_hz_accepts_and_rejects tx_rf_out_of_us_band_flags_only_out_of_band`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p pancetta-tui
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): pure MHz-parse + interim US-out-of-band helpers"
```

### Task 9: Freq-modal + split state on `App` + `TuiCommand::SetSplit`

**Files:**
- Modify: `pancetta-tui/src/app.rs` — new modal-state struct + `App` fields + default init + small methods
- Modify: `pancetta-tui/src/tui_runner.rs` — `TuiCommand::SetSplit` variant

- [ ] **Step 1: Add the `TuiCommand::SetSplit` variant**

In `tui_runner.rs` `TuiCommand` enum (after `SetFrequency` at `:195`):

```rust
    /// Enable/disable rig split (RX dial ≠ TX dial). `tx_frequency` is the
    /// split TX dial in Hz (ignored when `enabled == false`).
    SetSplit { enabled: bool, tx_frequency: u64 },
```

- [ ] **Step 2: Add the modal state struct and `App` fields**

In `app.rs`, add a small state struct (near `DeviceSelectionState` usage; define it in `app.rs`):

```rust
/// Which field of the frequency-entry modal is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreqModalField {
    RxDial,
    TxSplit,
}

/// State for the Shift+F frequency-entry modal.
#[derive(Debug, Clone, Default)]
pub struct FreqModalState {
    /// Modal visible.
    pub visible: bool,
    /// RX dial input buffer (MHz string).
    pub rx_buffer: String,
    /// TX split input buffer (MHz string); empty = simplex.
    pub tx_buffer: String,
    /// Focused field.
    pub field: FreqModalFieldOpt,
}
```

Because `Default` can't derive a non-`Default` enum cleanly, model `field` as a plain enum with a `Default`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreqModalFieldOpt {
    #[default]
    RxDial,
    TxSplit,
}
```

(Drop the first `FreqModalField` enum — use only `FreqModalFieldOpt`. It is named with the `Opt` suffix only to carry `#[derive(Default)]`; rename to `FreqModalField` if you prefer and add a manual `Default` impl. Pick ONE name and use it consistently in Tasks 9–10.)

Add `App` fields (near `quit_confirm_visible` at `:620`):

```rust
    /// Frequency-entry modal (Shift+F). See `FreqModalState`.
    pub freq_modal: FreqModalState,
    /// Active split TX dial in Hz for display (0 = simplex). Set optimistically
    /// when the operator applies split; authoritative atomic lives in the
    /// coordinator.
    pub split_tx_hz: u64,
    /// True once this session has shown the out-of-band acknowledgment modal,
    /// so it is shown at most once per session.
    pub out_of_band_warned: bool,
    /// True while the required out-of-band acknowledgment modal is visible.
    pub out_of_band_ack_visible: bool,
    /// The TX RF (Hz) that triggered the out-of-band modal (for the message).
    pub out_of_band_rf_hz: u64,
```

In `App`'s constructor (the `Self { ... }` with `quit_confirm_visible: false,` at `:764`), add:

```rust
            freq_modal: FreqModalState::default(),
            split_tx_hz: 0,
            out_of_band_warned: false,
            out_of_band_ack_visible: false,
            out_of_band_rf_hz: 0,
```

- [ ] **Step 3: Build**

Run: `cargo build -p pancetta-tui`
Expected: builds clean (fields unused warnings are fine until Task 10).

- [ ] **Step 4: Commit**

```bash
cargo fmt -p pancetta-tui
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): freq-modal + split display state; TuiCommand::SetSplit"
```

### Task 10: Modal key handling + apply logic

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` — modal intercept block (mirror `:616-631`), Shift+`F` open, and an out-of-band-ack intercept

- [ ] **Step 1: Add the out-of-band-ack intercept (highest priority)**

In `handle_key_event`, immediately after `let mut app = self.app.write().await;` (`:614`) and BEFORE the quit-confirm block (`:616`), add:

```rust
        // Required out-of-band acknowledgment modal — must be dismissed first.
        if app.out_of_band_ack_visible {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    app.out_of_band_ack_visible = false;
                    app.status_message = "Out-of-band TX acknowledged".to_string();
                }
                _ => {}
            }
            return Ok(true);
        }
```

- [ ] **Step 2: Add the freq-modal intercept block**

After the device-selection modal block (`:679`, the `return Ok(true);` that closes it), add:

```rust
        // Frequency-entry modal (Shift+F): two MHz text fields.
        if app.freq_modal.visible {
            match key.code {
                KeyCode::Esc => {
                    app.freq_modal.visible = false;
                    app.status_message = "Frequency entry cancelled".to_string();
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    app.freq_modal.field = match app.freq_modal.field {
                        crate::app::FreqModalFieldOpt::RxDial => {
                            crate::app::FreqModalFieldOpt::TxSplit
                        }
                        crate::app::FreqModalFieldOpt::TxSplit => {
                            crate::app::FreqModalFieldOpt::RxDial
                        }
                    };
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                    match app.freq_modal.field {
                        crate::app::FreqModalFieldOpt::RxDial => app.freq_modal.rx_buffer.push(c),
                        crate::app::FreqModalFieldOpt::TxSplit => app.freq_modal.tx_buffer.push(c),
                    }
                }
                KeyCode::Backspace => match app.freq_modal.field {
                    crate::app::FreqModalFieldOpt::RxDial => {
                        app.freq_modal.rx_buffer.pop();
                    }
                    crate::app::FreqModalFieldOpt::TxSplit => {
                        app.freq_modal.tx_buffer.pop();
                    }
                },
                KeyCode::Enter => {
                    // Parse RX dial; required.
                    let rx_hz = crate::app::parse_mhz_to_hz(&app.freq_modal.rx_buffer);
                    let tx_hz = crate::app::parse_mhz_to_hz(&app.freq_modal.tx_buffer); // None = simplex
                    match rx_hz {
                        None => {
                            app.status_message = "Invalid RX dial — enter MHz e.g. 14.085".to_string();
                        }
                        Some(rx) => {
                            app.freq_modal.visible = false;

                            // Apply RX dial (existing path).
                            self.message_tx.send(TuiCommand::SetFrequency { vfo: 0, frequency: rx })?;

                            // Apply split.
                            let (split_enabled, split_freq) = match tx_hz {
                                Some(tx) => (true, tx),
                                None => (false, 0),
                            };
                            app.split_tx_hz = split_freq;
                            self.message_tx.send(TuiCommand::SetSplit {
                                enabled: split_enabled,
                                tx_frequency: split_freq,
                            })?;

                            // Out-of-band check on the effective TX RF + current offset.
                            let tx_dial = if split_enabled { split_freq } else { rx };
                            let tx_rf = tx_dial + app.tx_frequency_offset as u64;
                            if crate::app::tx_rf_out_of_us_band(tx_rf) && !app.out_of_band_warned {
                                app.out_of_band_warned = true;
                                app.out_of_band_ack_visible = true;
                                app.out_of_band_rf_hz = tx_rf;
                            }

                            app.status_message = if split_enabled {
                                format!("Dial {:.3} MHz, SPLIT TX {:.3} MHz", rx as f64 / 1e6, split_freq as f64 / 1e6)
                            } else {
                                format!("Dial {:.3} MHz (simplex)", rx as f64 / 1e6)
                            };
                            // Reset buffers for next open.
                            app.freq_modal.rx_buffer.clear();
                            app.freq_modal.tx_buffer.clear();
                            app.freq_modal.field = crate::app::FreqModalFieldOpt::RxDial;
                        }
                    }
                }
                _ => {}
            }
            return Ok(true);
        }
```

- [ ] **Step 3: Add the Shift+`F` open binding**

In the normal key match (alongside `KeyCode::Char('f')` at `:1045`), add:

```rust
            // Shift+F — open the arbitrary-frequency / split entry modal.
            KeyCode::Char('F') => {
                app.freq_modal.visible = true;
                app.freq_modal.field = crate::app::FreqModalFieldOpt::RxDial;
                app.freq_modal.rx_buffer.clear();
                app.freq_modal.tx_buffer.clear();
                app.status_message = "Freq entry: dial MHz, Tab→split, Enter, Esc".to_string();
            }
```

- [ ] **Step 4: Build + run TUI tests**

Run: `cargo build -p pancetta-tui && cargo test -p pancetta-tui`
Expected: builds; existing tests pass.

- [ ] **Step 5: Add a modal-state test**

Add to `app.rs` tests (drive `FreqModalState` transitions purely, no terminal):

```rust
    #[test]
    fn freq_modal_buffers_collect_and_reset() {
        let mut m = super::FreqModalState { visible: true, ..Default::default() };
        m.rx_buffer.push('1');
        m.rx_buffer.push('4');
        assert_eq!(super::parse_mhz_to_hz(&m.rx_buffer), Some(14_000_000));
        m.tx_buffer.clear();
        assert_eq!(super::parse_mhz_to_hz(&m.tx_buffer), None); // simplex
    }
```

Run: `cargo test -p pancetta-tui freq_modal_buffers_collect_and_reset`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt -p pancetta-tui
git add pancetta-tui/src/tui_runner.rs pancetta-tui/src/app.rs
git commit -m "feat(tui): Shift+F freq/split modal + required out-of-band ack modal"
```

### Task 11: Render the modal, the ack overlay, and the split chip

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs` — add render fns; call them in the overlay dispatch (mirror `render_device_selection_modal` at `tui_runner.rs:1198` / overlay calls at `:1171-1198`)
- Modify: `pancetta-tui/src/ui/mod.rs` (or wherever the title-bar banners render, near the TX-policy banner `:271`) — split chip

- [ ] **Step 1: Add the modal + ack render functions**

Mirror the existing `render_device_selection_modal` pattern (centered `Rect`, `Clear`, bordered `Block`). In `ui/mod.rs` add:

```rust
/// Render the Shift+F frequency-entry modal.
pub fn render_freq_modal(f: &mut Frame, area: Rect, m: &crate::app::FreqModalState) {
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use ratatui::style::{Style, Color};
    let popup = centered_rect(40, 9, area); // reuse the helper used by other modals
    f.render_widget(Clear, popup);
    let rx_focus = matches!(m.field, crate::app::FreqModalFieldOpt::RxDial);
    let body = format!(
        " RX dial (MHz): {}{}\n TX split (MHz): {}{}\n   (blank = simplex)\n [Enter] apply   [Tab] field   [Esc] x",
        m.rx_buffer, if rx_focus { "_" } else { "" },
        m.tx_buffer, if !rx_focus { "_" } else { "" },
    );
    let p = Paragraph::new(body).block(
        Block::default().borders(Borders::ALL).title(" Set Frequency ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(p, popup);
}

/// Render the required out-of-band acknowledgment modal.
pub fn render_out_of_band_modal(f: &mut Frame, area: Rect, tx_rf_hz: u64) {
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use ratatui::style::{Style, Color};
    let popup = centered_rect(50, 7, area);
    f.render_widget(Clear, popup);
    let body = format!(
        " TX {:.3} MHz is OUTSIDE the US ham bands.\n You are responsible for legal operation.\n\n [Enter] acknowledge",
        tx_rf_hz as f64 / 1e6,
    );
    let p = Paragraph::new(body).block(
        Block::default().borders(Borders::ALL).title(" ⚠ Out of band ")
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(p, popup);
}
```

If `centered_rect` does not exist, copy the centering math from `render_device_selection_modal` (read it at `tui_runner.rs:1198`) — do NOT invent a new helper name without checking.

- [ ] **Step 2: Call them in the overlay dispatch**

In the render path where the other overlays are dispatched (`tui_runner.rs:1171-1198`, after device-selection / before/after quit-confirm), add — ack modal has top priority:

```rust
            if app.out_of_band_ack_visible {
                crate::ui::render_out_of_band_modal(f, f.area(), app.out_of_band_rf_hz);
            } else if app.freq_modal.visible {
                crate::ui::render_freq_modal(f, f.area(), &app.freq_modal);
            }
```

(Place this so it does not get hidden by, and does not hide, the existing modals — match the existing if/else-if overlay chain.)

- [ ] **Step 3: Add the split chip to the title bar**

Where the TX-policy banner renders (`ui/mod.rs:271` area), append a split indicator when `app.split_tx_hz != 0`, e.g. a cyan chip `SPLIT TX 14.090`. Follow the existing banner-building code style (read the surrounding lines first). Representative:

```rust
    if app.split_tx_hz != 0 {
        spans.push(Span::styled(
            format!(" SPLIT TX {:.3} ", app.split_tx_hz as f64 / 1e6),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
    }
```

- [ ] **Step 4: Build + run TUI tests**

Run: `cargo build -p pancetta-tui && cargo test -p pancetta-tui`
Expected: builds; tests pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt -p pancetta-tui
git add pancetta-tui/src/ui/mod.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): render freq modal, out-of-band ack modal, split chip"
```

### Task 12: Relay `SetSplit`; clear split on band change

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs` — handle `TuiCommand::SetSplit` (store atomic + forward bus + status); in the `SetFrequency` handler (`:810`), when it is a band change, also clear split
- Modify: `pancetta/src/coordinator/autonomous.rs` — on `ChangeBand`, clear split
- The relay needs the `split_tx_frequency_hz` clone in scope (capture it where `cmd_operating_freq_hz` is captured, using `coordinator.split_tx_frequency_hz()`)

- [ ] **Step 1: Capture the split atomic into the relay task**

Find where `cmd_operating_freq_hz` is cloned for the relay task (search `rg -n "cmd_operating_freq_hz" pancetta/src/coordinator/tui_relay.rs` and the spawn site). Add a sibling capture, e.g.:

```rust
        let cmd_split_tx_hz = self.split_tx_frequency_hz();
```

- [ ] **Step 2: Handle `SetSplit` in the TuiCommand match**

In the `match command { ... }` (same level as the `SetFrequency` arm at `:810`), add:

```rust
                        pancetta_tui::tui_runner::TuiCommand::SetSplit { enabled, tx_frequency } => {
                            let store = if enabled { tx_frequency } else { 0 };
                            cmd_split_tx_hz.store(store, Ordering::Relaxed);
                            info!(target: "rig.split", "TUI SetSplit enabled={} tx={} Hz", enabled, tx_frequency);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetSplit { enabled, tx_frequency },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward SetSplit to hamlib: {}", e);
                            }
                        }
```

- [ ] **Step 3: Clear split on a band change in the `SetFrequency` handler**

In the `SetFrequency` arm, inside the `if super::is_band_change(old_freq_hz, frequency) { ... }` block (`:826`), after the teardown send, clear split (changing RX band invalidates the split TX freq):

```rust
                                // A band change invalidates any split TX freq.
                                if cmd_split_tx_hz.swap(0, Ordering::Relaxed) != 0 {
                                    let clr = ComponentMessage::new(
                                        ComponentId::Tui,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetSplit {
                                                enabled: false,
                                                tx_frequency: 0,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    let _ = cmd_message_bus.send_message(clr).await;
                                }
```

- [ ] **Step 4: Clear split on autonomous `ChangeBand`**

In `coordinator/autonomous.rs` where `OperatorAction::ChangeBand { dial_frequency }` is handled (`:509-567`), after it stores the new dial / sends teardown, clear the split atomic the same way (you will need the split atomic clone captured into the autonomous task — follow the `operating_frequency_hz` capture there). Representative:

```rust
            if split_tx_frequency_hz.swap(0, Ordering::Relaxed) != 0 {
                // send SetSplit{ enabled:false, tx_frequency:0 } onto the bus (mirror Step 3)
            }
```

- [ ] **Step 5: Build**

Run: `cargo build -p pancetta`
Expected: builds clean. Confirm `warn` is imported in `tui_relay.rs`.

- [ ] **Step 6: Commit**

```bash
cargo fmt -p pancetta
git add pancetta/src/coordinator/tui_relay.rs pancetta/src/coordinator/autonomous.rs
git commit -m "feat(coordinator): relay SetSplit; clear split on band change (manual + autonomous)"
```

---

## Phase 5 — Coordinator-level end-to-end test

### Task 13: coord_sim scenario — split-active QSO logs TX dial; mock keys with split

**Files:**
- Modify: `pancetta/tests/coord_sim.rs` — extend `CoordSim` to carry/inject the split atomic; add a scenario

- [ ] **Step 1: Read the existing CoordSim fixture**

Run: `rg -n "operating_frequency|dial_frequency|active_tx_qsos|set_dial_frequency_source|MockRig" pancetta/tests/coord_sim.rs`
Read the construction so the split atomic is injected exactly like the dial atomic (the fixture mirrors `coordinator/qso.rs` wiring).

- [ ] **Step 2: Write the failing scenario**

Add a test that: sets the RX dial atomic to `14_074_000`, sets the split atomic to `14_090_000`, injects both into the sim's `QsoManager` (`set_dial_frequency_source` + `set_split_tx_frequency_source`), drives a QSO to completion, and asserts the `QsoCompleted` metadata `frequency` reflects the **split** dial (≈ `14_090_000 + offset`), not the RX dial. If the fixture has a helper that runs a full exchange and returns completed metadata, reuse it; otherwise mirror an existing completion scenario.

```rust
#[tokio::test(flavor = "current_thread")]
async fn split_active_qso_logs_tx_dial() {
    let sim = CoordSim::new().await; // or the fixture's constructor
    sim.set_rx_dial(14_074_000);
    sim.set_split_tx(14_090_000);
    // ... drive an exchange to Completed using the fixture's existing helpers ...
    let meta = sim.run_to_completion_returning_metadata().await; // adapt to real API
    // offset is small (<4 kHz); assert we're stamped on 20m split TX dial, not RX.
    assert!(meta.frequency >= 14_090_000.0 && meta.frequency < 14_094_000.0,
        "expected split TX dial stamp, got {}", meta.frequency);
}
```

Adapt method names to the fixture's real API discovered in Step 1 — do not invent helpers that don't exist; if a getter/setter for the split atomic is missing, add it to `CoordSim` in this task.

- [ ] **Step 3: Run to verify it fails (then passes after wiring)**

Run: `cargo test -p pancetta --test coord_sim split_active_qso_logs_tx_dial`
Expected: FAIL first (split source not injected in the fixture), then PASS after Step 2's injection is added.

- [ ] **Step 4: Run the full coord_sim suite**

Run: `cargo test -p pancetta --test coord_sim`
Expected: all pass (existing scenarios unaffected — simplex path: split atomic stays 0).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add pancetta/tests/coord_sim.rs
git commit -m "test(coord_sim): split-active QSO logs TX dial RF"
```

---

## Phase 6 — Docs + full verification

### Task 14: Documentation

**Files:**
- Modify: `CLAUDE.md` (Architecture Highlights — add a split/arbitrary-freq bullet)
- Modify: `pancetta-tui` help overlay / keymap text if it lists keys (search `rg -n "Shift" pancetta-tui/src/ui/mod.rs` and the help text) — add Shift+`F`

- [ ] **Step 1: Add a CLAUDE.md architecture bullet** describing: one `split_tx_frequency_hz` atomic (0=simplex), `RigControl::set_split`/`set_split_freq` (rigctld `S`/`I`, `OPERATOR-CONFIRM(split)`), `RigControlMessage::SetSplit`, effective-TX-dial RF stamp, Shift+`F` modal, interim once-per-session US-out-of-band ack modal, split-clears-on-band-change. Reference the spec and the `project_global_bandplan_todo` future work.

- [ ] **Step 2: Update the help/keymap text** to include Shift+`F` = "set arbitrary dial / split".

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add CLAUDE.md pancetta-tui/src/ui/mod.rs
git commit -m "docs: document arbitrary-freq + split feature and Shift+F keymap"
```

### Task 15: Full workspace verification (pre-push gate)

- [ ] **Step 1: fmt check**

Run: `cargo fmt --check`
Expected: clean (no diffs).

- [ ] **Step 2: clippy (workspace must stay at zero warnings)**

Run: `cargo clippy --workspace --features transmit --all-targets -- -D warnings`
Expected: no warnings/errors.

- [ ] **Step 3: full workspace test (the pre-push hook runs this on the working tree)**

Run: `cargo test --workspace --features transmit`
Expected: all pass.

- [ ] **Step 4: hamlib single-threaded suite**

Run: `cargo test -p pancetta-hamlib --lib -- --test-threads=1`
Expected: all pass.

- [ ] **Step 5: Controller pushes (NOT a subagent)** — `git push origin main` (or open a PR if the default-branch classifier blocks), then verify foreground with `git ls-remote origin` that the remote HEAD matches local. Do not pipe push through `tail`.

---

## Self-review notes (author)

- **Spec coverage:** §1 state model → Tasks 6–7; §2 hamlib plumbing → Tasks 1–5; §3 RF stamp/logging → Task 6 (FREQ_RX stretch intentionally omitted — note below); §4 TUI modal → Tasks 8–11; §5 out-of-band warning → Tasks 8, 10, 11; §6 autonomous/band-change clear → Task 12; §7 testing → embedded TDD steps + Task 13.
- **FREQ_RX (stretch):** the spec marks `FREQ_RX` as "only if cheap." It is NOT in this plan's tasks (keeps scope tight). If desired, add it where the ADIF record is built (`pancetta-qso/src/adif.rs:414-415`) reading the RX dial when split is active; tracked as a follow-up, not a gap.
- **Keymap deviation:** Shift+`F` (open) instead of `f` (taken). Clear-split = blank TX field in the modal (no second key). Flagged at top.
- **Naming consistency:** `effective_tx_dial`, `set_split_tx_frequency_source`, `split_tx_frequency_hz`, `parse_mhz_to_hz`, `tx_rf_out_of_us_band`, `FreqModalState`, `FreqModalFieldOpt`, `render_freq_modal`, `render_out_of_band_modal`, `TuiCommand::SetSplit`, `RigControlMessage::SetSplit` — used consistently across tasks. (Task 9 explicitly says pick ONE field-enum name and keep it; the plan uses `FreqModalFieldOpt` throughout.)
- **No exhaustiveness breaks:** the hamlib `_ => {}` arm absorbs `SetSplit` until Task 5 adds the explicit arm; trait default bodies keep all existing `RigControl` impls compiling.
