# Batch 78 — knob audit (full) on raw_530_full (ft8_lib truth)

Baseline row: default (mp=1 ldpc=100) [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | default (mp=1 ldpc=100) [DEFAULT] | 17787 | 14489 | +0 | 3298 | +0 | 0.8146 | +0.0000 | 470s |
| 1 | mp=2 | 17918 | 14504 | +15 | 3414 | +116 | 0.8095 | -0.0051 | 885s |
| 2 | Fast preset (mp=2 ldpc=200) | 17953 | 14513 | +24 | 3440 | +142 | 0.8084 | -0.0062 | 1202s |
| 3 | mp=3 | 17921 | 14505 | +16 | 3416 | +118 | 0.8094 | -0.0052 | 914s |
