# Phase 3: Multi-Stream TX — Design Spec

> **Status:** Approved
> **Date:** 2026-04-03
> **Depends on:** Phase 2 (autonomous operator + priority engine) complete
> **Related:** `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md` Phase 3

## Goal

Enable pancetta to transmit N simultaneous FT8 signals at different audio frequencies within the same 15-second slot, allowing parallel QSOs. Default N=2, configurable up to ~5 (limited by RF power budget, not software).

## What Already Exists

The codebase has significant multi-stream infrastructure from Phase 2:

| Component | Status | Notes |
|-----------|--------|-------|
| `modulate_multi_tx()` | Working | Sums N signals, normalizes to 95% headroom, enforces guard bands |
| `FrequencyAllocator` | Working | Tracks per-QSO offsets, center-outward search, min 75 Hz separation |
| `MultiTransmitRequest` | Working | Message bus type for bundled TX |
| Coordinator bundling | Working | Collects `Vec<OperatorAction::Transmit>`, bundles into one TX |
| `max_concurrent_qsos` config | Exists | Set to 1, decision logic assumes single QSO |
| `SlotParityConfig` | Working | Enforces same time slice for all TX (Even/Odd/Auto) |
| Auto sequencer | Working | Per-QSO event-driven, independent state machines |

**What needs building:**
1. Smart frequency allocator with noise/occupancy awareness
2. Multi-slot decision logic in the autonomous operator
3. Spectral data routing to the frequency allocator
4. Multi-stream loopback test

## Architecture

```
Decode cycle produces:
  ├─ Vec<DecodedMessageInfo>  (frequency offsets, SNR, time slot)
  └─ WaterfallData            (power matrix, ~19 Hz bins)
        │
        ▼
  SmartFrequencyAllocator     (new: pancetta-qso/src/frequency.rs)
  │  Inputs: spectral data, decode history, own frequencies, DX target
  │  Output: ranked FrequencyCandidates
  │
  ▼
  AutonomousOperator          (modified: multi-slot decision logic)
  │  Opens slots when score ≥ min_multi_slot_score
  │  Each slot gets best available frequency from allocator
  │
  ▼
  Vec<OperatorAction::Transmit>  (one per active QSO)
  │
  ▼
  Coordinator bundling        (existing, no changes)
  │
  ▼
  modulate_multi_tx()         (existing, no changes)
  │
  ▼
  Summed audio output         (single buffer, all signals combined)
```

All active slots transmit in the same time slice. The radio cannot RX and TX simultaneously, so all QSOs share the same parity (first or second 15-second slot).

---

## 1. Smart Frequency Allocator

New module: `pancetta-qso/src/frequency.rs`

### Design

Stateless and pure. Given the current spectral snapshot and decode history, returns a ranked list of frequency candidates. The autonomous operator picks the top candidate.

All criteria are soft-scored with weights — no hard gates. On a crowded band the best candidate may score low, but it's still the best available. The allocator always returns candidates; the operator always picks the best one.

### Inputs

- **Spectral data:** Recent WaterfallData power matrices (last 2–4 cycles, covering both time slots). Provides per-bin noise floor at ~19 Hz resolution.
- **Decode history:** Rolling buffer of decoded messages from the last N cycles (default 4, ~60s) with frequency offset and time slot (first/second). Provides occupancy map.
- **Own active frequencies:** Offsets currently in use by our QSOs.
- **DX target offset (optional):** The frequency of the station we're trying to call.

### Scoring Criteria (descending weight)

1. **Clear in both time slots** — No decoded activity within ±50 Hz in either slot in recent cycles. Strong positive weight.
2. **Low noise floor** — Average power in the ±25 Hz region from WaterfallData. Lower = better.
3. **No noisy neighbors** — No strong signals (peaks in power matrix) within ±100 Hz.
4. **No recent activity** — No decodes at this offset in the last N cycles.
5. **Center bias** — Prefer offsets near passband center (~1500 Hz). Score decays toward edges.
6. **DX proximity bias** — When hunting, prefer offsets ±50–200 Hz from the DX station's TX offset. Same offset is usable but least preferred within the proximity range (their clock may be off).
7. **Own-frequency separation** — ≥75 Hz from any of our own active QSO offsets. Strong negative weight if violated.

### Output

```rust
pub struct FrequencyCandidate {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_both_slots: bool,
    pub noise_floor_db: f64,
}
```

Returns `Vec<FrequencyCandidate>` sorted by score descending.

### Replaces

The existing `FrequencyAllocator` in `autonomous.rs` (geometric center-outward search with no spectral awareness). The new allocator subsumes its responsibilities.

---

## 2. Multi-Slot Decision Logic

Changes to `AutonomousOperator` in `pancetta-qso/src/autonomous.rs`.

### New Config Fields

```toml
[autonomous]
max_concurrent_qsos = 2          # Default 2, practical cap ~5
min_multi_slot_score = 0.7       # Second+ slot only opens above this threshold
```

- Setting `min_multi_slot_score` high → conservative (only rare DX gets a second slot)
- Setting it to 0 → aggressive (always fill available slots)

### Decision Flow

1. **First slot** follows existing logic: best target above `min_dx_score`.
2. **Additional slots:** Only if `active_qsos < max_concurrent_qsos` AND next-best candidate scores ≥ `min_multi_slot_score`.
3. Each new slot gets its own frequency from the smart allocator.
4. Each slot's QSO runs independently through the auto sequencer.

### Slot Lifecycle

1. Operator sees high-scoring target → asks `SmartFrequencyAllocator` for best offset → opens slot.
2. Auto sequencer drives that QSO's message exchange at the allocated offset.
3. On QSO completion or failure → slot released, frequency freed.

### No Cross-Slot Dependencies

Each QSO is fully independent: different frequency, different state machine instance, different callsign. The only shared resource is the audio output buffer (handled by existing `modulate_multi_tx` summing and normalization).

---

## 3. Spectral Data Plumbing

Route existing data to the frequency allocator. No new DSP work.

### Decode History Tracking

The autonomous operator already receives `Vec<DecodedMessageInfo>` per cycle with `frequency_hz`. Add a rolling buffer of the last N cycles (default 4) annotated with which time slot (first/second) each decode was in. This is a few hundred entries max.

### Noise Floor from WaterfallData

WaterfallData already flows through the message bus to the TUI. Route it also to the autonomous operator (or directly to the frequency allocator). The power matrix at ~19 Hz resolution is sufficient for noise floor estimation per frequency region. No changes to the decoder or DSP pipeline.

---

## 4. Modulator & Coordinator

**No changes needed.** The existing infrastructure handles multi-stream TX:

- `modulate_multi_tx()` sums N signals, normalizes, enforces guard bands
- Coordinator bundling collects multiple `OperatorAction::Transmit` and creates `MultiTransmitRequest`
- Transmitter component calls `modulate_multi_tx()` for multi-item requests

**One addition:** Log/event emission when the operator opens a new slot, including allocated frequency and target callsign. Important for debugging multi-QSO behavior without a TUI.

---

## 5. Configuration

All new config lives under `[autonomous]`:

```toml
[autonomous]
max_concurrent_qsos = 2          # Max simultaneous QSOs (default 2)
min_multi_slot_score = 0.7       # Threshold for opening additional slots

[autonomous.frequency]
decode_history_cycles = 4        # How many recent cycles to consider (default 4, ~60s)
center_bias_hz = 1500.0          # Center of passband preference (default 1500)
dx_proximity_min_hz = 50.0       # Minimum offset from DX station (default 50)
dx_proximity_max_hz = 200.0      # Maximum preferred offset from DX station (default 200)
min_separation_hz = 75.0         # Minimum separation between own QSOs (default 75)
neighbor_guard_hz = 100.0        # Avoid strong signals within this range (default 100)
```

---

## 6. Testing

### Unit Tests: Smart Frequency Allocator
- Synthetic spectral data + decode history → verify scoring priorities
- Center bias, noise avoidance, DX proximity, neighbor avoidance
- Crowded band → still returns candidates (lower scores)
- Own-frequency separation enforced
- Empty band → picks center

### Unit Tests: Multi-Slot Decision Logic
- Score below `min_multi_slot_score` → no second slot opened
- Score above threshold + slot available → second slot opened
- `max_concurrent_qsos` reached → no more slots
- Slot released on QSO completion → slot available again

### Integration: Multi-Stream Loopback Test
- Two mock DX stations CQ'ing at different frequency offsets
- Pancetta decodes both, scores both above threshold, opens two slots
- Both QSOs run to completion (CQ → grid → report → RR73 → 73) simultaneously
- Verifies: encoding, multi-stream modulation, decoding both signals from summed audio, independent state machines, frequency separation

### What's NOT Tested in Phase 3
- Real audio hardware (Phase 4)
- TUI display of multi-QSO state (Phase 4)
- Real rig behavior with multi-tone TX (Phase 4)

---

## 7. File Changes

| File | Change |
|------|--------|
| `pancetta-qso/src/frequency.rs` | **New** — Smart frequency allocator |
| `pancetta-qso/src/lib.rs` | Add `pub mod frequency` |
| `pancetta-qso/src/autonomous.rs` | Multi-slot decision logic, decode history buffer, wire smart allocator |
| `pancetta-config/src/autonomous.rs` | New config fields (`min_multi_slot_score`, frequency sub-section) |
| `pancetta/src/coordinator.rs` | Route WaterfallData to autonomous operator, slot open/close logging |
| `pancetta/tests/loopback_qso.rs` | New multi-stream loopback test |

No changes to: modulator, transmitter component, message bus, auto sequencer, QSO state machine, DSP pipeline.

---

## 8. What This Does NOT Include

- TUI changes (Phase 4)
- Real hardware testing (Phase 4)
- Mixed-protocol multi-TX (e.g., FT8 + FT4 simultaneously)
- Per-stream power control (all streams share equal power)
- Band hopping between active QSOs
- Contest/Fox-Hound multi-stream behavior
