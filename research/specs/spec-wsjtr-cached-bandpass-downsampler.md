# Algorithm spec: cached 192k-FFT + per-candidate bandpass + 3200-point iFFT downsampler

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- File paths (traceability only, NOT quoted):
  - `crates/jt9r/src/wsjt_ft8.rs`, the `Ft8DecodeContext` and `Ft8Downsampler`
    structs and the `Ft8Downsampler::downsample` method.
  - `crates/jt9r/src/decoder.rs`, `Decoder::decode_single_pass_ft8` shows the
    "wrap audio in an Arc, decode candidates in parallel" pattern that gives
    the downsampler its sharing semantics.
  - The wsjtr authors describe this as a port of WSJT-X's
    `lib/ft8/ft8_downsample.f90`.
- Companion docs: `docs/jt9r.md` ("Downsampling" subsection) and
  `docs/wsjtr.md`.
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

Pancetta's decoder operates on a magnitude spectrogram throughout the
decode pipeline: candidate sync, fine sync, soft-bit extraction. The
spectrogram is a stack of short overlapping FFTs (currently sized so each
frequency bin is about one tone wide), and downstream stages read tone
power by indexing bins directly. The intentional simplification is that
phase is discarded.

WSJT-X's approach is fundamentally different. Instead of trusting a
short-window spectrogram, it builds a **complex baseband** representation
of each candidate: a per-candidate stream of complex samples centred on
the candidate frequency, narrowband-filtered to roughly the FT8 signal
width, at a sample rate of 200 Hz (32 samples per symbol). All of fine
sync, frequency tweak, time refine, and the per-symbol 8-tone FFT then
run on this complex baseband stream — not on the global spectrogram.

The complex baseband retains phase, which enables coherent matched-filter
sync (multiply by the conjugate of the Costas tone vector and sum, then
square the magnitude) — a strictly stronger statistic than the
incoherent magnitude correlation the spectrogram supports. It also
rejects spectral interference from other FT8 signals at adjacent
frequencies, because the per-candidate band is only ~62.5 Hz wide; signals
outside that band contribute nothing to that candidate's decode. On a
crowded contest-class band this is a huge SNR-effective improvement.

The clever bit, and the reason this is practical at decode time, is that
the per-candidate bandpass is implemented in the **frequency domain** off
a single cached 192,000-point FFT of the entire 15-second audio buffer.
The forward FFT is computed once per 15-second decode window (and once
per pass in multi-pass mode, on the residual). Each candidate then walks
the cached spectrum, copies its band into a 3,200-point buffer, applies
edge tapers, circularly shifts so the carrier lands at DC, runs a single
3,200-point inverse FFT, scales by a unitary factor, and emerges with
2,812 complex baseband samples at 200 Hz.

The MEMORY-cited "single biggest documented sensitivity gap between
pancetta and WSJT-X" is exactly this missing complex-baseband layer.

## Algorithm description (PROSE ONLY — no code)

### Inputs

- A 15-second buffer of audio samples at 12 kHz, real-valued. Conventionally
  exactly 180,000 samples (15 * 12,000). Shorter buffers are zero-padded to
  this length; longer buffers are truncated.
- A list of candidate carrier frequencies (Hz) emerging from the sync
  candidate stage. Each candidate is a single `f64`/`f32` Hz value in the
  audio passband (nominally 200 Hz to 4007 Hz).

### Outputs (per candidate)

- A complex baseband buffer of 2,812 samples at 200 Hz sample rate, centred
  on the candidate carrier frequency. That is 32 samples per FT8 symbol
  (since the FT8 symbol rate is 6.25 Hz, and 200 / 6.25 = 32). The buffer
  covers roughly 14 seconds of decoded duration (2,812 / 200 ≈ 14.06 s),
  which is more than the 79-symbol FT8 burst length (~12.64 s) and leaves
  room for the fine-sync time search.

### Construction phase (once per 15-second decode window)

1. **Zero-pad / truncate** the input audio to exactly 180,000 samples. If
   the input is shorter, the tail is zero-padded; if longer, it is
   truncated.
2. **Promote to complex**: build a buffer of length NFFT1 = 192,000 complex
   numbers. For the first 180,000 entries, set the real part to the audio
   sample and the imaginary part to zero. For the remaining 12,000 entries
   (slots 180,000 through 191,999), leave both parts at zero. This
   zero-padding lengthens the FFT to a round 192,000 (which is
   2^7 * 3 * 5^3 = 192000 = 60 * 3200, chosen so a downstream length-3,200
   inverse FFT exactly corresponds to a 1/60 decimation in time).
3. **Run a single forward FFT of length 192,000** over this complex buffer.
   The output is a length-192,000 complex spectrum. This spectrum is
   **cached** for the entire lifetime of the decode context (one 15-second
   window). All candidate downsamples in this window read from the same
   cached spectrum.
4. **Pre-build the inverse FFT plan** of length 3,200 (NFFT2 = 3200 =
   NFFT1 / 60) and the raised-cosine taper (see step 6).

The cost of step 3 is the dominant work in construction: one 192,000-point
complex FFT per window. The wsjtr author notes this is amortized across
all candidates in the window. A typical window has tens to a few hundred
candidates, so the amortized per-candidate cost of the forward FFT is
sub-millisecond.

### Per-candidate downsample

For each candidate frequency `f0_hz`:

1. **Compute the FFT bin spacing**:
   `df = SAMPLE_RATE / NFFT1 = 12000 / 192000 = 0.0625 Hz` per bin. This is
   1/100 of the FT8 symbol rate (baud = 6.25 Hz), which is what gives this
   approach its sub-Hz frequency centering precision.

2. **Compute the centre bin index** `i0 = round(f0_hz / df)`. This is the
   bin in the cached spectrum that holds the strongest tone energy for the
   candidate.

3. **Compute the upper and lower band edges**:
   - Upper edge frequency: `ft = f0_hz + 8.5 * baud`, clamped above at
     `NFFT1 / 2 = 96000` bins.
   - Lower edge frequency: `fb = f0_hz - 1.5 * baud`, clamped below at
     bin 1.
   - In bins:
     `it = round(ft / df)` (clamped to ≤ NFFT1/2),
     `ib = round(fb / df)` (clamped to ≥ 1).
   - The asymmetric span (-1.5 baud below the carrier, +8.5 baud above)
     reflects the FT8 8-FSK alphabet: tones run from 0 to 7 above the
     carrier, so the carrier itself is at the bottom edge of the
     alphabet; the lower edge `-1.5 * baud` is purely a guard band, while
     the upper edge `+8.5 * baud` covers tones 0..7 (which span
     0..7 baud) plus a guard band of 1.5 baud above tone 7.
   - In Hz, that's about -9.375 Hz to +53.125 Hz around the carrier — a
     band roughly 62.5 Hz wide.

4. **Extract the band into the iFFT buffer**: allocate a length-NFFT2 =
   3200 complex buffer initialized to zero. Walk the cached spectrum from
   bin `ib` to bin `it` inclusive (which is at most ~1,000 bins for a
   62.5 Hz-wide band), copying each cached complex value into successive
   slots `0`, `1`, `2`, ... of the iFFT buffer. Stop at the first of
   (a) running out of source bins to copy or (b) filling all NFFT2 slots
   (the latter should never happen with the documented edge formulas; the
   wsjtr source defensively checks).

5. **Apply edge tapers** to reduce spectral leakage. Pre-build a length-101
   raised-cosine taper:
   `taper[i] = 0.5 * (1 + cos(i * pi / 100))` for i in 0..=100. Values run
   from 1.0 at i=0 to 0.0 at i=100.
   - **Lower edge**: multiply the first 101 (or fewer if the band is
     narrower) entries of the iFFT buffer by `taper[100 - n]` for n in
     0..=100. This is the taper indexed in reverse — slot 0 gets
     `taper[100] = 0`, slot 100 gets `taper[0] = 1`. So the very first
     bin is zeroed and the taper ramps up smoothly to 1.0 over 100 bins.
   - **Upper edge**: multiply the last 101 entries by `taper[n]` for n in
     0..=100. Slot `k-101` gets `taper[0] = 1`, slot `k-1` gets
     `taper[100] = 0`. So the taper ramps from 1.0 down to 0 over the last
     101 bins.
   - In both cases, only apply the taper if the band is wider than 100
     bins; for narrow bands (rare in practice), the asymmetric handling
     described in the source applies.

6. **Circularly shift** the buffer so the carrier bin lands at index 0
   (DC). The shift amount is `shift = (i0 - ib) mod NFFT2`. Rotate the
   buffer left by `shift` slots. After the shift, the carrier component
   is at index 0 of the iFFT buffer, the upper sideband (tones 0..7
   above carrier) is at low positive indices, and the (mostly empty)
   tapered guard band wraps around to high indices. This makes the
   inverse FFT produce a baseband-centred complex output.

7. **Run the length-NFFT2 = 3200 inverse FFT** over the buffer. Output is
   2,812 complex samples (with NFFT2 - 2812 = 388 samples of "wraparound"
   guard at the end; downstream consumers only use the first 2,812).

8. **Scale by the unitary normalization factor**
   `fac = 1 / sqrt(NFFT1 * NFFT2) = 1 / sqrt(192000 * 3200) = 1 / sqrt(614,400,000) ≈ 4.04e-5`.
   Multiply every output sample by `fac`. The factor is chosen so that
   forward + inverse FFT chain together preserves energy (Parseval),
   independently of the FFT library's normalization convention.

The result is a length-NP2 = 2812 complex array at 200 Hz sample rate,
with the candidate carrier at DC, FT8 tones 0..7 at the 8 lowest positive
frequencies (with bin spacing ~0.0625 Hz at 200 Hz sample rate ×
NFFT_symbol=32 ≈ 6.25 Hz per tone), and adjacent FT8 signals heavily
attenuated by the bandpass.

### Numerical constants (facts, not expression)

- Audio sample rate: 12,000 Hz.
- Audio buffer length per window: NMAX = 15 * 12000 = 180,000 samples.
- Cached forward FFT length: NFFT1 = 192,000.
- Per-candidate inverse FFT length: NFFT2 = 3,200.
- Downsample ratio: NDOWN = NFFT1 / NFFT2 = 192000 / 3200 = 60.
- Output complex baseband sample rate: FS2 = FS / NDOWN = 12000 / 60 = 200 Hz.
- Output usable length: NP2 = 2,812 complex samples.
- Samples per symbol at baseband: 32 (= 200 / 6.25).
- FT8 symbol rate (baud): 6.25 Hz.
- Bin spacing of cached FFT: df = 0.0625 Hz.
- Upper band edge offset: +8.5 * baud = +53.125 Hz above carrier.
- Lower band edge offset: -1.5 * baud = -9.375 Hz below carrier.
- Resulting band width: ~62.5 Hz (10 baud).
- Taper length: 101 samples, raised-cosine, `0.5 * (1 + cos(i * pi / 100))`.
- Unitary scaling factor: `1 / sqrt(NFFT1 * NFFT2) ≈ 4.04e-5`.
- Costas pattern for sync (used by downstream `sync8d`-style consumer):
  [3, 1, 4, 0, 6, 5, 2] over 7 symbols.

### Edge cases

- **Audio shorter than 15 seconds**: zero-pad to 180,000 samples; the
  forward FFT and downsample still work. The trailing zero samples
  contribute nothing to the spectrum.
- **Audio longer than 15 seconds**: truncate to 180,000 samples. (Typical
  in multi-pass scenarios where the buffer may be slightly oversized.)
- **Candidate frequency below baud * 1.5 ≈ 9.375 Hz**: the lower edge
  formula `fb = f0 - 1.5*baud` could go negative or below the DC bin; the
  source clamps `ib >= 1`. In pancetta, the candidate stage's `freq_min`
  default of 200 Hz makes this case impossible.
- **Candidate frequency above (sample_rate/2 - 8.5*baud) ≈ 5946 Hz**: the
  upper edge formula could exceed Nyquist; the source clamps
  `it <= NFFT1/2`. The default `freq_max` of 4007 Hz keeps this safe.
- **Numerical underflow on the unitary scale factor**: the product
  NFFT1 * NFFT2 ≈ 6.14e8 stays well within f32 precision; the sqrt and
  the per-sample multiply are clean.
- **Aliasing**: the asymmetric 10-baud band carries small spectral content
  below the lower edge (the carrier is at the bottom of the alphabet, so
  the candidate signal occupies +0 to +7 baud); the lower 1.5-baud guard
  plus the taper smoothly rolls off ambient noise below the carrier
  before iFFT, preventing wraparound.
- **Multiple candidates at the same exact frequency**: not a special
  case — the downsample is a pure function of `f0_hz`, and calling it
  twice produces identical output. The decode pipeline dedups upstream of
  the downsampler.
- **Numerical precision of the bin-index rounding**: integer rounding of
  `f0_hz / df` produces a frequency-quantization error of up to ±0.03 Hz
  in the centering. Downstream `fine_sync` does a ±2.5 Hz fractional
  frequency tweak that more than absorbs this.

## Conflict with pancetta's existing mechanisms

Pancetta currently has no per-candidate complex baseband stage. The
spectrogram in `pancetta-ft8/src/decoder.rs` is built once per 15-second
window from short overlapping windowed FFTs (~80–160 ms each, depending on
TIME_OSR), and downstream stages read tone power by indexing
`(symbol_t, freq_bin)` directly. All sync, fine sync, and soft-bit metric
generation operate against this magnitude/power spectrogram.

Adopting the cached-bandpass downsampler is a **structural change**, not
an incremental tweak. The downsampler doesn't replace the spectrogram by
itself; it replaces the *post-sync inputs* to fine sync and soft-bit
extraction, while leaving coarse candidate sync on the spectrogram. The
implementation steps:

1. Add a new `Ft8DecodeContext`-equivalent struct in pancetta-ft8 that
   owns (a) the cached 192k-point complex spectrum and (b) the 3200-point
   inverse FFT plan. Build it once per call into the decode pipeline
   immediately after spectrogram construction.
2. For each surviving candidate (after coarse sync candidate selection
   from the spectrogram), call the per-candidate downsample to obtain its
   2812-sample complex baseband.
3. Replace the spectrogram-based fine-sync and soft-bit stages with
   complex-baseband consumers. These are described in companion specs
   (and in the existing pancetta `spec-wsjtr-grid-refinement.md` /
   `spec-wsjtr-sync-norm.md`); at a minimum the consumer needs:
   - A 32-point complex FFT applied per symbol to extract complex tone
     bins (replacing the spectrogram bin lookup).
   - A coherent matched-filter sync (`sync8d`-style) using the complex
     Costas template; this is strictly more sensitive than the magnitude
     sync the spectrogram supports.
   - The multi-block LLR generator (`bmeta`/`bmetb`/`bmetc`) operates on
     these complex bins; pancetta already has the 5-pass LLR family,
     but currently uses the spectrogram magnitudes — it should be moved
     to consume the complex bins. The `(a+b).norm()` and `(a+b+c).norm()`
     joint-symbol amplitudes are the load-bearing primitives.

Concrete conflict-points and mitigations:

1. **Existing spectrogram is not deleted**: keep the spectrogram for the
   coarse sync candidate stage (sync-bc, percentile normalization, etc.).
   The downsampler is added as a second-stage representation per
   candidate, not as a global replacement.
2. **Memory cost**: NFFT1 = 192,000 complex samples = ~1.5 MB (f32 real
   + f32 imag) for the cached spectrum, allocated once per decode window.
   Negligible on Fast/Moderate tiers; track on Slow tier per hb-216 tier
   classifier. Per-candidate iFFT buffer is ~25 KB transient.
3. **Compute cost (forward FFT)**: a single 192,000-point complex FFT
   per window. Using rustfft, this is a few milliseconds on M-class
   silicon. Amortized across N candidates, the per-candidate cost is
   negligible.
4. **Compute cost (per-candidate iFFT)**: a single 3,200-point inverse
   FFT plus a copy + taper + rotate of ~1,000 complex bins per candidate.
   This is well under 1 ms on M-class silicon. Total per-candidate
   downsample is sub-millisecond.
5. **Interaction with hb-091 scoped fast path**: the existing scoped fast
   path opts out of *spectrogram* sync work in tight-budget regimes. The
   downsampler is downstream of candidate selection, so it sees only the
   already-accepted candidates and the hb-091 budget is not directly
   affected. Slow-tier (hb-216) operator should be allowed to disable the
   downsampler if the per-candidate cost is unacceptable; in that case,
   pancetta falls back to its existing spectrogram-based soft-bit path.
6. **Interaction with neural OSD**: orthogonal. The neural OSD operates
   on LLRs; only the *source* of the LLRs changes.
7. **Interaction with hb-103 / hb-058 / hb-062 / hb-217 FP filters**: all
   downstream of decode; orthogonal.
8. **Interaction with the `spec-wsjtr-sync-bc.md` and
   `spec-wsjtr-sync-norm.md` specs already shipped**: those operate on
   the spectrogram (coarse sync stage), which the downsampler does **not**
   touch. They remain in their current form.
9. **Interaction with the `spec-wsjtr-grid-refinement.md` 5x5 Goertzel
   refinement**: complementary. The Goertzel refinement is the
   spec-as-written replacement for "refine the (dt, freq) of a coarse
   candidate without leaving the audio domain"; the downsampler replaces
   "extract symbol amplitudes for soft-bit metrics after the candidate is
   fixed." Order: Goertzel-refine the (dt, freq) (or skip), then
   downsample at the refined frequency, then symbol-extract and LLR.
10. **Multi-pass with subtraction**: when pancetta runs multipass with
    subtraction (the residual-audio path), the cached spectrum is
    **invalidated** between passes (because the audio changed). Each pass
    rebuilds the forward FFT. The cost is paid per pass; in practice this
    is fine because pancetta's multipass is gated for crowded bands.
11. **Determinism**: rustfft produces deterministic output for a given
    plan and input; same audio → same baseband. Required for
    bootstrap-CI policy.

## Estimated Rust port effort

- ~120 LOC for the downsampler struct + per-candidate method, including
  the taper precompute and the unitary scaling. The forward FFT plan and
  the inverse FFT plan are both rustfft one-liners.
- ~30 LOC for the decode-context wrapper that owns the cached spectrum,
  hands out borrowed references to consumers, and exposes the inverse
  FFT plan.
- ~250–400 LOC for the per-candidate consumer (sync8d-style matched
  filter, ctwk_32 frequency-tweak, time refine, symbol extraction). Some
  of this may already exist in pancetta in a different form; an
  implementer audit is required.
- ~150 LOC for adapting the existing 5-pass LLR generator to consume
  complex bins instead of spectrogram magnitudes.
- ~200 LOC unit tests: synthetic single-tone in/out (carrier centering
  precision), known FT8 burst in/out (round-trip to soft-bit, decode
  succeeds), boundary conditions (audio at exactly 180k samples; below;
  above), candidates at the band edges, two candidates in the same
  window (forward FFT cached correctly).
- Total: ~750–950 LOC, ~3–4 iter sessions for implementation + tests + a
  hard-200 eval to confirm the sensitivity gain.

## Implementation notes for the implementer thread

- Use rustfft. Plan the 192,000-point forward FFT and the 3,200-point
  inverse FFT once per decode context (cached in the context struct);
  rustfft's `FftPlanner` caches internally too, so re-planning is cheap
  but explicit plan-stash makes the per-window code clean.
- The 192,000 length is composite-smooth (NFFT1 = 192000 = 2^7 * 3 * 5^3
  = 128 * 1500) and rustfft handles it efficiently. Do not change the
  length to a power of two; the 60:1 downsample ratio is what makes the
  whole approach work.
- The cached spectrum is read-only once built; share it via `Arc<...>`
  or by-reference across rayon parallel candidate processing.
- Pre-compute the taper once at context creation. The taper is a fixed
  array of 101 f32 values.
- Implement the per-candidate downsample as a pure method on the context;
  it borrows `&self` for the cached spectrum and the inverse FFT plan,
  takes `f0_hz` as an argument, and returns a `Vec<Complex<f32>>` (or
  `Box<[Complex<f32>; 2812]>` if you prefer no allocation; the source
  uses a `Vec` for ergonomic reasons).
- Verify with a synthetic test: synthesize a pure complex tone at
  1500 Hz embedded in zero-mean audio, run the downsampler at f0 =
  1500 Hz, confirm (a) the output is approximately a complex sinusoid
  at DC (0 Hz centred), (b) the magnitude is preserved (within the
  scale-factor tolerance), (c) running at f0 = 1500 Hz + 3 Hz produces
  an output complex sinusoid at -3 Hz / 200 Hz cycle rate. Both
  conditions are required for downstream sync to work.
- Verify the asymmetric band: synthesize one strong tone at f0 + 4 baud
  and a weak interferer at f0 + 15 baud (well above the band edge);
  confirm the downsampler suppresses the interferer by at least 30 dB.
- Numerical precision: f32 is sufficient for the FFT chain. The cached
  spectrum is f32 complex; the inverse FFT is f32 complex; the scaling
  factor is f32. No f64 needed anywhere in the downsampler itself.
  (The f64 question is relevant for BP — see the sister spec
  `spec-wsjtr-f64-tanh-bp.md`.)
- Splice into pancetta-ft8 around `pancetta-ft8/src/decoder.rs` at the
  point where candidates have been accepted by the sync stage; add a
  branch that, on Fast/Moderate tiers (per hb-216), routes candidates
  through the complex-baseband path instead of the spectrogram-based
  one. Keep the spectrogram path callable via config for a one-cycle
  fallback during eval.
- Gate behind a config knob `use_complex_baseband: bool` (default
  true on Fast, false elsewhere) for the first hard-200 eval. Once
  recall regression is ruled out, default true everywhere.
- Cite as `wsjtr-inspired` in the journal entry; the underlying
  algorithm originates in WSJT-X's Fortran (`ft8_downsample.f90`), the
  wsjtr source is the inspection target, the pancetta implementation
  is independent.
- Expected eval signal: on hard-200, recall on the strong-signal
  bucket should be approximately flat (the baseline catches strong
  signals already); recall on the medium-SNR bucket (-15 to -19 dB
  band) should rise by ~1–2 dB worth of effective gain — that is
  the gap the wsjtr cross-references attribute to "this is the
  single biggest documented sensitivity gap". Expect ~+10–25 truth
  hits per 200-WAV corpus, concentrated in dense-band conditions
  where adjacent-signal rejection matters most.
- This is plan-sized work, not a 1-session diagnostic. Recommended
  workflow:
  - Session 1: implement and unit-test the downsampler in isolation
    (no decoder integration). Verify with synthetic tones.
  - Session 2: implement the complex-baseband sync8d-style consumer
    and a single LLR pass; integrate into decoder behind a config
    flag; run on a small fixture corpus.
  - Session 3: full 5-pass LLR family; hard-200 eval.
  - Session 4: bootstrap-CI graduation if eval is positive; tier
    integration if not regressing on Slow.
