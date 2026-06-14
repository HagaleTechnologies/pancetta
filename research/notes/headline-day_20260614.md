# Batch 87 — headline numbers under hash-normalized scoring (hb-248)

Current production defaults; raw = exact-text scoring (all prior
batches); norm = `hash_normalize_message` on both sides.

## day_20260614

3879 decodes, 3409 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 3163 | 716 | 0.8154 | 3163 | 7.22% |
| hash-normalized | 3278 | 601 | 0.8451 | 3278 | 3.84% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

