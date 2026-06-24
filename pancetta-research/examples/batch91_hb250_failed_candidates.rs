//! hb-250 premise probe (Batch 91) — matched-filter re-demod of
//! sync-passing, LDPC/CRC-failing candidates.
//!
//! Batch 90 (hb-090 Stage B) showed the phase-coherent matched filter
//! beats the production spectrogram max-log demod on REAL decoded
//! signals (controls p50 96.6% vs 94.3% with ±240-sample refinement)
//! while staying at chance on sub-Costas missed truths. hb-250 asks the
//! follow-on question: at positions where signal demonstrably exists —
//! sync candidates that PASSED the Costas threshold but FAILED to
//! produce a CRC-valid decode at any pass — does the matched filter
//! produce LLRs good enough to converge some of them?
//!
//! ## Pipeline
//!
//! 1. For each WAV in the probe corpus (top-20 hard-200 worst WAVs +
//!    the 20 Batch-86 kill-switch slots), run the production decoder via
//!    `Ft8Decoder::decode_window_with_candidate_dump` (Batch 91's
//!    additive `#[doc(hidden)]` diagnostic entry point) to obtain every
//!    sync candidate that entered the per-pass candidate loop, tagged
//!    with whether that loop decoded it.
//! 2. Failed candidates = dump records with `decoded == false`, deduped
//!    on the (3.125 Hz, 0.08 s) candidate grid, excluding positions
//!    within (6.25 Hz, 0.16 s) of any successful decode (those are
//!    sidekick candidates of signals pancetta already has).
//! 3. Truth-adjacent = failed candidates within (6.25 Hz, 0.16 s) of an
//!    ft8_lib truth (`research/baselines/ft8/{sha}.ft8lib.json`, real
//!    freq/time post-Batch-85) whose hash-normalized text production
//!    did NOT decode — i.e. the candidate was pointing at a real signal
//!    pancetta missed.
//! 4. For each truth-adjacent failed candidate, evaluate sign-agreement
//!    of the truth codeword (encoder round-trip; hash-token texts that
//!    fail re-encode are skipped and counted) against three demods AT
//!    THE CANDIDATE'S OWN coordinates (production won't have truth):
//!    spectrogram max-log, matched filter, matched filter + ±240-sample
//!    refinement. Scaffolding reused from
//!    `batch90_hb090_matched_filter.rs` (+2 row convention validated
//!    there at 91–96% on controls).
//!
//! ## Pre-registered read-out (from the Batch 91 brief)
//!
//! Mechanism ALIVE if matched-filter sign-agreement at truth-adjacent
//! failed candidates has
//!   - median >= 85%                       (OSD-2 viable), OR
//!   - median >= 75% AND >= +8 points over the spectrogram demod at the
//!     same positions                       (BP-with-better-LLRs viable).
//! Below that: SHELVE hb-250 with the distribution recorded.
//! If truth-adjacent failed candidates number < ~30 across both
//! corpora: SHELVE-POPULATION regardless of agreement.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch91_hb250_failed_candidates

use anyhow::Context;
use num_complex::Complex;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, ProtocolParams, SyncCandidateRecord};
use pancetta_research::metrics::hash_normalize_message;
use rustfft::FftPlanner;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const SAMPLE_RATE: u32 = 12_000;
const FREQ_OSR: usize = 2;
const TIME_OSR: usize = 2;
const NUM_TONES: usize = 8;
const NUM_SYMBOLS: usize = 79;
const NUM_CODEWORD_BITS: usize = 174;
/// Take the top-K worst hard-200 WAVs (same selection as Batch 90).
const TOP_K_WAVS: usize = 20;
/// Stage A's empirically confirmed row convention (+2).
const STEP_OFFSET: i64 = 2;
/// Samples per FT8 symbol @ 12 kHz.
const SAMPLES_PER_SYMBOL: usize = 1920;
/// Tone spacing in Hz.
const TONE_SPACING_HZ: f64 = 6.25;
/// Refinement: shifts −240..=+240 step 60.
const REFINE_RANGE: isize = 240;
const REFINE_STEP: isize = 60;
/// Truth/success adjacency gates (pre-registered in the Batch 91 brief).
const GATE_FREQ_HZ: f64 = 6.25;
const GATE_DT_S: f64 = 0.16;
/// Pre-registered population floor.
const MIN_POPULATION: usize = 30;

// ============================================================================
// Spectrogram (matches Ft8Decoder::compute_spectrogram for FT8 @ 12 kHz)
// — identical to the batch90 example; used for the max-log baseline demod.
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
// Demod front-end A: spectrogram max-log magnitudes (identical to batch90)
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
// Demod front-end B: phase-coherent matched filter (identical to batch90)
// ============================================================================

/// Phase-coherent matched-filter demod at a fixed sample shift.
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

/// Matched filter with ±REFINE_RANGE local time refinement.
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

/// Tone symbols (length 79) → 174 codeword bits.
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
// Coordinate mapping (STEP_OFFSET = +2 baked in per Batch 90 Stage A)
// ============================================================================

/// (freq_hz, dt_s) → (time_step, freq_bin, freq_sub) for the example's
/// unpadded spectrogram. Returns None when dt is negative enough that
/// the +2 row convention would clamp (the decoder's padded spectrogram
/// can represent those; this example's cannot).
fn pos_from_freq_dt(freq_hz: f64, dt_s: f64) -> Option<(usize, usize, usize)> {
    let sub_bin_hz = TONE_SPACING_HZ / FREQ_OSR as f64; // 3.125
    let total_sub = (freq_hz / sub_bin_hz).round() as i64;
    if total_sub < 0 {
        return None;
    }
    let freq_bin = (total_sub / FREQ_OSR as i64) as usize;
    let freq_sub = (total_sub % FREQ_OSR as i64) as usize;
    let step_s = 0.08;
    let time_step = (dt_s / step_s).round() as i64 + STEP_OFFSET;
    if time_step < 0 {
        return None;
    }
    Some((time_step as usize, freq_bin, freq_sub))
}

// ============================================================================
// Sample statistics
// ============================================================================

#[derive(Default, Clone)]
struct LlrStats {
    n: usize,
    mean_abs_llr: f64,
    sum_abs_llr: f64,
    sign_agreements: Vec<f64>,
}

impl LlrStats {
    fn record(&mut self, llrs: &[f32], truth_bits: &[u8; NUM_CODEWORD_BITS]) -> f64 {
        let mut sum_abs = 0.0_f64;
        let mut agree = 0usize;
        for i in 0..NUM_CODEWORD_BITS {
            sum_abs += llrs[i].abs() as f64;
            let bit_inferred: u8 = if llrs[i] < 0.0 { 1 } else { 0 };
            if bit_inferred == truth_bits[i] {
                agree += 1;
            }
        }
        self.n += 1;
        self.sum_abs_llr += sum_abs / NUM_CODEWORD_BITS as f64;
        self.mean_abs_llr = self.sum_abs_llr / self.n as f64;
        let frac = agree as f64 / NUM_CODEWORD_BITS as f64;
        self.sign_agreements.push(frac);
        frac
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

/// Load ft8_lib truth decodes for `sha`: (freq_hz, dt_s, message).
fn load_ft8lib_truth(ws: &PathBuf, sha: &str) -> anyhow::Result<Vec<(f64, f64, String)>> {
    let p = ws.join(format!("research/baselines/ft8/{sha}.ft8lib.json"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).context(format!(
        "ft8_lib truth missing for {sha} — run batch71_ft8lib_truth_all"
    ))?)?;
    Ok(v["decodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    Some((
                        d.get("freq_hz")?.as_f64()?,
                        d.get("time_sec")?.as_f64()?,
                        d.get("message")?.as_str()?.trim().to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

// ============================================================================
// Main
// ============================================================================

/// One deduped failed candidate.
#[derive(Clone, Copy)]
struct FailedCandidate {
    freq_hz: f64,
    dt_s: f64,
    start_sample: isize,
    sync_score: f64,
    first_pass: usize,
}

fn within_gate(freq_a: f64, dt_a: f64, freq_b: f64, dt_b: f64) -> bool {
    (freq_a - freq_b).abs() <= GATE_FREQ_HZ && (dt_a - dt_b).abs() <= GATE_DT_S
}

#[allow(clippy::too_many_arguments)]
fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    // ---- Corpus A: top-20 hard-200 worst WAVs (same selection as Batch 90)
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
    let mut wav_list: Vec<(String, PathBuf, &'static str)> = Vec::new();
    for sha in &top_hashes {
        if let Some(p) = path_by_sha.get(sha) {
            wav_list.push((sha.clone(), PathBuf::from(p), "hard200"));
        } else {
            eprintln!("  sha {} not in hard_200 manifest — skip", &sha[..8]);
        }
    }

    // ---- Corpus B: 20 Batch-86 kill-switch slots (raw-corpus representation)
    let ks: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hb104_kill_switch.json"),
    )?)?;
    for e in ks["entries"].as_array().context("kill-switch entries")? {
        wav_list.push((
            e["wav_sha256"].as_str().unwrap().to_string(),
            PathBuf::from(e["wav_path"].as_str().unwrap()),
            "killsw",
        ));
    }

    let pp = ProtocolParams::ft8();
    let cfg = Ft8Config::default();
    let mut encoder = Ft8Encoder::new();

    // Aggregates.
    let mut total_dump = 0usize;
    let mut total_failed = 0usize;
    let mut total_truth_adjacent = 0usize;
    let mut skipped_encode = 0usize;
    let mut skipped_coords = 0usize;
    let mut unique_truths_covered: usize = 0;
    let mut ta_sync_scores: Vec<f64> = Vec::new();
    let mut ta_best_shifts: Vec<f64> = Vec::new();
    // Stats per corpus and combined: [maxlog, mf, mf+refine].
    let mut ta_maxlog = LlrStats::default();
    let mut ta_mf = LlrStats::default();
    let mut ta_mf_ref = LlrStats::default();
    let mut ta_maxlog_by: HashMap<&'static str, LlrStats> = HashMap::new();
    let mut ta_mf_by: HashMap<&'static str, LlrStats> = HashMap::new();
    let mut ta_mf_ref_by: HashMap<&'static str, LlrStats> = HashMap::new();
    // Controls sanity (production decode positions, both corpora pooled).
    let mut ctl_maxlog = LlrStats::default();
    let mut ctl_mf_ref = LlrStats::default();

    eprintln!(
        "hb-250 premise probe: failed-candidate matched-filter re-demod on {} WAVs \
         (top-{TOP_K_WAVS} hard-200 + {} kill-switch slots)...",
        wav_list.len(),
        wav_list.iter().filter(|w| w.2 == "killsw").count(),
    );

    println!(
        "{:<7} {:>9} {:>6} {:>5} {:>7} {:>6} {:>7} {:>9} {:>8}",
        "corpus", "sha", "truth", "prod", "missed", "dump", "failed", "truth_adj", "ta_eval"
    );

    for (sha, wav_path, corpus) in &wav_list {
        let truths = load_ft8lib_truth(&ws, sha)?;
        let samples = match load_wav(wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };
        let audio_f64: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
        let spec = compute_spectrogram(&audio_f64);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let (production_decodes, dump) = decoder
            .decode_window_with_candidate_dump(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window_with_candidate_dump: {e}"))?;

        // Production text set (hash-normalized) + success positions.
        let prod_texts: HashSet<String> = production_decodes
            .iter()
            .map(|d| hash_normalize_message(d.text.trim()))
            .collect();
        let mut success_pos: Vec<(f64, f64)> = production_decodes
            .iter()
            .map(|d| (d.frequency_offset, d.time_offset))
            .collect();
        for r in dump.iter().filter(|r| r.decoded) {
            success_pos.push((r.freq_hz, r.dt_s));
        }

        // Missed truths = ft8_lib truths production didn't decode (text-level).
        let missed_truths: Vec<&(f64, f64, String)> = truths
            .iter()
            .filter(|(_, _, msg)| !prod_texts.contains(&hash_normalize_message(msg)))
            .collect();

        // Failed candidates: decoded == false, deduped on the candidate
        // grid, not adjacent to any success position.
        let mut failed_by_key: HashMap<(i64, i64), FailedCandidate> = HashMap::new();
        for r in dump.iter().filter(|r: &&SyncCandidateRecord| !r.decoded) {
            let key = (
                (r.freq_hz / (TONE_SPACING_HZ / FREQ_OSR as f64)).round() as i64,
                (r.dt_s / 0.08).round() as i64,
            );
            let e = failed_by_key.entry(key).or_insert(FailedCandidate {
                freq_hz: r.freq_hz,
                dt_s: r.dt_s,
                start_sample: r.start_sample,
                sync_score: r.sync_score,
                first_pass: r.pass,
            });
            if r.sync_score > e.sync_score {
                e.sync_score = r.sync_score;
            }
            if r.pass < e.first_pass {
                e.first_pass = r.pass;
            }
        }
        let failed: Vec<FailedCandidate> = failed_by_key
            .into_values()
            .filter(|c| {
                !success_pos
                    .iter()
                    .any(|(f, t)| within_gate(c.freq_hz, c.dt_s, *f, *t))
            })
            .collect();

        // Truth-adjacent failed candidates → evaluate demods.
        let mut wav_truth_adjacent = 0usize;
        let mut wav_evaluated = 0usize;
        let mut wav_truths_hit: HashSet<usize> = HashSet::new();
        for c in &failed {
            // Nearest missed truth within the gate (by freq distance, then dt).
            let mut best: Option<(usize, f64)> = None;
            for (ti, (tf, tt, _)) in missed_truths.iter().enumerate() {
                if within_gate(c.freq_hz, c.dt_s, *tf, *tt) {
                    let d = (c.freq_hz - tf).abs() + (c.dt_s - tt).abs();
                    if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                        best = Some((ti, d));
                    }
                }
            }
            let Some((ti, _)) = best else { continue };
            wav_truth_adjacent += 1;
            ta_sync_scores.push(c.sync_score);

            let (_, _, truth_msg) = missed_truths[ti];
            let truth_symbols = match encoder.encode_message(truth_msg, None) {
                Ok(s) => s,
                Err(_) => {
                    skipped_encode += 1;
                    continue;
                }
            };
            let truth_bits = tone_symbols_to_codeword(&truth_symbols, &pp);

            // Spectrogram max-log at the candidate's own coordinates.
            let Some((t0, f0, fs)) = pos_from_freq_dt(c.freq_hz, c.dt_s) else {
                skipped_coords += 1;
                continue;
            };
            if t0 + NUM_SYMBOLS * TIME_OSR + 1 >= spec.num_steps || f0 + NUM_TONES >= spec.num_bins
            {
                skipped_coords += 1;
                continue;
            }
            wav_truths_hit.insert(ti);
            wav_evaluated += 1;

            let tone_mags = extract_symbols(&spec, t0, f0, fs);
            let llrs_maxlog = compute_llrs_db(&tone_mags, &pp);

            // Matched filter at the candidate's own (start_sample, freq).
            let (mf_mags, _) = matched_filter_symbols(&audio_f64, c.start_sample, c.freq_hz, 0);
            let llrs_mf = compute_llrs_db(&mf_mags, &pp);
            let (mf_ref_mags, best_shift) =
                matched_filter_refined(&audio_f64, c.start_sample, c.freq_hz);
            let llrs_mf_ref = compute_llrs_db(&mf_ref_mags, &pp);
            ta_best_shifts.push(best_shift as f64);

            ta_maxlog.record(&llrs_maxlog, &truth_bits);
            ta_mf.record(&llrs_mf, &truth_bits);
            ta_mf_ref.record(&llrs_mf_ref, &truth_bits);
            ta_maxlog_by
                .entry(corpus)
                .or_default()
                .record(&llrs_maxlog, &truth_bits);
            ta_mf_by
                .entry(corpus)
                .or_default()
                .record(&llrs_mf, &truth_bits);
            ta_mf_ref_by
                .entry(corpus)
                .or_default()
                .record(&llrs_mf_ref, &truth_bits);
        }
        unique_truths_covered += wav_truths_hit.len();

        // Controls sanity (sampled: every production decode).
        for d in &production_decodes {
            let txt = d.text.trim().to_string();
            let sym = match encoder.encode_message(&txt, None) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let bits = tone_symbols_to_codeword(&sym, &pp);
            let Some((t0, f0, fs)) = pos_from_freq_dt(d.frequency_offset, d.time_offset) else {
                continue;
            };
            if t0 + NUM_SYMBOLS * TIME_OSR + 1 >= spec.num_steps || f0 + NUM_TONES >= spec.num_bins
            {
                continue;
            }
            let start_sample = (d.time_offset * SAMPLE_RATE as f64).round() as isize;
            let tone_mags = extract_symbols(&spec, t0, f0, fs);
            ctl_maxlog.record(&compute_llrs_db(&tone_mags, &pp), &bits);
            let (mf_ref_mags, _) =
                matched_filter_refined(&audio_f64, start_sample, d.frequency_offset);
            ctl_mf_ref.record(&compute_llrs_db(&mf_ref_mags, &pp), &bits);
        }

        total_dump += dump.len();
        total_failed += failed.len();
        total_truth_adjacent += wav_truth_adjacent;

        println!(
            "{:<7} {:>9} {:>6} {:>5} {:>7} {:>6} {:>7} {:>9} {:>8}",
            corpus,
            &sha[..8],
            truths.len(),
            production_decodes.len(),
            missed_truths.len(),
            dump.len(),
            failed.len(),
            wav_truth_adjacent,
            wav_evaluated,
        );
    }

    println!("\n=== hb-250 premise probe: aggregate ===\n");
    println!("Total dump records (all passes):          {total_dump}");
    println!("Failed candidates (deduped, non-success): {total_failed}");
    println!(
        "Truth-adjacent failed candidates:         {total_truth_adjacent} ({:.1}% of failed)",
        100.0 * total_truth_adjacent as f64 / total_failed.max(1) as f64
    );
    println!("  skipped (truth not re-encodable):       {skipped_encode}");
    println!("  skipped (coords out of example spec):   {skipped_coords}");
    println!("  evaluated:                              {}", ta_mf.n);
    println!("  unique missed truths covered:           {unique_truths_covered}");
    println!(
        "  truth-adjacent sync scores: mean={:.1} p50={:.1} p90={:.1}",
        mean(&ta_sync_scores),
        percentile(&ta_sync_scores, 50.0),
        percentile(&ta_sync_scores, 90.0),
    );

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

    println!("\n[CONTROLS — production-decode positions, sanity gate]");
    row("max-log (spectrogram)", &ctl_maxlog);
    row("matched filter +refine", &ctl_mf_ref);

    println!("\n[TRUTH-ADJACENT FAILED CANDIDATES — combined]");
    row("max-log (spectrogram)", &ta_maxlog);
    row("matched filter", &ta_mf);
    row("matched filter +refine", &ta_mf_ref);
    for corpus in ["hard200", "killsw"] {
        println!("\n[TRUTH-ADJACENT FAILED CANDIDATES — {corpus}]");
        if let Some(s) = ta_maxlog_by.get(corpus) {
            row("max-log (spectrogram)", s);
        }
        if let Some(s) = ta_mf_by.get(corpus) {
            row("matched filter", s);
        }
        if let Some(s) = ta_mf_ref_by.get(corpus) {
            row("matched filter +refine", s);
        }
    }

    println!(
        "\nRefinement best-shift distribution (samples): mean={:.0} p50={:.0}",
        mean(&ta_best_shifts),
        percentile(&ta_best_shifts, 50.0),
    );

    // ---- Pre-registered read-out ----
    let maxlog_p50 = percentile(&ta_maxlog.sign_agreements, 50.0) * 100.0;
    let mf_p50 = percentile(&ta_mf.sign_agreements, 50.0) * 100.0;
    let mf_ref_p50 = percentile(&ta_mf_ref.sign_agreements, 50.0) * 100.0;
    println!(
        "\n--- Pre-registered read-out (hb-250) ---\n\
         truth-adjacent failed candidates evaluated = {} (population floor {MIN_POPULATION})\n\
         max-log p50                = {maxlog_p50:.1}%\n\
         matched filter p50         = {mf_p50:.1}%   (delta vs max-log: {:+.1} pts)\n\
         matched filter +refine p50 = {mf_ref_p50:.1}%   (delta vs max-log: {:+.1} pts)\n\
         Bars: ALIVE-OSD2 p50 >= 85%; ALIVE-BP p50 >= 75% AND >= +8 pts over max-log; \
         else SHELVE. n < {MIN_POPULATION} => SHELVE-POPULATION.",
        ta_mf.n,
        mf_p50 - maxlog_p50,
        mf_ref_p50 - maxlog_p50,
    );
    let verdict_for = |p50: f64, delta: f64| -> &'static str {
        if ta_mf.n < MIN_POPULATION {
            "SHELVE-POPULATION"
        } else if p50 >= 85.0 {
            "ALIVE-OSD2"
        } else if p50 >= 75.0 && delta >= 8.0 {
            "ALIVE-BP"
        } else {
            "SHELVE"
        }
    };
    println!(
        "\nVerdict (matched filter):         {}",
        verdict_for(mf_p50, mf_p50 - maxlog_p50)
    );
    println!(
        "Verdict (matched filter +refine): {}",
        verdict_for(mf_ref_p50, mf_ref_p50 - maxlog_p50)
    );

    Ok(())
}
