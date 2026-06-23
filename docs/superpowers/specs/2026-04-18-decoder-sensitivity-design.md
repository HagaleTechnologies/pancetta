# FT8 Decoder Sensitivity Improvement — Design Spec

## Goal

Improve Pancetta's pure-Rust FT8 decoder from 34% of ft8_lib's sensitivity to 100%+, measured by cross-validation against the reference C implementation on real-world WAV recordings.

## Current State

- Cross-validation test: 13/38 decodes (34% of ft8_lib)
- Test files: `pancetta-ft8/tests/fixtures/wav/` (WSJT-X and JTDX recordings)
- Benchmark command: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate`

## Root Cause Analysis

| Rank | Stage | Issue | Impact |
|------|-------|-------|--------|
| 1 | Symbol extraction | Uses independent 1920-pt FFT (6.25 Hz bins) instead of reading from 3840-pt spectrogram (3.125 Hz bins). Sub-bin signals suffer spectral leakage. | 2-4 dB |
| 2 | LDPC decoding | Normalized min-sum (factor 0.75) instead of sum-product (tanh/atanh). | 0.5-1 dB |
| 3 | Sync search range | No search before time=0. Misses early-arriving signals (clock drift, DX). | 1-2 dB for affected signals |
| 4 | Frequency floor | MIN_FREQ_BIN=32 skips signals below 200 Hz. | Total miss below 200 Hz |

## Phase A: Reach Parity (34% -> 100%)

### A1: Symbol Extraction from Spectrogram

**File:** `pancetta-ft8/src/decoder.rs`

Replace `extract_symbols_complex()` with `extract_symbols_from_spectrogram()`.

Current approach: for each candidate, run a fresh 1920-point complex-DFT per symbol (up to 6,320 FFTs per window). Resolution is 6.25 Hz — signals between bins lose energy.

New approach: read 8 tone magnitudes directly from the already-computed spectrogram at `power[time_step][freq_sub][freq_bin]`. For candidate at `(t0, f0, freq_sub)`, data symbol `i` at time step `t0 + i*2` reads bins `f0+0` through `f0+7`. Apply Gray code mapping, compute LLRs from the dB values directly. Zero additional FFTs. Better resolution (3.125 Hz effective with freq_osr=2).

The spectrogram already stores values in dB (`10 * log10(1e-12 + mag2)`). The max-log LLR formula operates on dB values directly:
```
llr[bit0] = max(s2[4..7]) - max(s2[0..3])
```
where `s2[j] = spectrogram_power_at_tone[gray(j)]`.

The `decode_candidate` method signature changes to accept `&Spectrogram` instead of `&[f64]` audio.

### A2: Sum-Product LDPC Decoder

**File:** `pancetta-ft8/src/decoder.rs` (LdpcDecoder)

Add sum-product algorithm alongside existing min-sum. Port ft8_lib's implementation from `vendor/ft8_lib/ft8/ldpc.c:bp_decode`.

Check node update (sum-product):
```
For each check node c with variable neighbors N(c):
  For each variable v in N(c):
    product = 1.0
    for each u in N(c), u != v:
      product *= tanh(msg_v_to_c[u] / 2.0)
    msg_c_to_v[v] = 2.0 * atanh(product)
```

Use ft8_lib's Pade approximant for tanh (degree-5, clamped at +/-4.97):
```rust
fn fast_tanh(x: f32) -> f32 {
    let x = x.clamp(-4.97, 4.97);
    let x2 = x * x;
    let num = x * (135135.0 + x2 * (17325.0 + x2 * (378.0 + x2)));
    let den = 135135.0 + x2 * (62370.0 + x2 * (3150.0 + x2 * 28.0));
    num / den
}

fn fast_atanh(x: f32) -> f32 {
    let x = x.clamp(-0.9999999, 0.9999999);
    0.5 * ((1.0 + x) / (1.0 - x)).ln()
}
```

Add `LdpcAlgorithm` enum:
```rust
enum LdpcAlgorithm {
    MinSum { normalization_factor: f32 },
    SumProduct,
}
```

Default to `SumProduct`. Keep min-sum available for benchmarking.

### A3: Extended Sync Search

**File:** `pancetta-ft8/src/decoder.rs` (costas_sync_search, compute_costas_score)

Extend time search range to include negative offsets:
- Current: `t0 in 0..=max_time_step`
- New: `t0 in -10..=max_time_step` (10 half-symbol steps before nominal start)

Implementation: the spectrogram is computed from the audio buffer which includes ~1 second of overlap from the previous window. Extend the spectrogram computation to start earlier by padding the audio array. When `t0` is negative, map to the corresponding spectrogram index (offset by the padding amount).

Alternatively, simpler approach: pad the spectrogram array with 10 extra time steps at the beginning (computed from the overlap audio), then offset all `t0` values by +10 in the search. Candidates store the original (unpadded) time offset for decoding.

### A4: Lower Frequency Floor

**File:** `pancetta-ft8/src/decoder.rs`

Change `MIN_FREQ_BIN` from 32 to 0. Single constant change.

Raise `max_freq_bin` from `(4000.0 / tone_spacing) as usize` to `num_bins - NUM_TONES` (natural maximum where all 8 tones fit within the spectrogram).

## Phase B: Beat ft8_lib (100% -> 100%+)

### B1: OSD-2 Extension

**File:** `pancetta-ft8/src/osd.rs`

Add OSD-2: try all `C(91,2) = 4,095` pairs of info-bit flips when BP leaves <=3 parity errors (tighter gate than OSD-1's <=5). The infrastructure already exists — OSD-1 iterates single flips. OSD-2 adds a nested loop.

Gate OSD-2 on remaining parity errors <=3 to limit CPU cost and false positive risk. Each trial: flip 2 bits, recompute parity, check CRC-14.

### B2: Phase-Aware Signal Subtraction

**File:** `pancetta-ft8/src/decoder.rs` (subtract_signal)

Current: scalar amplitude projection, ignores phase, gated behind `#[cfg(feature = "transmit")]`.

New approach:
1. Remove the feature gate — subtraction should always be available
2. Re-encode the decoded message to tone symbols
3. Use `Ft8Modulator` to generate the time-domain signal at the candidate's exact frequency offset and time offset
4. Cross-correlate with original audio to estimate complex amplitude (magnitude + phase)
5. Subtract the phase-aligned reconstruction from the audio
6. This gives cleaner residuals for pass 2+

### B3: Wider Sync Search

Increase `MAX_SYNC_CANDIDATES` from 80 to 120. With sum-product LDPC being more expensive per candidate, this is a CPU/sensitivity trade-off. The extra candidates capture weaker signals that pass the MIN_SYNC_SCORE threshold.

## Success Criteria

| Milestone | Target | Threshold Change |
|-----------|--------|-----------------|
| After Phase A | >= 38/38 (100%) | Raise to 0.95 |
| After Phase B | > 38 decodes | Raise to 1.00 |
| Regression floor | Never below 30% | Keep at 0.30 |

## Testing Strategy

Each fix is independently measurable via the cross-validation test:
```
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate
```

The test reports per-file decode counts (ours vs ft8_lib) and overall ratio. After each fix, run the test and record the improvement.

Unit tests for each component:
- Spectrogram symbol extraction: verify tone magnitudes match for a known synthetic signal
- Sum-product LDPC: verify convergence on known codewords with noise
- Extended sync: verify detection of signals starting before t=0
- OSD-2: verify recovery of 2-bit error patterns

## File Map

| File | Changes |
|------|---------|
| `pancetta-ft8/src/decoder.rs` | Symbol extraction rewrite, LDPC algorithm switch, sync range extension, freq floor |
| `pancetta-ft8/src/osd.rs` | OSD-2 addition |
| `pancetta-ft8/src/lib.rs` | New config fields for LDPC algorithm, OSD depth |
| `pancetta-ft8/tests/wav_decode_tests.rs` | Threshold updates as sensitivity improves |

## Non-Goals

- Changing the spectrogram computation (it's already correct at freq_osr=2)
- Changing CRC or message parsing (already correct)
- GPU acceleration or SIMD (future optimization, not sensitivity)
- FT4 protocol support (separate project)
