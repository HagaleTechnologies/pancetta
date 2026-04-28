# DX-Slot-Aware TX Scheduling

**Date:** 2026-04-27
**Status:** Spec — pending user review, then implementation plan
**Owner:** K5ARH
**Related:** `pancetta/src/coordinator/tx.rs`, `pancetta/src/coordinator/qso.rs`,
`pancetta-tui/src/tui_runner.rs`, `pancetta-qso/src/qso_manager.rs`

## Problem

Pancetta's TX scheduler picks the next slot boundary at least
`MIN_LEAD = 1s` away (`pancetta/src/coordinator/tx.rs:181`,
`next_audio_start(now, 1s)`) regardless of which 15-second slot the
DX station was on. FT8 stations alternate slots — DX on Even slots
(`:00`, `:30`) means we must transmit on Odd slots (`:15`, `:45`),
and vice versa. The current scheduler doesn't know about parity, so
two failure modes appear:

1. **Same-slot collision.** If the operator hits Space close to the
   slot boundary (e.g., 14.6s into the DX's slot), `next_audio_start`
   skips over the *opposite* slot at `:15` because audio-start at
   `:15.5` is only 0.9s away (< `MIN_LEAD`), and lands at `:30.5` —
   which is the *same* parity as the DX. Result: we transmit on top
   of the DX. The operator either misses the contact or we both lose
   the slot to mutual interference.

2. **Excessive deferral.** Even when we don't collide, the
   conservative `MIN_LEAD = 1s` makes us skip viable slots. The
   operator presses Space, sees the call appear in the queue, then
   waits 30+ seconds for what should have been the next opposite
   slot. This is unnecessary friction and unlike the operator
   experience in WSJT-X.

The bug applies equally to the autonomous operator path — any code
that builds a `TransmitRequest` from a decoded message currently has
no way to enforce opposite-parity TX.

## What WSJT-X Does

WSJT-X's `Modulator/Modulator.cpp` `start()` method handles late
arming via skip-ahead in the audio buffer:

```cpp
unsigned delay_ms = 1000;
if (mode == "FT8" || (mode == "FST4" && m_nsps == 720)) delay_ms = 500;
...
if (synchronize) {
    if (delay_ms > mstr) m_silentFrames = (delay_ms - mstr) * m_frameRate / 1000;
}
// adjust for late starts
if (!m_silentFrames && mstr >= delay_ms) {
    m_ic = (mstr - delay_ms) * m_frameRate / 1000;
}
```

`mstr` is milliseconds into the slot when `start()` runs, `delay_ms`
is the nominal pre-roll (FT8 = 500ms), and `m_ic` is the audio-buffer
read cursor. When the modulator starts late, WSJT-X advances the
cursor forward into the precomputed 12.64s waveform by `(mstr -
delay_ms) * sample_rate` samples and emits the remainder.

This works because FT8 has three Costas sync arrays at symbol
positions 0, 36, and 72 (= 0s, 5.76s, 11.52s into the waveform). A
receiver that only catches the middle and end Costas — i.e., misses
the first ~5s — can still synchronize and decode. Operators routinely
report that arming TX 4–8 seconds late produces a clean decode at the
other end.

WSJT-X also locks parity once when you double-click a CQ: the
"Tx even/1st" toggle latches to the opposite of the heard station's
slot and stays there for every transmission in that QSO.

## Design

Two cooperating mechanisms:

### 1. DX slot parity propagation

A new enum `pancetta_core::slot::SlotParity { Even, Odd }` is
threaded from the decoder through the TUI and the QSO state machine
into the TX scheduler. Every `DecodedMessage` carries the parity of
the slot whose audio produced it. Every `TransmitRequest` carries the
parity the TX must use (which is the *opposite* of the DX slot for
QSO responses, or a configured self-parity for unsolicited CQ).

The QSO state machine **latches** parity once per QSO: at QSO start,
`tx_parity = opposite_of(dx_parity)` is stored in `QsoMetadata`, and
every subsequent `MessageToSend` for that QSO inherits it. This
matches WSJT-X behavior and avoids re-deriving parity from "now"
during long pauses (which would be wrong if the operator gets
distracted across multiple slots).

### 2. WSJT-X-style late-start audio playback

The waveform is still fully pre-modulated up front (no change to
`pancetta-ft8::Ft8Modulator`). What changes is how the TX scheduler
in `pancetta/src/coordinator/tx.rs` *delivers* the modulated samples
to `pancetta-audio`. The scheduler is rewritten around three
branches keyed off `mstr` (ms past the target slot boundary):

- **Early** (`mstr < 500`): pad `(500 - mstr) * SR / 1000` zero
  samples in front of the waveform, emit full 12.64s.
- **Late but viable** (`500 ≤ mstr ≤ TX_LATE_MAX_MS`): skip the
  cursor by `(mstr - 500) * SR / 1000` samples, emit the remainder.
  Receiver decodes via middle/end Costas. Default `TX_LATE_MAX_MS =
  8000`, configurable.
- **Too late** (`mstr > TX_LATE_MAX_MS`): defer to the *next*
  opposite-parity slot (15s + 15s = 30s away).

`MIN_LEAD = 1s` is removed. The new minimum lead is `PTT_LEAD ≈
80ms` — enough to engage PTT before audio. Operators with slow PTT
relays can override via config.

## Components

| Layer | File | Change |
|---|---|---|
| Slot helpers | `pancetta-core/src/slot.rs` | New `SlotParity { Even, Odd }`. New `slot_parity(slot_start: DateTime<Utc>) -> SlotParity` (computed as `(timestamp_secs / 15) % 2`). New `next_slot_with_parity(now, parity) -> DateTime<Utc>`. |
| Decoded message | `pancetta-ft8/src/message.rs` | Add `slot_parity: Option<SlotParity>` to `DecodedMessage`. `Option` to keep existing tests / callers compatible; the coordinator path always sets it. |
| Decoder dispatch | `pancetta/src/coordinator/ft8.rs` | At each window receive, compute the slot start corresponding to the audio (= `next_slot_start(now) - SLOT_NS` since the decoder runs after the slot ends) and stamp `slot_parity` on every `DecodedMessage` before forwarding. |
| TUI view | `pancetta-tui::DecodedMessageView` | Add `slot_parity: Option<SlotParity>`. Populated in `tui_relay.rs` from the underlying `DecodedMessage`. |
| TUI selector | `pancetta-tui::App::get_selected_station` | Return `(callsign, audio_hz, Option<SlotParity>)`. Both `BandActivity` and `DxHunter` paths populate from the latest decoded message for the callsign. `DxHunter` may have `None` (DX cluster spots don't carry slot info). |
| TUI command | `pancetta-tui::TuiCommand::CallStation` | Add `dx_parity: Option<SlotParity>`. |
| QSO message bus | `pancetta::message_bus::QsoMessage::StartQso` | Add `dx_parity: Option<SlotParity>`. |
| QSO manager | `pancetta-qso::QsoManager::respond_to_cq` | Accept `dx_parity: Option<SlotParity>`. Compute `tx_parity = dx_parity.map(opposite)` and store in `QsoMetadata.tx_parity`. |
| QSO state events | `pancetta-qso::QsoEvent::MessageToSend` | Add `tx_parity: Option<SlotParity>` (read from the QSO metadata at emit time). |
| TX request | `pancetta::message_bus::MessageType::{TransmitRequest, MultiTransmitRequest}` | Add `tx_parity: Option<SlotParity>`. `MultiTransmitRequest` carries a single parity for the bundle. |
| Autonomous (coord) | `pancetta/src/coordinator/autonomous.rs` | Read `slot_parity` from the `DecodedMessage` it reacts to; thread `dx_parity` into the `TransmitRequest` it builds. |
| Autonomous (qso) | `pancetta-qso/src/autonomous.rs` | When the operator transitions a QSO, the latched `tx_parity` from `QsoMetadata` propagates automatically. No new logic — just consume the new field on the event. |
| TX scheduler | `pancetta/src/coordinator/tx.rs` | Replace `next_audio_start(now, MIN_LEAD)` with the early/late/skip branches described above. New helper `schedule_tx(now, required_parity, tx_late_max_ms) -> TxSchedule { target_slot, silent_pad_samples, cursor_offset_samples }`. Both `TransmitRequest` and `MultiTransmitRequest` paths share the helper. Drop `MIN_LEAD = 1s`. Reduce `PTT_LEAD` from 200ms to 80ms (configurable). |
| Config | `pancetta-config/src/station.rs` (or appropriate station config struct) | New `tx_late_max_ms: u64` (default 8000), `tx_self_parity: TxSelfParity { Auto, Even, Odd }` (default `Auto`), `ptt_lead_ms: u64` (default 80). |

## Data Flow

```text
Audio @ slot N
        |
        v
DSP slot-aligned window
        |
        v
ft8.rs decoder thread
        |  computes slot_parity from "now()"
        v
DecodedMessage{slot_parity: Even, ...}
        |
        +---> TUI band activity (DecodedMessageView carries parity)
        |
        +---> QSO component (AP context, dup detection — unchanged)
        |
        +---> Autonomous: tx_parity = Odd for any TX in response

User presses Space (DX heard on Even)
        |
        v
TuiCommand::CallStation { callsign, audio_hz, dx_parity: Even }
        v
QsoMessage::StartQso { callsign, frequency, dx_parity: Even }
        v
QsoManager::respond_to_cq → metadata.tx_parity = Odd (latched)
        v
QsoEvent::MessageToSend { tx_parity: Odd, ... }
        v
TransmitRequest { tx_parity: Odd, ... }
        v
tx.rs scheduler:
    target_slot = next_slot_with_parity(now, Odd)
    mstr = max(0, (now - target_slot).ms)
    if mstr < 500:
        silent_pad = (500 - mstr) * 12 samples
        cursor = 0
    else if mstr <= 8000:
        silent_pad = 0
        cursor = (mstr - 500) * 12 samples
    else:
        target_slot = next_slot_with_parity(target_slot + 1ms, Odd)
        recompute (will hit early branch)
    PTT @ target_slot - 80ms
    Emit silent_pad zeros, then waveform[cursor..end]
```

For the autonomous path, the only difference is the entry point: a
decoded message with `slot_parity: Even` is consumed by the operator,
which builds a `TransmitRequest { tx_parity: Odd, ... }` and the
scheduler does the same thing.

For unsolicited CQ (no DX heard), `tx_parity = None` on the request.
The scheduler reads `tx_self_parity` from config: `Even` or `Odd`
fixes one; `Auto` picks whichever next slot is closer (i.e., the
literal next slot regardless of parity). When a station then
*answers* our CQ, the new QSO is created with the answerer's slot
as `dx_parity`, and the latch from then on is `opposite_of(dx_parity)`
— even if that disagrees with the parity our CQ went out on. This
matches WSJT-X: the TX-even toggle is updated on first reply.

## Edge Cases

- **CQ when no DX context exists.** `tx_parity = None` →
  `tx_self_parity` fallback. `Auto` reproduces today's behavior of
  taking the next available slot regardless of parity, but with the
  new compressed `MIN_LEAD` (so we don't over-defer).

- **Multi-stream TX bundle.** `MultiTransmitRequest` has a single
  `tx_parity` for the bundle because a slot is physically one slot.
  This is consistent with how multi-stream is used: we received N
  replies to our CQ in their (opposite-of-ours) slot, and we answer
  all N of them in our next own-parity slot — so the bundle naturally
  shares one parity. The bundle's `tx_parity` is set by the QSO
  state machine of whichever active QSO triggers the multi-TX (they
  all latched to the same value when they were created in response
  to our CQ).

- **Late-press bigger than `TX_LATE_MAX_MS`.** Defer 30s to the next
  opposite-parity slot. Operator sees a status message
  ("TX deferred — too late for current slot, transmitting at
  HH:MM:SS"). This matches the user's "wait 2 windows" alternative.

- **DX cluster spots (no `slot_parity`).** DX Hunter spots come from
  the cluster, not from on-air decodes; they have no slot parity.
  `dx_parity = None` flows through, so calling a clustered DX
  behaves the same as CQing — `tx_self_parity` fallback. Acceptable
  because the operator hasn't actually heard the station yet on this
  pass; the first received decode will refine the parity for any
  subsequent QSO responses.

- **PTT engage time variance.** `ptt_lead_ms` is configurable in
  case 80ms isn't enough on a slow mechanical relay. Documented in
  config.

- **System clock drift.** Out of scope here — the existing slot
  helpers already require NTP-synced clocks within ±1s, documented
  in `README.md`.

## Testing

### Unit tests (`pancetta-core::slot`)

- `slot_parity_even_at_boundary`: parity at `:00` and `:30` is `Even`.
- `slot_parity_odd_at_boundary`: parity at `:15` and `:45` is `Odd`.
- `next_slot_with_parity_skips_same_parity`: from `now=:05`, next
  `Odd` is `:15`; from `now=:20`, next `Odd` is `:45`.
- `next_slot_with_parity_at_boundary_advances`: from `now=:15.000`,
  next `Odd` is `:45` (current slot has already started).
- `slot_parity_opposite_invariant`: `opposite(Even) == Odd` and
  vice versa; idempotent under double-flip.

### Unit tests (`tx.rs` scheduler)

- `schedule_tx_early_pads_silent`: `now=:05.0`, parity `Odd` →
  `target=:15`, `silent_pad = 500ms`, `cursor=0`.
- `schedule_tx_on_time_no_pad_no_skip`: `now=:15.500`, parity `Odd`
  → `target=:15`, `silent_pad=0`, `cursor=0`.
- `schedule_tx_late_skips_cursor`: `now=:20.0`, parity `Odd` →
  `target=:15`, `silent_pad=0`, `cursor=4500ms*SR/1000`.
- `schedule_tx_too_late_defers`: `now=:24.5`, parity `Odd`,
  `tx_late_max_ms=8000` → defer to `:45`, hits early branch.
- `schedule_tx_collision_avoidance`: `now=:14.6`, DX parity `Even`,
  required `Odd` → target `:15`, **never** `:30` (this is the bug
  regression test).

### Integration tests

- Extend `pancetta/tests/loopback_qso.rs` with two new scenarios:
  1. **Late-press case.** Drive the QSO through `respond_to_cq` with
     a simulated press time of slot+5s. Assert the `TransmitRequest`
     is scheduled for the opposite parity, that audio emit happens
     within the *current* opposite slot (not 30s later), and that
     the loopback decoder recovers the message text.
  2. **Same-parity collision avoidance.** Press at slot+14.6s with
     DX on Even parity. Assert TX fires at `:15`, not `:30`.

- New end-to-end test in `pancetta-qso/tests/`: simulate a 4-message
  QSO (CQ → response → report → RR73) and assert that all four
  outbound TXs land on the same parity once latched.

### Manual hardware validation

Once integrated, the standard FTdx10 PSKReporter check:

1. Hear a DX station on Even. Press Space. Confirm TX fires on Odd
   (visible in the rig's status / waterfall).
2. Press Space at slot+5s on the next decode. Confirm we still
   transmit *this* slot via skip-ahead, audio decodes elsewhere
   (PSKReporter spot within ~30s), no TX into DX's slot.
3. With autonomous operator enabled, run for one slot and confirm
   it picks the opposite parity for any response it sends.

## Configuration Defaults

```toml
[station]
# Maximum latency past the slot boundary at which we still attempt
# skip-ahead TX. Beyond this, defer to the next opposite-parity slot.
# 8s leaves ~5s of audio on air, with two of three Costas sync
# arrays still in window.
tx_late_max_ms = 8000

# When calling CQ (no DX context), pick this parity.
# "auto" = whichever next slot is closer; explicit value = lock it.
tx_self_parity = "auto"

# PTT engage lead time before slot boundary. Drop from the prior
# 200ms (used as part of the 1s safety margin we are removing).
# Bump up if PTT relay is mechanically slow.
ptt_lead_ms = 80
```

## Out of Scope

- Decoder-side improvements to handle weak signals or multi-pass
  subtraction (covered in `2026-03-30-wsjt-x-parity-design.md`,
  `2026-04-18-decoder-sensitivity-design.md`).
- TX cancellation / re-arming UI (currently no in-flight TX cancel;
  Phase 5 work).
- Slot-parity persistence across pancetta restarts (parity is
  derived freshly per-decode; no need to persist).
- Hardware PTT keying paths beyond hamlib (no RTS/DTR / GPIO changes).

## Acceptance

The fix is complete when:

1. The unit and integration tests above pass.
2. Manual hardware validation passes: a Space-press at slot+5s
   produces a real on-air TX *that same slot* and is decoded by
   another station (PSKReporter spot or local SDR confirmation).
3. The autonomous operator picks the opposite parity for every
   response it generates from a decoded message.
4. Default config produces zero same-parity-as-DX collisions over a
   30-minute on-air session with the FTdx10 (manual observation).
