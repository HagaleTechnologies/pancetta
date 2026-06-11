//! Batch 72 — decoder knob sweep on raw_530_full corpus.
//!
//! Hypothesis: hard_200-tuned defaults (max_decode_passes=2,
//! ldpc_iterations=200, osd_depth=Some(1)) may not be optimal for the
//! realistic recording conditions in raw_530_full (2066 sequential
//! slots from 2026-05-30, characterized as 25 decodes/slot,
//! repeat-heavy, QSO-continuous).
//!
//! Sweeps 8 configurations against ft8_lib truth:
//!   1. mp=1, ldpc=200, osd=Some(1) — Slow tier preset
//!   2. mp=2, ldpc=100, osd=Some(1) — less LDPC
//!   3. mp=2, ldpc=200, osd=None — BP only (no OSD)
//!   4. mp=2, ldpc=200, osd=Some(0) — OSD-0 only
//!   5. mp=2, ldpc=200, osd=Some(2) — deeper OSD
//!   6. mp=2, ldpc=300, osd=Some(1) — more LDPC
//!   7. mp=3, ldpc=200, osd=Some(1) — more passes
//!   8. mp=3, ldpc=300, osd=Some(2) — Fast tier max-effort
//!
//! Decision rule:
//!   - Default reference from Batch 68: mp=2 / ldpc=200 / osd=Some(1)
//!     → 37902 TPs / 17125 FPs / precision 0.6889
//!   - If any config beats default by ≥+50 TPs AND precision drop
//!     ≤ -0.01 absolute → graduation candidate
//!   - If any config matches default TPs but improves precision by
//!     ≥ +0.01 → speed/precision win
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch72_knob_sweep

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
        ws.join("research/corpus/curated/ft8/raw_530_full.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded raw_530_full: {} entries", entries.len());

    let configs: Vec<(&str, usize, usize, Option<u8>)> = vec![
        ("baseline default", 2, 200, Some(1)),
        ("Slow tier", 1, 200, Some(1)),
        ("less LDPC", 2, 100, Some(1)),
        ("BP only (no OSD)", 2, 200, None),
        ("OSD-0 only", 2, 200, Some(0)),
        ("deeper OSD-2", 2, 200, Some(2)),
        ("more LDPC", 2, 300, Some(1)),
        ("more passes mp=3", 3, 200, Some(1)),
        ("Fast tier max", 3, 300, Some(2)),
    ];
    let mut rows: Vec<(String, usize, usize, f64, f64)> = Vec::new();
    for (label, mp, ldpc, osd) in &configs {
        eprintln!("---- {label} (mp={mp}, ldpc={ldpc}, osd={osd:?}) ----");
        let cfg = Ft8Config {
            max_decode_passes: *mp,
            ldpc_iterations: *ldpc,
            osd_depth: *osd,
            ..Ft8Config::default()
        };
        let (tot, tps, secs) = run(label, &entries, &cfg)?;
        let prec = tps as f64 / tot.max(1) as f64;
        println!(
            "{label} (mp={mp}, ldpc={ldpc}, osd={osd:?}): {tot} dec / {tps} TPs / prec {prec:.4} / {secs:.0}s"
        );
        rows.push((label.to_string(), tot, tps, prec, secs));
    }

    // Baseline is row 0.
    let (_, base_tot, base_tps, base_prec, _) = &rows[0];
    println!("\n## Knob sweep summary (vs default baseline)\n");
    println!("| Label | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec | Elapsed |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (label, tot, tps, prec, secs) in &rows {
        let fps = tot - tps;
        let base_fps = base_tot - base_tps;
        let dtps = *tps as i64 - *base_tps as i64;
        let dfps = fps as i64 - base_fps as i64;
        let dprec = prec - base_prec;
        println!(
            "| {label} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} | {secs:.0}s |"
        );
    }

    let notes_path = ws.join("research/notes/2026-06-09-batch72-knob-sweep.md");
    let mut body = String::new();
    body.push_str("# Batch 72 — decoder knob sweep on raw_530_full (ft8_lib truth)\n\n");
    body.push_str(
        "| Label | mp | ldpc | osd | Decodes | TPs | Δ TPs | FPs | Δ FPs | Precision | Δ Prec |\n",
    );
    body.push_str("|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for ((label, mp, ldpc, osd), (_, tot, tps, prec, _)) in configs.iter().zip(rows.iter()) {
        let fps = tot - tps;
        let dtps = *tps as i64 - *base_tps as i64;
        let dfps = fps as i64 - (base_tot - base_tps) as i64;
        let dprec = prec - base_prec;
        body.push_str(&format!(
            "| {label} | {mp} | {ldpc} | {osd:?} | {tot} | {tps} | {dtps:+} | {fps} | {dfps:+} | {prec:.4} | {dprec:+.4} |\n"
        ));
    }
    std::fs::write(&notes_path, body)?;
    Ok(())
}
