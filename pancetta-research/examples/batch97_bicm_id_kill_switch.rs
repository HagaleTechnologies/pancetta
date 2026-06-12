//! Batch 97 — hb-252 BICM-ID kill-switch.
//!
//! Pancetta's per-symbol max-log tone-LLR extraction is the
//! zero-feedback degenerate case of the SOMAP iterative demodulator
//! (Valenti & Cheng, IEEE JSAC 23(9) 2005, eq. 8). Batch 97 wires the
//! non-degenerate case behind `Ft8Config::bicm_id_iterations` (default
//! 0 = byte-identical): after a candidate's standard BP attempt fails
//! CRC, LDPC extrinsics feed back as per-bit a-priori values into the
//! symbol-level LLR computation and BP re-runs, up to N global
//! iterations.
//!
//! Part A (synthetic SNR sweep): encode+modulate 20 distinct standard
//! messages, embed in AWGN at SNRs −24..−16 dB (2500 Hz reference BW)
//! in 0.5 dB steps, N trials/point (paired noise across configs),
//! decode rate for bicm_id_iterations ∈ {0, 2, 4}. Reports the
//! 50%-decode-rate threshold per config and the threshold shift in dB.
//! Plant reused from `batch30_snr_recall_curve.rs`.
//!
//! Part B (real-corpus spot check): hard_200 first 50 WAVs, iterations
//! {0, 2}, hash-normalized ft8_lib-truth scoring
//! (`pancetta_research::metrics::hash_normalize_message`); TP/FP/wall
//! deltas.
//!
//! Pre-registered verdict bars (see
//! `research/notes/2026-06-12-batch97-bicm-id.md`):
//!   PROCEED: synthetic shift ≥ 0.2 dB AND real ΔTP > 0 with ΔFP ≤ 2·ΔTP
//!   MECHANISM-CONFIRMED-CORPUS-PENDING: shift ≥ 0.2 dB, real flat
//!   SHELVE: shift < 0.2 dB (feedback non-no-op asserted by unit tests)
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch97_bicm_id_kill_switch
//! Env:
//!   BATCH97_TRIALS=50      trials per SNR point (Part A)
//!   BATCH97_REAL_WAVS=50   WAV count (Part B)
//!   BATCH97_SKIP_SYNTH=1 / BATCH97_SKIP_REAL=1

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};
use pancetta_research::metrics::hash_normalize_message;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f32 = 12_000.0;

/// 20 distinct standard messages (CQ / grid / report / RR73 shapes).
const MESSAGES: [&str; 20] = [
    "CQ K1ABC FN42",
    "CQ W9XYZ EN50",
    "CQ N5DDD DM79",
    "CQ G4AAA IO91",
    "CQ JA1BBB PM95",
    "K1ABC W9XYZ EN50",
    "W9XYZ K1ABC -07",
    "K1ABC W9XYZ R-09",
    "W9XYZ K1ABC RR73",
    "K1ABC W9XYZ 73",
    "N5DDD G4AAA IO91",
    "G4AAA N5DDD -15",
    "N5DDD G4AAA R-18",
    "G4AAA N5DDD RR73",
    "CQ VK3CCC QF22",
    "VK3CCC JA1BBB PM95",
    "JA1BBB VK3CCC +03",
    "VK3CCC JA1BBB R+01",
    "JA1BBB VK3CCC RRR",
    "CQ PY2DDD GG66",
];

fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
        let u2: f32 = rng.gen_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

fn signal_power(samples: &[f32]) -> f32 {
    samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// σ for AWGN such that SNR in 2500 Hz reference bandwidth = snr_db.
fn sigma_for_snr_db(p_signal: f32, snr_db: f32) -> f32 {
    let bw: f32 = 2500.0;
    let p_n_in_bw = p_signal / 10.0f32.powf(snr_db / 10.0);
    let nyquist = SAMPLE_RATE / 2.0;
    (p_n_in_bw * nyquist / bw).sqrt()
}

fn cfg_with_iters(iters: usize) -> Ft8Config {
    Ft8Config {
        bicm_id_iterations: iters,
        ..Ft8Config::default()
    }
}

/// Linear-interpolated SNR at which the rate curve first crosses 50%
/// scanning from the lowest SNR upward. None if never ≥ 50%.
fn threshold_50(snrs: &[f32], rates: &[f64]) -> Option<f64> {
    if rates[0] >= 0.5 {
        return Some(snrs[0] as f64);
    }
    for k in 1..rates.len() {
        if rates[k] >= 0.5 && rates[k - 1] < 0.5 {
            let span = (snrs[k] - snrs[k - 1]) as f64;
            let frac = (0.5 - rates[k - 1]) / (rates[k] - rates[k - 1]);
            return Some(snrs[k - 1] as f64 + frac * span);
        }
    }
    None
}

fn part_a_synthetic(trials: usize) -> Result<()> {
    println!("## Part A — synthetic SNR sweep (paired AWGN, {trials} trials/point)");

    // Pre-modulate the 20 clean reference signals once.
    let mut encoder = Ft8Encoder::new();
    let mut modulator =
        Ft8Modulator::new_default().map_err(|e| anyhow::anyhow!("modulator: {e}"))?;
    let mut cleans: Vec<Vec<f32>> = Vec::with_capacity(MESSAGES.len());
    for msg in MESSAGES {
        let symbols = encoder
            .encode_message(msg, None)
            .map_err(|e| anyhow::anyhow!("encode '{msg}': {e}"))?;
        let mut clean = modulator
            .modulate_symbols(&symbols, 0.0)
            .map_err(|e| anyhow::anyhow!("modulate '{msg}': {e}"))?;
        clean.resize(WINDOW_SAMPLES, 0.0);
        cleans.push(clean);
    }

    let snrs: Vec<f32> = (0..17).map(|k| -24.0 + 0.5 * k as f32).collect();
    let iter_configs: [usize; 3] = [0, 2, 4];
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); iter_configs.len()];
    let mut walls: Vec<f64> = vec![0.0; iter_configs.len()];

    println!("\n  SNR (dB) | iters=0        | iters=2        | iters=4");
    println!("  -------- | -------------- | -------------- | --------------");
    for (si, &snr_db) in snrs.iter().enumerate() {
        let mut successes = [0usize; 3];
        for trial in 0..trials {
            let msg_idx = trial % MESSAGES.len();
            let clean = &cleans[msg_idx];
            let p_signal = signal_power(clean);
            let sigma = sigma_for_snr_db(p_signal, snr_db);
            // Paired noise: same realization for all three configs.
            let seed = 0xB97_0000u64 + (si as u64) * 1000 + trial as u64;
            let mut rng = StdRng::seed_from_u64(seed);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            for (ci, &iters) in iter_configs.iter().enumerate() {
                let t0 = std::time::Instant::now();
                let mut decoder = Ft8Decoder::new(cfg_with_iters(iters))
                    .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
                let decoded = decoder
                    .decode_window(&recv)
                    .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
                walls[ci] += t0.elapsed().as_secs_f64();
                if decoded.iter().any(|d| d.text == MESSAGES[msg_idx]) {
                    successes[ci] += 1;
                }
            }
        }
        let mut row = format!("  {snr_db:>+6.1}  ");
        for (ci, &s) in successes.iter().enumerate() {
            let rate = s as f64 / trials as f64;
            rates[ci].push(rate);
            row.push_str(&format!("| {s:>3}/{trials:<3} {:>5.1}% ", rate * 100.0));
        }
        println!("{row}");
    }

    println!("\n### 50%-decode-rate thresholds");
    let mut thrs: Vec<Option<f64>> = Vec::new();
    for (ci, &iters) in iter_configs.iter().enumerate() {
        let thr = threshold_50(&snrs, &rates[ci]);
        match thr {
            Some(t) => println!(
                "  iters={iters}: {t:+.2} dB   (decode wall {:.1}s)",
                walls[ci]
            ),
            None => println!(
                "  iters={iters}: curve never reaches 50% in [-24,-16] (wall {:.1}s)",
                walls[ci]
            ),
        }
        thrs.push(thr);
    }
    if let (Some(t0), Some(t2), Some(t4)) = (thrs[0], thrs[1], thrs[2]) {
        println!("\n  threshold shift iters 0→2: {:+.3} dB", t0 - t2);
        println!("  threshold shift iters 0→4: {:+.3} dB", t0 - t4);
        println!("  (positive = BICM-ID decodes at lower SNR; bar = 0.2 dB)");
    }
    Ok(())
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

fn load_ft8lib_truth(ws: &Path, sha: &str) -> Option<HashSet<String>> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    Some(
        v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["message"].as_str())
            .map(hash_normalize_message)
            .collect(),
    )
}

fn part_b_real(n_wavs: usize) -> Result<()> {
    println!("\n## Part B — real-corpus spot check (hard_200 first {n_wavs} WAVs, iters {{0, 2}})");
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<&Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .take(n_wavs)
        .collect();

    let mut missing_truth = 0usize;
    for (ci, iters) in [0usize, 2].into_iter().enumerate() {
        let cfg = cfg_with_iters(iters);
        let mut tps = 0usize;
        let mut fps = 0usize;
        let t0 = std::time::Instant::now();
        for entry in &entries {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let Some(truth) = load_ft8lib_truth(&ws, sha) else {
                if ci == 0 {
                    missing_truth += 1;
                }
                continue;
            };
            let samples = load_wav(Path::new(wav_path))?;
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&samples)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            for d in &decoded {
                if truth.contains(&hash_normalize_message(&d.text)) {
                    tps += 1;
                } else {
                    fps += 1;
                }
            }
        }
        let wall = t0.elapsed().as_secs_f64();
        println!("  iters={iters}: TP={tps} FP={fps} wall={wall:.1}s");
    }
    if missing_truth > 0 {
        println!("  ({missing_truth} WAVs skipped — no ft8_lib truth file)");
    }
    Ok(())
}

fn main() -> Result<()> {
    let trials: usize = std::env::var("BATCH97_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let n_wavs: usize = std::env::var("BATCH97_REAL_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("# Batch 97 — hb-252 BICM-ID kill-switch\n");
    if std::env::var("BATCH97_SKIP_SYNTH").is_err() {
        part_a_synthetic(trials)?;
    }
    if std::env::var("BATCH97_SKIP_REAL").is_err() {
        part_b_real(n_wavs)?;
    }
    Ok(())
}
