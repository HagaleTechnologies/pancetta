//! Batch 52 — re-measure WSJT-X mainline npre2 OSD preprocessing on
//! hard-200 with `osd_depth = Some(3)` to expose the mechanism.
//!
//! Batch 51 measured npre2 at the default `osd_depth = Some(1)` and saw
//! +0 TPs — expected, because the npre2 hash-table warm start only
//! activates when `osd_depth >= 3` (see `decoder.rs:138`). This probe
//! lifts the OSD depth to 3 so the mechanism actually fires, then
//! toggles `osd_npre2_preprocessing_enabled` to isolate npre2's
//! contribution.
//!
//! Matrix (on top of `max_decode_passes = 2, ldpc_iterations = 200`):
//!
//! 1. **Baseline (depth=3)**: `osd_depth = Some(3)`, npre2 OFF
//! 2. **npre2 ON (depth=3)**: same + `osd_npre2_preprocessing_enabled = true`
//!
//! Stop conditions:
//! - If the depth=3 baseline differs from the default-depth baseline
//!   (5301 TPs reference) by more than ±20 TPs, the depth lift itself
//!   is a meaningful intervention and should be flagged.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch52_npre2_depth3

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

fn run(entries: &[Value], cfg: &Ft8Config) -> Result<(usize, usize, f64)> {
    let ws = workspace_root()?;
    let mut total = 0usize;
    let mut tps = 0usize;
    let t0 = std::time::Instant::now();
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
    Ok((total, tps, t0.elapsed().as_secs_f64()))
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

    println!("loaded hard-200 manifest: {} entries", entries.len());
    println!("## Batch 52 — npre2 at OSD depth 3 measurement");

    // Two-config matrix: depth=3 baseline (npre2 OFF) vs depth=3 + npre2 ON.
    // The OSD depth lift itself is a confound — capture both numbers and
    // surface the depth=3 baseline delta vs the reference 5301 TPs separately.

    let cfg_depth3_off = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        osd_depth: Some(3),
        osd_npre2_preprocessing_enabled: false,
        ..Ft8Config::default()
    };

    let cfg_depth3_on = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        osd_depth: Some(3),
        osd_npre2_preprocessing_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("baseline at osd_depth=3 (npre2 OFF)…");
    let (tot_b, tps_b, secs_b) = run(&entries, &cfg_depth3_off)?;
    println!("\nbaseline (depth=3, npre2 OFF): {tot_b} decodes / {tps_b} TPs ({secs_b:.1}s)");

    eprintln!("npre2 ON at osd_depth=3…");
    let (tot_on, tps_on, secs_on) = run(&entries, &cfg_depth3_on)?;
    let delta = tps_on as i64 - tps_b as i64;
    println!("npre2 ON (depth=3): {tot_on} decodes / {tps_on} TPs ({secs_on:.1}s, Δ {delta:+})");

    // Reference: default baseline (depth=1, npre2 OFF) on hard-200 = 5301 TPs.
    let default_baseline_tps: i64 = 5301;
    let depth3_delta_vs_default = tps_b as i64 - default_baseline_tps;

    let notes_path = ws.join("research/notes/2026-06-09-batch52-npre2-measurement.md");
    let precision_b = tps_b as f64 / tot_b.max(1) as f64;
    let precision_on = tps_on as f64 / tot_on.max(1) as f64;

    let stop_flag = if depth3_delta_vs_default.abs() > 20 {
        format!(
            "\n> **STOP-CONDITION TRIGGERED**: depth=3 baseline (TPs={tps_b}) \
             differs from default-depth reference (5301) by \
             {depth3_delta_vs_default:+} TPs (>±20). The OSD depth lift \
             itself is a meaningful intervention; treat depth=3 as a separate \
             ship candidate rather than a neutral re-baseline for npre2.\n"
        )
    } else {
        String::new()
    };

    let body = format!(
        "# Batch 52 — npre2 OSD preprocessing at osd_depth=3 (hard-200)\n\n\
         Re-measurement of `osd_npre2_preprocessing_enabled` on hard-200 \
         with the OSD depth lifted to 3 so the npre2 hash-table warm start \
         actually fires (the mechanism is a no-op at the default \
         `osd_depth = Some(1)`).\n\n\
         Baseline config: `max_decode_passes = 2`, `ldpc_iterations = 200`, \
         `osd_depth = Some(3)`. Reference TPs at default `osd_depth = Some(1)` \
         from prior batches: **5301**.\n\n\
         | Config | Decodes | TPs | Δ vs depth=3 baseline | Δ vs default-depth (5301) | Precision | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|---:|\n\
         | baseline (depth=3, npre2 OFF) | {tot_b} | {tps_b} | 0 | {depth3_delta_vs_default:+} | {precision_b:.4} | {secs_b:.1}s |\n\
         | npre2 ON (depth=3) | {tot_on} | {tps_on} | {delta:+} | {:+} | {precision_on:.4} | {secs_on:.1}s |\n\
         {stop_flag}\n",
        tps_on as i64 - default_baseline_tps,
    );

    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
