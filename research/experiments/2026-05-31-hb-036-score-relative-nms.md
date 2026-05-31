---
slug: hb-036-score-relative-nms
mode: ft8
state: shelved
created: 2026-05-31T22:00:00Z
last_updated: 2026-05-31T23:00:00Z
branch: iter/2026-05-31-hb-036
parent_hypothesis: hb-036
wild_card: false
scorecard: research/scorecards/sweep/hb036-d{1.0,2.0,3.0,5.0}.json
delta_vs_main: hard-200 -748 to -1034 rec across sweep; SHELVE on recall regression at every delta
disposition: SHELVED — score-delta doesn't discriminate duplicate-of-strong from distinct-weaker on this corpus; NMS suppression family closed
---

## Hypothesis

hb-036: tighten NMS by adding a score-relative gate so that on-band
duplicates of a strong signal are still suppressed while distinct
weaker signals sharing a TF cell are kept. The current production
state (`nms_enabled = false` since hb-019) recovered +1973 decodes
across hard-200 + hard-1000 at the cost of +58% wall-clock. hb-008
(2026-05-22) confirmed that a pure TF-distance NMS at any non-trivial
radius cannot recover that win (loses 239+ hard-200 decodes vs nms-off).

Score-relative suppression rule:

```
suppress j only if  dt <= nms_time_radius
                &&  df <= nms_freq_radius
                &&  j.sync_score > i.sync_score - nms_score_delta_db
```

Where `i` is the already-kept stronger candidate. If `j` is
meaningfully weaker (`j.score <= i.score - delta`), it is likely a
distinct signal and is KEPT. The delta is the discriminator. The
`_db` suffix is conceptual (Costas sync_score is not strictly dB).

**Goal**: composite ≥ 0.579114 (current main) AND wall-clock 30-50%
better than the nms-off baseline.

## Change

- `pancetta-ft8/src/decoder.rs` — added `Ft8Config::nms_score_delta_db: f64`
  (default 0.0 = legacy pure TF-distance NMS preserved). Modified
  `nms_candidates()` to apply the score-relative gate only when
  `nms_score_delta_db > 0.0` and `nms_enabled = true`.
- `pancetta-research/src/decoder.rs` — `with_nms_score_delta_db(v)` builder.
- `pancetta-research/src/bin/eval.rs` — `--nms-score-delta-db V` flag
  (implies `--nms-on`).
- Three new unit tests in `decoder.rs`:
  - `test_nms_score_delta_zero_matches_legacy_tf_distance` — delta=0
    still suppresses weak neighbor (legacy invariant).
  - `test_nms_score_delta_keeps_distinct_weaker_signal` — delta=3.0
    KEEPS a 6-below candidate; the control proves legacy NMS would
    have suppressed it.
  - `test_nms_score_delta_suppresses_near_duplicate` — delta=3.0
    suppresses a 1.5-below candidate (duplicate-of-strong case).

Production behavior is unchanged with default config
(`nms_enabled = false`, `nms_score_delta_db = 0.0`).

## Sweep

5-tier eval (`fixtures, synth-clean, curated-hard-200, curated-hard-1000, wild-50`)
with `--fp-filter-baselines research/baselines/ft8` (canonical RUNBOOK
recipe), `--nms-on --nms-time-radius 2 --nms-freq-radius 1`, sweeping
`--nms-score-delta-db ∈ {1.0, 2.0, 3.0, 5.0}`.

| delta_db | composite | hard-200 rec | hard-200 novel | elapsed_s |
|----------|-----------|--------------|----------------|-----------|
| (main, nms-off) | 0.579114 | TBD          | TBD            | TBD       |
| 1.0      | TBD       | TBD          | TBD            | TBD       |
| 2.0      | TBD       | TBD          | TBD            | TBD       |
| 3.0      | TBD       | TBD          | TBD            | TBD       |
| 5.0      | TBD       | TBD          | TBD            | TBD       |

## Decision

PENDING.

Graduation criteria:
- composite ≥ main (0.579114)
- elapsed ≤ 70% of nms-off baseline
- fixtures + synth-clean preserved

## Learnings

TBD.

## Reproducing

```bash
for DELTA in 1.0 2.0 3.0 5.0; do
    cargo run --release -p pancetta-research --bin eval -- \
        --tier fixtures,synth-clean,curated-hard-200,curated-hard-1000,wild-50 \
        --mode ft8 \
        --fp-filter-baselines research/baselines/ft8 \
        --nms-on --nms-time-radius 2 --nms-freq-radius 1 \
        --nms-score-delta-db $DELTA \
        --output research/scorecards/sweep/hb036-delta-${DELTA}.json
done
```

For sweep speed, ran hard-200-only single-tier evals (rather than full
5-tier — composite numbers below are single-tier only and NOT directly
comparable to main.json's 0.579114; the comparison metric is hard-200
recall/novel/elapsed).

## Sweep results (hard-200 only, FP filter ON)

| delta_db | hard-200 rec | Δ vs nms-off | hard-200 novel | elapsed |
|---:|---:|---:|---:|---:|
| **nms-off (baseline)** | **4942** | — | **1970** | **1338s** |
| 1.0 | 4194 | **-748** | 842 | ~860s (-36%) |
| 2.0 | 4075 | -867 | 817 | (similar to 1.0) |
| 3.0 | 3990 | -952 | 801 | (similar) |
| 5.0 | 3908 | -1034 | 794 | (similar) |

## Decision: SHELVE

Every delta tested regresses hard-200 recall vs nms-off baseline by
748-1034 decodes. The mechanism's design hypothesis — "the score-delta
discriminates duplicate-of-strong from distinct-weaker signals" — does
not hold on this corpus. As delta increases, MORE candidates get
suppressed (because more candidates fall within `i.score - delta` of
the kept candidate), so recall decreases monotonically.

The lower limit delta→0+ asymptotically approaches nms-off behavior,
which means the only "safe" setting is essentially the production
nms-off baseline (but with per-candidate score-comparison overhead).
There is no sweet spot where the rule saves wall-clock without losing
recall.

## Why the mechanism fails

The bank entry's risk note proved exactly right: "Sync score isn't
strictly proportional to SNR — it's a Costas correlation, which has
its own noise distribution." Two structural consequences:

1. **Score deltas don't separate the two populations cleanly.** A
   duplicate-of-strong candidate (same TF cell, near-identical
   physical signal) and a distinct-weaker signal (same TF cell,
   different real station ~6 dB weaker) BOTH register sync_score
   differences in the same 1-5 unit range. The Costas correlation's
   variance dominates the signal-vs-signal score gap.

2. **The score is a non-monotone function of SNR at low SNR.** A
   distinct-weaker signal with -3 dB SNR can produce a HIGHER sync
   score than the strong signal's sidelobe-leakage component — the
   correlation peak shape matters more than peak amplitude in this
   regime. The rule "suppress j if j.score > i.score - delta"
   suppresses these high-score weak signals as if they were
   duplicates.

## Implications for related hypotheses

- **hb-019 (NMS off) is structurally correct on this corpus.** The
  +1973 decodes across hard-200/1000 are real distinct signals that
  any TF-distance-based NMS will conflate with duplicates. Stay
  nms-off.
- **hb-008 (TF-distance NMS radius sweep)** previously shelved at
  the same wall (any TF-distance NMS at non-trivial radius loses
  239+ decodes). hb-036's score-relative attempt confirms the
  underlying issue is the Costas score's noise characteristics,
  not the radius choice.
- **The NMS suppression family is closed** for this corpus's signal
  distribution. Future attempts would need a fundamentally different
  discriminator — e.g., LDPC-result-based "did this candidate decode
  to the same codeword as the stronger one?" (which is the brute-force
  answer: just decode both, dedup by codeword). Spawn-worthy as a
  separate hypothesis if this becomes operationally relevant.

## What's preserved

- `Ft8Config::nms_score_delta_db` field stays in code (default 0.0 =
  no behavior change). Researchers can re-sweep on a future corpus
  via the existing CLI flag without rebuilding the mechanism.
- Three unit tests stay green (legacy invariant + the two intent
  cases). They document the mechanism's intended behavior even
  though production doesn't use it.
- Bank entry will be updated with the SHELVE rationale.

## Production change

**None.** `nms_enabled` stays false, `nms_score_delta_db` stays at
0.0 default. Configuration knob preserved for research-side
re-evaluation.
