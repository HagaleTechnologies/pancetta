# Algorithm spec: DT refinement during multipass signal subtraction

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- Primary doc (traceability only, NOT quoted): `docs/wsjtr.md`
  §"DT Refinement in Signal Subtraction (Feb 2026)"
- Code paths cited for traceability only, NOT to be read by implementer:
  `crates/jt9r/src/subtract.rs` (`Ft8Subtractor::subtract`, the `refine_dt`
  parameter and `SubtractDtConfig` struct),
  `crates/wsjtr/src/main.rs` (caller passes `refine_dt=true` for between-pass
  external subtraction).
- Upstream lineage: this is wsjtr's port and extension of WSJT-X
  `subtractft8.f90`'s `lrefinedt` mechanism, with parabolic interpolation
  via `peakup.f90`. The original WSJT-X version tests 3 fixed time offsets;
  wsjtr generalizes to `2·steps + 1` configurable offsets.
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

In a multipass FT8 decoder, after decoding a candidate signal the decoder
*subtracts* the reconstructed waveform from the residual audio so that the
next pass can find weaker signals previously masked by the strong one.
Subtraction quality depends on three estimated parameters: frequency, time
offset (DT), and starting phase. If any of those three are off by even a
small amount, the subtracted waveform doesn't fully cancel the real signal
and the residual carries the leftover energy forward, masking nearby weaker
signals on subsequent passes.

The decoder's reported DT is quantized to whatever resolution the sync
stage produced — typically ±20 ms or so. After fine-sync that improves but
is still imperfect. The subtraction stage can do its own further refinement
locally: try the subtraction at a small grid of sub-sample DT offsets,
evaluate residual energy after each, and pick the DT that minimizes
residual. Parabolic interpolation across the three best-scoring offsets
gives sub-step precision.

WSJT-X mainline uses a 3-point version of this trick. wsjtr's
experimentation found the 3-point grid is too coarse — many subtractions
land at the ±90-sample edge of the search, indicating the true minimum is
outside the tested range or the step size is too large to resolve a clear
minimum. A 21-point grid at 5-sample spacing (~0.42 ms) catches the true
minimum cleanly and produces a measurable net improvement.

Per wsjtr's measurement on 85 high-activity windows across 22 captures:
+120 decodes gained, 40 lost relative to the WSJT-X-style 3-point baseline,
net **+80 / 4314 ≈ 1.9%** improvement. The "lost" decodes come from
subtractions that, with the finer grid, landed at a false-minimum offset
and corrupted nearby spectral energy.

This mechanism applies *between* external passes (where pancetta's
multipass loop produces a clean residual for the next pass to consume).
Within a single pass, where many candidates are being subtracted
concurrently, the WSJT-X convention is to NOT refine — the original
3-point coarse approach (or skipping refinement entirely) is what wsjtr's
internal `decoder.rs` and WSJT-X's `ft8b.f90` use within-pass.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- `audio`: the current residual audio buffer (12 kHz mono float32, length
  ~180000 samples for a 15-second window).
- `decode`: the decoded signal to subtract, containing at minimum
  - `dt_decoder` (seconds): the decoder-reported DT.
  - `freq_hz` (Hz): the decoded centre frequency.
  - `itone[79]` (the 79-symbol tone sequence) — sufficient to reconstruct
    the modulated waveform.
- `subtract_dt_offset` (seconds): a convention bridge between the decoder's
  DT reference frame and the subtractor's sample-time reference frame.
  Default 0.5; absolute starting sample is then
  `nstart = (dt_decoder + subtract_dt_offset) * 12000`.
- A configuration block with two parameters governing the refinement grid:
  - `range`: maximum offset from the centre, in samples. Default 90.
    Recommended 50.
  - `steps`: number of grid points on each side of the centre. Default 1
    (3-point grid). Recommended 10 (21-point grid).
- `verbose` (bool): optional logging.

### Outputs

- A modified `audio` buffer with the decoded signal subtracted at the
  refinement-chosen DT offset, **or** the original `audio` unmodified if
  the refinement step concluded no clear minimum exists (the principled
  "skip" path that avoids corrupting the residual).

### Steps

1. **Compute the centre starting sample.** `nstart =
   round((dt_decoder + subtract_dt_offset) * 12000)`. This is the time
   index at which the reconstructed waveform would begin if no
   refinement were applied — equivalent to WSJT-X's
   `lrefinedt=false` behaviour.

2. **Build the offset grid.** Generate `2·steps + 1` offsets evenly
   spaced from `-range` to `+range`, in increments of `range/steps`.
   With the recommended `range=50, steps=10`, the grid is
   `{-50, -45, -40, ..., 0, ..., +40, +45, +50}` — 21 offsets
   at 5-sample (~0.42 ms) spacing.

3. **Evaluate residual energy at each offset.** For each candidate
   offset `d` in the grid:
   a. Form a trial start sample `nstart_d = nstart + d`.
   b. Reconstruct the decoded signal's complex waveform using
      `itone`. This is the standard FT8 8-FSK reconstruction at
      `freq_hz`; pancetta already does this in its existing
      subtractor.
   c. Compute the trial residual: subtract the reconstructed waveform
      from `audio` starting at `nstart_d`, only in the sample range
      occupied by the reconstructed waveform (`nstart_d ..
      nstart_d + NFRAME` where `NFRAME` is the 79-symbol modulated
      length, approximately `79 * 1920 = 151680` samples plus a
      ramp-up / ramp-down margin).
   d. Compute a quality metric on the trial residual. The natural
      choice is the residual energy in the frequency band occupied
      by this signal (a narrow band around `freq_hz`, e.g., ±100 Hz).
      Lower energy = better cancellation = better DT. The actual
      computation can be either: (i) a direct sum of squared samples
      in the relevant audio range (cheaper, less precise — uses the
      whole audio bandwidth); or (ii) an FFT-based band-limited
      energy estimate (more precise, costs ~2 FFTs of the affected
      window per evaluation). WSJT-X and wsjtr use the band-limited
      FFT version; pancetta should match for accuracy.
   e. Record `(d, energy_d)`. Do NOT permanently mutate `audio`
      during evaluation — these are trial evaluations only.

4. **Find the coarse minimum.** Scan all `2·steps + 1` energies; pick
   the offset `d_min` with the smallest energy.

5. **Parabolic interpolation for sub-step precision.** Take the three
   energies at `d_min - 1·step_size`, `d_min`, `d_min + 1·step_size`
   (i.e., the minimum and its two grid-neighbours). Fit a parabola
   through these three (energy, offset) points and solve for the
   parabola's vertex offset `d_refined`. The standard parabolic
   interpolation formula:

   `dx = 0.5 * (y_left - y_right) / (y_left - 2·y_mid + y_right)`

   where `y_left, y_mid, y_right` are the three energies and `dx` is
   the fractional adjustment in units of `step_size`. The refined
   offset is `d_refined = d_min + dx · step_size`.

6. **Skip-condition guard.** If `|dx| > 1.0`, the parabolic fit is
   degenerate — typically because the minimum lies outside the grid
   (the parabola opens upward but the centre point is *not* lower
   than its neighbours), or because the three energies don't form a
   meaningful curvature. In this case, **skip the subtraction
   entirely**: leave `audio` unmodified. This is the principled
   defense against corrupting the residual with a bad subtraction.

7. **Edge guard.** If `d_min` is at the grid boundary (i.e.,
   `d_min == -range` or `d_min == +range`), the three-point fit can't
   be formed (there's no neighbour outside the grid). The same skip
   rule applies — the boundary location indicates the true minimum is
   outside the searched range, which makes the subtraction
   untrustworthy.

8. **Apply the subtraction at the refined offset.** Reconstruct the
   waveform one more time, this time at the *fractional* sample offset
   `nstart + d_refined`. Fractional-sample alignment can be done by
   either (i) phase-shifting the IFFT output (cheap, but requires the
   reconstruction to be in the frequency domain), or (ii)
   resampling-with-fractional-delay via an interpolation filter
   (more general, more expensive). Either is fine — wsjtr uses the
   frequency-domain phase-shift trick because its subtractor already
   operates in the frequency domain.

9. **Subtraction gating (orthogonal to refinement).** A subtraction is
   only applied if the reconstructed waveform fits entirely within
   the recorded audio extent: `nstart_d + NFRAME ≤ samples_recorded`.
   This guard exists regardless of refinement and is important for
   live mode's early-pass shorter audio.

10. **Verbose logging (optional).** When enabled, log the chosen
    `d_refined`, the parabolic `dx`, and whether the subtraction was
    applied or skipped. This is invaluable during eval — it lets a
    journal entry verify that the refinement grid is sized correctly
    (most chosen offsets should be interior, not at the edges).

### Numerical constants (facts, not expression)

- Sample rate: 12 kHz.
- Subtractor DT reference offset: `subtract_dt_offset = 0.5` seconds
  (decoder reports DT relative to ~0.5 s into the 15-second slot).
- Recommended grid: `range = 50` samples (±4.2 ms), `steps = 10` (21
  total offsets), step size `range/steps = 5` samples (~0.42 ms).
- WSJT-X-style fallback grid: `range = 90` samples (±7.5 ms),
  `steps = 1` (3 total offsets at -90, 0, +90), step size 90 samples
  (~7.5 ms). The wsjtr measurements show this is too coarse.
- Skip threshold on parabolic fit: `|dx| > 1.0` (i.e., the
  interpolated minimum lies outside the central three-point
  bracket).
- Reconstructed-waveform length `NFRAME`: 79 symbols × 1920 samples
  per symbol = 151680 audio samples (~12.64 s), plus a short ramp.
- Window length: 15 s × 12000 = 180000 samples nominal.

### Edge cases

- **Skip with `|dx| > 1`.** As above. Better to skip than corrupt the
  residual.
- **Skip at grid boundary.** Same.
- **Subtraction extends past audio extent.** Standard subtraction
  gating: if `nstart + d_refined + NFRAME > samples_recorded`, skip
  (or, in WSJT-X mainline, truncate to in-window samples and accept
  partial cancellation — both choices are reasonable; wsjtr skips
  outright).
- **Zero-length or NaN energy.** A degenerate residual (e.g., all
  zeros, or a NaN from upstream) produces nonsense energies. Treat
  any non-finite energy as +∞ (worst possible offset), so the grid
  search naturally rejects it.
- **Very weak signal with low SNR.** wsjtr offers a configurable
  `subtract_min_snr` threshold: only refine + subtract decodes with
  SNR ≥ threshold. In practice the default (subtract everything)
  works because even moderately confident decodes produce a
  reasonable subtraction. Pancetta should plumb the same opt-in
  threshold but default it off.
- **Multipass interaction.** Refinement is applied *between* external
  passes (i.e., where the residual will be re-decoded). Within a
  single pass — where many candidates are being decoded and
  subtracted concurrently — the WSJT-X convention and wsjtr's
  internal `decoder.rs` both run subtraction *without* refinement
  (or with the cheap 3-point version). Pancetta should follow the
  same convention.
- **Wider ranges produce more false minima.** wsjtr's measurement
  found that `range=200, steps=10` gains +94 raw decodes but loses
  58 (net +36), versus `range=50, steps=10`'s net +80. The
  false-minimum rate increases with range faster than the
  true-minimum recovery rate. Do not tune wider than 50 without
  re-measuring.
- **Step size dominates over range.** wsjtr found that step size
  matters more than range: any configuration with step=5 samples
  outperforms any configuration with step≥10 at the same range.
  Step=5 (10 steps × range 50) is the cleanest choice.

### Measurement reference (from wsjtr's published table)

The following are wsjtr's published net-decode improvements relative
to the 3-point baseline (`range=90, steps=1`), measured on 85
high-activity windows across 22 captures, settings `-p 3 -d 3 -m 200
-c 2000 --no-early-exit`. These are facts to be cited, not derivations:

- `range=40, steps=8` (step 5 samples, 17 points): +109 gained, 52 lost, **net +57**.
- `range=45, steps=9` (step 5, 19 points): +114 gained, 41 lost, **net +73**.
- `range=50, steps=10` (step 5, 21 points): +120 gained, 40 lost, **net +80**. *Recommended config.*
- `range=60, steps=6` (step 10, 13 points): +107 gained, 36 lost, **net +71**.
- `range=60, steps=12` (step 5, 25 points): +107 gained, 33 lost, **net +74**.
- `range=90, steps=9` (step 10, 19 points): +78 gained, 18 lost, **net +60**.
- `range=200, steps=10` (step 20, 21 points): +94 gained, 58 lost, **net +36**.

The recommended setting is the `(50, 10)` row: largest net improvement,
21 evaluations per signal, ~5-sample step.

## Conflict with pancetta's existing mechanisms

Pancetta's current subtractor in `pancetta-ft8` lives in the FT8
modulator-then-subtract path used by the multipass decoder. Today it
subtracts at the decoder-reported DT with no refinement step. This
mechanism is purely additive at the subtractor level: it does not
change the decoder's reported DT, only how that DT is used when
applying the subtraction.

Possible conflicts:

1. **Cost budget.** Each refinement evaluation is approximately two
   FFTs over the signal's local window (one to project the
   reconstructed waveform into the frequency domain, one to assess
   band-limited residual energy after subtraction). With 21 evaluations
   per signal and 30-50 decoded signals per window, that's
   ~630-1050 FFT pairs per window, each on a window of ~152 k complex
   samples. wsjtr measures ~60-90 ms total per-window overhead on
   "modern multi-core CPU" with their default (3 evaluations) and
   400-600 ms with 21 evaluations. On the Slow tier this is the
   difference between trivial and one-third of a slot's wall clock.
   Recommend: gate by hb-216 tier — Fast/Moderate use 21-point grid,
   Slow uses 3-point or disables refinement entirely. Add an
   `enable_subtractor_dt_refinement: bool` flag to `Ft8Config`.

2. **Interaction with hb-091 scoped fast path.** The hb-091 work
   already shaves wall-clock in the decoder hot loop on Moderate/Slow
   tiers. DT refinement adds work specifically in the *subtractor*
   between passes, not in the decoder. The two work in opposite phases
   of the multipass loop and don't conflict directly, but their cost
   budgets share the per-window deadline. The implementer should
   measure combined wall-clock before flipping the refinement
   default on for any tier.

3. **Interaction with multipass / `max_decode_passes`.** Refinement
   only matters when there are MORE than one pass — its whole job is
   to produce a cleaner residual for the *next* pass. With
   `max_decode_passes=1` (Slow tier under hb-216) refinement is
   pointless and should be force-disabled regardless of the
   `enable_subtractor_dt_refinement` flag.

4. **Interaction with the 5×5 grid refinement (wsjtr-grid-refinement
   spec).** Both mechanisms refine `(dt, freq)` estimates, but at
   different stages of the pipeline. The 5×5 grid refinement happens
   at the *candidate* stage (improving the decoder's input estimate
   for soft-bit demod). DT-refinement-during-subtract happens at the
   *subtraction* stage (improving the next pass's residual). They are
   complementary, not redundant. Cleaner candidate-stage DT means the
   decoder reports a better DT; the subtractor still benefits from
   one final ±5-sample sweep at subtraction time because the
   decoder's DT is still quantized to the symbol grid.

5. **Interaction with hb-115 / hb-100 / hb-218 capture-effect work.**
   Mildly positive. A cleaner subtraction of the strong member of a
   capture-locked pair leaves a cleaner residual for the weak member
   to be decoded in a subsequent pass (which is exactly what
   capture-effect joint-decode work is about).

6. **Interaction with FP filtering.** Mildly positive but indirect.
   Cleaner residuals reduce the prevalence of "ghost" candidates
   (spectral artifacts from imperfect subtraction). Fewer ghost
   candidates → fewer ghost decodes → less work for hb-052/058/103.

## Estimated Rust port effort

- Code additions to pancetta-ft8's subtractor:
  - Refinement-grid generator + per-offset energy evaluator: ~60 LOC.
  - Parabolic interpolation helper: ~25 LOC.
  - Skip-condition guards: ~15 LOC.
  - Fractional-sample subtraction (phase-shift or fractional-delay
    interp): ~40 LOC, depending on which technique pancetta's
    subtractor already supports.
  - Config plumbing (`SubtractDtConfig` struct in `Ft8Config` plus
    tier-driven default): ~25 LOC.
- Unit tests:
  - Parabolic interpolation correctness against analytical parabolas:
    ~30 LOC.
  - End-to-end synthetic test (inject a signal at known DT, confirm
    refinement lands on the correct sub-sample offset): ~80 LOC.
  - Skip-condition test (degenerate parabola, edge boundary): ~40 LOC.
- Total: ~175 LOC production + ~150 LOC tests.
- 1-2 iter sessions (one for implementation + unit tests; one for
  hard-200 eval and tier-default tuning).

## Implementation notes for the implementer thread

- **Splice point.** The existing pancetta subtractor — search
  `pancetta-ft8/src/` for `Subtractor` or `subtract_ft8` — already has
  a "subtract this decode from this audio" entry point. Add a new
  optional `refinement: Option<SubtractDtConfig>` parameter. When `Some`,
  run the 21-point grid + parabolic interpolation; when `None`, behave
  exactly as today. Default `None` for the within-pass internal
  multipass; `Some(SubtractDtConfig::default())` for the between-pass
  external loop.

- **Per-evaluation cost shape.** The 21-point grid is the cost-driver.
  Use the existing frequency-domain reconstruction; do NOT
  re-modulate the waveform 21 times from scratch. Precompute the
  reconstructed waveform's complex spectrum once, then for each
  trial offset `d` apply a phase ramp `exp(-j·2π·k·d/N)` to the
  spectrum and IFFT/subtract. The phase-ramp version is much cheaper
  than re-modulating.

- **Energy metric.** Use the band-limited version (sum of squared
  spectrum magnitudes within ±100 Hz of `freq_hz`), not the
  whole-audio sum-of-squares. The latter is dominated by other
  signals' energy and gives a noisy minimum.

- **Test fixture.** Inject a synthetic FT8 burst at a known
  off-quantization DT (e.g., the burst's true onset at sample 1003
  while the decoder would report sample 960 = `dt=80*40ms`).
  Confirm that the 21-point refinement chooses an offset near
  `+43` samples and the residual energy after subtraction is at
  least 20 dB lower than the WSJT-X-style 3-point baseline. This
  test is the unit-level proof of mechanism.

- **End-to-end measurement.** Re-run the corpus-wide hard-200 (or a
  subset thereof) with `enable_subtractor_dt_refinement` toggled on
  and off. Expect a +1.5 to +2.5% net-decode improvement; if the
  measurement comes in materially below +1%, something is wrong with
  the implementation (likely the energy metric or the skip-condition
  guard).

- **Bootstrap-CI gate.** Pancetta's policy is bootstrap-CI gating for
  small-delta graduations. A 1.9% net improvement on a few thousand
  decodes is borderline — definitely run the bootstrap before
  graduating. The expected CI lower bound should clear zero with
  good margin, but cite the bootstrap result in the journal entry.

- **Tier default policy.** Enable by default on Fast (full 21-point
  grid). Enable on Moderate (full 21-point grid, but watch wall-clock
  metrics). On Slow, leave disabled or use the WSJT-X 3-point grid
  (which is materially cheaper but already gains some of the
  benefit).

- **Citation hygiene.** Cite as `wsjtr-inspired (subtractor DT
  refinement, 21-point grid)`. The underlying technique
  (parabolic interpolation over an energy-vs-offset grid) is
  standard signal processing; cite "WSJT-X subtractft8 + peakup
  (Franke/Taylor)" if a primary academic-level source is wanted.
  Pancetta's contribution beyond wsjtr is the bootstrap-CI gate,
  not the algorithm itself.
