# Batch 69 — LLR whitening re-verified with ft8_lib truth

hard_1000, ft8_lib truth (Batch 66 source).

| Config | Decodes | TPs | FPs | Precision |
|---|---:|---:|---:|---:|
| baseline (OFF) | 22364 | 14514 | 7850 | 0.6490 |
| whitening ON | 21656 | 14523 | 7133 | 0.6706 |

Δ TPs: +9 | Δ FPs: -717

**LLR whitening graduation HOLDS with ft8_lib truth**: TPs +9, FPs -717. Graduation is truth-source-independent.
