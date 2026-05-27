---
slug: hb-075-mrc-coherent
mode: ft8
state: graduated
created: 2026-05-26T18:00:00Z
last_updated: 2026-05-26T18:00:00Z
branch: iter/2026-05-26-batch-12
parent_hypothesis: hb-075
wild_card: false
scorecard: research/scorecards/history/2026-05-26-hb-075-mrc-coherent.json
delta_vs_main: composite +0.001283 (0.556996 -> 0.558279); hard-200 +22 rec / +1 novel; hard-1000 +78 rec / -7 novel
disposition: GRADUATE hb-075 — MRC-weighted coherent cross-cycle averaging is the production default. Biggest single-iter win of the session.
---

## Hypothesis

hb-075 (priority 0.30, spawned 2026-05-26 from hb-074): weight each
member's contribution by the magnitude of its un-normalised Costas
accumulator, instead of the unit-magnitude rotor hb-074 used.
Equivalent to multiplying each member by `conj(acc_i)` directly —
alignment + MRC magnitude-weighting in one op. The fix targeted
**hb-074's diagnosed failure mode**: noisy rotors on marginal
candidates raised sum variance and dropped ~10 hard-200 recovered.

## Change

`pancetta-ft8/src/decoder.rs`:
- New `Ft8Config::cross_cycle_coherent_mrc: bool` flag (default flipped
  false → **true**) — paired with `cross_cycle_coherent` (also default
  flipped → **true**).
- Refactored hb-074's `estimate_candidate_phase_rotor` to share
  `compute_costas_complex_accumulator` (the un-normalised sum).
- Coherent branch of `cross_cycle_averaging_pass` now picks the
  multiplier: `conj(acc)` when MRC is on, `conj(rotor)` (unit) when off.
  `conj(acc)` = `|acc|·conj(rotor)`, so this gives both alignment and
  MRC weighting in one op — strong rotors dominate the sum, noisy weak
  rotors contribute weakly.

Research builder `with_cross_cycle_coherent_mrc` + `--cross-cycle-coherent-mrc`
eval flag. Lib tests stay 195 (the existing rotor unit test is
unchanged; MRC is a callsite choice, not a function-shape change).

## Result

### Targeted hard-200 four-way A/B

| config              | recovered | novel | rate    |
|---------------------|----------:|------:|--------:|
| noncoh no-filter    |      4409 |  1613 | 0.51411 |
| **MRC  no-filter**  | **4431 (+22)** | 1620 (+7) | 0.51667 |
| noncoh with filter  |      4408 |   844 | 0.51399 |
| **MRC  with filter**| **4430 (+22)** | **845 (+1)** | 0.51656 |

vs hb-074 unweighted coherent (yesterday): **+22 vs -10** — the MRC
weighting flips the sign of the delta exactly as the failure-mode
diagnosis predicted.

### Full 5-tier with production FP filter

| metric                     | old main.json (hb-056) | hb-075 on  | Δ        |
|----------------------------|-----------------------:|-----------:|---------:|
| **composite**              |               0.556996 | **0.558279** | **+0.001283** |
| fixtures pass_rate         |                    1.0 |        1.0 |        0 |
| synth-clean @50            |                    -20 |        -20 |        0 |
| hard-200 rec / novel       |             4408 / 844 | 4430 / 845 | +22 / +1 |
| **hard-1000 rec / novel**  |            14437 / 2897 | **14515 / 2890** | **+78 / -7** |
| wild-50                    |                      0 |          0 |        0 |
| elapsed                    |                  1785s |      1782s |     ~0   |

**Hard-1000 novels DECREASED** (-7). The MRC weighting is *more*
precise than the non-coherent power sum — strong-rotor weighting
filters out marginally-aligned junk that non-coherent's plain power
summation admits. Recall AND precision both moved positive on the
larger corpus.

## Why hb-074 lost and hb-075 wins

hb-074's diagnosis was correct: marginal-candidate rotors are noisy
(phase precision ~ 1/√(N_costas · SNR)), and `|cs_strong +
cs_weak·exp(jε)|²` has the same mean as the non-coherent power sum
but higher variance — dropping ~10 cases below the LDPC threshold.

hb-075's MRC weighting fixes that by scaling each member's
contribution by its rotor magnitude (which is proportional to signal
strength × √N_costas). A strong member contributes ~fully; a noisily-
phased weak member contributes ~weakly. The variance term drops by
roughly `(w_weak / w_strong)²` — large enough to flip the sign in
practice (-10 → +22 recovered).

Equivalent math: `|sum(mag_i · aligned_cs_i)|²` is the MRC-optimal
combiner when the signal-strength-to-noise ratio is roughly equal
across members (close to true here, since the candidates are
clustered by sync-score band).

## Decision

**GRADUATE.** `cross_cycle_coherent = true`, `cross_cycle_coherent_mrc
= true` (both default). main.json updated; scorecard archived to
`history/2026-05-26-hb-075-mrc-coherent.json`.

## Cumulative session impact

(start of session 2026-05-25 → now)

| metric                 | start     | now       | Δ          |
|------------------------|----------:|----------:|-----------:|
| **composite**          |  0.555131 | **0.558279** | **+0.003148** |
| hard-200 rec           |      4376 |      4430 | **+54**    |
| hard-1000 rec          |     14267 |     14515 | **+248**   |
| fixtures pass_rate     |       1.0 |       1.0 |          0 |
| synth-clean @50        |       -20 |       -20 |          0 |

5 graduations this session: hb-063 layered BP, hb-056 non-coherent
cross-cycle, hb-058 contest-FP rejection, hb-060/061 cleanup, hb-075
MRC coherent cross-cycle.

## Learnings / follow-ups

- **Negative-result iters pay back.** hb-074's "shelve with
  infrastructure" produced the diagnosis that made hb-075's one-line
  fix obvious. Without the negative there's no targeted MRC variant.
  Keep documenting negatives end-to-end — they're the seed of the
  next positive.
- **Coherent integration on real audio is corpus-sensitive but
  algorithm-fixable.** The non-coherent floor (hb-056 +0.000816) was a
  measurement of one algorithm's behavior, not an architectural
  ceiling. With MRC weighting, the coherent path is now ~1.5× the
  non-coherent contribution.
- **hb-076 (per-Costas-block phase recovery)** is now lower priority —
  the global rotor + MRC weighting handles the variance issue without
  needing intra-slot phase modeling. Bump hb-076 to 0.20 (was 0.30).
- **hb-077 (phase-coherent SDR-IQ corpus)** is also lower priority —
  the operator audio supports coherent gain just fine once weighted
  correctly. Bump to 0.20.
- Spectrogram complex-retention costs ~2× memory in the spectrogram
  pass; wall-clock unchanged. Acceptable for the +0.001283 composite
  win. If memory ever pinches, hb-078 (selective complex retention —
  only for the tone bands around candidates) is a follow-up.
