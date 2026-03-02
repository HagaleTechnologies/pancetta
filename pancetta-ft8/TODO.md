# FT8 Remaining Work

_Created 2026-03-01. Updated 2026-03-01. See ANALYSIS.md for the full honest assessment._

---

## ~~1. Fix Decoder Critical Bugs~~ — DONE (2026-03-01)

All 6 critical decoder bugs were fixed in a comprehensive rewrite of `src/decoder.rs`.
The old pipeline (AGC, coarse/fine freq search, coherent averaging, Doppler compensation)
was replaced with a clean spectrogram + Costas sync + complex DFT + soft LLR pipeline.

- **1.1 Costas sync**: 2D spectrogram search correlating against [3,1,4,0,6,5,2] at 21 positions
- **1.2 Complex DFT**: cos+sin correlation at 8 tone frequencies per symbol, magnitude-based
- **1.3 Soft LLRs**: Max-log approximation from per-symbol 8-tone magnitudes with Gray code mapping
- **1.4 LocalDecoder stub**: Removed entirely. Single sequential decode path.
- **1.5 Coherent averaging**: Removed (replaced by spectrogram approach — no averaging needed)
- **1.6 is_ft8_like_signal**: Removed (Costas sync score threshold serves as the signal filter)

184 tests pass (up from 169). No regressions.

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
[DONE] 1.1-1.6 Decoder critical bugs
2.1 Remove GFSK ──> 2.2 Fix soft-clip ──> 4.2 Assert round-trips
3.1 + 3.2 + 4.3: /R /P fixes + tests (independent, do anytime)
4.1 Real integration test signals (decoder is ready now)
5.1-5.4 Cross-implementation (decoder is ready now)
```

The next critical path is: **Fix modulator -> assert round-trips -> cross-implementation validation**.
The /R /P encoder fixes are independent and can be done anytime.
