//! Batch 99 — hb-253 exact Bessel noncoherent LLR metric kill-switch.
//!
//! Pancetta's per-symbol bit-LLR extraction (max-vs-max over dB tone
//! powers) is exactly the "parameter free dual-max" metric of Guillén
//! i Fàbregas & Grant, *Capacity Approaching Codes for Non-Coherent
//! Orthogonal Modulation* (IEEE TWC), eq. (13). Their exact
//! noncoherent metric (eqs. (1)/(6)) replaces `log |y|²` with
//! `ln I0(2·√Es·|y|/N0)` and the max with a true sum over labels;
//! measured gap ~0.6 dB when Es/N0 is known. Batch 99 wires it behind
//! `Ft8Config::llr_metric` (default `DualMax` = byte-identical) with
//! the simplest defensible per-candidate block-constant estimator
//! (N0 = median non-max tone power / ln 2; Es = mean max-tone power −
//! N0) and threads it through the hb-252 BICM-ID rescue (exact
//! log-sum-exp SOMAP, eq. (6)) — Batch 98's re-open condition is a
//! sharper rescue LLR, which this metric supplies if real.
//!
//! Part A (synthetic SNR sweep, plant reused from
//! `batch97_bicm_id_kill_switch.rs`): decode rate for
//! {DualMax, Bessel} × bicm_id_iterations {0, 2}, SNR −24..−16 dB in
//! 0.5 dB steps (2500 Hz reference BW), N trials/point (paired noise
//! across all four configs). Reports 50%-decode-rate thresholds.
//!
//! Part B (real-corpus spot): hard_200 first 50 WAVs, the four
//! configs, hash-normalized ft8_lib-truth scoring; TP/FP/wall deltas.
//! Key question (hb-252 re-open): does Bessel × iters=2 improve the
//! rescue's TP/FP economics vs Batch 98's +2/+17?
//!
//! Pre-registered verdict bars (see
//! `research/notes/2026-06-12-batch99-bessel-llr.md`):
//!   metric REAL:    Bessel iters=0 synthetic shift ≥ +0.15 dB vs DualMax
//!   hb-252 RE-OPEN: Bessel×iters=2 spot ΔFP ≤ 2×ΔTP with ΔTP > 0
//!                   (vs DualMax iters=0 baseline)
//!   SHELVE hb-253:  Bessel iters=0 synthetic shift < +0.1 dB
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch99_bessel_llr_kill_switch
//! Env:
//!   BATCH99_TRIALS=50      trials per SNR point (Part A)
//!   BATCH99_REAL_WAVS=50   WAV count (Part B)
//!   BATCH99_SKIP_SYNTH=1 / BATCH99_SKIP_REAL=1
//!   BATCH99_WHITEN_OFF=1   disable LLR whitening in ALL configs
//!                          (secondary diagnostic: whitening is a
//!                          per-symbol divisive step downstream of the
//!                          demapper that could partially mask the
//!                          metric difference)

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, LlrMetric, WINDOW_SAMPLES};
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

/// The four probe configs: metric × BICM-ID iterations.
const CONFIGS: [(LlrMetric, usize, &str); 4] = [
    (LlrMetric::DualMax, 0, "dualmax/it0"),
    (LlrMetric::Bessel, 0, "bessel/it0"),
    (LlrMetric::DualMax, 2, "dualmax/it2"),
    (LlrMetric::Bessel, 2, "bessel/it2"),
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

fn cfg_for(metric: LlrMetric, iters: usize) -> Ft8Config {
    let whiten_off = std::env::var("BATCH99_WHITEN_OFF").is_ok();
    Ft8Config {
        llr_metric: metric,
        bicm_id_iterations: iters,
        llr_whitening_enabled: !whiten_off && Ft8Config::default().llr_whitening_enabled,
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
    let mut rates: Vec<Vec<f64>> = vec![Vec::new(); CONFIGS.len()];
    let mut walls: Vec<f64> = vec![0.0; CONFIGS.len()];

    let header: String = CONFIGS
        .iter()
        .map(|(_, _, name)| format!("| {name:<14} "))
        .collect();
    println!("\n  SNR (dB) {header}");
    println!("  -------- {}", "| -------------- ".repeat(CONFIGS.len()));
    for (si, &snr_db) in snrs.iter().enumerate() {
        let mut successes = [0usize; 4];
        for trial in 0..trials {
            let msg_idx = trial % MESSAGES.len();
            let clean = &cleans[msg_idx];
            let p_signal = signal_power(clean);
            let sigma = sigma_for_snr_db(p_signal, snr_db);
            // Paired noise: same realization for all four configs.
            // Seed base matches batch97 so the dualmax/it0 and
            // dualmax/it2 columns are directly comparable.
            let seed = 0xB97_0000u64 + (si as u64) * 1000 + trial as u64;
            let mut rng = StdRng::seed_from_u64(seed);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            for (ci, &(metric, iters, _)) in CONFIGS.iter().enumerate() {
                let t0 = std::time::Instant::now();
                let mut decoder = Ft8Decoder::new(cfg_for(metric, iters))
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
    for (ci, &(_, _, name)) in CONFIGS.iter().enumerate() {
        let thr = threshold_50(&snrs, &rates[ci]);
        match thr {
            Some(t) => println!("  {name:<14}: {t:+.2} dB   (decode wall {:.1}s)", walls[ci]),
            None => println!(
                "  {name:<14}: curve never reaches 50% in [-24,-16] (wall {:.1}s)",
                walls[ci]
            ),
        }
        thrs.push(thr);
    }
    if let (Some(d0), Some(b0), Some(d2), Some(b2)) = (thrs[0], thrs[1], thrs[2], thrs[3]) {
        println!(
            "\n  metric shift  (dualmax/it0 → bessel/it0):  {:+.3} dB",
            d0 - b0
        );
        println!(
            "  bicm-id shift (dualmax/it0 → dualmax/it2): {:+.3} dB",
            d0 - d2
        );
        println!(
            "  combined      (dualmax/it0 → bessel/it2):  {:+.3} dB",
            d0 - b2
        );
        println!(
            "  (positive = decodes at lower SNR; bars: metric ≥ +0.15 dB REAL, < +0.1 dB SHELVE)"
        );
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
    println!("\n## Part B — real-corpus spot check (hard_200 first {n_wavs} WAVs, 4 configs)");
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
    let mut results: Vec<(usize, usize)> = Vec::new();
    for (ci, &(metric, iters, name)) in CONFIGS.iter().enumerate() {
        let cfg = cfg_for(metric, iters);
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
        println!("  {name:<14}: TP={tps} FP={fps} wall={wall:.1}s");
        results.push((tps, fps));
    }
    if missing_truth > 0 {
        println!("  ({missing_truth} WAVs skipped — no ft8_lib truth file)");
    }
    if results.len() == 4 {
        let (tp_base, fp_base) = results[0];
        println!("\n### Deltas vs dualmax/it0 baseline (TP={tp_base} FP={fp_base})");
        for (ci, &(_, _, name)) in CONFIGS.iter().enumerate().skip(1) {
            let (tp, fp) = results[ci];
            let dtp = tp as i64 - tp_base as i64;
            let dfp = fp as i64 - fp_base as i64;
            println!("  {name:<14}: ΔTP={dtp:+} ΔFP={dfp:+}");
        }
        let (tp_b2, fp_b2) = results[3];
        let dtp = tp_b2 as i64 - tp_base as i64;
        let dfp = fp_b2 as i64 - fp_base as i64;
        let reopen = dtp > 0 && dfp <= 2 * dtp;
        println!(
            "\n  hb-252 re-open bar (bessel/it2: ΔTP > 0 AND ΔFP ≤ 2×ΔTP): {}",
            if reopen { "MET" } else { "NOT MET" }
        );
        println!("  (Batch 98 reference at iters=2 gated T=18: ΔTP +2 / ΔFP +17)");
    }
    Ok(())
}

fn main() -> Result<()> {
    let trials: usize = std::env::var("BATCH99_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let n_wavs: usize = std::env::var("BATCH99_REAL_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("# Batch 99 — hb-253 exact Bessel noncoherent LLR metric kill-switch\n");
    if std::env::var("BATCH99_WHITEN_OFF").is_ok() {
        println!("(LLR whitening DISABLED in all configs via BATCH99_WHITEN_OFF)\n");
    }
    if std::env::var("BATCH99_SKIP_SYNTH").is_err() {
        part_a_synthetic(trials)?;
    }
    if std::env::var("BATCH99_SKIP_REAL").is_err() {
        part_b_real(n_wavs)?;
    }
    Ok(())
}
