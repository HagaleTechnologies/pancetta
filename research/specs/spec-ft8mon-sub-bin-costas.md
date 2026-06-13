# Algorithm spec: Sub-bin Costas sync via cached global FFT + bin rotation

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` approximately lines 480-628 (entry: the top-level `go()` method on the FT8 class; inner scoring kernel: `coarse()` near line 378 and `one_coarse_strength()`)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Pancetta's coarse sync currently quantizes candidate signals to FFT-bin
resolution (6.25 Hz at 12 kHz × 1920-sample symbols). Signals whose true
center sits halfway between two bins lose roughly half their energy to
spectral scalloping, and the analogous problem applies in the time
dimension when the true symbol boundary falls between sample-aligned
hop positions. The hb-217 diagnostic batch identified the
1000–2000 Hz band-middle as the worst recall region (55–57%), which is
exactly the geometry that scalloping predicts. This algorithm performs
the coarse Costas sync at 4× resolution in both axes (16 sub-bin
positions per FFT bin) without paying for 16 fresh full-buffer FFTs.

## Algorithm description (PROSE ONLY)

### Inputs
- The full slot's audio samples (typically the 15-second FT8 window after
  decimation to the working sample rate). Length is chosen to be one of
  a list of FFTW-friendly sizes (e.g. 18000, 36000, 54000, 72000, 144000,
  216000 samples).
- A frequency search range `[min_hz, max_hz]` (typically 0–3000 Hz).
- A time search range expressed in symbol indices `[si0, si1]`
  corresponding to roughly `-2.2 s` to `+2.4 s` around the nominal slot
  start (the `tminus` / `tplus` window).
- Two oversampling factors:
  - `coarse_hz_n` (default 4) — number of frequency sub-bin steps per
    natural FFT bin.
  - `coarse_off_n` (default 4) — number of time sub-bin steps per symbol
    period.

### Outputs
A list of candidates, each carrying:
- Estimated audio frequency in Hz (resolved to sub-bin granularity)
- Estimated time offset in samples (resolved to sub-symbol granularity)
- A Costas-sync strength score (higher is better)

The list is sorted by strength so the strongest candidates can be fed
into fine-sync + LDPC decode first.

### Steps

1. **Compute one large FFT over the entire slot buffer.** This is the
   only full-length FFT performed during the sub-bin sweep. The result
   is a vector of complex frequency bins covering the band. The bin
   spacing equals `sample_rate / buffer_length`.

2. **Outer loop over frequency sub-step index** `hz_frac_i` from 0 to
   `coarse_hz_n - 1`. The fractional Hz shift for this iteration is
   `hz_frac_i × (bin_hz / coarse_hz_n)`, where `bin_hz` is the symbol-FFT
   bin spacing (6.25 Hz at 12 kHz × 1920-sample symbols). Picking
   `coarse_hz_n = 4` gives shifts of `{0, 1.5625, 3.125, 4.6875}` Hz.

3. **Apply the bin-rotation trick to realize the sub-bin frequency
   shift.** Rather than resample and re-FFT, the algorithm uses a
   frequency-domain shift on the cached global FFT:
   - Pick an integer "down" shift count such that the cached bins are
     rotated to put the desired sub-bin offset at bin center.
   - Build a rotated copy of the global FFT bins where each entry of the
     new array equals the cached array at the rotated index (with the
     appropriate phase ramp applied to realize the fractional shift).
   - Inverse-FFT the rotated bins to produce a time-domain signal that
     has been frequency-shifted by the desired fractional amount.
   This costs one inverse FFT per `hz_frac_i`, not one forward FFT per
   sub-bin × sub-offset combination.

4. **Compute per-symbol FFTs on the shifted time-domain signal.** A
   length-`block` (1920 samples per symbol at 12 kHz) FFT is taken at
   each symbol position, producing the 2D array `bins[symbol_index][tone_bin]`
   of complex values. This step is the "ffts()" call in ft8mon.

5. **Inner loop over time sub-step index** `off_frac_i` from 0 to
   `coarse_off_n - 1`. The fractional time shift is simply realized by
   advancing the starting sample index by `off_frac_i × (block / coarse_off_n)`
   samples before extracting the per-symbol FFTs (i.e. each value of
   `off_frac_i` selects a different starting index into the same shifted
   buffer).

6. **For each (hz_frac_i, off_frac_i) combination, evaluate Costas
   strength at every candidate (frequency bin, symbol-start) pair.** The
   scoring kernel iterates the candidate frequency bin `bi` across the
   working band and the candidate symbol-start `si` across `[si0, si1]`.
   For each candidate:
   - Read the eight tone magnitudes `|bins[si + k][bi + t]|` for k in the
     three Costas-block positions (symbols 0–6, 36–42, 72–78) and
     t in 0..7.
   - At each Costas symbol, accumulate the magnitude of the
     **expected** tone as "signal" and the sum of the magnitudes of the
     other seven tones as "noise". The expected Costas tone sequence is
     `{3, 1, 4, 0, 6, 5, 2}`.
   - At each data symbol position, treat the strongest tone as the
     unknown best guess; accumulate the other seven as noise.
   - Combine signal and noise into a score. The default scoring method
     (`coarse_strength_how = 6`) is `signal / noise`.

7. **Collect, sort, deduplicate.**
   - Append each (sub-bin Hz, sub-symbol offset, strength) tuple into a
     master candidate list.
   - After all 16 (hz_frac × off_frac) combinations are exhausted, sort
     by strength descending.
   - Per-frequency-bin deduplication: keep only the top
     `ncoarse` (default 1) symbol-offset candidates per bin, with a
     minimum spacing of `ncoarse_blocks` symbol-times between candidates
     in the same bin.

8. **Promote** the top candidates to the fine-sync + LDPC decode stage,
   skipping any frequencies already claimed by an earlier successful
   decode in this slot. The "already-decoded" exclusion zone is
   `already_hz` Hz wide (default 27 Hz).

### Numerical constants (facts, not expression)
- `coarse_hz_n = 4` — frequency sub-bin oversampling factor.
- `coarse_off_n = 4` — time sub-symbol oversampling factor.
- `bin_hz = 6.25 Hz` at 12 kHz sample rate, 1920-sample symbol blocks.
- Frequency sub-step size = `bin_hz / coarse_hz_n` = 1.5625 Hz.
- Time sub-step size = `block / coarse_off_n` = 480 samples at 12 kHz
  (40 ms).
- Costas sync pattern: `{3, 1, 4, 0, 6, 5, 2}` at symbol positions
  0–6, 36–42, 72–78.
- Default `coarse_strength_how = 6` (signal-to-noise ratio).
- Default `ncoarse = 1` candidates retained per frequency bin.
- Default `already_hz = 27 Hz` exclusion zone after a successful decode.
- Symbols per frame = 79.
- Tones per symbol = 8.
- Time window: `tminus ≈ 2.2 s`, `tplus ≈ 2.4 s` (DT search bounds).

### Edge cases
- **At the band edges**, the bin-rotation trick can wrap rotated indices
  past the start/end of the spectrum. Out-of-range bins should be treated
  as zero (or skipped) rather than wrapping circularly into the opposite
  band edge.
- **Frequencies already covered by a successful decode** are skipped via
  an `already[round(hz / already_hz)]` flag array.
- **`(hz_frac_i = 0, off_frac_i = 0)`** is the natural bin-aligned
  search (no sub-bin refinement); this case should produce results
  identical to the current pancetta coarse sync, useful for regression
  testing.
- **Candidate de-duplication** — without per-bin de-dup the strongest
  signal in the band would otherwise generate one nearly identical
  candidate per sub-step combination (16 copies). The min-spacing rule
  prevents this.
- **Deadline / budget** — coarse sync runs under a wall-clock budget;
  outer loops should check the deadline between iterations.

## Conflict with pancetta's existing mechanisms

Pancetta's current coarse sync (per the codebase summary in CLAUDE.md
and Batch 30 results) operates at one-bin frequency resolution and the
existing sync-hop time grid. The hb-216 hardware-tier classifier already
flips `scoped_fast_path` on slow tiers, and `max_decode_passes=1` plus
`osd_depth=Some(1)` on slow tiers. The sub-bin Costas sweep multiplies
coarse-sync compute by ~16× (16 inverse FFTs and 16 scoring sweeps).
Recommended integration: gate the sub-bin sweep behind a config flag
(`coarse_sub_bin: bool`, default off) so the Fast tier opts in while
Moderate/Slow tiers retain the cheaper one-bin sweep. The
`scoped_fast_path` atomic gives a natural conditional.

Pancetta also already has the Costas pattern hard-coded for synthesis
and detection (`{3, 1, 4, 0, 6, 5, 2}`); no new constant needed.

The strongest interaction is with the existing per-bin deduplication
and the `already_hz`-style decode-suppression list — both must stay in
place, and the new sub-bin candidates must be folded into the same
per-bin retention rule (otherwise a single signal will emit ~16
near-duplicates into the fine-sync queue).

## Estimated Rust port effort
- ~250–400 LOC of new code in `pancetta-ft8/src/decoder/coarse.rs` (or
  wherever coarse sync currently lives), plus a frequency-domain shift
  helper.
- 2–3 sessions: (S1) bin-rotation shift primitive + golden test against
  resampled-and-re-FFTed reference at a few fractional shifts; (S2)
  16-position sweep wired through `coarse()`-equivalent with per-bin
  retention rule; (S3) eval on hard-200 corpus, tune
  `coarse_hz_n` / `coarse_off_n`, and gate behind a config.

## Implementation notes for the implementer thread

- The bin-rotation primitive is the load-bearing piece: a function
  `fft_shift(bins: &[Complex<f32>], shift_hz: f32, bin_hz: f32) -> Vec<f32>`
  that returns the time-domain signal frequency-shifted by `shift_hz`.
  Equivalent realizations: (a) FFT-domain rotation + iFFT, or (b)
  time-domain mixing with a complex exponential. (b) avoids an iFFT but
  loses the "one large FFT cached" win — prefer (a).
- The integer "down" shift count is `round(shift_hz / bin_hz)`; the
  fractional residual after that integer shift can be realized by a
  per-bin phase ramp before iFFT.
- Per-symbol FFTs on the shifted buffer can reuse pancetta's existing
  per-symbol FFT path verbatim — only the input buffer differs.
- For the scoring kernel, mirror the structure pancetta already uses for
  hard-tier coarse sync; add the (hz_frac_i, off_frac_i) tuple as an
  outer loop and emit candidates carrying the resolved sub-bin Hz and
  sub-symbol offset rather than rounded ones.
- Suggested config knobs to plumb through `Ft8Config`:
  `coarse_sub_bin: bool`, `coarse_hz_n: u8`, `coarse_off_n: u8`. Default
  `coarse_sub_bin = false`; the tier classifier flips it on for Fast.
- Regression test: with `coarse_hz_n = 1`, `coarse_off_n = 1` the output
  should be bit-exact identical to the existing coarse sync — that's
  the firewall against accidental behavior change.
- Eval target: band-middle 1000–2000 Hz recall on hard-200. Current
  baseline 55–57% (per hb-217 batch notes). Hypothesis: sub-bin sync
  should recover several percentage points on this band specifically,
  with neutral-to-positive effect on band edges.
- Watch for compute regression on Moderate / Slow tiers — keep the
  default off and only flip on Fast.
