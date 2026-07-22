# Symptom C Adaptive Coalesce Window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix Symptom C (multi-TX slow-start) in `pancetta/src/coordinator/tx.rs` by replacing the
fixed 800ms TX-coalesce window with an adaptive one, and fold in a timing-accuracy fix so widening
that window never grows an existing (already-shipped, small) audio-alignment drift.

**Architecture:** Four sequential changes to one file (`pancetta/src/coordinator/tx.rs`, ~5000
lines, a single giant `start_transmitter_component` async fn with two near-duplicate worker match
arms — `TransmitRequest` and `MultiTransmitRequest`). Task 1 extracts a pure helper with zero
behavior change (regression-safe foundation). Task 2 replaces the fixed pre-coalesce sleep with an
adaptive one (touches only code *before* the arms, doesn't touch `coalesce_backlog_into` or
`coalesce_transmit_requests` at all). Tasks 3 and 4 apply the timing-accuracy fix to each arm
independently — they can be reviewed and tested in isolation from each other and from Tasks 1-2,
even though all four land in the same PR per the approved spec.

**Tech Stack:** Rust, tokio (async worker loop), chrono (UTC timestamps), crossbeam-channel
(`tx_rx`), existing `interruptible_sleep` helper.

## Global Constraints

- **Zero on-air truncation.** This is a pre-PTT-only change. No task may key PTT differently or
  abort an in-progress transmission — that's explicitly out of scope (spec §"Goal").
- **CLAUDE.md invariant — same parity, no sequential windows:** "Every concurrent active QSO
  transmits on the same parity; never TX in sequential windows." Task 2's adaptive extension must
  never itself decide which items share a bundle (that's `coalesce_transmit_requests`'s existing,
  untouched parity-conflict logic) — it only decides *how long to wait* before the existing,
  unmodified coalesce call runs once.
- **CLAUDE.md invariant — drop-stale-TX liveness recheck:** "the worker re-checks QSO liveness at
  the last instant before PTT." Tasks 3/4 must not move the fresh-schedule recompute to before the
  existing Step 4b/4b-arm liveness/arm-gate rechecks — it goes *after* them, immediately before PTT
  (Step 5), so it doesn't skip or reorder any existing gate.
- **Symptom-B protection preserved:** the defer/viability *decision* (`schedule.target_slot`,
  `schedule.deferred`, computed once in Step 2 from the frozen `request_received_at`) must never be
  re-derived later in either arm. Tasks 3/4 only refresh the pad/cursor *math* against the
  already-decided `target_slot` — never re-run `resolve_required_parity`/the use-current-vs-defer
  check a second time. This is why Task 1 splits `pad_and_cursor_for_target` out of `schedule_tx`
  instead of just calling `schedule_tx` again.
- **`mode=FT8` byte-identical:** every new constant/helper introduced must reduce to today's exact
  FT8 behavior when nothing extends (protocol-scaled functions use the existing
  `coalesce_collect_window_ms` cycle-ratio pattern).
- Existing `schedule_tx_tests` (tx.rs:~3556 onward) must still pass, unmodified, after every task —
  they're the regression net for the pure scheduling math this plan touches.

---

## Task 1: Extract `pad_and_cursor_for_target` from `schedule_tx`

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:127-166` (the `schedule_tx` function body)
- Test: `pancetta/src/coordinator/tx.rs` (`schedule_tx_tests` module, ~line 3556+)

**Interfaces:**
- Produces: `fn pad_and_cursor_for_target(now: chrono::DateTime<chrono::Utc>, target_slot: chrono::DateTime<chrono::Utc>, sample_rate: u32) -> (usize, usize)` (returns `(silent_pad_samples, cursor_offset_samples)`) — private to `tx.rs`, used by Tasks 3 and 4.
- `schedule_tx`'s public signature and `TxSchedule`'s fields are unchanged — this task is a pure
  internal refactor.

This is a no-behavior-change extraction: split the pad/cursor arithmetic out of `schedule_tx` into
its own function so Tasks 3/4 can recompute pad/cursor against an *already-decided* `target_slot`
without re-running the slot-selection logic (which must only ever run once, per the Global
Constraints above).

- [ ] **Step 1: Confirm the current `schedule_tx_tests` baseline passes**

Run: `cargo test -p pancetta --lib coordinator::tx::schedule_tx_tests -- --nocapture`
Expected: all existing tests in this module PASS (this is your before-snapshot; Task 1 must not
change any of their outcomes).

- [ ] **Step 2: Replace `schedule_tx`'s body, extracting the pad/cursor math**

In `pancetta/src/coordinator/tx.rs`, find the current `schedule_tx` function (starts at line 127):

```rust
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
) -> TxSchedule {
    use pancetta_core::slot::{
        current_slot_start_with_period, next_slot_with_parity_with_period, SlotParity,
    };

    let cur_start = current_slot_start_with_period(now, slot_ns);
    let cur_parity = SlotParity::of_with_period(cur_start, slot_ns);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let use_current = cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms;
    let target = if use_current {
        cur_start
    } else {
        next_slot_with_parity_with_period(now, required_parity, slot_ns)
    };
    let deferred = !use_current;

    // mstr relative to the chosen target. When target is in the future,
    // (now - target) is negative; clamp so we hit the early branch.
    let mstr_signed = (now - target).num_milliseconds();
    let mstr_unsigned = mstr_signed.max(0) as u64;

    let (silent_pad_ms, cursor_ms) = if mstr_unsigned < DELAY_MS {
        (DELAY_MS - mstr_unsigned, 0)
    } else {
        (0, mstr_unsigned - DELAY_MS)
    };

    TxSchedule {
        target_slot: target,
        silent_pad_samples: (silent_pad_ms as usize) * (sample_rate as usize) / 1000,
        cursor_offset_samples: (cursor_ms as usize) * (sample_rate as usize) / 1000,
        deferred,
    }
}
```

Replace it with:

```rust
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
) -> TxSchedule {
    use pancetta_core::slot::{
        current_slot_start_with_period, next_slot_with_parity_with_period, SlotParity,
    };

    let cur_start = current_slot_start_with_period(now, slot_ns);
    let cur_parity = SlotParity::of_with_period(cur_start, slot_ns);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let use_current = cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms;
    let target = if use_current {
        cur_start
    } else {
        next_slot_with_parity_with_period(now, required_parity, slot_ns)
    };
    let deferred = !use_current;

    let (silent_pad_samples, cursor_offset_samples) =
        pad_and_cursor_for_target(now, target, sample_rate);

    TxSchedule {
        target_slot: target,
        silent_pad_samples,
        cursor_offset_samples,
        deferred,
    }
}

/// Silent-pad / cursor-skip math for a FIXED target slot boundary, given the
/// instant audio is about to actually ship. Split out of `schedule_tx` so a
/// later, more accurate clock read can refresh the pad/cursor WITHOUT
/// re-deciding which slot to target — that decision (the `use_current` check
/// above) must only ever be made once, off the frozen pre-coalesce
/// `request_received_at` timestamp (see
/// docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md
/// §2). Re-running the full slot-selection logic with a later timestamp
/// risks flipping `deferred` after downstream gates/pivots already assumed
/// the original decision; this function can't do that — it only computes
/// how far into (or before) an already-chosen `target_slot` `now` falls.
fn pad_and_cursor_for_target(
    now: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    sample_rate: u32,
) -> (usize, usize) {
    // mstr relative to the target. When target is in the future,
    // (now - target) is negative; clamp so we hit the early branch.
    let mstr_signed = (now - target_slot).num_milliseconds();
    let mstr_unsigned = mstr_signed.max(0) as u64;

    let (silent_pad_ms, cursor_ms) = if mstr_unsigned < DELAY_MS {
        (DELAY_MS - mstr_unsigned, 0)
    } else {
        (0, mstr_unsigned - DELAY_MS)
    };

    (
        (silent_pad_ms as usize) * (sample_rate as usize) / 1000,
        (cursor_ms as usize) * (sample_rate as usize) / 1000,
    )
}
```

- [ ] **Step 3: Re-run the existing tests to confirm zero behavior change**

Run: `cargo test -p pancetta --lib coordinator::tx::schedule_tx_tests -- --nocapture`
Expected: identical PASS results to Step 1 — same tests, same outcomes. If anything changed,
the extraction introduced a behavior difference; stop and diff against the original body above.

- [ ] **Step 4: Add direct unit tests for `pad_and_cursor_for_target`**

In the `schedule_tx_tests` module (same file, uses the existing `at(seconds: f64)` helper), add:

```rust
#[test]
fn pad_and_cursor_for_target_matches_schedule_tx_for_same_now() {
    // Sanity: calling the extracted helper with schedule_tx's own chosen
    // target and "now" must reproduce schedule_tx's own pad/cursor exactly
    // — this is what makes the Task 1 extraction provably behavior-neutral.
    let now = at(5.0);
    let s = schedule_tx(now, SlotParity::Odd, 8000, 12_000, SLOT_NS);
    let (pad, cursor) = pad_and_cursor_for_target(now, s.target_slot, 12_000);
    assert_eq!(pad, s.silent_pad_samples);
    assert_eq!(cursor, s.cursor_offset_samples);
}

#[test]
fn pad_and_cursor_for_target_refreshes_against_a_later_now() {
    // The key new behavior Tasks 3/4 rely on: given the SAME target_slot,
    // a later "now" produces a LARGER cursor (more of the waveform's front
    // trimmed) — because more real time has passed relative to the slot
    // boundary, independent of when target_slot was originally decided.
    let target = at(0.0); // slot boundary itself
    let (pad_early, cursor_early) = pad_and_cursor_for_target(at(0.2), target, 12_000);
    let (pad_late, cursor_late) = pad_and_cursor_for_target(at(3.0), target, 12_000);
    assert!(pad_early > 0, "200ms in: still inside the DELAY_MS pre-roll, expect padding");
    assert_eq!(pad_late, 0, "3s in: past DELAY_MS, expect no padding");
    assert!(cursor_late > cursor_early, "later refresh must trim more of the waveform's front");
}

#[test]
fn pad_and_cursor_for_target_stable_within_delay_ms_window() {
    // Two "now" reads a few ms apart, both still inside the DELAY_MS
    // pre-roll, should both land in the padding branch (cursor == 0) —
    // confirms there's no discontinuity right at the DELAY_MS boundary.
    let target = at(10.0);
    let (_, cursor_a) = pad_and_cursor_for_target(at(10.1), target, 12_000);
    let (_, cursor_b) = pad_and_cursor_for_target(at(10.3), target, 12_000);
    assert_eq!(cursor_a, 0);
    assert_eq!(cursor_b, 0);
}
```

- [ ] **Step 5: Run the new tests**

Run: `cargo test -p pancetta --lib coordinator::tx::schedule_tx_tests -- --nocapture`
Expected: all tests PASS, including the 3 new ones (`pad_and_cursor_for_target_matches_schedule_tx_for_same_now`, `pad_and_cursor_for_target_refreshes_against_a_later_now`, `pad_and_cursor_for_target_stable_within_delay_ms_window`).

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "refactor(tx): extract pad_and_cursor_for_target from schedule_tx

Pure extraction, zero behavior change (verified: existing schedule_tx_tests
pass unmodified). Lets Tasks 3/4 of the Symptom C plan refresh pad/cursor
math against an already-decided target_slot without re-running slot
selection."
```

---

## Task 2: Adaptive TX coalesce window

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` — the constants near line 68-90, and the pre-coalesce
  sleep at approximately line 1408-1423 (search for the comment `Brief collection window so
  same-parity openings`). Exact line numbers will have shifted slightly from Task 1's edit —
  locate by that comment text, not the line number.
- Test: `pancetta/src/coordinator/tx.rs` (new `#[cfg(test)]` module near the new helper functions)

**Interfaces:**
- Consumes: `coalesce_collect_window_ms(protocol) -> u64` (existing, unchanged), `resolve_required_parity(...)` (existing, unchanged), `schedule_tx(...)` (existing, from Task 1 — same public signature), `interruptible_sleep(...)` (existing, unchanged).
- Produces: `fn adaptive_coalesce_cap_ms(head: &MessageType, request_received_at: chrono::DateTime<chrono::Utc>, tx_self_parity: pancetta_config::station::TxSelfParity, tx_late_max_ms: u64, sample_rate: u32, slot_ns: i64, protocol: pancetta_ft8::Protocol) -> u64` — used only within this task's own call site; no other task depends on it.
- Does NOT modify `coalesce_backlog_into` or `coalesce_transmit_requests` — this task only changes
  how long the worker waits *before* calling the existing, unmodified `coalesce_backlog_into` once.

This is the core Symptom C fix: instead of a single fixed 800ms (protocol-scaled) sleep before
coalescing, take the base wait once as today, then extend in further base-length increments *only
while the channel's queued-message count keeps growing*, capped by remaining `tx_late_max_ms`
headroom (so a request already late in its slot extends little or not at all — never worse than
today) and by a protocol-scaled absolute ceiling (so a busy pileup can't eat an outsized fraction of
a short FT4/FT2 slot).

- [ ] **Step 1: Add the two new constants near `COALESCE_COLLECT_WINDOW_MS`**

Find (near line 68):
```rust
const COALESCE_COLLECT_WINDOW_MS: u64 = 800;
```

Add immediately after the existing `coalesce_collect_window_ms` function (which ends around line
90, right before `/// Output of \`schedule_tx\`:`):

```rust
/// FT8-baseline cap on total EXTENSION time (beyond the mandatory base
/// `COALESCE_COLLECT_WINDOW_MS` wait) the Symptom-C adaptive coalesce window
/// may add. Scaled by the same cycle-ratio as `coalesce_collect_window_ms`
/// for FT4/FT2. Independent of `tx_late_max_ms` — the remaining-headroom cap
/// computed in `adaptive_coalesce_cap_ms` already bounds against that; this
/// is a second, protocol-proportionate ceiling so a busy pileup can't
/// monopolize an outsized fraction of a short FT4/FT2 slot even when
/// tx_late_max_ms headroom alone would allow it (tx_late_max_ms itself isn't
/// mode-scaled today — a separately tracked open question, not addressed by
/// this change). See
/// docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §1.
const COALESCE_MAX_EXTENSION_MS: u64 = 3000;

/// Safety margin subtracted from remaining `tx_late_max_ms` headroom before
/// it's used as the adaptive window's extension cap. Covers Step 1's
/// encode/modulate time and other fixed per-message overhead between
/// `request_received_at` and the actual coalesce point, none of which is
/// otherwise accounted for in the headroom math — without this margin, a
/// request that arrives with headroom just barely covering the extension
/// alone could still get pushed past the `tx_late_max_ms` cliff by that
/// extra overhead.
const COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// Protocol-scaled `COALESCE_MAX_EXTENSION_MS` — see that constant's doc.
fn coalesce_max_extension_ms(protocol: pancetta_ft8::Protocol) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((COALESCE_MAX_EXTENSION_MS as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}

/// Remaining `tx_late_max_ms` headroom, in ms, available to extend the
/// Symptom-C adaptive coalesce window for the given head request — bounds
/// the window so it can never push a request past the late-skip cliff.
/// Returns the full (protocol-scaled) `COALESCE_MAX_EXTENSION_MS` when the
/// head has already resolved to a DEFERRED (next-slot) target, since
/// there's no current-slot cliff to protect in that case, and `0` if `head`
/// isn't a `TransmitRequest` (defensive — the only caller checks this
/// first).
fn adaptive_coalesce_cap_ms(
    head: &MessageType,
    request_received_at: chrono::DateTime<chrono::Utc>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
    protocol: pancetta_ft8::Protocol,
) -> u64 {
    let MessageType::TransmitRequest { tx_parity, .. } = head else {
        return 0;
    };
    let required_parity =
        resolve_required_parity(*tx_parity, tx_self_parity, request_received_at, slot_ns);
    let probe = schedule_tx(
        request_received_at,
        required_parity,
        tx_late_max_ms,
        sample_rate,
        slot_ns,
    );
    let protocol_ceiling = coalesce_max_extension_ms(protocol);
    if probe.deferred {
        return protocol_ceiling;
    }
    let elapsed_in_slot_ms = (request_received_at - probe.target_slot)
        .num_milliseconds()
        .max(0) as u64;
    let headroom = tx_late_max_ms
        .saturating_sub(elapsed_in_slot_ms)
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS);
    headroom.min(protocol_ceiling)
}
```

- [ ] **Step 2: Write unit tests for `adaptive_coalesce_cap_ms` (TDD — write before wiring it in)**

Add to the `schedule_tx_tests` module (reuses the existing `at()` helper and `SLOT_NS`):

```rust
fn tx_request(tx_parity: Option<SlotParity>) -> MessageType {
    MessageType::TransmitRequest {
        message_text: "CQ TEST".to_string(),
        frequency_offset: 1500.0,
        qso_id: None,
        tx_parity,
        origin: crate::message_bus::TxOrigin::Local,
    }
}

#[test]
fn adaptive_cap_shrinks_for_a_late_arriving_head() {
    // Arrives 7.5s into an 8000ms tx_late_max_ms budget: only 500ms of
    // headroom remains before the safety margin (500ms) eats the rest —
    // cap should be at or near zero.
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(7.5),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 0);
}

#[test]
fn adaptive_cap_has_room_for_an_early_arriving_head() {
    // Arrives 1s into the slot: 7000ms of raw headroom before tx_late_max_ms,
    // well above the protocol ceiling — cap should be the full FT8 ceiling
    // (3000ms), not the raw headroom.
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(1.0),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 3000);
}

#[test]
fn adaptive_cap_uses_protocol_ceiling_for_ft4() {
    // Same early-arrival case, but FT4's cycle is half FT8's — the ceiling
    // should scale down proportionally (1500ms), not stay at FT8's 3000ms.
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(0.5),
        TxSelfParity::Auto,
        8000,
        12_000,
        FT4_SLOT_NS,
        pancetta_ft8::Protocol::Ft4,
    );
    assert_eq!(cap, 1500);
}

#[test]
fn adaptive_cap_full_ceiling_when_already_deferred() {
    // A head that's already past tx_late_max_ms for the current slot
    // (deferred to the next one) has no current-slot cliff to protect —
    // cap is the full protocol ceiling.
    let head = tx_request(Some(SlotParity::Odd));
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(29.0), // >8000ms into slot 1 (Odd), forces defer
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 3000);
}

#[test]
fn adaptive_cap_zero_for_non_transmit_request() {
    let head = MessageType::MultiTransmitRequest {
        items: Vec::new(),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
    };
    let cap = adaptive_coalesce_cap_ms(
        &head,
        at(1.0),
        TxSelfParity::Auto,
        8000,
        12_000,
        SLOT_NS,
        pancetta_ft8::Protocol::Ft8,
    );
    assert_eq!(cap, 0);
}
```

- [ ] **Step 3: Run the new tests to verify correctness of the pure cap logic**

Run: `cargo test -p pancetta --lib coordinator::tx::schedule_tx_tests -- --nocapture`
Expected: all 5 new tests PASS (`adaptive_cap_shrinks_for_a_late_arriving_head`,
`adaptive_cap_has_room_for_an_early_arriving_head`, `adaptive_cap_uses_protocol_ceiling_for_ft4`,
`adaptive_cap_full_ceiling_when_already_deferred`, `adaptive_cap_zero_for_non_transmit_request`).

- [ ] **Step 3b: Add a dedicated cancellation test for `interruptible_sleep`**

`interruptible_sleep` (tx.rs:185) is already used at every other sleep point in this worker
(Step 4, 6, 8, 9) but has no dedicated unit test today — each use is only exercised indirectly via
integration tests. Since this task makes the collection-window sleep interruptible for the first
time, and the window can now run several seconds instead of a flat 800ms, add direct coverage for
the cancellation path itself (not the worker-loop integration around it, which stays covered by
`loopback_qso`/`coord_sim` per Task 2 Steps 6-7 below). Add to a new `#[cfg(test)] mod
interruptible_sleep_tests` near the function:

```rust
#[cfg(test)]
mod interruptible_sleep_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[tokio::test]
    async fn returns_false_and_waits_full_duration_when_not_aborted() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_millis(100), &shutdown, &abort).await;
        assert!(!aborted);
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn returns_true_promptly_when_abort_flips_mid_sleep() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            abort_clone.store(true, Ordering::Release);
        });
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
        assert!(aborted);
        // Should wake within the ~50ms poll granularity of when the flag flipped
        // (flag flips at ~20ms), not wait out the full 5s sleep.
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn returns_true_promptly_when_shutdown_flips_mid_sleep() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shutdown_clone.store(true, Ordering::Release);
        });
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
        assert!(aborted);
        assert!(start.elapsed() < Duration::from_millis(200));
    }
}
```

Run: `cargo test -p pancetta --lib coordinator::tx::interruptible_sleep_tests -- --nocapture`
Expected: all 3 tests PASS.

- [ ] **Step 4: Replace the fixed sleep with the adaptive loop**

Find this block (search for the comment text — line number has shifted from Task 1's edit):

```rust
                            if matches!(message.message_type, MessageType::TransmitRequest { .. }) {
                                // Brief collection window so same-parity openings
                                // started in quick succession (serial manual
                                // keypresses, each crossing async hops) all arrive
                                // before we coalesce — otherwise the first opening
                                // commits the slot alone and siblings trickle in
                                // one-per-cycle (the "slow-start" bug). Absorbed by
                                // the Step-6 slot-wait, so no real added latency.
                                // See COALESCE_COLLECT_WINDOW_MS /
                                // coalesce_collect_window_ms (FT4/FT2-scaled).
                                tokio::time::sleep(Duration::from_millis(
                                    coalesce_collect_window_ms(active_protocol),
                                ))
                                .await;
                                message.message_type = coalesce_backlog_into(
                                    message.message_type,
                                    &tx_rx,
                                    &message_bus,
                                    &active_tx_qsos,
                                )
                                .await;
```

Replace with:

```rust
                            if matches!(message.message_type, MessageType::TransmitRequest { .. }) {
                                // Adaptive collection window (Symptom C fix): take
                                // the base wait once, unconditionally (same as
                                // before — this is the byte-identical baseline for
                                // the common lone-request case), then extend in
                                // further base-length increments ONLY while the
                                // channel's queued-message count keeps growing,
                                // capped by remaining tx_late_max_ms headroom and a
                                // protocol-scaled ceiling. Never modifies
                                // coalesce_backlog_into/coalesce_transmit_requests
                                // — this only decides how long to wait before that
                                // existing, unmodified drain runs once. See
                                // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §1.
                                let base_wait_ms = coalesce_collect_window_ms(active_protocol);
                                let queue_len_before_base = tx_rx.len();
                                if interruptible_sleep(
                                    Duration::from_millis(base_wait_ms),
                                    &shutdown,
                                    &abort_current_tx,
                                )
                                .await
                                {
                                    if shutdown.load(Ordering::Acquire) {
                                        info!("TX aborted during collection window by shutdown");
                                        break;
                                    }
                                    info!("TX aborted during collection window by operator (F8)");
                                    send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                    continue;
                                }

                                let extension_cap_ms = adaptive_coalesce_cap_ms(
                                    &message.message_type,
                                    request_received_at,
                                    tx_self_parity,
                                    tx_late_max_ms,
                                    sample_rate,
                                    active_slot_ns.load(Ordering::Relaxed),
                                    active_protocol,
                                );
                                let mut extended_ms: u64 = 0;
                                let mut prev_len = tx_rx.len();
                                // `aborted` is set instead of `continue`/`break`-ing
                                // directly inside the loop: an unlabeled `continue`
                                // here would target this `while`, not the outer
                                // worker loop, which would wrongly resume
                                // extending after an operator abort instead of
                                // abandoning this TX attempt. Checking the flag
                                // once after the loop, exactly as done for every
                                // other single-sleep abort site in this worker,
                                // avoids that.
                                let mut aborted = false;
                                while prev_len > queue_len_before_base && extended_ms < extension_cap_ms
                                {
                                    let this_wait = base_wait_ms.min(extension_cap_ms - extended_ms);
                                    if interruptible_sleep(
                                        Duration::from_millis(this_wait),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        aborted = true;
                                        break;
                                    }
                                    extended_ms += this_wait;
                                    let new_len = tx_rx.len();
                                    if new_len <= prev_len {
                                        // Nothing new arrived this increment — stop
                                        // extending, nothing left to wait for.
                                        break;
                                    }
                                    prev_len = new_len;
                                }
                                if aborted {
                                    if shutdown.load(Ordering::Acquire) {
                                        info!(
                                            "TX aborted during collection window extension by shutdown"
                                        );
                                        break;
                                    }
                                    info!(
                                        "TX aborted during collection window extension by operator (F8)"
                                    );
                                    send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                    continue;
                                }

                                message.message_type = coalesce_backlog_into(
                                    message.message_type,
                                    &tx_rx,
                                    &message_bus,
                                    &active_tx_qsos,
                                )
                                .await;
```

- [ ] **Step 5: Build and confirm compilation**

Run: `cargo build -p pancetta --features transmit`
Expected: builds cleanly, no warnings about unused variables or unreachable code.

- [ ] **Step 6: Run the full existing TX test suite to check for regressions**

Run: `cargo test -p pancetta --lib coordinator::tx:: -- --nocapture`
Expected: all existing tests PASS, including `schedule_tx_tests` and any `coalesce_transmit_requests`
tests already in the file (this task must not change their behavior — it never touches those
functions).

- [ ] **Step 7: Run the workspace loopback integration test**

Run: `cargo test -p pancetta --test loopback_qso`
Expected: PASS. This exercises a real encode→modulate→decode QSO cycle through the coordinator; a
regression in the coalesce path would likely surface here as a missed or malformed TX.

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "fix(tx): adaptive TX-coalesce window (Symptom C)

Replace the fixed 800ms pre-coalesce sleep with one that extends in
further base-length increments only while the queue keeps growing,
capped by remaining tx_late_max_ms headroom and a protocol-scaled
ceiling. Never touches coalesce_backlog_into/coalesce_transmit_requests
— only how long the worker waits before that existing drain runs once.
Lone-request case is byte-identical to today (one base wait, queue never
grows, loop doesn't execute).

Docs: docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §1"
```

---

## Task 3: Timing-accuracy fix — single-TX (`TransmitRequest`) arm

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` — the `TransmitRequest` match arm, specifically the
  `let schedule = schedule_tx(...)` declaration (Step 2, currently ~line 1721 pre-Task-1/2 edits;
  locate by the `--- Step 2: Resolve required parity ---` comment within the `TransmitRequest` arm
  — it's the FIRST occurrence of that comment in the file) and everything from `--- Step 3: Build
  the audio buffer to ship ---` through `--- Step 4b-arm:` (locate by comment text; exact lines
  will have shifted from Tasks 1-2).
- Test: `pancetta/src/coordinator/tx.rs` (covered by existing `loopback_qso` integration test — this
  task's change is deep inside a single giant async fn and not independently unit-testable without
  extracting more; rely on the loopback test plus manual trace-log inspection per Step 5 below).

**Interfaces:**
- Consumes: `pad_and_cursor_for_target(now, target_slot, sample_rate) -> (usize, usize)` (Task 1).
- No new public interfaces — this task only changes control flow within the existing
  `TransmitRequest` arm.

Moves the audio-buffer construction (today's Step 3) from immediately after Step 2 to immediately
before Step 4c (the late-pivot check), using a **freshly-read clock** against the **already-decided**
`schedule.target_slot` (never re-deriving `target_slot`/`deferred` — see Global Constraints). Step
4c's existing pivot-rebuild logic is left completely unchanged: it already reuses
`schedule.silent_pad_samples`/`schedule.cursor_offset_samples`, so mutating those two fields in
place (via a `mut schedule` binding) is enough to make Step 4c automatically pick up the refreshed
values with no edits to Step 4c itself.

- [ ] **Step 1: Make `schedule` mutable in the `TransmitRequest` arm**

Find (Step 2 of the `TransmitRequest` arm — the FIRST occurrence in the file):

```rust
                                    let schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                        slot_ns,
                                    );
```

Change `let schedule` to `let mut schedule`:

```rust
                                    let mut schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                        slot_ns,
                                    );
```

- [ ] **Step 2: Remove Step 3's original position**

Find, immediately after the `if schedule.deferred { ... }` block that follows Step 2 (still within
the `TransmitRequest` arm):

```rust
                                    // --- Step 3: Build the audio buffer to ship ---
                                    // Pad zeros in front (early branch); skip cursor into
                                    // waveform (late branch); never both at the same time.
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        // Defensive: if cursor outran the waveform (shouldn't
                                        // happen because too-late defers), emit nothing and
                                        // skip TX.
                                        warn!("schedule_tx cursor exceeded waveform length; skipping TX");
                                        emit_tx_failure_diagnostic(
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            "internal scheduling error (cursor exceeded waveform)",
                                        )
                                        .await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;
```

Delete this entire block from its current position (you'll re-add an equivalent block at the new
position in Step 3 below).

- [ ] **Step 3: Insert the buffer-build at the new position, using a fresh clock read**

Find `--- Step 4b-arm: re-check the remote-TX arm` within the `TransmitRequest` arm and its
closing `}` (the block ends right before the `// --- Step 4c: late pivot to the freshest message ---`
comment). Immediately after that closing `}` and before the `// --- Step 4c` comment, insert:

```rust
                                    // --- Step 3 (moved): build the audio buffer,
                                    // refreshed against real time ---
                                    // request_received_at (Step 2) already decided WHICH
                                    // slot to target and whether to defer — that decision
                                    // is never re-made here (Symptom-B's protection, see
                                    // Global Constraints in the implementation plan). But
                                    // real time has moved on since Step 2 (through the
                                    // Symptom-C adaptive coalesce window, encoding, and the
                                    // gates above), and Step 6 below is a no-op for the
                                    // common current-slot case (target_slot is already in
                                    // the past), so audio actually ships at whatever "now"
                                    // is by the time we reach Step 7 — not at the "now"
                                    // schedule_tx originally saw. Refresh just the pad/cursor
                                    // math against the SAME schedule.target_slot so the
                                    // transmitted waveform stays correctly aligned to the
                                    // real FT8 slot grid regardless of how long the steps
                                    // above took. See
                                    // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2.
                                    let (fresh_pad_samples, fresh_cursor_samples) =
                                        pad_and_cursor_for_target(
                                            chrono::Utc::now(),
                                            schedule.target_slot,
                                            sample_rate,
                                        );
                                    schedule.silent_pad_samples = fresh_pad_samples;
                                    schedule.cursor_offset_samples = fresh_cursor_samples;

                                    // Pad zeros in front (early branch); skip cursor into
                                    // waveform (late branch); never both at the same time.
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        // Defensive: if cursor outran the waveform (shouldn't
                                        // happen because too-late defers), emit nothing and
                                        // skip TX.
                                        warn!("schedule_tx cursor exceeded waveform length at key-time; skipping TX");
                                        emit_tx_failure_diagnostic(
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            "internal scheduling error (cursor exceeded waveform at key-time)",
                                        )
                                        .await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;
```

Step 4c immediately below this (unchanged, still reads `schedule.cursor_offset_samples`/
`schedule.silent_pad_samples` for its own pivot rebuild) now automatically uses the refreshed
values — do not edit Step 4c itself.

- [ ] **Step 4: Build**

Run: `cargo build -p pancetta --features transmit`
Expected: builds cleanly. If `samples` is reported as moved/borrowed incorrectly, check that Step 1
(encode) still produces `samples` before this new insertion point uses it — the move of Step 3
must land AFTER Step 1's `samples` binding and AFTER Step 4b/4b-arm, not before.

- [ ] **Step 5: Verify via trace logs that Step 4c's pivot path still works**

Run: `cargo test -p pancetta --test loopback_qso -- --nocapture 2>&1 | grep -i "tx.pivot\|TX pivot"`
Expected: if the loopback test exercises a pivot scenario, `pancetta::tx.pivot` log lines appear
with sensible pad/cursor-driven behavior (no panics, no "cursor exceeded waveform" warnings). If the
loopback test doesn't naturally trigger a pivot, this step confirms no such warnings appear at all
(clean run) — the pivot-specific coverage is Task 3's known gap, flagged rather than silently
assumed; if the team wants dedicated pivot-timing test coverage, that's a fast-follow, not blocking
this plan (the pivot code path itself is unchanged, only its two input values are now fresher).

- [ ] **Step 6: Run the full loopback + coord_sim suites**

Run: `cargo test -p pancetta --test loopback_qso && cargo test -p pancetta --test coord_sim`
Expected: both PASS.

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "fix(tx): refresh audio pad/cursor against real time before PTT (single-TX arm)

Move Step 3's audio-buffer build from right after Step 2 to right before
Step 4c, using a freshly-read clock against the already-decided
schedule.target_slot (never re-deriving target_slot/deferred). Step 4c's
existing pivot logic is unchanged — it already reuses schedule's
pad/cursor fields, so mutating them in place is enough.

Docs: docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2"
```

---

## Task 4: Timing-accuracy fix — multi-TX (`MultiTransmitRequest`) arm

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` — the `MultiTransmitRequest` match arm: the `let
  schedule = schedule_tx(...)` declaration (Step 2, SECOND occurrence of that comment in the file),
  the original Step 3 block, and the Step 4b/4b-pivot tuple-resolution block (locate by `--- Step 3:
  Build the audio buffer ---` and `--- Step 4b-pivot: bundle-arm late pivot ---` comments within
  this arm; exact lines will have shifted from Tasks 1-3).
- Test: covered by `loopback_qso`/`coord_sim` integration tests (same rationale as Task 3 — deep
  inside a giant async fn, not independently unit-testable without a larger extraction this plan
  deliberately doesn't do, to keep the change minimal in already-incident-adjacent code).

**Interfaces:**
- Consumes: `pad_and_cursor_for_target(now, target_slot, sample_rate) -> (usize, usize)` (Task 1).
- No new public interfaces.

**This is the highest-risk task in the plan** — it touches the exact conditional rebuild logic from
the SM2LIY/C6AVD double-73 live-incident fix (PR #167). The approach: change BOTH branches of the
existing fast-path/rebuild `if`/`else` to stop trimming with the frozen `schedule` and instead
return the **untrimmed** sample buffer; then, in ONE place immediately before Step 5 (mirroring
Task 3's placement), refresh pad/cursor against real time and apply the trim once, uniformly,
regardless of which branch produced the untrimmed buffer. The fast-path/rebuild branching logic
itself (which items are stale, which got pivoted, the re-encode call) is **not touched** — only
where the final trim happens moves.

- [ ] **Step 1: Make `schedule` mutable in the `MultiTransmitRequest` arm**

Find (Step 2 of the `MultiTransmitRequest` arm — the SECOND occurrence of `--- Step 2: Resolve
required parity ---` in the file):

```rust
                                    let schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                        slot_ns,
                                    );
```

Change to `let mut schedule = schedule_tx(...)` (same edit as Task 3 Step 1, applied to this arm's
occurrence).

- [ ] **Step 2: Remove Step 3's original position in this arm**

Find, within the `MultiTransmitRequest` arm:

```rust
                                    // --- Step 3: Build the audio buffer ---
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform; skipping");
                                        for (text, qso_id) in
                                            item_texts.iter().zip(encoded_qso_ids.iter())
                                        {
                                            emit_tx_failure_diagnostic(
                                                &message_bus,
                                                qso_id.as_deref(),
                                                text,
                                                "internal scheduling error (cursor exceeded multi-TX waveform)",
                                            )
                                            .await;
                                        }
                                        for text in item_texts {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: text,
                                                    duration_ms: 0,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;
```

Replace it with a version that does NOT trim — just carries `samples` (the raw Step-1 waveform)
forward under a new name for clarity at the later trim point:

```rust
                                    // --- Step 3 (deferred): the audio buffer is now
                                    // built once, fresh, immediately before Step 5 —
                                    // see the new block right before "--- Step 5:
                                    // Assert PTT ---" below. `samples` (Step 1's raw,
                                    // untrimmed multi-tone waveform) is carried forward
                                    // as `raw_samples` for the fast path in the
                                    // Step 4b/4b-pivot resolution below; the "cursor
                                    // exceeded waveform" defensive check now happens
                                    // once, at the final trim point, against whichever
                                    // buffer (fast-path or rebuilt) is actually about
                                    // to be sent.
                                    let raw_samples = samples;
```

- [ ] **Step 3: Change the Step 4b/4b-pivot tuple resolution to stop trimming**

Find the tuple-resolution block (within `--- Step 4b-pivot: bundle-arm late pivot ---`):

```rust
                                    let (
                                        items,
                                        audio_out,
                                        item_texts,
                                        _encoded_qso_ids,
                                        audio_duration_ms,
                                    ) = if live_mask.iter().all(|&live| live) && pivots.is_empty() {
                                        // Fast path: nothing went stale.
                                        (
                                            items,
                                            audio_out,
                                            item_texts,
                                            encoded_qso_ids,
                                            audio_duration_ms,
                                        )
                                    } else {
```

Change the tuple's shape to carry `raw_samples: Vec<f32>` instead of `audio_out: Vec<f32>` /
`audio_duration_ms: u64`, and drop `raw_samples`'s trim in the fast-path arm:

```rust
                                    let (items, raw_samples, item_texts, _encoded_qso_ids) =
                                        if live_mask.iter().all(|&live| live) && pivots.is_empty() {
                                            // Fast path: nothing went stale, nothing
                                            // pivoted — carry the ORIGINAL Step-1
                                            // waveform forward untrimmed; the final
                                            // trim happens once, fresh, right before
                                            // Step 5 below.
                                            (items, raw_samples, item_texts, encoded_qso_ids)
                                        } else {
```

Now find, further down in the `else` (rebuild) branch, the part that currently trims
`new_samples` into `new_audio_out`:

```rust
                                        if schedule.cursor_offset_samples >= new_samples.len() {
                                            warn!("schedule_tx cursor exceeded rebuilt multi-TX waveform at key-time; dropping");
                                            for (text, qso_id) in
                                                rebuilt_texts.iter().zip(rebuilt_qso_ids.iter())
                                            {
                                                emit_tx_failure_diagnostic(
                                                    &message_bus,
                                                    qso_id.as_deref(),
                                                    text,
                                                    "internal scheduling error (cursor exceeded rebuilt multi-TX waveform)",
                                                )
                                                .await;
                                            }
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for text in rebuilt_texts {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: text,
                                                        duration_ms: 0,
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
                                            }
                                            continue;
                                        }

                                        let mut new_audio_out = Vec::with_capacity(
                                            schedule.silent_pad_samples + new_samples.len(),
                                        );
                                        new_audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                        new_audio_out.extend_from_slice(
                                            &new_samples[schedule.cursor_offset_samples..],
                                        );
                                        let new_audio_duration_ms = (new_audio_out.len() as f64
                                            / sample_rate as f64
                                            * 1000.0)
                                            as u64;

                                        info!(
                                            target: "pancetta::tx.policy",
                                            "multi-TX bundle re-encoded at key-time: {} of {} item(s) still live",
                                            rebuilt_texts.len(),
                                            items.len()
                                        );

                                        (
                                            live_items,
                                            new_audio_out,
                                            rebuilt_texts,
                                            rebuilt_qso_ids,
                                            new_audio_duration_ms,
                                        )
                                    };
```

Replace the "cursor exceeded" defensive check (now premature — it checked the FROZEN schedule's
cursor, but the final trim hasn't happened yet) and the trim itself with a pass-through of
`new_samples` untrimmed:

```rust
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "multi-TX bundle re-encoded at key-time: {} of {} item(s) still live",
                                            rebuilt_texts.len(),
                                            items.len()
                                        );

                                        (live_items, new_samples, rebuilt_texts, rebuilt_qso_ids)
                                    };
```

(The "cursor exceeded rebuilt multi-TX waveform" check and its accompanying diagnostic/complete-msg
cleanup loop are DELETED from here — an equivalent check runs once, at the final trim point in Step
4, against whichever buffer this whole block produced.)

- [ ] **Step 4: Insert the single, final trim immediately before Step 5**

Find `--- Step 5: Assert PTT ---` within the `MultiTransmitRequest` arm. Immediately before it
(after the `--- Step 4b-arm: re-check the remote-TX arm` block and the `for (qso_key, new_text) in
pivots { pivoted_once.insert(...); }` loop that precedes Step 5), insert:

```rust
                                    // --- Step 3 (final): build the audio buffer,
                                    // refreshed against real time ---
                                    // Mirrors the single-TX arm's equivalent block
                                    // (tx.rs Task 3). request_received_at (Step 2)
                                    // already decided WHICH slot to target and
                                    // whether to defer — never re-derived here.
                                    // raw_samples is whatever the fast-path/rebuild
                                    // resolution above produced (untrimmed); trim it
                                    // ONCE here, against a freshly-read clock and the
                                    // already-decided schedule.target_slot, so the
                                    // transmitted waveform stays aligned to the real
                                    // FT8 slot grid regardless of how long the steps
                                    // above (adaptive coalesce window, encoding,
                                    // possible key-time re-encode) took. See
                                    // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2.
                                    let (fresh_pad_samples, fresh_cursor_samples) =
                                        pad_and_cursor_for_target(
                                            chrono::Utc::now(),
                                            schedule.target_slot,
                                            sample_rate,
                                        );
                                    schedule.silent_pad_samples = fresh_pad_samples;
                                    schedule.cursor_offset_samples = fresh_cursor_samples;

                                    if schedule.cursor_offset_samples >= raw_samples.len() {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform at key-time; dropping");
                                        for (text, qso_id) in
                                            item_texts.iter().zip(_encoded_qso_ids.iter())
                                        {
                                            emit_tx_failure_diagnostic(
                                                &message_bus,
                                                qso_id.as_deref(),
                                                text,
                                                "internal scheduling error (cursor exceeded multi-TX waveform at key-time)",
                                            )
                                            .await;
                                        }
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for text in item_texts {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: text,
                                                    duration_ms: 0,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }

                                    let mut audio_out = Vec::with_capacity(
                                        schedule.silent_pad_samples + raw_samples.len()
                                            - schedule.cursor_offset_samples,
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    audio_out.extend_from_slice(&raw_samples[schedule.cursor_offset_samples..]);
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0) as u64;
```

- [ ] **Step 5: Check `_encoded_qso_ids` naming/usage carefully**

The original tuple bound `_encoded_qso_ids` (underscore-prefixed — the original code's own comment
notes "isn't consulted again after this point"). This plan's Step 4 above DOES consult it (in the
new defensive-check diagnostic loop). Rename the binding from `_encoded_qso_ids` to
`encoded_qso_ids_final` (drop the underscore prefix, since it's now used) at BOTH the fast-path and
rebuild-path tuple sites from Step 3 of this task, and update the reference in Step 4's new code
from `_encoded_qso_ids` to `encoded_qso_ids_final`. Confirm with `cargo build` (Step 6 below) — an
unused-variable warning on this binding means the rename wasn't applied consistently.

- [ ] **Step 6: Build**

Run: `cargo build -p pancetta --features transmit`
Expected: builds cleanly. Common issues to check if it doesn't:
- `samples` moved into `raw_samples` at the wrong point (must happen after Step 1's `samples`
  binding, which this task's Step 2 edit sits right after already).
- `new_samples` (from the rebuild branch's `encode_and_modulate_multi_tx` call) still in scope when
  Step 3 (this task)'s replacement code returns it — it should be, since the replacement just
  changes what the branch's tail expression returns, not where `new_samples` is bound.
- The `encoded_qso_ids_final` rename (Step 5) applied at every site.

- [ ] **Step 7: Run the full loopback + coord_sim suites**

Run: `cargo test -p pancetta --test loopback_qso && cargo test -p pancetta --test coord_sim`
Expected: both PASS. `coord_sim` in particular exercises multi-TX bundling scenarios — pay close
attention to any test with "multi" or "bundle" in its name.

- [ ] **Step 8: Run the full workspace test suite as a final regression check**

Run: `cargo test --workspace --features transmit`
Expected: PASS. This is the broadest net available for a change to this safety-critical,
incident-adjacent code path — run it in full before committing, not just the targeted subsets above.

- [ ] **Step 9: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "fix(tx): refresh audio pad/cursor against real time before PTT (multi-TX arm)

Mirrors the single-TX arm's fix (previous commit). Both the fast-path and
key-time-rebuild branches of the Step 4b/4b-pivot resolution now carry
their sample buffer forward UNTRIMMED; the trim happens once, fresh,
immediately before Step 5, against the already-decided
schedule.target_slot. The double-73-incident-era fast-path/rebuild
branching logic itself is unchanged — only where the final trim happens
moved.

Docs: docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2"
```

---

## Task 5: Documentation updates

**Files:**
- Modify: `docs/qso-state-machine-analysis.md` (Symptom C section)
- Modify: `docs/DECISIONS/tx-scheduling.md`

**Interfaces:** None — documentation only.

- [ ] **Step 1: Update the Symptom C section in `docs/qso-state-machine-analysis.md`**

Find the section starting `## Symptom C — "multi-TX starts single-only in the first window; multi
by the second" — CONFIRMED`. At the end of that section (before the `---` divider that follows),
add:

```markdown

**Resolved 2026-07-21** — see
`docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md` and
`docs/superpowers/plans/2026-07-21-symptom-c-adaptive-coalesce-window.md`. Implemented option
closest to (b) above: the fixed 800ms collection window now extends adaptively while the queue
keeps growing, capped by remaining `tx_late_max_ms` headroom and a protocol-scaled ceiling, instead
of a single fixed sleep. A folded-in timing-accuracy fix (refreshing the audio pad/cursor math
against real time immediately before PTT, rather than the frozen pre-coalesce timestamp) prevents
the wider window from growing an existing, previously-unaddressed audio-alignment drift.
```

Also update the file's top-of-file superseded-notice line that currently reads:

```markdown
> boxes below). Symptom C (multi-TX slow-start) is still open — see
```

Change `is still open` to `was resolved 2026-07-21` and update the reference:

```markdown
> boxes below). Symptom C (multi-TX slow-start) was resolved 2026-07-21 — see
> `project_symptom_c_multi_tx_deferred` in the assistant's memory (historical) or
> `docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md` for the fix.
```

- [ ] **Step 2: Append a dated entry to `docs/DECISIONS/tx-scheduling.md`**

The file's convention (confirmed by reading it) is dated `##`/`###` sections in landing order, with
one evergreen reference section (`## Coordinator-level QSO sim harness`) kept last. Insert the new
section AFTER `## QSO/TX deep-review remediation (2026-07-18) — 5 batches, all landed` and BEFORE
`## Coordinator-level QSO sim harness`:

```markdown
## Symptom C — adaptive coalesce window + timing-accuracy fix (2026-07-21)

`coordinator/tx.rs`: closes the residual gap left by the 2026-06-27 fix above (see "Multi-TX
slow-start fix" section) — the fixed `COALESCE_COLLECT_WINDOW_MS` (800ms) window still couldn't
batch serial manual keypresses realistically spaced 1-3s apart, so a pileup's siblings still
trickled in one-per-cycle via keep-call rearm instead of joining the first window. Confirmed still
open as of the 2026-07-18 deep review. Fix: the collection window now takes the same 800ms base
wait once, unconditionally (byte-identical to before for the common lone-request case), then
extends in further base-length increments only while the channel's queued-message count keeps
growing, capped by remaining `tx_late_max_ms` headroom (so a request already late in its slot
extends little or not at all) and a protocol-scaled ceiling (`COALESCE_MAX_EXTENSION_MS = 3000`ms
FT8-baseline, scaled like `coalesce_collect_window_ms`). Does not modify
`coalesce_backlog_into`/`coalesce_transmit_requests` — only how long the worker waits before that
existing drain runs once.

**Folded-in timing-accuracy fix:** tracing what happens downstream of `schedule_tx` found that its
`cursor_offset_samples`/`silent_pad_samples` (computed once, at Step 2, from the frozen
`request_received_at`) implicitly assumed audio ships immediately relative to that timestamp — but
for the common current-slot case, `target_slot` is already in the past, so the Step-6 slot-wait is a
no-op and audio actually ships whenever the worker reaches Step 7, i.e. `request_received_at` plus
however long the collection window, encoding, and gate checks actually took. That gap was small
(~800ms) and already-tolerated in production before this fix, but the wider adaptive window would
have grown it proportionally. Fix: `schedule_tx`'s pad/cursor math was split into a standalone
`pad_and_cursor_for_target(now, target_slot, sample_rate)` helper; both TX-worker arms now refresh
just the pad/cursor (never `target_slot`/`deferred` — that decision is made exactly once, per
Symptom B's existing protection) against a freshly-read clock immediately before PTT (Step 5),
using the already-decided `target_slot`. The single-TX arm's existing late-pivot logic (Step 4c)
needed no changes — it already reused `schedule`'s pad/cursor fields, so refreshing them in place
was sufficient. The multi-TX arm's fast-path/rebuild branching (from the double-73/SM2LIY-C6AVD
incident fix, PR #167) is unchanged in its own logic — only where the final audio trim happens
moved, from inside that branching to a single point right before Step 5.

Spec: `docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md`. Plan:
`docs/superpowers/plans/2026-07-21-symptom-c-adaptive-coalesce-window.md`.
```

- [ ] **Step 3: Commit**

```bash
git add docs/qso-state-machine-analysis.md docs/DECISIONS/tx-scheduling.md
git commit -m "docs: close out Symptom C (multi-TX slow-start) as resolved

Updates docs/qso-state-machine-analysis.md and docs/DECISIONS/tx-scheduling.md
to reflect the adaptive-coalesce-window fix landed in the preceding commits."
```
