# Algorithm spec: WSJT-X mainline subtractft8 — successful-decode signal removal

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8/subtractft8.f90` (~106 LOC)
- Companion: `lib/ft8/gen_ft8wave.f90` (reference-waveform generator).
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

When a signal has been successfully decoded, `subtractft8` removes it
from the audio buffer so subsequent decode passes can find weaker
signals that may have been masked by the strong one. This is the
mechanism that lets WSJT-X iterate "decode strongest → subtract → decode
next strongest → …" and pull tens of signals out of a single band slice.

Two modes:
1. **Plain subtract** (`lrefinedt = .false.`): use the supplied `dt` as
   the reference-waveform start time.
2. **dt-refined subtract** (`lrefinedt = .true.`): test three start
   offsets (`-90, 0, +90` audio samples relative to `dt`), fit a
   parabola through the three resulting post-subtract residual powers,
   find the minimum, and use that as the refined offset.

## Inputs

- `dd0` — audio buffer (length 180,000 samples @ 12 kHz). **MUTATED**
  in place — the subtracted signal is removed.
- `itone(79)` — the 79 channel-symbol tone indices of the decoded
  signal.
- `f0` — the decoded signal's frequency in Hz.
- `dt` — the decoded signal's time offset in seconds (relative to
  slot start at 0.5 s).
- `lrefinedt` — enable parabola-fit dt-refinement.

## Numerical constants

- `NMAX = 180000` — buffer length.
- `NFRAME = 1920 * 79 = 151680` — full FT8 frame length in samples.
- `NFFT = NMAX = 180000` — main FFT length.
- `NFILT = 4000` — LPF window length (controls the bandpass filter
  used during reconstruction).
- LPF taper: `window(j) = cos(π * j / NFILT)^2` for `j ∈ [-NFILT/2,
  NFILT/2]` (a Hann-squared-cosine window, length 4001 samples).
- End-correction: for `j ∈ [1, NFILT/2+1]`, scale the first and last
  `NFILT/2+1` samples of the filtered output by `1 / (1 - sum_tail /
  sumw)` where `sum_tail = sum(window(j-1:NFILT/2))`. This compensates
  for the LPF's energy loss at the edges where the filter doesn't have
  full overlap with the signal.
- Parabola-fit offsets: `idt ∈ {-90, 0, +90}` audio samples. Note:
  90 samples / 12000 sps = 7.5 ms, equivalent to ~3/8 of a symbol-
  step (`NSTEP = 480` samples) — wide enough to find the minimum,
  narrow enough to fit a parabola accurately.
- Parabola-fit reject: if `|dx| > 1.0` (the parabola minimum is more
  than one search-step from the central offset), **don't subtract** —
  the fit is unreliable.
- Frequency range for residual power computation (refinement mode):
  `[f0 - 1.5*6.25, f0 + 8.5*6.25]` Hz — same asymmetric guard as
  `ft8_downsample`.

## Algorithm description (prose only)

### Step 1: build the LPF (first call only)

A Hann-squared-cosine window is constructed of length `NFILT + 1 = 4001`
samples, normalized to unit sum. This becomes the bandpass filter
applied to the reconstructed reference signal.

The window is placed into the first 4001 samples of an `NFFT`-length
complex array `cw`, then circular-shifted by `NFILT/2 + 1` samples (to
center it at zero), then FFT'd. The result `cw` is the frequency-domain
filter coefficients, ready for spectral multiplication.

The end-correction array is computed once: for `j ∈ [1, NFILT/2+1]`,
`endcorrection(j) = 1 / (1 - sum(window(j-1:NFILT/2)) / sumw)`. The
first sample's correction is `1 / (1 - sum(window(0:NFILT/2)) / sumw)
= 1 / (1 - 0.5) = 2` (since the window is symmetric around zero).
The correction decreases toward 1 as `j` increases.

### Step 2: synthesize the reference waveform

Call `gen_ft8wave(itone, 79, 1920, 2.0, 12000.0, f0, cref, _, 1, NFRAME)`
to produce a complex reference waveform `cref` of length `NFRAME =
151680` samples. This is the *complex baseband-equivalent* of the
transmitted FT8 signal at frequency `f0`: `cref(t) = exp(j * (2π * f0 *
t + phi(t)))` where `phi(t)` is the 8-FSK tone modulation per `itone`.

The `2.0` argument is the GFSK bandwidth parameter (controls the
Gaussian shaping of the symbol transitions); `12000.0` is the sample
rate; `1920` is samples per symbol.

### Step 3: matched filter via internal `sqf` function

The internal function `sqf(idt)` does the actual subtraction work. It's
designed to be called multiple times with different `idt` offsets to
support parabola fitting.

For a given `idt`:

**(a) Mix the audio with the conjugate reference:**
- `nstart = dt * 12000 + 1 + idt` — the sample index in `dd0` where the
  reference is expected to start.
- For `i = 1..NFRAME`: `camp(i) = dd(nstart - 1 + i) * conj(cref(i))`.
  (Mixing the real audio with the complex-conjugate reference gives a
  complex baseband signal `camp` that contains the slowly-varying
  amplitude and phase of the signal we want to subtract.)

**(b) Apply the LPF:**
- Zero-pad `camp` from `NFRAME+1` to `NFFT`.
- FFT to frequency domain.
- Multiply elementwise by `cw` (the FFT'd LPF coefficients).
- Inverse FFT back to time domain.
- Apply end-correction to first `NFILT/2+1` samples and last
  `NFILT/2+1` samples (samples `NFRAME-NFILT/2:NFRAME` reversed).

The result `cfilt(1:NFRAME)` is the slowly-varying complex amplitude
of the signal — i.e., `cfilt(t) ≈ A(t) * exp(j*phi_residual(t))` where
`phi_residual` captures the phase error in the original frequency
estimate `f0`.

**(c) Reconstruct and subtract:**
- For `i = 1..NFRAME`: `z = cfilt(i) * cref(i)` (multiply the
  recovered amplitude by the synthesized reference to rebuild the
  real signal as it appeared in the audio).
- `dd(nstart - 1 + i) -= 2 * real(z)` (subtract the reconstructed
  signal from the audio buffer; the `2*` accounts for the fact that
  the real signal had power equally split between positive and
  negative frequencies in the complex domain).

**(d) Compute residual power (only if `ldt`):**
- FFT the modified `dd` buffer.
- Sum power in the FT8 band: `sqq = sum(|cx(i)|^2)` for `i` in the
  asymmetric `[f0 - 1.5*6.25, f0 + 8.5*6.25]` Hz range.
- Return `sqq` as the residual power.

If `ldt = .false.` (no dt-refinement), `sqq = 0` (don't compute it,
saves an FFT).

### Step 4: dt-refinement (when `lrefinedt = .true.`)

The outer body of `subtractft8` does:

1. `sqa = sqf(-90)` — residual power if we subtract with `dt - 7.5 ms`.
2. `sqb = sqf(+90)` — residual power if we subtract with `dt + 7.5 ms`.
3. `sq0 = sqf(0)` — residual power at the nominal `dt`.
4. `peakup(sqa, sq0, sqb, dx)` — fit a parabola through the three
   `(offset, sqq)` points and return the offset of the minimum as `dx`
   (normalized to step size — i.e., `dx ∈ [-1, +1]` ideally).
5. If `|dx| > 1.0`: the parabola minimum is outside the search range
   → the fit is unreliable → **do not subtract**, return with `dd0`
   unchanged.
6. Otherwise: `i2 = nint(90.0 * dx)` — best estimated offset in audio
   samples.
7. `sqf(i2)` — final subtraction at the refined offset.

The mechanism is: `sqf` mutates `dd` (the internal copy) each time it's
called. Each call **first** resets `dd = dd0` (the input snapshot),
**then** subtracts at the requested offset. So calling `sqf(-90)`,
`sqf(+90)`, `sqf(0)`, `sqf(i2)` in sequence is equivalent to "try four
different subtract offsets and keep only the final one".

### Step 5: commit the subtraction

`dd0 = dd` — write the final subtracted buffer back to the caller's
input. This is the only output: the audio buffer with one signal
removed.

## What wsjtr's / ft8mon's docs paraphrase or miss

1. **Frequency-domain LPF, not time-domain.** Mainline does the LPF
   as FFT → multiply → IFFT, *not* as time-domain convolution. This is
   ~100x faster for `NFILT = 4000` and matters when subtracting many
   signals per slot.
2. **End-correction for the first and last `NFILT/2+1` samples.** The
   LPF loses energy at the buffer edges because the window doesn't have
   full overlap; the end-correction scales those samples up to
   compensate. Without this, the edges of `cfilt` have artificially
   low amplitude, and the subtraction undershoots at the start and end
   of the signal.
3. **Parabola-fit dt-refinement uses `±90 sample` offsets**, not the
   smaller offsets used during sync search. The 90-sample offset is
   wide enough to capture residual-power curvature even when the
   initial `dt` estimate is off by a few ms.
4. **The `|dx| > 1.0` reject is a parabola-quality gate** — if the
   parabola opens upward but the minimum is outside the three sample
   points, the curve isn't well-fit. Refusing to subtract avoids
   corrupting the audio with a bad subtraction.
5. **`2 * real(cfilt * cref)` is the actual subtractor**, not just
   `real(cfilt * cref)`. The factor of 2 accounts for negative-
   frequency mirror.
6. **The mixing step uses `conj(cref)`, not `cref`.** Audio is real,
   so demodulation requires the conjugate.
7. **The reference waveform parameters are `(2.0, 12000.0, 1920)`** —
   GFSK BT product 2.0, sample rate 12 kHz, 1920 samples per symbol.
   `gen_ft8wave` applies Gaussian filtering to the tone transitions
   per this BT — getting the BT wrong by even 0.5 leaves visible
   residuals after subtraction.
8. **The buffer mutation pattern (`dd = dd0` at the start of each
   `sqf` call) is non-obvious.** It means the parabola-fit phase is
   "trial subtractions" — each call computes the residual *as if* this
   offset were the chosen one, without committing. Only the final
   `sqf(i2)` call's result is kept.
9. **The frequency range for residual-power computation uses the same
   asymmetric guard as `ft8_downsample`** (`-1.5 baud` to `+8.5 baud`).
   This isn't accidental — both modules are looking at the same band
   where the 8 tones live.
10. **A debug `write` statement** at line 61-62 (commented out) reveals
    that the residual energy of the subtracted band is a useful health
    metric: `1e-8 * sum(dd*dd)` after subtraction should be roughly
    the local noise floor. Pancetta could surface this metric.

## Conflict with pancetta's existing mechanisms

- Pancetta has signal subtraction in the decoder, but verify it uses
  frequency-domain LPF (the spectral multiplication approach) and not
  time-domain FIR. The latter is correct but ~100x slower.
- End-correction is easy to miss. If pancetta's subtractor leaves
  visible residuals at the start/end of subtracted signals, this is
  likely the cause.
- Parabola-fit dt-refinement is the "+0.5-1.0 dB" trick — it improves
  subtraction accuracy without requiring more iterations. Whether
  pancetta has this depends on the decoder pipeline.
- The `±90 sample` offset is wider than the fine-sync `±10 sample`
  offset used in `ft8b`. Don't confuse them — they're solving
  different problems (subtract dt vs decode dt).
- The `|dx| > 1.0` reject is a safety gate that prevents corrupting
  the audio when the dt-fit is unreliable. Skipping it sometimes
  corrupts marginal decodes' contributions.

## Estimated Rust port effort

- LPF construction + caching: ~50 LOC.
- End-correction: ~20 LOC.
- `sqf` internal function: ~80 LOC.
- Parabola-fit dt-refinement: ~30 LOC.
- Total: ~200 LOC, mostly leveraging existing FFT infrastructure.
- Sessions: 1.

## Implementation notes for the implementer thread

- The LPF should be cached as a `Vec<Complex<f32>>` of length NFFT
  (180,000). Computed once; reused for every subtract call.
- The end-correction is a `Vec<f32>` of length `NFILT/2 + 1 = 2001`,
  applied symmetrically at both ends.
- `gen_ft8wave` is its own file (~50 LOC); the GFSK Gaussian shaping
  needs to be ported separately. The BT product 2.0 and the
  per-symbol Gaussian convolution.
- The `peakup` helper is a standard 3-point parabola fit:
  `dx = 0.5 * (sqa - sqb) / (sqa - 2*sq0 + sqb)`. Two divisions, no
  fancy math.
- Test fixture: a clean synthetic WAV with two FT8 signals at
  different frequencies. After decoding signal A and subtracting, the
  residual should be only signal B (verify with FFT — signal A's
  tone energy should drop by >20 dB).
- Verify the asymmetric guard band — symmetric or wrong-sign guards
  will leave tone energy unsubtracted.
