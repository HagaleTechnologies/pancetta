# Batch 53 — Batch 52 mechanisms measurement

Auto_passband + cross-sequence A7 (hb-237 Session 3) measured on hard-200 and chrono_replay_mini33.

Base config: `max_decode_passes = 2, ldpc_iterations = 200`. Reference baseline TPs on hard-200 from prior batches: **5301**.

## Probe A — hard-200 (200 independent slots)

| Config | Decodes | TPs | Δ vs baseline | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|
| baseline (both OFF) | 7082 | 5301 | +0 | 0.7485 | 435.2s |
| auto_passband ON | 5094 | 3608 | -1693 | 0.7083 | 454.1s |
| cross_seq ON (empty seeds) | 7082 | 5301 | +0 | 0.7485 | 447.6s |
| both ON | 5095 | 3608 | -1693 | 0.7081 | 454.9s |

## Probe B — chrono_replay_mini33 (33 sequenced slots)

Slots walked in `slot_index` order. With cross_seq ON, seeds for slot N+1 are harvested from slot N's `from_callsign`/`to_callsign` (uppercased, dedup). `cs_emits` counts decodes EMITTED by `try_cross_sequence_decodes` that the standard pipeline didn't already produce.

| Config | Decodes | TPs | cs_emits | Δ TPs | Precision | Elapsed |
|---|---:|---:|---:|---:|---:|---:|
| cross_seq OFF (baseline) | 902 | 690 | 0 | +0 | 0.7650 | 62.0s |
| cross_seq ON | 989 | 690 | 87 | +0 | 0.6977 | 62.5s |

## Interpretation guide

- Probe A row 3 (cross_seq ON, no seeds): the consumer's defense-in-depth empty-seed early-return should make this byte-identical to the baseline. Any Δ ≠ 0 indicates the pipeline isn't actually byte-identical even when the consumer no-ops — investigate.
- Probe A row 2 (auto_passband ON): isolates the auto-passband effect. The mechanism rebuilds the per-slot passband to the noise-floor 95th percentile; on hard-200 most slots are noise-floor-bounded already so the lift is expected to be small.
- Probe B Δ TPs measures the **end-to-end** value of cross-sequence A7: extra TPs recovered from prior-slot callsign seeds. The `cs_emits` column measures *attempts*; not all of those will be in truth.
