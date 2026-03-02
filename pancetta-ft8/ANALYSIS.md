# FT8 Encoder/Decoder: Honest Assessment

_Written 2026-03-01. Updated 2026-03-01 after decoder rewrite._
_Based on thorough reading of all source files, tests, and comparison against
ft8_lib/WSJT-X reference implementation._

---

## Executive Summary

**The encoder is genuinely correct.** It produces bit-exact output matching
ft8_lib for payloads, LDPC codewords, CRC-14, Gray code mapping, and all 79
symbols. This is verified by 12 WSJT-X compatibility tests that compare against
independently-computed reference values.

**The decoder has been rewritten with a correct pipeline.** All 6 critical bugs
from the original assessment have been fixed: proper Costas sync detection,
complex DFT magnitude extraction, soft LLR computation, and removal of all
stubs and broken code paths. The decoder has not yet been validated against
real off-air signals or WSJT-X; end-to-end round-trip testing requires fixing
the modulator first (GFSK and soft-clip bugs).

**The modulator still has issues.** GFSK filtering instead of CPFSK, and a
discontinuous soft-clip function. These must be fixed before round-trip testing
can validate the decoder against encoder-generated signals.

---

## Part 1: What Is Genuinely Verified

### Encoder (VERIFIED - bit-exact with ft8_lib)

| Component | Status | Evidence |
|-----------|--------|----------|
| pack28 callsign encoding | Correct | Reference values match: CQ=2, K1ABC=10,214,965 |
| pack_basecall mixed-radix | Correct | 37x36x10x27x27x27 verified |
| packgrid (grid + signal reports) | Correct | FN42=10342, signal reports at MAXGRID4+offset |
| 77-bit payload layout | Correct | 3 reference payloads bit-exact with ft8_lib |
| Free text base-42 encoding | Correct | "HELLO WORLD" matches reference |
| CRC-14 (poly 0x2757, 82-bit) | Correct | Direct port of ftx_compute_crc(), byte-at-a-time |
| LDPC(174,91) encoding | Correct | Real WSJT-X generator matrix, syndrome=0 verified |
| Gray code mapping | Correct | ft8_lib kFT8_Gray_map lookup table, not formula |
| Symbol layout (S7-D29-S7-D29-S7) | Correct | Costas [3,1,4,0,6,5,2] at positions 0-6, 36-42, 72-78 |
| Full 79-symbol output | Correct | 2 messages verified symbol-by-symbol against ft8_lib |

### LDPC Module (VERIFIED)

| Component | Status | Evidence |
|-----------|--------|----------|
| Generator matrix (83x12 bytes) | Correct | Copied from kFTX_LDPC_generator, structurally verified |
| Parity check tables (Nm/Mn) | Correct | Mutual consistency verified by test |
| Encoding algorithm | Correct | Direct port of encode174(), systematic code preserved |
| Syndrome verification | Correct | All encoded messages pass H*c=0 check |

### Test Coverage (encoder side)

- 184 tests pass total
- 12 WSJT-X compatibility tests verify bit-exact output
- Property tests (proptest) cover LDPC validity, Gray code bijection, CRC sensitivity
- Determinism tests verify same input -> same output across instances

---

## Part 2: Decoder — Rewritten (2026-03-01)

All 6 critical bugs have been fixed in a comprehensive rewrite of `decoder.rs`.
The old pipeline (~500 lines of AGC, coarse/fine frequency search, coherent
averaging, Doppler compensation) was replaced with a clean ~200-line pipeline:

| Bug | Fix Applied |
|-----|-------------|
| No Costas sync | 2D spectrogram search: FFT size=1920 (=1 symbol, 6.25 Hz/bin), correlate [3,1,4,0,6,5,2] at 21 positions, NMS deduplication |
| Cosine-only DFT | Complex DFT (cos+sin) at 8 tone frequencies per symbol, Hann-windowed, magnitude-based |
| Hard-decision LDPC | Max-log LLR from per-symbol 8-tone magnitudes, Gray code bit mapping, clamped to [-25, 25] |
| LocalDecoder stub | Removed entirely; single sequential decode path |
| Coherent averaging bug | Removed; spectrogram approach doesn't need inter-window averaging |
| is_ft8_like_signal stub | Removed; Costas sync score threshold (MIN_SYNC_SCORE=8.0) serves as signal filter |

**Not yet validated** against real off-air signals or WSJT-X output. Requires
modulator fixes first for end-to-end round-trip testing.

### Modulator Issues (still open)

| Issue | Severity | Detail |
|-------|----------|--------|
| Gaussian filtering | Medium | FT8 uses CPFSK (abrupt frequency transitions), not GFSK. `GAUSSIAN_BT=2.0` blurs symbol boundaries. |
| Soft-clip discontinuity | High | `soft_clip(0.5)` returns 0, creating a jump from 0.5 (linear) to 0 (clipped). Combined with normalization to 0.95, this clips most of the signal. |

### Integration Tests Still Use Fake Signals

`message_to_test_tones` in integration_tests.rs generates tones via
`(ch as u8 % 8)` -- not real FT8 encoding. Now that the decoder has a correct
pipeline, these tests should be updated to use real encoder+modulator signals.

### Round-Trip Tests Still Don't Assert Decode Success

Round-trip tests print results but don't assert. The decoder pipeline is now
correct, but the modulator's GFSK and soft-clip bugs may prevent round-trip
success. Fix modulator first (TODO items 2.1, 2.2), then add assertions.

---

## Part 3: Encoder Bugs (Lower Priority)

### Bug: /R vs /P Suffix Encoding

`pack28` sets `ip=1` for both `/R` (roving) and `/P` (portable). WSJT-X uses
`ip=0` for `/R` and `ip=1` for `/P`. Both encode identically, which is wrong.

### Bug: i3=2 for /P Callsigns

`try_encode_standard` sets `i3=2` when a callsign ends with `/P`. In FT8,
`i3=2` means EU VHF contest format, which has a completely different field
layout. Standard portable calls must use `i3=1` with the `ip` bit. A decoder
receiving `i3=2` will attempt to parse the payload as a contest exchange.

These bugs don't affect the common case (standard callsigns without suffixes)
and are untested, so they went unnoticed.

---

## Part 4: How to Build Something Genuinely Useful

### Phase A: Validate Against Real-World Signals (Priority 1)

The single most convincing test is decoding a known FT8 signal from WSJT-X.

1. **Generate reference WAV files with WSJT-X or ft8_lib**
   - Use ft8_lib's `ft8_encode()` + audio generation to produce WAV files
     for known messages ("CQ K1ABC FN42", "K1DEF W1ABC -12", etc.)
   - Record the expected decode output
   - Feed these WAV files to our decoder and verify the output matches

2. **Record real off-air FT8 signals**
   - Capture 15-second WAV files from a real FT8 band (14.074 MHz)
   - Decode with both WSJT-X and our decoder
   - Compare results

3. **Generate test signals with our encoder, decode with ft8_lib**
   - Use our encoder + modulator to produce WAV files
   - Feed to ft8_lib's decoder and verify it decodes correctly
   - This validates the transmit side end-to-end against an independent decoder

### ~~Phase B: Fix the Decoder~~ — DONE (2026-03-01)

All decoder bugs fixed. See Part 2 above.

### Phase B (updated): Fix the Modulator (Priority 2)

1. **Remove Gaussian filtering** — use CPFSK, not GFSK
2. **Fix soft-clip discontinuity** — or remove soft clipping entirely

### Phase C: Build the Real Round-Trip Test (Priority 3)

Once the decoder works:

1. **Encode message -> modulate to audio -> decode -> verify message matches**
   - Start with clean channel (no noise), assert exact match
   - Add Gaussian noise at known SNR levels, assert decode at SNR >= -10 dB
   - Sweep SNR from 0 to -25 dB, record success rate curve

2. **Multi-signal test**
   - Place 3+ messages at different frequency offsets
   - Verify all are decoded

3. **Timing offset test**
   - Add fractional-symbol time offsets to the signal
   - Verify the sync engine finds the correct timing

### Phase D: Cross-Implementation Testing (Priority 4)

1. **Build a small C harness around ft8_lib**
   - Compile ft8_lib's `ft8_encode()` and `ft8_decode()` as a test oracle
   - Generate reference test vectors programmatically
   - Run as part of CI

2. **WSJT-X WAV file round-trip**
   - Use WSJT-X to generate reference audio files
   - Decode with our decoder, compare results

---

## Part 5: What We Actually Have Today

| Capability | Status | Confidence |
|------------|--------|------------|
| Encode standard FT8 messages | Working | High (bit-exact with ft8_lib) |
| Encode free text messages | Working | High (verified against reference) |
| LDPC(174,91) encoding | Working | High (syndrome verified) |
| CRC-14 computation | Working | High (matches ft8_lib algorithm) |
| Gray code mapping | Working | High (ft8_lib lookup tables) |
| Modulate to audio | Partially working | Medium (GFSK instead of CPFSK, soft-clip bug) |
| Detect FT8 signals in audio | Rewritten | Medium (Costas sync implemented, not yet validated on real signals) |
| Extract symbols from audio | Rewritten | Medium (complex DFT magnitude, not yet validated on real signals) |
| LDPC(174,91) decoding | Rewritten | Medium (soft LLR input, not yet validated on real signals) |
| Decode FT8 messages from audio | Rewritten | Medium (pipeline correct, blocked on modulator for round-trip) |
| End-to-end encode+decode | Blocked | Modulator bugs prevent validation |
| Interop with WSJT-X | Transmit only | Medium (encoder bit-exact, modulator has issues) |

---

## Appendix: File Reference

| File | Lines | Role | Status |
|------|-------|------|--------|
| `src/encoder.rs` | ~900 | FT8 message encoder | Working (with /R /P bugs) |
| `src/ldpc.rs` | ~850 | LDPC encode/decode, Gray code | Working |
| `src/message.rs` | ~1100 | CRC-14, message types, parsing | Working |
| `src/decoder.rs` | ~1100 | Full FT8 decoder pipeline | Rewritten (spectrogram + Costas sync + soft LLR) |
| `src/modulator.rs` | ~230 | 8-FSK audio modulation | Partially working |
| `src/lib.rs` | ~115 | Module exports, constants | OK |
| `tests/wsjtx_compat_tests.rs` | ~235 | WSJT-X reference comparison | All 12 pass |
| `tests/test_vectors.rs` | ~235 | Encoder determinism, structure | All pass |
| `tests/property_tests.rs` | ~180 | proptest fuzzing | All pass |
| `tests/round_trip_tests.rs` | ~310 | Encode-modulate-decode | Pass but don't assert decode |
| `tests/integration_tests.rs` | ~450 | Decoder integration | 3 fail, uses fake signals |
| `tests/transmission_tests.rs` | ~400 | Encoder unit tests | All pass |
