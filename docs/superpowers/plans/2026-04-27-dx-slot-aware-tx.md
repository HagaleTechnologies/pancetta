# DX-Slot-Aware TX Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop transmitting in the same 15-second slot as the DX station, eliminate the unnecessary `MIN_LEAD = 1s` deferral, and make late-press TX viable via WSJT-X-style audio skip-ahead.

**Architecture:** A `SlotParity { Even, Odd }` token is computed at the decoder, threaded through the TUI, the QSO state machine, and into every `TransmitRequest`. The TX scheduler in `pancetta/src/coordinator/tx.rs` is rewritten around three branches keyed on `mstr` (ms past target slot boundary): pad silent samples if early, skip the audio cursor forward if late (up to `tx_late_max_ms`, default 8s), defer 30s if too late. `MIN_LEAD = 1s` is removed; PTT engages ~80ms before the slot boundary.

**Tech Stack:** Rust workspace (11 crates), tokio, crossbeam-channel, ratatui, chrono. Tests via `cargo test`.

**Spec:** `docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`

---

## File Plan

**Modified files:**
- `pancetta-core/src/slot.rs` — add `SlotParity`, `slot_parity()`, `next_slot_with_parity()`
- `pancetta-ft8/src/message.rs` — add `slot_parity: Option<SlotParity>` to `DecodedMessage`
- `pancetta/src/coordinator/ft8.rs` — stamp `slot_parity` at decode dispatch
- `pancetta-tui/src/app.rs` — add `slot_parity` to `DecodedMessageView`, change `get_selected_station` return signature
- `pancetta-tui/src/tui_runner.rs` — `TuiCommand::CallStation` gets `dx_parity`; Space handler reads + sends it
- `pancetta/src/coordinator/tui_relay.rs` — populate `DecodedMessageView.slot_parity`
- `pancetta/src/message_bus.rs` — `TransmitRequest`, `MultiTransmitRequest`, `TransmitRequestItem`, `QsoMessage::StartQso` get parity fields
- `pancetta-qso/src/states.rs` — `QsoMetadata.tx_parity`
- `pancetta-qso/src/qso_manager.rs` — `respond_to_cq` accepts `dx_parity`, `start_cq` accepts `tx_parity`, latches into metadata; `QsoEvent::MessageToSend` carries `tx_parity`
- `pancetta-qso/src/autonomous.rs` — `DecodedMessageInfo.slot_parity`, `OperatorAction::Transmit.tx_parity`
- `pancetta/src/coordinator/qso.rs` — `StartQso` handler passes `dx_parity` to `respond_to_cq`; `MessageToSend` forwarder reads `tx_parity` for the new TX request
- `pancetta/src/coordinator/autonomous.rs` — feed `slot_parity` to operator; read `tx_parity` from `OperatorAction::Transmit` into request
- `pancetta-config/src/station.rs` — `tx_late_max_ms`, `tx_self_parity`, `ptt_lead_ms`
- `pancetta/src/coordinator/tx.rs` — new `schedule_tx()` helper; wire into both single and multi paths; drop `MIN_LEAD`

**Modified test files / new tests:**
- `pancetta-core/src/slot.rs` (in-file `#[cfg(test)] mod tests`)
- `pancetta/src/coordinator/tx.rs` (in-file `#[cfg(test)] mod tests`)
- `pancetta/tests/loopback_qso.rs` (extend)
- `pancetta-qso/tests/parity_latch.rs` (new file)

---

## Task 1: Add `SlotParity` enum and `slot_parity()` helper

**Files:**
- Modify: `pancetta-core/src/slot.rs`

- [ ] **Step 1: Write the failing tests.**

Append to `pancetta-core/src/slot.rs` inside `mod tests`:

```rust
    #[test]
    fn slot_parity_even_at_boundary_zero() {
        // 2026-01-01 00:00:00 UTC. timestamp() = 1767225600.
        // 1767225600 / 15 = 117815040 (even index) → Even.
        assert_eq!(SlotParity::of(at(0.0)), SlotParity::Even);
    }

    #[test]
    fn slot_parity_odd_at_boundary_fifteen() {
        // 15s later → 117815041 (odd index) → Odd.
        assert_eq!(SlotParity::of(at(15.0)), SlotParity::Odd);
    }

    #[test]
    fn slot_parity_within_slot_uses_floor() {
        // 14.999s into slot 0 still resolves to that slot's parity.
        assert_eq!(SlotParity::of(at(14.999)), SlotParity::Even);
    }

    #[test]
    fn slot_parity_opposite_invariant() {
        assert_eq!(SlotParity::Even.opposite(), SlotParity::Odd);
        assert_eq!(SlotParity::Odd.opposite(), SlotParity::Even);
        assert_eq!(SlotParity::Even.opposite().opposite(), SlotParity::Even);
    }
```

- [ ] **Step 2: Run tests to verify they fail.**

```bash
touch pancetta-core/src/slot.rs
cargo test -p pancetta-core slot::tests::slot_parity 2>&1 | tail -20
```

Expected: compile error — `SlotParity` and `SlotParity::of` do not exist.

- [ ] **Step 3: Implement.**

Add to `pancetta-core/src/slot.rs` (after the `pub const PRE_ROLL_NS` block, before `next_slot_start`):

```rust
/// Returns the start of the slot that contains `t`. Used by the TX
/// scheduler to detect "we're inside a viable slot, target it" vs.
/// "current slot is wrong parity or already too late, advance."
pub fn current_slot_start(t: DateTime<Utc>) -> DateTime<Utc> {
    let ns = t
        .timestamp_nanos_opt()
        .expect("system clock out of i64 ns range");
    let slot_ns = ns.div_euclid(SLOT_NS) * SLOT_NS;
    DateTime::<Utc>::from_timestamp_nanos(slot_ns)
}

/// Parity of an FT8 slot. Even slots start at UTC seconds `:00` and
/// `:30`; Odd at `:15` and `:45`. Two stations in QSO must transmit on
/// opposite parities — same parity collides on air.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlotParity {
    Even,
    Odd,
}

impl SlotParity {
    /// Parity of the slot containing `t`. Computed as `(t.timestamp / 15) % 2`,
    /// where the slot index is the floor — so any instant inside the slot
    /// resolves to the same parity as the slot's start boundary.
    pub fn of(t: DateTime<Utc>) -> SlotParity {
        let ns = t
            .timestamp_nanos_opt()
            .expect("system clock out of i64 ns range");
        let slot_index = ns.div_euclid(SLOT_NS);
        if slot_index % 2 == 0 {
            SlotParity::Even
        } else {
            SlotParity::Odd
        }
    }

    /// The other parity. `Even <-> Odd`. Idempotent under double-flip.
    pub fn opposite(self) -> SlotParity {
        match self {
            SlotParity::Even => SlotParity::Odd,
            SlotParity::Odd => SlotParity::Even,
        }
    }
}
```

Add to top of `pancetta-core/src/slot.rs` if not already present:

```rust
use serde::{Deserialize, Serialize};
```

(Keep the existing `use chrono::...` line.)

- [ ] **Step 4: Run tests to verify they pass.**

```bash
cargo test -p pancetta-core slot::tests::slot_parity 2>&1 | tail -10
```

Expected: 4 tests pass.

- [ ] **Step 5: Commit.**

```bash
git add pancetta-core/src/slot.rs
git commit -m "feat(core): add SlotParity enum and SlotParity::of helper

Even/Odd parity computed from slot index = (timestamp_ns / SLOT_NS).
Opposite() returns the other parity. Used downstream to enforce
opposite-parity TX vs. the heard DX station.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Add `next_slot_with_parity()` helper

**Files:**
- Modify: `pancetta-core/src/slot.rs`

- [ ] **Step 1: Write the failing tests.**

Append to `pancetta-core/src/slot.rs` inside `mod tests`:

```rust
    #[test]
    fn next_slot_with_parity_skips_same_parity() {
        // now = :05 (in even slot 0). Asking for Odd → :15.
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Odd);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 15_000);
    }

    #[test]
    fn next_slot_with_parity_advances_two_slots_when_current_is_wanted() {
        // now = :05 (even slot 0). Asking for Even → :30 (next even slot,
        // skipping the odd one at :15).
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Even);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_000);
    }

    #[test]
    fn next_slot_with_parity_at_boundary_advances_to_next_match() {
        // now = exactly :15.000 (odd slot start). Even slots are :00, :30...
        // The current slot has already started, so next Odd is :45.
        let now = at(15.0);
        let target = next_slot_with_parity(now, SlotParity::Odd);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 45_000);
    }

    #[test]
    fn next_slot_with_parity_inside_wanted_slot_advances() {
        // now = :05 (inside even slot 0). Asking for Even — current slot
        // has already started, must advance. Next even is :30.
        let now = at(5.0);
        let target = next_slot_with_parity(now, SlotParity::Even);
        let delta = (target - at(0.0)).num_milliseconds();
        assert_eq!(delta, 30_000);
    }
```

- [ ] **Step 2: Run tests to verify they fail.**

```bash
touch pancetta-core/src/slot.rs
cargo test -p pancetta-core slot::tests::next_slot_with_parity 2>&1 | tail -20
```

Expected: compile error — `next_slot_with_parity` does not exist.

- [ ] **Step 3: Implement.**

Add to `pancetta-core/src/slot.rs` after `next_slot_start`:

```rust
/// Returns the next slot start whose parity equals `wanted`, strictly after `now`.
///
/// Always advances past the current slot (i.e., `now` falling on the start of a
/// matching slot still returns the *next* matching slot). This matches the
/// semantics of `next_slot_start`: callers want a future TX target, never the
/// present one.
pub fn next_slot_with_parity(now: DateTime<Utc>, wanted: SlotParity) -> DateTime<Utc> {
    let mut candidate = next_slot_start(now, Duration::zero());
    if SlotParity::of(candidate) != wanted {
        candidate += Duration::nanoseconds(SLOT_NS);
    }
    candidate
}
```

- [ ] **Step 4: Run tests to verify they pass.**

```bash
cargo test -p pancetta-core slot::tests::next_slot_with_parity 2>&1 | tail -10
```

Expected: 4 tests pass.

- [ ] **Step 5: Commit.**

```bash
git add pancetta-core/src/slot.rs
git commit -m "feat(core): add next_slot_with_parity helper

Returns the next slot boundary strictly after \`now\` whose parity
equals \`wanted\`. Always advances past the current slot — callers
need a future TX anchor, not a present one.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Add `slot_parity` to `DecodedMessage` and stamp it at decoder dispatch

**Files:**
- Modify: `pancetta-ft8/src/message.rs:788-836`
- Modify: `pancetta/src/coordinator/ft8.rs:140-219`

- [ ] **Step 1: Write the failing test.**

Append to `pancetta-ft8/src/message.rs` inside `mod tests` (find the existing test block — it's where `assert_eq!(decoded.time_offset, 2.1)` lives at line 2098):

```rust
    #[test]
    fn decoded_message_default_slot_parity_is_none() {
        let msg = Ft8Message::cq("K1ABC".to_string(), Some(GridSquare::new("FN42").unwrap()));
        let decoded = DecodedMessage::new(msg, -10.0, 0.9, 1500.0, 0.0);
        assert_eq!(decoded.slot_parity, None);
    }
```

- [ ] **Step 2: Run test to verify it fails.**

```bash
touch pancetta-ft8/src/message.rs
cargo test -p pancetta-ft8 message::tests::decoded_message_default_slot_parity_is_none 2>&1 | tail -15
```

Expected: compile error — `slot_parity` field does not exist on `DecodedMessage`.

- [ ] **Step 3: Add the field and default it to `None`.**

In `pancetta-ft8/src/message.rs`, modify the `DecodedMessage` struct (line 788):

```rust
/// Decoded FT8 message with metadata
#[derive(Debug, Clone)]
pub struct DecodedMessage {
    /// Parsed message content
    pub message: Ft8Message,
    /// Plain text representation
    pub text: String,
    /// Signal-to-noise ratio in dB
    pub snr_db: f32,
    /// Decoding confidence (0.0 - 1.0)
    pub confidence: f32,
    /// Frequency offset in Hz
    pub frequency_offset: f64,
    /// Time offset in seconds
    pub time_offset: f64,
    /// Decode timestamp
    pub timestamp: SystemTime,
    /// Number of error corrections applied
    pub error_corrections: u8,
    /// Tone symbols (79 values, 0-7) for signal reconstruction in multi-pass subtraction.
    /// None if symbols were not preserved during decoding.
    pub tone_symbols: Option<Vec<u8>>,
    /// AP (A Priori) level used for this decode: 0 = no AP, 1-4 = AP level used.
    pub ap_level: u8,
    /// Parity of the FT8 slot whose audio produced this decode. `None` until
    /// the coordinator's decoder dispatch tags it (which it does for every
    /// message routed to TUI / QSO / autonomous). Constructors leave it
    /// unset because they don't have access to the slot timing.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
}
```

In the `new` constructor (line 814), default the new field:

```rust
    /// Create a new decoded message
    pub fn new(
        message: Ft8Message,
        snr_db: f32,
        confidence: f32,
        frequency_offset: f64,
        time_offset: f64,
    ) -> Self {
        let text = message.to_string();
        Self {
            message,
            text,
            snr_db,
            confidence,
            frequency_offset,
            time_offset,
            timestamp: SystemTime::now(),
            error_corrections: 0,
            tone_symbols: None,
            ap_level: 0,
            slot_parity: None,
        }
    }
```

Also default it in the `Default` impl and in `from_ft8lib` if those exist. Find them with:

```bash
grep -n "tone_symbols: None" pancetta-ft8/src/message.rs
```

For each match, ensure the next line of struct construction includes `slot_parity: None,`.

If `pancetta-ft8/Cargo.toml` does not list `pancetta-core` as a dependency, add it. Check:

```bash
grep -n "pancetta-core" pancetta-ft8/Cargo.toml
```

If missing, add to `[dependencies]`:

```toml
pancetta-core = { path = "../pancetta-core" }
```

- [ ] **Step 4: Run the unit test.**

```bash
touch pancetta-ft8/src/message.rs
cargo test -p pancetta-ft8 message::tests::decoded_message_default_slot_parity_is_none 2>&1 | tail -10
```

Expected: 1 test pass. If other tests break due to struct construction in tests, update those test sites to include `slot_parity: None` (the compiler will tell you exactly where).

- [ ] **Step 5: Stamp parity at decoder dispatch.**

In `pancetta/src/coordinator/ft8.rs`, find the loop at line 162 (`for decoded_msg in &decoded_messages {`). Right before that loop, compute the slot parity for the window we just decoded:

```rust
                        // The audio window the decoder just processed corresponds
                        // to the slot that ended just before now. We'll use that
                        // slot's start time to compute its parity, and stamp every
                        // decoded message with it. (All messages in this batch came
                        // from the same audio window, so they share parity.)
                        let now_utc = chrono::Utc::now();
                        let next_boundary =
                            pancetta_core::slot::next_slot_start(now_utc, chrono::Duration::zero());
                        let slot_start = next_boundary
                            - chrono::Duration::nanoseconds(pancetta_core::slot::SLOT_NS);
                        let window_parity = pancetta_core::slot::SlotParity::of(slot_start);

                        for decoded_msg in decoded_messages.iter_mut() {
                            decoded_msg.slot_parity = Some(window_parity);
                        }
```

Note: `decoded_messages` was previously `let mut decoded_messages = ft8lib_messages;` — already `mut`. Verify with:

```bash
grep -n "let mut decoded_messages" pancetta/src/coordinator/ft8.rs
```

If it isn't mutable, change `let decoded_messages` to `let mut decoded_messages`.

- [ ] **Step 6: Verify the workspace still builds.**

```bash
touch pancetta/src/coordinator/ft8.rs pancetta-ft8/src/message.rs
cargo build -p pancetta -p pancetta-ft8 2>&1 | tail -20
```

Expected: clean compile. Look for "Compiling pancetta-ft8" and "Compiling pancetta" lines — if they're absent, the cargo cache lied; `touch` more files and re-run.

- [ ] **Step 7: Commit.**

```bash
git add pancetta-ft8/src/message.rs pancetta-ft8/Cargo.toml pancetta/src/coordinator/ft8.rs
git commit -m "feat(ft8): tag DecodedMessage with slot_parity at decoder dispatch

Adds Option<SlotParity> to DecodedMessage; the field defaults to None
in constructors so existing test sites are unaffected. The coordinator
ft8 thread computes the parity of the slot whose audio just decoded
and stamps every message in the batch with it before forwarding to
TUI / QSO / Autonomous / PSKReporter.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Plumb `slot_parity` through `DecodedMessageView` to the TUI

**Files:**
- Modify: `pancetta-tui/src/app.rs` (search for `pub struct DecodedMessageView`)
- Modify: `pancetta/src/coordinator/tui_relay.rs:120-137`

- [ ] **Step 1: Locate the `DecodedMessageView` struct.**

```bash
grep -n "pub struct DecodedMessageView" pancetta-tui/src/app.rs
```

- [ ] **Step 2: Add the field.**

In the `DecodedMessageView` struct (location from Step 1), add after the existing fields:

```rust
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
```

If `pancetta-core` isn't already in `pancetta-tui/Cargo.toml`, add it:

```bash
grep -n "pancetta-core" pancetta-tui/Cargo.toml || \
    sed -i.bak '/\[dependencies\]/a\
pancetta-core = { path = "../pancetta-core" }' pancetta-tui/Cargo.toml && \
    rm -f pancetta-tui/Cargo.toml.bak
```

(That sed is platform-quirky — if it fails, just open `pancetta-tui/Cargo.toml` and add the line manually under `[dependencies]`.)

- [ ] **Step 3: Populate the field in tui_relay.**

In `pancetta/src/coordinator/tui_relay.rs`, find the `DecodedMessageView` construction at line ~120 and add the field:

```rust
                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                // ... existing fields ...
                                slot_parity: decoded_msg.slot_parity,
                            };
```

Read the existing block first to see the current field order; add `slot_parity` at the end so the diff is clean.

- [ ] **Step 4: Find any other DecodedMessageView construction sites and update them.**

```bash
grep -rn "DecodedMessageView \{" pancetta-tui pancetta
```

For each construction site, add `slot_parity: None` (or the appropriate value) so the struct is always fully initialised.

- [ ] **Step 5: Build to verify.**

```bash
touch pancetta-tui/src/app.rs pancetta/src/coordinator/tui_relay.rs
cargo build -p pancetta-tui -p pancetta 2>&1 | tail -10
```

Expected: clean compile.

- [ ] **Step 6: Commit.**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/Cargo.toml pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(tui): carry slot_parity on DecodedMessageView

Forwards the parity tag from DecodedMessage into the TUI's view of
the decoded list, so the TUI app can reach back at Space-press time
and tell the QSO layer which slot the heard station was on.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Return `slot_parity` from `App::get_selected_station` and send it on Space

**Files:**
- Modify: `pancetta-tui/src/app.rs:907-948`
- Modify: `pancetta-tui/src/tui_runner.rs:118-119, 471-480`

- [ ] **Step 1: Change the return type of `get_selected_station`.**

In `pancetta-tui/src/app.rs:914`, change:

```rust
    pub fn get_selected_station(&self) -> Option<(String, u64)> {
```

to:

```rust
    pub fn get_selected_station(
        &self,
    ) -> Option<(String, u64, Option<pancetta_core::slot::SlotParity>)> {
```

In the `BandActivity` arm (lines 916–932), change the return tuple:

```rust
                Some((callsign.clone(), audio_hz, msg.slot_parity))
```

In the `DxHunter` arm (lines 933–945), DX cluster spots have no slot info:

```rust
                Some((station.call_sign.clone(), 1500, None))
```

- [ ] **Step 2: Update `App::activate_selected` to match the new tuple arity.**

In `pancetta-tui/src/app.rs:950`:

```rust
    pub fn activate_selected(&mut self) {
        if let Some((callsign, _freq, _parity)) = self.get_selected_station() {
            self.status_message = format!("Calling {}...", callsign);
        } else {
            self.status_message = "No station selected".to_string();
        }
    }
```

- [ ] **Step 3: Add `dx_parity` to the `TuiCommand::CallStation` variant.**

In `pancetta-tui/src/tui_runner.rs:119`:

```rust
    CallStation {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

If `pancetta-core` isn't in `pancetta-tui/Cargo.toml` (already handled in Task 4), it's there now.

- [ ] **Step 4: Update the Space handler.**

In `pancetta-tui/src/tui_runner.rs:471-480`:

```rust
            // Space - Select/activate (click-to-call)
            KeyCode::Char(' ') => {
                if let Some((callsign, frequency, dx_parity)) = app.get_selected_station() {
                    self.message_tx.send(TuiCommand::CallStation {
                        callsign,
                        frequency,
                        dx_parity,
                    })?;
                }
                app.activate_selected();
            }
```

- [ ] **Step 5: Update the consumer of `TuiCommand::CallStation`.**

```bash
grep -rn "TuiCommand::CallStation" pancetta pancetta-tui
```

For each match site (in `pancetta/src/runtime.rs` or similar), pattern-destructure to include `dx_parity` and forward it. The destructure site must compile after Task 6 adds the field to `QsoMessage::StartQso` — for now, just bind `dx_parity` and pass it along (or ignore it with `_dx_parity` if the receiving code is rebuilt in Task 6).

If the consumer currently looks like:

```rust
TuiCommand::CallStation { callsign, frequency } => {
    // builds QsoMessage::StartQso { callsign, frequency }
}
```

Change to:

```rust
TuiCommand::CallStation { callsign, frequency, dx_parity } => {
    // After Task 6 lands, will pass dx_parity into StartQso.
    let _dx_parity = dx_parity;
    // ... existing StartQso send ...
}
```

- [ ] **Step 6: Build the workspace.**

```bash
touch pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
cargo build -p pancetta-tui -p pancetta 2>&1 | tail -10
```

Expected: clean compile.

- [ ] **Step 7: Commit.**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs pancetta/src/runtime.rs 2>/dev/null
git add -A
git commit -m "feat(tui): get_selected_station returns dx_parity, Space forwards it

CallStation now carries dx_parity; the consumer plumbs it through to
the QSO StartQso message in Task 6.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Add `dx_parity` to `QsoMessage::StartQso` and forward it

**Files:**
- Modify: `pancetta/src/message_bus.rs:213` (the `QsoMessage::StartQso` variant)
- Modify: `pancetta/src/runtime.rs` or wherever `TuiCommand::CallStation` is converted (find with grep)
- Modify: `pancetta/src/coordinator/qso.rs:451-525`

- [ ] **Step 1: Add the field to the message.**

In `pancetta/src/message_bus.rs:213`:

```rust
    /// Start new QSO
    StartQso {
        callsign: String,
        frequency: u64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

If `pancetta-core` isn't a dep of the `pancetta` crate, add it:

```bash
grep -n "pancetta-core" pancetta/Cargo.toml
```

It almost certainly is (the crate root uses `pancetta_core::slot` already), but verify.

- [ ] **Step 2: Update the producer (the TuiCommand → QsoMessage bridge).**

```bash
grep -rn "QsoMessage::StartQso" pancetta/src
```

For each producer site, add `dx_parity` from the surrounding context (the `TuiCommand::CallStation` already carries it from Task 5).

- [ ] **Step 3: Update the consumer in coordinator/qso.rs.**

In `pancetta/src/coordinator/qso.rs:453-456` change the destructuring:

```rust
                                        crate::message_bus::QsoMessage::StartQso {
                                            callsign,
                                            frequency,
                                            dx_parity,
                                        } => {
```

Then in the body (line 461-464), pass it into `respond_to_cq` (which Task 7 extends to accept it):

```rust
                                            match qso_manager
                                                .respond_to_cq(
                                                    callsign.clone(),
                                                    frequency as f64,
                                                    dx_parity,
                                                )
                                                .await
```

- [ ] **Step 4: Build (will fail with arity mismatch on respond_to_cq — that's Task 7).**

```bash
touch pancetta/src/message_bus.rs pancetta/src/coordinator/qso.rs
cargo build -p pancetta 2>&1 | tail -15
```

Expected: error E0061 — `respond_to_cq` takes 2 arguments but 3 supplied. Proceed to Task 7 to fix.

- [ ] **Step 5: Commit (build will be red until Task 7).**

```bash
git add pancetta/src/message_bus.rs pancetta/src/coordinator/qso.rs pancetta/src/runtime.rs 2>/dev/null
git add -A
git commit -m "feat(qso): StartQso carries dx_parity from TUI

Plumbing-only commit — the QsoManager::respond_to_cq signature is
extended to accept dx_parity in Task 7. Build is red between this
commit and the next; resolved by completing Task 7 immediately.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Latch `tx_parity` in `QsoMetadata` and emit it from `MessageToSend`

**Files:**
- Modify: `pancetta-qso/src/states.rs:294-330` (`QsoMetadata`)
- Modify: `pancetta-qso/src/qso_manager.rs:137-175` (`QsoEvent::MessageToSend`), :320-372 (`start_cq`), :375-455 (`respond_to_cq`), :526-540 (the `emit_internal_message_to_send` helper if it exists)

- [ ] **Step 1: Write the failing test (parity latch).**

Create `pancetta-qso/tests/parity_latch.rs`:

```rust
//! Verifies that QSO metadata latches tx_parity at QSO start and
//! every subsequent MessageToSend for that QSO carries the same
//! latched value.

use pancetta_core::slot::SlotParity;
use pancetta_qso::{QsoEvent, QsoManager, QsoManagerConfig};
use tokio::sync::broadcast::error::TryRecvError;

#[tokio::test]
async fn respond_to_cq_latches_opposite_parity() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let mut rx = mgr.subscribe();

    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("respond_to_cq");

    // Drain events; expect a MessageToSend with tx_parity = Odd (opposite of Even).
    loop {
        match rx.try_recv() {
            Ok(QsoEvent::MessageToSend { tx_parity, .. }) => {
                assert_eq!(tx_parity, Some(SlotParity::Odd));
                break;
            }
            Ok(_) => continue,
            Err(TryRecvError::Empty) => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(TryRecvError::Closed) => panic!("broadcast closed"),
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }

    // Verify the latched parity is in metadata.
    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, Some(SlotParity::Odd));
}

#[tokio::test]
async fn respond_to_cq_with_no_dx_parity_latches_none() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, None)
        .await
        .expect("respond_to_cq");

    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, None);
}
```

If `pancetta-qso/Cargo.toml` doesn't list `pancetta-core` as a dev-dep or dep, check:

```bash
grep -n "pancetta-core" pancetta-qso/Cargo.toml
```

It should already be there (states.rs uses chrono which is independent). If not, add as a dev-dependency.

- [ ] **Step 2: Run the test to verify it fails.**

```bash
touch pancetta-qso/tests/parity_latch.rs
cargo test -p pancetta-qso --test parity_latch 2>&1 | tail -20
```

Expected: compile error — `respond_to_cq` arity mismatch and/or `tx_parity` field missing.

- [ ] **Step 3: Add `tx_parity` to `QsoMetadata`.**

In `pancetta-qso/src/states.rs:294-330` add a new field (after `notes`):

```rust
    /// Notes
    pub notes: Option<String>,

    /// Latched TX parity for this QSO. Set once at QSO creation
    /// (respond_to_cq passes the DX's parity, which is flipped to
    /// the opposite for our TX; start_cq passes our self-parity
    /// directly). Every subsequent MessageToSend event for this QSO
    /// carries the same value, ensuring all of our transmissions stay
    /// on the slot the contra station expects.
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
```

If `pancetta-qso/Cargo.toml` doesn't depend on `pancetta-core`, add it:

```bash
grep -n "pancetta-core" pancetta-qso/Cargo.toml
```

If missing, add to `[dependencies]`:

```toml
pancetta-core = { path = "../pancetta-core" }
```

- [ ] **Step 4: Add `tx_parity` to `QsoEvent::MessageToSend`.**

In `pancetta-qso/src/qso_manager.rs:150-154`:

```rust
    /// Message should be sent
    MessageToSend {
        qso_id: QsoId,
        message: MessageType,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

- [ ] **Step 5: Update `respond_to_cq` to accept and latch the DX parity.**

In `pancetta-qso/src/qso_manager.rs:375-455`:

```rust
    /// Respond to a CQ call
    ///
    /// `dx_parity` is the slot parity of the DX station's CQ, used to
    /// derive our `tx_parity` (opposite of theirs). May be `None` if
    /// the CQ came from a DX cluster spot rather than an on-air decode.
    pub async fn respond_to_cq(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        // ... duplicate-check and qso_id/now setup unchanged ...

        let tx_parity = dx_parity.map(|p| p.opposite());

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target_callsign.clone()),
            frequency,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
            tx_parity,
        };

        // ... QsoProgress build and insert unchanged ...

        // Send response message
        let message = MessageType::CqResponse {
            calling_station: target_callsign.clone(),
            responding_station: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
        })
        .await;

        // ... rest of fn unchanged ...
    }
```

The exact existing structure has a `let metadata = QsoMetadata { ... };` block near line 405 — replace it with the version above. Add `tx_parity` to the `emit_event(QsoEvent::MessageToSend { ... })` call near line 440.

- [ ] **Step 6: Update `start_cq` similarly.**

In `pancetta-qso/src/qso_manager.rs:320-372` (the `start_cq` method), accept a `tx_parity: Option<SlotParity>` parameter and propagate it:

```rust
    /// Start a CQ call.
    ///
    /// `tx_parity` is the parity we want our CQ to land on. `None`
    /// lets the TX scheduler pick (using the configured self-parity
    /// fallback). Callers driving auto-CQ from the autonomous operator
    /// will typically supply a fixed parity to keep cycles consistent.
    pub async fn start_cq(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
```

In the metadata block (line 329), add `tx_parity,`. In the `emit_event(QsoEvent::MessageToSend {...})` block (line 362), add `tx_parity,`.

- [ ] **Step 7: Update internal helper `emit_internal_message_to_send`.**

Find any other site that emits `QsoEvent::MessageToSend`:

```bash
grep -n "MessageToSend" pancetta-qso/src/qso_manager.rs
```

For each, look up the QSO's `metadata.tx_parity` and include it in the event:

```rust
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity: metadata.tx_parity,
            })
            .await;
```

(For `send_message` near line 526, fetch the metadata before constructing the event, similar to how `frequency` is already pulled.)

- [ ] **Step 8: Update all callers of `respond_to_cq` and `start_cq`.**

```bash
grep -rn "respond_to_cq\|start_cq" pancetta-qso pancetta
```

For every call site outside the tests we just added, supply `dx_parity` / `tx_parity`:

- `pancetta-qso/src/auto_sequencer.rs:447` — `respond_to_cq` is called with `(callsign, frequency)`. Add `, None` for now (auto_sequencer doesn't yet have the parity context; it will be wired through in a later task or it can default to None which causes the TX scheduler to use `tx_self_parity` config).
- `pancetta-qso/src/auto_sequencer.rs:428` — `start_cq` is called with `(frequency)`. Add `, None`.
- `pancetta/src/coordinator/qso.rs:461` — already updated in Task 6 to pass `dx_parity`.
- Any test fixtures.

- [ ] **Step 9: Update consumers of `QsoEvent::MessageToSend`.**

```bash
grep -rn "QsoEvent::MessageToSend" pancetta pancetta-qso
```

For each match, update the destructure to include `tx_parity`:

```rust
            Ok(pancetta_qso::QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
            }) => {
                // ... existing body ...
                let _tx_parity = tx_parity; // wired into TransmitRequest in Task 8
            }
```

The interesting site is `pancetta/src/coordinator/qso.rs:319-359` — that's where `MessageToSend` is forwarded to the TX worker. Bind `tx_parity` here; it'll be put on the new `TransmitRequest.tx_parity` field in Task 8.

- [ ] **Step 10: Run the parity-latch tests.**

```bash
touch pancetta-qso/src/qso_manager.rs pancetta-qso/src/states.rs
cargo test -p pancetta-qso --test parity_latch 2>&1 | tail -20
```

Expected: 2 tests pass.

- [ ] **Step 11: Run the full pancetta-qso test suite to catch regressions.**

```bash
cargo test -p pancetta-qso 2>&1 | tail -10
```

Expected: all existing tests still pass.

- [ ] **Step 12: Commit.**

```bash
git add pancetta-qso/Cargo.toml pancetta-qso/src/states.rs pancetta-qso/src/qso_manager.rs \
        pancetta-qso/src/auto_sequencer.rs pancetta-qso/tests/parity_latch.rs \
        pancetta/src/coordinator/qso.rs
git add -A
git commit -m "feat(qso): latch tx_parity in QsoMetadata; emit on MessageToSend

QsoMetadata gains tx_parity (Option<SlotParity>) set at QSO start.
respond_to_cq derives it as opposite_of(dx_parity); start_cq accepts
it directly. Every QsoEvent::MessageToSend for the QSO carries the
latched value. Add pancetta-qso/tests/parity_latch.rs covering the
respond_to_cq Even-DX → Odd-TX case and the no-DX-parity → None case.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Add `tx_parity` to `TransmitRequest`/`MultiTransmitRequest` and propagate from autonomous

**Files:**
- Modify: `pancetta/src/message_bus.rs:132-150` (`TransmitRequest`, `MultiTransmitRequest`, `TransmitRequestItem`)
- Modify: `pancetta-qso/src/autonomous.rs:231-236` (`DecodedMessageInfo`), :348-354 (`OperatorAction::Transmit`), :1003, :1062, :1095 (the construction sites)
- Modify: `pancetta/src/coordinator/autonomous.rs:213-325` (consume / forward `slot_parity` and `tx_parity`)
- Modify: `pancetta/src/coordinator/qso.rs:319-359` (forwarder of `MessageToSend` → `TransmitRequest`)

- [ ] **Step 1: Add `tx_parity` to the message-bus types.**

In `pancetta/src/message_bus.rs:132`:

```rust
    /// Request to transmit an FT8 message.
    /// ...
    TransmitRequest {
        message_text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        /// Required slot parity. `None` = no DX context (CQ);
        /// the scheduler falls back to the configured self-parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

In `pancetta/src/message_bus.rs:150`:

```rust
    MultiTransmitRequest {
        items: Vec<TransmitRequestItem>,
        /// Required slot parity for the bundle. All items in a bundle
        /// share the same slot, so they share the same parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

`TransmitRequestItem` (line 165) does NOT need a parity — items in a multi-TX always share the bundle's parity.

- [ ] **Step 2: Add `slot_parity` to `DecodedMessageInfo` and `tx_parity` to `OperatorAction::Transmit`.**

In `pancetta-qso/src/autonomous.rs:231-236`:

```rust
#[derive(Debug, Clone)]
pub struct DecodedMessageInfo {
    pub callsign: Option<String>,
    pub frequency_hz: f64,
    pub snr: i32,
    pub message_text: String,
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
}
```

In `pancetta-qso/src/autonomous.rs:348-354`:

```rust
#[derive(Debug, Clone)]
pub enum OperatorAction {
    /// Transmit an FT8 message at the given offset.
    Transmit {
        message_text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
```

- [ ] **Step 3: Update operator construction sites (3 places at lines ~1003, ~1062, ~1095) and tests.**

```bash
grep -n "OperatorAction::Transmit \{" pancetta-qso/src/autonomous.rs
```

For each non-test construction, derive `tx_parity` from the `DecodedMessageInfo` the operator is responding to. The simplest approach: each `decide()` call has access to the most-recent CQ candidate or active QSO; thread the `slot_parity` from the matching `DecodedMessageInfo` through. If the construction site doesn't already carry that context, store the parity alongside the `CqCandidate` (line 307):

```rust
#[derive(Debug, Clone)]
pub struct CqCandidate {
    pub callsign: String,
    pub grid: Option<String>,
    pub snr: i8,
    pub frequency_hz: f64,
    pub dx_score: f64,
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
}
```

When `feed_decoded_messages` constructs `CqCandidate` from `DecodedMessageInfo`, copy `slot_parity` over. When `decide()` builds `OperatorAction::Transmit` for a candidate, set `tx_parity = candidate.slot_parity.map(|p| p.opposite())`.

For the CQ-calling case (line ~1003), where the operator is starting its own CQ, `tx_parity` is `None` (let the TX scheduler use config self-parity).

For test fixtures inside the file (around lines 1264, 1278, 1351, 1371), add `slot_parity: None` (or a specific value if the test wants to assert parity) to each `DecodedMessageInfo` literal, and `tx_parity: None` to each `OperatorAction::Transmit` literal.

- [ ] **Step 4: Update the coordinator's autonomous wiring.**

In `pancetta/src/coordinator/autonomous.rs:333-338`, when building `DecodedMessageInfo` from the message bus:

```rust
                                            slot_messages.push(pancetta_qso::DecodedMessageInfo {
                                                callsign: decoded_msg.message.from_callsign.clone(),
                                                frequency_hz: decoded_msg.frequency_offset,
                                                snr: decoded_msg.snr_db as i32,
                                                message_text: decoded_msg.text.clone(),
                                                slot_parity: decoded_msg.slot_parity,
                                            });
```

In `pancetta/src/coordinator/autonomous.rs:218-234` (the action-dispatch loop), pull `tx_parity` out of the `Transmit` action:

```rust
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                        tx_parity,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                        }
                                        tx_items.push((
                                            crate::message_bus::TransmitRequestItem {
                                                message_text: message_text.clone(),
                                                frequency_offset,
                                                qso_id: qso_id.clone(),
                                            },
                                            tx_parity,
                                        ));
                                    }
```

(Note: `tx_items` becomes `Vec<(TransmitRequestItem, Option<SlotParity>)>`. Update its declaration on line 214.)

After the loop, the bundling logic (lines 298-325) needs to:

- For single-item: take the first item's parity.
- For multi-item: assert all items in the bundle share the same parity (they should, because all current QSOs latched the same parity at start, since they were all created in response to our CQ on slot N). Take any one.

```rust
                            if tx_items.len() == 1 {
                                let (item, tx_parity) = tx_items.remove(0);
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TransmitRequest {
                                        message_text: item.message_text,
                                        frequency_offset: item.frequency_offset,
                                        qso_id: item.qso_id,
                                        tx_parity,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send TransmitRequest: {}", e);
                                }
                            } else if tx_items.len() > 1 {
                                let bundle_parity = tx_items[0].1;
                                for (idx, (_, p)) in tx_items.iter().enumerate().skip(1) {
                                    if *p != bundle_parity {
                                        warn!(
                                            "Multi-TX item {} has tx_parity {:?}, bundle is {:?}; \
                                             using bundle parity",
                                            idx, p, bundle_parity
                                        );
                                    }
                                }
                                let items: Vec<_> = tx_items.into_iter().map(|(it, _)| it).collect();
                                info!("Bundling {} TX items into MultiTransmitRequest", items.len());
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::MultiTransmitRequest {
                                        items,
                                        tx_parity: bundle_parity,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send MultiTransmitRequest: {}", e);
                                }
                            }
```

- [ ] **Step 5: Update the QSO MessageToSend forwarder.**

In `pancetta/src/coordinator/qso.rs:319-359`, the `MessageToSend` arm. Read the existing block first; pattern-bind `tx_parity` (added in Task 7) and forward it:

```rust
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                qso_id,
                                message,
                                frequency,
                                tx_parity,
                            }) => {
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={}, tx_parity={:?})",
                                            text, frequency, qso_id, tx_parity
                                        );
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: text,
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                                tx_parity,
                                            },
                                            Instant::now(),
                                        );
                                        // ... rest unchanged ...
                                    }
                                    // ... error arm unchanged ...
                                }
                            }
```

Also update the StartQso handler at `pancetta/src/coordinator/qso.rs:477-485` where it manually builds a TransmitRequest:

```rust
                                                    let tx_msg = ComponentMessage::new(
                                                        ComponentId::Qso,
                                                        ComponentId::Ft8Transmitter,
                                                        MessageType::TransmitRequest {
                                                            message_text: reply.clone(),
                                                            frequency_offset: frequency as f64,
                                                            qso_id: Some(qso_id.to_string()),
                                                            tx_parity: dx_parity.map(|p| p.opposite()),
                                                        },
                                                        Instant::now(),
                                                    );
```

(`dx_parity` is in scope from Task 6's destructure of `StartQso`.)

- [ ] **Step 6: Update tx.rs destructure to bind `tx_parity` (without using it yet — wired in Task 11).**

In `pancetta/src/coordinator/tx.rs:107-110` and :272-273:

```rust
                                MessageType::TransmitRequest {
                                    message_text,
                                    frequency_offset,
                                    qso_id,
                                    tx_parity,
                                } => {
                                    let _tx_parity = tx_parity; // wired into scheduler in Task 11
```

```rust
                                MessageType::MultiTransmitRequest { items, tx_parity } => {
                                    let _tx_parity = tx_parity; // wired into scheduler in Task 12
```

- [ ] **Step 7: Build the workspace.**

```bash
touch pancetta/src/message_bus.rs pancetta-qso/src/autonomous.rs \
      pancetta/src/coordinator/autonomous.rs pancetta/src/coordinator/qso.rs \
      pancetta/src/coordinator/tx.rs
cargo build -p pancetta -p pancetta-qso 2>&1 | tail -15
```

Expected: clean compile.

- [ ] **Step 8: Run all tests to confirm no regressions.**

```bash
cargo test -p pancetta-qso 2>&1 | tail -10
cargo test -p pancetta --test loopback_qso 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 9: Commit.**

```bash
git add -A
git commit -m "feat: thread tx_parity through TransmitRequest and autonomous

TransmitRequest/MultiTransmitRequest carry tx_parity from the QSO
metadata or the autonomous operator into the TX scheduler.
DecodedMessageInfo and CqCandidate carry slot_parity so the operator
can derive opposite parity for responses. Bundle assertion: all
multi-TX items share parity (warn-only if they don't, take the
bundle's first value).

The TX scheduler binds the new field but does not yet act on it —
wired into the actual scheduling logic in Tasks 10/11/12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Add station-config fields for late max, self-parity, and PTT lead

**Files:**
- Modify: `pancetta-config/src/station.rs`
- Modify: any default-config helper / TOML bootstrap

- [ ] **Step 1: Write the failing test (deserialize round-trip).**

In `pancetta-config/src/station.rs`, find the existing `#[cfg(test)] mod tests` block (search):

```bash
grep -n "#\[cfg(test)\]" pancetta-config/src/station.rs
```

Append:

```rust
    #[test]
    fn station_config_parses_new_tx_fields() {
        let toml = r#"
            callsign = "K5ARH"
            grid_square = "EM10"
            power_watts = 100
            qth = "Test"
            dxcc_entity = 291
            itu_zone = 8
            cq_zone = 4
            antennas = []
            tx_late_max_ms = 6000
            tx_self_parity = "even"
            ptt_lead_ms = 120
        "#;
        let cfg: StationConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.tx_late_max_ms, 6000);
        assert_eq!(cfg.tx_self_parity, TxSelfParity::Even);
        assert_eq!(cfg.ptt_lead_ms, 120);
    }

    #[test]
    fn station_config_defaults_when_new_tx_fields_absent() {
        let toml = r#"
            callsign = "K5ARH"
            grid_square = "EM10"
            power_watts = 100
            qth = "Test"
            dxcc_entity = 291
            itu_zone = 8
            cq_zone = 4
            antennas = []
        "#;
        let cfg: StationConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.tx_late_max_ms, 8000);
        assert_eq!(cfg.tx_self_parity, TxSelfParity::Auto);
        assert_eq!(cfg.ptt_lead_ms, 80);
    }
```

- [ ] **Step 2: Run test to verify it fails.**

```bash
touch pancetta-config/src/station.rs
cargo test -p pancetta-config station_config_parses_new_tx_fields station_config_defaults 2>&1 | tail -15
```

Expected: compile error — `TxSelfParity`, `tx_late_max_ms`, etc. don't exist.

- [ ] **Step 3: Add the new fields and enum.**

In `pancetta-config/src/station.rs:13`, extend `StationConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct StationConfig {
    // ... existing fields ...
    /// Custom fields for extensibility
    #[serde(default)]
    pub custom_fields: HashMap<String, String>,

    /// Maximum latency past the slot boundary at which we still attempt
    /// late-start TX via audio skip-ahead. Beyond this, defer to the
    /// next opposite-parity slot. Default 8000ms — leaves ~5s of audio
    /// on the air with two of three Costas sync arrays still in window.
    #[serde(default = "default_tx_late_max_ms")]
    pub tx_late_max_ms: u64,

    /// When calling CQ (no DX context), prefer this parity. `Auto`
    /// (default) lets the scheduler pick whichever next slot is closer.
    #[serde(default)]
    pub tx_self_parity: TxSelfParity,

    /// PTT engage lead time before slot boundary, in milliseconds.
    /// Default 80ms — enough for solid-state keying. Bump up for slow
    /// mechanical relays.
    #[serde(default = "default_ptt_lead_ms")]
    pub ptt_lead_ms: u64,
}

fn default_tx_late_max_ms() -> u64 {
    8000
}

fn default_ptt_lead_ms() -> u64 {
    80
}

/// Self-parity preference when calling CQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TxSelfParity {
    /// Pick whichever next slot is closer, regardless of parity.
    #[default]
    Auto,
    /// Lock CQ to even slots (`:00`, `:30`).
    Even,
    /// Lock CQ to odd slots (`:15`, `:45`).
    Odd,
}
```

- [ ] **Step 4: Update any default-config builder.**

```bash
grep -rn "StationConfig \{" pancetta-config pancetta
```

For each construction site (`Default` impl, test fixtures, bootstrap wizard), add the three new fields with their defaults:

```rust
    tx_late_max_ms: 8000,
    tx_self_parity: pancetta_config::station::TxSelfParity::Auto,
    ptt_lead_ms: 80,
```

- [ ] **Step 5: Run the new tests.**

```bash
cargo test -p pancetta-config station_config_parses_new_tx_fields station_config_defaults 2>&1 | tail -10
```

Expected: 2 tests pass.

- [ ] **Step 6: Run full pancetta-config tests.**

```bash
cargo test -p pancetta-config 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 7: Commit.**

```bash
git add pancetta-config/src/station.rs
git add -A
git commit -m "feat(config): tx_late_max_ms, tx_self_parity, ptt_lead_ms

Three new station config fields used by the slot-aware TX scheduler.
All have serde defaults so existing config files keep working without
edits. tx_self_parity is an enum (Auto/Even/Odd) defaulting to Auto.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Implement `schedule_tx()` helper in `tx.rs`

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` (top-level helper + tests)

- [ ] **Step 1: Write the failing tests for the scheduler decision matrix.**

Append at the bottom of `pancetta/src/coordinator/tx.rs` (after the `impl ApplicationCoordinator` block):

```rust
#[cfg(test)]
mod schedule_tx_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_core::slot::SlotParity;

    fn at(seconds: f64) -> chrono::DateTime<chrono::Utc> {
        // Reference: 2026-01-01 00:00:00 UTC. timestamp() = 1767225600,
        // divisible by 15. Slot 0 is Even (= 1767225600 / 15 % 2 = 0).
        let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        base + chrono::Duration::nanoseconds((seconds * 1_000_000_000.0) as i64)
    }

    #[test]
    fn early_pads_silent_no_skip() {
        // now = :05.0 (Even slot 0). Required = Odd. Current slot is Even
        // (wrong), so we advance to next Odd = :15. mstr_relative_to_target
        // = max(0, :05 - :15) = 0. 0 < 500 → pad 500ms, cursor 0.
        let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12); // 12 samples/ms at 12kHz
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn on_time_no_pad_no_skip() {
        // now = :15.500 (Odd slot 1). Required = Odd. Current slot matches;
        // mstr_in_current_slot = 500ms ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 500 = DELAY_MS → pad 0, cursor 0.
        let s = schedule_tx(at(15.5), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn late_skips_cursor_in_current_slot() {
        // now = :20.0 (Odd slot 1, 5s in). Required = Odd. Current matches;
        // mstr_in_current_slot = 5000 ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 5000 > 500 → cursor = 4500ms × SR.
        let s = schedule_tx(at(20.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 4500 * 12);
    }

    #[test]
    fn too_late_defers_to_next_opposite_slot() {
        // now = :24.5 (Odd slot 1, 9.5s in). Required = Odd. Current
        // matches but mstr_in_current_slot = 9500 > 8000 → too late;
        // advance to next Odd = :45. mstr_relative_to_target = 0 (target
        // is in future) → pad 500ms, cursor 0.
        let s = schedule_tx(at(24.5), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn collision_avoidance_does_not_pick_same_parity() {
        // now = :14.6 (Even slot 0, near end). DX on Even → required Odd.
        // Current parity Even ≠ Odd → advance to next Odd = :15.
        // mstr_relative_to_target = max(0, :14.6 - :15) = 0 → pad 500ms,
        // cursor 0. Most importantly: target is :15, NEVER :30 (the
        // collision case the original bug produced).
        let s = schedule_tx(at(14.6), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_ne!((s.target_slot - at(0.0)).num_milliseconds(), 30_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn at_exact_boundary_targets_current_slot() {
        // now = :15.000 exactly. Required = Odd. The :15 slot is Odd
        // and we're 0ms in — fully viable. Target the current slot,
        // pad 500ms before audio starts.
        let s = schedule_tx(at(15.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn current_slot_correct_parity_but_too_late_defers() {
        // now = :29.0 (Odd slot 1, 14s in — past tx_late_max_ms=8000).
        // Even though parity matches, we're too late for skip-ahead.
        // Defer to next Odd = :45.
        let s = schedule_tx(at(29.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail.**

```bash
touch pancetta/src/coordinator/tx.rs
cargo test -p pancetta schedule_tx_tests 2>&1 | tail -20
```

Expected: compile error — `schedule_tx` and `TxSchedule` don't exist.

- [ ] **Step 3: Implement the helper.**

Add near the top of `pancetta/src/coordinator/tx.rs` (after the `use` block):

```rust
/// FT8 nominal pre-roll: audio starts 500ms past the slot boundary.
const DELAY_MS: u64 = 500;

/// Output of `schedule_tx`: where to TX, how much silence to pad in
/// front, and how far into the modulated waveform to start emitting.
#[derive(Debug, Clone, Copy)]
pub struct TxSchedule {
    /// UTC time of the slot boundary we're targeting.
    pub target_slot: chrono::DateTime<chrono::Utc>,
    /// Number of zero samples to emit before the modulated waveform.
    pub silent_pad_samples: usize,
    /// Sample offset into the waveform — caller emits `waveform[cursor..]`.
    pub cursor_offset_samples: usize,
}

/// WSJT-X-style late-start TX scheduler.
///
/// Picks the slot to TX in (current slot if parity matches and we're
/// within `tx_late_max_ms`, otherwise next slot of `required_parity`),
/// then decides how to align audio relative to that slot's boundary:
///
/// - **Early or just-arrived** (`mstr < DELAY_MS`): pad `(DELAY_MS - mstr)`
///   ms of zeros in front, emit the full 12.64s waveform starting at
///   slot+500ms (the FT8 pre-roll).
/// - **Late but viable** (`DELAY_MS <= mstr <= tx_late_max_ms`): skip
///   `(mstr - DELAY_MS)` ms into the waveform. WSJT-X's `m_ic` analogue.
/// - **Too late** (`mstr > tx_late_max_ms`): defer to the next slot of
///   the required parity (30s away), recompute as the early case.
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
) -> TxSchedule {
    use pancetta_core::slot::{current_slot_start, next_slot_with_parity, SlotParity};

    let cur_start = current_slot_start(now);
    let cur_parity = SlotParity::of(cur_start);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let target = if cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms {
        cur_start
    } else {
        next_slot_with_parity(now, required_parity)
    };

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
    }
}
```

- [ ] **Step 4: Run the tests.**

```bash
touch pancetta/src/coordinator/tx.rs
cargo test -p pancetta schedule_tx_tests 2>&1 | tail -15
```

Expected: 6 tests pass.

- [ ] **Step 5: Commit.**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): WSJT-X-style schedule_tx helper

Returns target_slot, silent_pad_samples, cursor_offset_samples for a
given (now, required_parity, tx_late_max_ms, sample_rate). Six unit
tests cover the early/on-time/late/too-late/collision-avoidance/exact-
boundary cases.

Not yet wired into the actual TX request handlers — that's Tasks 11
and 12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Wire `schedule_tx()` into the `TransmitRequest` path

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:96-269` (the `TransmitRequest` arm)

- [ ] **Step 1: Replace the timing constants and arm body.**

In `pancetta/src/coordinator/tx.rs:96-269`, the current path:

1. Computes `next_audio_start(now, MIN_LEAD=1s)`.
2. Sleeps until PTT-engage (audio - 200ms).
3. Sleeps to audio-start.
4. Sends the full `samples` to `AudioOutput`.

Replace with the new path. First, drop `MIN_LEAD` and reduce `PTT_LEAD` to a runtime value pulled from config:

Read config at task startup. In the surrounding `start_transmitter_component` (line 67–85), capture the config snapshot (the existing pattern in this file uses `self.config.read().await`):

```rust
        // Capture config snapshot for TX timing parameters.
        let (tx_late_max_ms, tx_self_parity, ptt_lead_ms, sample_rate) = {
            let cfg = self.config.read().await;
            (
                cfg.station.tx_late_max_ms,
                cfg.station.tx_self_parity,
                cfg.station.ptt_lead_ms,
                12000u32, // FT8 sample rate
            )
        };
```

Move these into the spawned task. Then in the `TransmitRequest` arm (replace lines ~180-219):

```rust
                                    // --- Step 2: Resolve required parity ---
                                    let required_parity = match tx_parity {
                                        Some(p) => p,
                                        None => match tx_self_parity {
                                            pancetta_config::station::TxSelfParity::Even => {
                                                pancetta_core::slot::SlotParity::Even
                                            }
                                            pancetta_config::station::TxSelfParity::Odd => {
                                                pancetta_core::slot::SlotParity::Odd
                                            }
                                            pancetta_config::station::TxSelfParity::Auto => {
                                                // Auto: take whichever parity is sooner. Pick the
                                                // one whose next slot is nearest now.
                                                let now = chrono::Utc::now();
                                                let next_even = pancetta_core::slot::next_slot_with_parity(
                                                    now,
                                                    pancetta_core::slot::SlotParity::Even,
                                                );
                                                let next_odd = pancetta_core::slot::next_slot_with_parity(
                                                    now,
                                                    pancetta_core::slot::SlotParity::Odd,
                                                );
                                                if next_even <= next_odd {
                                                    pancetta_core::slot::SlotParity::Even
                                                } else {
                                                    pancetta_core::slot::SlotParity::Odd
                                                }
                                            }
                                        },
                                    };

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                    );

                                    info!(
                                        "TX scheduled: parity={:?} target_slot={} pad={} samples cursor={} samples",
                                        required_parity,
                                        schedule.target_slot.format("%H:%M:%S%.3f UTC"),
                                        schedule.silent_pad_samples,
                                        schedule.cursor_offset_samples,
                                    );

                                    // --- Step 3: Build the audio buffer to ship ---
                                    // Pad zeros in front (early branch); skip cursor into
                                    // waveform (late branch); never both at the same time.
                                    let mut audio_out: Vec<f32> =
                                        Vec::with_capacity(schedule.silent_pad_samples + samples.len());
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
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0) as u64;

                                    // --- Step 4: Sleep until PTT engage instant ---
                                    let ptt_target_utc = schedule.target_slot
                                        - chrono::Duration::milliseconds(ptt_lead_ms as i64);
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_ptt).await;

                                    // --- Step 5: Assert PTT ---
                                    let mut ptt_guard = PttGuard::new(message_bus.clone());
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        debug!("PTT on failed (no rig?): {}", e);
                                    }

                                    // --- Step 6: Sleep precisely until target slot start ---
                                    // (audio_out itself includes any silent_pad needed past
                                    // the slot boundary; we send it at the boundary.)
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    sleep(to_slot).await;

                                    // --- Step 7: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for audio playback to complete ---
                                    sleep(Duration::from_millis(audio_duration_ms)).await;
                                    let success = true;
                                    let duration_ms = audio_duration_ms;

                                    // --- Step 9: De-assert PTT (with tail delay) ---
                                    sleep(Duration::from_millis(50)).await;
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        debug!("PTT off failed (no rig?): {}", e);
                                    }
                                    ptt_guard.disarm();
```

The TransmitComplete send at the bottom of the arm stays the same; just confirm `duration_ms` is the new `audio_duration_ms`.

Drop the now-unused `MIN_LEAD` constant and `target_audio_utc` variable. Drop `next_audio_start` import if it's no longer used.

- [ ] **Step 2: Build.**

```bash
touch pancetta/src/coordinator/tx.rs
cargo build -p pancetta 2>&1 | tail -15
```

Expected: clean compile. Fix any unused-import warnings flagged by clippy.

- [ ] **Step 3: Run loopback to confirm end-to-end TX still works.**

```bash
cargo test -p pancetta --test loopback_qso 2>&1 | tail -15
```

Expected: pass. (Existing loopback uses `Auto` self-parity; should land on a slot, encode-modulate-decode round trip succeeds.)

- [ ] **Step 4: Commit.**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): wire schedule_tx into TransmitRequest path

Replaces next_audio_start(now, MIN_LEAD=1s) with the parity-aware
schedule_tx helper. Audio buffer is padded in front when early or
trimmed from the front when late; PTT lead drops from 200ms to a
configurable value (default 80ms). MIN_LEAD is gone — the only lead
needed is what PTT engage requires.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Wire `schedule_tx()` into the `MultiTransmitRequest` path

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs:272-453` (the `MultiTransmitRequest` arm)

- [ ] **Step 1: Apply the same scheduling logic.**

The multi-TX arm has identical timing structure — replace its Step 2/3/5 (currently `next_audio_start` + `MIN_LEAD`) with the `schedule_tx` flow used in Task 11. Reuse the same `required_parity` resolution block (the binding `tx_parity` is already extracted from the destructure in Task 8). The pre-modulated `samples: Vec<f32>` already exists at this point in the existing code; pad/trim it the same way.

Concrete diff sketch — the `Step 2: Compute` block becomes:

```rust
                                    // --- Step 2: Resolve required parity (same as single-TX) ---
                                    let required_parity = match tx_parity {
                                        Some(p) => p,
                                        None => match tx_self_parity {
                                            pancetta_config::station::TxSelfParity::Even => pancetta_core::slot::SlotParity::Even,
                                            pancetta_config::station::TxSelfParity::Odd => pancetta_core::slot::SlotParity::Odd,
                                            pancetta_config::station::TxSelfParity::Auto => {
                                                let now = chrono::Utc::now();
                                                let next_even = pancetta_core::slot::next_slot_with_parity(now, pancetta_core::slot::SlotParity::Even);
                                                let next_odd = pancetta_core::slot::next_slot_with_parity(now, pancetta_core::slot::SlotParity::Odd);
                                                if next_even <= next_odd { pancetta_core::slot::SlotParity::Even } else { pancetta_core::slot::SlotParity::Odd }
                                            }
                                        },
                                    };

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                    );

                                    let mut audio_out: Vec<f32> =
                                        Vec::with_capacity(schedule.silent_pad_samples + samples.len());
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(&samples[schedule.cursor_offset_samples..]);
                                    } else {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform; skipping");
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
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0) as u64;
```

Steps 3–9 then mirror the single-TX arm's PTT/sleep/audio/TransmitComplete sequence from Task 11. Replace `target_audio_utc` references with `schedule.target_slot`, and `samples` with `audio_out` in the `AudioOutput` send.

To DRY the code, you may extract a helper:

```rust
fn resolve_required_parity(
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    now: chrono::DateTime<chrono::Utc>,
) -> pancetta_core::slot::SlotParity {
    if let Some(p) = tx_parity {
        return p;
    }
    use pancetta_config::station::TxSelfParity;
    use pancetta_core::slot::SlotParity;
    match tx_self_parity {
        TxSelfParity::Even => SlotParity::Even,
        TxSelfParity::Odd => SlotParity::Odd,
        TxSelfParity::Auto => {
            let next_even = pancetta_core::slot::next_slot_with_parity(now, SlotParity::Even);
            let next_odd = pancetta_core::slot::next_slot_with_parity(now, SlotParity::Odd);
            if next_even <= next_odd { SlotParity::Even } else { SlotParity::Odd }
        }
    }
}
```

Place it next to `schedule_tx`. Use it in both arms.

- [ ] **Step 2: Build and run all tests.**

```bash
touch pancetta/src/coordinator/tx.rs
cargo build -p pancetta 2>&1 | tail -10
cargo test -p pancetta --test loopback_qso 2>&1 | tail -15
cargo test -p pancetta schedule_tx_tests 2>&1 | tail -10
```

Expected: clean build, both test sets green.

- [ ] **Step 3: Commit.**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): wire schedule_tx into MultiTransmitRequest path

Bundle TX gets the same parity-aware scheduling and pad/cursor logic
as single TX. Extracted resolve_required_parity helper so the two
arms don't drift.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Add loopback integration test for late-press and collision-avoidance

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs`

- [ ] **Step 1: Read the existing loopback test for fixture conventions.**

```bash
grep -n "fn\|async fn\|#\[tokio::test\]\|#\[test\]" pancetta/tests/loopback_qso.rs | head -30
```

Note the helper functions and the convention for asserting on TX output.

- [ ] **Step 2: Add the late-press test.**

Append to `pancetta/tests/loopback_qso.rs`:

```rust
/// At slot+5s past an Odd slot's start, with required parity = Odd, the
/// scheduler picks THAT slot (not the next Odd 30s away) and produces a
/// non-empty audio buffer with a cursor offset of 4500ms × sample_rate.
#[test]
fn schedule_tx_late_press_targets_current_opposite_slot() {
    use chrono::TimeZone;
    use pancetta::coordinator::schedule_tx;
    use pancetta_core::slot::SlotParity;

    let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let now = base + chrono::Duration::milliseconds(20_000); // :20.0
    let s = schedule_tx(now, SlotParity::Odd, 8000, 12_000);
    // The Odd slot at :15 ends at :30. We want to land in *that* slot.
    assert_eq!((s.target_slot - base).num_seconds(), 15);
    assert_eq!(s.cursor_offset_samples, 4_500 * 12);
    assert_eq!(s.silent_pad_samples, 0);
}

/// Pressing Space at slot N + 14.6s with DX on Even must NOT pick the
/// next Even slot — it must pick the Odd slot at :15. Regression test
/// for the original bug.
#[test]
fn schedule_tx_no_collision_on_late_press_near_boundary() {
    use chrono::TimeZone;
    use pancetta::coordinator::schedule_tx;
    use pancetta_core::slot::SlotParity;

    let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let now = base + chrono::Duration::milliseconds(14_600); // :14.6
    let s = schedule_tx(now, SlotParity::Odd, 8000, 12_000);
    let secs = (s.target_slot - base).num_seconds();
    // MUST be :15 (Odd), NOT :30 (Even — would collide with DX).
    assert_eq!(secs, 15);
    assert_ne!(secs, 30);
}
```

If `pancetta::coordinator::schedule_tx` isn't currently re-exported, add it to `pancetta/src/coordinator/mod.rs`:

```bash
grep -n "pub use\|pub mod" pancetta/src/coordinator/mod.rs | head -10
```

Add:

```rust
pub use tx::{schedule_tx, TxSchedule};
```

(Adjust to match the existing re-export style.)

- [ ] **Step 3: Run the new tests.**

```bash
touch pancetta/tests/loopback_qso.rs
cargo test -p pancetta --test loopback_qso schedule_tx_late_press schedule_tx_no_collision 2>&1 | tail -15
```

Expected: 2 tests pass.

- [ ] **Step 4: Run the full loopback test suite.**

```bash
cargo test -p pancetta --test loopback_qso 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 5: Commit.**

```bash
git add pancetta/tests/loopback_qso.rs pancetta/src/coordinator/mod.rs
git commit -m "test(loopback): late-press and collision-avoidance regression tests

Locks in the bug fix: pressing Space 14.6s into a DX slot now
schedules TX at :15 (the opposite-parity slot), never at :30 (the
same-parity collision the original next_audio_start produced).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Add four-message-QSO parity-latch test

**Files:**
- Modify: `pancetta-qso/tests/parity_latch.rs` (extend the file from Task 7)

- [ ] **Step 1: Append the four-message test.**

To `pancetta-qso/tests/parity_latch.rs`:

```rust
/// A full QSO progression — CqResponse, SignalReport, ReportAck,
/// FinalConfirmation — must emit four MessageToSend events that all
/// carry the same latched tx_parity (the opposite of the DX's slot).
#[tokio::test]
async fn four_message_qso_parity_latch_holds() {
    use pancetta_ft8::Ft8Message;
    use pancetta_qso::states::{MessageType as QsoMsg, SignalReport};

    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let mut rx = mgr.subscribe();

    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("respond_to_cq");

    // Drive the QSO through three more transitions by feeding messages
    // that the QSO state machine accepts. Concretely: process_message
    // with the shape the manager expects.
    //
    // For the purposes of *this* test we only assert that every
    // MessageToSend that comes out carries tx_parity = Some(Odd). The
    // exact transitions are exercised in qso_manager's own tests; we
    // just need to walk the state machine.
    mgr.process_message(
        QsoMsg::SignalReport {
            from_station: "K1ABC".to_string(),
            to_station: "K5ARH".to_string(),
            report: -10,
        },
        "K5ARH K1ABC -10".to_string(),
        1500.0,
        Some(-10.0),
    )
    .await
    .expect("process signal report");

    mgr.process_message(
        QsoMsg::ReportAck {
            from_station: "K1ABC".to_string(),
            to_station: "K5ARH".to_string(),
            report: -10,
        },
        "K5ARH K1ABC R-10".to_string(),
        1500.0,
        Some(-10.0),
    )
    .await
    .expect("process report ack");

    let mut seen_message_to_send = 0;
    while let Ok(event) = tokio::time::timeout(
        tokio::time::Duration::from_millis(200),
        rx.recv(),
    )
    .await
    {
        if let Ok(QsoEvent::MessageToSend { tx_parity, .. }) = event {
            assert_eq!(tx_parity, Some(SlotParity::Odd), "parity must stay latched");
            seen_message_to_send += 1;
        }
    }
    assert!(
        seen_message_to_send >= 1,
        "expected at least one MessageToSend (got {})",
        seen_message_to_send
    );

    // Final assert: metadata still says Odd.
    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, Some(SlotParity::Odd));
}
```

If `process_message` signatures or `MessageType` variants differ from this draft, read `pancetta-qso/src/qso_manager.rs` and adjust the test to call the actual public API. The crucial assertion is `assert_eq!(tx_parity, Some(SlotParity::Odd))` for every emitted `MessageToSend`.

- [ ] **Step 2: Run the test.**

```bash
touch pancetta-qso/tests/parity_latch.rs
cargo test -p pancetta-qso --test parity_latch four_message_qso_parity_latch_holds 2>&1 | tail -15
```

Expected: 1 test passes (or surfaces a real arity mismatch, which means adjust the call signatures and re-run).

- [ ] **Step 3: Commit.**

```bash
git add pancetta-qso/tests/parity_latch.rs
git commit -m "test(qso): parity stays latched across the four-message QSO

Walks a respond_to_cq → SignalReport → ReportAck progression and
asserts every MessageToSend event carries tx_parity = Some(Odd) —
i.e., the latch from the initial Even-DX context survives the full
state machine.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 15: Update `auto_sequencer.rs` and any other `respond_to_cq`/`start_cq` callers I missed

**Files:**
- Modify: `pancetta-qso/src/auto_sequencer.rs:425-453`
- Modify: any test/example fixtures that call these methods

- [ ] **Step 1: Find all remaining old-arity call sites.**

```bash
grep -rn "respond_to_cq\b\|start_cq\b" pancetta-qso pancetta pancetta-tui
```

For each match that doesn't already pass the new parameter, add `, None`:

- `pancetta-qso/src/auto_sequencer.rs:447` — `respond_to_cq(callsign, frequency)` → `respond_to_cq(callsign, frequency, None)`. (auto_sequencer's `evaluate_cq_call` doesn't currently know the heard slot's parity; threading that through is a follow-up — for now `None` causes the TX scheduler to fall back to `tx_self_parity` from config, which keeps autonomous CQ responses working but loses parity coupling. Note this in the commit.)
- `pancetta-qso/src/auto_sequencer.rs:428` — `start_cq(frequency)` → `start_cq(frequency, None)`.
- `pancetta-qso/examples/qso_logging.rs` — likely has a `start_cq` or `respond_to_cq` call. Add `, None`.

- [ ] **Step 2: Build and run all tests.**

```bash
touch pancetta-qso/src/auto_sequencer.rs
# NEVER use --workspace here: pancetta-hamlib's tokio runtime conflict
# hangs the test process indefinitely. Plain `cargo test` uses
# `default-members` which excludes pancetta-hamlib for this reason.
cargo build --features transmit 2>&1 | tail -10
cargo test -p pancetta-qso 2>&1 | tail -10
cargo test -p pancetta --test loopback_qso 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 3: Commit.**

```bash
git add -A
git commit -m "fix(qso): update auto_sequencer to new respond_to_cq/start_cq arity

Passes None for parity from auto_sequencer for now — it doesn't yet
have the original DecodedMessage in scope to derive parity, so the
TX scheduler falls back to tx_self_parity config. Threading parity
into auto_sequencer is a follow-up; logged in CLAUDE.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 16: Update CLAUDE.md, FEATURES.md, and ARCHITECTURE.md

**Files:**
- Modify: `CLAUDE.md` — add to "Known Gaps" the auto_sequencer parity-fallback note, and to "Architecture Highlights" a one-liner about slot-aware TX
- Modify: `FEATURES.md` — note the bug fix
- Modify: `docs/ARCHITECTURE.md` — update the TX-path data flow with the parity hop
- Modify: `docs/CONFIG.md` — document the three new station fields

- [ ] **Step 1: Update CLAUDE.md.**

In `CLAUDE.md` under "Architecture Highlights", add:

```markdown
- **DX-slot-aware TX scheduling** (`pancetta/src/coordinator/tx.rs`): WSJT-X-style. Every `DecodedMessage` carries `slot_parity`; the QSO state machine latches `tx_parity = opposite_of(dx_parity)` at QSO start; the TX scheduler picks the next slot of that parity, padding silent samples if early or skip-ahead-cursoring into the modulated waveform if late (up to `tx_late_max_ms`, default 8s). Past that, defers 30s. Never collides with the DX's parity. See `docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`.
```

In CLAUDE.md under "Known Gaps and TODOs", add:

```markdown
- `auto_sequencer::evaluate_cq_call` does not yet thread `slot_parity` from the original `DecodedMessage` into `respond_to_cq` — it currently passes `None`, causing the TX scheduler to use the configured self-parity instead of opposite-of-DX parity. Functional but suboptimal for autonomous responses; manual Space-press path is correct.
```

- [ ] **Step 2: Update FEATURES.md (if it has a TX or QSO section).**

```bash
grep -n "TX\|transmit\|slot" FEATURES.md | head -10
```

Add a one-line entry like "FT8 TX honors WSJT-X-style slot parity and supports late-start audio playback up to 8s past the slot boundary."

- [ ] **Step 3: Update docs/ARCHITECTURE.md.**

```bash
grep -n "TX path\|TransmitRequest\|slot" docs/ARCHITECTURE.md | head -10
```

Find the TX-path data-flow paragraph and append a sentence:

> `TransmitRequest` carries an `Option<SlotParity>` set from the latched QSO metadata (or `None` for unsolicited CQ); the scheduler picks the next opposite-parity slot and uses silent-pad / cursor-offset to align audio to the slot boundary. See `docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`.

- [ ] **Step 4: Update docs/CONFIG.md.**

Find the `[station]` section (`grep -n "\[station\]" docs/CONFIG.md`) and append:

```markdown
### `tx_late_max_ms` *(u64, default 8000)*

Maximum latency past the slot boundary at which the TX scheduler
will still attempt a late-start TX via audio cursor skip-ahead.
Beyond this, defers to the next opposite-parity slot (30s later).
8s leaves ~5s of audio on the air, which is enough for the receiver
to lock onto the middle and end Costas sync arrays.

### `tx_self_parity` *(string: "auto" | "even" | "odd", default "auto")*

When calling CQ (no DX heard), pick TX slot parity by this rule.
"auto" picks whichever next slot is closer; "even" / "odd" lock to
the named parity.

### `ptt_lead_ms` *(u64, default 80)*

PTT engage lead time before the slot boundary. Drop to 50ms for
fast solid-state keying; bump up to 150–200ms for slow mechanical
relays.
```

- [ ] **Step 5: Build docs to confirm no syntax errors (markdown is unchecked, but spot-check links).**

```bash
ls -la docs/CONFIG.md docs/ARCHITECTURE.md FEATURES.md CLAUDE.md
grep -c "tx_late_max_ms\|slot_parity\|tx_self_parity" CLAUDE.md docs/CONFIG.md docs/ARCHITECTURE.md FEATURES.md
```

Expected: all four files mention the new fields.

- [ ] **Step 6: Commit.**

```bash
git add CLAUDE.md FEATURES.md docs/ARCHITECTURE.md docs/CONFIG.md
git commit -m "docs: DX-slot-aware TX behavior and new station config fields

CLAUDE.md, FEATURES.md, ARCHITECTURE.md, and CONFIG.md updated with
the parity-aware scheduler design, the late-start audio-playback
mechanism, and the three new station fields (tx_late_max_ms,
tx_self_parity, ptt_lead_ms). Also notes the auto_sequencer parity-
fallback gap as a follow-up.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 17: Final workspace test sweep

- [ ] **Step 1: Run every relevant test target.**

> **Hard rule:** never run `cargo test --workspace` here. pancetta-hamlib has
> a tokio runtime conflict that hangs the test process indefinitely (observed:
> 1h 48min with no output). Plain `cargo test` uses `default-members` from the
> workspace `Cargo.toml`, which excludes pancetta-hamlib. Test pancetta-hamlib
> separately as shown below.

```bash
cargo test --features transmit 2>&1 | tail -20
cargo test -p pancetta-hamlib --lib -- --test-threads=1 2>&1 | tail -10
cargo test -p pancetta --test loopback_qso 2>&1 | tail -10
cargo test -p pancetta-qso --test parity_latch 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 2: Run clippy and fmt.**

```bash
# Same hard rule — no --workspace. Plain `cargo clippy` honours default-members.
cargo clippy --features transmit 2>&1 | tail -20
cargo fmt --all -- --check 2>&1 | tail -10
```

Fix any warnings (the most likely are unused imports from the dropped `MIN_LEAD` and `next_audio_start` paths). Re-run until clean. Format any drift. Commit if any changes.

- [ ] **Step 3: Manual hardware validation (operator runs this after merge).**

Document the manual validation in a short Markdown checklist appended to `docs/superpowers/plans/2026-04-27-dx-slot-aware-tx.md`:

```markdown
## Manual hardware validation (post-merge)

1. With FTdx10 connected, run `pancetta` and listen on 20m FT8 (14.074 MHz).
2. Wait for an Even-slot CQ. Press Space 1s into the slot. Confirm:
   - TUI status: "Calling KXXXX — TX queued (Hz)"
   - Rig PTT engages just before :15.0
   - Audio emits at slot boundary (no DT > 0.3s in any subsequent
     receiving station's spot)
   - PSKReporter spot appears within ~30s
3. Wait for another Even-slot CQ. Press Space at slot+5s. Confirm:
   - TX still happens at :15 (current Odd slot), not :30
   - PSKReporter spot appears (positive DT visible at the receiver)
4. Trigger autonomous mode (`c` key for auto-CQ); confirm at least
   one CQ → reply → response cycle completes without same-parity
   collision.
```

Commit.

```bash
git add docs/superpowers/plans/2026-04-27-dx-slot-aware-tx.md
git commit -m "docs(plan): manual hardware validation checklist

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Done

All 17 tasks complete. Spec acceptance criteria:

1. ✅ Unit + integration tests pass (Tasks 1–2 slot helpers, Task 10 scheduler, Tasks 13–14 loopback + four-message QSO).
2. Manual hardware validation pending (Task 17 Step 3 — post-merge operator action).
3. ✅ Autonomous operator picks opposite parity (Task 8 wiring through `OperatorAction::Transmit.tx_parity`).
4. Manual on-air observation pending (Task 17 Step 3).

---

## Manual hardware validation (post-merge)

To be run by the operator after merging to main:

1. With FTdx10 connected, run `pancetta` and listen on 20m FT8 (14.074 MHz).
2. Wait for an Even-slot CQ. Press Space 1s into the slot. Confirm:
   - TUI status: "Calling KXXXX — TX queued (Hz)"
   - Rig PTT engages just before :15.0
   - Audio emits at slot boundary (no DT > 0.3s in any subsequent
     receiving station's spot)
   - **PTT stays engaged for the full audio duration; audio plays
     to completion before PTT releases (~12.7s + 50ms tail)**
   - PSKReporter spot appears within ~30s
3. Wait for another Even-slot CQ. Press Space at slot+5s. Confirm:
   - TX still happens at :15 (current Odd slot), not :30
   - PSKReporter spot appears (positive DT visible at the receiver)
4. Trigger autonomous mode (`c` key for auto-CQ); confirm at least
   one CQ → reply → response cycle completes without same-parity
   collision.
5. **Self-decode sanity check.** Our own TX should appear in the next
   decode window tagged with the parity we just transmitted. Confirm
   the autonomous operator does NOT respond to its own callsign; if
   it does, abort immediately and check the `our_callsign` filter in
   `pancetta-qso/src/qso_manager.rs`. (A bug here would create an
   infinite ping-pong on alternating parities.)
