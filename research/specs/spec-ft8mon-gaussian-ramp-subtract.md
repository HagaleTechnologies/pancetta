# Algorithm spec: Gaussian-ramp subtraction with measured next-symbol phase

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `subtract()` around lines 2770-2912. Constant:
  `subtract_ramp = 0.11` at line 96.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

When a strong signal is decoded, ft8mon subtracts its reconstructed
waveform from the slot buffer so that weaker signals previously
masked by the strong one become visible to subsequent decode passes
(spectral subtraction iteration). The subtraction quality determines
how clean the residual is — over-subtraction creates a hole that
behaves like a wideband noise burst at the strong signal's center
frequency; under-subtraction leaves residual energy that the next
pass mistakes for a real signal.

The naive subtraction is "for each symbol time, write
`amp × cos(2π × tone_hz × t + phase)` into a 160 ms window and
subtract from the buffer". This is wrong in three load-bearing ways:

1. **Hard edges at symbol boundaries** cause spectral splatter — the
   instantaneous frequency change from tone `f1` to tone `f2` at
   `t = symbol_boundary` creates broadband energy that the original
   GFSK-shaped transmit waveform did not contain. The residual will
   show spectral spikes at the symbol-boundary times.
2. **Mismatch between measured phase at one symbol and predicted
   phase at the next** — even with perfect frequency, the measured
   phases of two adjacent symbols (extracted from FFT bins) generally
   don't smoothly connect because the receiver's `(hz, off)` estimate
   has small residual errors. A naive subtraction will subtract a
   waveform with the wrong phase at every symbol boundary, leaving a
   significant residual.
3. **Mismatch with GFSK transmit shaping** — the FT8 transmit
   waveform is Gaussian-filtered before being frequency-modulated, so
   each symbol's instantaneous frequency ramps smoothly between
   neighbors over a roughly 3.5 ms transition. Subtracting a
   rectangular-envelope sinusoid leaves the transition energy
   uncancelled.

ft8mon's `subtract()` addresses all three with a unified
inter-symbol-transition mechanism: a `subtract_ramp = 0.11` symbol
fractional ramp at each symbol boundary (~3.5 ms at 32 sa/sym in
200-sps), inside which the instantaneous frequency linearly slews
from the current symbol's tone to the next symbol's tone, and inside
which a phase correction is spread to make the subtracted waveform
arrive at the next symbol's *measured* phase rather than its
*predicted* phase.

This is hb-226 candidate territory — pancetta's current subtraction
may be hard-edged, leaving exactly the splatter pattern this spec
fixes.

## Algorithm description (PROSE ONLY)

### Inputs
- `re79[79]` — the 79 reconstructed symbol indices (each in `0..8`)
  for the decoded message. Includes the three Costas blocks; obtained
  by re-encoding the LDPC output (`recode()`).
- `hz0`, `hz1` — best-fit start and end frequencies for the decoded
  signal (separate values to model linear drift; equal when no drift
  modeling).
- `off_sec` — best-fit time offset in seconds (slot-relative).
- The slot buffer `nsamples_` at the post-stage-1-reduction sample
  rate.

### Outputs
- Mutates `nsamples_` in place. The subtracted waveform's energy is
  removed from the buffer; the buffer becomes the input to the next
  decode pass.

### Steps

1. **Coarse-align via Hilbert shift.** Translate the slot buffer in
   frequency so the decoded signal's center sits exactly on an FFT
   bin (call it `bin0`). The shift amounts `diff0`, `diff1` are
   `bin0 × bin_hz - hz0` and `bin0 × bin_hz - hz1` respectively; the
   `hilbert_shift` helper applies a complex-mixer time-domain shift
   that linearly drifts from `diff0` at slot start to `diff1` at slot
   end, accommodating drift. The resulting buffer is `moved`.

2. **Measure per-symbol complex bins on `moved`.** Take an FFT at each
   symbol position from `off0 = off_sec × rate_`. At symbol `i`, pick
   the bin at `bin0 + re79[i]` (the symbol's true tone). Record:
   - `phases[i] = arg(c)` — measured phase of the true tone.
   - `amps[i] = |c| / (block / 2)` — measured amplitude (the
     `block/2` divisor un-normalizes the FFT magnitude convention).

3. **Compute the inter-symbol ramp length** as `ramp = round(block × subtract_ramp)`
   where `block` is samples per symbol at the working rate. At 200 sps
   with `block = 32` and `subtract_ramp = 0.11`, `ramp = 4` samples ≈ 20 ms.
   At 12 kHz with `block = 1920`, `ramp = 211` samples ≈ 17.6 ms.
   At higher target rates the ramp is longer in samples but constant in
   time-fraction-of-symbol. (Note: ft8mon's comment cites 3.5 ms; with
   `subtract_ramp = 0.11` and FT8's 160 ms symbol that's exactly
   17.6 ms, which is the GFSK BT product time constant — not 3.5 ms.
   The 3.5 ms comment in older sources may refer to a different rate.
   Use 0.11 as the authoritative parameter.)

4. **Initial on-ramp at slot start.** Before the first symbol's steady
   state, write a `ramp`-sample fade-in from 0 to full amplitude at
   the first symbol's tone:
   - For each sample `jj` in `0..ramp`, write
     `amp[0] × cos(phase[0] + jj × dtheta) × (jj / ramp)` and
     subtract from `moved` at index `off0 + jj`.

5. **For each symbol `si` in `0..79`**, do two phases:

   **Steady-body phase (`ramp ≤ jj < block - ramp`)**: write the
   measured-amplitude, measured-phase sinusoid at the symbol's tone:
   - `theta = phases[si] + jj × dtheta` where `dtheta = 2π × tone_hz / rate_`.
   - Subtract `amps[si] × cos(theta)` from `moved[off0 + block × si + jj]`.

   **Inter-symbol transition phase (`block - ramp ≤ jj < block + ramp`)**:
   smoothly slew frequency from current symbol's tone to next
   symbol's tone, while applying a phase correction so the slewed
   waveform lands at the *measured* phase of the next symbol (not
   the phase that naive frequency-integration would predict).
   - Look up the next symbol's tone `hz1` and phase `phase1`. If this
     is the last symbol (`si == 78`), use the current symbol's values
     (the trailing off-ramp will fade to zero so phase doesn't
     matter).
   - Compute `dtheta1 = 2π × hz1 / rate_` — the angular velocity for
     the next symbol.
   - The frequency-slew increment per sample is
     `inc = (dtheta1 - dtheta) / (2 × ramp)`. This linear angular-
     velocity ramp over the full `2 × ramp` transition samples is
     the approximation of WSJT-X's Gaussian shaping.
   - Predict where the naive integration would put the phase at the
     end of the next symbol's on-ramp:
     `actual = theta_at_offramp_start + dtheta × (2 × ramp) + inc × (4 × ramp²) / 2`.
   - Compute the target phase the next symbol needs to reach at the
     end of its on-ramp:
     `target = phase1 + dtheta1 × ramp`.
   - Unwrap the `(target - actual)` difference into `[-π, +π]` by
     adding or subtracting `2π` as needed.
   - The phase correction `adj = target - actual` is spread evenly
     across the `2 × ramp` transition samples: each sample's `theta`
     advance is augmented by `adj / (2 × ramp)`.
   - For each transition sample `jj` from `block - ramp` to
     `block + ramp - 1`:
     - Subtract `amps[si] × cos(theta)` from the buffer.
     - Update `theta += dtheta` (the angular velocity itself).
     - Update `dtheta += inc` (the angular acceleration).
     - Update `theta += adj / (2 × ramp)` (the phase-correction
       distribution).

   **Final off-ramp for the last symbol**: when `si == 78`, taper the
   off-ramp amplitude as `1 - ((jj - (block - ramp)) / ramp)` so it
   reaches zero by sample `block`, then stop. Don't slew into a
   non-existent next symbol.

6. **Un-shift back to original frequency.** Apply the inverse Hilbert
   shift (`-diff0`, `-diff1`) to `moved` and store in `nsamples_`.

### Numerical constants (facts, not expression)
- `subtract_ramp = 0.11` — fractional ramp length as a fraction of
  symbol period. With 160 ms symbols this is 17.6 ms total transition
  (8.8 ms off-ramp from current + 8.8 ms on-ramp into next).
- At 200 sps × 32 samples/symbol, `ramp = 4` samples per transition
  side (8 samples total).
- At 12000 sps × 1920 samples/symbol, `ramp = 211` samples per side.
- Minimum ramp = 1 sample (the source clamps `if(ramp < 1) ramp = 1`)
  to avoid divide-by-zero at extremely low sample rates.
- FFT magnitude un-normalization: divide by `block / 2` (the standard
  FFT-of-real-signal magnitude convention).
- Symbol count: 79. Tone spacing: 6.25 Hz. Costas pattern
  `{3, 1, 4, 0, 6, 5, 2}` at positions 0..6, 36..42, 72..78
  (subtraction treats Costas symbols the same as data — known tone,
  measured phase and amplitude).

### Edge cases
- **First symbol on-ramp** — written before the per-symbol loop with
  a linear `jj / ramp` amplitude fade. Phase = measured phase of
  symbol 0 (no slew from a non-existent previous symbol).
- **Last symbol off-ramp** — written inside the per-symbol loop but
  with the amplitude tapered to zero at sample `block`. The end loop
  bound becomes `end = block` rather than `block + ramp` so the
  off-ramp doesn't spill into a non-existent symbol 79.
- **Phase unwrap loop** — `target` and `actual` can differ by an
  arbitrary multiple of `2π` after frequency integration; the
  `while(|target - actual| > π)` loop wraps them to the nearest
  branch. This is correctness-critical: without it, the phase
  correction would distribute a 2π adjustment across `2 × ramp`
  samples, causing audible (and spectrally observable) "click" at
  every symbol boundary.
- **Amplitude estimate** — measured from the FFT magnitude at the
  symbol's true tone. This captures fading-induced amplitude
  variation across the message; subtracting with a constant
  amplitude (e.g. the message average) leaves bigger residuals at
  the deepest fades.
- **Drift handling** — `hz0 != hz1` triggers a drifting Hilbert shift
  rather than a constant one. Pancetta's current subtraction may
  assume zero drift; if so, this generalization can be a follow-on
  step.
- **bin0 + 8 past spectrum end** — the source guards
  `if(bin0 + 8 > bins[0].size()) return;` to avoid out-of-range FFT
  reads for very high-frequency signals near the Nyquist edge.

## Conflict with pancetta's existing mechanisms

Pancetta's coordinator does spectral subtraction between decode
passes (per the `multipass=2` config and the standard ft8_lib pattern).
The existing subtraction is likely simpler than this spec — at
minimum it should be reviewed for:

1. Whether it uses **measured** per-symbol phase or **predicted**
   phase. Predicted phase (integration from start) accumulates error
   across 79 symbols and leaves large residuals at the end of the
   message.
2. Whether it has **inter-symbol transitions** at all. A hard-edged
   subtraction (instantaneous frequency change at symbol boundaries)
   leaves spectral splatter that the next pass mistakes for weak
   signals — false positives concentrated at the strong-signal center
   frequency.
3. Whether it spreads the inter-symbol **phase correction** across
   the transition. Even with frequency slewing, a phase jump at the
   midpoint of the slew is visible in the spectrum.

The hb-217 corpus-scale capture-effect finding — neighbor density 0/1/2/3
→ 76.0%/42.6%/26.7%/14.9% recall — is consistent with subtraction
residue contaminating the bands adjacent to strong signals. A clean
subtraction should restore recall in the "1 neighbor" bucket by 5-10
percentage points.

Interaction with the `prevdecs` cross-slot subtraction (separate
spec): the same subtraction code runs for both intra-slot and
cross-slot subtraction. Cleaning it up wins twice.

Interaction with hb-103 / hb-058 FP filters: a clean subtraction
*reduces* the FP rate (fewer splatter-driven false candidates), so
expect the FP filters to fire less. No threshold recalibration
needed.

## Estimated Rust port effort
- ~200-300 LOC in `pancetta-ft8/src/decoder/subtract.rs` (or
  wherever pancetta's current subtract lives), plus ~50 LOC for the
  drifting Hilbert shift helper if not already present.
- 2 sessions: (S1) implement the smooth-ramp subtraction with a
  golden test that subtracting a synthesized known signal from itself
  yields a residual within ~-50 dB of the original at all frequencies
  outside the signal's bandwidth (the splatter test); (S2) eval on
  hard-200, specifically the "1 neighbor" bucket per hb-217.

## Implementation notes for the implementer thread

- The structure of the per-symbol loop is the load-bearing detail:
  steady body → off-ramp → on-ramp into next symbol → next symbol's
  steady body. The off-ramp and the next symbol's on-ramp together
  form the `2 × ramp`-sample transition; don't accidentally write
  the on-ramp twice (once as the previous symbol's off-ramp tail
  and once as the next symbol's on-ramp head).
- The angular-acceleration approach (constant `inc` per sample)
  produces a **linearly-changing frequency**, which is the linear
  approximation to a true Gaussian frequency profile. ft8mon's
  comment calls this "approximating wsjt-x's gaussian frequency
  shift". A more faithful Gaussian curve is a follow-on if needed,
  but the linear approximation captures most of the spectral cleanup.
- The phase-spreading update (`theta += adj / (2 × ramp)` per sample)
  must happen *in addition to* the `theta += dtheta` and
  `dtheta += inc` updates inside the transition loop. Order of the
  three updates matters because they're cumulative; match the source
  order to avoid subtle off-by-one phase errors at transition
  endpoints.
- The Hilbert shift is the most novel helper. If pancetta doesn't
  already have a drifting (linearly-interpolating between two shift
  rates) variant, prototype with a constant shift first
  (`diff0 == diff1`) and add drift later. Drift typically matters
  only at high HF where transmitter frequency wander is significant
  (~1-3 Hz per 13 seconds).
- Tier interaction: subtraction quality matters for the second and
  later decode passes. Slow tier runs only one pass, so this work
  has zero impact on Slow tier. Moderate and Fast tier benefit
  proportionally to `max_decode_passes`.
- Eval: in addition to hard-200 recall, run the spectral-purity test:
  synthesize a single FT8 signal at -10 dB, subtract, FFT the
  residual, and look for splatter spikes at 6.25 Hz multiples around
  the signal's center. With hard-edged subtraction these are
  prominent; with ramped subtraction they should be < -40 dB.
- Don't fuse this with the wsjtr DT-refinement-during-subtract spec
  (`spec-wsjtr-dt-refinement-during-subtract.md`) in one session —
  they're orthogonal mechanisms (one refines the (hz, off) estimate
  before subtracting; this one improves the subtraction waveform
  itself given any estimate). Land them sequentially so each can be
  evaluated in isolation.
