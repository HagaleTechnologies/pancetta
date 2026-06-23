# Batch 84 — ship validation on remaining corpus types (ft8_lib truth)

TODAY = current defaults; PRE-SHIP = cands=300 + mp=2 + ldpc=200
(the effective Fast-tier config before Batches 78/83).

## sparse_419 (300 slots)

| Config | Decodes | TPs | Δ TPs | FPs | Δ FPs | Wall |
|---|---:|---:|---:|---:|---:|---:|
| PRE-SHIP | 40 | 27 | +0 | 13 | +0 | 103s |
| TODAY | 40 | 27 | +0 | 13 | +0 | 37s |

## qso_continuous_530 (500 slots)

| Config | Decodes | TPs | Δ TPs | FPs | Δ FPs | Wall |
|---|---:|---:|---:|---:|---:|---:|
| PRE-SHIP | 11507 | 9074 | +0 | 2433 | +0 | 769s |
| TODAY | 11258 | 9056 | -18 | 2202 | -231 | 132s |

