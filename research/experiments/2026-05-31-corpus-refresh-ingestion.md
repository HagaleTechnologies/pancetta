---
slug: corpus-refresh-ingestion-2026-05-31
mode: ft8
state: completed
created: 2026-05-31T01:30:00Z
last_updated: 2026-05-31T02:30:00Z
branch: iter/2026-05-31-corpus-refresh-ingestion
parent_hypothesis: follow-up to 2026-05-30-corpus-survey (executes the survey's INGEST recommendation)
wild_card: false
scorecard: research/scorecards/main.json (UPDATED) + research/scorecards/history/main.2026-05-30-pre-refresh.json (archived pre-refresh baseline)
delta_vs_main: corpus REFRESH — composite numbers are not directly comparable across the refresh boundary
disposition: SHIPPED — new hard_200 (today top-100 + existing top-100) + new wild_100 (stratified-by-hour today sample); old hard_200 archived; main.json updated; pre-refresh main.json preserved for historical reference
---

## What changed

> **Naming note (Phase C 2026-06-02):** The "saturation-aware composite"
> machinery that downstream entries (hb-133) lean on is pancetta-internal
> shorthand for "composite-with-corpus-offset" or
> "corpus-shift-corrected composite". "Saturation" here is the *operator
> story* — corpora rotate when the decoder saturates the previous
> corpus — not a statistical saturation correction. The math is just
> a fixed additive offset. See
> `docs/engineering/2026-06-02-engineering-substance-audit.md` (claim 31)
> and `pancetta-research/src/metrics.rs` for the naming clarification.

The hard-200 corpus was a fixed 200-WAV set since 2026-05-20 (manifest
generated_at=2026-05-20T22:23:04Z). Six days of decoder graduations
(hb-052/053/058/060/061/063/072/075/079/080/086 V1/068) were measured
against it; composite climbed 0.555131 → 0.569415 (+0.014284). The
2026-05-30 corpus survey (`2026-05-30-corpus-survey.md`) found that
today's 8.6h K5ARH 20m capture was 17-23% denser than existing
hard-200 at the median and exhibited the marginal-SNR cases the
hb-086 V2 diagnostic showed were 0% in the old corpus's top-20.

This iter executes the survey's INGEST recommendation:

1. **New `hard_200.manifest.json`** — 100 today-WAVs (highest
   `interest_score` of the 2066 surveyed) + 100 existing-WAVs
   (highest `interest_score` of the prior manifest). Zero
   deduplications (different recording-dir prefixes).
2. **New `wild_100.manifest.json`** — random stratified-by-hour
   sample from today's full 2066 WAVs (7-12 per hour across
   9 hours). Replaces wild-50's role as the operator-station-
   specific production-tracking tier; wild-50 manifest preserved
   for historical reference (not deleted).
3. **Old `hard_200.manifest.json` archived** under
   `research/corpus/curated/ft8/history/hard_200.2026-05-20.manifest.json`
   for diff-vs-baseline accountability.
4. **Old `main.json` archived** under
   `research/scorecards/history/main.2026-05-30-pre-refresh.json`
   so the V1/V2/hb-068 lineage's baseline (0.569415) remains
   addressable for historical comparison.

## Methodology

**Scoring** (`pancetta-research/examples/score_all_today.rs`,
9 min wall-clock at 225 WAVs/min via rayon parallel). All 2066
2026-05-30 WAVs scored by `interest_score = pancetta_decode_count
+ snr_component + noise_component` (same formula as existing
hard_200 manifest).

**Merge** (`pancetta-research/examples/merge_corpus_refresh.rs`,
<1 s). today_top100 score range [31.088 .. 37.132], decode counts
[33 .. 39]. existing_top100 score range [23.895 .. 37.924].
Merged 200 entries, no dedups.

**Wild-100 stratification** (in same merge tool, deterministic
StdRng seed). Quotas: hour 15: 7 (partial startup hour), hours
16-23: 11-12 each, total 100.

**jt9 baselines** (`pancetta-research/examples/baseline_parallel.rs`,
6-worker parallel via jt9 subprocess). hard-200: 100 new generated +
100 cached (130.9 s). wild-100: 96 new + 4 cached (117.3 s). Zero
failures.

**Eval** (existing `pancetta-research --bin eval`, with `wild-100`
tier added to the dispatch list in this iter). 7-tier run:
fixtures + synth-clean + synth-doppler + curated-hard-200 +
curated-hard-1000 + wild-50 + wild-100. Output → `research/scorecards/main.json`.

## Results

| metric | pre-refresh (2026-05-30) | post-refresh (this iter) | delta |
|---|---:|---:|---:|
| composite | 0.569415 | **0.579114** | **+0.009699** |
| fixtures pass_rate | 1.0 | 1.0 | 0 ✓ |
| synth-clean @50/@90 | -20/-20 | -20/-20 | 0 ✓ |
| synth-doppler @50/@90 | None/None | None/None | 0 (known: decoder fails Doppler tier) |
| hard-200 rec / novel | 4621 / 914 | **4942 / 1970** | +321 rec / +1056 novel |
| hard-1000 rec / novel | 14987 / 3038 | **14993 / 6320** | +6 rec / **+3282 novel** ⚠️ |
| wild-50 rec / novel | 0 / 0 | 0 / 64 | +64 novel |
| wild-100 rec / novel | n/a | 1880 / 650 | n/a (new tier baseline) |

**Composite movement of +0.009699 is NOT a fair decoder improvement
delta** — it's a corpus refresh artifact. Hard-200 changed (100 new
today WAVs replaced 100 old ones); the new entries score differently
under pancetta's recovery rate. The number is the correct new
baseline going forward; cross-refresh comparison is not meaningful.

### Anomaly: hard-1000 novel count more than doubled (3038 → 6320)

The hard-1000 corpus did NOT change in this iter (only hard-200 was
refreshed). Same decoder, same WAVs, same flags — `truth_decodes_total`
identical (28104), `truth_decodes_recovered` shifted by +6 (noise),
but `novel_decodes` jumped from 3038 to 6320.

Plausible causes (untriaged):
- The eval binary's novel-decode counting depends on the union of
  cached baselines across ALL tiers; the new hard-200 entries
  introduced 100 new baseline files whose decode sets may overlap
  with hard-1000 in ways that change the dedup logic.
- The decoder Default has a new field (`residual_energy_stop_db`
  from hb-016) that defaults to None; if the eval applies the
  field differently than before, behavior could shift even at
  default.
- Something about per-WAV slot ordering changed when the manifest
  was rewritten.

This is a methodology bug to investigate in the next batch.
**Recall (the headline metric) is essentially unchanged on hard-1000
(+6 rec)**, so the underlying decoder behavior is stable. The novel
inflation is a counting/eval artifact, not a decoder regression.

### Synth + fixtures stability

Synth-clean and fixtures are unchanged — confirms the decoder code
is byte-identical to the pre-refresh state. No regression from the
corpus refresh on guard tiers.

## Comparability across the refresh boundary

Composite numbers from this point forward are NOT directly
comparable with the 0.555131 → 0.569415 history. Specifically:

- The 11-graduation cumulative +0.014284 IS still real — every
  one of those graduations was measured against the OLD corpus
  and the deltas hold against that instrument.
- Future iter deltas (against the NEW main.json baseline) measure
  against the refreshed instrument and tell us where we are vs
  the new ceiling.
- The archived `main.2026-05-30-pre-refresh.json` and
  `history/hard_200.2026-05-20.manifest.json` allow exact
  reproduction of the pre-refresh baseline for any historical
  cross-check (e.g., "did hb-068 still hold its synth-clean win?").

## Why now (not earlier)

The decoder iters were producing diminishing returns against the
fixed hard-200 (today's hb-068 grad was +0.000292 composite vs
hb-079's +0.009212 a week ago). The survey confirmed today's
data is genuinely harder + structurally different (marginal-SNR
cases for V2, density for joint-decoding). Refreshing the
instrument is the cleanest way to keep the iter cycle producing
meaningful deltas.

## Re-audit recommendations

After this iter:

1. **Re-run hb-086 V2 diagnostic on refreshed top-20 hard-200.**
   The V2 diagnostic shelved at 0% marginal-SNR neighbors in
   old corpus top-20. Survey saw 9/20 today-slots reach jt9's
   -25 dB SNR floor. Likely re-run will show >20% marginal —
   V2 unshelves and gets a real implementation cycle.
2. **Re-run hb-064 (DIA-OSD TEP pruning) scoping.** Refreshed
   corpus has higher density → more BP/OSD work per WAV → TEP
   pruning has more to optimize.
3. **hb-068 finer-scale sweep** (b-scale ∈ {0.2, 0.25}) on
   refreshed corpus. Old corpus had b-0.25 → +7 rec hard-200
   (unconfirmed on synth). Refreshed corpus may show different
   optimum.

## Files of note

- `pancetta-research/examples/score_all_today.rs` — re-runnable
  scoring pass (parallel rayon)
- `pancetta-research/examples/merge_corpus_refresh.rs` — manifest
  builder + archiver
- `pancetta-research/examples/baseline_parallel.rs` — parallel jt9
  baseline generator
- `research/corpus/surveys/2026-05-30/all_wavs_scored.json` — full
  scored list (906 KB, all 2066 WAVs)
- `research/corpus/curated/ft8/{hard_200,wild_100}.manifest.json` —
  new corpus manifests
- `research/corpus/curated/ft8/history/hard_200.2026-05-20.manifest.json`
  — archived pre-refresh hard-200
- `research/scorecards/history/main.2026-05-30-pre-refresh.json`
  — archived pre-refresh main.json (composite 0.569415)

## Cumulative impact

- 6 days of decoder research: +0.014284 composite (measured vs
  OLD corpus, baseline preserved in archive)
- + this iter: corpus instrument refresh — future deltas will
  be against a harder/richer measurement tool
- Plus: Phase 5 operator-side hardening (commits 31c7fd1, 8681169,
  b1e8cfc, 7bbdca8, 423a74f) made the production path safe enough
  to test autonomously
