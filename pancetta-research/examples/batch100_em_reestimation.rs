//! Batch 100 — hb-259 per-iteration EM (Es, N0) re-estimation inside
//! the BICM-ID rescue loop.
//!
//! Batch 99's verdict named block-constant Es/N0 estimation as the
//! Bessel rescue's failure mode on interference-dominated slots and
//! hb-259 as the sole re-open path for hb-252/253's rescue economics.
//! Mechanism (Cheng, Valenti & Torrieri, "Turbo-NFSK", MILCOM 2005;
//! Cheng dissertation ch. 6, read directly): each global rescue
//! iteration re-estimates Es and N0 via EM — E-step forms per-symbol
//! posterior tone probabilities from the current Bessel likelihoods
//! and the decoder's extrinsic priors (eqs. (6.11)/(6.13), Costas
//! symbols as pilots); M-step moment-matches believed-signal /
//! believed-noise tone powers (power-domain simplification of the
//! implicit eq. (6.16)). Batch 99's static median/max estimator seeds
//! iteration 0. Flag: `Ft8Config::bicm_id_em_reestimation` (default
//! false, byte-identity test-pinned). NOTE: EM lives strictly INSIDE
//! the rescue loop, so it is definitionally inert at
//! `bicm_id_iterations = 0` — a "bessel/it0 EM-on" config is
//! byte-identical to bessel/it0 and is not measured separately.
//!
//! Part A (synthetic SNR sweep, plant/seeds reused from batch99):
//! {dualmax/it0, bessel/it0, bessel/it2 EM-off, bessel/it2 EM-on},
//! SNR −24..−16 dB in 0.5 dB steps, N trials/point (paired noise).
//! Bar: EM-on ≥ EM-off (no regression) — EM pays under model
//! mismatch, so AWGN parity is acceptable (the synthetic plant is
//! exactly the channel where the static block-constant estimate is
//! already correct).
//!
//! Part B (the decisive test — interference-dominated slots):
//! hard_200 first 50 WAVs, ft8_lib truth, hash-normalized. Rows:
//! dualmax/it0 baseline, bessel/it2 EM-off (must reproduce batch99
//! +1/+27), bessel/it2 EM-on. hb-252/253 re-open bar (pre-registered):
//! EM-on ΔTP > 0 AND ΔFP ≤ 2×ΔTP vs dualmax/it0.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch100_em_reestimation
//! Env:
//!   BATCH100_TRIALS=50      trials per SNR point (Part A)
//!   BATCH100_REAL_WAVS=50   WAV count (Part B)
//!   BATCH100_SKIP_SYNTH=1 / BATCH100_SKIP_REAL=1

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

/// The probe configs: (metric, bicm iters, EM, name).
const CONFIGS: [(LlrMetric, usize, bool, &str); 4] = [
    (LlrMetric::DualMax, 0, false, "dualmax/it0"),
    (LlrMetric::Bessel, 0, false, "bessel/it0"),
    (LlrMetric::Bessel, 2, false, "bessel/it2/em-off"),
    (LlrMetric::Bessel, 2, true, "bessel/it2/em-on"),
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

fn cfg_for(metric: LlrMetric, iters: usize, em: bool) -> Ft8Config {
    Ft8Config {
        llr_metric: metric,
        bicm_id_iterations: iters,
        bicm_id_em_reestimation: em,
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
        .map(|(_, _, _, name)| format!("| {name:<18} "))
        .collect();
    println!("\n  SNR (dB) {header}");
    println!(
        "  -------- {}",
        "| ------------------ ".repeat(CONFIGS.len())
    );
    for (si, &snr_db) in snrs.iter().enumerate() {
        let mut successes = [0usize; 4];
        for trial in 0..trials {
            let msg_idx = trial % MESSAGES.len();
            let clean = &cleans[msg_idx];
            let p_signal = signal_power(clean);
            let sigma = sigma_for_snr_db(p_signal, snr_db);
            // Paired noise: same realization for all configs. Seed
            // base matches batch97/99 so the dualmax/it0, bessel/it0,
            // and bessel/it2 EM-off columns are directly comparable
            // to the batch99 table.
            let seed = 0xB97_0000u64 + (si as u64) * 1000 + trial as u64;
            let mut rng = StdRng::seed_from_u64(seed);
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let recv: Vec<f32> = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
            for (ci, &(metric, iters, em, _)) in CONFIGS.iter().enumerate() {
                let t0 = std::time::Instant::now();
                let mut decoder = Ft8Decoder::new(cfg_for(metric, iters, em))
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
            row.push_str(&format!("| {s:>3}/{trials:<3} {:>7.1}%   ", rate * 100.0));
        }
        println!("{row}");
    }

    println!("\n### 50%-decode-rate thresholds");
    let mut thrs: Vec<Option<f64>> = Vec::new();
    for (ci, &(_, _, _, name)) in CONFIGS.iter().enumerate() {
        let thr = threshold_50(&snrs, &rates[ci]);
        match thr {
            Some(t) => println!("  {name:<18}: {t:+.2} dB   (decode wall {:.1}s)", walls[ci]),
            None => println!(
                "  {name:<18}: curve never reaches 50% in [-24,-16] (wall {:.1}s)",
                walls[ci]
            ),
        }
        thrs.push(thr);
    }
    if let (Some(d0), Some(b0), Some(boff), Some(bon)) = (thrs[0], thrs[1], thrs[2], thrs[3]) {
        println!(
            "\n  metric shift   (dualmax/it0 → bessel/it0):        {:+.3} dB",
            d0 - b0
        );
        println!(
            "  composed       (dualmax/it0 → bessel/it2/em-off): {:+.3} dB",
            d0 - boff
        );
        println!(
            "  composed + EM  (dualmax/it0 → bessel/it2/em-on):  {:+.3} dB",
            d0 - bon
        );
        println!(
            "  EM delta       (bessel/it2 em-off → em-on):       {:+.3} dB",
            boff - bon
        );
        println!(
            "\n  Part A bar (EM-on >= EM-off, i.e. EM delta >= ~0): {}",
            if boff - bon >= -0.05 {
                "PASSED (no regression)"
            } else {
                "FAILED (EM regresses on AWGN)"
            }
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
    println!("\n## Part B — real-corpus spot check (hard_200 first {n_wavs} WAVs)");
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
    for (ci, &(metric, iters, em, name)) in CONFIGS.iter().enumerate() {
        // bessel/it0 adds nothing on the spot (batch99 measured it);
        // skip to keep the decisive rows cheap. EM at it0 is inert by
        // construction (EM lives inside the rescue loop).
        if iters == 0 && metric == LlrMetric::Bessel {
            results.push((0, 0));
            continue;
        }
        let cfg = cfg_for(metric, iters, em);
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
        println!("  {name:<18}: TP={tps} FP={fps} wall={wall:.1}s");
        results.push((tps, fps));
    }
    if missing_truth > 0 {
        println!("  ({missing_truth} WAVs skipped — no ft8_lib truth file)");
    }
    if results.len() == 4 {
        let (tp_base, fp_base) = results[0];
        println!("\n### Deltas vs dualmax/it0 baseline (TP={tp_base} FP={fp_base})");
        for (ci, &(_, iters, _, name)) in CONFIGS.iter().enumerate().skip(1) {
            if iters == 0 {
                continue; // skipped row
            }
            let (tp, fp) = results[ci];
            let dtp = tp as i64 - tp_base as i64;
            let dfp = fp as i64 - fp_base as i64;
            println!("  {name:<18}: ΔTP={dtp:+} ΔFP={dfp:+}");
        }
        let (tp_off, fp_off) = results[2];
        println!(
            "\n  batch99 reproduction check (bessel/it2/em-off expected ΔTP +1 / ΔFP +27): ΔTP={:+} ΔFP={:+}",
            tp_off as i64 - tp_base as i64,
            fp_off as i64 - fp_base as i64
        );
        let (tp_on, fp_on) = results[3];
        let dtp = tp_on as i64 - tp_base as i64;
        let dfp = fp_on as i64 - fp_base as i64;
        let reopen = dtp > 0 && dfp <= 2 * dtp;
        println!(
            "\n  hb-252/253 re-open bar (bessel/it2/em-on: ΔTP > 0 AND ΔFP ≤ 2×ΔTP): {}",
            if reopen { "MET" } else { "NOT MET" }
        );
        println!("  (references: dual-max it2 gated +2/+17 [B98]; bessel it2 static +1/+27 [B99])");
    }
    Ok(())
}

fn main() -> Result<()> {
    let trials: usize = std::env::var("BATCH100_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let n_wavs: usize = std::env::var("BATCH100_REAL_WAVS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("# Batch 100 — hb-259 EM (Es, N0) re-estimation in the BICM-ID rescue\n");
    if std::env::var("BATCH100_SKIP_SYNTH").is_err() {
        part_a_synthetic(trials)?;
    }
    if std::env::var("BATCH100_SKIP_REAL").is_err() {
        part_b_real(n_wavs)?;
    }
    Ok(())
}
