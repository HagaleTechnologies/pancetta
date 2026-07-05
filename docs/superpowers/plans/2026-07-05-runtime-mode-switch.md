# Runtime FT8/FT4/FT2 Mode Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator cycle pancetta's station-wide FT8/FT4/FT2 operating mode live (Shift+M), instead of only at startup via `[rig].mode`, gated so a switch can never land mid-QSO.

**Architecture:** A new `Coordinator::active_protocol_mode: Arc<AtomicU8>` (encoding `pancetta_config::OperatingMode`) replaces the write-once `active_protocol` field. A single synchronous function, `try_switch_operating_mode`, gates on the existing `active_tx_qsos` set and atomically flips `ft8_config.protocol` + the timing atomics with zero `.await` in the critical section. Three already-existing "re-check shared state every window" loops (DSP buffer sizing, decode-loop decoder rebuild, TX-worker encoder selection) each grow one more field to watch, mirroring mechanisms that already exist for band-switching and tier-preset changes.

**Tech Stack:** Rust, tokio (async runtime), `pancetta-ft8` (codec), `pancetta-config` (config types), `ratatui` (TUI).

## Global Constraints

- `mode=FT8`, no switch ever requested ⇒ every changed code path must be byte-identical to today's behavior (the standing regression invariant already used for FT4/Hound/split work in this codebase).
- No `.await` between the `active_tx_qsos` check and the point the switch is fully applied (see Task 7) — this is a hard safety invariant, not a style preference.
- FT2 is not scoped for correctness in this plan — `protocol_from_mode` already falls back to FT8 protocol params when the `ft2` Cargo feature is off (the default build). The mode-switch mechanism must not special-case this; it inherits the existing fallback automatically by routing through `protocol_from_mode`.
- Every new pure function gets unit tests before the surrounding wiring task is considered done (TDD).
- Run `cargo test --workspace --features transmit` before the final commit (per project convention — this passes safely, parking_lot deadlock was fixed 2026-04-28).

---

### Task 1: `OperatingMode` u8 codec + cycle

**Files:**
- Modify: `pancetta-config/src/rig.rs:14-21` (the `OperatingMode` enum), and its test module (search `test_operating_mode_ft2` for the existing test block, ~line 1200).
- Test: same file, `#[cfg(test)] mod tests` block.

**Interfaces:**
- Produces: `OperatingMode::as_u8(&self) -> u8` (Ft8=0, Ft4=1, Ft2=2), `OperatingMode::from_u8(v: u8) -> Self` (unrecognized → Ft8), `OperatingMode::cycle(&self) -> Self` (Ft8→Ft4→Ft2→Ft8). Used by Task 2 (Coordinator atomic) and Task 6 (TUI cycle handler).

- [ ] **Step 1: Write the failing tests**

Add to `pancetta-config/src/rig.rs`'s test module:

```rust
#[test]
fn operating_mode_u8_roundtrip() {
    assert_eq!(OperatingMode::from_u8(OperatingMode::Ft8.as_u8()), OperatingMode::Ft8);
    assert_eq!(OperatingMode::from_u8(OperatingMode::Ft4.as_u8()), OperatingMode::Ft4);
    assert_eq!(OperatingMode::from_u8(OperatingMode::Ft2.as_u8()), OperatingMode::Ft2);
    assert_eq!(OperatingMode::Ft8.as_u8(), 0);
    assert_eq!(OperatingMode::Ft4.as_u8(), 1);
    assert_eq!(OperatingMode::Ft2.as_u8(), 2);
}

#[test]
fn operating_mode_from_u8_unrecognized_defaults_to_ft8() {
    assert_eq!(OperatingMode::from_u8(99), OperatingMode::Ft8);
}

#[test]
fn operating_mode_cycle_order() {
    assert_eq!(OperatingMode::Ft8.cycle(), OperatingMode::Ft4);
    assert_eq!(OperatingMode::Ft4.cycle(), OperatingMode::Ft2);
    assert_eq!(OperatingMode::Ft2.cycle(), OperatingMode::Ft8);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta-config operating_mode_u8_roundtrip operating_mode_from_u8_unrecognized_defaults_to_ft8 operating_mode_cycle_order`
Expected: FAIL with "no method named `as_u8`/`from_u8`/`cycle` found"

- [ ] **Step 3: Implement**

Add right after the `OperatingMode` enum definition in `pancetta-config/src/rig.rs` (after line 21):

```rust
impl OperatingMode {
    /// Stable `u8` encoding for atomic storage. The mapping is fixed and
    /// MUST NOT change (`0` = Ft8, `1` = Ft4, `2` = Ft2).
    pub fn as_u8(&self) -> u8 {
        match self {
            OperatingMode::Ft8 => 0,
            OperatingMode::Ft4 => 1,
            OperatingMode::Ft2 => 2,
        }
    }

    /// Decode an [`OperatingMode`] from its stable `u8` encoding (see
    /// [`OperatingMode::as_u8`]). Any unrecognized value decodes to the safe
    /// default [`OperatingMode::Ft8`] — callers writing the atomic only ever
    /// store values produced by `as_u8`, so this branch is defensive.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => OperatingMode::Ft8,
            1 => OperatingMode::Ft4,
            2 => OperatingMode::Ft2,
            _ => OperatingMode::Ft8,
        }
    }

    /// Cycle to the next mode in the Ft8 → Ft4 → Ft2 → Ft8 order. Drives the
    /// operator's runtime mode-switch key (Shift+M).
    pub fn cycle(&self) -> Self {
        match self {
            OperatingMode::Ft8 => OperatingMode::Ft4,
            OperatingMode::Ft4 => OperatingMode::Ft2,
            OperatingMode::Ft2 => OperatingMode::Ft8,
        }
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p pancetta-config operating_mode_u8_roundtrip operating_mode_from_u8_unrecognized_defaults_to_ft8 operating_mode_cycle_order`
Expected: PASS (3 tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta-config/src/rig.rs
git commit -m "feat(config): add OperatingMode u8 codec + cycle for runtime mode switching"
```

---

### Task 2: Coordinator `active_protocol_mode` atomic

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs:538-542` (replace the `active_protocol` field), `~860-866` (construction), `~977-980` (struct-literal init), `~1324-1330` (accessor).
- Test: `pancetta/src/coordinator/mod.rs` existing test module (search `protocol_from_mode_maps_ft8_and_ft4`, ~line 1601).

**Interfaces:**
- Consumes: `OperatingMode::as_u8`/`from_u8` (Task 1).
- Produces: `Coordinator::active_protocol_mode(&self) -> Arc<AtomicU8>` (raw handle for hot loops), `Coordinator::active_protocol(&self) -> pancetta_ft8::Protocol` (unchanged signature, now computed live from the atomic instead of returning a stored `Copy` field — every existing call site, e.g. `tx.rs:804`, keeps compiling unchanged). Task 4/5/6/7 clone `active_protocol_mode()` into their hot loops; Task 7 also stores into it.

- [ ] **Step 1: Write the failing test**

Add near `protocol_from_mode_maps_ft8_and_ft4` in `pancetta/src/coordinator/mod.rs`'s test module:

```rust
#[test]
fn active_protocol_reads_live_from_atomic() {
    use std::sync::atomic::{AtomicU8, Ordering};
    let atomic = Arc::new(AtomicU8::new(pancetta_config::OperatingMode::Ft8.as_u8()));
    assert_eq!(
        protocol_from_mode(pancetta_config::OperatingMode::from_u8(
            atomic.load(Ordering::Relaxed)
        )),
        pancetta_ft8::Protocol::Ft8
    );
    atomic.store(pancetta_config::OperatingMode::Ft4.as_u8(), Ordering::Relaxed);
    assert_eq!(
        protocol_from_mode(pancetta_config::OperatingMode::from_u8(
            atomic.load(Ordering::Relaxed)
        )),
        pancetta_ft8::Protocol::Ft4
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta active_protocol_reads_live_from_atomic`
Expected: FAIL to compile is not expected here (this test only exercises Task 1's already-shipped codec + the existing `protocol_from_mode`) — this test should already PASS once Task 1 lands. Its real purpose is to lock in the exact load/decode idiom the rest of this task now depends on. If it fails, stop and recheck Task 1 landed first.

- [ ] **Step 3: Replace the field, constructor, and accessor**

In `pancetta/src/coordinator/mod.rs`, replace the field at line 538-542:

```rust
    /// Active digital-mode protocol (FT8 / FT4 / FT2), encoded as
    /// `pancetta_config::OperatingMode::as_u8()`. Seeded once at startup from
    /// `[rig].mode`; from this task onward it is also written by
    /// `try_switch_operating_mode` when the operator cycles mode live
    /// (Shift+M). The TX worker, DSP thread, and decode loop all poll this
    /// atomic each iteration (mirroring how they already poll the dial-freq
    /// and tier-preset atomics) so a runtime switch reaches every consumer
    /// without a restart.
    active_protocol_mode: Arc<std::sync::atomic::AtomicU8>,
```

Replace the constructor snippet at ~line 860-866 (`let active_protocol = match config.rig.operating_mode() { ... }`) with:

```rust
        let active_operating_mode = match config.rig.operating_mode() {
            Ok(mode) => mode,
            Err(e) => {
                warn!("invalid [rig].mode ({e}); defaulting to FT8");
                pancetta_config::OperatingMode::Ft8
            }
        };
        let active_protocol = protocol_from_mode(active_operating_mode);
```

(The rest of the constructor block — `active_slot_ns_init`, `active_decode_phase_ns_init`, the `info!` log — is unchanged; it already derives from the local `active_protocol` variable, which still exists as a local here, just no longer stored as a `Copy` field.)

Replace the struct-literal field at ~line 977-980:

```rust
            // Active digital-mode protocol from [rig].mode (default Ft8),
            // encoded for atomic storage. Written by `try_switch_operating_mode`
            // when the operator cycles mode live (Task 7).
            active_protocol_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                active_operating_mode.as_u8(),
            )),
```

Replace the accessor at ~line 1324-1330:

```rust
    /// Raw atomic handle for hot loops (DSP thread, decode loop, TX worker)
    /// that need to poll the live mode every iteration without a function-call
    /// indirection through `Protocol`.
    pub(crate) fn active_protocol_mode(&self) -> Arc<std::sync::atomic::AtomicU8> {
        self.active_protocol_mode.clone()
    }

    /// Active digital-mode protocol (FT8 / FT4 / FT2), read live from
    /// `active_protocol_mode`. `Ft8` (the default) is byte-identical to the
    /// old write-once field's value until the first runtime switch.
    pub(crate) fn active_protocol(&self) -> pancetta_ft8::Protocol {
        protocol_from_mode(pancetta_config::OperatingMode::from_u8(
            self.active_protocol_mode
                .load(std::sync::atomic::Ordering::Relaxed),
        ))
    }
```

- [ ] **Step 4: Run to verify it compiles and passes**

Run: `cargo build -p pancetta --features transmit && cargo test -p pancetta active_protocol_reads_live_from_atomic protocol_from_mode_maps_ft8_and_ft4`
Expected: builds clean, both tests PASS. (`tx.rs:804`'s `self.active_protocol()` call site needs no edit — same name, same return type.)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/mod.rs
git commit -m "refactor(coordinator): back active_protocol with a live atomic"
```

---

### Task 3: `mode_flush_decision` pure function

**Files:**
- Modify: `pancetta/src/coordinator/dsp.rs` — add right after `band_flush_decision` (line ~84).
- Test: same file's `#[cfg(test)] mod tests` block (search `use super::band_flush_decision;`, ~line 675).

**Interfaces:**
- Produces: `mode_flush_decision(cached_mode: u8, cur_mode: u8) -> bool`. Consumed by Task 4.

- [ ] **Step 1: Write the failing tests**

Add to `dsp.rs`'s test module:

```rust
#[test]
fn mode_flush_decision_same_mode_no_flush() {
    assert!(!mode_flush_decision(0, 0));
}

#[test]
fn mode_flush_decision_changed_mode_flushes() {
    assert!(mode_flush_decision(0, 1)); // FT8 -> FT4
    assert!(mode_flush_decision(1, 2)); // FT4 -> FT2
    assert!(mode_flush_decision(2, 0)); // FT2 -> FT8
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta mode_flush_decision`
Expected: FAIL with "cannot find function `mode_flush_decision`"

- [ ] **Step 3: Implement**

Add right after `band_flush_decision` in `dsp.rs`:

```rust
/// Decide whether the in-flight FT8 audio window must be flushed because the
/// active operating mode changed (the operator cycled FT8/FT4/FT2 live).
///
/// `cached_mode`/`cur_mode` are `pancetta_config::OperatingMode::as_u8()`
/// values. Unlike [`band_flush_decision`] there is no "unknown baseline"
/// case — the DSP thread always starts with a real mode read from config at
/// spawn time, so a simple inequality is the whole decision.
fn mode_flush_decision(cached_mode: u8, cur_mode: u8) -> bool {
    cached_mode != cur_mode
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p pancetta mode_flush_decision`
Expected: PASS (2 tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/dsp.rs
git commit -m "feat(dsp): add mode_flush_decision pure function"
```

---

### Task 4: DSP thread — resize/flush on mode change

**Files:**
- Modify: `pancetta/src/coordinator/dsp.rs:245-270` (capture the atomic before `spawn_blocking`), `:308` area (mutable locals), `:394-417` (the band-change guard block — add the mode-change guard right after it), `:340-348` (`next_window_time` — needs recompute alongside a mode flush).

**Interfaces:**
- Consumes: `Coordinator::active_protocol_mode()` (Task 2), `mode_flush_decision` (Task 3), `super::derive_dsp_timing`, `super::protocol_from_mode` (both pre-existing).
- Produces: nothing new consumed by later tasks — this closes out the DSP side.

- [ ] **Step 1: Capture the atomic before `spawn_blocking`**

In `start_dsp` (`dsp.rs`), right after `let operating_frequency_hz = self.operating_frequency_hz.clone();` (line ~250), add:

```rust
        // Live mode atomic — polled once per audio batch alongside the dial
        // frequency so a runtime FT8/FT4/FT2 switch (Shift+M) resizes the
        // decode window without a restart.
        let active_protocol_mode = self.active_protocol_mode();
```

- [ ] **Step 2: Make the timing locals mutable inside the closure**

Change (still inside `start_dsp`, before `let handle = tokio::task::spawn_blocking(move || {`):

The existing `dsp_window_samples`/`dsp_decode_phase`/`dsp_overlap_samples`/`dsp_slot_ns` `let` bindings (lines 267-270) are captured by value into the closure already — no change needed there (they seed the *initial* mutable locals inside the closure). Inside the closure, change the declarations at lines 287-291 from immutable to mutable, and add a cached-mode local:

```rust
            // Protocol-derived window length; mutable so a live mode switch
            // (Task 7) can resize it without restarting this thread.
            let mut ft8_window_samples = dsp_window_samples;
            let mut dsp_window_samples = dsp_window_samples;
            let mut dsp_decode_phase = dsp_decode_phase;
            let mut dsp_overlap_samples = dsp_overlap_samples;
            let mut dsp_slot_ns = dsp_slot_ns;
            // Cached mode (as `OperatingMode::as_u8()`), seeded from the same
            // config read that produced `protocol` above. Compared each
            // iteration against the live atomic to detect a runtime switch.
            let mut cached_mode_u8: u8 = pancetta_config::OperatingMode::Ft8.as_u8(); // placeholder, corrected below
```

Then, immediately above (near where `protocol`/`timing` are computed before the closure, lines 258-270), also capture the *seed* mode value to move into the closure — replace the `placeholder` line above with a value actually threaded in. Concretely, before `let handle = tokio::task::spawn_blocking(move || {`, compute:

```rust
        let seed_mode_u8 = config_operating_mode_u8; // see note below
```

To get `config_operating_mode_u8` cleanly, change the earlier block (lines 258-263) that derives `protocol` to also keep the `OperatingMode` value:

```rust
        let active_operating_mode = config
            .rig
            .operating_mode()
            .unwrap_or(pancetta_config::OperatingMode::Ft8);
        let protocol = super::protocol_from_mode(active_operating_mode);
        drop(config);
        let seed_mode_u8 = active_operating_mode.as_u8();
```

And inside the closure, seed the cached local from the moved-in value instead of the placeholder:

```rust
            let mut cached_mode_u8: u8 = seed_mode_u8;
```

- [ ] **Step 3: Recompute `ft8_window_samples` from the (now possibly stale) mutable `dsp_window_samples`**

`ft8_window_samples` was previously a `let` alias of `dsp_window_samples` taken once (line 287, comment "Captured from `DspTiming` above"). Since both are now `mut` locals updated together on a mode flush (Step 4 below keeps them in lock-step), no separate handling is needed here beyond declaring both `mut` as shown in Step 2 — just confirm every later read of `ft8_window_samples` (line 330, 527, 537) still compiles unchanged since it's still a `usize` local, just now mutable.

- [ ] **Step 4: Add the mode-change guard, right after the band-change guard**

In the closure's `while !shutdown...` loop, right after the existing band-change block ends (after `band_ref_dial_hz = new_ref;`, line ~417), add:

```rust
                        // Mode-change guard: if the operator cycled the
                        // station-wide mode (Shift+M) since the audio now
                        // buffered was captured, the buffered samples are the
                        // wrong frame geometry for the new mode's decoder —
                        // flush and resize, mirroring the band-change guard
                        // immediately above.
                        let cur_mode_u8 =
                            active_protocol_mode.load(Ordering::Relaxed);
                        if mode_flush_decision(cached_mode_u8, cur_mode_u8) {
                            let new_protocol = super::protocol_from_mode(
                                pancetta_config::OperatingMode::from_u8(cur_mode_u8),
                            );
                            let new_timing = super::derive_dsp_timing(
                                &pancetta_ft8::ProtocolParams::from_protocol(new_protocol),
                            );
                            info!(
                                target: "operator.override",
                                "DSP: mode change {} -> {} — flushing {} in-flight \
                                 samples and resizing the decode window",
                                pancetta_config::OperatingMode::from_u8(cached_mode_u8).as_u8(),
                                cur_mode_u8,
                                ft8_buffer.len()
                            );
                            dsp_window_samples = new_timing.window_samples;
                            ft8_window_samples = new_timing.window_samples;
                            dsp_decode_phase = new_timing.decode_phase;
                            dsp_overlap_samples = new_timing.overlap_samples;
                            dsp_slot_ns = new_timing.slot_ns;
                            ft8_buffer.clear();
                            last_live_wf_samples = 0;
                            bin_history.clear();
                            next_window_time = pancetta_core::slot::next_phase_with_period(
                                chrono::Utc::now(),
                                dsp_decode_phase,
                                dsp_slot_ns,
                            );
                            cached_mode_u8 = cur_mode_u8;
                        }
```

(`next_window_time` must already be declared `mut` — check the existing `let mut next_window_time = ...` at line ~340; it already is, since the boundary-anchored scheduling logic reassigns it elsewhere in the loop. If it is not `mut` in the current source, add `mut`.)

- [ ] **Step 5: Build and run the DSP test module**

Run: `cargo build -p pancetta --features transmit`
Expected: builds clean. (No new automated test here beyond `mode_flush_decision`'s own unit tests from Task 3 — the resize/flush wiring itself is exercised end-to-end by Task 11's `coord_sim` scenario, since it requires a running audio pipeline to observe.)

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/dsp.rs
git commit -m "feat(dsp): resize decode window and flush buffer on runtime mode switch"
```

---

### Task 5: Decode thread — extend the hot-rebuild check

**Files:**
- Modify: `pancetta/src/coordinator/ft8.rs:342-343` (cached locals), `:389-411` (the hot-rebuild diff/rebuild block).
- Test: none new here — covered by Task 11's `coord_sim` scenario (decoder rebuild is only observable through a live decode).

**Interfaces:**
- Consumes: `Ft8Config.protocol` (already a field, already read via `ft8_config_shared.try_read()`).

- [ ] **Step 1: Add the cached local**

Right after `let mut last_osd_depth = initial_ft8_config.osd_depth;` (line 343), add:

```rust
            let mut last_protocol = initial_ft8_config.protocol;
```

- [ ] **Step 2: Extend the diff and rebuild**

Change the condition at line 393 from:

```rust
                            if cur_max != last_max_passes || cur_osd != last_osd_depth {
```

to:

```rust
                            let cur_protocol = cfg_guard.protocol;
                            if cur_max != last_max_passes
                                || cur_osd != last_osd_depth
                                || cur_protocol != last_protocol
                            {
```

And inside the `Ok(d) => { ... last_max_passes = cur_max; last_osd_depth = cur_osd; }` arm (lines 397-404), add:

```rust
                                        last_protocol = cur_protocol;
```

Update the surrounding `info!` log to mention protocol too (optional but keeps the log honest):

```rust
                                        info!(
                                            "FT8 decoder rebuilt: max_decode_passes={}, osd_depth={:?}, protocol={}",
                                            cur_max, cur_osd, cur_protocol
                                        );
```

- [ ] **Step 3: Build**

Run: `cargo build -p pancetta --features transmit`
Expected: builds clean. (`pancetta_ft8::Protocol` already derives `PartialEq, Eq` per `pancetta-ft8/src/protocol.rs:10`, so `cur_protocol != last_protocol` compiles directly.)

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/ft8.rs
git commit -m "feat(ft8): rebuild decoder on runtime protocol change too"
```

---

### Task 6: TX worker — re-read protocol per request

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:798-826` (capture + initial encoder build before `tokio::spawn`), and inside the `while !shutdown` loop body (right after `abort_current_tx.store(false, Ordering::Release);`, ~line 837).

**Interfaces:**
- Consumes: `Coordinator::active_protocol_mode()` (Task 2).
- Produces: the loop-local `active_protocol`/`encoder` bindings that all existing call sites inside the loop (lines ~1023-1034, ~1616-1618) already reference by name — unchanged after this task, just now backed by a per-iteration re-check instead of a value fixed at spawn time.

- [ ] **Step 1: Capture the atomic instead of a one-time snapshot**

Replace line 804 (`let active_protocol = self.active_protocol();`) with:

```rust
            // Live mode atomic — re-checked at the top of every request-
            // processing cycle (Task 7 below) so a runtime FT8/FT4/FT2 switch
            // (Shift+M) takes effect on the very next TX, not just at
            // coordinator startup.
            let active_protocol_mode_atomic = self.active_protocol_mode();
            let active_protocol = self.active_protocol();
```

(Keep the `active_protocol` local — it still seeds the pre-loop `encoder` build at lines 821-826 unchanged, and the `info!("... protocol {}", active_protocol)` log at line 813-816 unchanged.)

- [ ] **Step 2: Make `active_protocol`/`encoder` mutable inside the spawned task**

Change line 821 from `let mut encoder = match active_protocol {` — it is already `let mut encoder`, so no change there. But `active_protocol` itself is captured by the closure as an immutable `Copy` value; shadow it as mutable right after the closure opens (right after the `info!` at lines 813-816, before the `let mut encoder = match active_protocol { ... }` block):

```rust
                let mut active_protocol = active_protocol;
```

- [ ] **Step 3: Re-check and rebuild at the top of the request loop**

Right after `abort_current_tx.store(false, Ordering::Release);` (line ~837, top of `while !shutdown.load(Ordering::Acquire) {`), add:

```rust
                    // Re-check the live mode atomic every cycle. Encoder
                    // construction is cheap (no per-request TX happens more
                    // than once every several seconds in FT8/FT4/FT2
                    // operation) so rebuilding on a detected change, rather
                    // than every single request, keeps the common case
                    // (no change) a plain atomic load.
                    let live_protocol = super::protocol_from_mode(
                        pancetta_config::OperatingMode::from_u8(
                            active_protocol_mode_atomic.load(Ordering::Relaxed),
                        ),
                    );
                    if live_protocol != active_protocol {
                        info!(
                            "TX worker: protocol changed {} -> {} — rebuilding encoder",
                            active_protocol, live_protocol
                        );
                        active_protocol = live_protocol;
                        encoder = match active_protocol {
                            pancetta_ft8::Protocol::Ft8 => Ft8Encoder::new(),
                            _ => Ft8Encoder::with_protocol(
                                pancetta_ft8::ProtocolParams::from_protocol(active_protocol),
                            ),
                        };
                    }
```

This reuses the exact construction logic already at lines 821-826 (just moved into a reachable-every-cycle spot); every existing call site inside the loop that references `active_protocol`/`encoder`/`modulator` by name (lines ~1023-1034, ~1616-1618) is unaffected, since those names still resolve to the same (now-live) bindings.

- [ ] **Step 4: Build**

Run: `cargo build -p pancetta --features transmit`
Expected: builds clean.

- [ ] **Step 5: Regression check — FT8 stays byte-identical**

Run the existing TX encode/modulate regression tests to confirm nothing shifted for the default FT8 path:

Run: `cargo test -p pancetta --features transmit encode_and_modulate`
Expected: PASS (pre-existing tests unaffected — this task only changes *when* the encoder is (re)built, not its construction logic).

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "fix(tx): re-read active protocol every TX cycle instead of once at spawn"
```

---

### Task 7: `try_switch_operating_mode` + `ModeSwitchError`

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs` — add near `protocol_from_mode`/`derive_dsp_timing` (~line 253, after `mode_str`).
- Test: same file's test module.

**Interfaces:**
- Consumes: `Coordinator::active_tx_qsos` (confirmed `pub(crate) active_tx_qsos: Arc<std::sync::RwLock<HashSet<String>>>`, `mod.rs:672`), `Coordinator::ft8_config` (confirmed `pub(crate) ft8_config: Arc<RwLock<Ft8Config>>` where `RwLock` = `tokio::sync::RwLock` per the `use tokio::sync::RwLock;` import at `mod.rs:363` — both fields are directly cloneable as `self.ft8_config.clone()`, no accessor needed), `active_protocol_mode` atomic (Task 2), `active_slot_ns`/`active_decode_phase_ns` atomics (pre-existing — `active_slot_ns` already has a `pub(crate) fn active_slot_ns(&self)` accessor at `mod.rs:1311`; `active_decode_phase_ns` does NOT have one yet, confirmed by grep — add it in Step 3 below), `derive_dsp_timing`, `protocol_from_mode`.
- Produces: `ModeSwitchError` enum (`QsosActive(usize)`, `ConfigLockBusy`) and `try_switch_operating_mode`, both **`pub`** (not `pub(crate)`) — this codebase's established precedent for coord_sim-testability is to bump exactly the items a `pancetta/tests/*.rs` integration test needs to full `pub` (see `coalesce_transmit_requests`, `tx_qso_is_live`, `active_tx_qso_key`, `resolve_required_parity`, all `pub fn` in `coordinator/{tx,mod}.rs` specifically for this reason — confirmed by grep). Task 12's `coord_sim.rs` scenarios call this function directly, so it must be reachable from outside the crate.
  ```rust
  pub fn try_switch_operating_mode(
      new_mode: pancetta_config::OperatingMode,
      active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
      ft8_config: &std::sync::Arc<tokio::sync::RwLock<pancetta_ft8::Ft8Config>>,
      active_protocol_mode: &std::sync::Arc<std::sync::atomic::AtomicU8>,
      active_slot_ns: &std::sync::Arc<std::sync::atomic::AtomicI64>,
      active_decode_phase_ns: &std::sync::Arc<std::sync::atomic::AtomicI64>,
  ) -> Result<(), ModeSwitchError>
  ```
  Consumed by Task 9 (tui_relay.rs's `CycleOperatingMode` handler) and Task 12 (`coord_sim.rs`).

- [ ] **Step 0: Add the missing `active_decode_phase_ns` accessor**

`active_slot_ns` already has a `pub(crate) fn active_slot_ns(&self) -> Arc<AtomicI64>` accessor (`mod.rs:1311-1313`), but `active_decode_phase_ns` does not. Add one identically shaped, right after it:

```rust
    /// Active protocol's decode-phase atomic in nanoseconds (FT8 → 13e9,
    /// FT4 → 6.5e9). Cloned into the decode loop's parity-stamping sites and
    /// (from this task onward) written by `try_switch_operating_mode`.
    pub(crate) fn active_decode_phase_ns(&self) -> Arc<std::sync::atomic::AtomicI64> {
        self.active_decode_phase_ns.clone()
    }
```

- [ ] **Step 1: Write the failing tests**

Add to `pancetta/src/coordinator/mod.rs`'s test module:

```rust
#[test]
fn try_switch_operating_mode_refuses_with_active_qso() {
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(
        std::collections::HashSet::from(["W1ABC-14074000".to_string()]),
    ));
    let ft8_config = Arc::new(tokio::sync::RwLock::new(Ft8Config::default()));
    let active_protocol_mode = Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_config::OperatingMode::Ft8.as_u8(),
    ));
    let active_slot_ns = Arc::new(std::sync::atomic::AtomicI64::new(15_000_000_000));
    let active_decode_phase_ns = Arc::new(std::sync::atomic::AtomicI64::new(13_000_000_000));

    let result = try_switch_operating_mode(
        pancetta_config::OperatingMode::Ft4,
        &active_tx_qsos,
        &ft8_config,
        &active_protocol_mode,
        &active_slot_ns,
        &active_decode_phase_ns,
    );

    assert!(matches!(result, Err(ModeSwitchError::QsosActive(1))));
    // Nothing was touched.
    assert_eq!(
        active_protocol_mode.load(Ordering::Relaxed),
        pancetta_config::OperatingMode::Ft8.as_u8()
    );
    assert_eq!(active_slot_ns.load(Ordering::Relaxed), 15_000_000_000);
}

#[tokio::test]
async fn try_switch_operating_mode_succeeds_when_idle() {
    let active_tx_qsos = Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
    let ft8_config = Arc::new(tokio::sync::RwLock::new(Ft8Config::default()));
    let active_protocol_mode = Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_config::OperatingMode::Ft8.as_u8(),
    ));
    let active_slot_ns = Arc::new(std::sync::atomic::AtomicI64::new(15_000_000_000));
    let active_decode_phase_ns = Arc::new(std::sync::atomic::AtomicI64::new(13_000_000_000));

    let result = try_switch_operating_mode(
        pancetta_config::OperatingMode::Ft4,
        &active_tx_qsos,
        &ft8_config,
        &active_protocol_mode,
        &active_slot_ns,
        &active_decode_phase_ns,
    );

    assert!(result.is_ok());
    assert_eq!(
        active_protocol_mode.load(Ordering::Relaxed),
        pancetta_config::OperatingMode::Ft4.as_u8()
    );
    assert_eq!(active_slot_ns.load(Ordering::Relaxed), 7_500_000_000);
    assert_eq!(ft8_config.read().await.protocol, pancetta_ft8::Protocol::Ft4);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta try_switch_operating_mode`
Expected: FAIL with "cannot find function/type `try_switch_operating_mode`/`ModeSwitchError`"

- [ ] **Step 3: Implement**

Add to `pancetta/src/coordinator/mod.rs` (near `protocol_from_mode`):

```rust
/// Error returned by [`try_switch_operating_mode`] when a runtime mode
/// switch (operator Shift+M) cannot be applied. `pub` (not `pub(crate)`) so
/// `pancetta/tests/coord_sim.rs` can assert on it directly — mirrors the
/// existing precedent of `tx_qso_is_live`/`coalesce_transmit_requests` being
/// `pub` specifically for coord_sim testability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSwitchError {
    /// `N` QSO(s) are currently non-terminal; switching mid-exchange would
    /// leave DSP/decode/TX disagreeing about the active protocol for an
    /// in-flight QSO. The operator retries once clear.
    QsosActive(usize),
    /// The shared `Ft8Config` lock was unavailable (contended or poisoned).
    /// Fails CLOSED — better to refuse the switch than leave DSP and decode
    /// disagreeing about frame geometry.
    ConfigLockBusy,
}

/// Attempt a runtime FT8/FT4/FT2 mode switch (operator Shift+M).
///
/// Gated on `active_tx_qsos` being empty: holds the **synchronous**
/// `std::sync::RwLock` read guard for the ENTIRE critical section below (no
/// `.await` anywhere in this function — it does not need to be `async`).
/// Any concurrent QSO-open needs a WRITE lock on that same set to register,
/// so it blocks until this function returns (or never starts if the initial
/// check already sees a non-empty set). This mirrors the auto-repark
/// feature's documented "zero `.await` between the live-QSO check and the
/// write" invariant (`coordinator/autonomous.rs`).
///
/// Ordering: the fallible step (`ft8_config.try_write()`) happens BEFORE any
/// atomic store, so a failure here leaves every consumer (DSP, decode, TX,
/// QSO) seeing the OLD mode consistently — never a torn state where some
/// consumers see new atomics but the decoder config is still old.
pub fn try_switch_operating_mode(
    new_mode: pancetta_config::OperatingMode,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    ft8_config: &std::sync::Arc<tokio::sync::RwLock<pancetta_ft8::Ft8Config>>,
    active_protocol_mode: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    active_slot_ns: &std::sync::Arc<std::sync::atomic::AtomicI64>,
    active_decode_phase_ns: &std::sync::Arc<std::sync::atomic::AtomicI64>,
) -> Result<(), ModeSwitchError> {
    use std::sync::atomic::Ordering;

    let active_guard = active_tx_qsos
        .read()
        .map_err(|_| ModeSwitchError::QsosActive(usize::MAX))?;
    if !active_guard.is_empty() {
        return Err(ModeSwitchError::QsosActive(active_guard.len()));
    }

    let new_protocol = protocol_from_mode(new_mode);
    let mut cfg_guard = ft8_config
        .try_write()
        .map_err(|_| ModeSwitchError::ConfigLockBusy)?;
    cfg_guard.protocol = new_protocol;
    drop(cfg_guard);

    let timing = derive_dsp_timing(&pancetta_ft8::ProtocolParams::from_protocol(new_protocol));
    let decode_phase_ns = timing
        .decode_phase
        .num_nanoseconds()
        .unwrap_or(13_000_000_000);
    active_slot_ns.store(timing.slot_ns, Ordering::Relaxed);
    active_decode_phase_ns.store(decode_phase_ns, Ordering::Relaxed);
    active_protocol_mode.store(new_mode.as_u8(), Ordering::Relaxed);

    drop(active_guard);
    Ok(())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p pancetta try_switch_operating_mode`
Expected: PASS (2 tests)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/mod.rs
git commit -m "feat(coordinator): add try_switch_operating_mode gated on active QSOs"
```

---

### Task 8: `QsoManager::set_active_mode` + `QsoMessage::SetOperatingMode`

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs:682-684` (near the `config()` accessor), `pancetta/src/message_bus.rs:523` area (the `QsoMessage` enum — add a new variant near `SetFoxMode`), `pancetta/src/coordinator/qso.rs:2843` area (add a new match arm near `SetFoxMode`'s handler).
- Test: `pancetta-qso/src/qso_manager.rs` test module (near `active_mode_ft4_stamps_metadata`, ~line 4002).

**Interfaces:**
- Consumes: nothing new.
- Produces: `QsoManager::set_active_mode(&mut self, mode: String)`. Consumed by Task 9's `tui_relay.rs` handler indirectly, via the bus message.

- [ ] **Step 1: Write the failing test**

Add to `pancetta-qso/src/qso_manager.rs`'s test module, right after `active_mode_ft4_stamps_metadata`:

```rust
#[tokio::test]
async fn set_active_mode_affects_qsos_opened_afterward() {
    let mut manager = QsoManager::new(test_config()); // starts at "FT8"
    manager.set_active_mode("FT4".to_string());
    assert_eq!(manager.config().active_mode, "FT4");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta-qso set_active_mode_affects_qsos_opened_afterward`
Expected: FAIL with "no method named `set_active_mode` found"

- [ ] **Step 3: Implement the setter**

Add to `pancetta-qso/src/qso_manager.rs`, right after the `config()` accessor (line ~684):

```rust
    /// Update the station-wide active mode string stamped into every
    /// [`QsoMetadata::mode`] this manager creates from now on. Only affects
    /// QSOs opened AFTER this call — anything already in progress keeps its
    /// already-stamped mode. Called from the coordinator's QSO task when the
    /// operator switches mode live (Shift+M); the caller (coordinator) is
    /// responsible for having already confirmed no QSO is active before the
    /// switch (see `try_switch_operating_mode`), so this setter itself does
    /// no gating.
    pub fn set_active_mode(&mut self, mode: String) {
        self.config.active_mode = mode;
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p pancetta-qso set_active_mode_affects_qsos_opened_afterward`
Expected: PASS

- [ ] **Step 5: Add the bus message variant**

In `pancetta/src/message_bus.rs`, add a new `QsoMessage` variant right after `SetFoxMode { on: bool }` (~line 643):

```rust
    /// The operator switched the station-wide operating mode live
    /// (Shift+M, gated by `try_switch_operating_mode` — the coordinator only
    /// sends this after confirming no QSO is active). Updates
    /// `QsoManager::active_mode` for QSOs opened from now on; anything
    /// already logged keeps its already-stamped mode.
    SetOperatingMode {
        /// The new mode string (`"FT8"` / `"FT4"` / `"FT2"`), from
        /// `super::mode_str`.
        mode: String,
    },
```

- [ ] **Step 6: Handle it in the QSO component's task**

In `pancetta/src/coordinator/qso.rs`, add a new match arm right after the `SetFoxMode` block ends (find the closing brace of the `crate::message_bus::QsoMessage::SetFoxMode { on } => { ... }` arm and insert immediately after):

```rust
                                        crate::message_bus::QsoMessage::SetOperatingMode {
                                            mode,
                                        } => {
                                            qso_manager.set_active_mode(mode.clone());
                                            info!("QSO manager active mode set to {}", mode);
                                        }
```

- [ ] **Step 7: Build**

Run: `cargo build -p pancetta --features transmit`
Expected: builds clean.

- [ ] **Step 8: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs pancetta/src/message_bus.rs pancetta/src/coordinator/qso.rs
git commit -m "feat(qso): add set_active_mode + SetOperatingMode bus message"
```

---

### Task 9: TUI trigger — `CycleOperatingMode` + Shift+M

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:354` area (add `TuiCommand::CycleOperatingMode` near `CycleTxPolicy`), `~1461-1466` (add the `Shift+M` key handler near the `g` handler), `pancetta/src/coordinator/tui_relay.rs:729` area (clone the new Arcs into the command-relay task), `~1338` area (add the handler near `CycleTxPolicy`'s).
- Test: `pancetta-tui/src/tui_runner.rs` test module (near `key_g_emits_cycle_tx_policy`-style tests, search `"g must emit CycleTxPolicy"` ~line 2710).

**Interfaces:**
- Consumes: `try_switch_operating_mode` (Task 7), `QsoMessage::SetOperatingMode` (Task 8), `mode_str` (pre-existing, `pancetta/src/coordinator/mod.rs:259`).
- Produces: `TuiCommand::CycleOperatingMode` (no payload — the coordinator owns the current mode and computes `.cycle()` itself, mirroring `CycleTxPolicy`).

- [ ] **Step 1: Write the failing TUI-side test**

Add to `pancetta-tui/src/tui_runner.rs`'s test module, near the `CycleTxPolicy` key test:

Confirmed exact test-harness API from the neighboring `key_g_cycles_tx_policy` test (`tui_runner.rs:2699-2717`): `make_runner().await -> (TuiRunner, Receiver<TuiCommand>, Arc<RwLock<App>>)`, key events go through `r.handle_key_event(key('M')).await.unwrap()` where `key(c: char) -> KeyEvent` (line 2169) wraps `KeyCode::Char(c)` with `KeyModifiers::NONE` — this codebase's convention is that the terminal already delivers an uppercase char for a shifted letter, so no separate modifier bit is asserted (matches how `g`/`P` are tested). Unlike `g`, `CycleOperatingMode` must NOT optimistically flip `app.station_info.mode` (Task 9 design: a mode switch can be refused, and flip-then-rollback would flicker the title bar), so this test only asserts the emitted command, not a local banner change:

```rust
#[tokio::test]
async fn key_shift_m_emits_cycle_operating_mode() {
    let (mut r, cmd_rx, app) = make_runner().await;
    let mode_before = app.read().await.station_info.mode.clone();
    r.handle_key_event(key('M')).await.unwrap();
    assert!(
        matches!(cmd_rx.try_recv(), Ok(TuiCommand::CycleOperatingMode)),
        "Shift+M must emit CycleOperatingMode"
    );
    assert_eq!(
        app.read().await.station_info.mode,
        mode_before,
        "no optimistic local flip — wait for the coordinator's ModeUpdate/StatusUpdate echo"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta-tui key_shift_m_emits_cycle_operating_mode`
Expected: FAIL with "cannot find variant `CycleOperatingMode`"

- [ ] **Step 3: Add the `TuiCommand` variant**

In `pancetta-tui/src/tui_runner.rs`, add right after `CycleTxPolicy,` (line ~354):

```rust
    /// Operator pressed Shift+M: cycle the station-wide operating mode
    /// FT8 → FT4 → FT2 → FT8. Unlike `CycleTxPolicy` this can be REFUSED
    /// (a QSO is active) — the coordinator relay does not optimistically
    /// flip anything locally; it waits for either a `ModeUpdate` (success)
    /// or a `StatusUpdate` (refusal) echo.
    CycleOperatingMode,
```

- [ ] **Step 4: Add the key handler**

In `pancetta-tui/src/tui_runner.rs`, right after the `g` (`CycleTxPolicy`) handler block (~line 1466), add:

```rust
            // Shift+M - cycle the station-wide operating mode. No optimistic
            // local flip (unlike `g`): a mode switch can be refused while a
            // QSO is active, and flip-then-rollback would flicker the
            // title-bar mode span. Wait for the coordinator's echo.
            KeyCode::Char('M') => {
                self.message_tx.send(TuiCommand::CycleOperatingMode)?;
            }
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p pancetta-tui key_shift_m_emits_cycle_operating_mode`
Expected: PASS

- [ ] **Step 6: Clone the new Arcs into the command-relay task**

In `pancetta/src/coordinator/tui_relay.rs`, right after `let cmd_tx_offset_hold_hz = self.tx_offset_hold_hz();` (line ~737), add:

```rust
        // Mode-switch machinery (Shift+M). Cloned here (not previously
        // needed by this task) so the CycleOperatingMode handler can call
        // `try_switch_operating_mode` directly, mirroring how `cmd_tx_policy`
        // is used by CycleTxPolicy. `ft8_config`/`active_tx_qsos` are
        // `pub(crate)` fields (confirmed `mod.rs:651,672`) so they clone
        // directly with no accessor; `active_protocol_mode`/`active_slot_ns`/
        // `active_decode_phase_ns` go through their `pub(crate) fn` accessors
        // (Task 2, Task 7 Step 0).
        let cmd_active_tx_qsos = self.active_tx_qsos.clone();
        let cmd_ft8_config = self.ft8_config.clone();
        let cmd_active_protocol_mode = self.active_protocol_mode();
        let cmd_active_slot_ns = self.active_slot_ns();
        let cmd_active_decode_phase_ns = self.active_decode_phase_ns();
```

- [ ] **Step 7: Add the handler**

In `pancetta/src/coordinator/tui_relay.rs`, right after the `CycleTxPolicy` arm closes (after line ~1382 where it sends `TuiMessage::TxPolicyUpdate`), add:

```rust
                        pancetta_tui::tui_runner::TuiCommand::CycleOperatingMode => {
                            let current = pancetta_config::OperatingMode::from_u8(
                                cmd_active_protocol_mode.load(Ordering::Relaxed),
                            );
                            let next = current.cycle();
                            match super::try_switch_operating_mode(
                                next,
                                &cmd_active_tx_qsos,
                                &cmd_ft8_config,
                                &cmd_active_protocol_mode,
                                &cmd_active_slot_ns,
                                &cmd_active_decode_phase_ns,
                            ) {
                                Ok(()) => {
                                    let mode_str = super::mode_str(next).to_string();
                                    warn!(
                                        target: "operator.override",
                                        "Operator switched operating mode: {} -> {}",
                                        super::mode_str(current),
                                        mode_str
                                    );
                                    let set_mode_msg = ComponentMessage::new(
                                        ComponentId::Tui,
                                        ComponentId::Qso,
                                        MessageType::QsoMessage(
                                            crate::message_bus::QsoMessage::SetOperatingMode {
                                                mode: mode_str.clone(),
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = cmd_message_bus.send_message(set_mode_msg).await
                                    {
                                        warn!("Failed to notify QSO component of mode switch: {}", e);
                                    }
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::ModeUpdate {
                                            mode: mode_str,
                                        },
                                    );
                                }
                                Err(super::ModeSwitchError::QsosActive(n)) => {
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "Mode".to_string(),
                                            status: format!(
                                                "can't switch mode: {} QSO(s) active",
                                                n
                                            ),
                                        },
                                    );
                                }
                                Err(super::ModeSwitchError::ConfigLockBusy) => {
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "Mode".to_string(),
                                            status: "mode switch busy, try again".to_string(),
                                        },
                                    );
                                }
                            }
                        }
```

(`try_switch_operating_mode`/`ModeSwitchError` are defined at the top of `coordinator/mod.rs`, i.e. directly in the `coordinator` module scope; `tui_relay.rs` is a sibling submodule, so `super::try_switch_operating_mode`/`super::ModeSwitchError`/`super::mode_str` is the correct and already-established path prefix — confirmed by the existing `super::mode_str(...)` call at `tui_relay.rs:135`.)

- [ ] **Step 8: Add `TuiMessage::ModeUpdate` (used above, needed by Task 10 too)**

In `pancetta-tui/src/tui_runner.rs`, add right after `TxPolicyUpdate { policy: pancetta_core::TxPolicy }` (line ~181):

```rust
    /// Authoritative mode echo. Sent by the coordinator relay after a
    /// successful `CycleOperatingMode` switch. Drives the title-bar mode
    /// span (Task 10) and the manual band-change dial resolution.
    ModeUpdate {
        /// New mode string (`"FT8"` / `"FT4"` / `"FT2"`).
        mode: String,
    },
```

And in `handle_message`, right after the `TxPolicyUpdate` arm (line ~693-695):

```rust
            TuiMessage::ModeUpdate { mode } => {
                app.station_info.mode = mode;
            }
```

- [ ] **Step 9: Build**

Run: `cargo build --features transmit`
Expected: builds clean across `pancetta-tui` and `pancetta`.

- [ ] **Step 10: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs pancetta/src/coordinator/tui_relay.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(tui): wire Shift+M runtime mode cycling end to end"
```

---

### Task 10: Title-bar display — bold mode span, remove redundant chip, live decode-relay stamping

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs:636-639` (bold the mode span), `:675-685` (remove the redundant `mode_chip_label` chip block), `:1405-1439` (remove `mode_chip_label` fn + its 2 tests), `pancetta/src/coordinator/tui_relay.rs:128-141` (make `relay_active_mode` a live per-decode read), `:280` (unchanged call site — still `mode: relay_active_mode.clone()`, now backed by a live value).

**Interfaces:**
- Consumes: `Coordinator::active_protocol_mode()` (Task 2), `mode_str` (pre-existing).

- [ ] **Step 1: Bold the always-visible mode span**

In `pancetta-tui/src/ui/mod.rs`, change lines 636-639 from:

```rust
        Span::styled(
            &app.station_info.mode,
            Style::default().fg(app.theme.accent_color()),
        ),
```

to:

```rust
        Span::styled(
            &app.station_info.mode,
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
```

- [ ] **Step 2: Remove the now-redundant conditional mode chip**

Delete the block at lines 675-685 (`// Active operating-mode chip...` through the closing `}` of the `if let Some(label) = mode_chip_label(...)`  block). The always-visible bold span from Step 1 already shows the current mode unconditionally, so this second, FT8-hidden chip becomes pure duplication for non-FT8 modes.

- [ ] **Step 3: Remove `mode_chip_label` and its tests**

Delete `pub fn mode_chip_label` (lines 1405-1419) and its `#[cfg(test)] mod tests { ... mode_chip_hidden_for_ft8 ... mode_chip_shown_for_non_ft8 ... }` block (lines 1421-1439) — grep the file first to confirm no other reference exists (`rg -n "mode_chip_label" pancetta-tui/src` should show zero remaining hits after this edit besides the deletion itself).

- [ ] **Step 4: Run the existing title-bar test to confirm it still passes**

Run: `cargo test -p pancetta-tui informational_chips_have_no_background_tx_policy_banner_keeps_its_own`
Expected: PASS unchanged — this test scans for *any* accent-fg, no-background cell in row 0 (there are still several: FREQ chip, SPLIT chip, TX-offset chip, the band span, and now the bold mode span), so removing one specific chip doesn't break its assertion.

- [ ] **Step 5: Make the decode-relay's mode stamping live**

In `pancetta/src/coordinator/tui_relay.rs`, replace the one-time `relay_active_mode` computation (lines 128-141):

```rust
        // Station-wide active operating mode string ("FT8"/"FT4"/"FT2"),
        // stamped onto every decode view forwarded to the TUI. FT8 is a
        // station-global mode (not per-decode); read once at relay startup
        // from `[rig].mode`. Defaults to "FT8" on parse error, so the legacy
        // path is byte-identical.
        let relay_active_mode = {
            let cfg = self.config.read().await;
            super::mode_str(
                cfg.rig
                    .operating_mode()
                    .unwrap_or(pancetta_config::OperatingMode::Ft8),
            )
            .to_string()
        };
```

with:

```rust
        // Station-wide active operating mode atomic — cloned here so the
        // per-decode mode stamping below reads the LIVE mode (a runtime
        // Shift+M switch takes effect on the very next decode), not a
        // one-time snapshot taken at relay startup.
        let relay_active_protocol_mode = self.active_protocol_mode();
```

Then, at the call site (line ~280, `mode: relay_active_mode.clone(),`), replace with:

```rust
                                mode: super::mode_str(pancetta_config::OperatingMode::from_u8(
                                    relay_active_protocol_mode.load(Ordering::Relaxed),
                                ))
                                .to_string(),
```

- [ ] **Step 6: Build**

Run: `cargo build --features transmit`
Expected: builds clean.

- [ ] **Step 7: Commit**

```bash
git add pancetta-tui/src/ui/mod.rs pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(tui): bold live mode span, drop redundant chip, live decode-relay mode"
```

---

### Task 11: Manual band-change mode-awareness

**Files:**
- Modify: `pancetta-tui/src/app.rs:1813-1849` (`apply_band_selection`).
- Test: `pancetta-tui/src/app.rs` test module (search for existing band-change tests, e.g. around `band_up`/`band_down` coverage).

**Interfaces:**
- Consumes: `app.station_info.mode` (already live as of Task 9/10), `pancetta_core::Band::dial_for(bool)` (pre-existing).

- [ ] **Step 1: Write the failing tests**

Add near the existing band-change tests in `pancetta-tui/src/app.rs`'s test module:

```rust
#[tokio::test]
async fn apply_band_selection_uses_ft4_dial_when_mode_is_ft4() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.station_info.mode = "FT4".to_string();
    // Land on 20m, which has a standard FT4 sub-band.
    let idx = app
        .config
        .bands
        .bands
        .iter()
        .position(|b| b.name == "20m")
        .expect("20m band must exist in default config");
    app.current_band_index = idx;
    let dial_hz = app.apply_band_selection(None);
    assert_eq!(dial_hz, 14_080_000); // pancetta_core::Band::Band20m.ft4_frequency()
}

#[tokio::test]
async fn apply_band_selection_ft8_unaffected_by_mode_field() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    assert_eq!(app.station_info.mode, "FT8");
    let idx = app
        .config
        .bands
        .bands
        .iter()
        .position(|b| b.name == "20m")
        .unwrap();
    app.current_band_index = idx;
    let dial_hz = app.apply_band_selection(None);
    assert_eq!(dial_hz, 14_074_000); // unchanged FT8 dial
}
```

(`apply_band_selection` is currently a private `fn` — confirm its visibility in `app.rs`; if the test module is a `#[cfg(test)] mod tests { use super::*; ... }` inside the same file, private visibility is fine since the test module already has access.)

- [ ] **Step 2: Run to verify the FT4 test fails**

Run: `cargo test -p pancetta-tui apply_band_selection_uses_ft4_dial_when_mode_is_ft4`
Expected: FAIL — the current code always uses `band.ft8_frequency`, so it would assert `14_074_000 == 14_080_000` and fail.

- [ ] **Step 3: Implement**

In `pancetta-tui/src/app.rs`'s `apply_band_selection`, replace lines 1831-1843 (`let band = ...` through `let base_status = ...`) with:

```rust
        let band = &self.config.bands.bands[self.current_band_index];
        let band_name = band.name.clone();
        // Mode-aware dial resolution (closes the manual-band-change gap
        // documented for FT4 mode). FT8 → the TUI's own band table
        // (unchanged). FT4 → `Band::dial_for(true)`, the same call the
        // autonomous ChangeBand handler already uses; falls back to the FT8
        // frequency (with a status note) on a band with no standard FT4
        // sub-band. FT2 → no standard dial-frequency table exists yet (FT2
        // remains blocked on the operator resolving two incompatible spec
        // candidates); falls back to FT8 with a status note.
        let core_band =
            pancetta_core::Band::from_frequency((band.ft8_frequency * 1_000_000.0) as u64);
        let (dial, fallback_note) = match self.station_info.mode.as_str() {
            "FT4" => match core_band.and_then(|b| b.dial_for(true)) {
                Some(hz) => (hz, None),
                None => (
                    (band.ft8_frequency * 1_000_000.0) as u64,
                    Some(format!(
                        "{} has no standard FT4 frequency — using FT8 dial",
                        band_name
                    )),
                ),
            },
            "FT2" => (
                (band.ft8_frequency * 1_000_000.0) as u64,
                Some("FT2 dial frequencies not yet defined — using FT8 dial".to_string()),
            ),
            _ => ((band.ft8_frequency * 1_000_000.0) as u64, None),
        };
        self.station_info.operating_frequency = dial as f64 / 1_000_000.0;
        let base_status = match fallback_note {
            Some(note) => format!("Band: {} — {:.3} MHz ({})", band_name, self.station_info.operating_frequency, note),
            None => format!("Band: {} — {:.3} MHz", band_name, self.station_info.operating_frequency),
        };
```

Then find the existing `let dial = (band.ft8_frequency * 1_000_000.0) as u64;` line right below (it becomes dead — the local is now computed above as `dial`) and remove that duplicate line. Confirm every later use of `dial` in the rest of `apply_band_selection` (the function's return value) still refers to the new `dial: u64` computed above.

(`Band::from_frequency` is confirmed as `pub fn from_frequency(freq: u64) -> Option<Band>` in `pancetta-core/src/types/band.rs:77` — takes whole Hz, hence the `as u64` cast above, not MHz.)

- [ ] **Step 4: Run to verify both tests pass**

Run: `cargo test -p pancetta-tui apply_band_selection_uses_ft4_dial_when_mode_is_ft4 apply_band_selection_ft8_unaffected_by_mode_field`
Expected: PASS (both)

- [ ] **Step 5: Run the full existing band-selection test suite for regressions**

Run: `cargo test -p pancetta-tui band_up band_down apply_band_selection`
Expected: PASS (all existing tests, unaffected for FT8)

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "fix(tui): make manual band-change mode-aware (FT4 dial, FT2 fallback)"
```

---

### Task 12: `coord_sim` scenarios, regression invariant, docs, final verification

**Files:**
- Modify: `pancetta/tests/coord_sim.rs` — add 4 fields to the `CoordSim` fixture (`ft8_config`, `active_protocol_mode`, `active_slot_ns`, `active_decode_phase_ns`, none of which exist on it today — confirmed by reading the current struct, which only holds `active_tx_qsos`/`tx_policy` of the mode-switch-relevant shared state) + 3 new scenarios; `CLAUDE.md` (Known Gaps / Architecture Highlights sections).

**Interfaces:**
- Consumes: everything from Tasks 1-11. Also confirmed from reading `coord_sim.rs` directly: the crate is imported as `pancetta_lib` (not `pancetta`) — `use pancetta_lib::coordinator::{...}` (line 71-74, already imports `active_tx_qso_key, coalesce_transmit_requests, remote_tx_permitted, tx_qso_is_live, CoalesceEntry`, all `pub` specifically for this file); `CoordSim::new(our_callsign: &str) -> Self` (not argument-less); `pump_qso_events(&mut self) -> Vec<PendingTx>` is **synchronous**, no `.await`; `manager: QsoManager` is a plain field, so `sim.manager.respond_to_cq_with(...).await` / `sim.manager.start_cq(...).await` / `sim.manager.set_active_mode(...)` (Task 8) all call directly.

- [ ] **Step 1: Add the new fields to `CoordSim`**

In `pancetta/tests/coord_sim.rs`, add to the `pub struct CoordSim { ... }` definition (right after the existing `pub tx_policy: Arc<AtomicU8>,` field):

```rust
    /// Shared FT8 decoder config, exactly as the coordinator holds it —
    /// `try_switch_operating_mode` writes `.protocol` here.
    pub ft8_config: Arc<tokio::sync::RwLock<pancetta_ft8::Ft8Config>>,
    /// Active-mode atomic, exactly as the coordinator holds it.
    pub active_protocol_mode: Arc<AtomicU8>,
    /// Active slot-length atomic (ns), exactly as the coordinator holds it.
    pub active_slot_ns: Arc<std::sync::atomic::AtomicI64>,
    /// Active decode-phase atomic (ns), exactly as the coordinator holds it.
    pub active_decode_phase_ns: Arc<std::sync::atomic::AtomicI64>,
```

And in `CoordSim::new`, right after `tx_policy: Arc::new(AtomicU8::new(TxPolicy::Full.as_u8())),` in the struct literal:

```rust
            ft8_config: Arc::new(tokio::sync::RwLock::new(pancetta_ft8::Ft8Config::default())),
            active_protocol_mode: Arc::new(AtomicU8::new(
                pancetta_config::OperatingMode::Ft8.as_u8(),
            )),
            active_slot_ns: Arc::new(std::sync::atomic::AtomicI64::new(15_000_000_000)),
            active_decode_phase_ns: Arc::new(std::sync::atomic::AtomicI64::new(13_000_000_000)),
```

Add `use pancetta_lib::coordinator::{try_switch_operating_mode, ModeSwitchError};` and `use pancetta_config::OperatingMode;` to the file's `use` block (near the existing `use pancetta_lib::coordinator::{...}` at line 71).

- [ ] **Step 2: Build to confirm the fixture still compiles**

Run: `cargo test -p pancetta --test coord_sim --no-run`
Expected: builds clean (no test bodies changed yet, just new fields + imports).

- [ ] **Step 3: Write the "refused with active QSO" scenario**

Mirrors the existing `ptt_keys_for_scheduled_qso` scenario's exact opening shape (`coord_sim.rs:865-894`):

```rust
#[tokio::test]
async fn mode_switch_refused_while_qso_active() {
    let mut sim = CoordSim::new("K5ARH").await;
    sim.manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");

    // Populate the REAL active_tx_qsos set via the same populater logic the
    // coordinator uses (StateChanged -> active inserts).
    let _pending = sim.pump_qso_events();

    let result = try_switch_operating_mode(
        OperatingMode::Ft4,
        &sim.active_tx_qsos,
        &sim.ft8_config,
        &sim.active_protocol_mode,
        &sim.active_slot_ns,
        &sim.active_decode_phase_ns,
    );

    assert!(
        matches!(result, Err(ModeSwitchError::QsosActive(n)) if n >= 1),
        "expected QsosActive, got {:?}",
        result
    );
    assert_eq!(
        sim.active_protocol_mode.load(Ordering::Relaxed),
        OperatingMode::Ft8.as_u8(),
        "atomic must be untouched on refusal"
    );
}
```

- [ ] **Step 4: Write the "succeeds when idle, next QSO uses new mode" scenario**

```rust
#[tokio::test]
async fn mode_switch_succeeds_idle_and_next_qso_uses_new_mode() {
    let mut sim = CoordSim::new("K5ARH").await;

    let result = try_switch_operating_mode(
        OperatingMode::Ft4,
        &sim.active_tx_qsos,
        &sim.ft8_config,
        &sim.active_protocol_mode,
        &sim.active_slot_ns,
        &sim.active_decode_phase_ns,
    );
    assert!(result.is_ok());
    assert_eq!(sim.active_slot_ns.load(Ordering::Relaxed), 7_500_000_000);

    // Mirror what the QSO component's task does on QsoMessage::SetOperatingMode
    // (Task 8) — direct call, since coord_sim has no separate task boundary.
    sim.manager.set_active_mode("FT4".to_string());

    sim.manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");
    let _pending = sim.pump_qso_events();

    let active = sim.manager.get_active_qsos().await;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].1.metadata.mode, "FT4");
}
```

- [ ] **Step 5: Write the regression invariant test**

```rust
#[tokio::test]
async fn mode_switch_never_requested_is_byte_identical_to_today() {
    let mut sim = CoordSim::new("K5ARH").await;
    assert_eq!(
        sim.active_protocol_mode.load(Ordering::Relaxed),
        OperatingMode::Ft8.as_u8()
    );
    assert_eq!(sim.active_slot_ns.load(Ordering::Relaxed), 15_000_000_000);

    // Exact body of `ptt_keys_for_scheduled_qso` — proves an FT8-only
    // operator who never touches Shift+M sees no behavior change.
    let qso_id = sim
        .manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
        )
        .await
        .expect("respond_to_cq_with");
    let pending = sim.pump_qso_events();
    assert!(!pending.is_empty());
    sim.drive_slot(pending).await;
    sim.timeline.assert_keyed_for_qso(&qso_id.to_string());
    sim.timeline.assert_all_released();
    sim.timeline
        .assert_keyed_at_offset(&qso_id.to_string(), 1500.0);
}
```

- [ ] **Step 6: Run the new scenarios**

Run: `cargo test -p pancetta --test coord_sim mode_switch`
Expected: PASS (3 new tests)

- [ ] **Step 7: Full workspace regression run**

Run: `cargo test --workspace --features transmit`
Expected: PASS, zero regressions (per project convention this is safe to run — parking_lot deadlock fixed 2026-04-28).

- [ ] **Step 8: Update CLAUDE.md**

Add a new bullet under "Architecture Highlights" (after the FT4-mode bullet), summarizing: `active_protocol_mode` is now a live atomic (not write-once), `try_switch_operating_mode` gates on `active_tx_qsos`, Shift+M triggers it, DSP/decode/TX all re-check per iteration, manual band-change is now mode-aware, and FT2 remains fallback-to-FT8 on non-`ft2`-feature builds (unchanged, pre-existing behavior — this work does not change FT2 correctness).

Also update the "Known Gaps and TODOs" section: remove or annotate-as-fixed the note about `apply_band_selection` being mode-unaware (grep `TODO(ft4)` across `CLAUDE.md`/`app.rs` comments to find the exact wording to update).

- [ ] **Step 9: Commit**

```bash
git add pancetta/tests/coord_sim.rs CLAUDE.md
git commit -m "test(coord_sim): add runtime mode-switch scenarios + update docs"
```

---

## Self-Review Notes (for the implementer)

Every field name, accessor, and helper referenced above (`ft8_config`'s `pub(crate)` visibility, the missing `active_decode_phase_ns()` accessor added in Task 7 Step 0, the `super::X` cross-module convention, `Band::from_frequency(u64)`'s exact signature, and `coord_sim.rs`'s real `CoordSim::new(&str)`/`pump_qso_events()`/`manager.respond_to_cq_with(...)` API) was confirmed by reading the live source during planning, not guessed. Two things are worth a second look during implementation, not because they're unresolved but because they're easy to get subtly wrong:

- **Task 6**: `Ft8Encoder::new()` / `Ft8Encoder::with_protocol(...)` are assumed cheap enough to rebuild on a detected protocol change (once every several seconds at most, since FT8/FT4/FT2 TX cadence is never faster than that). If profiling ever shows otherwise, cache all three encoder variants instead of rebuilding — but don't add that complexity up front without evidence it's needed.
- **Task 6, noted but explicitly NOT fixed here**: the keep-call pivot-retry path at `tx.rs:~1285` calls `encoder.encode_message(&new_text, None)` / `modulator.modulate_symbols(&s, 0.0)` directly (bypassing `encode_for_protocol`/`modulate_for_protocol`), which only produces correct output for FT8's fixed-length coding. This is a **pre-existing quirk** for FT4/FT2 (it already existed for startup-selected FT4 before this plan), not something this plan introduces or is scoped to fix — flag it as a follow-up if it turns out to matter once FT4/FT2 keep-calls are actually exercised on air.
