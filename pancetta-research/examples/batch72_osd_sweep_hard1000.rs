//! Batch 72 verification — OSD depth sweep on hard_1000.
//!
//! Batch 72 raw_530_full sweep found that pancetta's production
//! default `osd_depth = Some(2)` produces ~7000 spurious FPs on
//! real-world recordings. Before shipping a default change, verify
//! the same pattern holds on hard_1000 (the canonical curated corpus
//! that drove the original LLR whitening graduation).
//!
//! 5 configs (mp=2, ldpc=200, ft8_lib truth):
//!   osd=None / Some(0) / Some(1) / Some(2) (default) / Some(3)
//!
//! Decision rule:
//!   - If osd=None/Some(0)/Some(1) all have Δ TPs ≥ -10 AND Δ FPs ≤
//!     -500 vs default → ship default change to lower OSD
//!   - If hard_1000 prefers osd=Some(2) → keep default; document
//!     corpus-specific knob behavior
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch72_osd_sweep_hard1000

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
    let mut tot = 0usize;
    let mut tps = 0usize;
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
        if (i + 1) % 100 == 0 {
            eprintln!(
                "  [{label} {}/{}] tps={tps} ({:.0}s)",
                i + 1,
                entries.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    Ok((tot, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_1000.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded hard_1000: {} entries", entries.len());

    let osd_configs: Vec<(&str, Option<u8>)> = vec![
        ("osd=None", None),
        ("osd=Some(0)", Some(0)),
        ("osd=Some(1)", Some(1)),
        ("osd=Some(2) [current default]", Some(2)),
        ("osd=Some(3)", Some(3)),
    ];
    let mut rows: Vec<(String, usize, usize, f64, f64)> = Vec::new();
    for (label, osd) in &osd_configs {
        eprintln!("---- {label} ----");
        let cfg = Ft8Config {
            max_decode_passes: 2,
            ldpc_iterations: 200,
            osd_depth: *osd,
            ..Ft8Config::default()
        };
        let (tot, tps, secs) = run(label, &entries, &cfg)?;
        let prec = tps as f64 / tot.max(1) as f64;
        println!("{label}: {tot} dec / {tps} TPs / prec {prec:.4} / {secs:.0}s");
        rows.push((label.to_string(), tot, tps, prec, secs));
    }

    // Find current-default (osd=Some(2)) row as baseline.
    let base = rows
        .iter()
        .find(|(l, _, _, _, _)| l.contains("current default"))
        .expect("default row");
    let (base_tot, base_tps, base_prec) = (base.1, base.2, base.3);

    println!("\n## OSD depth sweep on hard_1000 (vs current default Some(2))\n");
    println!("| Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (label, tot, tps, prec, secs) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - base_prec;
        println!(
            "| {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} | {secs:.0}s |"
        );
    }

    let notes_path = ws.join("research/notes/2026-06-09-batch72-osd-sweep-hard1000.md");
    let mut body = String::new();
    body.push_str("# Batch 72 verification — OSD depth sweep on hard_1000\n\n");
    body.push_str("| Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec |\n");
    body.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for (label, tot, tps, prec, _) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - base_prec;
        body.push_str(&format!(
            "| {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} |\n"
        ));
    }
    std::fs::write(&notes_path, body)?;
    Ok(())
}
