# Algorithm spec: sub-sample DT refinement (FT4/FT2 — applicable to FT8)

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary document (traceability only, NOT quoted):
  `Release_Notes.txt` lines ~73–75 (v3.1.0 260522): "Added a7 decoding
  technology, sub-sample DT refinement, and a 4th pass after a7 for
  both FT4 and FT2."
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  Existing FT4/FT2 fine-sync routines (the sub-sample refinement is a
  modification of the per-pass sync). Specific path not enumerated in
  release notes; structural model from FT8's
  `wsjtx/lib/ft8/sync8.f90` and `wsjtx/lib/ft8/sync8d.f90`.
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: introduced as part of WSJT-X Improved v3.1.0 for
  FT4/FT2. The technique is general signal-processing — sub-sample
  time-offset refinement via interpolation of the sync correlation
  — and predates WSJT-X. ft8mon (in pancetta's spec library) also has
  a related sub-bin Costas refinement (see spec-ft8mon-sub-bin-costas.md
  and spec-ft8mon-known-tone-refinement.md for frequency-domain analogs).

## Purpose

FT8 / FT4 / FT2 sync detection finds the time-domain location of the
Costas arrays within the audio window. The sync algorithm searches over
a grid of `(time_offset, frequency_offset)` candidate values and picks
the one with the highest correlation against the known Costas tone
sequences.

Standard sync grid resolution is one audio sample (or half-sample) in
time. At 12 kHz sample rate, that is ~83 µs — already very fine, but
the demodulation pipeline that follows is sensitive to fractional-sample
drift. Specifically, FT8 demodulates 79 symbols of 0.16 s each; a sync
error of even half a sample compounds into a per-symbol phase error
that degrades the soft-LLR values fed to LDPC.

The standard fix is to oversample (run the sync at a finer grid), but
oversampling is expensive — O(grid_size × candidates × FFT cost).
The sub-sample DT refinement is a cheaper alternative: take the coarse
sync result and **interpolate around the peak** to find a fractional
time-offset that maximizes the correlation. This recovers most of the
benefit of a finer grid at a fraction of the CPU cost.

The release notes describe this addition (v3.1.0 line ~74, verbatim):
"Added a7 decoding technology, sub-sample DT refinement, and a 4th
pass after a7 for both FT4 and FT2."

The mechanism is mode-agnostic — FT8 already has a partial version of
this (the coarse sync grid is followed by a fine-sync refinement step
in `sync8d.f90`); the v3.1.0 work extended sub-sample-accurate
refinement to FT4 and FT2. Pancetta should consider implementing
sub-sample DT refinement *for FT8* if it isn't already at this fidelity.

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm is invoked per sync candidate, after the standard
coarse-and-fine sync grid search has produced an integer-sample
time-offset answer. Inputs:

- **Sync correlation function**: a vector of correlation values
  `c[k]` over a small range of time offsets `k = ..., -1, 0, +1, ...`
  near the coarse-best offset (call it `k* = 0`).
- **Coarse-best time-offset**: `t_coarse` (in samples).
- **Sample rate**: 12 kHz for FT8 (6 kHz for FT4; 16 kHz for FT2).

### Outputs

- **Refined sub-sample time offset**: `t_refined = t_coarse + δt`,
  where `δt ∈ (-0.5, +0.5)` samples is the interpolated peak location.
- Equivalent in seconds: `dt_refined = t_refined / sample_rate`.

### Steps

1. **Take three correlation values around the peak.** Let `c[-1]`,
   `c[0]`, and `c[+1]` be the correlation values at one sample before,
   exactly at, and one sample after the coarse-best offset. These are
   already computed by the standard sync; the refinement is essentially
   free to add.

2. **Fit a parabola.** A parabola through three equally-spaced points
   has a closed-form vertex location:
   `δt = 0.5 * (c[-1] - c[+1]) / (c[-1] - 2*c[0] + c[+1])`.
   This is the standard parabolic interpolation formula (e.g.,
   Smith's "Spectral Audio Signal Processing" — public textbook
   reference, no GPL).
   - Numerator: `c[-1] - c[+1]` measures the asymmetry of the peak.
   - Denominator: `c[-1] - 2*c[0] + c[+1]` is the negative discrete
     second derivative at the peak (a sanity check: should be positive
     for a genuine local maximum; if zero or negative, fall back to
     `δt = 0`).

3. **Clamp the result.** If `|δt| > 0.5`, the parabolic fit has wandered
   beyond the bracketing interval (likely because the peak is not at
   `k=0` after all, or because of noise asymmetry). Clamp to `[-0.5,
   +0.5]` and continue. The caller's downstream pipeline will tolerate
   the residual error.

4. **Update the time offset.** `t_refined = t_coarse + δt`. Pass this
   to the demodulation stage in lieu of `t_coarse`. The demodulation
   uses `t_refined` to set the per-symbol sampling phase for
   per-symbol-coherent integration; an accurate sub-sample offset
   minimizes the per-symbol phase drift and improves SNR of the
   resulting LLRs.

5. **Optionally repeat for frequency.** The same parabolic
   interpolation can refine the frequency offset (Spectral interpolation
   — analogous to spec-ft8mon-sub-bin-costas.md and the FT8 fine-sync
   pass). If pancetta already does spectral interpolation, no change.
   If only time-domain refinement is wanted (the v3.1.0 release notes
   specifically call out "sub-sample DT" — i.e., time only), keep the
   frequency offset as found by the standard search.

### Numerical constants (facts, not expression)

- **Sample rate (FT8)**: 12 000 Hz. Sub-sample resolution achievable
  via parabolic interpolation: down to ~10 µs (a few percent of a
  sample period). Linear interpolation gives ~50% of a sample;
  parabolic gives ~5–10%.
- **Three-point parabolic interpolation formula**:
  `δt = 0.5 * (c[-1] - c[+1]) / (c[-1] - 2*c[0] + c[+1])`.
  Closed-form; trivial to evaluate.
- **Clamp range**: `δt ∈ [-0.5, +0.5]`. Outside this, the
  parabolic fit is invalid (the peak isn't really at `k=0`).
- **Sanity check on denominator**: must be positive. A zero or
  negative denominator means the three points are not a peak (they're
  a flat or rising surface). Fall back to `δt = 0`.

### Edge cases

- **Two-tied correlation peaks.** If `c[-1] == c[+1]`, the parabolic
  fit gives `δt = 0` — fine. If `c[0] == c[-1] > c[+1]`, the parabolic
  fit gives a non-zero `δt`, which represents the correct sub-sample
  location of the asymmetric peak. The algorithm handles both naturally.
- **Noise asymmetry.** Bin noise can produce a `δt` that points away
  from the true peak. Empirically, parabolic interpolation is robust
  to noise at SNR > -15 dB; below that, the refinement is unhelpful
  but not harmful (the LDPC pipeline absorbs the residual error).
- **Peak exactly at a sample boundary.** All three correlations are
  approximately equal. The numerator is small; the denominator is
  small. The ratio is approximately `0/0` — the clamp catches it
  (`δt = 0` is the correct answer here anyway).
- **Sync candidate at sample-vector boundary.** The refinement
  requires `c[-1]` and `c[+1]` to exist. If the coarse sync peak is
  at the very first or last sample of the search range, the refinement
  is unavailable. Fall back to `δt = 0` and log at `debug!` level.

## Conflict with pancetta's existing mechanisms

- **Pancetta has fine sync but unknown sub-sample refinement.**
  Check the current `pancetta-ft8/src/sync.rs` (or equivalent) — does
  the existing fine-sync already do parabolic interpolation? If yes,
  this spec is informational (no port needed). If no, this is a
  small, high-value addition.

- **Synergy with the cross-sequence a7 spec.** a7's fine-sync step
  (per spec-wsjtr-cross-sequence-a7.md step 6) uses ±10 sample / ±2.5
  Hz coarse + ±4 sample / ±0.5 Hz refine windows. Adding parabolic
  refinement on top recovers the last ~5–10% sub-sample precision
  essentially free.

- **Synergy with spec-ft8mon-known-tone-refinement.md and
  spec-ft8mon-sub-bin-costas.md.** Those specs cover sub-bin
  *frequency* refinement (spectral interpolation). The present spec
  covers sub-sample *time* refinement (sync-correlation interpolation).
  Both stack — one over time, one over frequency. The fully-refined
  `(dt, freq)` is what feeds the demodulation pipeline.

- **No conflict with hb-091/hb-216 (tier).** The refinement is O(1)
  per sync candidate. Default on across all tiers.

- **No conflict with FP filters.** Better sync → cleaner LLRs → more
  decisive LDPC outcomes. The FP filter pipeline downstream is
  unaffected.

- **Interaction with the 4th-pass-after-a7 spec.** Both add small
  yield improvements. Stacking together is multiplicative for marginal
  signals near the decode threshold.

## Estimated Rust port effort

- New code:
  - A `parabolic_peak(c_minus, c_zero, c_plus) -> f32` helper in
    `pancetta-dsp/src/sync.rs` (or equivalent). ~20 LOC + ~40 LOC
    tests (synthetic peaks at known sub-sample positions, recover
    them to 1% accuracy).
  - Integration into existing sync paths: call `parabolic_peak` after
    the coarse-and-fine sync grid produces its winner. ~15 LOC per
    call site (likely 2–3 call sites). ~45 LOC total.
- Total: ~65 LOC + ~40 LOC tests.
- 1 iter session:
  - Implement, write synthetic tests, run hard-200, bootstrap-CI on
    yield change.

## Implementation notes for the implementer thread

- **The math is closed-form.** The parabolic peak formula is two
  lines of code. Do not over-engineer — no need for higher-order fits
  or iterative refinement. Three points + the closed form is enough
  to recover ~5–10% of a sample period, which is the limiting
  precision below which LDPC noise dominates.

- **Test against synthetic peaks first.** Construct a synthetic
  correlation vector with a known sub-sample peak (e.g., generate
  `c[k] = exp(-(k - 0.37)^2 / 2)` and verify the refinement
  recovers 0.37 to within 0.01 or so). This unit test is cheap and
  catches sign errors / off-by-one errors immediately.

- **Sanity-check the denominator.** Always check the denominator's
  sign before dividing. A non-positive denominator means the three
  points are not a peak; fall back to `δt = 0`.

- **Clamp to ±0.5.** Always. If the unclamped value falls outside
  the bracketing interval, the answer is wrong, and the clamped
  value is at worst not-better than `δt = 0`.

- **Provenance is unnecessary.** Sub-sample refinement is a
  pre-demodulation step. The decoded message looks exactly like a
  standard decode; no `DecodeProvenance` tag is needed.

- **No tier gating.** O(1) per sync candidate; turn it on
  unconditionally.

- **Citation hygiene.** Cite as "parabolic peak interpolation
  (textbook: Smith, Spectral Audio Signal Processing, public)" for
  the math, and "inspired by WSJT-X Improved v3.1.0 sub-sample DT
  refinement (DG2YCB)" for the operational application. The math is
  textbook; the inspiration for applying it at the FT-mode sync
  stage is the WSJT-X Improved release notes.
