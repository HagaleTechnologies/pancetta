# Algorithm spec: Known-tone (Hz, time-offset) refinement before subtraction

## Source attribution
- Origin: ft8mon
- File paths in `ft8.cc`:
  - ~2948–2967: invocation site inside `try_decode` after LDPC+CRC passes
  - ~1160–1214: `search_both_known` (2D refinement search)
  - ~1005–1012: `one_strength_known` with `known_strength_how = 7`
    (phase-coherence score)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Multi-pass decoding reuses the residual signal after subtracting already-
decoded signals from the slot. The cleanliness of that residual depends
directly on how accurately the decoded signal's frequency and time
offset are estimated. The coarse + fine sync stages produce estimates
that are good enough to find and decode the signal, but the
quantization residuals and the limited window of the fine search leave
sub-bin / sub-sample errors that smear the subtracted waveform. After
a successful LDPC + CRC decode, the *exact* 79-symbol tone sequence is
known, which lets the decoder run a richer position-search using the
known waveform as a matched filter — refining (Hz, time-offset) to the
precision needed for a clean subtraction. Cleaner residuals → more
multipass decodes of weaker overlapping signals.

## Algorithm description (PROSE ONLY)

### Inputs
- The full slot's audio samples (decimated working buffer).
- The pre-refinement estimates `(hz_initial, off_initial)` from coarse +
  fine sync.
- The now-known 79-symbol tone sequence reconstructed from the LDPC
  codeword via the `recode` mapping (decoded 174 bits → 58 data symbols,
  interleaved with the three Costas blocks, gray-coded to tones).

### Outputs
- A refined `(hz_refined, off_refined)` pair, expressed in audio Hz and
  sample-domain offset, intended to drive the subtraction routine.

### Steps

1. **Set up the 2D search grid** around the initial estimate:
   - Frequency axis: `hz_initial ± third_hz_win` (default ±0.25 Hz),
     divided into `third_hz_n` steps (default 3). Step size
     `hz_inc = 2 × third_hz_win / (third_hz_n − 1)` = 0.25 Hz at
     defaults.
   - Time axis: `off_initial ± third_off_win` symbol-times (default
     ±0.075 symbol periods, or roughly ±2.4 samples at the internal
     200 samples/sec working rate; the rate is decimated for this
     refinement step), divided into `third_off_n` steps (default 4).
     Step size `off_inc = 2 × third_off_win / (third_off_n − 1)`.

2. **One large FFT over the working buffer** is computed up front.
   Subsequent frequency offsets are realized by bin rotation
   (`fft_shift_f`-style) of the cached FFT, not by recomputing.

3. **Outer loop over frequency grid**:
   For each candidate `hz_test` in the frequency grid:
   - Apply the bin-rotation frequency shift to bring `hz_test` to bin
     center.
   - Call `search_time_fine_known` (the inner time-axis search) which
     iterates over the time grid and evaluates strength at each
     `(hz_test, off_test)` pair via `one_strength_known`.

4. **Strength evaluation** at each `(hz_test, off_test)`:
   `one_strength_known(known_symbols, rate, hz_test, off_test, how = 7)`
   computes a **phase-coherence** score. Steps inside this score:
   - For each of the 79 symbol positions, extract the complex FFT bin at
     the tone frequency *that is now known to be transmitted at this
     position*. Call this complex value `c[i]`.
   - Accumulate `D = Σ |c[i] − c[i−1]|` for `i` from 1 to 78 — i.e.
     the sum of the magnitudes of *first differences* of the complex
     tone bins from symbol to symbol.
   - Return `-D` (the negation of that sum).
   The interpretation: if the candidate (Hz, time-offset) is correct,
   the complex value at the known tone bin for each symbol should be
   approximately equal in both amplitude and phase across consecutive
   symbols (modulo channel slow fading) — i.e. the first differences
   should be small. Any frequency or timing misalignment introduces a
   sample-to-sample phase ramp that explodes the differences. Lower
   sum-of-differences ↔ better alignment ↔ higher (less negative)
   score. The negation lets the surrounding code use a single "argmax"
   convention.

5. **Pick the argmax** over the full 2D grid: the `(hz_refined,
   off_refined)` pair with the highest (least negative) score wins.

6. **Convert refined coordinates back to original sample-rate units**
   before passing into subtraction:
   - The time-offset value from the search is expressed in samples at
     the decimated working rate (200 Hz in ft8mon); multiply by
     `original_rate / working_rate` to convert to original-rate samples.
   - The frequency value is expressed in baseband Hz relative to the
     decimation center; add the original band-center offset.

7. **Hand `(hz_refined, off_refined)` to the subtractor.** The subtractor
   (a separate routine, conceptually `subtract`):
   - Reconstructs a synthesized signal in the time domain by integrating
     the phase of each of the 79 known tones at the refined frequency,
     symbol by symbol.
   - Applies a smooth raised-cosine ramp at each symbol boundary to
     avoid discontinuities.
   - Allows for slight inter-symbol frequency drift via interpolated
     phase rates between symbols.
   - Subtracts the reconstructed waveform sample-by-sample from the
     original buffer.
   - The residual buffer is then re-handed to the next decode pass.

### Numerical constants (facts, not expression)
- `third_hz_win` = 0.25 Hz — half-width of frequency refinement window.
- `third_hz_n` = 3 — frequency grid points.
- `third_off_win` ≈ 0.075 symbol periods — half-width of time
  refinement window.
- `third_off_n` = 4 — time grid points.
- Working sample rate during refinement = 200 Hz (decimated) in ft8mon.
- `known_strength_how = 7` — selects the phase-coherence (sum of
  first differences, negated) scoring mode.
- 79 symbols per frame, 7 Costas tones at positions 0–6, 36–42, 72–78
  (Costas tones are folded into the same scoring since they are also
  known).

### Edge cases
- **Boundary clipping**: when `off_initial` is near the edge of the
  slot, the time grid may extend into samples that don't exist. Clip
  the grid points that fall outside the buffer.
- **Empty score at a grid point**: if all the first differences happen
  to be zero (degenerate — only possible at perfect tone-aligned DC
  silence), the score is `0`, which beats any negative score; ensure
  the grid sweep doesn't trivially select an empty/zero-power region.
  In practice the LDPC pre-validation makes this impossible (a
  zero-power signal would not have decoded).
- **Frequency drift across the frame**: the `how = 7` score implicitly
  tolerates slow drift because consecutive differences average it out,
  but large drift (more than the frequency grid spacing) cannot be
  corrected here. A separate drift-search step (not specced here) would
  be needed.
- **Costas tones**: include them in the difference sum. They are known
  (just like data tones), and their inclusion improves the SNR of the
  refinement score by ~25% more sample points.
- **Decimation-rate mismatch**: the refined coordinates must be rescaled
  back to original sample-rate units before feeding the subtractor.
  Off-by-one errors here cause visibly degraded subtraction.

## Conflict with pancetta's existing mechanisms

Pancetta currently has multi-pass decode (`max_decode_passes`, default
≥2) but per the project notes the multi-pass coverage gain has been
modest in recent batches. The "subtract uses fine-sync coordinates"
pattern is the suspect: fine sync was tuned for *finding* the signal,
not for the clean *removal* of it. Adding the known-tone refinement
between LDPC + CRC and the subtractor is a focused upgrade that doesn't
touch the rest of the pipeline.

Two specific knobs are likely to interact:
- `osd_depth` — known-tone refinement runs *after* LDPC + CRC success,
  so OSD-recovered codewords (which are noisier on average) also
  benefit from refinement before their residual is subtracted.
- hb-216 tier-classifier `max_decode_passes = 1` on Slow tier — when
  passes = 1, the refinement is wasted compute (there is no subsequent
  pass that would consume the cleaner residual). Gate the refinement
  behind `passes > 1`.

The refinement adds a small per-decode cost (3 × 4 = 12 score
evaluations, each a partial FFT readout) — negligible compared to the
already-paid LDPC + CRC + subtractor cost. No need to tier-gate beyond
the `passes > 1` check.

## Estimated Rust port effort
- ~200–300 LOC in `pancetta-ft8/src/decoder/refine.rs` (new file).
- 1–2 sessions: (S1) port the `how=7` score and the 12-point grid
  search, with a unit test that synthesizes a known signal at a known
  off-bin / off-sample offset and confirms refinement converges to the
  truth within tolerance; (S2) wire into the subtractor invocation
  path, eval on hard-200 with `max_decode_passes ≥ 2`, measure pass-2
  decode delta.

## Implementation notes for the implementer thread

- The known-tone refinement *requires* the full complex per-symbol FFT
  output at the candidate frequency — magnitude-only doesn't work
  because the score is built on complex first differences. If pancetta
  discards phase post-FFT, retain or recompute phase for the
  refinement call.
- The bin-rotation primitive needed here is the same one as the sub-bin
  Costas spec. If both specs are implemented, share the implementation.
- The 12-point grid is small enough that brute-force scan is the right
  algorithm; no need for gradient descent or interpolation.
- Suggested struct: `pub struct Refinement { pub hz: f32, pub off_samples: i64 }`
  returned from `refine_known_tones(...)`.
- Suggested integration point in pancetta-ft8: between
  `decode_one_candidate` returning Ok(codeword) and `subtract_known(...)`.
  Replace the `(hz_fine, off_fine)` arguments to the subtractor with
  the refined pair.
- Test plan: synthesize a single FT8 signal at known (Hz, t) with sub-bin
  offsets in {0, 0.25, 0.5, 0.75} × bin_hz and sub-sample offsets in
  {0, 0.25, 0.5, 0.75} samples; confirm refinement converges to within
  ±0.05 Hz and ±0.1 sample on each.
- Eval metric: count of pass-2 decodes on hard-200 corpus, holding
  pass-1 fixed. This isolates the refinement-via-subtraction win from
  any first-pass changes.
- Optional follow-on: also re-run the LDPC bit-LLR computation on the
  refined symbol bins, in case the slightly tighter alignment promotes
  marginal bits and lets OSD finish a partial decode. ft8mon doesn't
  do this; it's a pancetta-original extension worth a separate
  experiment.
