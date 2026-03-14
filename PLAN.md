# Pancetta Implementation Plan

_Created 2026-03-13. Execution order: Phase 0B → Phase 1A + Phase 1B in parallel → Phase 2A → Phase 2B → Phase 3A → Phase 3B → Phase 4A → Phase 4B → Phase 5B._

---

## Plan A: FT8 DSP Improvements

### Phase 1A — Decoder Sensitivity (highest impact) — DONE (2026-03-14)
- [x] **1a** Add LLR normalization (variance=24.0, matching ft8_lib)
- [x] **1b** Add frequency oversampling (freq_osr=2, double FFT size)
- [x] **1c** Improve Costas sync scoring — dB neighbor-comparison (ft8_lib style)
- [ ] **1d** Successive decoding with interference cancellation — deferred
- [x] **1e** WAV cross-validation assertions — assert we decode ≥80% of what ft8_lib decodes

### Phase 2A — Contest Messages (independent of Phase 1A) — DONE (2026-03-14)
- [x] **2a** i3=4 nonstandard callsign decode (58-bit base-38 + 12-bit hash)
- [x] **2b** i3=3 ARRL RTTY Roundup decode (basic)
- [x] **2c** i3=0 n3=5 telemetry decode (18 hex digits)
- [x] **2c'** Hash table rewritten to use ft8_lib algorithm (base-38 × magic constant)
- [ ] **2c''** i3=0 sub-types: DXpedition (n3=1), EU VHF (n3=2), Field Day (n3=3,4) — deferred
- [ ] **2d** Contest message encoding (after decode is verified) — deferred

### Phase 3A — Performance (after Phase 1A stabilizes) — PARTIAL (2026-03-14)
- [x] **3a** Baseline existing benchmarks (186ms synthetic, 13.7s real off-air in release)
- [x] **3b** Add real-signal benchmark test, assert <2x real-time (25.28s)
- [ ] **3c** Profile hot spots (flamegraph) — deferred
- [ ] **3d** Optimize LDPC/candidate pruning to hit real-time (<12.64s) — deferred

### Phase 4A — GFSK Modulation (independent, low priority) — DONE (2026-03-14)
- [x] **4a** Gaussian pulse shaping filter (BT=2.0)
- [x] **4b** Apply to frequency trajectory, add `PulseShape` enum
- [x] **4c** Validate: ft8_lib decodes our GFSK, round-trip tests pass

---

## Plan B: Application Layer Buildout

### Phase 0B — Stabilize Foundation — DONE (2026-03-13)
- [x] Fix `pancetta-core` test compilation (mode_v2 import paths, PancettaError Clone)
- [x] Fix `pancetta-dx` chrono API (`use chrono::Timelike`), geography geodesic, tracker/statistics
- [x] Fix `pancetta-hamlib` examples, stubs for missing libhamlib
- [x] Fix `pancetta-audio` test assertions (48kHz vs 12kHz defaults)
- [x] Fix `pancetta-config` loader tests, hot_reload tokio runtime handle
- [x] Fix `pancetta-qso` async logger, database schema, statistics, exchange
- [x] Fix `pancetta` runtime tests (nested tokio runtime), CLI test case sensitivity
- [x] 385 tests passing, 0 failures across 10 crates

### Phase 1B — RX Pipeline (Audio In → Decode → Display) — DONE (2026-03-14)
- [x] Fix message bus routing (switch to point-to-point channels)
- [x] Wire decoded messages → TUI band activity panel
- [x] Add `--wav <file>` playback mode for testing without a radio
- [x] Fully integrate TUI main loop (raw mode, event polling, rendering)
- [ ] Implement FT8 15-second timing cycle synchronization — deferred to Phase 2B

### Phase 2B — TX Pipeline (Encode → Modulate → Audio Out) — DONE (2026-03-14)
- [x] Enable `transmit` feature by default in main binary
- [x] Wire TUI → coordinator → encoder → modulator (generates audio samples)
- [x] Implement TUI message input buffer (13 char, uppercase, backspace)
- [x] Wire TUI SendMessage → coordinator TransmitRequest → encode + modulate
- [ ] TX timing (align to slot boundaries) — deferred (needs NTP/system clock work)
- [ ] PTT control via hamlib — deferred (needs hardware)
- [ ] Audio output routing through `AudioManager` — deferred (needs audio output device)

### Phase 3B — QSO Management (parallel with Phase 2B) — DONE (2026-03-14)
- [x] Connect `pancetta-qso` to coordinator (decoded msgs in, TX requests out)
- [ ] Wire auto-sequencing (CQ → grid → report → RR73 → 73) — deferred (auto-sequencer ~60% implemented)
- [x] Click-to-call from TUI band activity
- [x] SQLite database init at `~/.pancetta/qso.db`, auto-logging
- [x] ADIF export, duplicate detection (already implemented in pancetta-qso crate)

### Phase 4B — Configuration & Polish — PARTIAL (2026-03-14)
- [x] First-run setup wizard (callsign, grid, power; saves to ~/.pancetta/pancetta.toml)
- [x] Hot-reload config via file watcher (already implemented in pancetta-config)
- [ ] Audio device selection UI — deferred (needs cpal device enumeration)
- [ ] Error recovery (device disconnect, component crash restart) — deferred
- [ ] Real waterfall display (scrolling spectrogram) — deferred
- [x] Logging to file with rotation (daily rotation to ~/.pancetta/logs/)

### Phase 5B — Advanced Features — PARTIAL (2026-03-14)
- [x] Hamlib rig control (rigctld TCP, frequency polling, PTT, mock rig fallback)
- [x] DX cluster integration (spot monitoring, forwarding to TUI)
- [ ] DXCC highlighting — deferred (needs entity database)
- [x] PSK Reporter spot upload (UDP binary IPFIX protocol, batched every 5 min)
- [ ] Band hopping runtime loop — deferred (config + strategies exist, needs coordinator integration)
- [ ] Contest mode — deferred

---

## Execution Order

```
1. Phase 0B  (stabilize workspace)
2. Phase 1A  (decoder sensitivity)  ←─ parallel ─→  Phase 1B (RX pipeline)
3. Phase 2A  (contest messages)
4. Phase 2B  (TX pipeline)
5. Phase 3A  (performance)
6. Phase 3B  (QSO management)
7. Phase 4A  (GFSK modulation)
8. Phase 4B  (configuration & polish)
9. Phase 5B  (advanced features)
```

## Key Risks
- macOS audio latency (cpal quirks with buffer size control)
- FT8 timing precision (<100ms to slot boundaries, depends on NTP)
- Message bus backpressure (unbounded channels can grow if consumer falls behind)
- SQLite in async context (needs spawn_blocking)
- Cross-platform audio device quirks (ALSA/PulseAudio/WASAPI/CoreAudio)
