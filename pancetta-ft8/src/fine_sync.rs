//! Task W3.2 (decoder-true-positive-sensitivity plan, Workstream 3) — fine
//! dt/df search on a per-candidate baseband slice.
//!
//! Builds on Task W3.1's [`crate::baseband::extract_candidate_baseband`]: given
//! a per-candidate 200 Hz complex baseband slice (32 samples/symbol for FT8)
//! and the coarse candidate's assumed (dt=0, df=0) position within that slice,
//! [`refine`] searches a fine (dt, df) grid for the true time/frequency offset
//! via noncoherent Costas correlation power, then parabolic-refines both axes
//! to sub-grid precision.
//!
//! This module is standalone and **unwired**: nothing in the live decode path
//! calls [`refine`] yet. Task W3.3 (matched demod) is the planned first
//! caller, per `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
//! Workstream 3.
//!
//! ## Clean-room provenance
//!
//! No GPL source was read to write this module — see the four prose specs
//! below (`research/specs/`).
//!
//! - **Grid structure + noncoherent per-symbol power summation**: modeled on
//!   WSJT-X mainline's `sync8d` fine-sync stage
//!   (`spec-wsjtx-mainline-ft8b.md` Step 2: "correlates the baseband `cd0`
//!   against precomputed 32-sample complex Costas waveforms for each of the 7
//!   tones... Sums power over all 21 (tone × array) correlations") — a
//!   matched-filter correlation per Costas symbol, summed *noncoherently* (as
//!   power, not as a coherent multi-symbol complex sum) over all 21 known
//!   Costas (array, symbol) positions (7 tones/array × 3 arrays for FT8; the
//!   loop is protocol-generic via `ProtocolParams::{costas_positions,
//!   costas_length, costas_arrays}` so it also covers FT4/FT2's differently
//!   shaped Costas layout).
//! - **Two-axis grid sweep** (coarse dt/df enumeration before refinement):
//!   modeled on `spec-ft8mon-sub-bin-costas.md`'s two-nested-loop sub-bin
//!   sweep structure (frequency sub-step outer, time sub-step inner),
//!   simplified here to a direct 2D grid since a per-candidate baseband slice
//!   is already small (a few hundred complex samples) — no cached-global-FFT
//!   trick is needed at this scale, unlike that spec's whole-slot sweep.
//! - **Parabolic sub-grid refinement of both axes**:
//!   `spec-wsjtx-improved-subsample-dt-refinement.md` ("the same parabolic
//!   interpolation can refine the frequency offset... Both stack — one over
//!   time, one over frequency"), applying the textbook three-point parabola
//!   fit (Smith, *Spectral Audio Signal Processing*) independently to each
//!   axis at the 2D grid argmax — i.e. sequential per-axis refinement of one
//!   joint 2D argmax, not a joint 2D parabola fit. This mirrors WSJT-X
//!   mainline's own sequential dt-then-freq-then-dt structure
//!   (`spec-wsjtx-mainline-ft8b.md` Steps 2-4), collapsed to a single grid
//!   pass per axis (acceptable here since the initial grid is already at
//!   1/16-symbol / 0.5 Hz resolution — much finer than mainline's ±10-sample
//!   / ±0.5 Hz coarse-baseband grid, so a second dt pass buys little).
//! - **Undamped parabola core**: reuses
//!   [`crate::decoder::parabolic_peak_refinement`] directly, unscaled. That
//!   function's own math has *no* damping built in — the ×0.3
//!   `Ft8Config::sync_time_interp_delta_scale` factor lives at its call site
//!   in `decoder.rs`'s sync-candidate refinement code, applied *on top of*
//!   the function's raw `(refined_score, delta)` output. That damping exists
//!   because the *existing* coarse-sync code path interpolates a **dB-domain**
//!   score surface (the spectrogram there stores `10*log10(mag^2)` per bin,
//!   see `decoder.rs`'s `Spectrogram`/`compute_spectrogram_with` doc
//!   comments) — a logarithmic transform compresses a peak's curvature
//!   nonlinearly relative to the underlying linear-power signal, so an
//!   unscaled parabola fit through three dB-domain samples over-corrects on
//!   real noisy audio (per that field's doc: "the unscaled (1.0) parabolic
//!   delta over-corrects on noisy real-world audio and regresses recall").
//!   This module's grid values (`costas_power`, a sum of `Complex::norm_sqr`)
//!   are already **linear power**, not dB — the same compensating reason for
//!   damping does not apply, so `refine` calls `parabolic_peak_refinement`
//!   with no post-scale (the returned `delta` is used as-is).

use crate::baseband::BasebandSlice;
use crate::decoder::parabolic_peak_refinement;
use crate::protocol::ProtocolParams;
use num_complex::Complex;

/// Result of a fine dt/df search ([`refine`]'s output).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FineSync {
    /// Refined time offset, in baseband samples (200 Hz), relative to the
    /// [`BasebandSlice::nominal_start_index`] the caller's coarse sync
    /// assumed. Positive means the true signal starts *later* than that
    /// nominal position.
    pub dt_samples: f32,
    /// Refined frequency offset, in Hz, to add to [`BasebandSlice::freq_hz`]
    /// (the candidate carrier the slice was down-mixed against) to land on
    /// the true carrier.
    pub df_hz: f32,
    /// Noncoherent Costas correlation power at the grid point the
    /// refinement was anchored to: the sum of `|correlation|^2` over all
    /// Costas tone/array positions, evaluated at grid (not sub-grid-refined)
    /// resolution. A relative confidence score for this candidate's fine
    /// sync fit, not an SNR estimate.
    pub sync_power: f32,
}

/// dt search half-range, in symbols (±half a symbol, per the design spec).
const DT_HALF_RANGE_SYMBOLS: f64 = 0.5;
/// dt grid step, as a fraction of one symbol (1/16 symbol, per the design spec).
const DT_STEP_SYMBOL_FRACTION: f64 = 1.0 / 16.0;
/// df grid step (Hz), per the design spec.
const DF_STEP_HZ: f64 = 0.5;
/// Minimum df half-range to cover (Hz), per the design spec ("df ∈ ±3.2 Hz").
/// The actual swept half-range is rounded UP to the next whole `DF_STEP_HZ`
/// step (±3.5 Hz) so this minimum is always fully covered rather than
/// undershot by a fractional step.
const DF_MIN_HALF_RANGE_HZ: f64 = 3.2;

/// Maximize noncoherent Costas correlation power over a fine dt/df grid on a
/// per-candidate baseband slice, then parabolic-refine both axes.
///
/// Searches `dt` over `±half a symbol` in `1/16`-symbol steps and `df` over
/// (at least) `±3.2 Hz` in `0.5 Hz` steps, evaluating the summed Costas-tone
/// correlation power at each grid point (`costas_power`), then independently
/// parabola-refines the dt and df axes around the 2D grid argmax to sub-grid
/// precision (see the module doc for why this is safe to do per-axis rather
/// than as a joint 2D fit, and why the refinement is undamped).
///
/// If the 2D argmax falls on the edge of either axis's grid (so a 3-point
/// neighborhood can't be formed on that axis), that axis's refinement is
/// skipped (delta = 0) — the grid-quantized position is returned as-is for
/// that axis, exactly like `parabolic_peak_refinement`'s own "not a local
/// max" fallback.
pub fn refine(bb: &BasebandSlice, pp: &ProtocolParams) -> FineSync {
    let sps = bb.samples_per_symbol as f64;
    let dt_step_samples = (sps * DT_STEP_SYMBOL_FRACTION).round().max(1.0) as isize;
    let dt_half_range_samples = (sps * DT_HALF_RANGE_SYMBOLS).round() as isize;
    let n_dt: isize = (dt_half_range_samples / dt_step_samples).max(1);
    let n_df: isize = (DF_MIN_HALF_RANGE_HZ / DF_STEP_HZ).ceil() as isize;

    let df_count = (2 * n_df + 1) as usize;
    let mut grid = vec![0.0f64; (2 * n_dt + 1) as usize * df_count];
    let idx = |di: isize, dj: isize| -> usize {
        ((di + n_dt) as usize) * df_count + (dj + n_df) as usize
    };

    for di in -n_dt..=n_dt {
        let dt_offset = di * dt_step_samples;
        for dj in -n_df..=n_df {
            let df_hz = dj as f64 * DF_STEP_HZ;
            grid[idx(di, dj)] = costas_power(bb, pp, dt_offset, df_hz);
        }
    }

    // 2D argmax over the grid.
    let (mut best_di, mut best_dj, mut best_val) = (0isize, 0isize, f64::MIN);
    for di in -n_dt..=n_dt {
        for dj in -n_df..=n_df {
            let v = grid[idx(di, dj)];
            if v > best_val {
                best_val = v;
                best_di = di;
                best_dj = dj;
            }
        }
    }

    // Independent per-axis parabolic refinement around the 2D argmax.
    let dt_delta = if best_di > -n_dt && best_di < n_dt {
        let y_left = grid[idx(best_di - 1, best_dj)];
        let y_center = grid[idx(best_di, best_dj)];
        let y_right = grid[idx(best_di + 1, best_dj)];
        parabolic_peak_refinement(y_left, y_center, y_right).1
    } else {
        0.0
    };
    let df_delta = if best_dj > -n_df && best_dj < n_df {
        let y_left = grid[idx(best_di, best_dj - 1)];
        let y_center = grid[idx(best_di, best_dj)];
        let y_right = grid[idx(best_di, best_dj + 1)];
        parabolic_peak_refinement(y_left, y_center, y_right).1
    } else {
        0.0
    };

    let dt_samples = (best_di as f64 + dt_delta) * dt_step_samples as f64;
    let df_hz = (best_dj as f64 + df_delta) * DF_STEP_HZ;

    FineSync {
        dt_samples: dt_samples as f32,
        df_hz: df_hz as f32,
        sync_power: best_val as f32,
    }
}

/// Sum of `|correlation|^2` over every Costas tone/array position (21 for
/// FT8: 7 tones/array × 3 arrays), at a hypothesized `(dt_offset, df_hz)`.
/// A frame that falls outside `bb.samples` (dt pushed past the slice's
/// margin) is skipped rather than panicking — this only matters at the
/// extreme edge of the search grid on a pathologically small baseband slice,
/// and skipping degrades gracefully to a lower (never spuriously inflated)
/// score for that grid point.
fn costas_power(bb: &BasebandSlice, pp: &ProtocolParams, dt_offset: isize, df_hz: f64) -> f64 {
    let sps = bb.samples_per_symbol;
    let mut power = 0.0f64;
    for (m, &group_start) in pp.costas_positions.iter().enumerate() {
        for k in 0..pp.costas_length {
            let symbol_idx = group_start + k;
            let tone = pp.costas_arrays[m][k] as usize;
            let frame_start =
                bb.nominal_start_index as isize + dt_offset + (symbol_idx * sps) as isize;
            if frame_start < 0 {
                continue;
            }
            let frame_start = frame_start as usize;
            let frame_end = frame_start + sps;
            if frame_end > bb.samples.len() {
                continue;
            }
            let frame = &bb.samples[frame_start..frame_end];
            let corr = correlate_tone(frame, tone, sps, df_hz, bb.sample_rate_hz);
            power += corr.norm_sqr();
        }
    }
    power
}

/// Matched-filter correlation of one `sps`-sample baseband frame against the
/// expected FSK tone `tone`, under a hypothesized frequency-offset
/// correction `df_hz`.
///
/// Uses a frame-LOCAL sample index `n` (0..sps) for both the tone-bin
/// reference and the `df_hz` twiddle, rather than a baseband-slice-global
/// index. A physically-continuous `df_hz` phase ramp across the whole slice
/// would additionally carry a constant per-frame phase offset
/// (`2π·df_hz·frame_start/sample_rate_hz`) relative to the local-index
/// version — but that offset multiplies every term of this frame's
/// correlation sum by the SAME unit-magnitude complex factor, so it cannot
/// change `|corr|` (only its arg). Since [`costas_power`] only ever consumes
/// `corr.norm_sqr()`, dropping that offset is an exact simplification, not
/// an approximation — each frame's correlation is self-contained and does
/// not need the other frames' absolute sample positions.
fn correlate_tone(
    frame: &[Complex<f64>],
    tone: usize,
    sps: usize,
    df_hz: f64,
    sample_rate_hz: f64,
) -> Complex<f64> {
    let cycles_per_sample = tone as f64 / sps as f64 + df_hz / sample_rate_hz;
    let w = -2.0 * std::f64::consts::PI * cycles_per_sample;
    let mut acc = Complex::new(0.0, 0.0);
    for (n, &s) in frame.iter().enumerate() {
        let ph = w * n as f64;
        acc += s * Complex::new(ph.cos(), ph.sin());
    }
    acc
}

#[cfg(all(test, feature = "transmit"))]
mod w32_fine_sync_tests {
    use super::*;
    use crate::baseband::{extract_candidate_baseband, DECIM, FS, FS_BB};
    use crate::{Ft8Encoder, Ft8Modulator};
    use rand::{rngs::StdRng, Rng, SeedableRng};

    /// Build a real FT8 tx signal (encoder + modulator, not a hand-rolled
    /// tone) at `carrier_hz`, then place it into an audio buffer such that
    /// its true start sample is offset from an ASSUMED (coarse-candidate)
    /// `start_sample` by exactly `dt_audio_samples` (signed; positive means
    /// the true signal starts later than the assumed position — matching
    /// [`FineSync::dt_samples`]'s sign convention).
    ///
    /// Returns `(audio, assumed_start_sample)` ready to feed to
    /// `extract_candidate_baseband`.
    fn build_offset_signal(
        message: &str,
        carrier_hz: f64,
        dt_audio_samples: isize,
    ) -> (Vec<f64>, isize) {
        let symbols = Ft8Encoder::new()
            .encode_message(message, None)
            .expect("encode");
        let mut modulator = Ft8Modulator::new(FS as u32, carrier_hz, 0.9).expect("modulator");
        let audio_f32 = modulator.modulate_symbols(&symbols, 0.0).expect("modulate");
        let tx: Vec<f64> = audio_f32.iter().map(|&s| s as f64).collect();

        let pad = dt_audio_samples.max(0) as usize;
        let mut audio = vec![0.0f64; pad];
        audio.extend_from_slice(&tx);
        // Trailing padding so a candidate biased toward negative dt (true
        // signal starts before the assumed position) still has enough
        // trailing audio for the extraction's window + margin.
        audio.extend_from_slice(&vec![0.0f64; tx.len()]);

        let true_start = pad as isize;
        let assumed_start_sample = true_start - dt_audio_samples;
        (audio, assumed_start_sample)
    }

    /// Seeded, deterministic real-valued Gaussian noise (Box-Muller over a
    /// seeded PRNG) — used only by the -18 dB statistical test below; the
    /// clean-signal tests add no noise at all.
    fn add_seeded_gaussian_noise(audio: &mut [f64], snr_db: f64, active_len: usize, seed: u64) {
        let signal_power: f64 = audio[..active_len.min(audio.len())]
            .iter()
            .map(|&x| x * x)
            .sum::<f64>()
            / active_len.max(1) as f64;
        let snr_linear = 10f64.powf(snr_db / 10.0);
        let noise_std = (signal_power / snr_linear).sqrt();

        let mut rng = StdRng::seed_from_u64(seed);
        let mut i = 0;
        while i < audio.len() {
            let u1: f64 = rng.random::<f64>().max(1e-12);
            let u2: f64 = rng.random::<f64>();
            let r = (-2.0 * u1.ln()).sqrt();
            let theta = 2.0 * std::f64::consts::PI * u2;
            audio[i] += r * theta.cos() * noise_std;
            i += 1;
            if i < audio.len() {
                audio[i] += r * theta.sin() * noise_std;
                i += 1;
            }
        }
    }

    /// dt error in milliseconds between a `FineSync` result and the known
    /// true `(dt_audio_samples)` offset used to build the test signal.
    fn dt_error_ms(fs: &FineSync, dt_audio_samples: isize) -> f64 {
        let expected_dt_bb = dt_audio_samples as f64 / DECIM as f64;
        let error_samples = fs.dt_samples as f64 - expected_dt_bb;
        error_samples / FS_BB * 1000.0
    }

    fn df_error_hz(fs: &FineSync, true_df_hz: f64) -> f64 {
        fs.df_hz as f64 - true_df_hz
    }

    /// TDD RED->GREEN anchor: at exactly zero dt/df (the trivial point),
    /// `refine` should report ~zero on both axes with a healthy sync power
    /// (nonzero — a real Costas-locked signal, not a degenerate grid).
    #[test]
    fn zero_offset_recovers_near_zero_dt_and_df() {
        let pp = ProtocolParams::ft8();
        let f_cand = 1500.0_f64;
        let (audio, start_sample) = build_offset_signal("CQ K5ARH EM10", f_cand, 0);
        let bb = extract_candidate_baseband(&audio, f_cand, start_sample, &pp);
        let fs = refine(&bb, &pp);

        assert!(
            dt_error_ms(&fs, 0).abs() <= 10.0,
            "dt_samples={}",
            fs.dt_samples
        );
        assert!(df_error_hz(&fs, 0.0).abs() <= 0.2, "df_hz={}", fs.df_hz);
        assert!(
            fs.sync_power > 0.0,
            "sync_power should be positive for a real signal"
        );
    }

    /// Clean-signal tolerance sweep: draws (dt, df) pairs across the search
    /// grid (interior grid-aligned points spanning most of the ±half-symbol
    /// / ±3.2 Hz range) and asserts every one meets the design spec's clean
    /// tolerance: |dt error| <= 10 ms, |df error| <= 0.2 Hz.
    #[test]
    fn clean_signal_meets_tolerance_across_the_grid() {
        let pp = ProtocolParams::ft8();
        let f_cand = 1500.0_f64;
        // dt test points in seconds (multiples of 1/16 symbol = 10 ms, and
        // of 1/DECIM audio-sample granularity), spanning most of the
        // +-half-symbol (+-80 ms) search range.
        let dt_test_s = [-0.07, -0.03, 0.0, 0.04, 0.07];
        // df test points in Hz (multiples of the 0.5 Hz grid step), spanning
        // most of the +-3.2 Hz (actual grid +-3.5 Hz) search range.
        let df_test_hz = [-3.0, -1.5, 0.0, 1.5, 3.0];

        let mut worst_dt_ms = 0.0_f64;
        let mut worst_df_hz = 0.0_f64;
        for &dt_s in &dt_test_s {
            let dt_audio_samples = (dt_s * FS).round() as isize;
            assert_eq!(
                dt_audio_samples % DECIM as isize,
                0,
                "test fixture: dt offset must be an exact multiple of DECIM"
            );
            for &true_df in &df_test_hz {
                let carrier_hz = f_cand + true_df;
                let (audio, start_sample) =
                    build_offset_signal("CQ K5ARH EM10", carrier_hz, dt_audio_samples);
                let bb = extract_candidate_baseband(&audio, f_cand, start_sample, &pp);
                let fs = refine(&bb, &pp);

                let dt_err = dt_error_ms(&fs, dt_audio_samples).abs();
                let df_err = df_error_hz(&fs, true_df).abs();
                worst_dt_ms = worst_dt_ms.max(dt_err);
                worst_df_hz = worst_df_hz.max(df_err);

                assert!(
                    dt_err <= 10.0,
                    "dt_s={dt_s} true_df={true_df}: dt error {dt_err:.3} ms > 10 ms \
                     (fs.dt_samples={}, fs.df_hz={})",
                    fs.dt_samples,
                    fs.df_hz
                );
                assert!(
                    df_err <= 0.2,
                    "dt_s={dt_s} true_df={true_df}: df error {df_err:.3} Hz > 0.2 Hz \
                     (fs.dt_samples={}, fs.df_hz={})",
                    fs.dt_samples,
                    fs.df_hz
                );
            }
        }
        eprintln!(
            "clean_signal_meets_tolerance_across_the_grid: worst dt={worst_dt_ms:.3} ms, \
             worst df={worst_df_hz:.3} Hz over {} points",
            dt_test_s.len() * df_test_hz.len()
        );
    }

    /// Sub-grid precision check: a (dt, df) pair deliberately NOT aligned to
    /// any grid point (dt = 17 ms is not a multiple of the 10 ms grid step;
    /// df = 1.1 Hz is not a multiple of the 0.5 Hz grid step) must still be
    /// recovered within tolerance — this is the parabolic refinement's job,
    /// not just the coarse grid's.
    #[test]
    fn clean_signal_off_grid_point_recovered_via_parabolic_refinement() {
        let pp = ProtocolParams::ft8();
        let f_cand = 1500.0_f64;
        let dt_s = 0.017; // not a multiple of the 10 ms (2-baseband-sample) grid step
        let true_df = 1.1_f64; // not a multiple of the 0.5 Hz grid step
                               // Round to the nearest exact-DECIM-multiple of audio samples so the
                               // baseband-domain expectation used by `dt_error_ms` is exact (the
                               // baseband domain only has 1/DECIM = 1/60th the audio-domain
                               // resolution). 0.017s * 12000 = 204 samples -> nearest multiple of
                               // DECIM=60 is 180 (15 ms = 3 baseband samples) -- still off the
                               // dt search grid's step of 2 baseband samples, so this remains a
                               // genuine off-grid-point test.
        let dt_audio_samples = ((dt_s * FS) / DECIM as f64).round() as isize * DECIM as isize;
        assert_eq!(dt_audio_samples % DECIM as isize, 0);

        let (audio, start_sample) =
            build_offset_signal("CQ K5ARH EM10", f_cand + true_df, dt_audio_samples);
        let bb = extract_candidate_baseband(&audio, f_cand, start_sample, &pp);
        let fs = refine(&bb, &pp);

        let dt_err = dt_error_ms(&fs, dt_audio_samples).abs();
        let df_err = df_error_hz(&fs, true_df).abs();
        assert!(dt_err <= 10.0, "off-grid dt error {dt_err:.3} ms > 10 ms");
        assert!(df_err <= 0.2, "off-grid df error {df_err:.3} Hz > 0.2 Hz");
    }

    /// -18 dB AWGN statistical tolerance (design spec): median dt error over
    /// 50 seeded trials <= 20 ms, median df error <= 0.5 Hz. Each trial draws
    /// a fresh (dt, df) pair (deterministically, from the seed) across the
    /// search grid and a fresh noise realization, so this is a genuine
    /// distribution measurement, not one lucky/unlucky draw.
    #[test]
    fn minus_18db_awgn_meets_median_tolerance_over_50_trials() {
        let pp = ProtocolParams::ft8();
        let f_cand = 1500.0_f64;
        const N_TRIALS: usize = 50;
        const SNR_DB: f64 = -18.0;

        let mut dt_errors_ms = Vec::with_capacity(N_TRIALS);
        let mut df_errors_hz = Vec::with_capacity(N_TRIALS);

        for trial in 0..N_TRIALS {
            // Deterministic per-trial (dt, df) draw from a seeded RNG,
            // spanning the same interior range as the clean-signal grid
            // sweep above.
            let mut draw_rng = StdRng::seed_from_u64(0xF17E_0000 + trial as u64);
            let dt_s = draw_rng.random_range(-0.07..=0.07);
            let true_df = draw_rng.random_range(-3.0..=3.0);

            let dt_audio_samples = ((dt_s * FS) / DECIM as f64).round() as isize * DECIM as isize;
            let (mut audio, start_sample) =
                build_offset_signal("CQ K5ARH EM10", f_cand + true_df, dt_audio_samples);

            // Active tx region for the SNR power reference: from the true
            // start through one full window (matches build_offset_signal's
            // tx length before its trailing pad).
            let tx_len_samples = pp.total_samples(FS as u32);
            let active_len = (dt_audio_samples.max(0) as usize + tx_len_samples).min(audio.len());
            add_seeded_gaussian_noise(&mut audio, SNR_DB, active_len, 0xA5A5_0000 + trial as u64);

            let bb = extract_candidate_baseband(&audio, f_cand, start_sample, &pp);
            let fs = refine(&bb, &pp);

            dt_errors_ms.push(dt_error_ms(&fs, dt_audio_samples).abs());
            df_errors_hz.push(df_error_hz(&fs, true_df).abs());
        }

        let median = |v: &mut Vec<f64>| -> f64 {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = v.len();
            if n % 2 == 1 {
                v[n / 2]
            } else {
                0.5 * (v[n / 2 - 1] + v[n / 2])
            }
        };
        let median_dt_ms = median(&mut dt_errors_ms);
        let median_df_hz = median(&mut df_errors_hz);

        eprintln!(
            "minus_18db_awgn_meets_median_tolerance_over_50_trials: \
             median dt error = {median_dt_ms:.3} ms, median df error = {median_df_hz:.3} Hz \
             (N={N_TRIALS}, SNR={SNR_DB} dB)"
        );

        assert!(
            median_dt_ms <= 20.0,
            "median dt error {median_dt_ms:.3} ms > 20 ms over {N_TRIALS} trials at {SNR_DB} dB"
        );
        assert!(
            median_df_hz <= 0.5,
            "median df error {median_df_hz:.3} Hz > 0.5 Hz over {N_TRIALS} trials at {SNR_DB} dB"
        );
    }

    /// Grid-edge safety: an argmax landing at the very edge of either axis's
    /// search grid must not panic (no 3-point neighborhood to refine)."]
    #[test]
    fn does_not_panic_on_degenerate_all_zero_baseband() {
        let pp = ProtocolParams::ft8();
        // An all-zero baseband slice (e.g. a candidate baseband extraction
        // that came back empty/short) must not panic — every grid point
        // ties at 0.0 power, so the "first" (most-negative-index) point
        // wins deterministically and both axis refinements skip (flat
        // input, non-concave).
        let bb = BasebandSlice {
            samples: vec![Complex::new(0.0, 0.0); 32 * (pp.num_symbols + 4)],
            sample_rate_hz: FS_BB,
            samples_per_symbol: 32,
            num_symbols: pp.num_symbols,
            nominal_start_index: 64,
            margin_samples: 64,
            start_sample: 0,
            freq_hz: 1500.0,
        };
        let fs = refine(&bb, &pp);
        assert_eq!(fs.sync_power, 0.0);
    }
}
