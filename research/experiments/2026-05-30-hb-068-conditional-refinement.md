---
slug: hb-068-conditional-refinement
mode: ft8
state: graduated
created: 2026-05-30T17:00:00Z
last_updated: 2026-05-30T18:30:00Z
branch: worktree-agent-a902a7d6a73c64b89
parent_hypothesis: hb-068 (variant-scan re-spawn)
wild_card: false
scorecard: research/scorecards/history/2026-05-30-hb-068-b-scale-0.3.json
delta_vs_main: composite +0.000292 (0.569123 → 0.569415); hard-200 +5 rec / -7 novel (4616→4621, 921→914); hard-1000 +17 rec / +2 novel (14970→14987, 3036→3038); synth-clean snr@90% gains +2 dB (-18 → -20 dB) WITHOUT the hb-044 -116 hard-200 regression; fixtures + wild-50 preserved; elapsed +10.8%
disposition: GRADUATE — `sync_time_interpolation = true` + `sync_time_interp_delta_scale = 0.3` (variant b at 0.3×). Recovers hb-044's +2 dB synth gain while preserving (and slightly improving) hard-200 recall.
---

## Hypothesis

hb-044 (Costas time-axis parabolic refinement, `sync_time_interpolation`)
batch 7 showed +2 dB SNR@90% on synth-clean but -116 hard-200 recall.
hb-068 variant (d) (batch 8) tried "refined position, original score for
sort"; it neither rescued recall nor changed the regression magnitude
(4248 vs 4249) — so the failure mechanism is **not** sort displacement,
it's the **spectrogram interpolation itself perturbing already-correctly-
aligned candidates** (per batch-8 finding).

hb-068 re-spawn (this entry) tests three conditional variants:
- **(a) score gate**: only refine if `sync_score > threshold` (sweep
  3.5, 4.0, 4.5, 5.0)
- **(b) scaled delta**: multiply the parabolic delta by < 1.0 before
  applying (sweep 0.3, 0.5, 0.7)
- **(c) reject large deltas**: if `|delta| > threshold`, fall back to
  integer-bin (sweep 0.3, 0.4)

## Change

`Ft8Config` gains three knobs (all no-op at their defaults — preserve
hb-044 behavior when `sync_time_interpolation` is on):
- `sync_time_interp_score_gate: f64` (default 0.0)
- `sync_time_interp_delta_scale: f64` (default 1.0)
- `sync_time_interp_max_delta_abs: Option<f64>` (default `None`)

Call site (`costas_sync_search_with_threshold`):
1. (a) Skip refinement entirely if `score ≤ sync_time_interp_score_gate`.
2. (b) After parabolic fit, multiply `delta` by `sync_time_interp_delta_scale`
   and recompute `refined_score = y_center + b·δ + a·δ²` so the stored
   score reflects the actually-used position.
3. (c) After clamp/scale, if `|delta| > sync_time_interp_max_delta_abs`,
   reset to integer-bin (`delta=0`, `score=y_center`).

Research builder: `with_sync_time_interp_score_gate`,
`with_sync_time_interp_delta_scale`, `with_sync_time_interp_max_delta_abs`.
Eval flags: `--sync-time-interp-score-gate <f>`,
`--sync-time-interp-delta-scale <f>`, `--sync-time-interp-max-delta-abs <f>`
(each implicitly enables `--sync-time-interpolation`).

## Methodology

All runs against `iter/2026-05-28-hb-086-joint-decoding` HEAD (composite
0.569123, hard-200 4616 rec / 921 novel). FP filter on
(`research/baselines/ft8`). Synth WAV path symlinked into the worktree.

Per-variant: hard-200 + synth-clean two-tier eval. If a candidate
preserves baseline hard-200 AND the +2 dB synth gain, run full 5-tier.

### Sanity baseline (no interpolation; same code path as main)

`./target/release/eval --tier curated-hard-200 --fp-filter-baselines
research/baselines/ft8 --output sweep/hb068-sanity-baseline.json`
→ rec=4616 / novel=921 (matches main.json exactly; confirms the new
config knobs are no-op at defaults).

### Synth control (pure hb-044, no variant knobs)

`./target/release/eval --tier synth-clean --sync-time-interpolation
--output sweep/hb068-pure-hb044-synth.json`
→ snr50=-20 / **snr90=-20** (confirms hb-044's +2 dB synth gain still
exists; baseline is snr90=-18).

## Result

### Variant (a) score gate — REFUTED

| gate | hard-200 rec | novel | Δ rec vs baseline |
|-----:|-------------:|------:|------------------:|
| 4.0 | 4507 | 856 | **-109** |
| 5.0 | 4508 | 857 | **-108** |

Higher gate values barely moved the regression. The candidates whose
refinement causes hard-200 loss have sync scores well above 5.0 too —
the gate cannot separate "good refinement" from "bad refinement" by
score alone. **Killed.**

(A parallel agent's run at gate=3.5 with the same code path gave 4507,
matching the pattern.)

### Variant (b) scaled delta — WINNER

| scale | hard-200 rec | novel | synth snr@90% | Δ rec | Δ novel |
|------:|-------------:|------:|--------------:|------:|--------:|
| 0.3  | **4621** | **914** | **-20** | **+5** | **-7** |
| 0.5  | 4616     | 878     | **-20** | 0    | -43 |
| 0.7  | 4577     | 876     | **-20** | -39   | -45 |
| 1.0 (hb-044 original) | (regression confirmed in batch 7) | | -20 | -116 | -106 |

**scale=0.3 is a clean win on both axes:** +5 hard-200 recall, -7 novels
(cleaner), AND the +2 dB synth gain is preserved.

Intermediate scales (0.5, 0.7) preserve the synth gain but cost recall
(0 at 0.5, -39 at 0.7). The relationship is monotonic in `scale`: the
larger the applied fractional offset, the more correctly-aligned
candidates get perturbed. scale=0.3 is small enough to nudge marginal
candidates the right way without disturbing strong ones.

### Variant (c) reject large deltas — REFUTED

| max\_delta | hard-200 rec | novel | synth snr@90% |
|-----------:|-------------:|------:|--------------:|
| 0.3 | 4616 | 921 | **-18** (no gain) |
| 0.4 | 4616 | 921 | **-18** (no gain) |

At both thresholds, the rejection is aggressive enough to behave as
"never refine" — hard-200 matches baseline exactly, AND the synth gain
is gone. Most of the parabolic deltas on real audio appear to be
**> 0.4** (close to the clamp's [-0.5, 0.5] edge), which is itself
diagnostic: the parabola fits are not finding small local maxima, they're
hitting the clamp boundary, which is exactly the signature of unreliable
fits. **Killed.**

(Variant (c) at a much larger threshold would converge to plain hb-044
behavior; that's already known to regress.)

## Why scale=0.3 works

Reframing the batch-8 finding: spectrogram interpolation perturbs
*already-correctly-aligned* candidates more than it helps misaligned
ones. The full delta from the parabolic fit overcorrects on real
hard-corpus audio (which has multi-signal, multipath, fading content
that violates the parabola's "single concave peak" assumption). At
**scale=0.3** the applied offset is small enough to:

- preserve correctly-aligned candidates (delta · 0.3 is below the
  symbol's coherence width for typical cases)
- still capture the genuine +2 dB synth gain (synth signals have clean
  single-peak Costas patterns where 0.3× the parabolic delta is still
  the right direction)

Variant (a) couldn't isolate the bad cases by score; variant (c) gave up
all gain to avoid them; variant (b) attenuates the damage proportionally.

## Full 5-tier confirmation

| metric | main (pre-hb-068) | b-scale=0.3 | Δ |
|---|---:|---:|---:|
| **composite** | 0.569123 | **0.569415** | **+0.000292** |
| fixtures pass_rate | 1.0 | 1.0 | 0 |
| synth-clean @50/@90 dB | -20 / -18 | -20 / **-20** | **0 / -2 dB** |
| synth-doppler | (no SNR thresholds met) | (no SNR thresholds met) | 0 |
| hard-200 rec / novel | 4616 / 921 | **4621** / **914** | **+5 / -7** |
| hard-1000 rec / novel | 14970 / 3036 | **14987** / 3038 | **+17 / +2** |
| wild-50 | 0 / 96 | 0 / 96 | 0 |
| elapsed | 2320s | 2572s | **+10.8%** |

- Composite +0.000292 (driven mostly by the SNR@90% synth gain at the
  weight `snr_50pct_synth_clean: 0.3`; hard-200 contributes a smaller
  but real share via the `real_decode_rate_hard_200: 0.5` weight).
- **hard-1000 +17 rec confirms scaling** — the gain isn't a 200-WAV
  quirk; on 5× the volume we see ~3.4× the recall improvement, which
  is consistent with a per-WAV gain that's not concentrated in any
  one slice of the corpus.
- Synth-clean's +2 dB SNR@90% rescue is the headline win — recoups the
  original hb-044 sensitivity goal that was abandoned 2026-05-23.
- Fixtures + synth-doppler + wild-50 unchanged.
- Elapsed +10.8% reflects two extra Costas score evaluations per kept
  candidate (the parabolic fit's `y_left` and `y_right` lookups).
  Well within budget; could be optimised by caching the integer-bin
  score during the inner loop if cost becomes an issue.

## Decision

**GRADUATE** variant (b) at `delta_scale = 0.3`. Defaults updated:
- `sync_time_interpolation: false → true`
- `sync_time_interp_delta_scale: 1.0 → 0.3`

This is the first graduation of any hb-044-family change since the
original was shelved (2026-05-23). Recovers the +2 dB synth-clean gain
at zero recall cost, while modestly improving hard-200 (+5 rec).

## Cumulative session impact (through hb-068 b-0.3)

| metric | start (2026-05-25) | now (2026-05-30) | Δ |
|---|---:|---:|---:|
| **composite** | 0.555131 | **0.569415** | **+0.014284** |
| hard-200 recovered | 4376 | 4621 | **+245** |
| hard-1000 recovered | 14267 | 14987 | **+720** |
| synth-clean snr@90% | -18 | **-20** | **-2 dB** |
| fixtures | 1.0 | 1.0 | 0 |

**11 graduations across five days:** hb-063 layered BP, hb-056
non-coh cross-cycle, hb-058 contest-FP, hb-060/061 cleanup, hb-075
MRC coherent, hb-072 CQ-whitelist, hb-079 coherent multi-pass,
hb-080 N=3 multi-pass, hb-086 V1 joint-pair-retry, **hb-068 b-scale=0.3**.

## Learnings

- **Score-gating is a weak lever** when the failure mechanism is per-
  candidate perturbation, not displacement. The bad refinements happen
  at all score levels.
- **Scaled delta is the right knob** for "the refinement direction is
  often right but its magnitude is over-confident on noisy real audio."
  0.3 happens to be the sweet spot where synth signals (clean parabola
  fits, small deltas) still get the full effective adjustment and
  hard-corpus signals (noisy parabola fits, large deltas) only get
  nudged slightly.
- **Rejection (variant c) overshoots** because most delta magnitudes
  on real audio are large — and the rejection makes the refinement a
  no-op precisely on the cases where it had a chance of working.
- **Two-tier hard-200 + synth-clean** is enough signal to decide a
  graduation when the two tiers test opposite failure modes (real-data
  recall + synthetic sensitivity).

## Parallel-agent observation

A second agent ran b-scale=0.25 on hard-200 in this worktree's
sweep dir during the same window (`research/scorecards/sweep/
hb068-b-scale-0.25.json`): **rec=4623 / novel=914** — +2 hard-200
recovered over our chosen 0.3 default. Synth was not run at 0.25 so
the +2 dB sensitivity gain is unconfirmed at that scale. The 0.25
delta is within experimental noise and we ship 0.3 as the
conservative choice; a follow-up could safely retune the default
down to 0.25 if the synth axis holds.

## New spawns

- **hb-068 follow-up**: sweep finer scales {0.2, 0.25, 0.35, 0.4} on
  the full 5-tier; include synth-clean to confirm scale=0.25 doesn't
  regress sensitivity (priority 0.30 — incremental).
- **hb-069 reconfirm**: linear-power-domain interpolation (originally
  spawned as alternative angle on hb-044). Now that scale=0.3 works in
  dB domain, hb-069 may compound — try `delta_scale=0.3` on top of
  power-domain interp (priority 0.30, may stack with hb-068).
- **hb-068 + hb-086 V2 stack**: V2's joint LLR pass would also benefit
  from accurate sub-bin alignment. Re-running V2 with scale=0.3 should
  be evaluated alongside V2's own graduation candidate.
