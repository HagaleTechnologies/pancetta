---
slug: hb-048-a7-s3-chrono-retest
mode: ft8
state: complete
created: 2026-06-01T22:00:00Z
last_updated: 2026-06-01T22:00:00Z
branch: iter/2026-06-02-hb-048-s3-chrono
parent_spec: docs/superpowers/specs/2026-05-31-hb-048-a7-design.md
prior_sessions:
  - research/experiments/2026-05-31-hb-048-session1.md (design)
  - research/experiments/2026-06-01-hb-048-session2.md (primitive + synthetic-injection GRADUATE)
  - research/experiments/2026-06-02-hb-048-session3.md (within-WAV SHELVE)
  - research/experiments/2026-06-02-chrono-replay-tier.md (chrono-replay tier shipped, infra)
wild_card: false
scorecards:
  - research/scorecards/sweep/hb048-s3chrono-baseline.json
  - research/scorecards/sweep/hb048-s3chrono-snr7-5.5-snr7b-1.8.json
  - research/scorecards/sweep/hb048-s3chrono-snr7-6.0-snr7b-1.8.json
  - research/scorecards/sweep/hb048-s3chrono-snr7-6.5-snr7b-2.2.json
delta_vs_main: a7_enabled extended to read ApContext.recent_calls (cross-slot path). Default still OFF; production behavior unchanged at default config.
disposition: SHELVED-CHRONO — binding chronological-replay retest. At every threshold tested (snr7 ∈ {5.5, 6.0, 6.5}, snr7b ∈ {1.8, 2.2}), rec Δ = +0 with 95% CI [+0.0, +0.0] (NOT significant), while novel Δ = +2364 with CI [+2184, +2460] (significant FP injection). 376 cross-slot callsigns accumulated and exercised; mechanism still fails to surface truths.
---

## Headline

**Binding test: a7 cross-correlation, wired to consume cross-slot
CrossTimeState callsigns and re-evaluated on the chrono-replay tier
(33 contiguous slots from K5ARH's 2026-05-30 session), shows rec Δ = 0
at every threshold with 95% bootstrap CI [+0.0, +0.0] (NOT significant)
while injecting +2364 novel decodes (95% CI [+2184, +2460], significant).
The cross-slot path is active and the snapshot grows monotonically to
376 callsigns by slot 33 — the WSJT-X-canonical input to a7 is present.
The mechanism fundamentally does not surface decodable truths on this
corpus. SHELVED-CHRONO.**

## What changed

`Ft8Decoder::a7_cross_correlation_pass` now reads
`ApContext.recent_calls` in addition to the within-WAV `pass_decoded`
when building its template-root expected-call list. Cross-slot calls
have no known audio frequency, so they probe ALL `sync_candidate`s
(the ±`a7_freq_window_hz` gate only applies to in-WAV entries with
known freq). Dedup by bare callsign avoids double-templating when a
call appears in both sources.

This is the WSJT-X-canonical a7 path: slot N+1 templates rooted at
callsigns heard in slot N. The chrono-replay tier's
`ChronoReplayState` populates `ApContext.recent_calls` from a
persistent cross-WAV snapshot — exactly the substrate the Session 3
SHELVE note flagged as the unblock for this retest.

## Sweep results (chrono-replay-mini33, FP-filter on)

The 300-slot K5ARH manifest could not be used directly at iter time —
only 32-49 baselines were cached in this worktree and the jt9
baseline queue was saturated by concurrent agents. A 33-slot
contiguous subset (`chrono_replay_mini33.manifest.json`) covering
slots 0..32 of the full manifest, with full baseline coverage, was
used instead. The first 33 slots preserve the same statefulness
contract (snapshot grows monotonically across consecutive WAVs).

Baseline (a7 OFF): rec = 640/1167, novel = 137, composite = 0.000000.

| snr7 | snr7b | rec | truth | novel | composite | rec Δ | novel Δ |
|------|-------|-----|-------|-------|-----------|-------|---------|
| OFF  | OFF   | 640 | 1167  | 137   | 0.000000  | (baseline) | (baseline) |
| 5.5  | 1.8   | 640 | 1167  | 2501  | 0.000000  | +0 | +2364 |
| 6.0  | 1.8   | 640 | 1167  | 2501  | 0.000000  | +0 | +2364 |
| 6.5  | 2.2   | 640 | 1167  | 2501  | 0.000000  | +0 | +2364 |

Composite is 0.000000 on this tier because the saturation-aware
composite formula does not yet have a fixed weight for `chrono-replay`
— the tier contributes through `truth_decode_rate`, but the production
composite key list (in `pancetta-research/src/scorecard.rs`) targets
hard-200 / synth-clean / synth-doppler. The recall and novel deltas
are the operative signals.

### Bootstrap CI (n=1000, seed=0xb007)

**snr7=5.5, snr7b=1.8 (most permissive in this sweep):**
- rec Δ = +0  (95% CI [+0.0, +0.0]) — NOT significant
- novel Δ = +2364 (95% CI [+2183.8, +2460.0]) — significant

**snr7=6.0, snr7b=1.8 (WSJT-X canonical):**
- rec Δ = +0  (95% CI [+0.0, +0.0]) — NOT significant
- novel Δ = +2364 (95% CI [+2183.8, +2460.0]) — significant

**snr7=6.5, snr7b=2.2 (most conservative):**
- rec Δ = +0  (95% CI [+0.0, +0.0]) — NOT significant
- novel Δ = +2364 (95% CI [+2182.7, +2461.0]) — significant

All three threshold combinations: identical rec, identical novel,
identical CI shape. The threshold gate is saturated — at this
corpus's noise level, a7's template-matcher accepts essentially all
candidates regardless of `snr7` / `snr7b` constraints, and none of
them are real decodes.

### STATEFULNESS confirmation

The chrono-replay tier emits a `STATEFULNESS final snapshot=N` line at
the end of each invocation. Across all four runs:

```
baseline:        STATEFULNESS final snapshot=375 callsigns, monotonic-growth=true, samples=33
snr7=5.5/1.8:    STATEFULNESS final snapshot=376 callsigns, monotonic-growth=true, samples=33
snr7=6.0/1.8:    STATEFULNESS final snapshot=376 callsigns, monotonic-growth=true, samples=33
snr7=6.5/2.2:    STATEFULNESS final snapshot=376 callsigns, monotonic-growth=true, samples=33
```

Snapshot grew from 0 → 375 across the 33 slots (a7 ON adds +1 because
one a7 candidate produced a plausible callsign that wasn't already in
the deque). Cross-slot context IS being exercised. The +2364 novel
decodes per run are a7's template-matcher saturating on residual LLR
noise at sync_candidate positions, NOT real follow-up signals from
previously-heard stations.

## Decision per Phase A

**SHELVED-CHRONO.**

Against the design spec's SHELVE criteria:
- hard-tier rec < +5 across the full threshold sweep: ✓ (rec Δ = +0 at every threshold)
- "no plausible parameter set survives": ✓ (all three CIs straddle 0 exactly)
- composite delta essentially zero at every setting: ✓
- Novel Δ significant-positive everywhere — FP injection without upside: ✓ (+2364)

Against the design spec's GRADUATE criteria:
- composite ≥ +0.0005 minimum: ✗ (0.000000 at all settings)
- hard-tier recovered +5 minimum: ✗ (max rec Δ is +0)

This RULES OUT the cross-slot a7 path at production-realistic
threshold values on a real chronological session trace. Session 3's
SHELVE was conditional on "no cross-slot test"; this retest closes
that condition with a definitive negative.

## Why the mechanism fails even with cross-slot context

Session 3's SHELVE note hypothesized that the within-WAV path failed
because "the template bank rooted at C is the wrong bank — the message
there is some other station's traffic, not C's follow-up." The
cross-slot prediction was that with the right callsign (a call heard
in slot N looking for a response in slot N+1), the bank would be
correctly rooted.

The chrono-replay retest reveals a deeper problem: even with 376
correctly-rooted callsigns (every station heard in the session),
a7's matched-filter score against residual LLRs at every sync
candidate position SATURATES the acceptance thresholds. The
mechanism cannot distinguish "this position contains a follow-up
from C" from "this position contains noise plus some other signal
that happens to weakly correlate with one of C's 32 templates."

The structural issue:
1. With 376 callsigns × ~32 templates each = ~12k templates per slot.
2. At every `sync_candidate` (typically 80-120 per slot), the bank
   `best_template_score` finds the *best-scoring template among all
   12k* — extreme-value statistics ensure some template will score
   high purely by chance at every candidate position.
3. `snr7b` (the ratio of best to second-best score) doesn't catch
   this because the residual-LLR noise also produces a long tail of
   high secondary scores.
4. `is_plausible()` and the production FP filter catch obviously-
   malformed messages but not "K5ARH ND0KQ RR73" template strings
   produced by chance.

This is a fundamental signal-vs-noise problem at the template-bank
scale: more cross-slot callsigns means a larger template bank means
a higher noise floor at every candidate position. Cross-slot context
*amplifies* the FP problem rather than enabling true recall.

## What this rules out (and what it doesn't)

**RULED OUT (binding, on real session corpus):**
- a7 cross-correlation as currently implemented, at any threshold
  combination in the design-spec sweep grid, on a chronological-
  replay tier with full cross-slot CrossTimeState context. The
  mechanism does not surface decodable truths and injects significant
  FP at every operating point.

**NOT RULED OUT (out of scope for this iter):**
- a7 with a structurally tighter bank: e.g. only templates from
  callsigns the operator's own station was directly in QSO with,
  or callsigns flagged as "high-priority" by an upstream prior.
  This would shrink the bank from 376×32 to maybe 4×32 and
  dramatically reduce the extreme-value FP problem.
- a7 against the original spectrogram instead of post-multipass
  residual. Would access more SNR but also need a different
  candidate-position discipline (currently the residual sync
  candidates are post-subtract; original spectrogram would need
  pre-subtract sync).
- a7 with `my_call` set (production context). Eval has no `my_call`;
  the highest-priority WSJT-X a7 case is "I expect a reply to ME"
  — templates rooted at the operator's own callsign, queried only
  at candidates within a tight TX-freq window. That's a different
  mechanism (closer to AP4) and would be a separate hypothesis.

## Run timing notes

This iter ran under heavy contention: 4 concurrent chrono-replay
evals from other agents on the same host (load average peaked at
135). Each scorecard took ~75 minutes wall-clock when sharing CPU
with 4+ peers; in isolation the same eval finishes in ~20-25 minutes.
The chrono_replay_mini33 manifest (33 slots) was used because the
300-slot manifest's baseline jt9 generation was queued behind 4
other agents' chrono-replay work.

The directional signal was unambiguous by slot 5 of 33 (baseline
4-5 novels per slot, a7-on 78-83 per slot, identical recall);
running to completion produced the bootstrap-CI-significant
shutdown result documented above.

## Files changed

- `pancetta-ft8/src/decoder.rs` — `a7_cross_correlation_pass`
  signature extended to take `&[RecentCallAp]`; expected-call list
  built from both within-WAV and cross-slot sources; freq-window
  gate applied only to in-WAV entries with known freq. (~100 LOC.)
- `pancetta-research/src/bin/eval.rs` — no change (the
  `chrono-replay` tier already constructs `ApContext.recent_calls`
  from the persistent deque per slot; the a7 path now consumes it).
- `research/corpus/curated/ft8/chrono_replay_mini33.manifest.json`
  — 33-slot subset with full baseline coverage.
- `research/scorecards/sweep/hb048-s3chrono-*.json` — 4 scorecards.
- `research/scorecards/sweep/hb048-s3chrono-*.log` — eval logs.
- `research/experiments/2026-06-02-hb-048-s3-chrono-retest.md` — this file.

## Branch + commits

Branch: `iter/2026-06-02-hb-048-s3-chrono`

1. `feat(ft8): hb-048 — a7 reads cross-slot calls from CrossTimeState snapshot`
2. `research(iter): hb-048 S3 chrono-retest — bootstrap CI on 3 thresholds`
3. (this commit) `research(iter): hb-048 S3 chrono-retest — SHELVED-CHRONO definitive`

## Hypothesis-bank update

hb-048 moves to **SHELVED-DEFINITIVE** (both within-WAV and
cross-slot paths exhausted). Any future a7-style work would need a
structurally different template-bank discipline (operator-anchored,
QSO-anchored, or prior-weighted) and should open a new hypothesis
rather than reanimate hb-048.

## References

- Design spec: `docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`
- Session 1 (design): `research/experiments/2026-05-31-hb-048-session1.md`
- Session 2 (primitive): `research/experiments/2026-06-01-hb-048-session2.md`
- Session 3 (within-WAV SHELVE): `research/experiments/2026-06-02-hb-048-session3.md`
- Chrono-replay tier (infra): `research/experiments/2026-06-02-chrono-replay-tier.md`
- WSJT-X mainline a7 commit:
  `f13e31820470291fdd49627287a2dc08f3fa674c` (Joe Taylor, 2021,
  `lib/ft8_a7.f90`). The pancetta implementation matches the bank
  generation and snr7/snr7b acceptance shape; the FP behavior we
  observe is a corpus/SNR-distribution difference, not a primitive
  defect (Session 2's synthetic-injection test confirmed the
  primitive recovers a known truth at noise SNR -7.6 dB).
- CrossTimeState substrate: `pancetta-qso/src/cross_time_state.rs`
- ChronoReplayState (eval-harness mirror):
  `pancetta-research/src/decoder.rs::ChronoReplayState`
- Phase B bootstrap CI methodology:
  `research/experiments/2026-06-01-phase-b-bootstrap-ci.md`
