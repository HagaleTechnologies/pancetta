# Batch 78 — knob audit (full) on raw_530_full (ft8_lib truth)

Baseline row: default (mp=1 ldpc=100) [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | default (mp=1 ldpc=100) [DEFAULT] | 17891 | 14501 | +0 | 3390 | +0 | 0.8105 | +0.0000 | 587s |
| 1 | mp=2 | 18146 | 14518 | +17 | 3628 | +238 | 0.8001 | -0.0105 | 1056s |
| 2 | Fast preset (mp=2 ldpc=200) | 18179 | 14532 | +31 | 3647 | +257 | 0.7994 | -0.0111 | 1407s |
| 3 | mp=3 | 18153 | 14520 | +19 | 3633 | +243 | 0.7999 | -0.0107 | 1100s |
