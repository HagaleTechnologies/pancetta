# FT8 Remaining Work

_Created 2026-03-01. Updated 2026-03-02. See ANALYSIS.md for the full honest assessment._

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
- **1.6 is_ft8_like_signal**: Removed (Costas sync score threshold serves as signal filter)

---

## ~~2. Fix Modulator Bugs~~ — DONE (2026-03-01)

- **2.1** Removed Gaussian filtering — pure 8-CPFSK with rectangular pulse shape
- **2.2** Removed soft-clip entirely — normalized to 0.95 peak + dither instead

---

## ~~3. Fix Encoder Edge Cases~~ — DONE (2026-03-01)

- **3.1** /R vs /P suffix: Both encode as `ip=1` per FT8 protocol (not distinguishable). This is correct behavior, not a bug.
- **3.2** Fixed `i3` field: Always `i3=1` for standard messages (was incorrectly using `i3=2` for /P callsigns)

---

## ~~4. Fix Test Infrastructure~~ — DONE (2026-03-01)

- **4.1** Integration tests now use real encoder+modulator signals (not fake `ch % 8` tones)
- **4.2** Round-trip tests assert decoded message matches input for all message types
- **4.3** Added /R and /P suffix encoder tests (pack28 flags, i3 field, round-trip)

186 tests pass. 0 failures.

---

## 5. Cross-Implementation Validation — IN PROGRESS

### 5.1 Decode Real Off-Air FT8 Signals
- **Files**: `tests/fixtures/wav/` (9 WAV files from JTDX, WSJT, BasicFT8)
- **Task**: Read WAV files, feed to decoder, verify we get reasonable FT8 messages
- **Requires**: `hound` crate for WAV reading

### 5.2 ft8_lib FFI Integration
- Build ft8_lib C code via `cc` crate
- Create Rust FFI bindings for `ft8_encode()` and `ft8_decode()`
- Compare our encoder output against ft8_lib's encoder
- Compare our decoder output against ft8_lib's decoder

### 5.3 Bidirectional Cross-Validation
- Generate WAV files with our encoder, decode with ft8_lib
- Generate WAV files with ft8_lib, decode with our decoder
- Gold standard: two independent implementations agree

### 5.4 CI Pipeline
- GitHub Actions workflow for `cargo test --features transmit -p pancetta-ft8`
- Run on push/PR to main
- Include cross-implementation tests

---

## Recommended Order

```
[DONE] 1.1-1.6 Decoder critical bugs
[DONE] 2.1-2.2 Modulator fixes
[DONE] 3.1-3.2 Encoder edge cases
[DONE] 4.1-4.3 Test infrastructure
5.1 Decode real WAV files
5.2 ft8_lib FFI integration
5.3 Bidirectional cross-validation
5.4 CI pipeline
```
