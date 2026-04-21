# Decoder Phase A: Reach ft8_lib Parity

**Date:** 2026-04-20
**Status:** Approved
**Builds on:** `2026-04-18-decoder-sensitivity-design.md`
**Goal:** Raise decode rate from 34% to 100% of ft8_lib on reference WAV files

## Context

Pancetta's FT8 decoder currently decodes 13/38 signals (34% of ft8_lib) on real WAV recordings. The root cause analysis in the sensitivity design spec identified four gaps totaling 4-8 dB of lost sensitivity. Multi-pass infrastructure (signal subtraction, budget tracking, candidate reduction) is already operational but dormant — first-pass decodes are too few to produce useful residuals.

This spec covers **Phase A only**: four independent fixes to reach ft8_lib parity. Phase B (OSD-2/3, AP decoding, advanced subtraction) and subsequent sub-projects (hamlib integration, cqdx.io) follow in separate specs.

## Fixes

Ordered by implementation sequence (simplest first, each independently testable):

### A4: Lower Frequency Floor

**File:** `pancetta-ft8/src/decoder.rs` line 66
**Change:** `MIN_FREQ_BIN` from 16 (100 Hz) to 0
**Also:** Verify `max_freq_bin` at line 1056 extends to `num_bins - NUM_TONES`

**Rationale:** FT8 signals can appear anywhere in the 200-3000 Hz audio passband. The 100 Hz floor is unnecessarily conservative and causes total misses for sub-100 Hz signals.

**Risk mitigation:** If false positives appear from DC-adjacent bins (hum artifacts), raise floor to 4 (25 Hz). Monitor cross-validation false positive count.

### A1: Symbol Extraction from Spectrogram

**File:** `pancetta-ft8/src/decoder.rs`
**Change:** New function `extract_symbols_from_spectrogram()` replaces `extract_symbols_complex()` as the primary symbol extraction path.

**Current problem:** `extract_symbols_complex()` (line 1787) computes an independent 1920-point FFT per symbol (6.25 Hz bins). The spectrogram already provides 3840-point FFTs at 2x frequency oversampling — better resolution going to waste. Sub-bin signals suffer 2-4 dB spectral leakage penalty.

**New implementation:**

```rust
fn extract_symbols_from_spectrogram(
    &self,
    spectrogram: &Spectrogram,
    t0: usize,        // time step from sync search
    f0: usize,        // frequency bin from sync search
    freq_sub: usize,  // frequency sub-bin (0 or 1)
) -> (Vec<u8>, Vec<f32>)
```

**Algorithm:**
1. For each of 79 symbols at index `s`:
   - Time step: `t = t0 + s * TIME_OSR`
   - Read 8 tone magnitudes: `mag[tone] = power[t][freq_sub][f0 + tone]` for tone 0..7
2. Hard symbols: `argmax(mag[0..8])` per symbol
3. Soft LLRs: For each of 3 bits encoding 8 tones:
   - `llr[bit] = max(mag[tones where bit=1]) - max(mag[tones where bit=0])`
   - Spectrogram values are already in dB, so subtraction gives log-likelihood ratio directly

**Fallback:** Keep `extract_symbols_complex()`. If spectrogram extraction produces a CRC failure, retry with the DFT method. This handles edge cases where a signal straddles bin boundaries.

**Expected gain:** 2-4 dB

### A3: Extended Sync Search (Negative Time)

**File:** `pancetta-ft8/src/decoder.rs`
**Change:** Extend Costas sync search to cover `t0 < 0` (before the nominal slot start).

**Current problem:** `costas_sync_search()` (line 1059) starts at `t0 = 0`. Signals arriving early due to clock drift or long-path DX propagation are missed (1-2 dB worth of signals).

**Implementation:**
1. In `compute_spectrogram()`: Accept additional audio samples preceding the nominal window. Prepend 10 extra time steps (10 × 960 / 12000 = 0.8 seconds of look-back).
2. Add `time_padding: usize` field to `Spectrogram` struct to track the prepended offset.
3. `costas_sync_search()` naturally searches from `t0 = 0` through the full padded spectrogram — the padding extends coverage into what was previously negative time.
4. When reporting `DecodedMessage` time offsets, subtract `time_padding` so reported values remain relative to the nominal slot start.

**Search space impact:** 10 extra time steps × TIME_OSR=2 = 20 additional positions per frequency. Roughly 12% more search space — well within the 2-second budget.

**Expected gain:** 1-2 dB for affected signals

### A2: Verify Sum-Product LDPC

**File:** `pancetta-ft8/src/decoder.rs`

**Status:** The code already has `LdpcAlgorithm::SumProduct` as the default (line 3006) with `fast_tanh`/`fast_atanh` implementations. The original sensitivity spec assumed min-sum was in use — this may already be resolved.

**Action:**
1. Trace the decode path: multi-pass loop → `decode_soft()` → `belief_propagation()` → confirm `SumProduct` branch (lines 3136-3145) is exercised.
2. If sum-product is already active: this item is a **no-op**. Document as complete.
3. If something overrides to min-sum: fix the override.
4. Verify `fast_tanh` Padé approximant accuracy: max error vs `f32::tanh()` on [-5, 5] should be < 1e-4. If larger, the approximation may degrade weak-signal LLR precision.

**Expected gain:** 0.5-1 dB if not already active; 0 dB if already working.

## Testing Strategy

### Per-Fix Validation
Each fix gets:
- A unit test proving the specific mechanism works (e.g., spectrogram extraction produces correct symbols for a known signal)
- A cross-validation benchmark run recording decode count before/after

### Regression Gate
- Decode count must never drop below the pre-fix baseline after any change
- False positive count must remain at 0

### Final Acceptance
- **Target:** 38/38 decodes (100% of ft8_lib) on reference WAV files
- **Stretch:** If Phase A alone exceeds 100%, record the overshoot — it validates that spectrogram extraction provides gains beyond what ft8_lib achieves
- **Benchmark command:** `cargo test -p pancetta-ft8 --test benchmark_tests`

## Non-Goals

- Multi-pass tuning (infrastructure works, just needs first-pass fuel)
- AP decoding (Phase B / Advanced Decoder spec)
- OSD-2/3 (Phase B)
- Parallel decode optimization (Advanced Decoder spec)
- Any changes outside `pancetta-ft8`

## Dependencies

- Reference WAV files must be available in the test fixtures
- No external crate additions required
- All changes are backward-compatible (existing tests must continue to pass)
