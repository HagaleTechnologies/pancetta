# FT8 Encoder/Decoder: Honest Assessment

_Written 2026-03-01. Updated 2026-03-02 after completing all encoder/decoder/modulator fixes._
_Based on thorough reading of all source files, tests, and comparison against
ft8_lib/WSJT-X reference implementation._

---

## Executive Summary

**The encoder is genuinely correct.** It produces bit-exact output matching
ft8_lib for payloads, LDPC codewords, CRC-14, Gray code mapping, and all 79
symbols. This is verified by 15 WSJT-X compatibility tests.

**The decoder works end-to-end.** All 6 critical bugs from the original
assessment were fixed. The message parser was rewritten with correct mixed-radix
callsign decoding, grid unpacking, and free text parsing. Full round-trip
(encode → modulate → decode) is verified for all standard FT8 message types.

**The modulator is clean.** Pure 8-CPFSK with rectangular pulse shape, continuous
phase accumulation, and 0.95 peak normalization. No Gaussian filtering, no
soft-clip discontinuity.

**186 tests pass, 0 failures.**

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
| /R /P suffix encoding | Correct | ip flags verified, i3=1 always for standard messages |

### Decoder (VERIFIED - round-trip with encoder)

| Component | Status | Evidence |
|-----------|--------|----------|
| Costas sync detection | Working | 2D spectrogram search with NMS, MIN_SYNC_SCORE=8.0 |
| Frequency refinement | Working | ±1 bin search compensates for Hann window leakage |
| Complex DFT symbol extraction | Working | cos+sin at 8 tone frequencies, magnitude-based |
| Soft LLR computation | Working | Max-log approximation with Gray code bit mapping |
| LDPC belief propagation | Working | Soft-input BP decoder, max 50 iterations |
| CRC-14 verification | Working | Same algorithm as encoder |
| Message parsing | Working | Correct i3 extraction (bits 74-76), mixed-radix callsign decode |
| Grid/report unpacking | Working | MAXGRID4=32400, special tokens (RRR, RR73, 73) |
| Free text decoding | Working | i3=0 n3=0, base-42, 13 characters |
| Suffix handling | Working | ip=1 → append /R to callsign |

### Modulator (VERIFIED - round-trip with decoder)

| Component | Status | Evidence |
|-----------|--------|----------|
| 8-CPFSK modulation | Working | Rectangular pulse shape, continuous phase |
| Peak normalization | Working | 0.95 peak + dither |
| Frequency offset | Working | Tested at ±100 Hz offsets |

### Test Coverage

| Category | Count | What |
|----------|-------|------|
| Library unit tests | 92 | LDPC, CRC, Gray code, encoder, decoder internals |
| Integration tests | 11 | Real signals at various SNR, configs |
| Property tests | 11 | proptest: LDPC validity, Gray code bijection, CRC sensitivity |
| Round-trip tests | 16 | All message types: CQ, DX, grid, report, RRR, 73, RR73, free text |
| Signal generator tests | 5 | Signal generation utilities |
| Test vectors | 8 | Determinism, Costas arrays, CRC reference values |
| Transmission tests | 26 | Encoder unit tests |
| WSJT-X compat tests | 15 | Bit-exact comparison with ft8_lib reference values |
| Doc tests | 2 | API examples |
| **Total** | **186** | **All passing** |

---

## Part 2: What Remains

### Not Yet Validated Against External Implementations

The round-trip tests prove internal consistency (our encoder → our decoder),
but do not prove interoperability with WSJT-X or ft8_lib. Specifically:

1. **Our decoder has not decoded real off-air FT8 signals** — 9 WAV files exist
   in `tests/fixtures/wav/` but no code reads them yet.

2. **Our encoder output has not been decoded by ft8_lib** — bit-exact payloads
   and symbols are verified, but the full audio → decode path through an
   independent decoder has not been tested.

3. **Our decoder has not decoded ft8_lib-generated signals** — no reference
   WAV files from ft8_lib have been fed to our decoder.

### Known Limitations

| Limitation | Impact |
|------------|--------|
| /R and /P suffixes are indistinguishable | Protocol-level: both encode as ip=1 |
| No i3=2 (EU VHF contest) support | Contest exchanges not decoded |
| No i3=3 (ARRL RTTY Roundup) support | Contest exchanges not decoded |
| No i3=4 (nonstandard callsign) support | Unusual callsigns not decoded |
| Decoder not optimized for speed | Works but not benchmarked against real-time requirement |
| Single-threaded decoding | No parallel candidate processing |

---

## Part 3: File Reference

| File | Lines | Role | Status |
|------|-------|------|--------|
| `src/encoder.rs` | ~900 | FT8 message encoder | Working (bit-exact with ft8_lib) |
| `src/ldpc.rs` | ~850 | LDPC encode/decode, Gray code | Working |
| `src/message.rs` | ~1100 | CRC-14, message types, parsing | Working (rewritten 2026-03-01) |
| `src/decoder.rs` | ~1100 | Full FT8 decoder pipeline | Working (rewritten 2026-03-01) |
| `src/modulator.rs` | ~230 | 8-CPFSK audio modulation | Working (fixed 2026-03-01) |
| `src/lib.rs` | ~115 | Module exports, constants | OK |
| `tests/wsjtx_compat_tests.rs` | ~365 | WSJT-X reference comparison | 15 pass |
| `tests/round_trip_tests.rs` | ~305 | Encode-modulate-decode | 16 pass, all assert |
| `tests/integration_tests.rs` | ~300 | Decoder integration | 11 pass, real signals |
| `tests/transmission_tests.rs` | ~530 | Encoder unit tests | 26 pass |
| `tests/property_tests.rs` | ~250 | proptest fuzzing | 11 pass |
| `tests/test_vectors.rs` | ~235 | Encoder determinism, structure | 8 pass |
| `tests/test_signal_generator.rs` | ~260 | Signal generation utilities | 5 pass |
