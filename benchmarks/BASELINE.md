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
