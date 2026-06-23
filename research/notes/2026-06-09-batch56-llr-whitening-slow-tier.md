# Batch 56 — LLR whitening Slow-tier preset on hard_1000

Verification that Batch 53's default-ON graduation extends to the Slow-tier preset (`max_decode_passes = 1, osd_depth = Some(1)`), which is the FTdx10 MiniPC production target.

| Config | Decodes | TPs | FPs | Δ TPs | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|---:|
| Slow baseline (OFF) | 18099 | 16226 | 1873 | 0 | 0.8965 | 778.1s |
| Slow whitening ON | 18105 | 16240 | 1865 | +14 | 0.8970 | 780.1s |

**Δ FPs**: -8

## Decision

**Slow-tier mirror of Fast-tier**: TPs ↑ AND precision ↑. Default-ON is correct across all tiers; no tier-conditional flipping needed.

## Comparison to Batch 53 (Fast/Moderate tier)

- Batch 53 (mp=2, ldpc=200) on hard_1000: +4 TPs / -713 FPs / +3.3% precision
- Batch 56 (mp=1, osd_depth=1) on hard_1000: +14 TPs / -8 FPs / precision Δ 0.0005
