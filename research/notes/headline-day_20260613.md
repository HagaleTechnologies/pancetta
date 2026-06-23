# Batch 87 — headline numbers under hash-normalized scoring (hb-248)

Current production defaults; raw = exact-text scoring (all prior
batches); norm = `hash_normalize_message` on both sides.

## day_20260613

11293 decodes, 9578 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 9130 | 2163 | 0.8085 | 9130 | 4.68% |
| hash-normalized | 9303 | 1990 | 0.8238 | 9303 | 2.87% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

