//! hb-088 OSD-without-Costas-pre-gate — feasibility / kill-switch diagnostic.
//!
//! Spawned 2026-05-31 from the hb-086 V3 SHELVE (Costas-relaxation surfaces
//! noise, not signal). hb-087 attacks the same wall via callsign priors;
//! this hypothesis is the structurally different sibling: BYPASS Costas
//! pre-gate entirely by feeding residual-extracted LLRs DIRECTLY to OSD's
//! flip-pattern enumeration at positions whose Costas sync_score is below
//! the production `min_sync_score` threshold (so sync_search wouldn't even
//! hand them to LDPC BP).
//!
//! ## Why this might work where V3 didn't
//!
//! V3 ran Costas → BP → OSD at relaxed sync threshold. BP failed → CRC
//! failed → 0 decodes. V3's mechanism trace: 100-131 truly-new candidates
//! per worst-WAV, all BP-converge on noise, ~98% CRC FP-rejected, last 2%
//! plausibility-rejected.
//!
//! This proposal SKIPS BP entirely. OSD's flip enumeration is independent
//! of BP convergence. The structural argument: at a position with a real
//! weak signal, the LLR signs are mostly correct (signal beats noise on
//! most tones) but some bits are wrong; OSD-2's 4095 flip patterns
//! enumerate the most-reliable basis and might find the truth. At a
//! noise-only position, the LLR sign pattern is random and no 2-flip
//! pattern produces a valid codeword + valid CRC.
//!
//! ## Why this might NOT work (kill switch)
//!
//! If LLR magnitudes are too small (sub-Costas positions have weak energy
//! on all tones), the parity-check matrix is poorly conditioned and many
//! flip patterns produce CRC-passing codewords by coincidence. CRC-14
//! catches 1 in 16384 by chance; OSD-2 enumerates 4095 patterns; combined
//! FP rate ~25% per position. At 300+ positions per WAV, that's ~75 FPs
//! per WAV — devastating.
//!
//! ## What this diagnostic measures
//!
//! On refreshed top-20 hard-200 WAVs, the diagnostic:
//!   1. Builds the production-equivalent spectrogram from raw audio.
//!   2. Decodes the WAV with production `Ft8Decoder` to get control
//!      positions (successful decodes — sync_score >= MIN_SYNC_SCORE).
//!   3. For each missed truth (truth not in production decodes): converts
//!      (freq_hz, dt_s) to (freq_bin, time_step, freq_sub); computes
//!      Costas sync_score at that position; extracts 79 symbols' tone
//!      magnitudes; computes 174 max-log-LLRs.
//!   4. For each control position: same LLR extraction.
//!   5. For each truth (target OR control): encodes the truth message
//!      via `Ft8Encoder::encode_message` to get the 79 tone-symbols,
//!      reverses Gray code to recover the 174-bit reference codeword.
//!   6. Reports LLR magnitude distribution and sign-agreement with the
//!      reference codeword for sub-Costas targets vs control.
//!
//! ## Kill-switch criteria
//!
//! PROCEED iff BOTH conditions hold on the aggregate of all sub-Costas
//! missed truths:
//!   (a) mean |LLR| at sub-Costas positions >= 10% of mean |LLR| at
//!       control positions (i.e., not dominated by noise floor); AND
//!   (b) median LLR-sign-agreement with truth codeword >= 85% (so a
//!       2-flip OSD has a reasonable shot at finding the truth — ~26
//!       wrong bits is way over OSD-2's budget; we need fewer than ~14).
//!
//! If only (a) and sign-agreement is in [60%, 85%], note as OSD-4
//! follow-up (not in OSD-2's flip budget but reachable with deeper OSD).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb088_osd_without_costas_feasibility

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

// ============================================================================
// Spectrogram (matches Ft8Decoder::compute_spectrogram for FT8 @ 12 kHz)
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
// Symbol extraction + LLR computation (matches Ft8Decoder paths)
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
// Drop 21 Costas symbols, reverse Gray on the 58 data symbols.
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
        // 3 bits MSB-first per the encoder's generate_symbols_protocol path.
        out[bit_idx] = (bits3 >> 2) & 1;
        out[bit_idx + 1] = (bits3 >> 1) & 1;
        out[bit_idx + 2] = bits3 & 1;
        bit_idx += 3;
    }
    debug_assert_eq!(bit_idx, NUM_CODEWORD_BITS);
    out
}

// ============================================================================
// Coordinate mapping
// ============================================================================

/// (freq_hz, dt_s) → (time_step, freq_bin, freq_sub) for the production
/// spectrogram. tone_spacing = 6.25 Hz, freq_osr = 2 ⇒ 3.125 Hz per
/// sub-bin. samples_per_symbol = 1920 @ 12 kHz; time_osr = 2 ⇒
/// 80 ms per time_step. dt_s = 0 corresponds to t0 = 0 (the Costas
/// alignment offset matches sync_search's t0).
fn pos_from_freq_dt(freq_hz: f64, dt_s: f64) -> (usize, usize, usize) {
    let sub_bin_hz = 6.25 / FREQ_OSR as f64; // 3.125
    let total_sub = (freq_hz / sub_bin_hz).round() as i64;
    let freq_bin = (total_sub.max(0) / FREQ_OSR as i64) as usize;
    let freq_sub = (total_sub.max(0) % FREQ_OSR as i64) as usize;
    // Each time_step is 80 ms (samples_per_symbol / time_osr / 12000).
    let step_s = 0.08;
    // Batch 90 (hb-090 Stage A): the original mapping assumed row t
    // represents the symbol starting at t*960 samples — the same
    // convention bug Batch 88 fixed in the decoder (row t actually
    // represents the symbol starting at (t-2)*960, so the row FOR a
    // symbol at dt is dt/0.08 + 2). Rather than assert the +2, sweep
    // it: PANCETTA_HB088_STEP_OFFSET (default 0 = historical behavior)
    // shifts the row; sign-agreement vs offset locates the true
    // convention empirically.
    let step_offset: i64 = std::env::var("PANCETTA_HB088_STEP_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let time_step = ((dt_s / step_s).round() as i64 + step_offset).max(0) as usize;
    (time_step, freq_bin, freq_sub)
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
    /// fraction in [0, 1] of bits whose LLR sign matches the truth codeword
    sign_agreements: Vec<f64>,
    /// Costas sync_scores at the position the LLRs were extracted from
    sync_scores: Vec<f64>,
}

impl LlrStats {
    fn record(&mut self, llrs: &[f32], truth_bits: &[u8; NUM_CODEWORD_BITS], sync_score: f64) {
        let mut sum_abs = 0.0_f64;
        let mut max_abs = 0.0_f64;
        let mut agree = 0usize;
        for i in 0..NUM_CODEWORD_BITS {
            let mag = llrs[i].abs() as f64;
            sum_abs += mag;
            if mag > max_abs {
                max_abs = mag;
            }
            // LLR sign convention: llr<0 => bit=1, llr>0 => bit=0.
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
        self.sync_scores.push(sync_score);
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
    let mut sub_costas_stats = LlrStats::default();
    let mut control_stats = LlrStats::default();
    let mut all_subcostas_stats = LlrStats::default(); // includes truths whose sync passes too
    let mut per_wav: Vec<(String, usize, usize, usize, usize)> = Vec::new(); // sha, truth, rec, missed_w_encode, sub_costas

    eprintln!(
        "Loading top-{TOP_K_WAVS} hard-200 WAVs, building spectrogram + LLR diagnostic at \
         truth positions vs control positions...",
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

        // Build our spectrogram.
        let audio_f64: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        let spec = compute_spectrogram(&audio_f64);

        // Run production decode for control + missed-truth identification.
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

            // Try to encode the truth message to derive reference codeword bits.
            let truth_symbols = match encoder.encode_message(truth_msg, None) {
                Ok(s) => s,
                Err(_) => continue,
            };
            wav_missed_encodable += 1;
            let truth_bits = tone_symbols_to_codeword(&truth_symbols, &pp);

            // Map (freq, dt) to spectrogram coords.
            let (t0, f0, fs) = pos_from_freq_dt(*freq_hz, *dt_s);
            // Pull tone_magnitudes + LLRs + sync_score from MY spectrogram at
            // that position.
            if t0 + NUM_SYMBOLS * TIME_OSR + 1 >= spec.num_steps {
                continue;
            }
            if f0 + NUM_TONES >= spec.num_bins {
                continue;
            }
            let sync_score = compute_costas_score(&spec, &pp, t0, f0, fs);
            let tone_mags = extract_symbols(&spec, t0, f0, fs);
            let llrs = compute_llrs_db(&tone_mags, &pp);

            all_subcostas_stats.record(&llrs, &truth_bits, sync_score);
            if sync_score < MIN_SYNC_SCORE {
                sub_costas_stats.record(&llrs, &truth_bits, sync_score);
                wav_subcostas += 1;
            }
        }

        // Control: at each successful pancetta decode, extract LLRs and
        // measure against the decoded message's encoded reference. This
        // gives the "what does a real signal look like at OUR spectrogram"
        // anchor.
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
            let sync_score = compute_costas_score(&spec, &pp, t0, f0, fs);
            let tone_mags = extract_symbols(&spec, t0, f0, fs);
            let llrs = compute_llrs_db(&tone_mags, &pp);
            control_stats.record(&llrs, &bits, sync_score);
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

    println!("\n=== hb-088 OSD-without-Costas feasibility (top-{TOP_K_WAVS} hard-200) ===\n");
    println!("Per-WAV breakdown:");
    println!(
        "  {:>9} {:>6} {:>5} {:>14} {:>11}",
        "sha", "truth", "rec", "miss_w_encode", "sub_costas"
    );
    for w in &per_wav {
        println!("  {:>9} {:>6} {:>5} {:>14} {:>11}", w.0, w.1, w.2, w.3, w.4);
    }

    let report = |label: &str, s: &LlrStats| {
        println!("\n[{label}]  n = {}", s.n);
        if s.n == 0 {
            println!("  (empty)");
            return;
        }
        let agree_mean = mean(&s.sign_agreements) * 100.0;
        let agree_p10 = percentile(&s.sign_agreements, 10.0) * 100.0;
        let agree_p50 = percentile(&s.sign_agreements, 50.0) * 100.0;
        let agree_p90 = percentile(&s.sign_agreements, 90.0) * 100.0;
        let sync_mean = mean(&s.sync_scores);
        let sync_p10 = percentile(&s.sync_scores, 10.0);
        let sync_p90 = percentile(&s.sync_scores, 90.0);
        println!(
            "  mean |LLR|      = {:.3}    max |LLR| seen = {:.3}",
            s.mean_abs_llr, s.max_abs_llr,
        );
        println!(
            "  sign-agreement  mean={:.1}%  p10={:.1}%  p50={:.1}%  p90={:.1}%",
            agree_mean, agree_p10, agree_p50, agree_p90,
        );
        println!(
            "  sync_score      mean={:.2}  p10={:.2}  p90={:.2}",
            sync_mean, sync_p10, sync_p90,
        );
    };

    report("CONTROL (production-decode positions)", &control_stats);
    report("MISSED + sub-Costas (target population)", &sub_costas_stats);
    report("MISSED (all encodable, any sync)", &all_subcostas_stats);

    // Kill switch
    let llr_ratio = if control_stats.mean_abs_llr > 0.0 {
        sub_costas_stats.mean_abs_llr / control_stats.mean_abs_llr
    } else {
        0.0
    };
    let agree_median = if sub_costas_stats.sign_agreements.is_empty() {
        0.0
    } else {
        percentile(&sub_costas_stats.sign_agreements, 50.0) * 100.0
    };

    println!(
        "\n--- Kill-switch ---\nLLR-magnitude ratio (sub-Costas / control) = {:.3}  (gate: >= 0.10)",
        llr_ratio,
    );
    println!(
        "Median sign-agreement at sub-Costas positions    = {:.1}%  (OSD-2 gate: >= 85%)",
        agree_median,
    );

    let mag_ok = llr_ratio >= 0.10;
    let agree_ok_osd2 = agree_median >= 85.0;
    let agree_ok_osd4 = (60.0..85.0).contains(&agree_median);

    let verdict = if mag_ok && agree_ok_osd2 {
        format!(
            "PROCEED — sub-Costas LLR magnitudes are within {:.0}% of control AND median \
             sign-agreement is {:.1}% (within OSD-2's flip budget of ~14 wrong bits over 91 \
             info positions). OSD-without-Costas has structural footing; specify implementation.",
            llr_ratio * 100.0,
            agree_median,
        )
    } else if mag_ok && agree_ok_osd4 {
        format!(
            "WEAK PROCEED — LLR magnitudes pass ({:.0}% of control) but sign-agreement is \
             {:.1}% — within OSD-4's reach (3-4 flip budget per LDPC orbit) but NOT OSD-2's. \
             Consider as OSD-4 follow-up only; OSD-4 enumerates C(91,4)=2.6M patterns — \
             expensive. Spec primary as OSD-2; defer OSD-4 variant.",
            llr_ratio * 100.0,
            agree_median,
        )
    } else if !mag_ok {
        format!(
            "SHELVE — sub-Costas LLR magnitudes are only {:.1}% of control. LLRs at \
             sub-Costas positions are dominated by noise; OSD's flip enumeration will \
             produce mostly random CRC-passing codewords (~75 FP/WAV expected). The \
             mechanism does not have signal to find.",
            llr_ratio * 100.0,
        )
    } else {
        format!(
            "SHELVE — even with adequate LLR magnitudes ({:.0}% of control), sign-agreement \
             with truth codeword is only {:.1}% (would require flipping {} bits — far \
             beyond OSD-2's 2-flip budget or OSD-3's 3-flip budget). The LLR sign pattern \
             at sub-Costas positions does not encode the truth.",
            llr_ratio * 100.0,
            agree_median,
            ((100.0 - agree_median) / 100.0 * 174.0) as usize,
        )
    };
    println!("\nVerdict: {verdict}");

    Ok(())
}
