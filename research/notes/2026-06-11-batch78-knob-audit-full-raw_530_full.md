# Batch 78 — knob audit (full) on raw_530_full (ft8_lib truth)

Baseline row: default (mp=1 ldpc=100) [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | default (mp=1 ldpc=100) [DEFAULT] | 46978 | 37841 | +0 | 9137 | +0 | 0.8055 | +0.0000 | 661s |
| 1 | mp=2 | 47690 | 37905 | +64 | 9785 | +648 | 0.7948 | -0.0107 | 1708s |
| 2 | Fast preset (mp=2 ldpc=200) | 47780 | 37934 | +93 | 9846 | +709 | 0.7939 | -0.0116 | 2366s |
| 3 | mp=3 | 47709 | 37907 | +66 | 9802 | +665 | 0.7945 | -0.0110 | 1924s |
