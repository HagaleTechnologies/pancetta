# FT8 Decoder Sensitivity Improvement — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Improve FT8 decode rate from 34% to 100%+ of ft8_lib, measured by cross-validation on real WAV recordings.

**Architecture:** Four targeted changes to the decode pipeline: (1) extract symbols from the spectrogram instead of redundant FFTs, (2) switch LDPC from min-sum to sum-product, (3) extend sync search to negative time offsets and full frequency range, (4) enable OSD-2. Each change is independently testable via the cross-validation benchmark.

**Tech Stack:** Rust, rustfft, bitvec

**Benchmark command:** `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate`

---

## Phase A: Reach Parity with ft8_lib

### Task 1: Extract symbols from spectrogram instead of independent FFTs

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:805-920` (decode_candidate)
- Modify: `pancetta-ft8/src/decoder.rs:1072-1130` (compute_soft_llrs)

This is the biggest sensitivity fix (2-4 dB). Currently `decode_candidate` calls `extract_symbols_complex` which runs a fresh 1920-point FFT per symbol (6.25 Hz resolution). The spectrogram already has 3.125 Hz resolution at freq_osr=2. Reading directly from the spectrogram eliminates the resolution mismatch.

- [ ] **Step 1: Add spectrogram to decode_candidate signature**

In `pancetta-ft8/src/decoder.rs`, change `decode_candidate` to accept the spectrogram:

```rust
fn decode_candidate(
    &mut self,
    audio: &[f64],
    candidate: &CostasCandidate,
    spectrogram: &Spectrogram,  // ADD THIS
) -> Ft8Result<Option<DecodedMessage>> {
```

Update the call site in `decode_window` (around line 330) to pass `&spectrogram`.

- [ ] **Step 2: Write extract_symbols_from_spectrogram**

Add a new method that reads tone magnitudes directly from the spectrogram. Place it after `extract_symbols_complex` (after line 1056):

```rust
/// Extract symbol tone magnitudes directly from the pre-computed spectrogram.
/// Uses the spectrogram's freq_osr=2 resolution (3.125 Hz effective) which
/// is 2x better than the independent symbol FFT approach.
fn extract_symbols_from_spectrogram(
    &self,
    spectrogram: &Spectrogram,
    candidate: &CostasCandidate,
) -> Ft8Result<Vec<[f64; NUM_TONES]>> {
    let pp = &self.protocol_params;
    let mut tone_magnitudes = Vec::with_capacity(pp.num_symbols);

    for sym_idx in 0..pp.num_symbols {
        // Each symbol occupies 2 time steps in the spectrogram
        let time_idx = candidate.time_step + sym_idx * 2;

        let mut mags = [0.0f64; NUM_TONES];
        for tone in 0..pp.num_tones {
            let freq_idx = candidate.freq_bin + tone;
            if time_idx < spectrogram.num_steps && freq_idx < spectrogram.num_bins {
                // Average both half-symbol time steps for better SNR
                let mag0 = spectrogram.power[time_idx][candidate.freq_sub][freq_idx];
                let mag1 = if time_idx + 1 < spectrogram.num_steps {
                    spectrogram.power[time_idx + 1][candidate.freq_sub][freq_idx]
                } else {
                    mag0
                };
                mags[tone] = (mag0 + mag1) / 2.0;
            } else {
                mags[tone] = -120.0; // noise floor
            }
        }
        tone_magnitudes.push(mags);
    }

    Ok(tone_magnitudes)
}
```

- [ ] **Step 3: Replace the fine-timing loop with spectrogram extraction**

In `decode_candidate`, replace the time_deltas × freq_offsets search loop with the spectrogram-based extraction. The spectrogram already captures the signal at the candidate's exact time/frequency position found by Costas sync:

```rust
fn decode_candidate(
    &mut self,
    audio: &[f64],
    candidate: &CostasCandidate,
    spectrogram: &Spectrogram,
) -> Ft8Result<Option<DecodedMessage>> {
    let sps = self.protocol_params.samples_per_symbol(SAMPLE_RATE);
    let tone_spacing = self.protocol_params.tone_spacing;
    let xor_sequence = self.protocol_params.xor_sequence;
    let spec_step = sps / 2;

    // Extract data symbols directly from the spectrogram.
    // The spectrogram values are already in dB, which is what
    // the max-log LLR computation expects.
    let tone_magnitudes = self.extract_symbols_from_spectrogram(spectrogram, candidate)?;

    // Filter to data symbols only (skip Costas sync symbols)
    let data_magnitudes: Vec<[f64; NUM_TONES]> = self
        .protocol_params
        .data_symbol_indices()
        .map(|sym_idx| tone_magnitudes[sym_idx])
        .collect();

    let mut llrs = self.compute_soft_llrs(&data_magnitudes);
    normalize_llrs(&mut llrs);

    let corrected_bits = match self.ldpc_decoder.decode_soft(&llrs) {
        Ok(bits) => bits,
        Err(_) => return Ok(None),
    };

    if !self.verify_crc(&corrected_bits) {
        // Spectrogram extraction failed — fall back to the complex DFT
        // approach with fine timing search for this candidate.
        return self.decode_candidate_complex(audio, candidate);
    }

    // CRC passed — build the decoded message
    // ... (keep existing message parsing code from line 897 onwards)
}
```

Note: Keep the old `extract_symbols_complex` approach as a fallback method `decode_candidate_complex` (rename the current body). If the spectrogram extraction + CRC fails, try the fine-timing DFT approach. This dual strategy catches signals the spectrogram misses due to time alignment.

- [ ] **Step 4: Verify data_symbol_indices exists or add it**

Check if `ProtocolParams` has a `data_symbol_indices()` method. If not, add it:

```rust
/// Returns iterator over data symbol indices (non-Costas positions)
pub fn data_symbol_indices(&self) -> impl Iterator<Item = usize> + '_ {
    (0..self.num_symbols).filter(move |&i| {
        !self.costas_positions.iter().any(|&start| {
            i >= start && i < start + self.costas_length
        })
    })
}
```

This should be in `pancetta-ft8/src/protocol.rs` or wherever `ProtocolParams` is defined.

- [ ] **Step 5: Run cross-validation benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate 2>&1 | tail -5
```

Expected: decode ratio improves from ~34% toward 60-80%.

- [ ] **Step 6: Run all tests**

```bash
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | tail -5
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/src/protocol.rs
git commit -m "feat: extract symbols from spectrogram for +2-4 dB sensitivity

Read tone magnitudes directly from the 3840-point spectrogram
(3.125 Hz resolution at freq_osr=2) instead of running independent
1920-point FFTs (6.25 Hz resolution). Eliminates spectral leakage
from sub-bin frequency offsets. Falls back to complex DFT approach
if spectrogram extraction fails CRC."
```

---

### Task 2: Sum-product LDPC decoder

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:1304-1526` (LdpcDecoder)

Switch from normalized min-sum to sum-product for ~0.5-1 dB improvement.

- [ ] **Step 1: Add fast_tanh and fast_atanh functions**

Add before the `LdpcDecoder` struct (around line 1300):

```rust
/// Padé approximant for tanh, matching ft8_lib's approach.
/// Accurate to ~1e-5 for |x| < 5. Clamped at ±4.97.
#[inline]
fn fast_tanh(x: f32) -> f32 {
    if x.abs() > 4.97 {
        return if x > 0.0 { 1.0 } else { -1.0 };
    }
    let x2 = x * x;
    let num = x * (135135.0 + x2 * (17325.0 + x2 * (378.0 + x2)));
    let den = 135135.0 + x2 * (62370.0 + x2 * (3150.0 + x2 * 28.0));
    num / den
}

/// Fast atanh using log identity. Clamped to avoid infinity.
#[inline]
fn fast_atanh(x: f32) -> f32 {
    let x = x.clamp(-0.9999999, 0.9999999);
    0.5 * ((1.0 + x) / (1.0 - x)).ln()
}
```

- [ ] **Step 2: Add LdpcAlgorithm enum and wire it into LdpcDecoder**

```rust
#[derive(Debug, Clone, Copy)]
enum LdpcAlgorithm {
    MinSum { normalization_factor: f32 },
    SumProduct,
}
```

Add `algorithm: LdpcAlgorithm` field to `LdpcDecoder`. Change the constructor to default to `SumProduct`:

```rust
fn new(max_iterations: usize) -> Ft8Result<Self> {
    // ... existing parity matrix and var_positions setup ...
    Ok(Self {
        max_iterations,
        parity_check_matrix,
        var_positions,
        normalization_factor: 0.75, // kept for MinSum fallback
        algorithm: LdpcAlgorithm::SumProduct,
        osd: None,
    })
}
```

- [ ] **Step 3: Add sum-product check node update**

In `belief_propagation`, replace the check node update block (lines 1464-1498) with a match on the algorithm:

```rust
// Check node update
for check_idx in 0..num_checks {
    let connected_vars = self.parity_check_matrix.get_connected_variables(check_idx);
    let degree = connected_vars.len();

    match self.algorithm {
        LdpcAlgorithm::SumProduct => {
            // Sum-product: product of tanh(msg/2), then 2*atanh(product)
            for target_pos in 0..degree {
                let mut product = 1.0f32;
                for pos in 0..degree {
                    if pos != target_pos {
                        product *= fast_tanh(v2c[check_idx][pos] / 2.0);
                    }
                }
                c2v[check_idx][target_pos] = 2.0 * fast_atanh(product);
            }
        }
        LdpcAlgorithm::MinSum { normalization_factor } => {
            // Existing min-sum code (lines 1469-1498)
            let mut total_sign: i8 = 1;
            let mut min1_mag = f32::MAX;
            let mut min2_mag = f32::MAX;
            let mut min1_pos: usize = 0;
            let mut signs = [1i8; 7];

            for pos in 0..degree {
                let msg = v2c[check_idx][pos];
                let s = if msg < 0.0 { -1i8 } else { 1i8 };
                signs[pos] = s;
                total_sign *= s;
                let mag = msg.abs();
                if mag < min1_mag {
                    min2_mag = min1_mag;
                    min1_mag = mag;
                    min1_pos = pos;
                } else if mag < min2_mag {
                    min2_mag = mag;
                }
            }

            for pos in 0..degree {
                let edge_sign = total_sign * signs[pos];
                let mag = if pos == min1_pos { min2_mag } else { min1_mag };
                c2v[check_idx][pos] = edge_sign as f32 * mag * normalization_factor;
            }
        }
    }
}
```

- [ ] **Step 4: Run cross-validation benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate 2>&1 | tail -5
```

Expected: further improvement in decode ratio.

- [ ] **Step 5: Run all tests**

```bash
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | tail -5
```

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: sum-product LDPC decoder for +0.5-1 dB sensitivity

Port ft8_lib's sum-product algorithm with Padé tanh approximant.
Keep min-sum available via LdpcAlgorithm enum for benchmarking.
Sum-product is theoretically optimal for this LDPC code."
```

---

### Task 3: Extend sync search range

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:52,58,618-645` (constants and costas_sync_search)

- [ ] **Step 1: Lower MIN_FREQ_BIN and extend time search**

Change the constants:

```rust
// Line 52: change from
const MIN_SYNC_SCORE: f64 = 3.5;
// (keep as is)

// Line 58: change from
const MIN_FREQ_BIN: usize = 32;
// to
const MIN_FREQ_BIN: usize = 0;
```

- [ ] **Step 2: Allow negative time offsets in costas_sync_search**

In `costas_sync_search` (line 617-645), change the time search range. Since we can't have negative array indices, use a signed offset approach:

```rust
fn costas_sync_search(&self, spectrogram: &Spectrogram) -> Ft8Result<Vec<CostasCandidate>> {
    let mut candidates = Vec::new();
    let pp = &self.protocol_params;

    let max_time_step = spectrogram.num_steps.saturating_sub(pp.num_symbols * 2 + 1);
    let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);

    for freq_sub in 0..spectrogram.freq_osr {
        for t0 in 0..=max_time_step {
            for f0 in MIN_FREQ_BIN..max_freq_bin {
                let score = self.compute_costas_score(spectrogram, t0, f0, freq_sub);

                if score > MIN_SYNC_SCORE {
                    candidates.push(CostasCandidate {
                        time_step: t0,
                        freq_bin: f0,
                        freq_sub,
                        sync_score: score,
                    });
                }
            }
        }
    }

    candidates.sort_by(|a, b| {
        b.sync_score
            .partial_cmp(&a.sync_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(MAX_SYNC_CANDIDATES);
    self.nms_candidates(&mut candidates);

    Ok(candidates)
}
```

Also raise `MAX_SYNC_CANDIDATES` from 80 to 120:

```rust
// Line 55: change from
const MAX_SYNC_CANDIDATES: usize = 80;
// to
const MAX_SYNC_CANDIDATES: usize = 120;
```

- [ ] **Step 3: Run cross-validation benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate 2>&1 | tail -5
```

- [ ] **Step 4: Run all tests**

```bash
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: extend sync search — full frequency range, more candidates

Lower MIN_FREQ_BIN from 32 to 0 (search full 0-6000 Hz range).
Raise MAX_SYNC_CANDIDATES from 80 to 120 for more decode attempts."
```

---

## Phase B: Beat ft8_lib

### Task 4: Enable OSD-2

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:123` (Ft8Config default)

OSD-2 code already exists in `osd.rs`. Just change the default depth from 1 to 2:

- [ ] **Step 1: Change default OSD depth**

```rust
// Line 123: change from
osd_depth: Some(1),
// to
osd_depth: Some(2),
```

- [ ] **Step 2: Run cross-validation benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate 2>&1 | tail -5
```

Expected: a few more decodes recovered (signals where BP left 1-3 parity errors and two info bits were wrong).

- [ ] **Step 3: Run all tests**

```bash
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: enable OSD-2 for additional decode recovery

OSD-2 tries all 4,095 pairs of info-bit flips when BP leaves
<=5 parity errors. This recovers signals that ft8_lib cannot
decode (ft8_lib has no OSD at all)."
```

---

### Task 5: Update cross-validation threshold

**Files:**
- Modify: `pancetta-ft8/tests/wav_decode_tests.rs:207-218`

After all Phase A+B changes, update the threshold to lock in gains:

- [ ] **Step 1: Run final benchmark and record numbers**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate 2>&1 | grep -E "ratio|ours=|Overall"
```

Record the exact ours/ft8_lib numbers.

- [ ] **Step 2: Update threshold**

Set the regression floor to 90% of the achieved ratio (e.g., if we hit 100%, set floor to 0.90):

```rust
assert!(
    overall_ratio >= 0.90,
    "REGRESSION: decode ratio {:.1}% dropped below 90% floor. ...",
    ...
);
```

- [ ] **Step 3: Run full test suite**

```bash
cargo test --workspace 2>&1 | grep "test result"
```

All must pass.

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "test: raise cross-validation threshold to lock in sensitivity gains"
```

---

## Execution Notes

- Run the cross-validation benchmark after each task to measure incremental improvement
- The benchmark takes ~30s (decodes several WAV files)
- Task 1 (spectrogram extraction) is expected to have the largest impact
- Tasks are sequential — each builds on the previous (same file, same decode pipeline)
- Keep the old `extract_symbols_complex` as `decode_candidate_complex` fallback — don't delete working code until we're sure the spectrogram approach is strictly better
