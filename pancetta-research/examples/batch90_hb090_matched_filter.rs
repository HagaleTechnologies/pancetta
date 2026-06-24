//! hb-090 Stage B — phase-coherent matched-filter demod at truth coordinates.
//!
//! Batch 90 Stage A (hb088_osd_without_costas_feasibility +
//! PANCETTA_HB088_STEP_OFFSET sweep) established that the correct
//! row convention for mapping (freq_hz, dt_s) into the example's
//! spectrogram is offset **+2** (controls peak at 91.4% sign-agreement
//! there, confirming Batch 88's SLIDING_FRAME_LOOKBACK_STEPS = 2), and
//! that sub-Costas misses sit at 50.6% sign-agreement at EVERY offset
//! under the existing max-log spectrogram-magnitude demod.
//!
//! hb-090's pre-registered kill-switch: replace the max-log demod with a
//! phase-coherent matched filter at the same (corrected, +2) coordinates.
//!
//!   PROCEED: sub-Costas median sign-agreement >= 70%
//!   WEAK:    58% - 70% (above-noise but below OSD viability)
//!   SHELVE:  < 58%
//!
//! ## What changes vs the hb088 example
//!
//! ONLY the demod front-end. For every evaluated position (control or
//! sub-Costas target) we additionally extract 79 x 8 tone magnitudes by
//! correlating the raw audio against the 8 complex tone templates
//! exp(-j 2π (f0 + k·6.25) t), k = 0..7 (the audio is real, so this is
//! cos + j·sin correlation; |correlation| is the phase-coherent matched
//! filter output). The 8-tone magnitudes (converted to dB) then flow
//! through the IDENTICAL gray-code max-log LLR function the spectrogram
//! path uses, so the only delta is spectrogram-magnitude vs
//! matched-filter front-end.
//!
//! ## Sample-offset mapping (row convention +2)
//!
//! `pos_from_freq_dt` here bakes in STEP_OFFSET = +2:
//!     time_step t0 = round(dt_s / 0.08) + 2.
//! Per the decoder's one-true-convention helper
//! (`candidate_offset_samples` in pancetta-ft8/src/decoder.rs:93, with
//! time_padding = 0 because this example's spectrogram prepends nothing):
//!     start_sample = (t0 - SLIDING_FRAME_LOOKBACK_STEPS) * 960
//!                  = (t0 - 2) * 960
//!                  = round(dt_s / 0.08) * 960  ≈  dt_s * 12000.
//! i.e. with the +2 row convention the time_step ALREADY points at the
//! row whose symbol starts at (t0 - 2)·960; subtracting the same 2 the
//! convention added recovers the plain dt → sample mapping (quantized to
//! the 80 ms step grid, so up to ±480 samples of residual quantization —
//! which is exactly what the optional ±240-sample refinement probes).
//! Symbol s (0..78) then starts at start_sample + s·1920.
//!
//! ## Frequency mapping
//!
//! (f0, fs) from `pos_from_freq_dt` are the 6.25 Hz bin and the 3.125 Hz
//! sub-bin, so the candidate's base tone frequency is
//!     base_hz = f0 · 6.25 + fs · 3.125,
//! and tone k sits at base_hz + k · 6.25 — the same quantization the
//! spectrogram path reads, keeping the two demods at identical
//! coordinates.
//!
//! ## Optional refinement (reported separately)
//!
//! ±240-sample local time refinement per position: shifts
//! −240..=+240 in steps of 60; pick the shift maximizing total filter
//! energy Σ_symbols max_tone |C|². Labeled `mf+refine` in the output.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch90_hb090_matched_filter

use anyhow::Context;
use num_complex::Complex;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, ProtocolParams};
use rustfft::FftPlanner;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

const SAMPLE_RATE: u32 = 12_000;
const FREQ_OSR: usize = 2;
const TIME_OSR: usize = 2;
const NUM_TONES: usize = 8;
const NUM_SYMBOLS: usize = 79;
const NUM_CODEWORD_BITS: usize = 174;
const MIN_SYNC_SCORE: f64 = 3.0;
const SLOT_S: f64 = 15.0;
/// Take the top-K worst hard-200 WAVs.
const TOP_K_WAVS: usize = 20;
/// Stage A's empirically confirmed row convention (+2).
const STEP_OFFSET: i64 = 2;
/// Samples per FT8 symbol @ 12 kHz.
const SAMPLES_PER_SYMBOL: usize = 1920;
/// Samples per spectrogram time step (TIME_OSR = 2).
const SAMPLES_PER_STEP: isize = 960;
/// Tone spacing in Hz.
const TONE_SPACING_HZ: f64 = 6.25;
/// Refinement: shifts −240..=+240 step 60.
const REFINE_RANGE: isize = 240;
const REFINE_STEP: isize = 60;

// ============================================================================
// Spectrogram (matches Ft8Decoder::compute_spectrogram for FT8 @ 12 kHz)
// — identical to the hb088 example; used for sync gating + max-log baseline.
// ============================================================================

struct Spec {
    /// power[time_step][freq_sub][freq_bin] = magnitude in dB
    power: Vec<Vec<Vec<f64>>>,
    num_steps: usize,
    num_bins: usize,
}

fn compute_spectrogram(audio: &[f64]) -> Spec {
    let block_size = 1920; // FT8 samples-per-symbol @ 12 kHz
    let nfft = block_size * FREQ_OSR; // 3840
    let subblock_size = block_size / TIME_OSR; // 960
    let num_bins = block_size / 2 + 1; // 961

    // Pad to at least msg_span + margin steps (79*2 + 50 = 208).
    let msg_span = NUM_SYMBOLS * TIME_OSR;
    let min_steps = msg_span + 50;
    let min_blocks = (min_steps + TIME_OSR - 1) / TIME_OSR;

    let mut audio_v: Vec<f64> = audio.to_vec();
    let num_blocks = audio_v.len() / block_size;
    if num_blocks < min_blocks {
        audio_v.resize(min_blocks * block_size, 0.0);
    }
    let num_blocks = audio_v.len() / block_size;
    let num_steps = num_blocks * TIME_OSR;

    // Hann window with ft8_lib's fft_norm = 2/nfft.
    let fft_norm = 2.0 / nfft as f64;
    let window: Vec<f64> = (0..nfft)
        .map(|i| {
            let x = (std::f64::consts::PI * i as f64 / nfft as f64).sin();
            fft_norm * x * x
        })
        .collect();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(nfft);

    let mut power = Vec::with_capacity(num_steps);
    let mut fft_buf = vec![Complex::new(0.0, 0.0); nfft];
    let mut last_frame = vec![0.0f64; nfft];
    let mut frame_pos = 0usize;

    for _block in 0..num_blocks {
        for _time_sub in 0..TIME_OSR {
            last_frame.copy_within(subblock_size.., 0);
            let new_start = nfft - subblock_size;
            for pos in 0..subblock_size {
                last_frame[new_start + pos] = if frame_pos < audio_v.len() {
                    audio_v[frame_pos]
                } else {
                    0.0
                };
                frame_pos += 1;
            }

            for i in 0..nfft {
                fft_buf[i] = Complex::new(window[i] * last_frame[i], 0.0);
            }
            fft.process(&mut fft_buf);

            let mut sub_power: Vec<Vec<f64>> = Vec::with_capacity(FREQ_OSR);
            for fs in 0..FREQ_OSR {
                let mut row = Vec::with_capacity(num_bins);
                for bin in 0..num_bins {
                    let src_bin = bin * FREQ_OSR + fs;
                    if src_bin < nfft / 2 + 1 {
                        let mag2 = fft_buf[src_bin].norm_sqr();
                        let db = 10.0 * (1e-12_f64 + mag2).log10();
                        row.push(db);
                    } else {
                        row.push(-120.0);
                    }
                }
                sub_power.push(row);
            }
            power.push(sub_power);
        }
    }

    Spec {
        power,
        num_steps,
        num_bins,
    }
}

// ============================================================================
// Costas sync score (matches Ft8Decoder::compute_costas_score)
// ============================================================================

fn compute_costas_score(spec: &Spec, pp: &ProtocolParams, t0: usize, f0: usize, fs: usize) -> f64 {
    let steps_per_symbol = TIME_OSR;
    let mut best_score = 0.0_f64;

    for half in 0..2 {
        let mut score = 0.0_f64;
        let mut num_average = 0usize;

        for (m, &group_start) in pp.costas_positions.iter().enumerate() {
            for k in 0..pp.costas_length {
                let symbol_idx = group_start + k;
                let time_idx = t0 + symbol_idx * steps_per_symbol + half;
                if time_idx >= spec.num_steps {
                    continue;
                }
                let sm = pp.costas_arrays[m][k] as usize;
                let freq_idx = f0 + sm;
                if freq_idx >= spec.num_bins {
                    continue;
                }
                let signal_mag = spec.power[time_idx][fs][freq_idx];

                if sm > 0 && f0 + sm - 1 < spec.num_bins {
                    let neighbor = spec.power[time_idx][fs][f0 + sm - 1];
                    score += signal_mag - neighbor;
                    num_average += 1;
                }
                if sm + 1 < pp.num_tones && f0 + sm + 1 < spec.num_bins {
                    let neighbor = spec.power[time_idx][fs][f0 + sm + 1];
                    score += signal_mag - neighbor;
                    num_average += 1;
                }
                if k > 0 && time_idx >= steps_per_symbol {
                    let prev = time_idx - steps_per_symbol;
                    if prev < spec.num_steps {
                        let neighbor = spec.power[prev][fs][freq_idx];
                        score += signal_mag - neighbor;
                        num_average += 1;
                    }
                }
                if k + 1 < pp.costas_length {
                    let next = time_idx + steps_per_symbol;
                    if next < spec.num_steps {
                        let neighbor = spec.power[next][fs][freq_idx];
                        score += signal_mag - neighbor;
                        num_average += 1;
                    }
                }
            }
        }

        let half_score = if num_average > 0 {
            score / num_average as f64
        } else {
            0.0
        };
        if half_score > best_score {
            best_score = half_score;
        }
    }

    best_score
}

// ============================================================================
// Demod front-end A: spectrogram max-log magnitudes (identical to hb088)
// ============================================================================

/// Returns Vec<[f64; NUM_TONES]> with 79 rows of tone magnitudes (dB).
fn extract_symbols(spec: &Spec, t0: usize, f0: usize, fs: usize) -> Vec<[f64; NUM_TONES]> {
    let mut out = Vec::with_capacity(NUM_SYMBOLS);
    let steps_per_symbol = TIME_OSR;
    for sym_idx in 0..NUM_SYMBOLS {
        let mut mags = [-120.0f64; NUM_TONES];
        let t_base = t0 + sym_idx * steps_per_symbol;
        for tone in 0..NUM_TONES {
            let fb = f0 + tone;
            if fb >= spec.num_bins {
                continue;
            }
            let a = if t_base < spec.num_steps {
                spec.power[t_base][fs][fb]
            } else {
                -120.0
            };
            let b = if t_base + 1 < spec.num_steps {
                spec.power[t_base + 1][fs][fb]
            } else {
                -120.0
            };
            mags[tone] = (a + b) * 0.5;
        }
        out.push(mags);
    }
    out
}

// ============================================================================
// Demod front-end B: phase-coherent matched filter on raw audio
// ============================================================================

/// Phase-coherent matched-filter demod at a fixed sample shift.
///
/// For each of the 79 symbols, correlates the 1920-sample window starting
/// at `start_sample + shift + sym·1920` against the 8 complex tone
/// templates exp(-j 2π (base_hz + k·6.25) n / 12000) and returns both the
/// dB magnitudes (for the LLR path, same units as the spectrogram path up
/// to a constant offset that max-log differences cancel) and the total
/// filter energy Σ_sym max_k |C_k|² (for the refinement search).
///
/// Out-of-range samples contribute zero (phasors still advance so the
/// template stays phase-continuous).
fn matched_filter_symbols(
    audio: &[f64],
    start_sample: isize,
    base_hz: f64,
    shift: isize,
) -> (Vec<[f64; NUM_TONES]>, f64) {
    let dt = 1.0 / SAMPLE_RATE as f64;
    // Per-sample rotation constants for the 8 tones.
    let rot: Vec<Complex<f64>> = (0..NUM_TONES)
        .map(|k| {
            let f = base_hz + k as f64 * TONE_SPACING_HZ;
            Complex::from_polar(1.0, -2.0 * std::f64::consts::PI * f * dt)
        })
        .collect();

    let mut out = Vec::with_capacity(NUM_SYMBOLS);
    let mut total_energy = 0.0f64;
    let n_audio = audio.len() as isize;

    for sym_idx in 0..NUM_SYMBOLS {
        let win_start = start_sample + shift + (sym_idx * SAMPLES_PER_SYMBOL) as isize;
        let mut acc = [Complex::new(0.0f64, 0.0f64); NUM_TONES];
        let mut ph = [Complex::new(1.0f64, 0.0f64); NUM_TONES];
        for m in 0..SAMPLES_PER_SYMBOL as isize {
            let idx = win_start + m;
            if idx >= 0 && idx < n_audio {
                let x = audio[idx as usize];
                for k in 0..NUM_TONES {
                    acc[k] += ph[k] * x;
                }
            }
            for k in 0..NUM_TONES {
                ph[k] *= rot[k];
            }
        }
        let mut mags = [-120.0f64; NUM_TONES];
        let mut best_e = 0.0f64;
        for k in 0..NUM_TONES {
            let e = acc[k].norm_sqr();
            // Same dB form as the spectrogram (10·log10(eps + power)); the
            // absolute scale differs by a constant, which cancels in the
            // max-log LLR differences.
            mags[k] = 10.0 * (1e-12_f64 + e).log10();
            if e > best_e {
                best_e = e;
            }
        }
        total_energy += best_e;
        out.push(mags);
    }
    (out, total_energy)
}

/// Matched filter with ±REFINE_RANGE local time refinement: evaluates
/// shifts −240..=+240 step 60 and returns (mags at best shift, best shift).
fn matched_filter_refined(
    audio: &[f64],
    start_sample: isize,
    base_hz: f64,
) -> (Vec<[f64; NUM_TONES]>, isize) {
    let mut best: Option<(Vec<[f64; NUM_TONES]>, f64, isize)> = None;
    let mut shift = -REFINE_RANGE;
    while shift <= REFINE_RANGE {
        let (mags, energy) = matched_filter_symbols(audio, start_sample, base_hz, shift);
        if best.as_ref().map(|b| energy > b.1).unwrap_or(true) {
            best = Some((mags, energy, shift));
        }
        shift += REFINE_STEP;
    }
    let (mags, _, s) = best.expect("at least one shift evaluated");
    (mags, s)
}

// ============================================================================
// LLR computation — shared by BOTH demods (the only delta is the front-end)
// ============================================================================

/// 174 max-log LLRs from 79 symbols' tone magnitudes (dB).
/// Convention (matches pancetta-ft8 OSD): llr < 0 => bit=1, llr > 0 => bit=0.
fn compute_llrs_db(tone_mags: &[[f64; NUM_TONES]], pp: &ProtocolParams) -> Vec<f32> {
    let mut llrs = Vec::with_capacity(NUM_CODEWORD_BITS);
    for &sym_idx in pp.data_symbol_indices() {
        let mags = &tone_mags[sym_idx];
        let mut s2 = [0.0f64; 8];
        for j in 0..8 {
            let tone_idx = binary_to_gray(j as u8) as usize;
            s2[j] = mags[tone_idx];
        }
        let max4 = |a: f64, b: f64, c: f64, d: f64| -> f64 { a.max(b).max(c.max(d)) };
        let llr0 = max4(s2[4], s2[5], s2[6], s2[7]) - max4(s2[0], s2[1], s2[2], s2[3]);
        let llr1 = max4(s2[2], s2[3], s2[6], s2[7]) - max4(s2[0], s2[1], s2[4], s2[5]);
        let llr2 = max4(s2[1], s2[3], s2[5], s2[7]) - max4(s2[0], s2[2], s2[4], s2[6]);
        llrs.push(-llr0 as f32);
        llrs.push(-llr1 as f32);
        llrs.push(-llr2 as f32);
    }
    debug_assert_eq!(llrs.len(), NUM_CODEWORD_BITS);
    llrs
}

fn binary_to_gray(b: u8) -> u8 {
    b ^ (b >> 1)
}
fn gray_to_binary(g: u8) -> u8 {
    let mut b = g;
    let mut s = g >> 1;
    while s > 0 {
        b ^= s;
        s >>= 1;
    }
    b
}

// ============================================================================
// Tone symbols (length 79) → 174 codeword bits.
// ============================================================================

fn tone_symbols_to_codeword(
    symbols: &[u8; NUM_SYMBOLS],
    pp: &ProtocolParams,
) -> [u8; NUM_CODEWORD_BITS] {
    let mut out = [0u8; NUM_CODEWORD_BITS];
    let mut bit_idx = 0usize;
    for &sym_idx in pp.data_symbol_indices() {
        let gray = symbols[sym_idx];
        let bits3 = gray_to_binary(gray);
        out[bit_idx] = (bits3 >> 2) & 1;
        out[bit_idx + 1] = (bits3 >> 1) & 1;
        out[bit_idx + 2] = bits3 & 1;
        bit_idx += 3;
    }
    debug_assert_eq!(bit_idx, NUM_CODEWORD_BITS);
    out
}

// ============================================================================
// Coordinate mapping (STEP_OFFSET = +2 baked in per Stage A)
// ============================================================================

/// (freq_hz, dt_s) → (time_step, freq_bin, freq_sub).
fn pos_from_freq_dt(freq_hz: f64, dt_s: f64) -> (usize, usize, usize) {
    let sub_bin_hz = TONE_SPACING_HZ / FREQ_OSR as f64; // 3.125
    let total_sub = (freq_hz / sub_bin_hz).round() as i64;
    let freq_bin = (total_sub.max(0) / FREQ_OSR as i64) as usize;
    let freq_sub = (total_sub.max(0) % FREQ_OSR as i64) as usize;
    let step_s = 0.08;
    let time_step = ((dt_s / step_s).round() as i64 + STEP_OFFSET).max(0) as usize;
    (time_step, freq_bin, freq_sub)
}

/// (time_step, freq_bin, freq_sub) → (start_sample, base_hz) for the
/// matched filter. See module docs: with the +2 row convention,
/// start_sample = (t0 − 2)·960 per `candidate_offset_samples`
/// (pancetta-ft8/src/decoder.rs:93, time_padding = 0 here).
fn sample_coords(t0: usize, f0: usize, fs: usize) -> (isize, f64) {
    let start_sample = (t0 as isize - STEP_OFFSET as isize) * SAMPLES_PER_STEP;
    let base_hz = f0 as f64 * TONE_SPACING_HZ + fs as f64 * (TONE_SPACING_HZ / FREQ_OSR as f64);
    (start_sample, base_hz)
}

// ============================================================================
// Sample statistics
// ============================================================================

#[derive(Default, Clone)]
struct LlrStats {
    n: usize,
    mean_abs_llr: f64,
    sum_abs_llr: f64,
    max_abs_llr: f64,
    sign_agreements: Vec<f64>,
}

impl LlrStats {
    fn record(&mut self, llrs: &[f32], truth_bits: &[u8; NUM_CODEWORD_BITS]) {
        let mut sum_abs = 0.0_f64;
        let mut max_abs = 0.0_f64;
        let mut agree = 0usize;
        for i in 0..NUM_CODEWORD_BITS {
            let mag = llrs[i].abs() as f64;
            sum_abs += mag;
            if mag > max_abs {
                max_abs = mag;
            }
            let bit_inferred: u8 = if llrs[i] < 0.0 { 1 } else { 0 };
            if bit_inferred == truth_bits[i] {
                agree += 1;
            }
        }
        let mean_abs = sum_abs / NUM_CODEWORD_BITS as f64;
        self.n += 1;
        self.sum_abs_llr += mean_abs;
        if max_abs > self.max_abs_llr {
            self.max_abs_llr = max_abs;
        }
        self.sign_agreements
            .push(agree as f64 / NUM_CODEWORD_BITS as f64);
        self.mean_abs_llr = self.sum_abs_llr / self.n as f64;
    }
}

fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((p / 100.0) * (s.len() - 1) as f64).round() as usize;
    s[idx.min(s.len() - 1)]
}
fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

// ============================================================================
// IO helpers
// ============================================================================

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == SAMPLE_RATE);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

// ============================================================================
// Main
// ============================================================================

/// All three demods at one position. Returns (maxlog, mf, mf_refined, best_shift).
#[allow(clippy::type_complexity)]
fn eval_position(
    spec: &Spec,
    audio: &[f64],
    pp: &ProtocolParams,
    t0: usize,
    f0: usize,
    fs: usize,
) -> (Vec<f32>, Vec<f32>, Vec<f32>, isize) {
    let tone_mags = extract_symbols(spec, t0, f0, fs);
    let llrs_maxlog = compute_llrs_db(&tone_mags, pp);

    let (start_sample, base_hz) = sample_coords(t0, f0, fs);
    let (mf_mags, _) = matched_filter_symbols(audio, start_sample, base_hz, 0);
    let llrs_mf = compute_llrs_db(&mf_mags, pp);

    let (mf_ref_mags, best_shift) = matched_filter_refined(audio, start_sample, base_hz);
    let llrs_mf_ref = compute_llrs_db(&mf_ref_mags, pp);

    (llrs_maxlog, llrs_mf, llrs_mf_ref, best_shift)
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(TOP_K_WAVS)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    let pp = ProtocolParams::ft8();
    // Stats buckets: [control, sub-Costas] x [maxlog, mf, mf+refine].
    let mut ctl_maxlog = LlrStats::default();
    let mut ctl_mf = LlrStats::default();
    let mut ctl_mf_ref = LlrStats::default();
    let mut sub_maxlog = LlrStats::default();
    let mut sub_mf = LlrStats::default();
    let mut sub_mf_ref = LlrStats::default();
    let mut sub_best_shifts: Vec<f64> = Vec::new();
    let mut ctl_best_shifts: Vec<f64> = Vec::new();
    let mut per_wav: Vec<(String, usize, usize, usize, usize)> = Vec::new();

    eprintln!(
        "hb-090 Stage B: matched-filter vs max-log demod at +2-convention truth coordinates \
         on top-{TOP_K_WAVS} hard-200 WAVs...",
    );

    let cfg = Ft8Config::default();
    let mut encoder = Ft8Encoder::new();

    for (idx, sha) in top_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));

        let baseline: Value = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)?;
        let truths: Vec<(f64, f64, String)> = baseline["decodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        Some((
                            d.get("freq_hz")?.as_f64()?,
                            d.get("dt_s")?.as_f64()?,
                            d.get("message")?.as_str()?.trim().to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };

        let audio_f64: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        let spec = compute_spectrogram(&audio_f64);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let production_decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        let production_msg_set: Vec<(f64, f64, String)> = production_decodes
            .iter()
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset.rem_euclid(SLOT_S),
                    d.text.trim().to_string(),
                )
            })
            .collect();

        let mut wav_recovered = 0usize;
        let mut wav_missed_encodable = 0usize;
        let mut wav_subcostas = 0usize;

        for (freq_hz, dt_s, truth_msg) in &truths {
            let matched = production_msg_set
                .iter()
                .any(|(_, _, pm)| pm.contains(truth_msg) || truth_msg.contains(pm));
            if matched {
                wav_recovered += 1;
                continue;
            }

            let truth_symbols = match encoder.encode_message(truth_msg, None) {
                Ok(s) => s,
                Err(_) => continue,
            };
            wav_missed_encodable += 1;
            let truth_bits = tone_symbols_to_codeword(&truth_symbols, &pp);

            let (t0, f0, fs) = pos_from_freq_dt(*freq_hz, *dt_s);
            if t0 + NUM_SYMBOLS * TIME_OSR + 1 >= spec.num_steps {
                continue;
            }
            if f0 + NUM_TONES >= spec.num_bins {
                continue;
            }
            let sync_score = compute_costas_score(&spec, &pp, t0, f0, fs);
            if sync_score >= MIN_SYNC_SCORE {
                continue; // only the sub-Costas target population
            }
            wav_subcostas += 1;

            let (llrs_maxlog, llrs_mf, llrs_mf_ref, best_shift) =
                eval_position(&spec, &audio_f64, &pp, t0, f0, fs);
            sub_maxlog.record(&llrs_maxlog, &truth_bits);
            sub_mf.record(&llrs_mf, &truth_bits);
            sub_mf_ref.record(&llrs_mf_ref, &truth_bits);
            sub_best_shifts.push(best_shift as f64);
        }

        // Controls: successful production decodes (sanity anchor — matched-
        // filter controls should be >= max-log controls' ~91%; if they're
        // LOW the sample mapping is wrong).
        for d in &production_decodes {
            let txt = d.text.trim().to_string();
            let sym = match encoder.encode_message(&txt, None) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let bits = tone_symbols_to_codeword(&sym, &pp);
            let (t0, f0, fs) =
                pos_from_freq_dt(d.frequency_offset, d.time_offset.rem_euclid(SLOT_S));
            if t0 + NUM_SYMBOLS * TIME_OSR + 1 >= spec.num_steps {
                continue;
            }
            if f0 + NUM_TONES >= spec.num_bins {
                continue;
            }
            let (llrs_maxlog, llrs_mf, llrs_mf_ref, best_shift) =
                eval_position(&spec, &audio_f64, &pp, t0, f0, fs);
            ctl_maxlog.record(&llrs_maxlog, &bits);
            ctl_mf.record(&llrs_mf, &bits);
            ctl_mf_ref.record(&llrs_mf_ref, &bits);
            ctl_best_shifts.push(best_shift as f64);
        }

        per_wav.push((
            sha[..8].to_string(),
            truths.len(),
            wav_recovered,
            wav_missed_encodable,
            wav_subcostas,
        ));
        eprintln!(
            "  [{idx:2}] {} truth={} rec={} missed_w_encode={} sub_costas={}",
            &sha[..8],
            truths.len(),
            wav_recovered,
            wav_missed_encodable,
            wav_subcostas,
        );
    }

    println!(
        "\n=== hb-090 Stage B: matched-filter vs max-log demod (top-{TOP_K_WAVS} hard-200, \
         STEP_OFFSET=+2) ===\n"
    );
    println!("Per-WAV breakdown:");
    println!(
        "  {:>9} {:>6} {:>5} {:>14} {:>11}",
        "sha", "truth", "rec", "miss_w_encode", "sub_costas"
    );
    for w in &per_wav {
        println!("  {:>9} {:>6} {:>5} {:>14} {:>11}", w.0, w.1, w.2, w.3, w.4);
    }

    let row = |label: &str, s: &LlrStats| {
        if s.n == 0 {
            println!("  {label:<26} (empty)");
            return;
        }
        println!(
            "  {label:<26} n={:>4}  agree mean={:5.1}%  p10={:5.1}%  p50={:5.1}%  p90={:5.1}%  \
             mean|LLR|={:7.3}",
            s.n,
            mean(&s.sign_agreements) * 100.0,
            percentile(&s.sign_agreements, 10.0) * 100.0,
            percentile(&s.sign_agreements, 50.0) * 100.0,
            percentile(&s.sign_agreements, 90.0) * 100.0,
            s.mean_abs_llr,
        );
    };

    println!("\nSide-by-side (same positions, same LLR function; only the demod differs):");
    println!("\n[CONTROLS — production-decode positions]");
    row("max-log (spectrogram)", &ctl_maxlog);
    row("matched filter", &ctl_mf);
    row("matched filter +refine", &ctl_mf_ref);
    println!("\n[SUB-COSTAS TARGETS — missed truths, sync < {MIN_SYNC_SCORE}]");
    row("max-log (spectrogram)", &sub_maxlog);
    row("matched filter", &sub_mf);
    row("matched filter +refine", &sub_mf_ref);

    println!(
        "\nRefinement best-shift distribution (samples): controls mean={:.0} p50={:.0}; \
         sub-Costas mean={:.0} p50={:.0}",
        mean(&ctl_best_shifts),
        percentile(&ctl_best_shifts, 50.0),
        mean(&sub_best_shifts),
        percentile(&sub_best_shifts, 50.0),
    );

    // Controls sanity gate.
    let ctl_maxlog_p50 = percentile(&ctl_maxlog.sign_agreements, 50.0) * 100.0;
    let ctl_mf_p50 = percentile(&ctl_mf.sign_agreements, 50.0) * 100.0;
    println!(
        "\nControls sanity: matched-filter p50 = {:.1}% vs max-log p50 = {:.1}%  ({})",
        ctl_mf_p50,
        ctl_maxlog_p50,
        if ctl_mf_p50 >= ctl_maxlog_p50 - 1.0 {
            "OK — sample mapping validated"
        } else {
            "LOW — sample mapping suspect; do NOT trust the sub-Costas numbers"
        },
    );

    // Pre-registered kill-switch on the unrefined matched filter.
    let sub_mf_p50 = percentile(&sub_mf.sign_agreements, 50.0) * 100.0;
    let sub_mf_ref_p50 = percentile(&sub_mf_ref.sign_agreements, 50.0) * 100.0;
    println!(
        "\n--- Pre-registered kill-switch (hb-090) ---\n\
         sub-Costas median sign-agreement, matched filter         = {sub_mf_p50:.1}%\n\
         sub-Costas median sign-agreement, matched filter +refine = {sub_mf_ref_p50:.1}%\n\
         Bars: PROCEED >= 70%, WEAK 58-70%, SHELVE < 58%",
    );
    let verdict_for = |p50: f64| -> &'static str {
        if p50 >= 70.0 {
            "PROCEED"
        } else if p50 >= 58.0 {
            "WEAK"
        } else {
            "SHELVE"
        }
    };
    println!(
        "\nVerdict (matched filter, primary): {}",
        verdict_for(sub_mf_p50)
    );
    println!(
        "Verdict (matched filter +refine, secondary): {}",
        verdict_for(sub_mf_ref_p50)
    );

    Ok(())
}
