---
slug: hb-086-v2-soft-cancellation
mode: ft8
state: shelved
created: 2026-05-30T00:00:00Z
last_updated: 2026-05-30T00:00:00Z
branch: iter/2026-05-30-batch-15
parent_hypothesis: hb-086 V2
wild_card: false
scorecard: (n/a — shelved at the diagnostic gate; no implementation, no eval run)
delta_vs_main: 0 (no code change merged)
disposition: SHELVE — kill-switch diagnostic confirmed the soft-posterior mechanism does not fit the corpus. No implementation.
---

## Hypothesis

V1 (`joint_pair_retry`, GRADUATED 2026-05-28) recovered +12 hard-200
by retrying failed original sync_candidates against the (post-multipass)
residual spectrogram. The V1 journal
(`2026-05-28-hb-086-joint-pair-retry-v1.md`) documented two leaks:

  (a) candidates whose pass-1 sync_search never found them — V1 only
      retries what `sync_search` already surfaced;
  (b) candidates whose residual LLRs are *still* corrupted after one
      hard ML subtract — by other neighbors C/D/... beyond the
      subtracted A, or by imperfect ML projection on A's modulation
      phase.

V2's design attacks (b) with **soft cancellation**: for V1-failed
candidates that have multiple nearby decoded neighbors, replace the
single hard ML subtract with probability-weighted cancellation of ALL
nearby decoded neighbors — using each neighbor's per-symbol tone
posteriors instead of the hard chosen tone. The intuition (carried
forward from textbook iterative interference cancellation in turbo
decoding): on symbols where a neighbor's LLR magnitude is small, the
true tone is uncertain, and a hard ML projection over-fits to the
likeliest tone, leaving residual energy at the true tone that
contaminates B's overlap bins. Soft cancellation preserves the
genuinely-uncertain symbol energy.

## Kill-switch diagnostic

Per the V2 spec's gate: before implementing, confirm two structural
preconditions hold on the top-20 worst hard-200 WAVs (the densest 17%
of all misses, where the V1 mechanism graduated):

  1. **PRIMARY**: ≥20% of V1-failed candidates must have neighbors
     with *meaningfully soft* tone posteriors — i.e., neighbors whose
     LLR magnitudes are small enough that soft posteriors differ
     non-trivially from hard projections. We proxy this with the
     neighbor-decode SNR distribution: neighbors below ≈ -15 dB are
     marginal-rotor decodes where LLR magnitudes shrink enough that
     soft ≠ hard. (Higher-SNR neighbors have sharp posteriors; soft
     collapses to hard and the mechanism is a no-op.)
  2. **SECONDARY**: ≥20% of V1-failed candidates must have **2+**
     nearby decoded neighbors (the multi-neighbor leak V2 actually
     attacks; if most V1-failures have only one nearby neighbor V1
     already handled it).

`pancetta-research/examples/hb086_v2_soft_posterior_potential.rs`
classifies each missed truth on the top-20 hard-200 WAVs by counting
nearby pancetta decodes at two windows: strict (±25 Hz × ±2 s — the
direct overlap window where V1's hard subtract fires) and relaxed
(±50 Hz × ±2 s — the adjacent-band window where FFT sidelobes still
leak). Production config with V1 ON.

### Diagnostic result

659 missed truths across top-20 WAVs.

| window           | v1-failed | with ≥2 nbrs | multi-nbr % | with marginal-SNR (<-15 dB) nbr | marginal % |
|------------------|----------:|-------------:|------------:|---------------------------------:|-----------:|
| strict ±25 Hz    | 345       | 51           | 14.8%       | 0                                | 0.0%       |
| relaxed ±50 Hz   | 515       | 179          | 34.8%       | 0                                | 0.0%       |

Strict-window neighbor-count distribution (over all 659 missed truths):

| neighbors | count | pct  |
|----------:|------:|-----:|
| 0         | 314   | 47.6%|
| 1         | 294   | 44.6%|
| 2         | 49    | 7.4% |
| 3         | 2     | 0.3% |

Neighbor-decode SNR distribution (strict window, n=398):
p10 = -5.7 dB, p25 = -3.8 dB, **median = -1.5 dB**, marginal (<-15 dB) = 0 (0.0%).
(Relaxed window n=730 is nearly identical: p10 = -5.5, p25 = -3.6,
median = -1.3, marginal = 0%.)

### Verdict: SHELVE

The secondary criterion clears at the relaxed window (34.8%) and fails
at the strict (14.8%). The **primary** criterion fails decisively at
both windows: **0% of V1-failed candidates have a marginal-SNR
neighbor**. Neighbors of missed truths are uniformly strong (median
-1.5 dB; the p10 tail bottoms at -5.7 dB, comfortably inside
high-confidence-LDPC territory). When neighbors decode at this SNR,
the corrected codeword's per-symbol LLRs are large and the tone
posteriors collapse to the hard chosen tone. **There is nothing for
soft cancellation to preserve** that hard projection isn't already
correctly identifying.

This isn't a "noise in the diagnostic" finding — it follows directly
from the corpus's structure. The top-20 worst hard-200 WAVs are
*dense*: pancetta decodes the strong stations; the WSJT-X-but-not-
pancetta misses are the **weak** stations packed in among them. The
leak from V1 → V2 is signals that are too weak for pancetta's
*own* LDPC budget despite a cleaner residual, not signals whose
nearby decoded neighbors are themselves weak/uncertain.

## Why hb-081 strengthens this verdict

hb-081 (MRC-weighted coherent subtract, SHELVED 2026-05-27) was the
adjacent move in the design space: scale the per-decode subtract
amplitude by `min(1, |acc|/threshold)`, dropping subtract energy on
weak/marginal decodes. It regressed −170 hard-200 because
under-subtracting weak decodes left masking energy that blocked the
multipass loop. Soft cancellation is morphologically the same move
applied per-symbol instead of per-decode: drop subtract energy where
the symbol's LLR is uncertain. **The hb-081 result is the same data
point at a coarser granularity** — when neighbors are strong (which
the V2 diagnostic confirms they are), under-subtracting hurts. V2 at
best earns zero on this corpus; more likely it regresses for the same
reason hb-081 did.

## Decision

**SHELVE** without implementation. Per the project's "narrowest
viable slice graduates first" rule and the V1 journal's explicit
guidance to use the diagnostic as a kill switch: the structural
preconditions for soft cancellation to outperform hard subtract do
not hold on this corpus. The +12 hard-200 V1 win is what this
sub-family of mechanism contributes to the project; the V2 path is
closed.

## Learnings

- **The kill switch worked as designed.** Diagnostic at ~3 min of
  CPU saved 2-3 sessions of implementation that would have produced
  a zero-or-negative result. This is the second consecutive hb-086
  iteration where the gate carried the decision (V1 PROCEED →
  graduated, V2 SHELVE → no code).
- **"Soft" buys nothing when the LDPC has already collapsed the
  codeword to high-confidence bits.** Soft cancellation is a turbo-
  decoding move; it pays off when you have multiple noisy estimates
  iterating toward convergence. Here the neighbors are ALREADY
  decoded — they've been through LDPC, CRC, plausibility check.
  Their codewords are correct (essentially probability 1.0); the
  per-symbol tone posteriors from those corrected bits are delta
  functions. Probability-weighted cancellation of a delta function
  is identical to hard cancellation. The mechanism's value
  proposition evaporates.
- **The hard-200 corpus's wall is weak signals, not under-subtracted
  neighbors.** This was implicit in the V1 journal's "Why the win
  is smaller than the diagnostic suggested" section but is now
  explicit. The remaining headroom on hard-200 lives in
  (i) sync_search itself missing weak candidates that the residual
  could rescue (a different mechanism — call it V3-sync-relaxation
  on residual), or (ii) LDPC/OSD budget changes for weak-LLR
  candidates. Both are non-trivially different bets from V2's soft
  cancellation.
- **Two-axis diagnostic > one-axis.** The single-axis "count of
  nearby neighbors" alone would have flagged PROCEED at the relaxed
  window. Adding the SNR-quality axis caught that the count was a
  red herring on this corpus. Future diagnostics on subtract-family
  mechanisms should check neighbor-confidence as well as
  neighbor-count.

## New spawns

- **hb-086 V3** (priority 0.30, spawned 2026-05-30 from V2 shelve):
  attack the OTHER V1 leak (the `(a)` leak — sync_search itself
  misses the candidate). Mechanism: after the multipass + V1 retry
  saturates, re-run sync_search on the residual at a **relaxed**
  threshold (lower `min_sync_score`), but only at frequency bins
  where the residual's tone energy has DROPPED substantially since
  pass-1 (i.e., where a neighbor was subtracted — the residual is
  now genuinely cleaner there). This avoids the broad-band noise
  pickup that a global threshold relaxation would cause, while
  catching the candidates V1 can't because sync_search never
  surfaced them in the first place. Distinct from hb-082 (which
  was a blind global residual-threshold relaxation that did nothing
  — the candidates that surface in the residual sync naturally
  cluster above 3.0). The V3 angle: relax only at *bins newly
  cleaned by subtraction*, where the noise floor genuinely did
  drop.
- (No follow-up V2.5 or per-symbol soft variant — the SNR-quality
  finding closes the soft-cancellation family on this corpus.)
