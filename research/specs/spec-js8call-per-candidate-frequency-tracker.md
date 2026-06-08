# Algorithm spec: JS8Call-Improved per-candidate frequency tracker

## Source attribution

- Origin: JS8Call-Improved (https://github.com/JS8Call-improved/JS8Call-improved)
- File path (for traceability, NOT to be quoted): `JS8_Mode/FrequencyTracker.h`
  and `JS8_Mode/FrequencyTracker.cpp`
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

After the candidate-generation stage of the decoder produces a coarse
frequency estimate (typically rounded to the FFT bin grid), there is
still a residual frequency error of up to half a bin (a few Hz) plus
whatever drift the transmitter contributed during the slot. Standard
FT8 decoders compensate for this with one or two refinement passes at
candidate-generation time (e.g., the `twkfreq` step in WSJT-X) and
then assume the corrected estimate is good for the whole slot.

When the actual drift is non-trivial — cheap radios, mobile stations,
solar-flare-induced ionospheric chirp — the front and back of a 13-
second JS8 transmission may experience different residual frequency
errors, which smears symbol energy across bins and weakens LLRs.

JS8Call-Improved's `FrequencyTracker` runs an *adaptive* per-candidate
frequency tracker *inside* the decode loop. It treats the Costas sync
symbols (which are at known tones) as pilot tones and uses the phase
residual at each sync block to update a running frequency estimate.
The tracker then applies the running correction to the complex
samples as they flow through the symbol demapper.

## Algorithm description (PROSE ONLY)

### Inputs (initialization)

- `coarse_hz`: the coarse frequency estimate handed in by the
  candidate generator.
- `sample_rate`: in samples per second (12 000 for FT8 / JS8 audio).
- Tuning parameters:
  - `alpha`: damping factor for the running estimate. Smaller =
    smoother, larger = more responsive. Typical 0.1–0.3.
  - `max_step_hz`: per-update cap on how much the estimate can move
    in one update. Typical 1–2 Hz.
  - `max_error_hz`: absolute bound on the running estimate (relative
    to coarse). If a pilot residual would push the estimate beyond
    this bound, clamp it. Typical 5 Hz (about half the FFT bin
    width at FT8's 6.25 Hz tone spacing).

### Inputs (per-call)

- A complex sample buffer (the chunk of audio currently being
  consumed by the symbol demapper).
- A residual frequency measurement extracted from the most recent
  pilot (Costas) symbols. The host decoder already computes this
  for its own sync refinement; the tracker just consumes it.

### Outputs

- A frequency-corrected complex sample buffer (the same buffer,
  rotated by the current tracked offset to remove residual error
  in-place).
- Observable state: `current_hz` (the current frequency-error
  estimate) and `average_step_hz` (rolling average of update sizes,
  useful for telemetry).

### Steps

1. **Initialise** with the coarse frequency. Set
   `current_hz_offset = 0` (the running deviation from coarse;
   absolute frequency is `coarse_hz + current_hz_offset`). Reset
   step statistics.
2. **For each chunk of samples** (typically one symbol's worth):
   1. **Apply**. Rotate the complex samples by the current offset:
      multiply sample `n` by `exp(-j × 2π × current_hz_offset × n /
      sample_rate)`. This removes the currently-estimated residual
      from the chunk before the demapper sees it.
   2. **Demap**. Pass the rotated chunk to the symbol demapper as
      usual.
3. **At each pilot opportunity** (i.e., after a Costas sync block is
   consumed, three times per slot):
   1. The host decoder computes a residual frequency measurement
      from the pilot tones' phases (this is standard sync-refinement
      math; it produces a value in Hz indicating how much the pilot
      tones are still drifting after the current `current_hz_offset`
      has been applied).
   2. **Update**. Move `current_hz_offset` toward the measurement
      with damping: `current_hz_offset += alpha × residual`.
   3. **Clamp the step**. If `alpha × residual` exceeds
      `max_step_hz`, cap it at `±max_step_hz`. This prevents a
      single noisy pilot from yanking the tracker off-course.
   4. **Clamp the bound**. If `|current_hz_offset|` would exceed
      `max_error_hz`, clamp it. This prevents the tracker from
      wandering arbitrarily far from coarse.
   5. Record the actual step size in `average_step_hz` (exponential
      moving average for monitoring).
4. **Continue** until all symbols are consumed. The tracker is then
   discarded (it is per-candidate, single-use).

### Numerical constants (facts, not expression)

- `alpha`: 0.1–0.3 is a sane range; 0.2 is a reasonable default.
- `max_step_hz`: 1–2 Hz; 1.5 Hz is a reasonable default.
- `max_error_hz`: 5 Hz; corresponds to ±0.8 FFT bins at FT8's 6.25 Hz
  bin spacing.
- Update cadence: once per Costas sync block (three updates per FT8
  slot; more for JS8 slow submodes).
- The tracker is stateful within one candidate's decode but does not
  carry state across candidates or across slots.

### Edge cases

- No usable pilot measurement (Costas correlation too low): skip the
  update for that opportunity. Do not let a zero or NaN residual
  poison the running estimate.
- All three Costas blocks unreliable: the tracker effectively never
  moves from coarse. Same outcome as today's decoder; no regression.
- Extreme drift (a station that drifts more than `max_error_hz`
  during a slot): the tracker will clamp and the back-half of the
  slot will demap with residual drift. This is a known-limitation
  case; the alternative is to widen `max_error_hz` and accept more
  candidate-generation false positives. The default favours
  precision.
- Sample-rate mismatch: the rotation math depends on the audio sample
  rate. Hard-code 12 000 for now; revisit if pancetta ever supports
  alternate sample rates.

## Conflict with pancetta's existing mechanisms

- Pancetta-ft8 does one-shot sync refinement (per the
  `spec-wsjtr-dt-refinement-during-subtract.md` and
  `spec-wsjtr-sync-bc.md` family). That step produces a corrected
  coarse frequency *and time*. The adaptive tracker does not
  replace it — it runs *after* the one-shot refinement and provides
  fine-grained drift correction during symbol demap.
- The tracker is per-candidate, so cost is bounded: ~120 complex
  multiplies per symbol times ND symbols times candidate count.
  Modest; cache-friendly; safe on the M4 / FTdx10 target.
- Pairs naturally with the LLR whitening pass: the tracker reduces
  the smear of symbol energy across bins, which makes the per-tone /
  per-symbol noise estimates in the whitener cleaner.

## Estimated Rust port effort

- ~120 LOC including the tracker struct, the apply / update methods,
  the clamping logic, and unit tests.
- 1–2 implementer sessions.

## Implementation notes for the implementer thread

- New module: `pancetta-ft8/src/decoder/freq_tracker.rs`.
- Public API:
  - `struct FrequencyTracker { coarse_hz, current_offset_hz, alpha,
    max_step_hz, max_error_hz, step_ema, sample_rate }`
  - `fn new(coarse_hz: f64, sample_rate: f64, cfg:
    &FreqTrackerConfig) -> Self`
  - `fn apply(&self, samples: &mut [Complex<f32>], chunk_start_n:
    usize)` — rotates the chunk in-place.
  - `fn update(&mut self, residual_hz: f64)` — applies clamped step.
  - `fn current_offset_hz(&self) -> f64` — for telemetry.
- The rotation is a simple complex multiplication by a precomputed
  twiddle ramp; cache the per-sample twiddle if needed.
- Config knobs in `pancetta-config::Ft8Config` or a new
  `Ft8FreqTrackerConfig`: `enabled` (default false), `alpha`
  (default 0.2), `max_step_hz` (default 1.5), `max_error_hz`
  (default 5.0).
- Call site: in the per-candidate decode loop in `decoder.rs`,
  instantiate a `FrequencyTracker` per candidate; call `apply`
  before each symbol's demap; call `update` after each Costas sync
  block. The residual is the same one the existing sync-refinement
  computes; expose it from that step.
- Telemetry: log `(current_offset_hz, step_ema)` at the end of each
  candidate; surface in the scorecard. Look for callsigns that
  consistently produce large `step_ema` — those are the drifting
  rigs and the operator population that this mechanism helps.
- Bench gate: new hypothesis bank entry. Suggest `hb-223` or
  next-free. Expected effect: improved decode rate on drifting
  stations; small but measurable on a corpus that contains them.
  May correlate with low-quality rigs and field-day / mobile
  operations.
