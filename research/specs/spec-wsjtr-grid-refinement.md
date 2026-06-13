# Algorithm spec: 5×5 (dt, freq) grid refinement via time-domain Goertzel

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File path (traceability only, NOT quoted): `crates/jt9r/src/sync.rs`,
  `refine_candidate` and its callee `full_costas_score`; the underlying
  per-tone power evaluator `tone_power` falls back to a Goertzel filter when
  it cannot use FFT.
- Companion doc: `docs/jt9r.md` (architecture summary, "Refinement Process"
  section)
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

The sync candidate search operates on a spectrogram with coarse resolution:
each spectrogram frame steps `NSTEP = 480` audio samples (40 ms at 12 kHz) in
time, and each frequency bin is one tone spacing (~6.25 Hz) wide. A signal
that lies between two grid points is detected, but its `(dt, freq)` estimate
is biased toward whichever grid point happened to score best — typically with
±20 ms time error and ±3 Hz frequency error.

That bias matters in two places downstream:

1. **Soft-bit demodulation**: the LDPC decoder expects symbols sampled at
   precisely the right time and frequency. Misalignment by a few hundred Hz
   spreads tone power across multiple bins and degrades soft-bit SNR.
2. **Candidate dedup and joint-decode**: when two transmitters at adjacent
   frequencies both produce candidates, accurate `(dt, freq)` discrimination
   is required to keep them as separate decodes instead of collapsing them.

Grid refinement re-evaluates each accepted candidate on a small `5 × 5`
search grid centred on its coarse estimate, using a finer-resolution metric.
The "finer-resolution" trick is to compute tone power using **time-domain
Goertzel filtering** instead of FFT bin lookup: Goertzel is a single-bin DFT
that can be tuned to *any* frequency, not just the FFT grid. This gives
sub-bin precision in frequency. Combined with sub-step offsets in time, the
candidate's `(dt, freq)` estimate is refined to roughly half-bin / half-step
precision.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- A single accepted `SyncHit` from the candidate stage, containing:
  - `dt0`: coarse time offset (in spectrogram frames; multiply by NSTEP to
    get sample offset).
  - `f0`: coarse audio centre frequency (in Hz).
  - The original sync metric value (used for comparison; replaced if the
    refinement finds a better point).
- The original 12 kHz audio sample buffer (NOT the spectrogram — refinement
  operates on samples directly).
- The Costas pattern constant and the 79-symbol FT8 frame structure.

### Outputs

- The same `SyncHit` with `dt0`, `f0`, and the sync metric replaced by the
  best point on the 5×5 grid.

### Steps

1. **Build the offset grid.** Generate 25 candidate `(d_dt, d_f)` offsets
   from the cross product `{-2, -1, 0, 1, 2} × {-2, -1, 0, 1, 2}`. The dt
   axis is measured in steps of `NSPS / 4 = 480` audio samples (40 ms at
   12 kHz). The freq axis is measured in steps of `TONE_SPACING_HZ / 2`,
   which equals approximately 4.6875 Hz (half a tone). So the grid covers
   ±80 ms in time and ±9.375 Hz in frequency.

2. **For each of the 25 grid points**, compute a refined sync score via
   `full_costas_score`:
   - Form the candidate's sample offset: `start_sample = dt0 * NSTEP +
     d_dt * 480` (treating `dt0` here as the coarse spectrogram-frame index
     scaled into samples).
   - Form the candidate's frequency: `f_candidate = f0 + d_f * 4.6875`.
   - For each of the three Costas blocks (symbol offsets 0, 36, 72) and
     each of the 7 symbols within that block:
     a. Locate the symbol's audio sample window: a contiguous span of
        NSPS = 1920 samples starting at
        `start_sample + (block_offset + symbol_index_within_block) * NSPS`.
     b. For that 1920-sample window, evaluate the power at 8 candidate
        tones via Goertzel filtering. The 8 tones are
        `f_candidate + t * TONE_SPACING_HZ` for `t in 0..8` (the full FT8
        8-FSK alphabet at this candidate's centre frequency).
     c. Identify which tone the Costas pattern says should be the
        *signal* tone at this symbol position. Read off its Goertzel power.
        Divide by the mean of all 8 Goertzel powers at this symbol.
        Accumulate the ratio.
   - After all 21 symbols (3 blocks × 7 symbols) are accumulated, divide
     the sum by 21 (i.e. take the mean ratio across all Costas symbols).
     This is the `full_costas_score` for this grid point.

3. **Pick the best grid point.** Scan all 25 scores; take the `(d_dt, d_f)`
   with the largest score.

4. **Update the hit.** Replace `dt0` with the refined sample offset (or
   equivalent frame-index expression), `f0` with the refined Hz, and the
   sync metric with the best score.

### Goertzel filter (single-bin DFT at arbitrary frequency)

The Goertzel filter is the workhorse here. Its purpose is to compute the
power of an audio buffer at a *specified* frequency, with sub-bin
precision relative to the would-be FFT grid. It is a second-order IIR.

The standard Goertzel update (fact, not derivation):

- Choose target frequency `f_target` and buffer length `N` (here N = NSPS =
  1920).
- Compute the normalized bin index `k = round((N * f_target) / SAMPLE_RATE)`
  — but unlike FFT, k does **not** have to be an integer for the filter to
  be defined; the wsjtr source rounds for the coefficient calculation.
- Compute the coefficient `coeff = 2 * cos(2 * pi * k / N)`.
- Initialize `s_prev = 0`, `s_prev2 = 0`.
- For each sample `x[n]` in the buffer:
  `s = x[n] + coeff * s_prev - s_prev2; s_prev2 = s_prev; s_prev = s.`
- After processing all N samples, the power at the target frequency is
  `s_prev^2 + s_prev2^2 - coeff * s_prev * s_prev2` (the standard Goertzel
  power formula).

The refinement uses this filter 8 times per symbol (once per candidate
tone) × 21 symbols × 25 grid points = **4200 Goertzel evaluations per
candidate**. With a 1920-sample buffer per evaluation, the total per-candidate
cost is on the order of 8 million multiply-add operations. This is not free,
which is why refinement is gated to *accepted* candidates only, after the
1000-candidate cap.

### Numerical constants (facts, not expression)

- Grid dimensions: 5 × 5 = 25 points.
- dt step size: NSPS / 4 = 480 audio samples per step (40 ms at 12 kHz).
- dt grid span: ±2 steps = ±80 ms.
- df step size: TONE_SPACING_HZ / 2 ≈ 4.6875 Hz per step.
- df grid span: ±2 steps = ±9.375 Hz.
- Goertzel window length: NSPS = 1920 samples (one full symbol per
  evaluation).
- Tones evaluated per symbol: 8 (the full FT8 8-FSK alphabet).
- Symbols evaluated per grid point: 21 (3 Costas blocks × 7 symbols).
- Total Goertzel evaluations per refined candidate: 25 × 21 × 8 = 4200.
- Sample rate: 12 kHz.

### Edge cases

- **Edge of audio buffer**: if `start_sample + d_dt * 480 + (block_offset +
  symbol) * NSPS` falls outside the audio buffer (negative or past the end),
  the source treats the missing samples as zero. The Goertzel power for that
  symbol is then dominated by whatever in-bounds samples exist, biasing the
  score lower for that grid point. This is the correct behaviour: out-of-window
  grid points naturally lose the comparison.
- **Coarse hit already near global optimum**: the `(0, 0)` grid point wins
  the comparison and the hit is unchanged in `(dt, freq)` but its metric is
  *recomputed using Goertzel instead of FFT*. This usually slightly raises
  the metric (cleaner per-tone estimate), which can change downstream
  ranking. Expected and acceptable.
- **Two refined candidates that converge to the same `(dt, freq)`**: the
  source does **not** dedup these. Downstream message-text dedup handles it.
- **Goertzel numerical stability**: at N = 1920 with `f32` arithmetic the
  filter is stable for the audio passband (200–3000 Hz). No special-casing
  needed. If pancetta uses `f64` internally for IIR stability that is fine.

## Conflict with pancetta's existing mechanisms

Pancetta's current candidate refinement is FFT-bin-only: the candidate's
`(dt, freq)` is whatever the coarsest grid point reported. There is no fine
search and no Goertzel pass. So this mechanism is purely additive at the
search level.

Possible conflicts to think through:

1. **Cost budget**: 4200 Goertzel evaluations per accepted candidate, times
   ~1000 candidates per slot worst case, is ~4M Goertzel evaluations per
   15-second slot. At 1920 samples each, that is ~8 billion MACs per slot
   raw, which is too much on a Slow tier and tight on Moderate. Mitigations:
   - Limit refinement to top-K candidates (e.g. top 100 by normalized sync
     metric). The wsjtr source does not gate, but pancetta should.
   - Gate the entire refinement step behind the hb-216 hardware tier:
     enable on Fast, disable on Slow, top-K-limit on Moderate.
   - Use the `scoped_fast_path` Arc<AtomicBool> so the gate is hot-loadable.
2. **Interaction with hb-091 scoped fast path**: hb-091 already opts out of
   *some* sync work in tight-budget regimes; the refinement gate should sit
   in the same conditional family.
3. **Interaction with the LDPC stage**: a refined `(dt, freq)` feeds the
   downstream soft-bit demodulator. If the demodulator currently assumes
   integer FFT-bin alignment, it must be taught to use the refined fractional
   frequency. Inspect `pancetta-ft8/src/decoder.rs` for the demod path and
   confirm it accepts an arbitrary float frequency, not just a bin index.
4. **Interaction with multi-stream TX**: refinement makes neighbouring
   candidate frequencies more accurate, which helps the `SmartFrequencyAllocator`
   logic when negotiating non-overlapping TX slots. Mildly positive.
5. **Effect on hb-217 RR73 fix**: orthogonal; the RR73 fix is in the
   parser, refinement is in the candidate search. Should not interact.
6. **Capture-effect interaction (hb-100, hb-115, hb-218 line)**: positive.
   Better `(dt, freq)` discrimination makes it easier to tell two
   capture-locked transmitters apart at the candidate stage, which is the
   foundation of the joint-decode line of work.

## Estimated Rust port effort

- Goertzel helper: ~25 LOC, standalone, in a new private fn somewhere in
  `pancetta-ft8`. Trivially testable against a `cos(2*pi*f*t)` synthetic
  buffer.
- `full_costas_score` helper: ~80 LOC (loop over 3 blocks × 7 symbols × 8
  tones, accumulate Goertzel powers, take ratios, average).
- `refine_candidate` wrapper: ~30 LOC (25-point grid, pick best).
- Plumbing into the existing candidate-acceptance loop: ~20 LOC.
- Hardware-tier gate: ~15 LOC (read scoped_fast_path or a new
  `enable_refinement` atomic).
- Unit tests: ~100 LOC (Goertzel correctness, full_costas_score against a
  known-good synthetic burst, refine_candidate finds the right grid point
  when the coarse hit is at `(-1, +1)`).
- Total: ~270 LOC.
- 2–3 iter sessions: 1 for Goertzel + score helper, 1 for plumbing + tier
  gate, 1 for hard-200 eval + cost budget validation.

## Implementation notes for the implementer thread

- Splice point: pancetta's candidate acceptance currently lives near
  `pancetta-ft8/src/decoder.rs` around the area where SyncHit-equivalent
  objects are emitted (the file structure may differ; search for the sync
  threshold check). The refinement step should sit **after** the sync
  threshold but **before** the soft-bit demod stage.
- Cost gating must happen *at the call site*, not inside `refine_candidate`,
  so the helper can be unit-tested in isolation. Suggested call shape:
  walk accepted candidates sorted by normalized sync descending, refine the
  first K (K ≈ 100 on Moderate, K ≈ 1000 on Fast, K = 0 on Slow), pass
  un-refined hits through unchanged on Slow.
- The Goertzel helper takes `(samples: &[f32], f_target_hz: f32, sample_rate:
  f32)` and returns power as `f32`. Keep it allocation-free; pre-compute
  `coeff` once per call.
- For the 8-tone-per-symbol evaluation, pre-compute the 8 `coeff` values
  once per grid point and reuse across all 21 symbols. This is roughly a 5×
  cost reduction over computing them per-symbol.
- Avoid recomputing the symbol-window slice indices inside the inner loop;
  hoist them.
- Unit-test Goertzel against a synthetic single-frequency sinusoid: window
  filled with `cos(2*pi*f*n/fs)`, expected power should be the analytical
  value `(N/2)^2` for amplitude 1.0, to within 1%.
- Unit-test refinement by injecting a synthetic FT8 burst at a known
  off-grid `(dt, freq)` (e.g. coarse hit at `(0, 0)` but truth at `(+1.3,
  -0.8)` grid units), and confirming the refined hit lands at `(+1, -1)` —
  the nearest grid point to truth.
- The doc summary's "(0.04s, 4 Hz) proximity dedup" is the *grid step
  sizes*, not a deduplication mechanism. Do not implement it as a dedup;
  the source does not.
- Cite as `wsjtr-inspired` in the journal entry.
