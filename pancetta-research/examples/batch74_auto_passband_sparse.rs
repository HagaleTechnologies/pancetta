//! Batch 74 — auto_passband re-test on real-world sparse-signal corpus.
//!
//! Batch 55 measured auto_passband on synthetic sparse signals and
//! found it inert (the threshold falls below noise floor → full WG
//! passband returned → byte-identical to OFF). Hypothesis: real
//! recordings have actual signal structure (per-band noise
//! distribution, occasional interferers) that synthetic uniform
//! Gaussian noise can't reproduce.
//!
//! Corpus: `sparse_419.manifest.json` (300 raw 4/19 slots, ~3.6
//! ft8_lib decodes/slot — sparse but actual signal).
//! Truth: ft8_lib FFI baseline (already produced by Batch 71).
//!
//! Two configurations:
//!   1. baseline (auto_passband OFF, current default)
//!   2. auto_passband ON
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch74_auto_passband_sparse

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
        if (i + 1) % 50 == 0 {
            eprintln!("  [{label} {}/{}] tps={tps}", i + 1, entries.len());
        }
    }
    Ok((tot, tps, t0.elapsed().as_secs_f64()))
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/sparse_419.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();
    println!("loaded sparse_419: {} entries", entries.len());

    let cfg_off = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        auto_passband_enabled: false,
        ..Ft8Config::default()
    };
    let cfg_on = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        auto_passband_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("baseline (auto_passband OFF)…");
    let (tot_b, tps_b, secs_b) = run("OFF", &entries, &cfg_off)?;
    let prec_b = tps_b as f64 / tot_b.max(1) as f64;
    println!("OFF: {tot_b} dec / {tps_b} TPs / prec {prec_b:.4} / {secs_b:.0}s");

    eprintln!("auto_passband ON…");
    let (tot_on, tps_on, secs_on) = run("ON", &entries, &cfg_on)?;
    let prec_on = tps_on as f64 / tot_on.max(1) as f64;
    let dtps = tps_on as i64 - tps_b as i64;
    let fps_b = tot_b - tps_b;
    let fps_on = tot_on - tps_on;
    let dfps = fps_on as i64 - fps_b as i64;
    println!(
        "ON: {tot_on} dec / {tps_on} TPs / prec {prec_on:.4} / {secs_on:.0}s (Δ TPs {dtps:+}, Δ FPs {dfps:+})"
    );

    let decision = if dtps >= 5 && dfps <= 0 {
        format!("**auto_passband shows real-world lift on sparse**: +{dtps} TPs / {dfps:+} FPs. Graduation candidate.")
    } else if dtps > 0 && dfps > 0 {
        format!("**Marginal sparse lift, FP-heavy**: +{dtps} TPs / +{dfps} FPs. Precision-positive only if ratio > 5.")
    } else if dtps == 0 && dfps < 0 {
        "**Net-neutral with precision lift**: TPs flat, FPs drop. Defensible default-ON."
            .to_string()
    } else {
        format!(
            "**auto_passband still inert/regressive on sparse**: Δ TPs = {dtps:+}, Δ FPs = {dfps:+}. Same Batch 55 verdict; stays default-OFF."
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let notes_path = ws.join("research/notes/2026-06-09-batch74-auto-passband-sparse.md");
    let body = format!(
        "# Batch 74 — auto_passband on sparse_419 (ft8_lib truth)\n\n\
         {} slots from raw 4/19 day (sparse signal, ~3.6 decodes/slot).\n\n\
         | Config | Decodes | TPs | FPs | Precision |\n|---|---:|---:|---:|---:|\n\
         | OFF | {tot_b} | {tps_b} | {fps_b} | {prec_b:.4} |\n\
         | ON | {tot_on} | {tps_on} | {fps_on} | {prec_on:.4} |\n\n\
         Δ TPs: {dtps:+}, Δ FPs: {dfps:+}\n\n{decision}\n",
        entries.len()
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
