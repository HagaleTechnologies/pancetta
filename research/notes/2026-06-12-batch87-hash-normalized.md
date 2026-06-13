# Batch 87 — headline numbers under hash-normalized scoring (hb-248)

Current production defaults; raw = exact-text scoring (all prior
batches); norm = `hash_normalize_message` on both sides.

## raw_530_full

46906 decodes, 39668 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 37839 | 9067 | 0.8067 | 37839 | 4.61% |
| hash-normalized | 38605 | 8301 | 0.8230 | 38605 | 2.68% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

## hard_1000

17787 decodes, 15329 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 14489 | 3298 | 0.8146 | 14489 | 5.48% |
| hash-normalized | 14709 | 3078 | 0.8270 | 14709 | 4.04% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

