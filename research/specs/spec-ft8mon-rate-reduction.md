# Algorithm spec: Per-thread band-of-interest rate reduction (`reduce_how=2`)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `reduce_rate()` around lines 630-709, top-of-`go()`
  call site around lines 740-795, `fbandpass()` around lines 2228-2285,
  `down_v7()` / `down_v7_f()` around lines 2287-2345
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

ft8mon decodes in N parallel worker threads, each thread handed a
slice of the audio band (`min_hz`..`max_hz` divided into `nthreads`
strips with `overlap = 20 Hz`). The per-thread audio stream is still
delivered at the full input sample rate (12 kHz), which is wasteful:
the strip is typically only 400-500 Hz wide. ft8mon performs a
two-stage rate reduction. The **first stage** (`reduce_rate` at the
top of `go()`) drops every thread from 12 kHz to the smallest rate
whose Nyquist still covers the strip plus a guard band, using a single
forward FFT, a frequency-domain cosine-tapered bandpass shape, a
bin-shift to translate the band down toward DC, and one inverse FFT.
This makes everything downstream — coarse sync, fine sync, soft
demod, subtraction — N times cheaper. The **second stage**
(`down_v7_f`, called per-candidate inside `one()`) shifts the
candidate's center frequency down to 25 Hz and IFFTs into a 200 sps
buffer with 32 samples/symbol, the canonical working format that the
rest of the decoder is built around.

This is conceptually similar to wsjtr's cached-bandpass downsampler
(also specced in `research/specs/spec-wsjtr-cached-bandpass-downsampler.md`)
but the mechanism is different. wsjtr does time-domain filtering with
a cached impulse response; ft8mon does it entirely in the frequency
domain with a single large FFT shared across all candidates in a
thread.

## Algorithm description (PROSE ONLY)

### Stage 1 — per-thread band reduction (`reduce_how = 2`)

#### Inputs
- The full-slot audio at the input sample rate `arate` (e.g. 12000 Hz),
  trimmed to a FFTW-friendly size (one of 18000, 18225, 36000, 36450,
  54000, 54675, 72000, 72900, 144000, 145800, 216000, 218700 samples).
- A target frequency window `[hz0, hz1]` for this worker thread,
  derived from the thread's slice of `min_hz..max_hz` plus `overlap`
  on each interior edge and an `reduce_extra` guard pad on each side.
- A reduced target sample rate `brate`, chosen as the smallest 100 Hz
  multiple whose Nyquist times a safety factor (`nyquist = 0.925`)
  still covers the strip width plus `2 × go_extra + 50` Hz. Reduction
  only fires if `brate < arate × 0.75`.

#### Outputs
- A new time-domain buffer at `brate` samples/second whose spectrum
  contains the original `[hz0, hz1]` content centered around
  `brate / 4` (the midpoint of the new Nyquist range).
- A scalar `delta_hz` reporting how far the band was translated down,
  so the caller can re-bias all downstream frequency references
  (including any list of previously-decoded signals — see the
  `prevdecs` spec for the cross-slot hook).

#### Steps

1. **Compute the FFT** of the entire trimmed slot buffer (one large
   forward FFT). Its bin spacing is `bin_hz = arate / alen`.

2. **Apply the cosine-tapered bandpass shape** (`fbandpass`) in the
   frequency domain. The shape is defined by four corner frequencies
   `(low_outer, low_inner, high_inner, high_outer)`:
   - Bins with `ihz <= low_outer` or `ihz >= high_outer` are zeroed.
   - Bins between `low_outer` and `low_inner` are scaled by a linear
     rising ramp from 0 to 1. (The source contains a commented-out
     cosine ramp variant; the linear one is the default.)
   - Bins between `low_inner` and `high_inner` are passed through
     unchanged (multiplied by 1.0).
   - Bins between `high_inner` and `high_outer` are scaled by a
     linear falling ramp from 1 to 0.

   For stage 1, the corners are derived from `[hz0, hz1]` widened by
   `reduce_extra` on each side; the shoulder width is either
   `reduce_shoulder` (if positive) or `brate × reduce_factor` (default
   `reduce_factor = 0.25`), clamped to stay outside the inner passband.

3. **Bin-shift toward DC.** The integer shift amount is `delta = omid - nmid`,
   where `omid = round((hz0 + hz1) / 2 / bin_hz)` and `nmid = round((brate / 4) / bin_hz)`.
   Build a new bin vector of length `blen/2 + 1` (where `blen = round(alen × brate / arate)`)
   by reading from index `i + delta` of the bandpassed input — i.e. the original
   passband is recentered around bin `nmid`, i.e. around `brate / 4`.
   Out-of-range source indices are treated as zero (no circular wrap).

4. **Inverse FFT** the shifted bin vector to produce a time-domain
   buffer of length `blen` at the reduced rate. This is the per-thread
   working audio for the entire rest of the decode pipeline.

5. **Report `delta_hz = delta × bin_hz`** so the caller can re-bias
   `min_hz_`, `max_hz_`, and every previously-decoded signal's
   frequency before further processing.

### Stage 2 — per-candidate downsample to 200 sps (`down_v7_f`)

#### Inputs
- The cached FFT of the post-stage-1 audio buffer (`bins` argument —
  computed once per pass at the top of `go()` and reused for every
  candidate, so no per-candidate FFT cost beyond the inverse).
- The candidate's frequency in Hz (relative to the post-stage-1 band).
- The post-stage-1 buffer length `len` and rate `rate_`.

#### Outputs
- A time-domain buffer at exactly 200 samples/second (32 samples/symbol
  at the FT8 6.25 Hz tone spacing) whose spectrum is centered such that
  the candidate sits at 25 Hz (bin 4 of the 0..100 Hz Nyquist range).

#### Steps

1. **Compute the integer bin-shift** `down = round((hz - 25) / bin_hz)`
   where `bin_hz = rate_ / len`. This translates the candidate from its
   original location to 25 Hz.

2. **Apply that shift** by reading bin `i + down` from the cached FFT
   into bin `i` of a new bin vector. Out-of-range source indices
   produce zero (not circular wrap).

3. **Cosine-taper the shifted bins** with `fbandpass`. Corners:
   - `low_inner  = 25.0 - shoulder200_extra` (default 25.0)
   - `low_outer  = max(low_inner - shoulder200, 0)` (default 15.0)
   - `high_inner = 75 - 6.25 + shoulder200_extra` (default 68.75)
   - `high_outer = min(high_inner + shoulder200, 100)` (default 78.75)

   The passband 25 Hz to 68.75 Hz covers exactly the 8 tones of an
   FT8 symbol (25.0, 31.25, …, 68.75 Hz), with cosine shoulders of
   width `shoulder200 = 10 Hz` on each side.

4. **Truncate the bin vector** to the first `blen/2 + 1` entries,
   where `blen = round(len × 200 / rate_)`, and inverse FFT to a
   time-domain buffer of length `blen`. Result is 200 sps audio.

5. **Convert the candidate's slot-relative sample offset** from
   post-stage-1 rate to 200 sps by `off200 = round(off × 200 / rate_)`.

### Numerical constants (facts, not expression)

Stage 1:
- `reduce_how = 2` — frequency-domain bandpass + shift + IFFT path.
  `reduce_how = 3` (hard mask, no taper) is the simpler alternative
  and is the source of the "no-taper baseline" comparison.
- `reduce_factor = 0.25` — shoulder width as fraction of target rate
  when `reduce_shoulder <= 0`.
- `reduce_extra = 0` — extra guard pad widening the passband.
- `nyquist = 0.925` — usable fraction of new Nyquist (5-15% rolloff
  reserved for shoulders).
- `go_extra = 3.5 Hz` — coarse-sync search frequency pad.
- `overlap = 20 Hz` — inter-thread overlap on shared band edges.
- `overlap_edges = 0` — when 1, also overlap the outermost edges.

Stage 2:
- `down_v7_f` target rate = 200 sps, 32 samples/symbol.
- `shoulder200 = 10 Hz` — cosine shoulder on each side of passband.
- `shoulder200_extra = 0 Hz` — additional inner-edge widening.
- Passband corners: 15 / 25 / 68.75 / 78.75 Hz.

### Edge cases
- **Out-of-range bin reads** during the shift use a zero fill rather
  than wrapping. Without this, low-frequency candidates near band
  edges would alias high-frequency content into the passband.
- **`reduce_factor`-derived shoulders** can collapse to zero width if
  the passband is wider than half the new rate; the source clamps via
  `std::min(hz00, hz0)` and `std::max(hz11, hz1)`.
- **Buffer trimming** — `go()` rounds the input length to the
  nearest FFTW-friendly size before any reduction, to keep the planner
  cache hit rate high. This costs at most a few percent of slot length.
- **Padding for tplus tail** — if the slot buffer would not contain
  `tplus × rate_` samples past the nominal end, ft8mon pads with
  randomly-sampled audio from elsewhere in the slot (not zeros, which
  would create artificial spectral edges that confuse soft demod).
  Padding length is rounded up to a whole second.
- **`prevdecs` re-biasing** — every cached previous-slot decode must
  have its `(hz0, hz1)` adjusted by `delta_hz` after stage 1, or
  subtraction will miss its target.

## Conflict with pancetta's existing mechanisms

Pancetta's current downsampler (per `pancetta-ft8/src/decoder/` and
`pancetta-dsp/`) operates at the full input sample rate without
per-thread band reduction. The wsjtr-style cached-bandpass downsampler
spec already on file (`spec-wsjtr-cached-bandpass-downsampler.md`)
covers a related mechanism with a different implementation strategy
(time-domain FIR vs frequency-domain IFFT). The choice between them
is implementation-driven:

- ft8mon's stage 1 is a single FFT shared across one thread's entire
  decode pass. If pancetta already FFTs the full slot for some other
  reason, this is essentially free.
- wsjtr's cached FIR is a time-domain convolution with a one-time
  designed FIR; cheaper if pancetta doesn't already have a full-slot
  FFT but more expensive if it does.

The two-stage structure (per-thread reduction first, per-candidate
200-sps reduction second) is independent of the choice of stage-1
mechanism and is the load-bearing part of the spec: the per-candidate
`down_v7_f` step at 32 samples/symbol is what every downstream
operation in ft8mon (sync, soft demod, subtraction) is sized around.

If pancetta currently does per-candidate frequency shifts without
downsampling, switching to 200 sps would shrink the per-symbol FFT
from 1920 samples (12 kHz) to 32 samples (200 Hz) — a 60× cost
reduction in the soft-demod step — at the cost of a slight loss of
sync precision below `bin_hz = 6.25 Hz`. ft8mon recovers that
precision via the sub-bin Costas sweep (already specced as
`spec-ft8mon-sub-bin-costas.md`).

## Estimated Rust port effort
- ~300-450 LOC total (`fbandpass` ~50 LOC, `reduce_rate` ~100 LOC,
  `down_v7_f` ~80 LOC, plus integration tests).
- 2-3 sessions: (S1) `fbandpass` + golden-test against a synthesized
  sinusoid passing through known corner frequencies; (S2) stage 1
  wired in front of the existing per-thread coarse sync, with a unit
  test that a synthetic candidate at any frequency in the strip is
  preserved within ~0.1 dB and translated by exactly `delta_hz`;
  (S3) stage 2 swapped in front of fine sync, with a regression
  test that the existing 12 kHz decoder paths still produce
  bit-identical results when `do_reduce = 0`.

## Implementation notes for the implementer thread

- The cosine-tapered bandpass is the load-bearing primitive. Recommend
  a single function with the signature
  `fbandpass(bins: &mut [Complex<f32>], bin_hz: f32, corners: (f32, f32, f32, f32))`
  that mutates in place. Keep the linear-ramp shoulders as the default
  (matches ft8mon's `#if 1` arm); the cosine variant is in the source
  as a `#if 0` arm but does not appear to ship enabled.
- The `nice_sizes` list of FFTW-friendly slot lengths is not part of
  the algorithm — it's a planner-cache optimization. Pancetta's
  RustFFT planner does not have the same mutex contention, so this can
  be omitted at first and added only if profiling shows a hot planner.
- For stage 2, every per-candidate iteration shares the same cached
  full-buffer FFT — make sure the implementation takes `&[Complex<f32>]`,
  not a fresh FFT per candidate. This is the single biggest savings
  in the whole structure.
- Tier interaction: stage 1 always pays off (per-thread strip is much
  narrower than full band); stage 2 also always pays off. No
  Fast/Moderate/Slow gating needed; this is pure win for all tiers.
- The `delta_hz` re-bias must propagate to:
  - The `min_hz_` / `max_hz_` per-thread bounds
  - Any cross-slot `prevdecs` entries (see related spec)
  - Eventually back through the callback so emitted decode frequencies
    are in the original input space, not the reduced one. ft8mon
    accumulates this in `down_hz_` and adds it at emit time.
- Regression baseline: `reduce_how = 3` (hard zero outside passband)
  is the no-shoulder fallback. The cosine shoulders specifically
  prevent LDPC-disturbing ringing at the band edges. A useful unit
  test: synthesize a signal near a band edge with `reduce_how = 2`
  vs `reduce_how = 3` and confirm that the time-domain output of the
  hard-mask version has visible Gibbs ringing while the cosine version
  does not.
- This spec composes with `spec-ft8mon-sub-bin-costas.md`: stage 1's
  cached full-slot FFT is the same FFT that the sub-bin Costas sweep
  re-uses (see the `bins = one_fft(samples_, ...)` line at the top of
  `go()`'s per-pass loop). One FFT, multiple consumers.
