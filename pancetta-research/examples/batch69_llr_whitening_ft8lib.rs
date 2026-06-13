//! Batch 69 — re-verify LLR whitening graduation with ft8_lib truth.
//!
//! Batch 53 graduated LLR whitening default-ON based on hard_1000
//! measurement (+4 TPs / -713 FPs) against pancetta-derived truth.
//! Batch 66-68 showed that pancetta-derived truth can hide artifacts
//! (the +8 hb-244 finding evaporated at scale). This probe re-verifies
//! the LLR whitening graduation against neutral ft8_lib truth.
//!
//! If the +4 TPs / -713 FPs result holds with ft8_lib truth, the
//! graduation is robust. If it changes significantly, re-evaluate.
//!
//! Two configs on hard_1000 with ft8_lib truth:
//!   1. baseline (whitening OFF)
//!   2. whitening ON
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch69_llr_whitening_ft8lib

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

fn load_ft8lib_truth(ws: &Path, sha: &str) -> HashSet<String> {
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

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
    let t0 = std::time::Instant::now();
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_ft8lib_truth(&ws, sha);
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
        if (i + 1) % 100 == 0 {
            eprintln!("    [{}/{}] tps={tps}", i + 1, entries.len());
        }
    }
    Ok((total, tps, t0.elapsed().as_secs_f64()))
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
    println!(
        "loaded hard_1000: {} entries (with ft8_lib truth)",
        entries.len()
    );

    let cfg_off = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        llr_whitening_enabled: false,
        ..Ft8Config::default()
    };
    let cfg_on = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        llr_whitening_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("baseline (whitening OFF)…");
    let (tot_b, tps_b, secs_b) = run(&entries, &cfg_off)?;
    let prec_b = tps_b as f64 / tot_b.max(1) as f64;
    println!("baseline (OFF): {tot_b} decodes / {tps_b} TPs ({secs_b:.1}s, prec {prec_b:.4})");

    eprintln!("whitening ON…");
    let (tot_on, tps_on, secs_on) = run(&entries, &cfg_on)?;
    let prec_on = tps_on as f64 / tot_on.max(1) as f64;
    let delta_tps = tps_on as i64 - tps_b as i64;
    let fps_b = tot_b - tps_b;
    let fps_on = tot_on - tps_on;
    let delta_fps = fps_on as i64 - fps_b as i64;
    println!(
        "ON: {tot_on} decodes / {tps_on} TPs ({secs_on:.1}s, Δ {delta_tps:+} TPs, prec {prec_on:.4})"
    );
    println!("Δ FPs: {delta_fps:+}");

    let decision = if delta_tps > 0 && delta_fps <= 0 {
        format!(
            "**LLR whitening graduation HOLDS with ft8_lib truth**: TPs +{delta_tps}, FPs {delta_fps:+}. Graduation is truth-source-independent."
        )
    } else if delta_tps > 0 && delta_fps > 0 && delta_fps < delta_tps * 5 {
        format!(
            "**Mixed under ft8_lib truth**: TPs +{delta_tps}, FPs +{delta_fps}. Ratio {:.2}. Graduation may need re-thinking; precision was the key argument in Batch 53.",
            delta_tps as f64 / delta_fps as f64
        )
    } else if delta_tps == 0 && delta_fps < 0 {
        "**Borderline**: TPs flat but FPs drop. Defensible default-ON.".to_string()
    } else {
        format!(
            "**LLR whitening graduation CONTRADICTED by ft8_lib truth**: TPs {delta_tps:+}, FPs {delta_fps:+}. Batch 53 graduation was pancetta-truth-biased. Need to re-evaluate."
        )
    };
    println!("\n## Decision\n\n{decision}\n");

    let notes_path = ws.join("research/notes/2026-06-09-batch69-llr-ft8lib.md");
    let body = format!(
        "# Batch 69 — LLR whitening re-verified with ft8_lib truth\n\n\
         hard_1000, ft8_lib truth (Batch 66 source).\n\n\
         | Config | Decodes | TPs | FPs | Precision |\n|---|---:|---:|---:|---:|\n\
         | baseline (OFF) | {tot_b} | {tps_b} | {fps_b} | {prec_b:.4} |\n\
         | whitening ON | {tot_on} | {tps_on} | {fps_on} | {prec_on:.4} |\n\n\
         Δ TPs: {delta_tps:+} | Δ FPs: {delta_fps:+}\n\n\
         {decision}\n"
    );
    std::fs::write(&notes_path, body)?;
    Ok(())
}
