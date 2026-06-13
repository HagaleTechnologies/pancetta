# Algorithm spec: WSJT-X mainline baseline + get_spectrum_baseline — per-bin noise-floor estimator

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- Files (traceability only; NOT quoted):
  - `lib/ft8/baseline.f90` (~49 LOC) — polynomial fit
  - `lib/ft8/get_spectrum_baseline.f90` (~54 LOC) — spectrum builder
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

`get_spectrum_baseline` builds an average power spectrum from the full
15-second audio buffer and feeds it to `baseline` to fit a smooth
"noise floor" curve. The output `sbase(bin)` is the *expected noise
power per frequency bin* across the band. This is used by `ft8b` to:

- Convert from instantaneous power to SNR via `xbase = 10^(0.1 *
  (sbase(bin) - 40))` (linearize the dB-scale baseline).
- Provide a frequency-dependent noise reference for SNR estimation
  (more accurate than a global noise floor when the band has uneven
  noise — e.g., a strong birdie at one frequency, quiet noise at
  others).

The fit is a 5th-degree polynomial against the *lower envelope* of the
log-power spectrum, segment-by-segment.

## Inputs

`get_spectrum_baseline(dd, nfa, nfb, sbase)`:
- `dd` — full 15-second audio buffer (180,000 samples).
- `nfa`, `nfb` — frequency search bounds in Hz (passed through; may
  be clamped).
- Output: `sbase(NH1)` — array of `NH1 = 1920` baseline values, one
  per `3.125 Hz` frequency bin.

`baseline(s, nfa, nfb, sbase)`:
- `s` — array of `NH1` power values (linear scale).
- `nfa`, `nfb` — frequency bounds in Hz.
- Output: `sbase(NH1)` — fitted baseline (dB scale + 0.65 offset).

## Numerical constants

- `NFFT1 = 3840` — FFT length. Bin width: `12000 / 3840 = 3.125 Hz`.
- `NST = NFFT1/2 = 1920` — step size for the long FFT (so consecutive
  FFTs overlap by 50%).
- `NF = 93` — number of FFT slices that fit in the 15-s buffer:
  `NMAX/NST - 1 = 180000/1920 - 1 = 93.75 → 93`.
- Nuttall window normalization: `window * NSPS * 2 / 300`.
- `nseg = 10` — number of frequency segments for the lower-envelope
  fit.
- `npct = 10` — percentile (10th percentile per segment becomes the
  "envelope").
- `nterms = 5` — polynomial order. The polynomial is 5th degree:
  `a(1) + t * (a(2) + t * (a(3) + t * (a(4) + t * a(5))))`.
- `+0.65` — DC offset added to the polynomial output. This biases the
  fitted baseline slightly above the actual 10th-percentile fit,
  presumably tuned for the downstream SNR formula in `ft8b`.
- Frequency clamping (in `get_spectrum_baseline`):
  - If `nfa < 100`: forced to `100`. If the window width is < 100,
    `nfb = nfa + window` (preserve narrow window for "decode again"
    operator request).
  - If `nfb > 4910`: forced to `4910`. Same window-preservation logic.

## Algorithm description (prose only)

### `get_spectrum_baseline` — build the average spectrum

1. **Compute the Nuttall window** (first call only). This is a
   4-term Nuttall window applied to NFFT1 samples, normalized so that
   `sum(window) = NSPS * 2 / 300` (the magic normalization factor
   matches the `1/300` scaling in `sync8`).
2. **Walk through the buffer in steps of `NST` samples** (overlapping
   by 50%). For each slice of length `NFFT1`:
   - Multiply by the window.
   - Real-to-complex FFT.
   - Take magnitude-squared of the first `NH1 = 1920` bins.
   - Add to the running sum `savg`.
3. **Clamp `nfa`/`nfb`** to `[100, 4910]` Hz (the FT8 baseband range).
   The clamping preserves window width if the operator requested a
   narrow re-decode (`nagain` mode).
4. **Call `baseline(savg, nfa, nfb, sbase)`** to fit the polynomial.

The output `savg` is the *sum* (not average — saves a division) of
93 windowed-FFT magnitude-squared spectra. Since `baseline` operates
on logarithms anyway, the absolute scale doesn't matter (it just
shifts the dB curve).

### `baseline` — fit polynomial to lower envelope

1. **Convert to dB**: for `i = nfa_bin..nfb_bin`, `s(i) = 10*log10(s(i))`.
   The bins outside `[nfa, nfb]` are unchanged (left in linear scale
   but not used).
2. **Slice the band into `nseg = 10` equal-width segments.** For each
   segment:
   - Find the 10th-percentile dB value via `pctile`.
   - For every bin in the segment whose dB value is at or below the
     10th percentile, save the `(bin_offset_from_midpoint, dB_value)`
     pair as a "lower envelope" sample.
   - Save up to `1000` total samples across all segments.
3. **Fit a 5th-degree polynomial** to these envelope samples via
   `polyfit`. The polynomial coefficients are `a(1..5)`. Note the
   x-coordinate is `i - i0` where `i0 = (nfb_bin - nfa_bin + 1) / 2`
   is the midpoint — i.e., the polynomial is centered on the middle
   of the search band for numerical stability.
4. **Evaluate the polynomial** at every bin `i ∈ [nfa_bin, nfb_bin]`:
   `sbase(i) = a(1) + t * (a(2) + t * (a(3) + t * (a(4) + t * a(5))))
   + 0.65` where `t = i - i0`. The `+0.65` is the constant offset.

The output is in dB scale.

## What wsjtr's / ft8mon's docs paraphrase or miss

1. **10-segment 10th-percentile lower-envelope is the noise-floor
   estimate**, not the median or trimmed mean. The 10th percentile is
   below most signals (which sit above the noise floor in active
   bands) but above the absolute quietest bins (which may be
   instrumentation artifacts). The 10-segment slicing makes the fit
   adapt to frequency-dependent noise floor.
2. **5th-degree polynomial** — high enough to capture broad shape
   (band-edge rolloff, regional noise humps) but low enough to not
   chase individual birdies. Higher orders would fit signals into the
   "noise" curve and underestimate SNR.
3. **`+0.65 dB` offset** is the calibration tweak that makes
   `xbase = 10^(0.1 * (sbase - 40))` and downstream SNR formulas in
   `ft8b` produce reportable SNR values consistent with WSJT-X
   convention. This is a *tuned* constant; don't try to derive it.
4. **`-40 dB` offset in the linearization** (in `ft8b`'s consumer:
   `xbase = 10^(0.1 * (sbase - 40))`) means the dB baseline is being
   converted to a power scale where signals at the noise floor have
   `xbase ≈ 1e-4`. Then `xsig / xbase / 3e6` gives the SNR
   relative-to-noise ratio. The `3e6` is the integration factor
   (1920 samples per symbol × 79 symbols × some normalization).
5. **The Nuttall window** is used here (4-term Nuttall has very low
   sidelobes, ~-93 dB), not a Hann or Hamming window. This matters
   for noise-floor estimation: high-sidelobe windows would spread
   strong signal energy into adjacent bins and contaminate the
   noise-floor estimate.
6. **The window normalization `NSPS * 2 / 300`** has the `1/300`
   factor that also appears in `sync8`'s scaling. These are
   intentionally matched so the dB scales are comparable across
   modules.
7. **`NF = 93` FFT slices** — not 94, not 100. The number is derived
   from `NMAX / NST - 1 = 93.75`, truncated. So one slice is
   "lost" to the truncation; doesn't matter for noise estimation.
8. **The frequency clamp `[100, 4910]` Hz** is hard-coded in
   `get_spectrum_baseline`. Lower than 100 Hz: subaudible content,
   not usable for FT8. Higher than 4910 Hz: above the upper edge of
   typical SSB receiver passband. Operator can't change these.

## Conflict with pancetta's existing mechanisms

- Pancetta uses a noise-floor estimate for SNR calibration. Verify
  the choice of percentile (10th vs median) and segment count
  (10 vs other) matches mainline.
- The Nuttall window is important; using Hann or rectangular
  windows here causes noise-floor mis-estimation in the presence of
  strong birdies/carriers.
- The `+0.65 dB` bias and the `-40 dB` linearization offset are
  coupled to each other and to `ft8b`'s SNR formula. Changing one
  requires re-tuning the others.

## Estimated Rust port effort

- Nuttall window builder: ~20 LOC.
- Sliding FFT + windowed-power accumulator: ~50 LOC.
- 10-segment 10th-percentile sampler: ~50 LOC.
- 5th-degree polyfit (or use a library): ~50 LOC.
- Total: ~200 LOC; mostly leverages existing FFT/window/percentile
  infrastructure.
- Sessions: 1.

## Implementation notes for the implementer thread

- The Nuttall window coefficients are standard: `a0 = 0.355768, a1 =
  0.487396, a2 = 0.144232, a3 = 0.012604`. The window has 4 cosine
  terms.
- The polynomial fit can use the `nalgebra` or `polyfit-rs` crates,
  or just solve the normal equations manually with `nalgebra`'s
  `Matrix5x5`.
- The 10th-percentile per segment can be a quickselect on the
  segment's 192 bins (`NH1/nseg = 192`); no need to fully sort.
- Don't forget the `+0.65` offset — it's load-bearing for the
  downstream SNR formula.
- Test fixture: a synthetic WAV with a uniform Gaussian noise floor
  plus a couple of strong sinusoidal birdies. The fitted baseline
  should track the noise floor and ignore the birdies.
