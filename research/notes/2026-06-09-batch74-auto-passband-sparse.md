# Batch 74 — auto_passband on sparse_419 (ft8_lib truth)

300 slots from raw 4/19 day (sparse signal, ~3.6 decodes/slot).

| Config | Decodes | TPs | FPs | Precision |
|---|---:|---:|---:|---:|
| OFF | 40 | 27 | 13 | 0.6750 |
| ON | 31 | 20 | 11 | 0.6452 |

Δ TPs: -7, Δ FPs: -2

**auto_passband still inert/regressive on sparse**: Δ TPs = -7, Δ FPs = -2. Same Batch 55 verdict; stays default-OFF.
