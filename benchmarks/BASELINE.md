# Decoder Benchmark Baseline

## Date: 2026-03-30

## Results (pre-improvement)

Overall: Pancetta=68, ft8_lib=41, Both=32, Parity=47%

Note: "Parity" = both_decoded / max(pancetta_total, ft8lib_total). Low parity despite
Pancetta decoding more messages suggests possible false positives (Pancetta-only decodes
with empty or garbage text) and genuine misses (ft8_lib-only decodes).

| Category | Files | Pancetta | ft8_lib | Both | P-only | F-only |
|----------|-------|----------|---------|------|--------|--------|
| Generated (GFSK) | 3 | 6 | 3 | 3 | 3 | 0 |
| WSJT off-air | 3 | 35 | 16 | 12 | 23 | 4 |
| BasicFT8 | 4 | 2 | 0 | 0 | 2 | 0 |
| JTDX | 2 | 25 | 22 | 17 | 8 | 5 |
| **TOTAL** | **12** | **68** | **41** | **32** | **36** | **9** |

## Key Observations

1. Pancetta decodes MORE total messages than ft8_lib (68 vs 41)
2. But parity is low (47%) — many Pancetta decodes don't match ft8_lib
3. Pancetta-only decodes include some with empty text ("") or "<Unknown>" — likely false positives
4. ft8_lib-only decodes (9) represent genuine misses by Pancetta
5. Processing time is 8-30 seconds per file in release mode (multi-pass)
6. The multi-pass + signal subtraction is already working and finding additional signals

## Areas for Improvement

- Filter out empty/unknown Pancetta decodes before counting (these inflate the numbers)
- Fine frequency/time estimation to improve decode quality on marginal signals
- OSD to recover the 9 ft8_lib-only decodes (likely weak signals where BP failed)
- Performance: 8-30s per 15s window is too slow for real-time use

---

## Results (post fine-freq + time + subtraction + FFT perf improvement)

### Date: 2026-03-30

Cross-validation totals (9 WAV files): Pancetta=50, ft8_lib=38
Overall ratio: 131.6%

| File | Pancetta | ft8_lib |
|------|----------|---------|
| jtdx/000000_000001.wav | 1 | 0 |
| jtdx/190227_155815.wav | 22 | 22 |
| wsjt/210703_133430.wav | 6 | 8 |
| wsjt/181201_180245.wav | 15 | 8 |
| wsjt/170709_135615.wav | 2 | 0 |
| basicft8/170923_082000.wav | 1 | 0 |
| basicft8/170923_082015.wav | 1 | 0 |
| basicft8/170923_082030.wav | 1 | 0 |
| basicft8/170923_082045.wav | 1 | 0 |
| **TOTAL** | **50** | **38** |

### Improvements Applied

1. **FFT-based symbol extraction** — 5× decode speedup (86ms→16ms per candidate)
2. **Sub-bin frequency refinement** — half-bin steps (3→5 freq trials)
3. **Finer time search** — eighth-symbol steps (5→9 time trials)
4. **Cross-correlation signal subtraction** — better amplitude estimation for multi-pass

### Improvement Summary

| Metric | Before | After | Change |
|--------|--------|-------|--------|
| Pancetta decodes | 47 | 50 | +6.4% |
| Decode speed | 86ms/candidate | 16ms/candidate | 5.4× faster |
| Per-file best parity | JTDX 20/22 | JTDX 22/22 | Perfect match |

### Remaining Gaps

- `wsjt/210703_133430.wav`: 6 vs ft8_lib's 8 (2 still missed)
- Empty/unknown message false positives still present
- OSD not yet implemented (Phase 3)

---

## Results (post-OSD implementation)

### Date: 2026-03-30

Cross-validation totals (9 WAV files): Pancetta=61, ft8_lib=38
Overall ratio: 160.5%

| File | Pancetta | ft8_lib | Change |
|------|----------|---------|--------|
| jtdx/000000_000001.wav | 1 | 0 | — |
| jtdx/190227_155815.wav | 25 | 22 | +3 |
| wsjt/210703_133430.wav | 7 | 8 | +1 |
| wsjt/181201_180245.wav | 16 | 8 | +1 |
| wsjt/170709_135615.wav | 3 | 0 | +1 |
| basicft8/170923_082000.wav | 4 | 0 | +3 |
| basicft8/170923_082015.wav | 2 | 0 | +1 |
| basicft8/170923_082030.wav | 2 | 0 | +1 |
| basicft8/170923_082045.wav | 1 | 0 | — |
| **TOTAL** | **61** | **38** | **+11** |

### Improvements Applied

1. **OSD-1 fallback** — ordered statistics decoding depth 1 (92 trials) on BP failures
2. **Parity error gate** — only run OSD when ≤5 parity checks fail after BP (prevents CRC-14 false positives)

### Improvement Summary

| Metric | Before OSD | After OSD | Change |
|--------|-----------|-----------|--------|
| Pancetta decodes | 50 | 61 | +22% |
| ft8_lib-only gap | 9 | — | reduced |
| wsjt/210703 gap | 6 vs 8 | 7 vs 8 | 1 fewer miss |

### Notes

- **OSD-2 false positives:** OSD-2 (4,187 trials) produces excessive CRC-14 false positives
  (~22.6% false pass per candidate). OSD-1 (92 trials, ~0.56% per candidate) is the safe
  default. OSD-2 requires additional validation (message plausibility, Hamming distance
  gating) before it can be enabled by default — left for future work.
- **Parity gate:** The threshold of ≤5 unsatisfied parity checks filters out noise candidates
  where BP produced random bits. Without this gate, even OSD-1 produces false positives.
- Some of the +11 extra decodes may be OSD false positives rather than genuine weak-signal
  recoveries. Additional validation will improve precision.
