# Batch 78 — knob audit (full) on raw_530_full (ft8_lib truth)

Baseline row: default (mp=1 ldpc=100) [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | default (mp=1 ldpc=100) [DEFAULT] | 46906 | 37839 | +0 | 9067 | +0 | 0.8067 | +0.0000 | 550s |
| 1 | mp=2 | 47269 | 37873 | +34 | 9396 | +329 | 0.8012 | -0.0055 | 1572s |
| 2 | Fast preset (mp=2 ldpc=200) | 47350 | 37896 | +57 | 9454 | +387 | 0.8003 | -0.0064 | 2165s |
| 3 | mp=3 | 47275 | 37875 | +36 | 9400 | +333 | 0.8012 | -0.0055 | 1635s |
