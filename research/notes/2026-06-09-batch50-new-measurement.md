# Batch 50 — new mechanism toggle measurement

| Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|
| baseline (all Batch 50 new OFF) | 7080 | 5301 | +0 | 0.7487 | 488.5s |
| LLR whitening ON | 6897 | 5303 | +2 | 0.7689 | 477.2s |
| per-candidate freq tracker ON | 7046 | 5299 | -2 | 0.7521 | 496.2s |
| 4th-pass-after-a7 ON | 7080 | 5301 | +0 | 0.7487 | 475.7s |
| all three new ON | 6894 | 5304 | +3 | 0.7694 | 492.9s |
