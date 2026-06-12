//! Batch 78 — post-Batch-72 knob audit: max_sync_candidates ×
//! max_parity_errors_for_osd (+ min_sync_score spot checks) on
//! raw_530_full with ft8_lib truth.
//!
//! Batch 72 found the hard-200-tuned `osd_depth` default was costing
//! ~7000 FPs on realistic traffic. This audits the remaining
//! hard-200-era knobs the same way. Two stages per probe-baseline
//! discipline (broad sweeps capped on a subset before full runs):
//!
//!   Stage 1 (default): 11 configs × first 200 slots (~30 min).
//!     3×3 lattice of max_sync_candidates {150,300,600} ×
//!     max_parity_errors_for_osd {3,6,10}, plus min_sync_score
//!     {2.5,3.5} at the default lattice point.
//!   Stage 2: PANCETTA_B78_FULL="<idx,idx,...>" runs the named config
//!     indices (from the stage-1 table) on all 2066 slots.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch78_knob_audit
//!   PANCETTA_B78_FULL="0,3" cargo run --release -p pancetta-research --example batch78_knob_audit

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
        if (i + 1) % 400 == 0 {
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

fn configs() -> Vec<(String, Ft8Config)> {
    let base = Ft8Config::default();
    let mut out = Vec::new();
    // Refinement mode: PANCETTA_B78_CANDS="75,100,200" sweeps just
    // max_sync_candidates at default parity, plus the default itself.
    if let Ok(spec) = std::env::var("PANCETTA_B78_CANDS") {
        out.push(("cands=300 [DEFAULT]".to_string(), base.clone()));
        for cands in spec.split(',').filter_map(|s| s.trim().parse::<usize>().ok()) {
            out.push((
                format!("cands={cands}"),
                Ft8Config {
                    max_sync_candidates: cands,
                    ..base.clone()
                },
            ));
        }
        return out;
    }
    for &cands in &[150usize, 300, 600] {
        for &parity in &[3usize, 6, 10] {
            let label = format!(
                "cands={cands} parity={parity}{}",
                if cands == 300 && parity == 6 {
                    " [DEFAULT]"
                } else {
                    ""
                }
            );
            out.push((
                label,
                Ft8Config {
                    max_sync_candidates: cands,
                    max_parity_errors_for_osd: parity,
                    ..base.clone()
                },
            ));
        }
    }
    for &score in &[2.5f64, 3.5] {
        out.push((
            format!("min_sync_score={score} (cands=300 parity=6)"),
            Ft8Config {
                min_sync_score: score,
                ..base.clone()
            },
        ));
    }
    out
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let all_entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let full_spec = std::env::var("PANCETTA_B78_FULL").ok();
    let all_configs = configs();

    let (stage, entries, selected): (&str, &[Value], Vec<usize>) = match &full_spec {
        Some(spec) => {
            let idx: Vec<usize> = spec
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            anyhow::ensure!(!idx.is_empty(), "PANCETTA_B78_FULL parsed to no indices");
            ("full", &all_entries, idx)
        }
        None => (
            "subset-200",
            &all_entries[..200.min(all_entries.len())],
            (0..all_configs.len()).collect(),
        ),
    };
    println!(
        "stage={stage}: {} slots, {} configs",
        entries.len(),
        selected.len()
    );

    let mut rows: Vec<(usize, String, usize, usize, f64, f64)> = Vec::new();
    for &ci in &selected {
        let (label, cfg) = &all_configs[ci];
        eprintln!("---- [{ci}] {label} ----");
        let (tot, tps, secs) = run(label, entries, cfg)?;
        let prec = tps as f64 / tot.max(1) as f64;
        println!("[{ci}] {label}: {tot} dec / {tps} TPs / prec {prec:.4} / {secs:.0}s");
        rows.push((ci, label.clone(), tot, tps, prec, secs));
    }

    // Baseline row = the [DEFAULT] config if present, else first row.
    let base_row = rows
        .iter()
        .find(|r| r.1.contains("[DEFAULT]"))
        .unwrap_or(&rows[0])
        .clone();
    let (base_tot, base_tps) = (base_row.2, base_row.3);

    let mut body = String::new();
    body.push_str(&format!(
        "# Batch 78 — knob audit ({stage}) on raw_530_full (ft8_lib truth)\n\n\
         Baseline row: {}\n\n\
         | # | Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Wall |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        base_row.1
    ));
    for (ci, label, tot, tps, prec, secs) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - (base_tps as f64 / base_tot.max(1) as f64);
        body.push_str(&format!(
            "| {ci} | {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} | {secs:.0}s |\n"
        ));
    }
    let notes_path = ws.join(format!(
        "research/notes/2026-06-11-batch78-knob-audit-{stage}.md"
    ));
    std::fs::write(&notes_path, &body)?;
    println!("\n{body}\nwrote {}", notes_path.display());
    Ok(())
}
