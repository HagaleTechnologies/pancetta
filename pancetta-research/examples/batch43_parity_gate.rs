//! Batch 43 — hb-224 probe: max_parity_errors_for_osd sweep.
//!
//! ft8mon uses osd_ldpc_thresh=70 (parity errors ≤ 13).
//! Pancetta default: max_parity_errors_for_osd=6 (parity errors ≤ 6).
//!
//! Probe widens the gate to {6 baseline, 9, 13, 20} to test whether
//! ft8mon's looser gate surfaces additional TPs at acceptable FP cost.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch43_parity_gate

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

fn run_pass(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, std::time::Duration)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
    let start = std::time::Instant::now();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }
    }
    Ok((total, tps, start.elapsed()))
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

    println!("## Batch 43 — max_parity_errors_for_osd sweep");

    let gate_values: Vec<usize> = vec![6, 9, 13, 20];

    println!(
        "\n{:<8} {:>10} {:>8} {:>8} {:>9}",
        "gate", "decodes", "TPs", "ΔTPs", "elapsed_s"
    );

    let mut baseline_tps = 0usize;
    for (i, &g) in gate_values.iter().enumerate() {
        let mut cfg = base.clone();
        cfg.max_parity_errors_for_osd = g;
        eprintln!("  gate={}…", g);
        let (total, tps, dur) = run_pass(&entries, &cfg)?;
        if i == 0 {
            baseline_tps = tps;
        }
        println!(
            "  {:<8} {:>10} {:>8} {:>+8} {:>9.1}",
            g,
            total,
            tps,
            tps as i64 - baseline_tps as i64,
            dur.as_secs_f64()
        );
    }

    Ok(())
}
