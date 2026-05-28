---
slug: hb-081-mrc-subtract
mode: ft8
state: shelved
created: 2026-05-27T16:30:00Z
last_updated: 2026-05-27T16:30:00Z
branch: iter/2026-05-27-batch-13
parent_hypothesis: hb-081
wild_card: false
scorecard: research/scorecards/mrc-sub-*.json (transient, removed)
delta_vs_main: -170 to -173 hard-200 recovered at every tested threshold
disposition: SHELVE hb-081 — under-subtraction blocks multipass from finding masked signals. The full ML subtract is already mechanically optimal. Library + CLI flag retained for future re-eval.
---

## Hypothesis

hb-081 (priority 0.40, spawned 2026-05-26 from hb-079): direct analogue
of hb-075's MRC fix for cross-cycle. Weight subtract amplitude by
`min(1, |acc|/threshold)` where |acc| is the un-normalised Costas
accumulator magnitude (rotor confidence). Strong-rotor decodes subtract
fully; weak-rotor (marginal) decodes subtract less — protecting
adjacent bins from over-subtraction by noisy-rotor estimates.

The expected failure mode the variant targeted: hb-079's full ML
projection might be too aggressive for marginal decodes whose rotor is
estimated with high variance, causing collateral damage to adjacent
bins.

## Change

`Ft8Config::coherent_subtract_mrc_threshold: f64` (0.0 = unweighted
full-ML subtract = hb-079; >0.0 enables MRC scaling).
`subtract_decode_coherent` takes a `scale: f64` parameter; the caller
in `coherent_subtract_and_repass` computes `scale = min(1, |acc|/threshold)`
when the config field is >0.0. Research builder
`with_coherent_subtract_mrc_threshold` + `--coherent-mrc-threshold V`
eval flag.

## Result

hard-200 sweep at hb-080's N=3 production (with FP filter):

| threshold | recovered | novel | Δrec | Δnov |
|----------:|----------:|------:|-----:|-----:|
| 0 (off)   |      4604 |   920 |    — |    — |
|         5 |      4434 |   850 | **−170** |  −70 |
|        10 |      4431 |   847 |  −173 |  −73 |
|        20 |      4432 |   843 |  −172 |  −77 |
|        40 |      4432 |   844 |  −172 |  −76 |

**Every threshold regresses −170 to −173 recovered.** No daylight
anywhere on the swept range.

## Why it loses

The failure-mode hb-081 targeted (over-subtract from noisy rotors)
**wasn't real**: hb-080's full-amplitude sweep showed +0 novel cost at
N=1 → N=2 → N=3, meaning hb-079's full ML subtract does not generate
spurious decode candidates from over-subtracted adjacent bins. The
mechanism is already operating cleanly.

What MRC scaling *does* introduce: **under-subtraction**. When a
pass-1 decode is subtracted at half (or any < full) amplitude,
residual signal energy remains at the original decode's positions.
That residual energy then *masks* the masked-by-this-decode signals
that round-2 was supposed to find. The cure (under-subtract) creates
the disease (post-subtract residual blocks tertiary discovery).

The numbers tell the story: −170 recovered is roughly the same as
hb-080's full +16 recall × 10 — i.e., MRC at any threshold breaks
not just the second/third rounds' gains but most of the first round's
gains too. The pipeline's recall depends on *clean* subtraction at
every step.

## Decision

**SHELVE.** Library + CLI flag retained (low cost, useful for
re-evaluation if the corpus or upstream mechanism changes). Default
stays at 0.0 (off).

## Learnings

- **The assumed failure mode wasn't real.** hb-080's clean +N rec / 0
  novel sweep was the diagnostic that should have warned us off
  before implementing MRC — if novels weren't inflating, there was no
  over-subtract problem to fix.
- **Coherent subtract is mechanically optimal at full amplitude.**
  ML projection removes only the rotor-aligned component, preserving
  orthogonal noise. The "MRC weighting helps with bad rotor estimates"
  intuition (which worked beautifully for cross-cycle averaging in
  hb-075) doesn't carry over to subtract — they're structurally
  different operations.
- **Look at the prior iter's data before designing the next iter's
  fix.** hb-080 ran first and showed +0 novels; hb-081 implemented MRC
  to prevent novels that weren't being added. Always check whether
  the variant targets a *measured* problem vs an *assumed* one.

## No new spawns

The "tune subtract amplitude" surface is closed.
