# Pancetta Features

## FT8 Decoder

Pancetta implements a complete FT8 decode pipeline: Costas sync pattern search in 2D (time × frequency), complex DFT symbol extraction, soft log-likelihood ratio computation, LDPC belief-propagation decoding, CRC-14 verification, and message parsing. Ordererd-Statistics Decoding (OSD) provides a second-pass recovery layer for frames that survive sync but fail LDPC, and A Priori (AP) decoding exploits known callsign context to push deeper into the noise floor. The decoder achieves >95% decode accuracy at SNR −20 dB and supports 50+ simultaneous decodes per 12.64-second window with a zero-allocation hot path. Correctness is verified against ft8_lib and WSJT-X reference implementations across ~200 tests.

## Autonomous Operator

The autonomous operator makes cycle-by-cycle decisions across three modes: hunt mode (pounce on rare or needed stations calling CQ), CQ mode (call CQ and answer incoming callers), and hybrid mode (balance outbound hunting with CQ activity). It manages even/odd 15-second slot parity, drives the full QSO state machine (CQ → grid report → signal report → RR73 → 73), and monitors the TX slot to detect doubling. All behavior is configurable — mode, aggressiveness, slot preference, and priority weights are set at runtime without code changes.

## Priority Scoring Engine

Each decoded CQ is scored against a weighted set of criteria: needed DXCC entity (35%), needed grid square (20%), POTA/SOTA activation (15%), rarity score (10%), signal strength (5%), with duplicate and recent-failure penalties applied on top. The weights are fully configurable via `PriorityWeights`, making it straightforward to tune for DX chasing, grid hunting, or contest operating. Scoring is stateless and synchronous — all external state (worked stations, recent failures, DXCC/grid needed sets) is injected via the `WorkedStationLookup` trait, keeping the engine pure and unit-testable. Score breakdowns are emitted per candidate for logging and diagnostics.

## Multi-Stream TX

The `SmartFrequencyAllocator` scores candidate TX frequencies across seven soft criteria: spectral noise floor, decoded-activity occupancy, neighbor interference guard, center-of-passband preference, DX proximity window, minimum separation between own simultaneous QSOs, and recent-history weighting. No hard gates are applied — on a crowded band the best-available frequency still gets selected. This enables N simultaneous FT8 signals within a single 15-second slot, each targeting a different station at a different audio offset. The allocator operates over a configurable passband (default 200–2800 Hz) in 25 Hz steps with a 60-second activity history.

## DSP Pipeline

The DSP pipeline connects real-time audio input to the FT8 decoder through a modular, async-friendly processing chain. Incoming 48 kHz stereo audio is decimated to 12 kHz mono using SINC-based resampling, then processed through cascaded biquad IIR bandpass filters tuned to the FT8 passband. Automatic gain control with hang time, compression, and noise gating prevents saturation and keeps signal levels stable across varying band conditions. Spectral subtraction provides adaptive noise reduction, and 12.64-second windows are extracted on FT8 slot boundaries for handoff to the decoder. Pipeline stages emit detailed performance metrics for runtime monitoring.

## Hardware Integration

Hamlib CAT control is integrated via a TCP client to `rigctld`, targeting the Yaesu FTdx10 (model 1042) connected by USB at 38400 baud. The integration covers frequency readback and PTT control, enabling the coordinator to command the rig into transmit at the correct moment and return to receive when the TX window closes. End-to-end TX has been validated on real hardware with clean ALC and tail-end PSKReporter spots across NA + EU. Pancetta refuses to spawn rigctld with suspicious serial-port paths and warns when `RIGCTLD_HOST` points outside loopback. The hamlib crate is tested independently due to tokio runtime constraints (`cargo test -p pancetta-hamlib --lib -- --test-threads=1`).

## Terminal Interface

The TUI is built on ratatui and crossterm. It exposes a live waterfall, a band-activity table of decoded messages, a DX hunter panel sourced from cqdx.io spots, a QSO status pane showing in-flight exchanges, and a station info / pipeline-health panel. Core controls — Space to call the selected station, `c` / `s` to start and stop auto-CQ, `D` for the audio device picker, `Tab` to cycle panels — are wired end-to-end through the coordinator. Audio init failures and QSO state-machine rejections surface in the status bar instead of dying silently in the log file. Density-glyph waterfall rendering keeps the panel visible on 16-color terminals when SSH'd in over slow links.

## cqdx.io Integration

The `pancetta-cqdx` crate provides an HTTP client for cqdx.io, a first-party web service that supplies rarity scoring, needed DXCC and grid lookups, and live spot groups. Responses are cached locally to reduce round-trip latency during rapid decode cycles. Because cqdx.io is developer-owned, custom API endpoints can be built to match exactly what the autonomous operator needs. Live API validation of the spot-group response envelope is pending on-air testing.
