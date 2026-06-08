# Algorithm spec: automatic passband baseline — rig-aware filter-edge optimization

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary documents (traceability only, NOT quoted):
  `Release_Notes.txt` lines ~76–79 (v3.1.0 260522 — the introduction of
  automatic passband baseline calculation).
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  `wsjtx/lib/ft8/baseline.f90` (the FT8 baseline computation),
  `wsjtx/lib/ft8/get_spectrum_baseline.f90` (FT8 spectrum baseline
  extraction — the new file added in v3.1.0),
  `wsjtx/lib/ft4/ft4_baseline.f90` (FT4 baseline; parallel implementation),
  `wsjtx/lib/fst4/fst4_baseline.f90` (FST4 baseline; parallel
  implementation; out of scope for FT8 work but informative for the
  general technique).
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: introduced in WSJT-X Improved v3.1.0 (May 2026).
  Not in WSJT-X mainline. JTDX has a related but distinct adaptive-baseline
  feature ("Adjust" / "AutoLevel").

## Purpose

FT8 decoders examine an audio spectrum window (typically 100–4500 Hz at
6.25 Hz bin resolution) for tone candidates. Before scoring candidates,
the spectrum is **normalized against a baseline** — an estimate of the
noise-floor energy per bin. Candidates are then ranked relative to that
baseline. A clean baseline → reliable candidate ranking → good
sensitivity *and* few false decodes.

Two failure modes plague the standard baseline:

1. **Operator's rig has a steeply rolled-off audio passband.** A typical
   SSB transceiver passes 200–2700 Hz cleanly but rolls off sharply
   outside. The audio spectrum below ~150 Hz and above ~2800 Hz contains
   mostly DSP-rolloff noise that gets fitted as if it were ambient noise
   — pulling the baseline down at the band edges and inflating apparent
   tone amplitudes in those regions. Result: candidates near the edges
   look artificially strong and produce false decodes.
2. **Operator's Wide Graph limits are poorly set.** The Wide Graph UI
   widget defines the spectrum slice the decoder searches. If the
   operator drags the limits to cover frequencies outside the rig's
   actual passband, the baseline computation includes regions with
   essentially no signal energy, again skewing the noise estimate.

The automatic passband baseline is a single mechanism that addresses
both: it detects the rig's actual usable passband from the spectrum's
own shape and confines the baseline computation to that interval. The
release notes phrase it (verbatim, line ~77): "The filter edges are now
automatically optimized according to the actual passband of your
transceiver." Net effect (verbatim, line ~78): "better decoding
performance and fewer false decodes when the Wide Graph limits are
poorly set."

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm runs once per receive window (post-FFT, pre-decode). Inputs:

- **Spectrum**: a long-window power-spectral-density vector covering
  the operator's Wide Graph range — typically 0–4500 Hz at 6.25 Hz bin
  resolution (the FT8 standard). In WSJT-X the underlying FFT is taken
  on a few-seconds slice and averaged across the window.
- **Operator-configured Wide Graph window**: `wg_low_hz` and
  `wg_high_hz` (e.g., 200 Hz and 3000 Hz). These are *user* hints to
  the decoder about where to look; the new mechanism treats them as
  outer bounds, not as exact filter edges.
- **Per-bin sample count or averaging duration**: needed to assess
  per-bin noise variance.

### Outputs

- **`auto_low_hz`** and **`auto_high_hz`**: the auto-detected passband
  edges, where the rig actually passes signal. Always `wg_low_hz ≤
  auto_low_hz ≤ auto_high_hz ≤ wg_high_hz`.
- A baseline (per-bin noise-floor estimate) computed *only over*
  `[auto_low_hz, auto_high_hz]` and interpolated/extrapolated outside.

### Steps

1. **Compute average power per frequency bin** across the entire
   spectrum range `[wg_low_hz, wg_high_hz]`. This is unchanged from the
   standard baseline pipeline — the FFT result is already in this form.

2. **Smooth the spectrum to expose rolloff shape.** Apply a wide moving
   average (e.g., 50–100 bins, ≈ 300–600 Hz) along the frequency axis.
   The smoothing window must be wide enough to average over the FT8
   tones themselves (which are 6.25 Hz wide and clustered in 50 Hz
   blocks) so that what remains is the slowly-varying rig passband
   shape.

3. **Identify the rig passband.** Find the contiguous frequency range
   over which the smoothed spectrum sits within a reasonable factor of
   its peak value. Concretely (per the release notes' description of
   "the actual passband of your transceiver"):
   - Find `peak_power = max(smoothed_spectrum)` over the operator's
     Wide Graph window.
   - Find a threshold `t = peak_power - delta_dB` where `delta_dB` is
     the rolloff allowance (a typical SSB rig has a 3-dB roll-off at
     the passband edge; a 6-dB to 10-dB threshold is a reasonable
     "where the noise floor has clearly dropped from the in-band
     value" boundary).
   - `auto_low_hz` = lowest frequency at which the smoothed spectrum
     exceeds `t` continuously to its right.
   - `auto_high_hz` = highest frequency at which the smoothed spectrum
     exceeds `t` continuously to its left.
   - Both clamped to `[wg_low_hz, wg_high_hz]`.

4. **Reject edge-rolloff bins from the baseline computation.** Replace
   the baseline values for bins below `auto_low_hz` or above
   `auto_high_hz` with the value at the nearest in-band bin (or with
   the average of the in-band baseline; either is reasonable). This
   prevents bins outside the rig passband from artificially lowering
   the noise floor.

5. **Compute the baseline normally over the auto-detected window.**
   The standard baseline computation (e.g., percentile-based or median-
   filter-based per-bin noise estimate) runs on the bins in
   `[auto_low_hz, auto_high_hz]`. The in-band region's baseline is
   reliable because it does not include rolloff noise.

6. **Apply the baseline to all candidates as before.** Tone candidates
   outside `[auto_low_hz, auto_high_hz]` may still be examined (if the
   operator has the Wide Graph range wider than the rig passband) but
   their per-bin amplitude is normalized against an in-band-extrapolated
   baseline rather than the inflated edge-rolloff baseline. False
   candidates from the rolloff region thus look correctly weak and are
   filtered out by the candidate-strength gate.

7. **Update on rig passband changes.** Per-window recomputation is the
   simplest design and the release notes' wording implies it (the
   filter edges are recomputed each receive cycle from the current
   spectrum shape). If the operator retunes the rig or adjusts the audio
   gain mid-session, the auto-passband adjusts within one or two windows
   without any operator action.

8. **Sanity bounds.** Always enforce a minimum-width passband (e.g.,
   `auto_high_hz - auto_low_hz >= 500 Hz`). If the detection produces a
   width smaller than this, fall back to the operator's `[wg_low_hz,
   wg_high_hz]` directly (likely indicates an empty band, a strong
   single carrier dominating the spectrum, or a configuration error).

### Numerical constants (facts, not expression)

- **FFT bin resolution**: 6.25 Hz (standard FT8; not specific to this
  mechanism).
- **Modes affected by automatic passband baseline** (per release
  notes line ~76, verbatim): FT4, FT2, and FT8 (STD — single-threaded
  decoder).
  - Implication: as of v3.1.0 the MTD does not get the auto-passband
    treatment; this is presumably future work. Pancetta should apply
    auto-passband to its main decoder path regardless of whether it
    has a STD/MTD split.
- **Smoothing window width**: not in the release notes. Empirically
  reasonable: 50–100 bins (≈ 300–600 Hz), enough to average over
  individual FT8 signals but narrower than typical SSB rig rolloff
  shape (~200 Hz transitions).
- **Rolloff allowance** (`delta_dB`): not in the release notes.
  Empirically reasonable: 6 dB. This is the standard
  half-power-down boundary of an SSB rig's audio passband.
- **Minimum sane passband width**: not in the release notes.
  Recommended sanity floor: 500 Hz. Below this, fall back to the
  operator's Wide Graph window.

### Edge cases

- **Wide Graph already tightly bounded.** If the operator has carefully
  set `[wg_low_hz, wg_high_hz]` to match her rig (e.g., 300–2700 Hz),
  the auto-passband detection will return values close to those bounds
  and the mechanism's behavior is essentially a no-op. Sensitivity does
  not regress — this is the design point per release notes line ~78
  ("the Wide Graph limits are poorly set").
- **Wide Graph set wider than the rig passband.** The common case for
  inexperienced operators (Wide Graph set 0–4500 Hz with an SSB rig
  passing 200–2700 Hz). The auto-passband shrinks the working window
  to the rig's actual passband, dramatically improving baseline quality
  and reducing FPs.
- **Very strong in-band signal.** A loud carrier or strong CW signal can
  produce a spectrum peak that dominates the smoothed-spectrum
  computation. The threshold `t = peak - delta_dB` then becomes too
  high and shrinks the auto-passband too aggressively. Mitigation:
  cap the peak value at some quantile (e.g., 95th percentile) rather
  than the absolute max.
- **Empty band.** If there's literally no signal energy anywhere in the
  spectrum, the smoothed spectrum is flat noise and the detected
  passband is essentially the full window. This is fine — the sanity
  floor check (step 8) ensures the result is sensible.
- **Audio DC component.** Many SSB audio chains have residual DC at
  bin 0 that looks like a huge peak. Skip bins below ~50 Hz when
  computing `peak_power` to avoid biasing the threshold.
- **Operator changes Wide Graph range mid-session.** The new window is
  used immediately on the next decode cycle. No special handling
  needed.
- **Symmetric rolloff is not guaranteed.** An SSB rig may pass
  200–2700 Hz (asymmetric around the band center). The algorithm
  detects each edge independently; do not assume `auto_low_hz =
  wg_low_hz + offset` and `auto_high_hz = wg_high_hz - offset`.

## Conflict with pancetta's existing mechanisms

- **Pancetta already has a baseline computation.** Check the current
  `pancetta-dsp/src/baseline.rs` (or equivalent) and `pancetta-ft8/`
  paths. The standard WSJT-X-derived baseline is a percentile-per-bin
  or rolling-median-per-bin estimator. The auto-passband mechanism
  layers in front of it (selects the bins, then runs the existing
  estimator on the selected range) — it is *not* a baseline algorithm
  replacement.

- **No conflict with hb-091 (scoped fast path) / hb-216 (tier
  classification).** The auto-passband adds at most a smoothing pass
  and an O(n_bins) edge-finding pass — negligible cost on any tier.
  Default-enabled on all tiers.

- **Synergy with hb-156 (lid_of_band tier) — weak.** Both are about
  the spectral shape, but hb-156 categorizes per-WAV difficulty, while
  auto-passband adjusts the actual baseline used. They are orthogonal.

- **Synergy with FP-filter line (hb-058/hb-062/hb-103) — significant.**
  Auto-passband removes FPs at the *baseline* level (before candidate
  generation). The FP-filter line removes FPs at the *acceptance* level
  (after LDPC convergence). Stacked, they multiply.
  - Expected effect: auto-passband alone should reduce FP rate by
    5–15% for operators whose rig passband is narrower than their
    Wide Graph window. (Not a measured number in WSJT-X Improved
    documentation; bootstrap-CI on pancetta's hard-200 to validate.)

- **No conflict with hb-103.** Auto-passband makes the post-decode
  feature values (especially `min_llr_magnitude` and `sync_quality`)
  more reliable, which strengthens hb-103's scoring. Pure win.

- **Interaction with the wide-graph display.** Pancetta does not have
  a Wide Graph UI per se, but the audio-input pipeline already has
  configured frequency limits in `pancetta-config`. The auto-passband
  reads those as the operator-hint bounds and tightens internally.
  The configured limits remain the outer envelope.

## Estimated Rust port effort

- New module:
  - `pancetta-dsp/src/passband.rs` (or extend existing baseline module):
    `detect_passband(spectrum: &[f32], wg_low: f32, wg_high: f32) ->
    (f32, f32)` — returns `(auto_low_hz, auto_high_hz)`. Smoothing
    pass + threshold-edge finder + sanity floor. ~80 LOC + ~80 LOC
    tests.
  - Extension to existing `baseline.rs`: a `compute_baseline_with_auto_passband`
    wrapper that calls `detect_passband` then runs the existing baseline
    on the auto-detected window. ~40 LOC + ~40 LOC tests.
- Coordinator/decoder wiring:
  - Update the call site to use the auto-passband baseline instead of
    the raw `[wg_low, wg_high]` baseline. ~20 LOC.
  - Config: a `enable_auto_passband: bool` flag in `Ft8Config`,
    default `true`. ~10 LOC.
- Calibration:
  - Test on a corpus of WAVs from rigs with different audio passbands
    (Yaesu FTdx10, Kenwood TS-590, IC-7300 — three common SSB rigs).
    Confirm auto-passband detects sensible edges per rig. Measure
    FP-rate change.
- Total: ~150 LOC + ~150 LOC tests + 1 calibration session.
- 2–3 iter sessions:
  1. `passband.rs` standalone module + synthetic tests (constructed
     spectra with known rolloff).
  2. Wiring into baseline + decoder + config.
  3. Hard-200 eval, bootstrap-CI, ship/shelve.

## Implementation notes for the implementer thread

- **The mechanism is shape-detection on the smoothed spectrum.** Do
  not try to make it cleverer with rig-model-specific tables (rigctld
  could in principle tell us "this is an FTdx10"); the spectrum-based
  detection is self-calibrating and rig-model agnostic.

- **Smoothing first, threshold second.** A naive max-power threshold
  on the raw spectrum will be confused by individual signals. Smooth
  with a window wide enough (≥300 Hz) to average over signals but
  narrower than the rig passband shape.

- **Reject DC.** Always skip the first ~10 bins (below ~50 Hz) when
  computing `peak_power`. They contain DC and audio-card power-supply
  artifacts that have nothing to do with the rig's RF passband.

- **Sanity floor.** Always enforce `auto_high_hz - auto_low_hz >= 500 Hz`
  before applying the result. If the detected width is smaller, fall
  back to the operator's Wide Graph window. This protects against
  pathological spectra (a single dominant carrier, an empty band, a
  loud Wi-Fi splat across the audio chain).

- **Per-window recomputation.** Cheap. Do it every window. No caching,
  no rig-fingerprint matching. The mechanism's appeal is its
  self-correcting nature; do not undermine that by caching.

- **Default-on.** This is a sensitivity-preserving FP-reducing change.
  Default `enable_auto_passband: true` across all tiers.

- **Test fixture.** Three classes of synthetic spectra are sufficient:
  1. Flat-noise spectrum (no signals, no rolloff) — auto-passband
     should return the operator's Wide Graph window unchanged.
  2. Flat-noise spectrum with a sharp rolloff at 200 Hz and 2700 Hz —
     auto-passband should return approximately (200, 2700).
  3. Flat-noise spectrum with rolloff + one strong tone at 1500 Hz —
     auto-passband should still return approximately the rolloff edges
     (the tone is a small bump on top of the noise floor).
  Add a real-WAV test from the existing pancetta WAV corpus once the
  synthetic tests pass.

- **Logging.** At `debug!` level (`target: "dsp.passband"`) log
  `auto_low_hz`, `auto_high_hz`, the width, and whether the result was
  clamped by the sanity floor. This is the operator's debugging window
  if FPs persist.

- **Citation hygiene.** Cite as "inspired by WSJT-X Improved
  automatic passband baseline v3.1.0 (DG2YCB)" in the journal entry.
  The underlying technique (spectrum-shape-based passband detection)
  is general signal-processing; no specific WSJT-X-Improved code is
  referenced. Pancetta's implementation is independent.
