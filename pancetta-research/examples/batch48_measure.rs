//! Batch 48 — measure hb-242 + wide-lag baseline + hb-245 + hb-229 impact.
//!
//! Comparison points (all on hard-200, mp=2 + ldpc=200):
//! - baseline (production state before Batch 48 ships)
//! - hb-242 sync_bc ON only (default-on flag, but explicit for clarity)
//! - hb-242 + wide-lag baseline (both flags ON)
//! - all-default config (whatever the new defaults give)
//!
//! hb-245 (parabolic time refinement) was already shipped as hb-044 —
//! this batch's contribution is documentation only, no measurement
//! delta.
//!
//! hb-229 (QSO partner band-collapse) is autonomous-only — only fires
//! when a QSO is in-flight. Hard-200 corpus has no live QSO state, so
//! it's a no-op for this measurement. Production impact is operational
//! (CPU savings during QSO), not recall.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch48_measure

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

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
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
    Ok((total, tps))
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

    println!("## Batch 48 — hb-242 + wide-lag baseline measurement");
    println!("(hb-245 was already shipped as hb-044; hb-229 is autonomous-only)");

    // Production-before-Batch-48 baseline: mp=2 + ldpc=200, hb-242 OFF
    let mut cfg_old = Ft8Config::default();
    cfg_old.max_decode_passes = 2;
    cfg_old.ldpc_iterations = 200;
    cfg_old.costas_partial_metric_enabled = false; // explicit OFF
    cfg_old.costas_two_baseline_enabled = false;
    eprintln!("baseline (hb-242 OFF, wide-lag OFF)…");
    let (b_total, b_tps) = run(&entries, &cfg_old)?;
    println!(
        "\n### Baseline (hb-242 OFF, wide-lag OFF): {} decodes / {} TPs",
        b_total, b_tps
    );

    // hb-242 ON (sync_bc) — this is the new DEFAULT
    let mut cfg_h242 = cfg_old.clone();
    cfg_h242.costas_partial_metric_enabled = true;
    eprintln!("hb-242 ON (default)…");
    let (t, p) = run(&entries, &cfg_h242)?;
    println!(
        "\n### hb-242 sync_bc ON (new default): {} decodes / {} TPs (Δ {:+})",
        t,
        p,
        p as i64 - b_tps as i64
    );

    // hb-242 + wide-lag baseline ON
    let mut cfg_both = cfg_h242.clone();
    cfg_both.costas_two_baseline_enabled = true;
    eprintln!("hb-242 + wide-lag baseline ON…");
    let (t2, p2) = run(&entries, &cfg_both)?;
    println!(
        "\n### hb-242 + wide-lag baseline ON: {} decodes / {} TPs (Δ {:+})",
        t2,
        p2,
        p2 as i64 - b_tps as i64
    );

    println!("\n### Summary");
    println!(
        "  hb-242 sync_bc alone:           Δ {:+} TPs",
        p as i64 - b_tps as i64
    );
    println!(
        "  hb-242 + wide-lag baseline:    Δ {:+} TPs",
        p2 as i64 - b_tps as i64
    );

    Ok(())
}
