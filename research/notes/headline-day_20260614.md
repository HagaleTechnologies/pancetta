# Batch 87 — headline numbers under hash-normalized scoring (hb-248)

Current production defaults; raw = exact-text scoring (all prior
batches); norm = `hash_normalize_message` on both sides.

## day_20260614

6706 decodes, 6194 truth messages.

| Scoring | TPs | FPs | Precision | Truth found | Miss rate |
|---|---:|---:|---:|---:|---:|
| raw exact-text | 5747 | 959 | 0.8570 | 5747 | 7.22% |
| hash-normalized | 5932 | 774 | 0.8846 | 5932 | 4.23% |

Resolved-hash residual mismatches (conservative rule's leftover): 0

