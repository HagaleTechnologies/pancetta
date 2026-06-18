# hb-243 — Cached-Bandpass Downsampler + Fine Sync (Design Spec)

**Status:** PROPOSED (design only — no decoder code in this commit)
**Date:** 2026-06-18
**Hypothesis:** `research/hypothesis_bank.md` → `hb-243` (PRIORITY 0.55)
**Mode:** FT8 (FT4/FT2 follow as a generalization; FT8 first)
**Effort:** 3–4 research sessions, phased, each phase independently eval-gated
**Default:** OFF behind a new `Ft8Config` flag; production behavior byte-identical until graduated.

---

## 0. License note (clean-room affirmation)

This spec was written from **public algorithm knowledge only**:

- General weak-signal / communications DSP theory: complex (analytic) mixing to
  baseband, decimation, polyphase / FIR low-pass filter design, fractional
  resampling, matched-filter SNR, and fine synchronization by cross-correlation
  peak interpolation (parabolic / quadratic interpolation of a discrete
  correlation peak). These are textbook results (e.g. Oppenheim & Schafer;
  Crochiere & Rabiner, *Multirate Digital Signal Processing*; Proakis; Lyons,
  *Understanding DSP*).
- The **openly published FT8 / WSJT-X algorithm descriptions**: Steven Franke
  (K9AN) & Joe Taylor (K1JT), *"The FT4 and FT8 Communication Protocols,"* QEX
  Jul/Aug 2020; Franke, Somerville & Taylor, *"Open-source soft-decision decoder
  for the JT65 (63,12) Reed–Solomon code"*; the FT8 protocol description on the
  WSJT-X project pages; and the **prose** description of the WSJT-X downsampler
  stage (`ft8_downsample`) as paraphrased in the wsjtr project documentation
  (`docs/jt9r.md §Downsampling`) — a *description*, not source.

**No GPLv3 source code was opened, read, copied, transcribed, or paraphrased**
for this spec — not WSJT-X (`ft8_downsample.f90`, `ft8b.f90`, `sync8.f90`),
not wsjtr, not JTDX, not ft8mon, not MSHV, not JS8Call. The spec is written so
that an implementer who has **never** read any of that source can build it from
this document plus public DSP theory alone. Where a numeric constant in a peer
decoder is not derivable from public theory, this spec lists it as an **open
question with a pancetta-side calibration sweep**, rather than importing a value
from source. The implementer agent must follow the project's clean-room
firewall (`feedback_clean_room_extraction`): implement from THIS spec only.

---

## 1. Goal & expected mechanism

### 1.1 The gap

Pancetta's decode chain operates on a **power spectrogram** for both coarse sync
and symbol extraction:

- `compute_spectrogram_with` (decoder.rs ~3402): a sliding-frame STFT, `nfft =
  sps * FREQ_OSR = 1920 * 2 = 3840`, `subblock_size = sps / TIME_OSR = 960`,
  stored as **dB power** per (time_step, freq_sub, bin). Frequency resolution is
  `6.25 / FREQ_OSR = 3.125 Hz`; time resolution is `960 / 12000 = 80 ms`
  (half-symbol).
- Coarse Costas sync (`costas_sync_search*`, `compute_costas_score*`) finds
  candidates on that grid: `CostasCandidate { time_step, freq_bin, freq_sub,
  sync_score, time_refinement }` (decoder.rs ~1436).
- Symbol extraction reads tone magnitudes off the spectrogram
  (`extract_symbols_from_spectrogram` / `par_extract_symbols_from_spectrogram`),
  or, for strong candidates, re-extracts in the time domain with a per-symbol
  complex-mix + windowed `sps`-point FFT
  (`extract_symbols_complex` / `par_extract_symbols_complex`).

This is sound, but two effects cost sensitivity versus WSJT-X's downsample-based
back end:

1. **Coarse alignment.** The candidate's frequency is quantized to a 3.125 Hz
   sub-bin grid and its time to an 80 ms (half-symbol) grid. The existing
   refinements (hb-044 parabolic `time_refinement` on the sync surface, and the
   `decode_candidate` fine-FFT path's `±3/8`-symbol × `±0.5`-bin grid) partly
   correct this, but the time grid is still coarse (240-sample steps) and the
   frequency grid only reaches half-bin (3.125 Hz). Residual misalignment
   spreads each tone's energy into adjacent FFT bins → lower effective per-symbol
   SNR → fewer marginal LDPC successes.

2. **Off-target noise in the matched filter.** Reading magnitudes from the
   full-bandwidth STFT means each symbol FFT integrates over its full bin
   bandwidth. A signal that has been **mixed to a narrow complex baseband and
   low-pass filtered to ~200 Hz** rejects out-of-band noise and adjacent signals
   *before* the per-symbol FFT, so the matched filter sees a cleaner input.

### 1.2 The mechanism

The standard fix (Franke–Taylor; the WSJT-X downsampler stage) is to take each
coarse candidate, **complex-mix the wide audio so the candidate's frequency
lands near DC, low-pass filter, and decimate** to a narrow complex baseband
(WSJT-X: ~200 Hz wide, ~32 complex samples/symbol). On that baseband:

- **Fine frequency** alignment is cheap and precise (sub-Hz): a small search of
  residual carrier offsets with quadratic interpolation of the
  Costas-correlation peak.
- **Fine time** alignment is cheap and precise (sub-sample): a small search of
  symbol-grid offsets with quadratic interpolation, well below the 80 ms
  spectrogram step.
- **Symbol/LLR extraction** at the refined offset sees a higher effective SNR
  (energy concentrated in-band; tighter matched filter), which is where the
  ~1–2 dB recovery comes from.

The decimation also makes the per-candidate cost **lower than a full per-symbol
`sps`-point FFT path** once amortized, because the baseband is short.

### 1.3 Why ~1–2 dB

Two compounding sources, both standard:

- **Tighter matched filter from finer alignment.** A GFSK tone mismatched by
  `Δf` over a symbol of length `T` loses correlation `~sinc(Δf·T)`. With
  `T = 0.16 s`, a residual `Δf` of even 1.5 Hz (half the current 3.125 Hz grid)
  costs a measurable fraction of a dB per symbol; driving residual `Δf` to
  sub-Hz and residual `Δt` to sub-sample recovers it. Similarly a time
  misalignment of a fraction of a symbol leaks energy across the symbol boundary.
- **Noise-bandwidth reduction.** Restricting the analysis bandwidth from the full
  passband to ~200 Hz around the signal, *before* per-symbol integration, lowers
  the noise power competing with each tone. The published WSJT-X figure for the
  combined effect of its fine-sync-on-baseband back end is on the order of 1–2 dB
  of decode-threshold improvement, consistent with hb-243's expected delta of
  closing 1–2 dB of the documented pancetta-vs-WSJT-X gap.

This is **not** a new coarse search — coarse Costas stays exactly as is. hb-243
is a **per-candidate refinement + re-extraction stage**.

---

## 2. Algorithm (math / pseudocode)

All rates below are for FT8 at the fixed `SAMPLE_RATE = 12000`. Symbols:
`sps = 1920`, `tone_spacing = 6.25 Hz`, `num_tones = 8`, `num_symbols = 79`
(`pancetta-ft8/src/lib.rs`). The signal occupies `8 * 6.25 = 50 Hz`; an
allowance for fine offset + filter skirts motivates a ~200 Hz baseband, matching
the published WSJT-X choice.

### 2.1 Parameters

```
fs        = 12000                       # input rate
sps       = 1920                        # input samples / symbol
df_tone   = 6.25                        # tone spacing (Hz)
W_signal  = num_tones * df_tone = 50    # occupied bandwidth (Hz)

# Baseband target (public WSJT-X choice; pancetta may sweep):
fs_bb     = 200                         # complex baseband rate (Hz)  [OPEN: sweep 100..400]
D         = fs / fs_bb = 60             # integer decimation factor
sps_bb    = sps / D = 32                # complex baseband samples / symbol
N_bb      = num_symbols * sps_bb = 2528 # baseband samples for the message body
```

`D = 60` is integer (`12000 / 200`), so decimation is exact and `sps_bb = 32` is
exact — no fractional resampling needed for the baseband *rate*. (Fractional
resampling is used only conceptually inside the fine-*time* search, §2.5, via
phase-accurate sub-sample interpolation, not for rate change.)

### 2.2 Candidate → audio coordinates (existing pancetta convention)

For a `CostasCandidate`:

```
f_cand   = freq_bin * tone_spacing + freq_sub * (tone_spacing / FREQ_OSR)   # Hz
                                                                            # (decoder.rs ~5927)
t0_samp  = candidate_offset_samples(time_step, time_padding, sps/TIME_OSR)  # signed samples
                                                                            # (decoder.rs ~108)
```

This is the **single** time/freq mapping pancetta already uses
(`candidate_offset_samples` is documented as "THE one place the
time_step → sample-offset convention lives"). hb-243 reuses both verbatim — it
refines `(f_cand, t0_samp)` to `(f_cand + Δf, t0_samp + Δt)`, it does not invent
a new mapping.

### 2.3 Complex mix to baseband

Let `x[n]` be the preprocessed real audio (`preprocess_audio`, decoder.rs ~3358:
peak-normalized `f64`). Mix the candidate's carrier to DC. WSJT-X centers the
*lowest tone* at a small positive offset so all 8 tones sit on one side of DC
inside the passband; pancetta will center on the candidate carrier `f_cand` (so
tone `k` lands at `k * 6.25 Hz` in the baseband, tones `0..7` spanning `0..50 Hz`,
comfortably inside `±100 Hz`):

```
y[n] = x[n] * exp(-j * 2π * f_cand * n / fs)        # analytic down-mix
```

This is the same per-sample complex rotation `extract_symbols_complex` already
performs (`phase_step = exp(-j 2π f / fs)`, decoder.rs ~6226), generalized from
per-symbol to the whole window. After mixing, the signal of interest is the
`0..50 Hz` band of `y[n]`.

### 2.4 Low-pass FIR + decimate

Decimate `y[n]` by `D = 60` to rate `fs_bb = 200`. To avoid aliasing, low-pass
*before* downsampling with cutoff comfortably above the 50 Hz signal but below
`fs_bb/2 = 100 Hz`:

```
LP design (windowed-sinc / Kaiser FIR, real coefficients on complex input):
  passband edge  fp = 60 Hz       # > 50 Hz signal, leaves tone-7 untouched
  stopband edge  fst = 100 Hz     # = fs_bb/2, kills alias images
  transition     ~40 Hz
  stopband atten ~60 dB           # weak-signal: don't let images masquerade as tones
  Kaiser β from atten (≈ 5.65 for 60 dB); N_taps ≈ atten / (22 * Δf/fs)
                                  # Δf=40 Hz, fs=12000 → N_taps ≈ 60/(22*0.00333) ≈ 818
```

Implementation choices (any is acceptable; decide by Phase-1 benchmark):

- **Polyphase decimator** (Crochiere–Rabiner): only compute every `D`-th output,
  so cost ≈ `N_taps * N_bb` multiply-adds per candidate, *not* `N_taps * len(x)`.
  Strongly preferred for per-candidate cost (§5).
- **Frequency-domain extraction** (the WSJT-X approach as publicly described):
  forward-FFT the *whole* window **once per slot** (cached), copy the bins
  spanning the candidate's band into a short buffer with a smooth taper, IFFT to
  the short baseband. This amortizes the expensive FFT across all candidates
  (the "**cached**-bandpass" in the hb-243 name). The per-candidate cost is then
  one short IFFT (`~4096`-point) plus a bin copy. **This is the recommended
  production design**; the polyphase time-domain decimator is the simpler Phase-1
  reference implementation to validate correctness against.

Either way the output is a complex baseband `b[m]`, `m = 0..N_bb`, at
`fs_bb = 200`, with the candidate carrier near DC.

> Public-FFT-extraction sketch (cached forward transform):
> ```
> X = FFT(x_window)                          # ONCE per slot, cached on Spectrogram/ctx
> k_lo = round((f_cand - bb_half) * NFFT/fs) # bb_half ≈ 100 Hz
> k_hi = round((f_cand + bb_half) * NFFT/fs)
> S    = X[k_lo..k_hi] * taper               # smooth window edges (Tukey/Hann skirt)
> b    = IFFT(zero_pad_or_resize(S, M_bb))   # M_bb = next_pow2(N_bb); rate fs_bb
> ```
> The DC-centering shift of §2.3 is folded into the bin selection (choosing
> `k_lo/k_hi` symmetric about `round(f_cand * NFFT/fs)`), so no separate
> per-sample rotation is needed on this path.

### 2.5 Fine frequency + fine time search on baseband

With `b[m]` at 200 Hz, run a small 2-D refinement using the **known 21 Costas
sync symbols** (3 Costas groups of 7 at symbol positions 0–6, 36–42, 72–78 — the
same known symbols `compute_costas_score` uses). Define a coherence metric over
the baseband at trial `(Δf, Δt)`:

```
for each candidate trial Δf in F_grid, Δt in T_grid:
    # residual de-rotation of leftover carrier offset:
    bb'[m] = b[m] * exp(-j 2π Δf m / fs_bb)
    # sub-sample time shift Δt (|Δt| < 1 baseband sample) via
    #   band-limited (sinc / linear-phase all-pass) interpolation, OR
    #   absorb integer part into symbol indexing + fractional part into a
    #   linear-phase ramp exp(-j 2π k Δt_frac / sps_bb) in the symbol FFT.
    score(Δf, Δt) = Σ over the 21 Costas symbols of
                      |  per-symbol DFT of bb' at the known Costas tone  |
                  (optionally phase-coherent: Σ symbol-to-symbol, see §2.7)
pick argmax; quadratic-interpolate the peak in BOTH axes:
    Δf* = parabola_vertex(score[Δf-1], score[Δf], score[Δf+1])
    Δt* = parabola_vertex(score[Δt-1], score[Δt], score[Δt+1])
```

Search ranges (start points; sweep in Phase 2/3):

- **Frequency** `F_grid`: cover the residual after coarse sub-bin quantization,
  i.e. about `±(tone_spacing / FREQ_OSR)/2 = ±1.5 Hz`, plus margin → search
  `±2.5 Hz` in `~0.5 Hz` steps (11 points), then parabolic-interpolate to
  sub-Hz. (hb-243 notes ±2.5 Hz; that matches the WSJT-X fine-freq span as
  publicly described.)
- **Time** `T_grid`: cover the residual after coarse half-symbol + hb-044
  parabolic refinement, about `±0.5` symbol → at `sps_bb = 32`, search
  `±10` baseband samples (≈ `±50 ms`) in 1-sample steps, then
  parabolic-interpolate to sub-sample (hb-243 notes ±10 samples).

This is `~11 × ~21 ≈ 231` cheap baseband trials per candidate (each a handful of
32-point per-symbol correlations over only the 21 known symbols), far cheaper
than the current `9 × 5 = 45` full-`sps`-point FFT trials in `decode_candidate`'s
time-domain fallback. Parabolic peak interpolation is exactly the technique
hb-044 already uses on the coarse sync surface (`sync_time_interpolation`); here
it is applied in **both** axes on the cleaner baseband correlation.

### 2.6 Re-extract symbols / LLRs at the refined offset

At `(f_cand + Δf*, t0_samp + Δt*)`, extract all 79 symbols' tone magnitudes from
the **baseband** (per-symbol 32-point — or zero-padded — DFT reading tones
`0..7`), producing the `[f64; NUM_TONES]` per-symbol tone-magnitude array that
the rest of pancetta's pipeline already consumes. Feed it through the **existing,
unmodified** demap → LLR → normalize → LDPC → CRC → plausibility chain
(`compute_soft_llrs*` / `par_compute_soft_llrs*`, `maybe_whiten_llrs`,
`maybe_impulse_robust_llrs`, `normalize_llrs`, `ldpc.decode_soft`, `verify_crc`,
`Ft8Message::is_plausible`). hb-243 changes **only how the tone-magnitude array
is produced** for a refined candidate; everything downstream is byte-identical.

### 2.7 Coherent vs non-coherent metric (open knob)

The baseband retains phase, so the §2.5 metric can be **non-coherent**
(`Σ |DFT|`, robust, default) or **phase-coherent** (`Σ` of complex DFT outputs
aligned by an estimated rotor across the Costas symbols, ~2× the alignment SNR
but sensitive to rotor noise). pancetta already has phase-coherent machinery for
cross-cycle averaging (`cross_cycle_coherent`) and a per-candidate frequency
tracker (`freq_tracker::FrequencyTracker`). hb-243 Phase 2/3 start non-coherent;
a coherent variant is a follow-on knob, **not** required for the first
graduation.

---

## 3. Integration into pancetta

### 3.1 New `Ft8Config` flag(s) (`pancetta-ft8/src/decoder.rs`, the `Ft8Config` struct ~186)

Add, all defaulted to the no-op value so production is byte-identical:

```rust
/// hb-243: enable the per-candidate cached-bandpass downsampler + fine
/// time/freq sync refinement stage. When true, each coarse Costas
/// candidate that the spectrogram path FAILS to decode is re-extracted
/// from a ~200 Hz complex baseband at the parabolically-refined
/// (Δf, Δt) offset before falling through to the legacy fine-FFT path.
/// Default false — byte-identical to the legacy decode path.
pub downsample_fine_sync_enabled: bool,                 // default false

/// hb-243: complex baseband rate (Hz). Must divide SAMPLE_RATE evenly.
/// Default 200 (D=60, 32 samples/symbol) per the public WSJT-X choice.
pub downsample_baseband_rate_hz: u32,                   // default 200

/// hb-243: fine-frequency half-search-range (Hz) and step.
pub downsample_fine_freq_radius_hz: f64,                // default 2.5
pub downsample_fine_freq_step_hz: f64,                  // default 0.5

/// hb-243: fine-time half-search-range (baseband samples) and step.
pub downsample_fine_time_radius_samp: usize,            // default 10
pub downsample_fine_time_step_samp: usize,              // default 1

/// hb-243: use the phase-coherent fine-sync metric (§2.7) instead of
/// the non-coherent |DFT| sum. Default false.
pub downsample_fine_sync_coherent: bool,                // default false
```

Wire each into the existing `Default for Ft8Config` impl, the `DecodeContext`
construction (decoder.rs ~2200), and the `DecodeContext` struct so the parallel
path sees them (mirrors how every other knob, e.g.
`per_candidate_freq_tracker_*`, threads through). Config validation
(`pancetta-config`) should reject `SAMPLE_RATE % downsample_baseband_rate_hz != 0`.

### 3.2 Where it slots into the decode flow

The production hot loop is `par_decode_candidate` (decoder.rs ~6994), invoked via
`rayon` `map_init(ldpc_init, decode_candidate_op)` (~2359/2407). Its current
shape:

1. spectrogram-path extraction over two `freq_sub` trials → LLR → LDPC → CRC →
   plausibility → return on success;
2. **else** (strong candidates only, `sync_score >= 3.5`) the time-domain
   fine-FFT fallback: `9 × 5` grid via `par_extract_symbols_complex`
   (decoder.rs ~8844).

hb-243 inserts a **new refinement stage between (1) and (2)**, gated on
`ctx.downsample_fine_sync_enabled`:

```
par_decode_candidate(ctx, cand, ldpc, fft_buffer):
    # (1) existing spectrogram path  --- unchanged
    if spectrogram path decodes: return Some(msg)

    # (1.5) NEW hb-243 stage (only if flag on)
    if ctx.downsample_fine_sync_enabled:
        b = baseband_extract(ctx, cand)          # §2.3-2.4, uses cached slot FFT if present
        (df, dt) = fine_sync(b, ctx, cand)       # §2.5 parabolic peak in both axes
        tone_mags = baseband_extract_symbols(b, df, dt)   # §2.6 → [f64; NUM_TONES] * 79
        llrs = <existing demap/whiten/normalize chain on tone_mags>
        if ldpc.decode_soft(llrs) ok and verify_crc and is_plausible:
            return Some(DecodedMessage at (f_cand+df, t0_samp+dt))

    # (2) existing fine-FFT fallback  --- unchanged
    ...
```

Because the new stage only runs when the **spectrogram path already failed**, it
is strictly additive recall: it can only *gain* decodes a coarse-grid extraction
missed. It also subsumes the legacy `(2)` fine-FFT path's role (finer, cheaper),
so a future cleanup may replace `(2)` with hb-243 once graduated — but **do not**
remove `(2)` in the same change; keep it as the off-path default.

New functions (all in `decoder.rs`, mirroring the `par_*` free-function style so
they are `Send`-safe for the rayon closure and unit-testable in isolation):

- `fn baseband_extract(ctx, candidate) -> Vec<Complex<f64>>` — §2.3/2.4.
  Recommended production form reads a **cached per-slot forward FFT** (see 3.3).
- `fn fine_sync(bb, ctx, candidate) -> (f64 /*Δf Hz*/, f64 /*Δt baseband samp*/)`
  — §2.5; reuse the parabolic-vertex helper that backs `sync_time_interpolation`.
- `fn baseband_extract_symbols(bb, df, dt, pp) -> Vec<[f64; NUM_TONES]>` — §2.6;
  output type matches `par_extract_symbols_from_spectrogram` so the downstream
  LLR chain is untouched.

The reported `DecodedMessage.base_frequency = f_cand + Δf*` and
`time_offset = (t0_samp + Δt*_in_input_samples) / SAMPLE_RATE` (convert the
baseband-sample `Δt*` back to input samples by `× D`). `subtract_signal` /
`reverse_derive_candidate` (decoder.rs ~7959) already round-trip arbitrary
`(freq, dt)`; the refined offset feeds them unchanged.

### 3.3 Cached forward FFT (the "cached" in cached-bandpass)

For the recommended frequency-domain extraction (§2.4), compute **one** forward
FFT of the window per slot and stash it where every candidate can read it.
Natural home: alongside the `Spectrogram` (which already lives for the whole
slot) or as a new field on `DecodeContext` (immutable, shared across the rayon
workers — it is read-only after construction, so `&[Complex<f64>]` is fine).
Build it once in `decode_window*` right after `compute_spectrogram`, only when
`downsample_fine_sync_enabled`. This is what makes per-candidate cost a short
IFFT instead of a full FFT.

### 3.4 Composition with existing mechanisms

- **hb-044 parabolic `time_refinement` / `sync_time_interpolation`:** these
  refine the *coarse-spectrogram* candidate. hb-243 takes the coarse candidate
  (with hb-044's fractional `time_refinement` folded into `t0_samp`) as its
  *starting point* and refines further on the baseband. They compose: hb-044
  narrows the coarse grid; hb-243 polishes on baseband. The `T_grid` center
  should incorporate hb-044's `time_refinement` so the baseband search is
  centered on the best coarse estimate.
- **Multipass / coherent subtract (`max_decode_passes`, `coherent_subtract_*`):**
  hb-243 runs inside `par_decode_candidate`, so each pass (original + residuals)
  gets the refinement automatically. The refined `(freq, dt)` produces a
  **better subtraction template** (the same motivation as the ft8mon "stage 3"
  knob already in config), which can compound recall on pass 2+. Measure with
  and without multipass.
- **AP / a-priori (`ap_context`, AP0/AP):** unchanged — AP affects LLR priors,
  not extraction geometry. hb-243's cleaner tone magnitudes feed the same AP LLR
  path.
- **Soft combiner / cross-cycle averaging:** orthogonal — they operate on LLRs /
  power across receptions. hb-243 improves the per-reception tone magnitudes that
  feed them. Keep both off for the first hb-243 A/B to isolate the effect.
- **`per_candidate_freq_tracker`:** conceptually overlapping (both refine
  frequency). hb-243's one-shot baseband fine-freq is the static counterpart; the
  tracker handles *drift within* a transmission. They can stack later; for the
  first eval, hold the tracker OFF (its default).

---

## 4. Phased implementation plan

Each phase is a separate iter branch, locally `fmt`+`clippy`+tests green, with a
research-eval checkpoint before the next. Corpora: `hard_200` (fast inner loop)
then `raw_530_full` / `hard_1000` for graduation, all under **ft8_lib truth**
(neutral labeling, per Probe Baseline Discipline). Report ΔTP / ΔFP / Δwall per
checkpoint.

### Phase 1 — baseband mixer + decimator, unit-tested in isolation (no decode wiring)

- Implement `baseband_extract` (both the polyphase reference and the cached-FFT
  production form) + an inverse sanity path.
- **Unit tests (in a `mod hb243_baseband_tests` block in `decoder.rs`):**
  - Mix+decimate a synthetic single FT8 tone at a known `f_cand`; assert the
    baseband peak lands at the expected bin within tolerance.
  - Mix+decimate a synthetic full 79-symbol FT8 signal (use the existing
    `encoder`/`modulator`); assert per-symbol baseband DFT recovers the
    transmitted tone sequence at SNR where the spectrogram path also succeeds
    (parity check, not a sensitivity claim yet).
  - Filter response: assert ≥ ~50 dB rejection at `fs_bb/2` and < 0.5 dB ripple
    in `0..50 Hz`.
- **Checkpoint:** no production behavior change (flag still gates nothing live);
  benchmark per-candidate baseband cost (polyphase vs cached-FFT) to choose the
  production form. **Gate to Phase 2:** correctness tests pass; cost is within
  budget (§5).

### Phase 2 — fine-FREQUENCY-only refinement on coarse candidates + eval

- Wire stage (1.5) but with `T_grid = {0}` (no time search): only `Δf*` applied,
  symbols re-extracted from baseband at the coarse time.
- **Eval:** `hard_200` A/B (flag off vs on). Expect a small positive ΔTP from
  finer frequency alignment, near-zero ΔFP (CRC+plausibility gate). If ΔFP blows
  up, the baseband symbol extraction or LLR scaling is wrong — debug before
  proceeding.
- **Gate to Phase 3:** ΔTP ≥ 0 with ΔFP economics no worse than the existing
  fine-FFT path; bootstrap-CI per Engineering Substance Check for small deltas.

### Phase 3 — add fine-TIME refinement (full 2-D) + eval

- Enable `T_grid` (§2.5), sub-sample interpolation, both-axis parabolic peak.
- **Eval:** `hard_200` then `raw_530_full`. This is where the bulk of the 1–2 dB
  should appear (time alignment is the coarser of the two grids). Sweep
  `downsample_fine_time_radius_samp` and `downsample_baseband_rate_hz` (100 / 200 /
  400) for the recall/cost knee.
- **Gate to Phase 4:** monotonic ΔTP across corpora, FP-clean, wall within budget.

### Phase 4 — re-extract LLRs from baseband as the canonical extraction + eval; coherent variant

- Confirm the baseband re-extraction (§2.6) is the value-add (vs. just refining
  offset and re-reading the spectrogram). A/B baseband-extract vs spectrogram-read
  at the refined offset to attribute the gain.
- Optional: implement `downsample_fine_sync_coherent` (§2.7) and A/B it.
- **Graduation eval:** `raw_530_full` + `hard_1000`, ft8_lib truth, with the
  production FP filter (hb-062) active. Graduate the default to `true` **only**
  if: ΔTP positive on both corpora with acceptable ΔFP/TP economics (the same bar
  Batch 78/83 applied to other recall levers), and wall-clock stays within the
  per-WAV budget on the slowest supported tier (coordinator `tier.rs`).
  Otherwise ship plumbing default-OFF and journal the result (the project's
  standard SHIP-OPT-IN / SHELVE outcome).

---

## 5. Risks / open questions

### 5.1 CPU cost (the main risk)

- **Per-candidate decimation must not be `O(N_taps × full_window)`.** With up to
  `max_sync_candidates = 200` candidates × multipass passes, a naive per-candidate
  full-window FIR is too expensive. Mitigations, in priority order:
  1. **Cached per-slot forward FFT + per-candidate short IFFT** (§3.3) — the
     dominant FFT is paid once; per-candidate is a bin copy + short IFFT. This is
     the design intent ("**cached**-bandpass").
  2. Polyphase decimator computing only kept samples (`N_taps × N_bb`, not
     `N_taps × len`).
  3. Run hb-243 **only on candidates the spectrogram path failed** (already the
     plan) — most strong signals decode on path (1) and never reach (1.5).
  4. Gate by `sync_score` (like the existing `< 3.5` fine-FFT gate) so only
     plausibly-decodable failures pay the cost.
- **Open:** measure the realistic per-slot overhead on the Slow hardware tier; if
  it exceeds budget, the graduation default stays OFF / Slow-tier forces OFF
  (mirroring how `tier.rs` already rewrites `Ft8Config` per tier).

### 5.2 Interaction with the `FREQ_OSR = 2` grid

- The coarse sub-bin grid is already 3.125 Hz, so hb-243's fine-freq is refining
  a *half-bin* residual at most. The gain from fine-freq alone may be modest
  (hence Phase 2 expects a *small* ΔTP); the larger win is fine-**time** + the
  baseband noise-bandwidth reduction. **Open:** does fine-freq pull its weight, or
  should the freq search be cheap/narrow (±1.5 Hz) and effort go to time? Phase 2
  vs Phase 3 deltas answer this empirically.

### 5.3 When it helps vs hurts

- **Helps:** marginal SNR signals on a quiet patch of band where coarse-grid
  misalignment is the last barrier; signals sitting between sub-bins/half-symbols.
- **Risk of hurting:** on **busy bands**, a wider-than-signal baseband window can
  pull in an adjacent signal 50–150 Hz away; the fine-sync metric could lock onto
  the neighbor or the re-extraction could mix two signals. Mitigations: keep the
  LP cutoff tight (`fp = 60 Hz`), keep the fine search ranges narrow, and lean on
  CRC + plausibility (any cross-contaminated extraction fails CRC). **Open:**
  confirm no FP inflation on the densest `hard_1000` slots.
- **Over-refinement:** like hb-044's unscaled parabolic delta over-correcting on
  noisy real audio (which forced `sync_time_interp_delta_scale = 0.3`), an
  aggressive baseband fine search on a noisy candidate can land on a noise peak.
  Mitigation: parabolic interpolation only on a clear single peak; reject when the
  peak is not well-formed (curvature/contrast gate) and fall back to the coarse
  offset — analogous to `sync_time_interp_max_delta_abs`. Add a
  `downsample_fine_*_max_delta` reject knob if Phase 3 shows over-correction.

### 5.4 Graduation bar (what the eval must show)

- **Recall:** ΔTP > 0, monotonic across `hard_200` → `raw_530_full` → `hard_1000`
  under ft8_lib truth (no corpus where it regresses), with bootstrap-CI not
  straddling 0 for the small-delta phases.
- **Precision:** ΔFP economics no worse than the existing recall levers the
  project ships (the hb-062 FP filter must remain effective on the new decodes;
  measure FP/TP ratio of the *novel* decodes specifically).
- **Cost:** wall-clock per WAV within budget on every supported tier, or
  Slow-tier-OFF via `tier.rs`.
- **Attribution (Phase 4):** the gain is from baseband re-extraction + fine sync,
  not from incidental candidate reordering — confirmed by the
  refined-offset-spectrogram-read A/B.

### 5.5 Other open questions

- Exact LP filter order / window vs. the cached-FFT taper width — finalize by the
  Phase-1 filter-response test, not by reading any peer source.
- Baseband rate (`100 / 200 / 400 Hz`): 200 is the public WSJT-X choice and the
  Phase-3 default; sweep to confirm pancetta's knee.
- Coherent (§2.7) vs non-coherent metric net gain — Phase 4 optional A/B.
- Whether to eventually **replace** the legacy time-domain fine-FFT fallback
  (`par_extract_symbols_complex`, decoder.rs ~8844) with hb-243 once graduated
  (cleanup, separate change).

---

## 6. Summary of touch points (for the implementer)

| Item | File:location | Change |
|------|---------------|--------|
| New config flags | `pancetta-ft8/src/decoder.rs` `Ft8Config` (~186) + `Default` impl | add 7 fields, all no-op defaults |
| `DecodeContext` plumbing | `decoder.rs` (~2200, struct + construction) | thread flags into the parallel ctx |
| Cached per-slot FFT | `decoder.rs` `decode_window*` after `compute_spectrogram` | build once when flag on |
| Refinement stage | `decoder.rs` `par_decode_candidate` (~6994), between path (1) and (2) | gated `if downsample_fine_sync_enabled` |
| New helpers | `decoder.rs` (free `par_*`-style fns) | `baseband_extract`, `fine_sync`, `baseband_extract_symbols` |
| Reuse verbatim | `candidate_offset_samples` (~108), `f_cand` formula (~5927), parabolic-vertex helper (hb-044), demap→LLR→LDPC→CRC chain | unchanged |
| Config validation | `pancetta-config` | `SAMPLE_RATE % baseband_rate == 0` |
| Unit tests | `decoder.rs` `mod hb243_baseband_tests` | Phase 1 correctness/filter tests |
| Eval | `pancetta-research` corpora (`hard_200`, `raw_530_full`, `hard_1000`), ft8_lib truth | per-phase checkpoints |

**Default state after this work lands:** `downsample_fine_sync_enabled = false`
— production decode path byte-identical until a graduation eval flips it.
