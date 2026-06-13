# Batch 87 — headline numbers under hash-normalized scoring (hb-248)

Current production defaults; raw = exact-text scoring (all prior
batches); norm = `hash_normalize_message` on both sides.

## day_20260613

4361 decodes, 3651 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 3492 | 869 | 0.8007 | 3492 | 4.35% |
| hash-normalized | 3555 | 806 | 0.8152 | 3555 | 2.63% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

