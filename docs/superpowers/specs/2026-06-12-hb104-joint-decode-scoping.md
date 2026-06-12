# hb-104 Joint Multi-Candidate Decode — Scoping Spec (Kill-Switch Experiment)

**Date**: 2026-06-12 (Batch 86)
**Status**: SCOPING — defines the kill-switch experiment that decides whether
hb-104 proceeds to a production-design spec or shelves.
**Premise data**: Batch 85 diagnostic — 46.5% of all pancetta misses on the
5/30 corpus (821/1,766) sit within ONE tone spacing (6.25 Hz) of a
time-overlapping signal pancetta successfully decoded; 48.5% within 25 Hz.
The addressable miss population is overwhelmingly co-channel collisions
(hb-100's capture-effect regime: weaker signal blocked at Δf = 6.25 Hz).

## Problem statement

Pancetta's pipeline is greedy: decode strongest → heuristic
spectrogram-domain subtract (`subtract_with_sidelobes`, per-symbol
amplitude from the dB spectrogram, fixed sidelobe scale 0.15) → re-decode
residual. For co-channel pairs the subtraction quality is the binding
constraint: residual error at the victim's exact bins is what blocks the
second decode. hb-104's hypothesis: solving signal amplitudes *jointly*
(vector decode) instead of greedily-sequentially recovers a meaningful
fraction of these misses.

## Kill-switch experiment (this batch)

"One-step ALS" = the first alternating-minimization step, isolated:
**replace the heuristic subtract of the DECODED co-channel signal with a
time-domain least-squares fit, then re-decode the residual at the miss
location.** If precision subtraction of the known signal can't move the
needle, full ALS iteration (re-fit after decoding the victim, alternate)
cannot either — the kill direction is sound. The converse (success)
green-lights the production-design spec for the full loop.

### Slot selection

From the refreshed ft8_lib truth (freq/time now real) × the Batch 66 5/30
scan: rank slots by count of (miss, decoded-signal) pairs with
|Δf| < 6.25 Hz and |Δdt| < 2 s. Take the top 20 slots (script:
`scripts/batch86_select_slots.py`, emits a JSON work list with the pair
coordinates).

### Prototype (`pancetta-research/examples/batch86_hb104_kill_switch.rs`)

Per selected slot:
1. **Greedy baseline**: `Ft8Decoder::default()` full decode →
   TP set vs ft8_lib truth (this includes production's multipass
   subtract — the comparison is against the COMPLETE greedy pipeline).
2. For each (miss, decoded-neighbor) pair from the work list where the
   neighbor IS in the greedy decode output:
   a. Re-encode the neighbor's message (transmit-feature encoder →
      79 tone symbols; skip pairs whose neighbor fails round-trip
      encoding, e.g. nonstandard-call edge cases).
   b. Synthesize its GFSK waveform via the public modulator at the
      decoded (freq, dt) on a 12 kHz grid.
   c. **LS fit**: solve min ‖audio − Σ a_k·s_k‖² for complex amplitude
      a_k (synth waveform + 90°-shifted copy = 2 real unknowns per
      signal; closed-form normal equations). Fit window = the symbols
      where the pair overlaps. Optional refinement (report both): one
      amplitude per 7.9-symbol block (10 unknowns) to track fading.
   d. Subtract the LS estimate from the raw audio.
   e. Re-decode the residual with the default decoder; count NEW
      ft8_lib-truth TPs not in the greedy set (whole-residual decode —
      no frequency scoping, to keep the comparison honest and allow
      serendipitous recoveries).
3. Aggregate: `recovered / target_misses` on the selected pairs.

### Success / abort criteria (pre-registered)

- **PROCEED** (write production-design spec): recovered ≥ 5% of targeted
  co-channel misses (bank's kill-switch threshold), with FP check —
  newly produced non-truth decodes ≤ 1 per recovered TP.
- **WEAK-PROCEED**: 2-5% recovery → one refinement round permitted
  (per-block amplitudes, phase ramp / freq fine-shift ±0.5 Hz in the
  fit) before re-judging.
- **SHELVE**: < 2% after refinement, or FP cost > 1/TP. Record mechanism
  evidence (residual energy at victim bins before/after) so the shelve
  is diagnostic, not just an outcome.

### Measurement notes

- Truth = ft8_lib (neutral); all counts tol=0 exact-text.
- Recovery is measured per-pair but reported per-slot too (a slot's
  second decode may surface without the targeted pair being causal).
- Wall-clock is uncapped for the prototype (research-only); production
  budget questions belong to the design spec, not the kill-switch.

## Explicitly out of scope (production-design spec, only if PROCEED)

- Full ALS loop (re-fit after victim decode, iterate to convergence)
- Joint solve across >2 signals (the K×K system)
- Candidate-hypothesis decoding without a known victim location
  (production has no truth file — victim discovery must come from
  sync candidates that failed LDPC, hb-086-style)
- Real-time budget, tier gating, config surface

## Risks

- Synth mismatch: pancetta's modulator pulse shape vs the on-air
  station's (different GFSK BT, audio chain). LS amplitude fit absorbs
  scale/phase, not pulse-shape mismatch — per-block amplitudes partially
  compensate. If mismatch dominates, the refinement round will show
  per-block fits helping; full mismatch-robustness is a design-spec
  topic (matched filtering on the RECEIVED estimate à la hb-090).
- Frequency quantization: decoded freq is grid-quantized (6.25/2 Hz
  bins + parabolic refinement); a 0.1-0.5 Hz error decorrelates the
  synth over 12.6 s. The ±0.5 Hz fine-shift in the refinement round
  addresses this; report fit residual energy to diagnose.
