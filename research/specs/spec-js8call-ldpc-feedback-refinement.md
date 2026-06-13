# Algorithm spec: JS8Call-Improved LDPC feedback refinement

## Source attribution

- Origin: JS8Call-Improved (https://github.com/JS8Call-improved/JS8Call-improved)
- File path (for traceability, NOT to be quoted): `JS8_Mode/ldpc_feedback.h`
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

When an LDPC decoder fails to converge on its first try, much of the
work it did was still informative — the partial codeword it produced
encodes its current best hypothesis. This mechanism uses that
hypothesis to *refine the soft inputs* of a subsequent decode attempt
inside a multi-pass loop, with the goal of breaking the symmetry that
caused the first failure. Bits where the soft input and the candidate
codeword agree get amplified (more confident); bits where they
disagree get attenuated, optionally erased entirely if their
disagreement is shallow enough. The next LDPC pass then runs on the
refined LLRs.

This is conceptually distinct from belief-propagation iterations
*inside* LDPC (which already happens). It is a meta-loop *around*
LDPC, treating the decoder as a black box and reshaping its inputs
between attempts.

## Algorithm description (PROSE ONLY)

### Inputs

- Original soft LLRs: a fixed-size array of floating-point values
  representing the per-bit log-likelihood-ratio estimates from the
  symbol demapper. Length matches the LDPC codeword length (174 for
  FT8 / JS8 Normal).
- Candidate codeword: a fixed-size array of small integers representing
  the hard-decision codeword produced by the most recent LDPC attempt
  (encoded as ±1, or 0/1 — either convention works).
- Erasure threshold: a scalar tunable, controlling at what residual
  LLR magnitude a bit gets marked as an erasure (LLR forced to zero)
  rather than just attenuated. May be disabled outright via an
  environment-variable knob.
- Optional: max iterations counter, also exposed via environment
  variable.

### Outputs

- Refined LLRs: same length as input, with per-bit sign and magnitude
  adjusted.
- A pair of counters: how many bits were classified as "confident"
  (agreement boosted) versus "uncertain" (disagreement attenuated or
  erased). Useful for telemetry and for deciding whether to loop
  again or give up.

### Steps

1. For each bit position `i`:
   1. Look at the sign of the original LLR (which way the soft
      demapper thinks the bit goes).
   2. Look at the bit value in the candidate codeword.
   3. **Agreement case**: original LLR sign matches candidate codeword
      bit. Multiply the LLR magnitude by a positive boost factor (the
      spec exposes this as a tunable; typical values fall in the
      1.1–2.0 range for "modest confidence amplification"). Sign is
      preserved.
   4. **Disagreement case**: original LLR sign disagrees with
      candidate codeword bit. Either attenuate the magnitude (multiply
      by a factor less than 1 — typical 0.3–0.7), or, if the absolute
      LLR magnitude is below the erasure threshold, force it to zero
      (treating the bit as erased — the LDPC decoder will fill it from
      parity).
2. Increment the confident / uncertain counters according to which
   case each bit took.
3. Return the refined LLRs and the counts. The caller decides
   whether to feed them back to LDPC again, hand off to OSD, or stop.

### Numerical constants (facts, not expression)

The exact default values are configurable via environment variables in
the JS8Call-Improved implementation. Specific recommended defaults
were not enumerated by the headers visible at reader date; the
implementer thread should:

- Treat boost factor `α_conf ≈ 1.5` as a starting point and tune.
- Treat attenuation factor `α_unc ≈ 0.5` as a starting point and tune.
- Treat the erasure threshold as a small fraction of the typical LLR
  scale — for an LLR distribution that runs roughly ±10, use 1.0 as a
  starting point.
- Max outer iterations: 2–3 is the regime that pays off; beyond that
  the meta-loop diverges or just spins.

These are *implementer-thread tuning targets*, not facts copied from
the source. They should be confirmed against pancetta's own LLR
distribution shape (which is set by pancetta-ft8's symbol demapper, not
by JS8Call-Improved's).

### Edge cases

- Erasure-disabled mode (env var set): the threshold branch is skipped
  entirely; disagreement bits are always attenuated, never erased.
- All-bits-agreement case: the LDPC decoder converged. The outer
  loop should detect this (uncertain count == 0) and stop.
- All-bits-disagreement case: pathological; means the candidate
  codeword is the complement of the soft input. Almost certainly
  noise; the outer loop should bail rather than re-feed.
- Numerical overflow on repeated boosts: clamp LLR magnitude to a
  reasonable maximum (e.g., ±30 in log-base-e LLR units) to avoid
  saturation issues downstream.

## Conflict with pancetta's existing mechanisms

- Pancetta-ft8 currently runs LDPC with belief-propagation, then OSD
  as a fallback if LDPC fails CRC. There is no meta-loop between LDPC
  attempts — the second attempt (if any) goes straight to OSD with no
  LLR refinement.
- Inserting the feedback-refinement loop between "LDPC failed" and
  "fall through to OSD" is non-conflicting. It gives one or two more
  chances before paying the OSD cost.
- The mechanism is also compatible with multi-pass SIC: the
  feedback-refinement loop sits per-candidate, inside one pass; SIC
  remains the outer outer loop.

## Estimated Rust port effort

- ~150 LOC including configuration plumbing, telemetry counters, and
  unit tests.
- 1–2 implementer sessions.
- Test surface: confirm on pancetta's hard-200 corpus that
  feedback-refined LDPC catches some fraction of the codewords that
  currently fall through to OSD, without elevating false-positive
  rate.

## Implementation notes for the implementer thread

- Insert as a new module `pancetta-ft8/src/ldpc/feedback.rs`, exposing
  one entry point `refine_llrs(llrs: &mut [f32], codeword: &[i8],
  cfg: &FeedbackConfig) -> FeedbackStats`.
- Caller in `pancetta-ft8/src/decoder.rs`: after the LDPC step, if
  CRC fails and `cfg.feedback_max_iters > 0`, run
  `refine_llrs` then retry LDPC; loop up to `feedback_max_iters`
  times before falling through to OSD.
- Config knobs in `pancetta-config::Ft8Config` (or a new
  `Ft8FeedbackConfig` sub-struct): `feedback_max_iters` (default 2),
  `feedback_boost_factor` (default 1.5), `feedback_attenuate_factor`
  (default 0.5), `feedback_erasure_threshold` (default 1.0, set to
  `f32::INFINITY` to disable erasure).
- Telemetry: emit `FeedbackStats { confident_bits, uncertain_bits,
  erased_bits, outer_iters_used }` per candidate; surface in the
  research scorecard so we can A/B against shelf.
- Bench gate: pancetta's hb- numbering convention applies. Suggest
  `hb-220` or next-free.
