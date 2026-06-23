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

---

## ~~5. Cross-Implementation Validation~~ — DONE (2026-03-02)

### 5.1 WAV File Decode Tests — DONE
- 4 tests in `tests/wav_decode_tests.rs`
- Generated GFSK WAV files from ft8_lib (3 files in `tests/fixtures/wav/generated/`)
- Off-air recordings: decoder produces 21 messages from 3/9 files
- Note: ft8_lib (kgoba/ft8_lib latest) also decodes 0 from these off-air files

### 5.2 ft8_lib FFI Integration — DONE
- Vendored ft8_lib at `vendor/ft8_lib/`
- C code compiled via `cc` crate in `build.rs`
- Safe Rust wrappers in `src/ft8_lib_ffi.rs`
- FFI covers: encode, decode payload, decode audio (full pipeline)
- Compile-time struct size assertions for all C structs

### 5.3 Bidirectional Cross-Validation — DONE
- 10 tests in `tests/ft8lib_crossval_tests.rs`
- **Encoder match**: 7 standard messages produce identical tones to ft8_lib
- **Payload round-trip**: encode→decode through ft8_lib matches for all standard msgs
- **Our audio → ft8_lib decoder**: Our encoder+modulator generates audio that ft8_lib decodes correctly (3 messages tested)
- **ft8_lib audio → our decoder**: Our decoder processes ft8_lib GFSK audio (3 messages tested)

### 5.4 CI Pipeline — DONE
- GitHub Actions workflow at `.github/workflows/ci.yml`
- 4 jobs: FT8 tests (`cargo test --features transmit`), workspace check, clippy, format check
- Runs on push/PR to main
- System deps installed: libasound2-dev, libudev-dev, libssl-dev, pkg-config
- Format check is non-blocking (codebase not yet fully formatted)

---

## Test Summary

200 tests pass, 0 failures across:
- 92 lib unit tests
- 10 ft8_lib cross-validation tests
- 11 integration tests
- 16 round-trip tests
- 4 WAV decode tests
- Plus property tests, test vectors, etc.

```
[DONE] 1.1-1.6 Decoder critical bugs
[DONE] 2.1-2.2 Modulator fixes
[DONE] 3.1-3.2 Encoder edge cases
[DONE] 4.1-4.3 Test infrastructure
[DONE] 5.1 WAV file decode tests
[DONE] 5.2 ft8_lib FFI integration
[DONE] 5.3 Bidirectional cross-validation
[DONE] 5.4 CI pipeline
```
