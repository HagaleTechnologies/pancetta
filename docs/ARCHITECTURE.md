# Pancetta Architecture

Pancetta is an autonomous FT8 ham radio station written as an 11-crate Cargo workspace.
The coordinator orchestrates a real-time pipeline from audio input through FT8 decode,
autonomous decision-making, and transmission — completing full CQ-to-73 QSO exchanges
without operator intervention.

---

## Crate Dependency Graph

```
Layer 0 — no internal deps:
  pancetta-core    — shared types, error handling
  pancetta-audio   — real-time audio I/O (cpal + ringbuf)
  pancetta-ft8     — FT8 encoder/decoder/modulator/OSD
  pancetta-dsp     — DSP pipeline (FFT, filtering, resampling)
  pancetta-tui     — terminal UI (ratatui)
  pancetta-config  — configuration with hot-reload

Layer 1 — depends on core/ft8:
  pancetta-qso     — QSO management, priority scoring, autonomous operator
  pancetta-hamlib  — Hamlib CAT control FFI
  pancetta-dx      — DX hunting, DXCC, PSKReporter
  pancetta-cqdx    — cqdx.io HTTP client, cache, types

Layer 2 — orchestrator:
  pancetta         — coordinator, message bus, runtime (depends on all above)
```

All crates are pure Rust. There is no REST API, Web UI, or mobile layer.

---

## End-to-End Data Flow

```
Audio In (USB codec, 48kHz stereo)
  |
  v
pancetta-audio  (AudioManager, cpal + ringbuf)
  | raw f32 samples via crossbeam channel
  v
pancetta-dsp  (DspPipeline)
  | decimate 4:1 -> 12kHz mono, bandpass filter, 15-sec window extraction
  v
pancetta-ft8  (Ft8Decoder)
  | LDPC decode, OSD, AP injection -> Vec<DecodedMessage>
  v
Coordinator  (pipeline.rs)
  | routes decoded messages
  |------> pancetta-tui  (waterfall, band activity, DX hunter)
  v
pancetta-qso  (AutonomousOperator + PriorityScorer)
  | score stations, pick best, generate response message
  v
pancetta-ft8  (Ft8Encoder)
  | encode -> 8-GFSK modulate -> f32 audio samples
  v
pancetta-audio  -> Audio Out (USB codec)
  |
pancetta-hamlib -> PTT control via rigctld (Yaesu FTdx10)
```

Each FT8 slot is 15 seconds. The pipeline must decode and decide within the slot boundary.
Multi-stream TX is supported: N simultaneous FT8 signals can be encoded into a single slot
at different audio frequencies.

---

## Coordinator

The coordinator lives in `pancetta/src/coordinator/` and is decomposed into submodules:

| File             | Role                                                         |
|------------------|--------------------------------------------------------------|
| `mod.rs`         | `ApplicationCoordinator` struct, startup sequencing          |
| `pipeline.rs`    | audio/DSP/FT8 pipeline setup, crossbeam channel wiring       |
| `components.rs`  | QSO engine, hamlib, cqdx.io component startup                |
| `hamlib.rs`      | rigctld process management and TCP connection                 |
| `health.rs`      | health checks and performance stats                          |
| `shutdown.rs`    | graceful shutdown, task join                                 |
| `wav_playback.rs`| WAV file playback mode for offline testing                   |
| `util.rs`        | shared utilities (linear resampler, etc.)                    |

**Communication model**: crossbeam channels carry point-to-point data (audio samples,
decoded messages, waterfall frames). A `MessageBus` handles broadcast control events
(frequency changes, QSO state transitions, DX spots, health signals).

The core channel topology established in `pipeline.rs`:

```
audio_to_dsp_tx  ->  audio_to_dsp_rx   (Vec<f32>, bounded 100)
dsp_to_ft8_tx    ->  dsp_to_ft8_rx     (Vec<f32>, bounded 2)
ft8_to_tui_tx    ->  ft8_to_tui_rx     (DecodedMessage, unbounded)
waterfall_tx     ->  waterfall_rx       (Vec<Vec<f32>>, unbounded)
```

---

## Key Abstractions

### `WorkedStationLookup` (pancetta-qso)

Trait interface used by `PriorityScorer` for synchronous station queries: duplicate
detection, rarity lookup, and needed DXCC/grid checks. Decouples scoring logic from
the coordinator's data sources.

```
pancetta_qso::priority::WorkedStationLookup
  - is_duplicate(callsign, band) -> bool
  - get_rarity(callsign) -> f64
  - is_needed_dxcc(entity) -> bool
  - is_needed_grid(grid) -> bool
```

### `PriorityScorer` (pancetta-qso)

Takes a slice of `DecodedMessage` plus a `&dyn WorkedStationLookup`, returns a
priority-ranked station list. Scoring weights: needed DXCC > needed grid > POTA/SOTA
> rarity score > general activity. Applies duplicate suppression and failure backoff.
Configured via `pancetta-config`.

### `AutonomousOperator` (pancetta-qso)

Decision engine operating in one of three modes:
- **Hunt**: pounce on rare stations identified by `PriorityScorer`
- **CQ**: call CQ and answer inbound callers by priority
- **Hybrid**: hunt when rare targets are present, CQ otherwise

Manages per-QSO state machines (CALLING -> EXCHANGING -> CONFIRMING -> COMPLETE).
Hands off completed exchanges to the QSO log.

### `SmartFrequencyAllocator` (pancetta-qso)

Selects TX audio frequency for each new QSO using 7 soft-scored criteria:
avoid QRM from active signals, maintain minimum spacing between simultaneous TX streams,
prefer clear channels, align with band segment conventions. Enables parallel QSOs
within a single 15-second slot.

### `CachedStationLookup` (pancetta / priority_evaluator.rs)

Coordinator-level implementation of `WorkedStationLookup`. Holds in-memory snapshots
of worked stations (per band), recent failures, needed DXCC entities, needed grids,
rarity scores from cqdx.io, notable callsigns, and network SNR data. Refreshed
periodically by the coordinator from cqdx.io and the QSO log.

---

## FT8 Protocol Notes

- Slot duration: 15 seconds (TX starts at 0s or 15s boundary)
- Audio passband: ~200–3000 Hz above suppressed carrier
- Modulation: 8-GFSK, 6.25 Hz tone spacing, 12000 samples/sec
- Coding: LDPC (174,87) + 12-bit CRC
- Message types: CQ, directed (call/grid/report/RR73/73), ARRL contest, free-text
- OSD (Ordered Statistics Decoding) extends decode depth beyond standard LDPC

`pancetta-ft8` is bit-exact with ft8_lib and WSJT-X (~200 tests).

---

## Known Gaps

- Grid "needed" set is never populated. cqdx.io has no
  `entities/needed-grids` endpoint yet; `is_needed_grid` returns
  `false` when the local set is empty so the priority weight doesn't
  inflate.
- `is_duplicate` checks callsign + audio frequency proximity within a
  configurable time window, but doesn't yet partition by band — a
  station worked on 20m won't be flagged as a duplicate when worked
  again on 40m. Set `[duplicate_checking].check_frequency = true` for
  band-aware dedup until this is fixed natively.
- cqdx.io `GET /api/v1/spots?live=true` response envelope key (`groups`)
  is unverified against the live API. A gated live test exists:
  `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`.

## Recent Milestones

- **Phase 1** — Loopback QSO. Full CQ-to-73 exchange through the
  encode → modulate → decode pipeline, with state-machine tests.
- **Phase 2** — Autonomous operator + priority engine. Configurable
  weighted scoring, POTA/SOTA detection, hunt/CQ/hybrid modes.
- **Phase 3** — Multi-stream TX. `SmartFrequencyAllocator` selects
  audio frequencies; up to N parallel QSOs in one slot.
- **Phase 4** — Hardware integration (complete, 2026-04-26). hamlib
  CAT control via rigctld short-form commands; first real-rig TX
  validated on a Yaesu FTdx10 with clean ALC and tail-end PSKReporter
  spots across NA + EU.
- **Phase 5** (current) — Full autonomous QSO loop on real hardware:
  CQ → grid → report → RR73 end-to-end without operator intervention.
