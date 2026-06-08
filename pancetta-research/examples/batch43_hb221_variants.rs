//! Batch 43 — hb-221 multi-interval variant sweep.
//!
//! Batch 42 measured +118 TPs from 5-window union {0, 0.5, 1.0, 1.5, 2.0s}.
//! This probe measures cheaper variants to find the Pareto-optimal ship:
//!
//! - 1-window {0}                  — production baseline equivalent
//! - 2-window {0, 1.0}
//! - 2-window {0, 1.5}
//! - 2-window {0, 2.0}             — extreme late-dt
//! - 3-window {0, 1.0, 2.0}        — balanced
//! - 5-window {0, 0.5, 1.0, 1.5, 2.0} — Batch 42 baseline
//!
//! Each "window" decodes samples[N*SR..N*SR+WINDOW_SAMPLES] where
//! WINDOW_SAMPLES=151680 (12.64s × 12kHz).
//!
//! Also measures "production-baseline-with-full-15s-WAV" for reference.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch43_hb221_variants

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;
const WINDOW_SAMPLES: usize = 151_680;

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
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn decode_window_at(
    samples: &[f32],
    start_s: f64,
    cfg: &Ft8Config,
) -> Result<Option<HashSet<String>>> {
    let start_n = (start_s * SAMPLE_RATE as f64) as usize;
    if start_n + WINDOW_SAMPLES > samples.len() {
        return Ok(None);
    }
    let window = &samples[start_n..start_n + WINDOW_SAMPLES];
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(window)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(Some(decoded.into_iter().map(|d| d.text).collect()))
}

fn decode_full(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let mut base = Ft8Config::default();
    base.max_decode_passes = 2;
    base.ldpc_iterations = 200;

    let variants: Vec<(&str, Vec<f64>)> = vec![
        ("1-window {0.0}", vec![0.0]),
        ("2-window {0, 1.0}", vec![0.0, 1.0]),
        ("2-window {0, 1.5}", vec![0.0, 1.5]),
        ("2-window {0, 2.0}", vec![0.0, 2.0]),
        ("3-window {0, 1.0, 2.0}", vec![0.0, 1.0, 2.0]),
        ("5-window all", vec![0.0, 0.5, 1.0, 1.5, 2.0]),
    ];

    let mut variant_results: Vec<(String, usize, usize)> = Vec::new(); // (label, tps, total)
    let mut full_15s_tps = 0usize;
    let mut full_15s_total = 0usize;

    eprintln!("Variant sweep (each = N decodes per WAV, union TPs)…");
    for (label, offsets) in &variants {
        eprintln!("  {}…", label);
        let mut tps = 0usize;
        let mut total = 0usize;
        for entry in entries.iter() {
            let wav_path = entry["wav_path"].as_str().context("wav_path")?;
            let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
            let samples = load_wav(Path::new(wav_path))?;
            let truth = load_truth(&ws, sha);
            let mut union: HashSet<String> = HashSet::new();
            for &off in offsets {
                if let Some(d) = decode_window_at(&samples, off, &base)? {
                    union.extend(d);
                }
            }
            total += union.len();
            for t in &union {
                if truth.contains(t) {
                    tps += 1;
                }
            }
        }
        variant_results.push((label.to_string(), tps, total));
    }

    eprintln!("Full-15s production baseline…");
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let decoded = decode_full(&samples, &base)?;
        full_15s_total += decoded.len();
        for t in &decoded {
            if truth.contains(t) {
                full_15s_tps += 1;
            }
        }
    }

    println!("## Batch 43 — hb-221 multi-interval variant sweep");
    println!(
        "\nProduction baseline (full 15s WAV, mp=2+ldpc=200): {} dec / {} TPs",
        full_15s_total, full_15s_tps
    );

    println!("\n### Variant results (vs production baseline)");
    println!(
        "  {:<26} {:>5} dec {:>6} TPs {:>+6}",
        "variant", "", "", "ΔTPs"
    );
    for (label, tps, total) in &variant_results {
        let dt = *tps as i64 - full_15s_tps as i64;
        let prec = *tps as f64 / (*total).max(1) as f64 * 100.0;
        println!(
            "  {:<26} {:>5}     {:>6} {:>+6}   ({:>5.1}% prec)",
            label, total, tps, dt, prec
        );
    }

    println!("\n### Recommendation:");
    let best = variant_results
        .iter()
        .filter(|(label, _, _)| label.contains("2-window") || label.contains("3-window"))
        .max_by_key(|(_, tps, _)| *tps);
    if let Some((label, tps, _)) = best {
        let dt = *tps as i64 - full_15s_tps as i64;
        if dt > 30 {
            println!("  → SHIP {} (Δ {:+} TPs over full-15s baseline)", label, dt);
        } else {
            println!(
                "  → All 2/3-window variants ≤30 TPs over baseline; consider 5-window or revisit"
            );
        }
    }

    Ok(())
}
