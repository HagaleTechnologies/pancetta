# Batch 91 — hb-250 premise probe: matched-filter re-demod of sync-passing, LDPC-failing candidates

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-91`
Example: `pancetta-research/examples/batch91_hb250_failed_candidates.rs`
Corpus: top-20 hard-200 worst WAVs (Batch 90 selection via
`research/scorecards/main.json` per_wav_top_failures) + the 20 Batch-86
kill-switch slots (`research/corpus/curated/ft8/hb104_kill_switch.json`)
Truth: ft8_lib (`research/baselines/ft8/{sha}.ft8lib.json`, real freq/time
post-Batch-85), text comparisons hash-normalized via
`pancetta_research::metrics::hash_normalize_message`

## Hypothesis

Batch 90 showed the phase-coherent matched filter beats the production
spectrogram max-log demod on real decoded signals (controls p50 96.6% vs
94.3% with ±240-sample refinement, mean|LLR| +24%) while staying at exact
chance on sub-Costas missed truths. hb-250: sync candidates that PASSED
the Costas threshold but FAILED LDPC/CRC are positions where signal
demonstrably exists — a coherent second-chance demod there might flip
enough bits to converge some of them.

## Decoder change (option (c) from the brief)

No existing dump path covered this population (`debug-decode` feature
prints top-10 candidates to stderr only; hb-093's
`take_residual_snr_diagnostic` is joint_pair_retry-specific), and a
research-side sync replication would not reproduce the production
NMS/rerank/multipass candidate stream. So: smallest additive decoder
change, mirroring the hb-093 precedent —

- `pancetta-ft8/src/decoder.rs`: new `#[doc(hidden)] pub struct
  SyncCandidateRecord { pass, freq_hz, dt_s, start_sample, sync_score,
  decoded }` + `#[doc(hidden)] pub fn
  decode_window_with_candidate_dump(&mut self, samples) ->
  Ft8Result<(Vec<DecodedMessage>, Vec<SyncCandidateRecord>)>`.
- The per-pass AP0/AP candidate loop's init + per-candidate closures are
  bound to names (`ldpc_init`, `decode_candidate_op`); when the dump flag
  is set, results are collected as `Vec<Option<DecodedMessage>>` so each
  candidate is tagged with its decode outcome, then flattened to the same
  Vec. When the flag is false (every other entry point), the branch runs
  the exact pre-existing `flatten().collect()` pipeline — zero cost when
  unused. `freq_hz`/`dt_s`/`start_sample` are computed with the decoder's
  own `candidate_offset_samples` (time padding handled), so dump
  coordinates share the `DecodedMessage` convention exactly.
- `cargo test --features transmit -p pancetta-ft8`: 519 passed / 0
  failed, exit code 0. No new clippy warnings (114 pre-existing
  workspace-lint warnings on both sides of the diff).

## Method

Per WAV: run `decode_window_with_candidate_dump` at default `Ft8Config`.
Failed candidates = records with `decoded == false`, deduped on the
(3.125 Hz, 0.08 s) candidate grid, minus positions within (6.25 Hz,
0.16 s) of any successful decode (sidekick candidates of already-decoded
signals). Truth-adjacent = within (6.25 Hz, 0.16 s) of an ft8_lib truth
whose hash-normalized text production did NOT decode. For each
truth-adjacent failed candidate, evaluate the truth codeword's LLR
sign-agreement under three demods AT THE CANDIDATE'S OWN coordinates
(production won't have truth): spectrogram max-log (+2 row convention),
matched filter at the dump's `start_sample`/`freq_hz`, and matched filter
with ±240-sample refinement (Batch 90 scaffolding reused verbatim).
Hash-token truths that fail encoder round-trip are skipped and counted.

## Results (40 WAVs: 20 hard-200 + 20 kill-switch)

Population:

| quantity | value |
|---|---|
| dump records (all passes) | 8000 (200/WAV: 120 pass-1 + 40 + 40) |
| failed candidates (deduped, non-success-adjacent) | 2360 |
| truth-adjacent failed candidates | **35 (1.5% of failed)** |
| skipped: truth not re-encodable (hash tokens) | 8 |
| skipped: coords out of example spectrogram | 0 |
| **evaluated** | **27** |
| unique missed truths covered | 13 |
| truth-adjacent sync scores | mean 11.6, p50 11.1, p90 14.8 |

Per-WAV truth-adjacent counts are 0–6 (hard-200) and 0–3 (kill-switch);
26 of 40 WAVs contribute zero. Full per-WAV table in the example output.

Demod comparison at truth-adjacent failed candidates (same positions,
same LLR function, only the front-end differs):

| Population | Demod | mean | p10 | p50 | p90 | mean\|LLR\| |
|---|---|---|---|---|---|---|
| CONTROLS (n=954, production decodes, sanity gate) | max-log | 92.0% | 78.2% | 95.4% | 100.0% | 9.75 |
| CONTROLS | mf +refine | 91.9% | 71.8% | **96.6%** | 100.0% | 11.91 |
| TRUTH-ADJ FAILED (n=27, combined) | max-log | 67.9% | 49.4% | **67.8%** | 85.1% | 6.89 |
| TRUTH-ADJ FAILED | matched filter | 70.3% | 50.0% | **67.2%** | 88.5% | 7.04 |
| TRUTH-ADJ FAILED | mf +refine | 70.5% | 50.6% | **67.8%** | 90.8% | 7.51 |
| — hard200 subset (n=19) | max-log / mf / mf+ref p50 | | | 62.6 / 66.7 / 67.2% | | |
| — killsw subset (n=8) | max-log / mf / mf+ref p50 | | | 83.3 / 82.8 / 80.5% | | |

Controls sanity gate: 95.4% / 96.6% p50 — reproduces Batch 90 exactly;
the sample mapping is valid on this corpus mix.

## Verdict (pre-registered bars)

- Population: 27 evaluated (35 truth-adjacent) < 30 floor →
  **SHELVE-POPULATION**.
- Distribution (recorded as required): matched-filter p50 = 67.2%,
  +refine p50 = 67.8%, vs max-log p50 = 67.8% — delta ≈ 0.0 pts. Misses
  BOTH bars (>= 85% OSD-2-viable; >= 75% AND +8 pts BP-viable) by a wide
  margin, so the verdict is SHELVE on the merits as well, not just on
  population size.

## Interpretation

The Batch-90 control-population matched-filter edge does NOT transfer to
the failed-candidate population. At sync-passing-but-LDPC-failing
positions adjacent to real missed signals, both demods plateau at ~68%
sign-agreement (vs ~50% at sub-Costas positions and ~95% at decoded
positions) — there IS partial signal there, but the corruption is in the
signal itself (collisions/interference across data symbols), not in the
front-end's incoherence, so a better linear demod recovers nothing. And
the opportunity was tiny regardless: across 40 of the hardest available
slots, only 35 failed candidates point at real missed truths (13 unique
messages), while 97.7% of failed candidates point at nothing real —
pushing harder decoding (e.g., forced OSD) through this population is
dominated by FP risk.

Bank action: hb-250 → SHELVED (both SHELVE-POPULATION and
below-both-bars on distribution; controls validated).

## Deviations from the brief

- Option (c) was used for candidate exposure (no suitable existing dump
  path; documented above). The addition is additive and `#[doc(hidden)]`.
- Demod evaluation runs on the ORIGINAL audio while pass >= 2 dump
  records refer to the subtraction residual; with success-adjacent
  candidates excluded, residual vs original differences at the evaluated
  positions are second-order for a premise probe (and the pass-1
  candidate list dominates the dump).
- The 6.25 Hz / 0.16 s gate was also used (per brief) to exclude failed
  candidates adjacent to SUCCESSFUL decodes before truth matching; this
  is required to avoid counting sidekick candidates of decoded signals
  as opportunities.
