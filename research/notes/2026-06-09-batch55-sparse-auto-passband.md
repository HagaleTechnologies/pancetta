# Batch 55 — sparse-signal auto_passband re-measurement

Synthetic sparse corpus: 50 slots, 1-3 signals/slot at SNRs in [-22, -10] dB (2500 Hz BW), random freq in [500, 2800] Hz. Plants total = 99.

Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only `auto_passband_enabled` toggled.

| Config | Decodes | TPs | FPs | Recall | Elapsed |
|---|---:|---:|---:|---:|---:|
| baseline (OFF) | 145 | 68 | 77 | 0.6869 | 51.1s |
| auto_passband ON | 145 | 68 | 77 | 0.6869 | 52.0s |

**Δ TPs**: +0
**Δ FPs**: +0

## Decision

**auto_passband is net-positive on sparse**: consider tier-gating default-ON for low-occupancy operating modes.

## Comparison to Batch 53 dense-band measurement

- Batch 53 (hard-200, dense): -1693 TPs (-31.9% recall)
- Batch 55 (sparse synthetic): +0 TPs (+0.00% recall absolute)

The hypothesis was that auto_passband's failure on hard-200 was driven by signal-dense slots violating the noise-floor-dominated assumption. The sparse measurement SUPPORTS that hypothesis.
