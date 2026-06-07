//! Batch 39 / hb-218c — BB2 (neighbor-mask) + CC1 (synth control) + DD1 (V3 audit).
//!
//! BB2: For top-20 louder-missed, notch the neighbor's freq band in the
//!      WAV (FFT-domain zero-out ±50 Hz) and re-decode. Does the truth
//!      surface when the neighbor's energy is removed?
//!
//! CC1: Synth-pair control — generate two synthesized FT8 signals at
//!      known Δfreq + Δsnr, where signal A is 6dB louder than B. Does
//!      pancetta prefer A's decode? Validates that pancetta correctly
//!      prioritizes louder in clean conditions.
//!
//! DD1: For all V3 decodes from BB1 (4329 across 122 WAVs), count how
//!      many match hard-200 truth (TPs) vs novel (FPs + jt9-missed).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch39_hb218c_bb

use anyhow::{Context, Result};
use pancetta_ft8::encoder::Ft8Encoder;
use pancetta_ft8::modulator::Ft8Modulator;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: u32 = 12_000;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct FrontierEntry {
    sha: String,
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
    neighbor_text: String,
    neighbor_freq_hz: f64,
    neighbor_dt_s: f64,
    neighbor_snr_db: f64,
    delta_freq_hz: f64,
    delta_snr_db: f64,
}

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn load_truth_texts(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

/// FFT-domain notch: zero out frequency bins within ±half_width_hz of
/// center_freq. Uses 1024-sample windows with 50% overlap and a simple
/// IFFT inverse via brute-force sum. Approximate but adequate for
/// quick-and-dirty mask experiments.
fn notch_filter(samples: &[f32], center_freq: f64, half_width_hz: f64) -> Vec<f32> {
    // Simple time-domain bandstop using a sinc-windowed FIR filter
    let n_taps = 257;
    let fs = SAMPLE_RATE as f64;
    let f_low = (center_freq - half_width_hz) / fs;
    let f_high = (center_freq + half_width_hz) / fs;

    // Bandstop = all-pass - bandpass; build bandpass first.
    let mut taps = vec![0.0f64; n_taps];
    let center = (n_taps / 2) as isize;
    for k in 0..n_taps {
        let n = k as isize - center;
        let bp = if n == 0 {
            2.0 * (f_high - f_low)
        } else {
            let nf = n as f64;
            ((2.0 * std::f64::consts::PI * f_high * nf).sin()
                - (2.0 * std::f64::consts::PI * f_low * nf).sin())
                / (std::f64::consts::PI * nf)
        };
        // Bandstop = δ[n] - bp; apply Hamming window
        let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * k as f64 / (n_taps - 1) as f64).cos();
        let bs = if n == 0 { 1.0 - bp } else { -bp };
        taps[k] = bs * w;
    }

    // Convolve
    let mut out = vec![0.0f32; samples.len()];
    for i in 0..samples.len() {
        let mut acc = 0.0f64;
        for k in 0..n_taps {
            let idx = i as isize + k as isize - center;
            if idx >= 0 && (idx as usize) < samples.len() {
                acc += taps[k] * samples[idx as usize] as f64;
            }
        }
        out[i] = acc as f32;
    }
    out
}

fn decode_default(samples: &[f32]) -> Result<HashSet<String>> {
    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let frontier_path = ws.join("research/scorecards/batch37_frontier.json");
    let frontier_json = std::fs::read_to_string(&frontier_path)?;
    let frontier: Vec<FrontierEntry> = serde_json::from_str(&frontier_json)?;

    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let mut sha_to_wav: HashMap<String, PathBuf> = HashMap::new();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        sha_to_wav.insert(sha.to_string(), PathBuf::from(wav_path));
    }

    let louder_missed: Vec<&FrontierEntry> =
        frontier.iter().filter(|f| f.delta_snr_db < -3.0).collect();

    println!("## Batch 39 BB+CC+DD — louder-missed deep probes");

    // === BB2 — Neighbor-mask probe ===
    println!("\n### BB2 — Neighbor-mask: notch ±60 Hz around neighbor.freq, re-decode (top-20)");
    let mut bb2_recovered = 0usize;
    let mut bb2_probed = 0usize;
    for v in louder_missed.iter().take(20) {
        let Some(wav_path) = sha_to_wav.get(&v.sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let notched = notch_filter(&samples, v.neighbor_freq_hz, 60.0);
        let decoded = decode_default(&notched)?;
        if decoded.contains(&v.text) {
            bb2_recovered += 1;
        }
        bb2_probed += 1;
    }
    println!(
        "  truth recovered post-mask: {}/{} ({:.1}%)",
        bb2_recovered,
        bb2_probed,
        bb2_recovered as f64 / bb2_probed.max(1) as f64 * 100.0
    );

    // === CC1 — Synth-pair control ===
    println!("\n### CC1 — Synth 2-signal control");
    println!("Generates two clean FT8 signals at 1000 Hz and 1015 Hz (Δfreq=15 Hz),");
    println!("with signal A at amplitude 1.0 and signal B at 0.5 (≈6 dB louder = A).");
    println!("Adds AWGN noise. Decodes. Which surfaces?\n");

    let mut encoder = Ft8Encoder::new();
    let sym_a = encoder.encode_message("K1ABC W2DEF FN42", None)?;
    let sym_b = encoder.encode_message("W2DEF K1ABC -10", None)?;
    let mut mod_a = Ft8Modulator::new(SAMPLE_RATE, 1000.0, 1.0)?;
    let wave_a = mod_a.modulate_symbols(&sym_a, 0.0)?;
    let mut mod_b = Ft8Modulator::new(SAMPLE_RATE, 1015.0, 0.5)?; // 6 dB quieter
    let wave_b = mod_b.modulate_symbols(&sym_b, 0.0)?;

    // 15s WAV = 180k samples; put both signals at dt_s = 0.5
    let wav_len = 180_000;
    let dt_samples = (0.5 * SAMPLE_RATE as f64) as usize;
    let mut wav = vec![0.0f32; wav_len];
    for (i, &s) in wave_a.iter().enumerate() {
        if dt_samples + i < wav_len {
            wav[dt_samples + i] += s;
        }
    }
    for (i, &s) in wave_b.iter().enumerate() {
        if dt_samples + i < wav_len {
            wav[dt_samples + i] += s;
        }
    }
    // Add light AWGN (rough -20 dBFS)
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for s in wav.iter_mut() {
        let noise: f32 = rng.gen_range(-0.05..0.05);
        *s += noise;
    }
    let decoded = decode_default(&wav)?;
    let a_decoded = decoded.contains("K1ABC W2DEF FN42");
    let b_decoded = decoded.contains("W2DEF K1ABC -10");
    println!("  A (louder, 1000 Hz) decoded: {}", a_decoded);
    println!("  B (quieter, 1015 Hz, -6dB) decoded: {}", b_decoded);
    println!("  total synth decodes: {}", decoded.len());
    if a_decoded && !b_decoded {
        println!("  → pancetta prioritizes LOUDER correctly in clean synth case.");
    } else if !a_decoded && b_decoded {
        println!("  → SURPRISING: pancetta picks QUIETER. Possible priority bug in synth too.");
    } else if a_decoded && b_decoded {
        println!("  → BOTH decode (Δfreq=15 Hz is enough separation).");
    } else {
        println!("  → NEITHER decodes (likely noise-induced).");
    }

    // === DD1 — V3 spurious-decode audit ===
    println!("\n### DD1 — V3 (-3.0 dB / window=12) total decodes vs mp=2 baseline TPs/FPs");
    let needed_shas: HashSet<String> = louder_missed.iter().map(|f| f.sha.clone()).collect();
    let mut mp2_tps = 0usize;
    let mut mp2_total = 0usize;
    let mut v3_tps = 0usize;
    let mut v3_total = 0usize;

    let mut cfg_mp2 = Ft8Config::default();
    cfg_mp2.max_decode_passes = 2;
    let mut cfg_v3 = cfg_mp2.clone();
    cfg_v3.joint_residual_sync_relax_db = -3.0;
    cfg_v3.joint_residual_sync_window_bins = 12;

    for sha in &needed_shas {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let truth = load_truth_texts(&ws, sha);

        let mut decoder = Ft8Decoder::new(cfg_mp2.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let mp2 = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        mp2_total += mp2.len();
        for d in &mp2 {
            if truth.contains(&d.text) {
                mp2_tps += 1;
            }
        }

        let mut decoder =
            Ft8Decoder::new(cfg_v3.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let v3 = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        v3_total += v3.len();
        for d in &v3 {
            if truth.contains(&d.text) {
                v3_tps += 1;
            }
        }
    }

    println!(
        "  mp=2 baseline: {} decodes ({} TPs, {:.1}% prec)",
        mp2_total,
        mp2_tps,
        mp2_tps as f64 / mp2_total.max(1) as f64 * 100.0
    );
    println!(
        "  V3 (relax=-3.0): {} decodes ({} TPs, {:.1}% prec)",
        v3_total,
        v3_tps,
        v3_tps as f64 / v3_total.max(1) as f64 * 100.0
    );
    println!(
        "  Δ: {:+} decodes, {:+} TPs",
        v3_total as i64 - mp2_total as i64,
        v3_tps as i64 - mp2_tps as i64
    );

    Ok(())
}
