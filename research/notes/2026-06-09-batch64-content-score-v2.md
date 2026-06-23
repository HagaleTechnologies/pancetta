# Batch 64 — hb-103 v2 AUC measurement

| Formula | AUC | TPs | FPs |
|---|---:|---:|---:|
| v1 (Batch 31) | 0.9578 | 5304 | 1597 |
| v2 (Batch 64) | 0.9578 | 5304 | 1597 |

ΔAUC: +0.0000

**v2 is no-op on hard-200**: ConfidenceFeatures telemetry doesn't add discrimination signal beyond v1. Likely because hard-200's standard messages all converge fast at BP-direct (low BP iters, high min_llr, no OSD), so the telemetry doesn't help separate TPs from FPs. v1 stays as the default; v2 available for opt-in.

Elapsed: 438.7s
