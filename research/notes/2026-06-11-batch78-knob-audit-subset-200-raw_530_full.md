# Batch 78 — knob audit (subset-200) on raw_530_full (ft8_lib truth)

Baseline row: default (mp=1 ldpc=100) [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | default (mp=1 ldpc=100) [DEFAULT] | 4204 | 3427 | +0 | 777 | +0 | 0.8152 | +0.0000 | 53s |
| 1 | mp=2 | 4238 | 3432 | +5 | 806 | +29 | 0.8098 | -0.0054 | 147s |
| 2 | Fast preset (mp=2 ldpc=200) | 4240 | 3433 | +6 | 807 | +30 | 0.8097 | -0.0055 | 203s |
| 3 | mp=3 | 4238 | 3432 | +5 | 806 | +29 | 0.8098 | -0.0054 | 155s |
