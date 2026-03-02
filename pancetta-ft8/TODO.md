# FT8 Remaining Work

_Created 2026-03-01. See ANALYSIS.md for the full honest assessment behind these items._

---

## 1. Fix Decoder Critical Bugs

These must be fixed in order -- each depends on the one before it.

### 1.1 Implement Real Costas Sync Detection
- **File**: `src/decoder.rs`, sync logic (also `sync.rs` if separate)
- **Problem**: Current sync engine does generic multi-tone energy detection. It never correlates against the Costas array `[3,1,4,0,6,5,2]`.
- **Fix**: Compute a 2D time-frequency spectrogram (short overlapping FFTs). For each candidate (time_offset, freq_offset), correlate the 21 known Costas symbol positions (0-6, 36-42, 72-78) against the spectrogram. Select the (time, freq) pair with maximum correlation score.
- **This is the single most important fix.** Nothing else in the decoder can work without correct time/frequency sync.

### 1.2 Fix Symbol Extraction: Complex DFT Magnitude
- **File**: `src/decoder.rs`, `correlate_with_tone` function
- **Problem**: Uses only the cosine (real) component of a single-frequency DFT. Result depends on unknown carrier phase, making symbol extraction unreliable.
- **Fix**: Correlate against both `cos` and `sin` at each tone frequency. Use magnitude `sqrt(real^2 + imag^2)` to determine the most likely tone. Also compute soft log-likelihood ratios from the magnitude differences between tones.

### 1.3 Pass Soft LLRs to LDPC Decoder
- **File**: `src/decoder.rs`, LDPC decode call site
- **Problem**: Hard-decision bits (0/1) converted to fixed +/-4.0 LLR. All soft reliability information from the correlator is discarded.
- **Fix**: For each of 174 codeword bits (3 bits per data symbol, 58 data symbols), compute LLR from the 8-tone magnitude vector. Pass these soft values to belief propagation instead of hard +/-4.0.

### 1.4 Remove or Rewrite LocalDecoder Stub
- **File**: `src/decoder.rs`, `LocalDecoder::decode_single_candidate` (~line 1314)
- **Problem**: Ignores audio entirely, returns `Ft8Message::default()` for any candidate with SNR > -15 and confidence > 0.7. This is the parallel decode path (activates when >4 candidates found).
- **Fix**: Either remove the parallel path and always use sequential decoding, or implement it properly using the same pipeline as the sequential path.

### 1.5 Fix Coherent Averaging Phase Computation
- **File**: `src/decoder.rs`, `coherent_symbol_averaging` (~line 490)
- **Problem**: `point.time_window as f64 * 0.16` treats window index as time in seconds. Actual time is `time_window * hop_size / sample_rate`.
- **Fix**: Multiply by correct factor.

### 1.6 Implement `is_ft8_like_signal`
- **File**: `src/decoder.rs` (~line 777)
- **Problem**: Always returns `true`. Every spectral peak becomes a decode candidate.
- **Fix**: Check for FT8-like characteristics: ~12.6s duration, 6.25 Hz tone spacing, presence of Costas-like energy pattern. Can be a rough filter -- LDPC CRC check is the final gate.

---

## 2. Fix Modulator Bugs

### 2.1 Remove Gaussian Filtering (GFSK -> CPFSK)
- **File**: `src/modulator.rs`
- **Problem**: `GAUSSIAN_BT=2.0` applies GFSK shaping. FT8 uses plain CPFSK with abrupt frequency transitions between symbols (with phase continuity).
- **Fix**: Remove the Gaussian filter. Use rectangular pulse shape (instant frequency transitions at symbol boundaries). Maintain continuous phase accumulation.

### 2.2 Fix Soft-Clip Discontinuity
- **File**: `src/modulator.rs`, `soft_clip` function (~line 225)
- **Problem**: `soft_clip(0.5)` returns 0, but `soft_clip(0.4999)` returns 0.4999. Discontinuity at +/-0.5. Combined with normalization to 0.95, this clips most of the signal.
- **Fix**: Either remove soft clipping (normalized CPFSK shouldn't need it) or fix the formula to be continuous. A correct soft clip: `x.signum() * (1.0 - (-x.abs()).exp())` or similar.

---

## 3. Fix Encoder Edge Cases

### 3.1 Fix /R vs /P Suffix Encoding
- **File**: `src/encoder.rs`, `pack28` (~line 391)
- **Problem**: Both `/R` (roving) and `/P` (portable) set `ip=1`. WSJT-X uses `ip=0` for `/R`, `ip=1` for `/P`.
- **Fix**: Set `ip=0` for `/R`, `ip=1` for `/P`.

### 3.2 Fix i3 Type Field for /P Callsigns
- **File**: `src/encoder.rs`, `try_encode_standard` (~line 220)
- **Problem**: Sets `i3=2` (EU VHF contest format) when callsign ends with `/P`. Should be `i3=1` (standard), with the `ip` bit carrying the portable flag.
- **Fix**: Always use `i3=1` for standard messages. Remove the `/P` -> `i3=2` branch.

---

## 4. Fix Test Infrastructure

### 4.1 Replace Fake Integration Test Signals
- **File**: `tests/integration_tests.rs`, `message_to_test_tones` function
- **Problem**: Generates tones via `(ch as u8 % 8)` -- not real FT8 encoding. No Costas arrays, no LDPC parity, no valid CRC. Tests only verify "doesn't crash."
- **Fix**: Use `Ft8Encoder::encode_message()` + `Ft8Modulator::modulate_symbols()` to generate real FT8 test signals. Gate with `#[cfg(feature = "transmit")]`.

### 4.2 Add Assertions to Round-Trip Tests
- **File**: `tests/round_trip_tests.rs`
- **Problem**: `test_round_trip_cq_clean` and similar tests print decode results but never assert that any message was decoded, or that the decoded message matches the input.
- **Fix**: After decoder bugs are fixed (items 1.1-1.6), add `assert!(decoded.iter().any(|m| m.contains("K1ABC")))` or equivalent.

### 4.3 Add /R and /P Encoder Tests
- **File**: `tests/wsjtx_compat_tests.rs` or `tests/transmission_tests.rs`
- **Problem**: No test exercises the `/R` or `/P` suffix path. Bugs 3.1 and 3.2 are completely untested.
- **Fix**: Add tests for `K1ABC/R` and `K1ABC/P` encoding after fixing bugs 3.1 and 3.2.

---

## 5. Cross-Implementation Validation

### 5.1 Generate ft8_lib Reference WAV Files
- Build a small C harness around ft8_lib's `ft8_encode()` + audio generation
- Generate WAV files for known messages at known frequencies
- Feed to our decoder and verify output matches
- This is the gold standard: an independent implementation says we're right

### 5.2 Test Our Encoder Output Against ft8_lib Decoder
- Use our encoder + modulator to produce WAV files
- Feed to ft8_lib's decoder
- Verify it decodes correctly
- Validates the transmit side end-to-end against an independent decoder

### 5.3 Decode Real Off-Air FT8 Signals
- Capture 15-second WAV files from a real FT8 band (14.074 MHz)
- Decode with both WSJT-X and our decoder
- Compare message lists
- This is the ultimate "does it really work" test

### 5.4 CI-Integrated Reference Vectors
- Compile ft8_lib as part of CI
- Generate reference test vectors programmatically for every commit
- Place reference data in `tests/fixtures/`

---

## Recommended Order of Attack

```
1.1 Costas sync ─────┐
                      ├──> 1.2 Complex DFT ──> 1.3 Soft LLR ──> 4.2 Assert round-trips
2.1 Remove GFSK ─────┘
2.2 Fix soft-clip ────┘
1.4 Remove LocalDecoder stub (independent, do anytime)
1.5 Fix coherent averaging (independent, do anytime)
1.6 is_ft8_like_signal (independent, low priority)
3.1 + 3.2 + 4.3: /R /P fixes + tests (independent, do anytime)
4.1 Real integration test signals (after 1.1-1.3 work)
5.1-5.4 Cross-implementation (after decoder works)
```

The critical path is: **Costas sync -> complex DFT -> soft LLR -> assert round-trips**.
Everything else can be done in parallel or after.
