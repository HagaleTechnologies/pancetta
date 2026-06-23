# End-to-End QSO: Design Spec

_2026-04-02. Approach: Vertical Slice — close one complete loop first, then layer sophistication._

---

## Overview

Get pancetta from "verified FT8 engine inside a partially-integrated shell" to "autonomous FT8 station that works real QSOs on the air." Four phases, each delivering something testable.

### Target Rig
- Yaesu FTdx10 connected via USB to a Windows 11 MiniPC
- Pancetta runs on the same machine (or on macOS over the network via rigctld)
- Development uses WAV files + mock rig until Phase 4

### Success Criteria
- Complete a real on-air QSO: decode → call → exchange → log
- Decode at parity with WSJT-X on the same audio
- Autonomous operator with configurable priority-based station selection
- Multi-stream TX: two simultaneous FT8 signals at different audio frequencies
- Full automation: hunt mode, CQ mode, hybrid mode

---

## Phase 1 — First Simulated QSO (Vertical Slice)

### Goal
Prove the entire pipeline works by simulating a complete QSO between two virtual stations — no radio, no audio hardware, pure in-memory.

### The Loopback QSO Test

```
Station A (us):     encode CQ → modulate → WAV buffer
Station B (mock):   decode → sees CQ → encode response → modulate → WAV buffer
Station A:          decode → sees response → auto-sequence next → encode → ...
...continues until QSO completes (CQ → grid → report → RR73 → 73)
```

Exercises: encoder, modulator, decoder, message parser, QSO state machine, auto-sequencing.

### Bug Fixes (Blocking)

1. **`decoder.rs:1130`** — `10.0 * power.log10()` produces `-inf` when `power == 0`. Add epsilon: `10.0 * (power + f32::EPSILON).log10()`.
2. **Critical `unwrap()` calls** in the TX path and QSO state machine — audit and fix the ones that would crash during the loopback test.

### Wiring Work

1. **`start_transmitter_component()`** — Currently a stub that logs. Wire it to:
   - Receive `TransmitRequest` from channel
   - Encode message via `pancetta-ft8` encoder
   - Modulate to audio samples via modulator
   - Send audio to output channel (or to WAV buffer in test mode)

2. **QSO auto-sequence message loop** — The state machine exists in `pancetta-qso`, but the coordinator's message loop that drives it is incomplete. Wire:
   - Decoded message in → state machine transition → TX request out

### Deliverable
`cargo test --test loopback_qso` passes — a full CQ-to-73 exchange between simulated stations, validating every layer.

---

## Phase 2 — Autonomous Operator + Priority Engine

### Goal
Pancetta watches decoded messages and autonomously decides what to call, in what order, based on a configurable priority scoring system.

### Priority Scoring Engine

New module: `pancetta-qso/src/priority.rs` (or a standalone module if it grows).

Each decoded CQ gets scored. The operator calls the highest-scoring station. Scores are computed from weighted factors:

| Factor | Signal | Default Weight |
|--------|--------|----------------|
| Needed DXCC | Entity not in QSO log | Very high |
| Needed grid/state/zone | Missing from log for target award | High |
| POTA/SOTA activator | Callsign pattern (`*/P`, known prefixes) or spot cross-reference | Medium-high |
| Rarity | How often this prefix seen in recent decode cycles | Medium |
| Signal strength | SNR from decoder — stronger = more likely to complete | Low positive |
| Duplicate penalty | Already worked on this band | Strong negative |
| Recency penalty | Called recently, QSO didn't complete | Mild negative (backoff) |

**Design principles:**
- Weights are configurable in `pancetta.toml` under `[autonomous.priorities]`
- The engine is stateless and pure: takes a decoded message + context (QSO log, seen-history) → returns a score
- Easy to unit test: feed synthetic messages, assert correct ordering

### Autonomous Operator Modes

- **Hunt mode** — Monitor and pounce. Score all decoded CQs, call the best one. If nothing above threshold, stay quiet.
- **CQ mode** — Transmit CQ, answer the highest-priority caller. Multiple callers ranked by score.
- **Hybrid** — CQ when band is quiet or no high-value targets; pounce when something good appears mid-CQ.

### Duplicate Suppression
Query the QSO log: worked this callsign on this band? If yes, heavy score penalty. Not a hard block — configurable. You might re-work on a different band or after a time window.

### POTA/SOTA Detection
Start simple: pattern matching on callsign suffixes and common CQ formats. Later, optionally cross-reference with live POTA spot API.

### Coordinator Decomposition (Targeted)
Extract autonomous operator loop and priority engine out of `coordinator.rs` into their own modules. Don't refactor the whole file — just carve out what's needed.

### Deliverables
- Priority scoring engine with unit tests
- Hunt mode working in loopback test (decode CQs → score → pick best → call → complete)
- CQ mode working in loopback test
- Configurable weights in `pancetta.toml`

---

## Phase 3 — Multi-Stream TX

### Goal
Transmit two or more FT8 signals simultaneously at different audio frequencies within the same 15-second slot, enabling parallel QSOs.

### Audio Summing
1. Generate signal A at frequency F1 (e.g., 1000 Hz audio offset)
2. Generate signal B at frequency F2 (e.g., 1500 Hz audio offset)
3. Sum sample buffers: `output[i] = signal_a[i] + signal_b[i]`
4. Normalize to prevent clipping: each signal at `1/N` amplitude for N simultaneous signals

The modulator already generates per-frequency audio. The summing is trivial.

### Dual QSO State Machine
The autonomous operator manages multiple concurrent QSO exchanges. Each "slot" is independent:
- Slot 1: Working K1ABC on 1000 Hz (report exchange phase)
- Slot 2: Working VE3XYZ on 1500 Hz (just sent grid)

Priority engine allocates slots:
- Rare DXCC appears mid-QSO → grab second slot
- Both slots free → pick two highest-scoring targets
- One slot free → use for next best target

### Frequency Management
Each slot claims a 50 Hz range. The frequency picker must:
- Avoid collisions with other stations (check decoded activity in the passband)
- Keep slots ≥100 Hz apart (guard band)
- Prefer frequencies with low noise floor

### Constraints
- Max 2 simultaneous signals by default (configurable)
- Rig must be in USB-D or DATA mode (linear amplification required)
- Power per signal = total power / N (3dB penalty per additional signal)

### Deliverables
- Multi-signal modulator (sum N signals, normalize)
- Frequency slot allocator with collision avoidance
- Concurrent QSO state machine manager
- Loopback test: two simultaneous QSOs complete to 73
- TUI shows both active QSOs

---

## Phase 4 — On-Air Readiness

### Goal
Everything works with real hardware. Launch pancetta, connect to the FTdx10, work real QSOs.

### Audio I/O
- `pancetta-audio` AudioManager already handles CPAL, device enumeration, resampling, ringbuf
- Wire for real: audio input → DSP → decoder; modulated audio → audio output → rig
- TUI audio device selector (DeviceSelectionState) connected to real devices
- Platform priority: macOS CoreAudio first, Windows WASAPI second

### Rig Control
- rigctld over TCP (already implemented in `pancetta-hamlib`)
- FTdx10: rigctld on the machine with USB connection, pancetta connects over network
- PTT: assert 200ms before TX audio, de-assert after TX ends
- Frequency polling: read VFO, display in TUI, log in QSO records
- Mock rig fallback remains for development/testing

### Unwrap Hardening (Targeted)
Not all 680 — the ~50-80 on the critical path:
- TX pipeline (crash during transmit = bad)
- QSO state machine (crash during QSO = lost contact)
- Audio I/O (crash on device error = app dies)
- TUI event loop (crash on render = terminal corruption)

### Decode Parity with WSJT-X
- Current: 3/9 off-air WAV files (ft8_lib also fails on same files — likely data quality)
- Expand off-air test WAV corpus
- Compare SNR detection thresholds against WSJT-X
- Verify OSD-2 is enabled and tuned correctly
- Profile and improve if gaps are found

### TUI Polish
- Waterfall displays real power data
- Band activity shows priority scores alongside decoded messages
- Active QSO panel with state machine status per slot
- TX indicator: what's being sent, on which frequency
- Basic help panel (keyboard shortcuts)

### Deliverables
- Working audio round-trip with real sound card
- rigctld connection to FTdx10 (or any Hamlib-supported rig)
- First real on-air QSO completed and logged
- ADIF export of logged QSOs
- Targeted unwrap hardening on critical paths

---

## Deferred (Explicitly Not In Scope)

- DX cluster integration (beyond what's already stubbed)
- PSK Reporter upload
- Contest mode / Fox-Hound
- Band hopping automation
- DXCC entity highlighting in TUI
- Credential hardening (LoTW zeroize + keyring)
- Full coordinator.rs decomposition (only targeted extraction)
- Full unwrap audit (only critical-path hardening)
- Coverage reporting / tarpaulin
- CI cargo-audit

---

## Key Risks

| Risk | Mitigation |
|------|------------|
| Decode rate below WSJT-X parity | Expand WAV corpus; profile SNR thresholds; OSD tuning |
| Multi-stream TX distortion on rig | Require USB-D/DATA mode; test with real rig in Phase 4 |
| QSO state machine race conditions with dual slots | Each slot is independent; no shared mutable state between them |
| Audio latency on macOS CoreAudio | CPAL buffer size tuning; measure actual latency in Phase 4 |
| Priority engine gaming (always picking same station type) | Backoff timers; rarity decay; configurable weights |
| rigctld network reliability | Health monitoring already exists; reconnect on failure |

---

## Architecture Notes

### Where New Code Lives
- Priority engine: `pancetta-qso/src/priority.rs`
- Autonomous operator extraction: `pancetta/src/autonomous.rs` (extracted from coordinator)
- Multi-signal modulator: `pancetta-ft8/src/modulator.rs` (extend existing)
- Frequency slot allocator: `pancetta/src/frequency.rs`
- Concurrent QSO manager: `pancetta/src/qso_manager.rs`
- Loopback test: `pancetta/tests/loopback_qso.rs`

### Dependencies Between Phases
- Phase 2 depends on Phase 1 (needs working TX + QSO state machine)
- Phase 3 depends on Phase 2 (needs autonomous operator to manage multiple slots)
- Phase 4 depends on Phase 1-3 (hardware integration of everything)
- Within phases, work items are largely sequential
