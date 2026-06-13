# Batch 78 — knob audit (subset-200) on raw_530_full (ft8_lib truth)

## Stage 1: 3×3 lattice + min_sync_score spot checks

Baseline row: cands=300 parity=6 [DEFAULT]

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | cands=150 parity=3 | 4116 | 3425 | -2 | 691 | -143 | 0.8321 | +0.0278 | 39s |
| 1 | cands=150 parity=6 | 4116 | 3425 | -2 | 691 | -143 | 0.8321 | +0.0278 | 40s |
| 2 | cands=150 parity=10 | 4116 | 3425 | -2 | 691 | -143 | 0.8321 | +0.0278 | 45s |
| 3 | cands=300 parity=3 | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 111s |
| 4 | cands=300 parity=6 [DEFAULT] | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 102s |
| 5 | cands=300 parity=10 | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 111s |
| 6 | cands=600 parity=3 | 4285 | 3427 | +0 | 858 | +24 | 0.7998 | -0.0045 | 313s |
| 7 | cands=600 parity=6 | 4285 | 3427 | +0 | 858 | +24 | 0.7998 | -0.0045 | 329s |
| 8 | cands=600 parity=10 | 4285 | 3427 | +0 | 858 | +24 | 0.7998 | -0.0045 | 347s |
| 9 | min_sync_score=2.5 (cands=300 parity=6) | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 102s |
| 10 | min_sync_score=3.5 (cands=300 parity=6) | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 102s |

Reads: `max_parity_errors_for_osd` fully inert at every cands level
(expected post-Batch-72: with osd_depth=Some(0) the parity gate never
binds). `min_sync_score` ±0.5 also inert at cands=300 — the candidate cap
binds before the score threshold does. `max_sync_candidates` is the only
live knob: 150 → −2 TPs / −143 FPs / 2.5× faster; 600 → +0 TPs / +24 FPs /
3× slower (cap-displacement: extra candidates are all noise).

## Stage 1.5: cands knee refinement (parity=6)

| # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | cands=300 [DEFAULT] | 4261 | 3427 | +0 | 834 | +0 | 0.8043 | +0.0000 | 103s |
| 1 | cands=75 | 3804 | 3346 | -81 | 458 | -376 | 0.8796 | +0.0753 | 27s |
| 2 | cands=100 | 3962 | 3410 | -17 | 552 | -282 | 0.8607 | +0.0564 | 30s |
| 3 | cands=200 | 4204 | 3427 | +0 | 777 | -57 | 0.8152 | +0.0109 | 55s |
| 4 | cands=250 | 4239 | 3427 | +0 | 812 | -22 | 0.8084 | +0.0042 | 75s |

Knee: cands=200 keeps ALL subset TPs, −57 FPs, 1.9× faster. cands=150
costs 2 TPs for −143 FPs. Below 150 the cap starts displacing real TPs
fast. Stage-2 frontier: {300 default, 200, 150} on the full 2066 slots.
