# Batch 53 — LLR whitening on hard_1000

Re-measurement of Batch 50's +2 TPs / +2.7% precision finding at 5× corpus scale.

Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only `llr_whitening_enabled` toggled.

| Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|
| baseline (whitening OFF) | 22365 | 16365 | 0 | 0.7317 | 2201.0s |
| whitening ON | 21656 | 16369 | +4 | 0.7559 | 2229.7s |

**Recommend default-ON in Batch 54**: TPs ↑ AND precision ↑ at 5× corpus scale.
