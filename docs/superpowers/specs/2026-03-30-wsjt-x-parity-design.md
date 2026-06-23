# Pancetta: WSJT-X FT8 Parity Design

**Date:** 2026-03-30
**Goal:** Make Pancetta a WSJT-X-competitive FT8 application on macOS, via hamlib/rigctld.

## Context

Pancetta has a working FT8 encoder/decoder (bit-exact with ft8_lib, 200 tests passing, decodes real off-air signals) and a functional TUI with waterfall display. However, the decoder's real-world decode rate is unmeasured against WSJT-X, the TX pipeline isn't wired end-to-end, and the codebase has accumulated half-baked features (FT2, multi-TX, contest logging, DX cluster) that dilute focus.

### Decision

- **Primary goal:** WSJT-X parity for FT8 RX+TX on macOS
- **Approach:** Decoder-first, then TX pipeline, then TUI polish
- **Non-parity features:** Feature-gate behind `cfg` flags, don't maintain or test
- **Platform:** macOS first, preserve cross-platform ability for later
- **Radio interface:** Hamlib/rigctld

## Phase 1: Decoder Benchmarking Harness

Before improving anything, measure the gap quantitatively.

### Deliverables

- CLI subcommand `pancetta benchmark-decode` that takes a WAV file or directory, runs Pancetta's decoder, and outputs structured results: each decode with frequency, time offset, SNR, and message text
- Companion script that runs WSJT-X's `jt9` CLI on the same WAVs and captures output in the same format
- Comparison tool that diffs the two result sets and reports: total decodes, overlap (both decoded), Pancetta-only, WSJT-X-only, with SNR distribution for misses

### Test Corpus

- Record 20-30 real off-air 15-second windows across different bands and conditions (quiet band, crowded band, weak signals, strong signals, QRM)
- Store as WAV fixtures in the repo (~370KB each at 12kHz mono)
- This becomes the regression suite — every decoder change gets re-benchmarked

### Success Metric

A single number: "Pancetta decodes X% of what WSJT-X decodes on the same audio." Target: >= 95%.

## Phase 2: Multi-Pass Signal Subtraction

The biggest decode count improvement. WSJT-X does 3 passes by default.

### Algorithm

1. **Pass 1:** Decode the spectrogram normally — strong signals decode first
2. **Subtract:** For each decoded message, reconstruct its expected signal using the encoder + modulator, estimate its amplitude/phase/frequency, and subtract from the spectrogram
3. **Pass 2:** Re-run sync search + decode on the cleaned spectrogram — weaker signals masked by strong ones now emerge
4. **Pass 3:** Repeat for the weakest signals
5. Deduplicate across passes

### Implementation Notes

- Signal subtraction code already exists in the decoder — needs validation, testing with real signals, and wiring into a proper multi-pass loop
- Amplitude/phase estimation must be accurate or subtraction creates artifacts that reduce decodes
- Frequency estimation needs sub-bin precision for clean subtraction (ties into Phase 4)
- Pass count should be configurable, default to 3
- Diminishing returns beyond 3 passes

### Validation

Run benchmark harness before and after on crowded band recordings. Expect significant jump in decode count, particularly for weaker signals.

## Phase 3: OSD (Ordered Statistics Decoding) Fallback

When LDPC belief propagation fails to converge, OSD can rescue weak signals — roughly +2 dB of sensitivity.

### Algorithm

1. LDPC BP runs first (fast, handles most signals)
2. If BP fails after max iterations, hand soft LLR values to OSD
3. OSD sorts bits by reliability (most reliable first), Gaussian-eliminates the generator matrix to match that ordering, then tries flipping least reliable bits in systematic combinations
4. If a valid codeword is found (passes CRC-14), accept the decode

### Depth Levels

- **OSD-0:** Solve the most-reliable-first system, check CRC. Cheap, catches some failures.
- **OSD-1:** Flip each of the k least reliable bits one at a time. ~91 checks. Good cost/gain tradeoff.
- **OSD-2:** Flip pairs. ~4,000 checks. WSJT-X uses this in deep search mode.
- Default to OSD-1. Make depth configurable.

### Implementation

- New module: `pancetta-ft8/src/osd.rs`
- Called from decoder after BP failure
- Interface: takes LLR array + generator matrix, returns decoded bits or failure
- Requires Gaussian elimination on a 91x174 binary matrix per candidate — bounded cost since it only runs on BP failures

### Validation

On benchmark corpus, focus on weak-signal decodes (SNR < -15 dB). OSD should recover signals that BP alone missed.

## Phase 4: Fine Frequency and Time Estimation

Sub-bin interpolation for both frequency and time offset.

### Frequency Refinement

- After Costas sync identifies a candidate at bin f0, evaluate sync score at fractional offsets (e.g., f0-0.5, f0-0.25, f0, f0+0.25, f0+0.5) using interpolated DFT
- Better frequency estimate leads to cleaner tone extraction, better LLRs, more LDPC convergences
- Directly improves signal subtraction quality (Phase 2 depends on this)

### Time Refinement

- Refine from sample-level to sub-sample precision by evaluating sync correlation at fractional sample offsets
- At 12 kHz sample rate, half-sample is ~42 us, which matters for the 0.16s symbol period
- Parabolic interpolation on sync scores around the peak

### Implementation

- Add `refine_candidate()` function: takes coarse (f, t) candidate, returns refined (f_fine, t_fine)
- Called after Costas sync, before symbol extraction
- Infrastructure (DFT, sync scoring) already exists

### Validation

Subtle improvement — measure via benchmark harness, focus on the "WSJT-X decoded, Pancetta didn't" bucket shrinking. Also validates indirectly through improved signal subtraction.

## Phase 5: TX Pipeline End-to-End

Wire existing pieces into a working transmit chain.

### What Exists

- Encoder (bit-exact) and Modulator (8-CPFSK) produce correct audio samples
- Hamlib crate exists (has tokio runtime conflict to fix)
- QSO state machine and auto-sequencing exist in pancetta-qso

### Work Items

1. **Fix hamlib crate** — resolve tokio runtime conflict so rigctld communication works
2. **Audio output path** — send modulated samples to configured output device via CPAL, timed to correct 15-second slot boundary
3. **PTT control** — hamlib PTT on before audio, PTT off after, with configurable lead-in/tail delays for relay settling
4. **TX sequencing in TUI** — user selects a CQ in band activity, TX message sequence auto-fills, Enable TX button, app handles exchange, user can halt
5. **TX/RX switching** — don't decode while transmitting, resume RX after TX slot ends
6. **Safety** — TX timeout (never transmit more than one 15-second slot without explicit re-trigger), power/frequency sanity checks, band-edge protection

### Out of Scope

- Split TX/RX frequency
- Multi-TX (feature-gated)
- Contest-specific TX behavior

### Validation

Key up into a dummy load, record transmitted audio on another receiver, decode it with WSJT-X. Confirm it sees valid FT8.

## Phase 6: AP (A Priori) Decoding

Use known information to decode signals too weak for blind decoding.

### AP Passes

- **AP pass 1:** Own callsign known (from config) — fix those 28 bits as high-confidence LLRs, giving LDPC a head start. ~2 dB coding gain.
- **AP pass 2:** Own callsign AND correspondent known (from QSO state) — fix 56 bits. ~4 dB gain.
- **AP pass 3:** Both callsigns AND expected message type known — fix 56+ bits.

### Implementation

- Decoder's LLR array gets "primed" with known bit values before LDPC BP runs
- New `ApContext` struct accepted optionally by decoder — contains known callsigns and expected message type
- AP context sourced from: station config (my callsign) and QSO state machine (who I'm working, what I expect next)
- Run blind decode first, then AP passes on candidates that failed blind decode
- CRC still validates everything — AP cannot produce false decodes

### Dependency

AP pass 1 works immediately from config. AP passes 2 and 3 require TX pipeline and QSO state machine (Phase 5) to provide QSO context.

### Validation

Record a weak-signal QSO in WSJT-X where AP made the difference (look for "a1"/"a2" flags in WSJT-X output). Feed same audio to Pancetta. Without AP, decodes should fail. With AP, they should succeed.

## Phase 7: TUI Polish

Make the TUI operationally equivalent to WSJT-X's workflow.

### Core UX

1. **Band activity list** — decoded messages with timestamp, SNR, frequency, message text. Scrollable. Select a CQ to start a QSO.
2. **TX controls** — Enable TX toggle, Halt TX, current TX message displayed, next message visible. Free text override.
3. **Frequency display** — dial frequency + audio offset. Select in waterfall or band activity to set TX frequency.
4. **Waterfall** — already exists, needs to support signal selection.
5. **QSO status** — who you're working, QSO stage, elapsed time.
6. **Log confirmation** — auto-log to ADIF on QSO completion (RR73 exchanged).
7. **Keybindings** — F1-F6 for TX messages, Esc to halt TX, Tab to toggle TX enable.

### Out of Scope

- Settings/config dialogs in TUI (config file + first-run wizard is sufficient)
- Band switching from TUI
- Log viewer (external tools handle ADIF)

### Validation

Side-by-side with WSJT-X: see CQ, select it, enable TX, watch the exchange, log it. Interaction pattern should feel familiar.

## Phase 8: Cleanup (Fast Follow)

Folded in alongside main work, not a separate phase.

### Feature-Gating

Gate behind `cfg` feature flags (don't delete, don't test by default):
- FT2 protocol (already gated)
- Multi-TX waveform summation
- Contest logging
- DX cluster integration
- PSKReporter upload

### Dependency Security

Bump per documented PLAN-fixes.md:
- `bytes` 1.10.1 -> 1.11.1 (integer overflow)
- `validator` 0.18 -> 0.20 (IDNA Punycode vulnerability)
- `ratatui` 0.28 -> 0.29 (Stacked Borrows violation)
- `sqlx` 0.7 -> 0.8 (binary protocol vulnerabilities)

### Dead Code

Fix remaining warnings (`DiagnosticInfo`, unused methods) — use or remove.

### Dual Database

Pick one of rusqlite or sqlx. For a single-user desktop app, rusqlite is the simpler choice. Tackle when wiring QSO logging in TX phase.

### CI

Add macOS runner since that's the target platform. Can wait until after decoder is solid.

## Phase Ordering and Dependencies

```
Phase 1 (Benchmark) ---- standalone, do first
Phase 2 (Multi-pass) --- depends on 1 for validation
Phase 3 (OSD) ---------- depends on 1 for validation, independent of 2
Phase 4 (Fine freq) ---- depends on 1 for validation, improves 2's subtraction
Phase 5 (TX pipeline) -- independent of 2-4, can overlap
Phase 6 (AP decoding) -- depends on 5 (QSO context), depends on 1 for validation
Phase 7 (TUI polish) --- depends on 5 (TX controls)
Phase 8 (Cleanup) ------ independent, fold in throughout
```

Recommended execution order: 1 -> 4 -> 2 -> 3 -> 5 -> 6 -> 7, with 8 throughout.

Note: Phase 4 (fine freq) before Phase 2 (multi-pass) because accurate sub-bin frequency estimation makes signal subtraction cleaner — subtraction with coarse frequency estimates creates artifacts. Phases 2 and 3 are independent of each other and could be parallelized.

Phase numbering reflects conceptual grouping (decoder improvements 1-4, TX 5-6, UX 7, cleanup 8), not execution order.
