# Batch 52 — npre2 OSD preprocessing at osd_depth=3 (hard-200)

Re-measurement of `osd_npre2_preprocessing_enabled` on hard-200 with the OSD depth lifted to 3 so the npre2 hash-table warm start actually fires (the mechanism is a no-op at the default `osd_depth = Some(1)`).

Baseline config: `max_decode_passes = 2`, `ldpc_iterations = 200`, `osd_depth = Some(3)`. Reference TPs at default `osd_depth = Some(1)` from prior batches: **5301**.

| Config | Decodes | TPs | Δ vs depth=3 baseline | Δ vs default-depth (5301) | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|---:|
| baseline (depth=3, npre2 OFF) | 9844 | 5279 | 0 | -22 | 0.5363 | 626.4s |
| npre2 ON (depth=3) | 10000 | 5285 | +6 | -16 | 0.5285 | 559.8s |

> **STOP-CONDITION TRIGGERED**: depth=3 baseline (TPs=5279) differs from default-depth reference (5301) by -22 TPs (>±20). The OSD depth lift itself is a meaningful intervention; treat depth=3 as a separate ship candidate rather than a neutral re-baseline for npre2.

