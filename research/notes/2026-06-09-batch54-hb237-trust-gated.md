# Batch 54 — hb-237 trust-gated cross-sequence A7 on chrono_replay full

Production-shape probe: `CallsignContinuityFilter` strict mode (rolling_cap=200), slot 0 bootstrap from own decodes. 300 sequenced slots from `chrono_replay.manifest.json`.

Config: `max_decode_passes = 2, ldpc_iterations = 200`. Only `cross_sequence_a7_enabled` toggled.

| Config | Decodes | TPs | FPs | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|
| baseline (cross_seq OFF) | 7454 | 2228 | 5226 | 0.2989 | 553.6s |
| cross_seq ON (trust-gated) | 8250 | 2228 | 6022 | 0.2701 | 567.9s |

**Δ TPs**: +0
**Δ FPs**: +796
**Δ Decodes**: +796

## Cross-sequence telemetry

- `cs_emits` (recoveries added by cross-sequence consumer): **796**
- `cs_recovered_tps` (of those, matched truth): **0**
- `cs_emit_fps` (of those, did NOT match truth): **796**
- Seeds offered by prior-slot decodes: **15038**
- Seeds passed trust gate: **9592** (63.8% trust-filter pass rate)

## Decision

**Keep default-OFF**: net-negative or FP-amplifying even with trust gate engaged.

## Comparison to Batch 53 mini33 (no trust gate)

- Batch 53 mini33: 87 cs_emits / 0 TPs / 87 FPs (precision crash 0.7650 → 0.6977)
- Batch 54 chrono_replay full: 796 cs_emits / 0 TPs / 796 FPs
- Per-slot rate (33 vs 300 slots) makes the comparison apples-to-apples after normalization.
