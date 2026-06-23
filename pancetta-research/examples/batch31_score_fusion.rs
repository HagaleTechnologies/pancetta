//! Batch 31 / Diagnostic T — Score-fusion of top features (hb-103 v1
//! prototype).
//!
//! From Diagnostic R+S, the strong discriminators are:
//!   - in_trust_set_both (AUC 0.837)
//!   - in_trust_set_any (AUC 0.755) — subset of both
//!   - confidence (AUC 0.706)
//!   - time_offset (AUC inverted ~0.576)  → reject FPs at high dt
//!
//! This diagnostic builds a simple fused score, sweeps threshold, and
//! reports the precision-recall curve. The goal is a single threshold
//! that catches the 33% of hard-200 FPs surviving the callsign-continuity
//! filter (Batch 30 M finding).
//!
//! Score formula (hand-weighted, no training):
//!   score(decode) = 2 * in_trust_set_both
//!                 + 1 * in_trust_set_any
//!                 + 1 * confidence
//!                 - 0.1 * time_offset
//!                 + 0.05 * snr_db
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch31_score_fusion

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

fn fused_score(d: &DecodedMessage, trust: &HashSet<String>) -> f64 {
    let calls = pancetta_qso::callsign_continuity::callsigns_in(&d.text);
    let in_trust_any = calls.iter().any(|c| trust.contains(c));
    let in_trust_both = calls.len() >= 2 && calls.iter().all(|c| trust.contains(c));

    let mut s = 0.0;
    if in_trust_both {
        s += 2.0;
    }
    if in_trust_any {
        s += 1.0;
    }
    s += d.confidence as f64;
    s -= 0.1 * d.time_offset;
    s += 0.05 * d.snr_db as f64;
    s
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH31_T_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let n_noise: usize = std::env::var("BATCH31_T_NOISE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 31 / Diagnostic T — score fusion (hb-103 v1 prototype)");
    let trust = load_all_truth_callsigns(&ws)?;
    println!(
        "  hard-200 WAVs: {}  noise: {}  trust set: {}",
        top_n,
        n_noise,
        trust.len()
    );

    let cfg = Ft8Config::default();
    let mut tps: Vec<f64> = Vec::new();
    let mut fps: Vec<f64> = Vec::new();

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
            let s = fused_score(d, &trust);
            if truth.contains(&d.text) {
                tps.push(s);
            } else {
                fps.push(s);
            }
        }
    }
    for i in 0..n_noise {
        let mut rng = StdRng::seed_from_u64(656565 + i as u64);
        let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, 0.05);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&noise)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            fps.push(fused_score(d, &trust));
        }
    }

    let n_tp = tps.len();
    let n_fp = fps.len();
    println!("  TP: {}  FP: {}", n_tp, n_fp);

    // AUC via Mann-Whitney
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
    let auc = (higher as f64 + 0.5 * tied as f64) / (n_tp * n_fp) as f64;
    println!("\n  Fused-score AUC: {:.3}", auc);

    // Threshold sweep
    let mut all_scores: Vec<f64> = tps.iter().chain(fps.iter()).cloned().collect();
    all_scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p_steps = [0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95];
    println!("\n  Threshold | TP-pass | FP-pass | TP recall | FP reduction | Lift");
    println!("  --------- | ------- | ------- | --------- | ------------ | -----");
    for &q in &p_steps {
        let idx = ((all_scores.len() as f64) * q) as usize;
        let thr = all_scores[idx.min(all_scores.len() - 1)];
        let tp_pass = tps.iter().filter(|&&s| s >= thr).count();
        let fp_pass = fps.iter().filter(|&&s| s >= thr).count();
        let tp_recall = tp_pass as f64 / n_tp as f64 * 100.0;
        let fp_red = (1.0 - fp_pass as f64 / n_fp as f64) * 100.0;
        let lift = if fp_pass == 0 {
            f64::INFINITY
        } else {
            tp_pass as f64 / fp_pass as f64
        };
        let lift_str = if lift.is_infinite() {
            "∞".to_string()
        } else {
            format!("{:.2}", lift)
        };
        println!(
            "  {:>+8.3}  | {:>3}/{:<3} | {:>3}/{:<3} |   {:>5.1}% |     {:>5.1}% | {}",
            thr, tp_pass, n_tp, fp_pass, n_fp, tp_recall, fp_red, lift_str
        );
    }

    // Pick the threshold that minimizes FPs while keeping ≥98% recall
    let mut best_thr: Option<f64> = None;
    let mut best_fp_pass = n_fp;
    let target_recall = 0.98;
    for thr_idx in 0..all_scores.len() {
        let thr = all_scores[thr_idx];
        let tp_pass = tps.iter().filter(|&&s| s >= thr).count();
        let recall = tp_pass as f64 / n_tp as f64;
        if recall < target_recall {
            continue;
        }
        let fp_pass = fps.iter().filter(|&&s| s >= thr).count();
        if fp_pass < best_fp_pass {
            best_fp_pass = fp_pass;
            best_thr = Some(thr);
        }
    }
    if let Some(t) = best_thr {
        let recall_pct = tps.iter().filter(|&&s| s >= t).count() as f64 / n_tp as f64 * 100.0;
        let fp_red = (1.0 - best_fp_pass as f64 / n_fp as f64) * 100.0;
        println!(
            "\n  Best 98%-recall threshold: {:+.3}  → recall {:.1}%, FP reduction {:.1}%",
            t, recall_pct, fp_red
        );
    }

    println!("\n### Verdict");
    if auc >= 0.85 {
        println!(
            "  PROCEED — AUC {:.3} >= 0.85; fused score is a strong discriminator. Ship hb-103 v1.",
            auc
        );
    } else if auc >= 0.75 {
        println!("  PROCEED-CAUTIOUS — AUC {:.3} is meaningful but below 0.85; ship as optional/sibling filter, not default replacement.", auc);
    } else {
        println!(
            "  SHELVE — AUC {:.3} too weak to justify shipping a new filter path.",
            auc
        );
    }

    Ok(())
}
