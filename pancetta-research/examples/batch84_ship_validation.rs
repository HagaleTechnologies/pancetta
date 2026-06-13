//! Batch 84 — cross-corpus-type validation of the Batch 78-83 ships.
//!
//! The cands=200 default (B78), v3 gate flip (B81, decoder-invisible
//! here) and Fast-preset retirement (B83) were verified on raw_530_full
//! and hard_1000. This probe checks the remaining curated corpus TYPES
//! for surprises: sparse band (sparse_419) and QSO-continuous traffic
//! (qso_continuous_530).
//!
//! Configs: TODAY (plain current defaults) vs PRE-SHIP (cands=300 +
//! mp=2 + ldpc=200 — what a Fast-tier station ran before today).
//!
//! Expectation from the audit corpora: TODAY loses ≤0.2% TPs, sheds
//! FPs, and runs ~4-6× faster. A corpus type where TODAY loses >1% of
//! TPs would be a regression flag worth investigating.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch84_ship_validation

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
        .join(format!("{sha}.ft8lib.json"));
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

fn run(label: &str, entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut tot = 0;
    let mut tps = 0;
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = match load_wav(Path::new(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truth = load_truth(&ws, sha);
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        tot += decoded.len();
        for d in &decoded {
            if truth.contains(&d.text) {
                tps += 1;
            }
        }
        if (i + 1) % 200 == 0 {
            eprintln!("  [{label} {}/{}] tps={tps}", i + 1, entries.len());
        }
    }
    Ok((tot, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let today = Ft8Config::default();
    let pre_ship = Ft8Config {
        max_sync_candidates: 300,
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };

    let mut body = String::from(
        "# Batch 84 — ship validation on remaining corpus types (ft8_lib truth)\n\n\
         TODAY = current defaults; PRE-SHIP = cands=300 + mp=2 + ldpc=200\n\
         (the effective Fast-tier config before Batches 78/83).\n\n",
    );
    for manifest_name in [
        "sparse_419.manifest.json",
        "qso_continuous_530.manifest.json",
    ] {
        let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
            ws.join("research/corpus/curated/ft8").join(manifest_name),
        )?)?;
        let entries: Vec<Value> = manifest["entries"]
            .as_array()
            .context("entries")?
            .iter()
            .cloned()
            .collect();
        let label = manifest_name.trim_end_matches(".manifest.json");
        eprintln!("---- {label}: {} entries ----", entries.len());

        let mut rows = Vec::new();
        for (cfg_label, cfg) in [("PRE-SHIP", &pre_ship), ("TODAY", &today)] {
            let (tot, tps, secs) = run(&format!("{label}/{cfg_label}"), &entries, cfg)?;
            println!("{label} {cfg_label}: {tot} dec / {tps} TPs / {secs:.0}s");
            rows.push((cfg_label, tot, tps, secs));
        }
        let (base_tot, base_tps) = (rows[0].1, rows[0].2);
        body.push_str(&format!("## {label} ({} slots)\n\n", entries.len()));
        body.push_str("| Config | Decodes | TPs | Δ TPs | FPs | Δ FPs | Wall |\n|---|---:|---:|---:|---:|---:|---:|\n");
        for (cfg_label, tot, tps, secs) in &rows {
            let fps = tot - tps;
            let dtps = *tps as i64 - base_tps as i64;
            let dfps = fps as i64 - (base_tot - base_tps) as i64;
            body.push_str(&format!(
                "| {cfg_label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {secs:.0}s |\n"
            ));
        }
        body.push('\n');
    }
    let notes_path = ws.join("research/notes/2026-06-12-batch84-ship-validation.md");
    std::fs::write(&notes_path, &body)?;
    println!("\n{body}\nwrote {}", notes_path.display());
    Ok(())
}
