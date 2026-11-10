//! hb-243 Phase 1 — baseband complex-mixer + decimator (isolated DSP block).
//!
//! Down-mixes a candidate carrier to DC and decimates the FT8 audio to a narrow
//! complex baseband, the front end for the planned fine time/frequency sync
//! (hb-243, `docs/superpowers/specs/2026-06-18-hb243-downsampler-design.md`).
//!
//! This module is **standalone and unwired** — nothing in the live decode path
//! calls it yet (Phase 1 = correctness + cost validation only). It is a
//! clean-room implementation from public DSP theory (windowed-sinc / Kaiser FIR,
//! complex mixing, integer decimation); no GPL source was consulted.
//!
//! Pipeline (FT8 @ `fs = 12000`):
//!   1. complex down-mix:  `y[n] = x[n] · exp(-j2π·f_cand·n/fs)`
//!   2. low-pass FIR (Kaiser windowed-sinc, real taps on complex input)
//!   3. integer decimate by `D = 60`  →  complex baseband at `fs_bb = 200 Hz`
//!      (`sps_bb = 32` complex samples/symbol, exact).
//!
//! Task W3.1 (`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
//! §3, D3) adds [`extract_candidate_baseband`] — a per-candidate front end that
//! slices a search-margin window out of the raw window audio around a
//! candidate's coarse `start_sample` and runs it through this same
//! down-mix/FIR/decimate pipeline, returning a [`BasebandSlice`]. This is
//! still **unwired**: `decoder.rs`'s `DecodeContext::audio: &[f64]` (the raw
//! window audio, already plumbed to the candidate stage for the existing
//! fine-FFT fallback at `par_extract_symbols_complex`) is exactly the buffer
//! a future caller would pass in — no new plumbing was needed, so none was
//! added. Task W3.2 (fine dt/df search) and W3.3 (matched demod) are the
//! planned first callers.

use crate::protocol::ProtocolParams;
use num_complex::Complex;

/// Input sample rate (FT8 fixed).
pub const FS: f64 = 12_000.0;
/// Baseband sample rate after decimation.
pub const FS_BB: f64 = 200.0;
/// Integer decimation factor (`FS / FS_BB`, exact).
pub const DECIM: usize = 60;

/// Search-margin size on each side of the nominal symbol window, in whole
/// symbols.
///
/// Sized for Task W3.2's planned fine dt/df search, which scans
/// `dt ∈ ±half symbol`: two full symbols of margin per side gives 4×
/// headroom over that ±0.5-symbol requirement, which comfortably absorbs
/// (a) the coarse candidate's own quantization error (the caller's
/// `start_sample` is only accurate to the spectrogram time step, not to the
/// sample), and (b) implementation slack for W3.3's matched-demod stage to
/// look a little past the nominal symbol boundaries without a second
/// extraction. Not a value from the design spec (none is given numerically
/// there) — a deliberate, documented choice future tasks can widen if the
/// fine-sync search ever needs more room.
pub const MARGIN_SYMBOLS: f64 = 2.0;

// Low-pass design targets (see spec §2.4): pass the 50 Hz signal (+ skirt),
// kill everything by `FS_BB/2 = 100 Hz` so decimation images can't masquerade
// as tones.
const FIR_PASS_HZ: f64 = 60.0; // > 50 Hz occupied bandwidth
const FIR_STOP_HZ: f64 = 100.0; // = FS_BB/2
const FIR_ATTEN_DB: f64 = 60.0; // weak-signal: deep stopband

/// Modified Bessel function of the first kind, order 0 — for the Kaiser window.
/// Series form (converges fast for the `x` ranges Kaiser β produces).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    // term_k = (x/2)^{2k} / (k!)^2 ; ratio term_k/term_{k-1} = (x/2)^2 / k^2
    for k in 1..50 {
        term *= (half_x / k as f64).powi(2);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Kaiser β from a target stopband attenuation (Kaiser's empirical formula).
fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// Kaiser tap count from attenuation and transition width (Hz), forced odd for
/// a symmetric (linear-phase) Type-I FIR.
fn kaiser_num_taps(atten_db: f64, transition_hz: f64, fs: f64) -> usize {
    let n = ((atten_db - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * transition_hz / fs)).ceil()
        as usize;
    if n.is_multiple_of(2) {
        n + 1
    } else {
        n
    }
}

/// Design the decimation low-pass: Kaiser-windowed sinc, real taps, unity DC
/// gain, cutoff at the midpoint of the pass/stop edges.
pub fn design_decimation_lowpass() -> Vec<f64> {
    let fc = 0.5 * (FIR_PASS_HZ + FIR_STOP_HZ); // 80 Hz
    let transition = FIR_STOP_HZ - FIR_PASS_HZ; // 40 Hz
    let beta = kaiser_beta(FIR_ATTEN_DB);
    let num_taps = kaiser_num_taps(FIR_ATTEN_DB, transition, FS);
    let m = (num_taps - 1) as f64 / 2.0;
    let i0_beta = bessel_i0(beta);
    let wc = 2.0 * fc / FS; // normalized cutoff (cycles/sample × 2)

    let mut taps = Vec::with_capacity(num_taps);
    for n in 0..num_taps {
        let t = n as f64 - m;
        // Ideal LP impulse response: wc · sinc(wc · t)
        let sinc = if t.abs() < 1e-9 {
            wc
        } else {
            (std::f64::consts::PI * wc * t).sin() / (std::f64::consts::PI * t)
        };
        // Kaiser window
        let r = t / m;
        let win = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
        taps.push(sinc * win);
    }
    // Normalize to unity DC gain.
    let dc: f64 = taps.iter().sum();
    for c in &mut taps {
        *c /= dc;
    }
    taps
}

/// Down-mix `audio` (preprocessed real samples at `FS`) to a complex baseband
/// centered on `f_cand_hz`, low-pass, and decimate by `DECIM`.
///
/// Output `b[k]` is the (group-delay-compensated) baseband at `FS_BB`, so `b[k]`
/// corresponds to input time `k·DECIM / FS` and the candidate carrier sits at DC
/// (tone `t` of the FT8 signal lands at `t · 6.25 Hz`).
pub fn baseband_extract(audio: &[f64], f_cand_hz: f64) -> Vec<Complex<f64>> {
    let taps = design_decimation_lowpass();
    baseband_extract_with(audio, f_cand_hz, &taps)
}

/// Like [`baseband_extract`] but with caller-supplied taps (lets a hot loop
/// design the filter once and reuse it across candidates).
pub fn baseband_extract_with(audio: &[f64], f_cand_hz: f64, taps: &[f64]) -> Vec<Complex<f64>> {
    let n = audio.len();
    let ntaps = taps.len();
    let m = (ntaps - 1) / 2; // symmetric FIR group delay (samples)

    // Down-mix to baseband.
    let w = -2.0 * std::f64::consts::PI * f_cand_hz / FS;
    let mut y = vec![Complex::new(0.0, 0.0); n];
    for (i, &x) in audio.iter().enumerate() {
        let ph = w * i as f64;
        y[i] = Complex::new(x * ph.cos(), x * ph.sin());
    }

    // FIR + decimate. Centered (zero-delay) output sample at input index `c`:
    //   f[c] = Σ_j taps[j] · y[c + m - j]
    // valid while every tap index stays in range: m ≤ c < n - m.
    let mut out = Vec::new();
    let mut c = if m.is_multiple_of(DECIM) {
        m
    } else {
        // first multiple of DECIM that is ≥ m
        m.div_ceil(DECIM) * DECIM
    };
    while c + m < n {
        let mut acc = Complex::new(0.0, 0.0);
        for (j, &h) in taps.iter().enumerate() {
            acc += y[c + m - j] * h;
        }
        out.push(acc);
        c += DECIM;
    }
    out
}

/// A per-candidate baseband extraction: the nominal 79-symbol (or
/// protocol-appropriate) transmission window, plus [`MARGIN_SYMBOLS`] of
/// extra context on each side, down-mixed to 200 Hz complex baseband.
///
/// Coordinates are all in the *baseband* (200 Hz, post-decimation) domain
/// unless a field name says otherwise. Symbol `s` (`0 <= s < num_symbols`) of
/// the nominal window lives at
/// `samples[nominal_start_index + s * samples_per_symbol .. + samples_per_symbol]`
/// — but a caller doing a fine dt search should expect the true signal to sit
/// a few samples either side of that (that's what the margin is for).
#[derive(Debug, Clone)]
pub struct BasebandSlice {
    /// Complex baseband samples at `FS_BB` (200 Hz).
    pub samples: Vec<Complex<f64>>,
    /// Baseband sample rate (`FS_BB`, 200 Hz).
    pub sample_rate_hz: f64,
    /// Baseband samples per symbol (32 for FT8/FT4 at `FS_BB` = 200 Hz).
    pub samples_per_symbol: usize,
    /// Number of symbols in the nominal (non-margin) window, from `pp.num_symbols`.
    pub num_symbols: usize,
    /// Index into `samples` where the nominal window (symbol 0) begins.
    /// Equal to the *leading* `margin_samples` value by construction — not
    /// to be confused with the (asymmetric) trailing margin; see that
    /// field's doc. Callers should use this field rather than recomputing it.
    pub nominal_start_index: usize,
    /// Baseband-domain margin available *before* `nominal_start_index`
    /// (i.e. `nominal_start_index` itself). A dt search may range back this
    /// far before running out of leading context; see [`MARGIN_SYMBOLS`]
    /// for the nominal sizing. The trailing margin (after the last nominal
    /// symbol) is `samples.len() - nominal_start_index - num_symbols *
    /// samples_per_symbol` — normally within a sample or two of this value,
    /// but not asserted equal: the FIR's own group delay consumes a small,
    /// deterministic slice of the requested margin on the leading edge only
    /// (see the function doc), so don't assume exact front/back symmetry.
    pub margin_samples: usize,
    /// The `start_sample` the caller passed in (audio-domain, `FS` = 12000 Hz
    /// sample index into the original `audio` slice) — echoed back so a
    /// consumer can map a baseband-domain dt refinement back to an
    /// audio-domain sample offset (`dt_audio_samples ≈ dt_bb_samples * DECIM`).
    pub start_sample: isize,
    /// The candidate carrier frequency (Hz) used for the down-mix.
    pub freq_hz: f64,
}

/// Extract a per-candidate baseband slice around a coarse candidate position.
///
/// Slices `[start_sample - margin, start_sample + window + margin)` out of
/// `audio` (zero-padding whatever falls outside `audio`'s bounds, so a
/// candidate near either edge of the recording still gets a full-size,
/// well-formed slice rather than a truncated one), then runs it through the
/// same down-mix → Kaiser-FIR → decimate-by-60 pipeline as
/// [`baseband_extract`], producing 200 Hz complex baseband at 32
/// samples/symbol.
///
/// `start_sample` is audio-domain (`FS` = 12000 Hz), matching the coarse
/// candidate position a caller like `decoder.rs`'s `DecodeContext::audio`
/// would provide; it may be negative or run past `audio.len()` after adding
/// the window — both are zero-padded rather than panicking, since a coarse
/// candidate near the edge of a decode window is an expected input, not a
/// caller bug.
///
/// This function is standalone and unwired (Task W3.1): nothing in the live
/// decode path calls it yet. It is the planned entry point for Task W3.2
/// (fine dt/df search) and W3.3 (matched demod).
pub fn extract_candidate_baseband(
    audio: &[f64],
    freq_hz: f64,
    start_sample: isize,
    pp: &ProtocolParams,
) -> BasebandSlice {
    let taps = design_decimation_lowpass();
    extract_candidate_baseband_with(audio, freq_hz, start_sample, pp, &taps)
}

/// Like [`extract_candidate_baseband`] but with caller-supplied FIR taps
/// (lets a hot loop design the filter once and reuse it across candidates).
pub fn extract_candidate_baseband_with(
    audio: &[f64],
    freq_hz: f64,
    start_sample: isize,
    pp: &ProtocolParams,
    taps: &[f64],
) -> BasebandSlice {
    let sps = pp.samples_per_symbol(FS as u32);
    let window_samples = pp.num_symbols * sps;
    let margin_samples_audio = (MARGIN_SYMBOLS * sps as f64).round() as isize;

    let lo = start_sample - margin_samples_audio;
    let hi = start_sample + window_samples as isize + margin_samples_audio;
    let span = (hi - lo).max(0) as usize;

    // Build the (possibly zero-padded) local slice `[lo, hi)` out of `audio`.
    let mut local = vec![0.0f64; span];
    let copy_lo = lo.max(0);
    let copy_hi = hi.min(audio.len() as isize);
    if copy_hi > copy_lo {
        let src_start = copy_lo as usize;
        let src_end = copy_hi as usize;
        let dst_start = (copy_lo - lo) as usize;
        let dst_end = dst_start + (src_end - src_start);
        local[dst_start..dst_end].copy_from_slice(&audio[src_start..src_end]);
    }

    let samples = baseband_extract_with(&local, freq_hz, taps);

    // Map the local-slice sample index `margin_samples_audio` (i.e. the
    // caller's `start_sample`) to a baseband output index. `baseband_extract_with`
    // emits output index `k` for local input index `c0 + k*DECIM`, where `c0`
    // is the first multiple-of-DECIM-aligned valid center (see that
    // function's doc). `margin_samples_audio` is constructed as a multiple
    // of `DECIM` (a whole number of symbols, and `sps` is itself a multiple
    // of `DECIM` for FT8/FT4), so it lands exactly on a decimation output
    // when `c0` is also DECIM-aligned (which it is by construction) —
    // dividing out any residual FIR group-delay alignment cleanly.
    let ntaps = taps.len();
    let m = (ntaps - 1) / 2;
    let c0 = if m.is_multiple_of(DECIM) {
        m
    } else {
        m.div_ceil(DECIM) * DECIM
    };
    let margin_bb = if margin_samples_audio >= 0 {
        (margin_samples_audio as usize).saturating_sub(c0) / DECIM
    } else {
        0
    };

    BasebandSlice {
        samples,
        sample_rate_hz: FS_BB,
        samples_per_symbol: sps / DECIM,
        num_symbols: pp.num_symbols,
        nominal_start_index: margin_bb,
        margin_samples: margin_bb,
        start_sample,
        freq_hz,
    }
}

#[cfg(test)]
mod hb243_baseband_tests {
    use super::*;

    /// DFT magnitude at a single normalized bin (cycles/sample), for spectral
    /// assertions without pulling rustfft into the test.
    fn goertzel_mag(samples: &[Complex<f64>], cycles_per_sample: f64) -> f64 {
        let w = -2.0 * std::f64::consts::PI * cycles_per_sample;
        let mut acc = Complex::new(0.0, 0.0);
        for (n, &s) in samples.iter().enumerate() {
            let ph = w * n as f64;
            acc += s * Complex::new(ph.cos(), ph.sin());
        }
        acc.norm()
    }

    #[test]
    fn design_lowpass_is_well_formed() {
        let taps = design_decimation_lowpass();
        assert!(taps.len() % 2 == 1, "linear-phase FIR must have odd length");
        // Unity DC gain.
        let dc: f64 = taps.iter().sum();
        assert!((dc - 1.0).abs() < 1e-9, "DC gain must be 1, got {dc}");
        // Symmetric (linear phase).
        let m = taps.len() / 2;
        for k in 0..m {
            assert!(
                (taps[k] - taps[taps.len() - 1 - k]).abs() < 1e-12,
                "FIR must be symmetric"
            );
        }
    }

    #[test]
    fn filter_response_meets_passband_and_stopband_spec() {
        let taps = design_decimation_lowpass();
        // Frequency response H(f) = Σ taps[n] e^{-j2π f n / FS}, magnitude in dB
        // relative to DC (unity). Evaluate the response of the *taps* directly.
        let resp_db = |f_hz: f64| -> f64 {
            let w = -2.0 * std::f64::consts::PI * f_hz / FS;
            let mut acc = Complex::new(0.0, 0.0);
            for (n, &h) in taps.iter().enumerate() {
                let ph = w * n as f64;
                acc += Complex::new(h * ph.cos(), h * ph.sin());
            }
            20.0 * acc.norm().log10()
        };
        // Passband 0..50 Hz: ripple < 0.5 dB.
        for i in 0..=50 {
            let db = resp_db(i as f64);
            assert!(
                db.abs() < 0.5,
                "passband ripple at {i} Hz = {db:.3} dB exceeds 0.5 dB"
            );
        }
        // Stopband at FS_BB/2 = 100 Hz and beyond: >= 50 dB rejection.
        for &f in &[100.0_f64, 120.0, 200.0, 400.0] {
            let db = resp_db(f);
            assert!(
                db <= -50.0,
                "stopband at {f} Hz = {db:.1} dB, need <= -50 dB"
            );
        }
    }

    #[test]
    fn single_tone_lands_near_dc() {
        // A pure tone at f_cand must, after down-mix + decimate, sit at DC
        // (bin 0) of the baseband — i.e. DC dominates any non-DC bin.
        let f_cand = 1234.0_f64;
        let n = (FS as usize) * 6; // 6 s of audio
        let audio: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f_cand * i as f64 / FS).cos())
            .collect();
        let bb = baseband_extract(&audio, f_cand);
        assert!(!bb.is_empty());
        let dc = goertzel_mag(&bb, 0.0);
        // A clearly non-DC baseband bin (e.g. 25 Hz → 25/200 cyc/sample).
        let off = goertzel_mag(&bb, 25.0 / FS_BB);
        assert!(
            dc > 20.0 * off.max(1e-9),
            "down-mixed tone should concentrate at DC: dc={dc:.2}, off={off:.2}"
        );
    }

    #[test]
    fn off_carrier_tone_appears_off_dc_at_expected_baseband_bin() {
        // A tone 25 Hz above the mix carrier must land at +25 Hz in the
        // baseband (carrier→DC convention), i.e. that bin dominates DC.
        let f_carrier = 1500.0_f64;
        let f_tone = f_carrier + 25.0;
        let n = (FS as usize) * 6;
        let audio: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f_tone * i as f64 / FS).cos())
            .collect();
        let bb = baseband_extract(&audio, f_carrier);
        let at_25 = goertzel_mag(&bb, 25.0 / FS_BB);
        let at_dc = goertzel_mag(&bb, 0.0);
        assert!(
            at_25 > 20.0 * at_dc.max(1e-9),
            "tone 25 Hz above carrier should land at +25 Hz baseband bin: \
             at_25={at_25:.2}, at_dc={at_dc:.2}"
        );
    }

    /// Full-signal parity: encode + modulate a real FT8 message, then confirm
    /// the baseband recovers the transmitted 8-FSK tone sequence per symbol.
    /// Requires the encoder/modulator (transmit feature).
    #[cfg(feature = "transmit")]
    #[test]
    fn full_ft8_signal_per_symbol_tone_recovery() {
        use crate::{Ft8Encoder, Ft8Modulator};

        let f_cand = 1500.0_f64;
        let symbols = Ft8Encoder::new()
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = Ft8Modulator::new(FS as u32, f_cand, 0.9).expect("modulator");
        // base_frequency already = f_cand, so frequency_offset = 0.
        let audio_f32 = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
        let audio: Vec<f64> = audio_f32.iter().map(|&s| s as f64).collect();

        let bb = baseband_extract(&audio, f_cand);
        // Input is 1920 samples/symbol; after decimation by DECIM=60 → 32.
        let sps_bb = 1920 / DECIM;
        assert_eq!(sps_bb, 32, "baseband samples per symbol");

        // For each FT8 symbol, the dominant baseband tone bin should match the
        // transmitted symbol value (tone t → t·6.25 Hz → bin t in a 32-pt DFT
        // over one symbol, since 6.25 Hz · 32/200 = 1 bin).
        let n_syms = symbols.len();
        let mut correct = 0;
        let mut counted = 0;
        for (s, &sym) in symbols.iter().enumerate() {
            let start = s * sps_bb;
            if start + sps_bb > bb.len() {
                break;
            }
            let frame = &bb[start..start + sps_bb];
            // Find the strongest of the 8 tone bins (0..8 of a 32-pt DFT).
            let mut best_bin = 0usize;
            let mut best_mag = -1.0;
            for tone in 0..8usize {
                let mag = goertzel_mag(frame, tone as f64 / sps_bb as f64);
                if mag > best_mag {
                    best_mag = mag;
                    best_bin = tone;
                }
            }
            counted += 1;
            if best_bin == sym as usize {
                correct += 1;
            }
        }
        assert!(counted > 0);
        // Allow a couple of edge/transition-symbol misses; the vast majority
        // must match (this is a parity check, not a sensitivity claim).
        let frac = correct as f64 / counted as f64;
        assert!(
            frac >= 0.9,
            "baseband per-symbol tone recovery {correct}/{counted} ({frac:.2}) < 0.90 \
             over {n_syms} symbols"
        );
    }

    /// Task W3.1 TDD: `extract_candidate_baseband` at a nonzero dt offset.
    ///
    /// Synthesizes a real FT8 signal (via the real encoder/modulator, not a
    /// hand-rolled tone) at 1503.1 Hz, then places it +0.12 s late relative
    /// to the coarse candidate's assumed `start_sample = 0` — i.e. the
    /// caller's coarse sync estimate is wrong by 0.12 s, exactly the
    /// scenario the search margin exists to cover. Recovering the correct
    /// per-symbol tones therefore requires the function to (a) extract a
    /// window wide enough to still contain the true signal despite the
    /// caller's `start_sample` missing it by 0.12 s, and (b) preserve
    /// baseband-domain coordinates precisely enough that
    /// `nominal_start_index + dt_bb` lands exactly on the true symbol-0
    /// boundary. A bug in the start_sample/margin bookkeeping (as opposed
    /// to a zero-offset bug that a dt=0 test would miss) would desync every
    /// symbol frame and collapse the recovered-tone fraction.
    #[cfg(feature = "transmit")]
    #[test]
    fn extract_candidate_baseband_recovers_symbols_at_nonzero_dt() {
        use crate::{Ft8Encoder, Ft8Modulator};

        let f_cand = 1503.1_f64;
        let pp = ProtocolParams::ft8();
        let symbols = Ft8Encoder::new()
            .encode_message("CQ K5ARH EM10", None)
            .expect("encode");
        let mut modulator = Ft8Modulator::new(FS as u32, f_cand, 0.9).expect("modulator");
        let audio_f32 = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
        let tx: Vec<f64> = audio_f32.iter().map(|&s| s as f64).collect();

        // dt = +0.12 s of silence prepended: the true signal starts 0.12 s
        // *after* sample 0, which is what the (deliberately wrong) coarse
        // `start_sample = 0` below assumes. Chosen as an exact multiple of
        // `DECIM` (60) so the expected baseband-domain offset is an exact
        // integer, keeping the assertion below free of rounding slop.
        let dt_offset_s = 0.12_f64;
        let dt_audio_samples = (dt_offset_s * FS).round() as usize;
        assert_eq!(
            dt_audio_samples % DECIM,
            0,
            "test fixture: dt offset must be an exact multiple of DECIM"
        );
        let dt_bb = dt_audio_samples / DECIM;

        let mut audio = vec![0.0f64; dt_audio_samples];
        audio.extend_from_slice(&tx);
        // No trailing padding added deliberately: `hi` (start_sample +
        // window + margin) runs past `audio.len()` here, so this also
        // exercises the function's own past-the-end zero-padding path
        // rather than relying on the test to provide a big enough buffer.

        let start_sample: isize = 0; // the coarse (wrong-by-dt) candidate estimate
        let bb = extract_candidate_baseband(&audio, f_cand, start_sample, &pp);

        assert_eq!(bb.samples_per_symbol, 32, "baseband samples per symbol");
        assert_eq!(bb.num_symbols, pp.num_symbols);
        assert!(
            bb.nominal_start_index >= dt_bb,
            "margin ({}) must be able to absorb the dt offset ({dt_bb} bb samples)",
            bb.nominal_start_index
        );

        let mut correct = 0;
        let mut counted = 0;
        for (s, &sym) in symbols.iter().enumerate() {
            let start = bb.nominal_start_index + dt_bb + s * bb.samples_per_symbol;
            if start + bb.samples_per_symbol > bb.samples.len() {
                break;
            }
            let frame = &bb.samples[start..start + bb.samples_per_symbol];
            let mut best_bin = 0usize;
            let mut best_mag = -1.0;
            for tone in 0..8usize {
                let mag = goertzel_mag(frame, tone as f64 / bb.samples_per_symbol as f64);
                if mag > best_mag {
                    best_mag = mag;
                    best_bin = tone;
                }
            }
            counted += 1;
            if best_bin == sym as usize {
                correct += 1;
            }
        }
        assert!(counted > 0);
        let frac = correct as f64 / counted as f64;
        assert!(
            frac >= 0.9,
            "baseband per-symbol tone recovery at dt=+0.12s: {correct}/{counted} \
             ({frac:.2}) < 0.90"
        );
    }
}
