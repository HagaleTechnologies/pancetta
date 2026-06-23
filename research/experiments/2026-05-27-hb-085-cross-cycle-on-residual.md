---
slug: hb-085-cross-cycle-on-residual
mode: ft8
state: shelved
created: 2026-05-27T17:00:00Z
last_updated: 2026-05-27T17:00:00Z
branch: iter/2026-05-27-batch-13
parent_hypothesis: hb-085 (new this batch)
wild_card: false
scorecard: n/a (design-analysis shelve, no code)
delta_vs_main: n/a
disposition: SHELVE hb-085 before implementation — running cross-cycle averaging on the post-subtract residual is structurally ill-motivated.
---

## Hypothesis

After hb-079's multipass subtracts pass-1 + cross-cycle decodes from the
complex spectrogram, re-run cross-cycle averaging on the residual to
catch repeating stations whose first repetitions were masked then
revealed by subtraction.

## Why shelve

The cross-cycle pass groups candidates at the *same* `(freq_sub,
freq_bin±1, t0 ≡ k·188 ±2)` and averages their per-symbol tone power
across slot repetitions. After hb-079's subtract:

1. **At positions that DID decode in pass 1**, the complex spectrogram is
   subtracted to near-zero by ML projection. Any cross-cycle group that
   contains such a position averages with zero — *dilutes* the
   integration, doesn't help.
2. **At positions newly revealed in the residual**, the candidates are
   typically *not* at the same `(freq_sub, freq_bin, t0 mod slot)` as
   any original repeating-station group — they're new, isolated decodes
   exposed by removing the masking neighbor. They have no peer in the
   residual to average with.
3. **For genuinely repeating stations the original cross-cycle missed:**
   if a station repeats in 3 slots and pass-1 cross-cycle integrated
   all 3 → cross-cycle already gets the full benefit. If pass-1 cross-
   cycle decoded only 1 (the strongest), the other 2 are still in the
   *original* spectrogram (pre-subtract) — but the original cross-cycle
   already saw them and tried to integrate. If it failed there (because
   the integrated SNR was sub-threshold), running it again post-subtract
   only changes things if subtraction *increased* SNR at the masked
   positions — which it doesn't (subtraction at a *different* position
   doesn't help the masked one).

The integration that *would* help — coherent subtract of the masking
signal followed by cross-cycle integration on the now-unmasked positions
— is exactly what hb-079's existing pipeline does *implicitly* on the
single un-subtracted slot's data, then re-decodes it as a standalone
candidate. The integration step doesn't add anything.

## Pattern: post-hb-079 follow-ups are saturated

This is the third consecutive shelve in batch 13 after hb-079
graduation (hb-081 −170 rec, hb-082 0 effect, now hb-085 unmotivated).
The pipeline is at its mechanical limit for this corpus:

- **N=3 multipass** found all the extractable masked signals (hb-080).
- **Full ML subtract** is already mechanically optimal (hb-081).
- **Residual sync threshold** isn't binding (hb-082).
- **Cross-cycle on residual** is structurally redundant (this).

Further composite gain requires *structural* changes that attack a
*different* limit (interference handling at the LDPC level via joint
decoding, OR coherent subtract that knows about adjacent-station
tone leakage, OR a fundamentally different decoder). The follow-up
tuning surface is closed.

## Decision

**SHELVE** before implementation. No code. The analysis above is the
deliverable. Recorded as the third "post-hb-079 follow-ups saturate"
data point feeding iter 7's joint-decoding design spec.

## Learnings

- **hb-079 was a sharp lever** — it extracted essentially everything
  the iterative-subtract mechanism could extract from this corpus.
  The follow-ups can refine but not amplify.
- **The "post-big-win shelve pattern"** mirrors what we saw after
  hb-075 (cross-cycle MRC): the surface around a graduated structural
  lever is mostly already-optimal. Look for the next *different*
  surface, not deeper drilling on the current one.
