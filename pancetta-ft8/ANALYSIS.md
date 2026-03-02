# FT8 Encoder/Decoder: Honest Assessment

_Written 2026-03-01. Based on thorough reading of all source files, tests, and
comparison against ft8_lib/WSJT-X reference implementation._

---

## Executive Summary

**The encoder is genuinely correct.** It produces bit-exact output matching
ft8_lib for payloads, LDPC codewords, CRC-14, Gray code mapping, and all 79
symbols. This is verified by 12 WSJT-X compatibility tests that compare against
independently-computed reference values.

**The decoder has never decoded an FT8 message.** Not in tests, not in
production. The decode pipeline has multiple fundamental bugs that make it
structurally incapable of decoding even a clean, high-SNR signal. The
integration tests use fake signals (ASCII % 8, not real FT8 encoding), and the
round-trip tests don't assert that decoding actually succeeds.

**We are half-done.** The transmit side is solid. The receive side needs to be
substantially rewritten.

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

- 169 tests pass total
- 12 WSJT-X compatibility tests verify bit-exact output
- Property tests (proptest) cover LDPC validity, Gray code bijection, CRC sensitivity
- Determinism tests verify same input -> same output across instances

---

## Part 2: What Is NOT Verified (Decoder)

### Critical Bug 1: No Costas Sync Detection

The sync engine (`sync.rs`) does generic multi-tone energy detection. It never
correlates against the Costas array [3,1,4,0,6,5,2]. Without proper sync
detection, the decoder cannot determine where an FT8 message starts in time or
at what frequency offset.

Real FT8 sync works by computing a 2D correlation of the 21 known Costas
symbols (at positions 0-6, 36-42, 72-78) against a time-frequency spectrogram.
The decoder does none of this.

### Critical Bug 2: LocalDecoder Is a Stub

`LocalDecoder::decode_single_candidate` (decoder.rs ~line 1314) ignores the
audio entirely and returns `Ft8Message::default()` for any candidate with
SNR > -15 and confidence > 0.7. This is the parallel decode path, which
activates when >4 candidates are found (the default `max_candidates = 50`).

### Critical Bug 3: Symbol Extraction Uses Cosine-Only Correlation

`correlate_with_tone` uses only the real (cosine) component of a single-
frequency DFT. The result depends on the unknown carrier phase, making symbol
extraction unreliable even at high SNR. A correct implementation uses the
complex magnitude: sqrt(real^2 + imag^2).

### Critical Bug 4: LDPC Decoder Gets Hard-Decision Input

The belief propagation decoder receives hard bits (0/1) converted to fixed
+/-4.0 LLR. All soft reliability information from the correlator is discarded.
This severely degrades error correction capability. Real FT8 decoders pass
actual log-likelihood ratios computed from tone correlation magnitudes.

### Critical Bug 5: Coherent Averaging Phase Computation Is Wrong

`coherent_symbol_averaging` (decoder.rs ~line 490) computes phase using the
window index directly (`point.time_window as f64 * 0.16`), but `time_window`
is a window count, not a time-in-seconds value. The actual time should be
`time_window * hop_size / sample_rate`. This makes "coherent" averaging
incoherent.

### Critical Bug 6: `is_ft8_like_signal` Always Returns True

The signal validation function (decoder.rs ~line 777) is a stub that
unconditionally returns `true`. Every spectral peak becomes a decode candidate.

### Modulator Issues

| Issue | Severity | Detail |
|-------|----------|--------|
| Gaussian filtering | Medium | FT8 uses CPFSK (abrupt frequency transitions), not GFSK. `GAUSSIAN_BT=2.0` blurs symbol boundaries. |
| Soft-clip discontinuity | High | `soft_clip(0.5)` returns 0, creating a jump from 0.5 (linear) to 0 (clipped). Combined with normalization to 0.95, this clips most of the signal. |

### Integration Tests Use Fake Signals

`message_to_test_tones` in integration_tests.rs generates tones via
`(ch as u8 % 8)` -- ASCII character values modulo 8. These are not FT8-encoded
signals. They have no Costas arrays, no LDPC parity, no valid CRC. The
integration tests verify the decoder doesn't crash, not that it decodes.

### Round-Trip Tests Don't Assert Decode Success

`test_round_trip_cq_clean` and similar tests encode a message, modulate to
audio, feed to the decoder, then `println!` the results without asserting that
any message was decoded. The comment admits: "the decoder's sync/correlation
pipeline may not perfectly align."

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

### Phase B: Fix the Decoder (Priority 2)

The decoder needs fundamental fixes before any of the above tests can pass.
Ordered by dependency:

1. **Implement real Costas sync detection**
   - Compute 2D time-frequency spectrogram (short FFT, overlapping windows)
   - For each (time_offset, freq_offset), correlate the 21 known Costas symbol
     positions against the spectrogram
   - Select the (time, freq) with maximum correlation
   - This is the single most important fix

2. **Fix symbol extraction to use complex DFT magnitude**
   - For each data symbol position, compute complex DFT at each of the 8 tone
     frequencies
   - Use magnitude (not just cosine component) to determine the most likely tone
   - Compute soft log-likelihood ratios from magnitude differences

3. **Pass soft LLRs to LDPC decoder**
   - For each of 174 codeword bits, compute LLR from the 8-tone magnitude vector
   - Pass these to belief propagation instead of hard +/-4.0

4. **Remove the LocalDecoder stub**
   - Either remove the parallel decode path entirely or implement it properly

5. **Fix modulator**
   - Remove Gaussian filtering (use CPFSK, not GFSK)
   - Fix soft-clip discontinuity

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
| Detect FT8 signals in audio | Not working | None (no Costas sync) |
| Extract symbols from audio | Not working | None (phase-dependent correlation) |
| LDPC(174,91) decoding | Untested | Low (algorithm correct but never exercised with real data) |
| Decode FT8 messages from audio | Not working | None (multiple critical bugs) |
| End-to-end encode+decode | Not working | None |
| Interop with WSJT-X | Transmit only | Medium (encoder bit-exact, modulator has issues) |

---

## Appendix: File Reference

| File | Lines | Role | Status |
|------|-------|------|--------|
| `src/encoder.rs` | ~900 | FT8 message encoder | Working (with /R /P bugs) |
| `src/ldpc.rs` | ~850 | LDPC encode/decode, Gray code | Working |
| `src/message.rs` | ~1100 | CRC-14, message types, parsing | Working |
| `src/decoder.rs` | ~1400 | Full FT8 decoder pipeline | Broken (multiple critical bugs) |
| `src/modulator.rs` | ~230 | 8-FSK audio modulation | Partially working |
| `src/lib.rs` | ~115 | Module exports, constants | OK |
| `tests/wsjtx_compat_tests.rs` | ~235 | WSJT-X reference comparison | All 12 pass |
| `tests/test_vectors.rs` | ~235 | Encoder determinism, structure | All pass |
| `tests/property_tests.rs` | ~180 | proptest fuzzing | All pass |
| `tests/round_trip_tests.rs` | ~310 | Encode-modulate-decode | Pass but don't assert decode |
| `tests/integration_tests.rs` | ~450 | Decoder integration | 3 fail, uses fake signals |
| `tests/transmission_tests.rs` | ~400 | Encoder unit tests | All pass |
