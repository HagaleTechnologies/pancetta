//! Batch 32 / Diagnostic W — logistic regression for hb-103 weights.
//!
//! Trains a simple batch logistic regression (gradient descent) on the
//! labeled (TP/FP) dataset to find optimal weights, then compares the
//! learned-weight AUC to Batch 31's hand-weighted formula (AUC 0.886).
//!
//! Features (expanded from Batch 31 with Diagnostic V additions):
//!   x[0] = 1.0                        (bias)
//!   x[1] = in_trust_set_both          (boolean)
//!   x[2] = in_trust_set_any           (boolean)
//!   x[3] = confidence                 ([0, 1])
//!   x[4] = -time_offset_s / 15        (normalized; higher = earlier)
//!   x[5] = snr_db / 30                (normalized)
//!   x[6] = -decode_time_s / 1.0       (NEW; negative because FPs are slower)
//!
//! Optimization: batch gradient descent on log-loss, 1000 iters,
//! step 0.05. Simple but sufficient for ~7000 samples × 7 features.
//!
//! Reports learned weights + AUC on the same population.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch32_lr_training

use anyhow::{Context, Result};
use pancetta_ft8::{DecodedMessage, Ft8Config, Ft8Decoder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;
const SLOT_S: usize = 15;
const WINDOW_SAMPLES: usize = SAMPLE_RATE * SLOT_S;
const N_FEATURES: usize = 7;

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

fn load_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn load_all_truth_callsigns(ws: &Path) -> Result<HashSet<String>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut out = HashSet::new();
    for e in manifest["entries"].as_array().context("entries")? {
        let sha = e["wav_sha256"].as_str().unwrap_or("");
        let bpath = ws
            .join("research/baselines/ft8")
            .join(format!("{}.json", sha));
        let Ok(txt) = std::fs::read_to_string(&bpath) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            continue;
        };
        let Some(arr) = v["decodes"].as_array() else {
            continue;
        };
        for d in arr {
            if let Some(msg) = d["message"].as_str() {
                for c in pancetta_qso::callsign_continuity::callsigns_in(msg) {
                    out.insert(c);
                }
            }
        }
    }
    Ok(out)
}

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

fn build_features(d: &DecodedMessage, trust: &HashSet<String>) -> [f64; N_FEATURES] {
    let calls = pancetta_qso::callsign_continuity::callsigns_in(&d.text);
    let in_trust_any = if calls.iter().any(|c| trust.contains(c)) {
        1.0
    } else {
        0.0
    };
    let in_trust_both = if calls.len() >= 2 && calls.iter().all(|c| trust.contains(c)) {
        1.0
    } else {
        0.0
    };
    let decode_time = d
        .decode_time_into_window
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.5);
    [
        1.0,
        in_trust_both,
        in_trust_any,
        d.confidence as f64,
        -(d.time_offset / 15.0),
        d.snr_db as f64 / 30.0,
        -decode_time,
    ]
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

fn dot(w: &[f64; N_FEATURES], x: &[f64; N_FEATURES]) -> f64 {
    let mut s = 0.0;
    for i in 0..N_FEATURES {
        s += w[i] * x[i];
    }
    s
}

fn auc_continuous(samples: &[(bool, [f64; N_FEATURES])], w: &[f64; N_FEATURES]) -> f64 {
    let tps: Vec<f64> = samples
        .iter()
        .filter(|s| s.0)
        .map(|s| dot(w, &s.1))
        .collect();
    let fps: Vec<f64> = samples
        .iter()
        .filter(|s| !s.0)
        .map(|s| dot(w, &s.1))
        .collect();
    if tps.is_empty() || fps.is_empty() {
        return 0.5;
    }
    let mut higher = 0u64;
    let mut tied = 0u64;
    for &t in &tps {
        for &f in &fps {
            if t > f {
                higher += 1;
            } else if t == f {
                tied += 1;
            }
        }
    }
    let total = (tps.len() * fps.len()) as u64;
    (higher as f64 + 0.5 * tied as f64) / total as f64
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH32_W_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH32_W_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 32 / Diagnostic W — logistic regression for hb-103 weights");
    let trust = load_all_truth_callsigns(&ws)?;
    println!("  trust set: {}", trust.len());

    let cfg = Ft8Config::default();
    let mut data: Vec<(bool, [f64; N_FEATURES])> = Vec::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let wav = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&wav)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            data.push((truth.contains(&d.text), build_features(d, &trust)));
        }
    }
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(515151 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            data.push((false, build_features(d, &trust)));
        }
    }

    println!(
        "  dataset: {} samples ({} TP, {} FP)",
        data.len(),
        data.iter().filter(|s| s.0).count(),
        data.iter().filter(|s| !s.0).count(),
    );

    // Baseline: Batch 31 hand-weighted formula
    let hand: [f64; N_FEATURES] = [
        0.0,         // bias (not in original formula)
        2.0,         // in_trust_both
        1.0,         // in_trust_any
        1.0,         // confidence
        15.0 * 0.1,  // -(dt/15) * (15*0.1) so the effective weight matches -0.1 * dt
        30.0 * 0.05, // snr_db/30 * (30*0.05) → 0.05 * snr_db
        0.0,         // decode_time (not in original formula)
    ];
    let hand_auc = auc_continuous(&data, &hand);
    println!("\n  Hand-weighted (Batch 31) AUC: {:.4}", hand_auc);

    // LR training
    let lr: f64 = 0.05;
    let n_iters = 2000;
    let mut w: [f64; N_FEATURES] = [0.0; N_FEATURES];
    let n = data.len() as f64;

    for iter in 0..n_iters {
        let mut grad: [f64; N_FEATURES] = [0.0; N_FEATURES];
        let mut loss = 0.0;
        for (y, x) in &data {
            let z = dot(&w, x);
            let p = sigmoid(z);
            let y_f = if *y { 1.0 } else { 0.0 };
            for i in 0..N_FEATURES {
                grad[i] += (p - y_f) * x[i];
            }
            let eps = 1e-9;
            loss -= y_f * (p + eps).ln() + (1.0 - y_f) * (1.0 - p + eps).ln();
        }
        for i in 0..N_FEATURES {
            grad[i] /= n;
            w[i] -= lr * grad[i];
        }
        if iter % 500 == 0 {
            println!(
                "    iter {:>4}: loss={:.4}  auc={:.4}",
                iter,
                loss / n,
                auc_continuous(&data, &w)
            );
        }
    }
    println!(
        "    iter {:>4}: final auc={:.4}",
        n_iters,
        auc_continuous(&data, &w)
    );

    println!("\n  Learned weights:");
    let names = [
        "bias",
        "in_trust_both",
        "in_trust_any",
        "confidence",
        "-dt/15",
        "snr_db/30",
        "-decode_time_s",
    ];
    for i in 0..N_FEATURES {
        println!("    w[{}] {:<17} = {:>+8.4}", i, names[i], w[i]);
    }

    // For interpretability: convert learned weights into pancetta-style
    // human-readable formula coefficients.
    println!("\n  Equivalent formula:");
    println!("    score = {:+.3}", w[0]);
    println!("           {:+.3} * in_trust_set_both", w[1]);
    println!("           {:+.3} * in_trust_set_any", w[2]);
    println!("           {:+.3} * confidence", w[3]);
    println!("           {:+.3} * time_offset_s", -w[4] / 15.0);
    println!("           {:+.3} * snr_db", w[5] / 30.0);
    println!("           {:+.3} * decode_time_s", -w[6]);

    let learned_auc = auc_continuous(&data, &w);
    let delta = learned_auc - hand_auc;
    println!(
        "\n  AUC delta (learned − hand): {:+.4}  ({:.1}% relative)",
        delta,
        delta / hand_auc * 100.0
    );

    if delta >= 0.02 {
        println!("\n## Verdict: PROCEED — learned weights give meaningful AUC lift; consider shipping as hb-103 v2");
    } else if delta >= 0.005 {
        println!("\n## Verdict: MARGINAL — hand-weighted is close to optimal; learned weights are not worth shipping");
    } else {
        println!("\n## Verdict: HAND-WEIGHTING IS OPTIMAL — no meaningful gain from training");
    }

    Ok(())
}
