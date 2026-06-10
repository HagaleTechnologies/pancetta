//! Batch 56 — LLR whitening Slow-tier preset validation.
//!
//! Batch 53 graduated LLR whitening to default-ON based on a hard_1000
//! measurement at Fast/Moderate tier preset
//! (`max_decode_passes = 2, ldpc_iterations = 200`). The Slow tier
//! preset (`max_decode_passes = 1, osd_depth = Some(1)`) is the
//! production target for the FTdx10 MiniPC. This probe verifies the
//! lift survives at the Slow preset.
//!
//! Two configs on hard_1000:
//!   1. Slow tier baseline (whitening OFF, `mp=1, osd_depth=Some(1)`)
//!   2. Slow tier + whitening ON
//!
//! Decision rule:
//!   - If TPs ↑ AND precision ↑ → default-ON is correct at Slow tier
//!     too; no further action needed.
//!   - If TPs ↑ but precision flat-or-↓ → flag for Batch 57 inspection.
//!   - If TPs ↓ → tier-conditional default needed; flip default-OFF
//!     for Slow tier specifically, keep default-ON for Fast/Moderate.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch56_llr_whitening_slow_tier

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
    for (i, entry) in entries.iter().enumerate() {
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
        if (i + 1) % 100 == 0 {
            eprintln!("    [{}/{}] tps so far: {tps}", i + 1, entries.len());
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
    println!("loaded hard_1000: {} entries", entries.len());

    // Slow-tier preset: max_decode_passes=1, osd_depth=Some(1).
    let cfg_off = Ft8Config {
        max_decode_passes: 1,
        osd_depth: Some(1),
        llr_whitening_enabled: false,
        ..Ft8Config::default()
    };
    let cfg_on = Ft8Config {
        max_decode_passes: 1,
        osd_depth: Some(1),
        llr_whitening_enabled: true,
        ..Ft8Config::default()
    };

    eprintln!("Slow-tier baseline (whitening OFF)…");
    let (tot_b, tps_b, secs_b) = run(&entries, &cfg_off)?;
    let prec_b = tps_b as f64 / tot_b.max(1) as f64;
    println!(
        "\nSlow baseline (OFF): {tot_b} decodes / {tps_b} TPs ({secs_b:.1}s, prec {prec_b:.4})"
    );

    eprintln!("Slow-tier + whitening ON…");
    let (tot_on, tps_on, secs_on) = run(&entries, &cfg_on)?;
    let delta = tps_on as i64 - tps_b as i64;
    let prec_on = tps_on as f64 / tot_on.max(1) as f64;
    println!(
        "Slow whitening ON: {tot_on} decodes / {tps_on} TPs ({secs_on:.1}s, Δ {delta:+}, prec {prec_on:.4})"
    );

    let fps_b = tot_b - tps_b;
    let fps_on = tot_on - tps_on;
    let delta_fps = fps_on as i64 - fps_b as i64;

    let decision = if delta > 0 && prec_on > prec_b {
        "**Slow-tier mirror of Fast-tier**: TPs ↑ AND precision ↑. Default-ON is correct across all tiers; no tier-conditional flipping needed."
    } else if delta == 0 && delta_fps < 0 {
        "**Borderline (TPs flat, FPs drop)**: precision-only lift at Slow tier; default-ON still defensible."
    } else if delta < 0 {
        "**TIER-CONDITIONAL FLIP NEEDED**: TPs regress at Slow tier. Add tier-conditional default in coordinator/tier.rs — Slow rewrite should set `llr_whitening_enabled = false`."
    } else if delta_fps > 0 {
        "**Caution**: FPs rise at Slow tier. Even if TPs lift, the FP lift may bother operators. Re-evaluate the graduation."
    } else {
        "**Net-zero**: whitening is inert at Slow tier (probably because `mp=1, osd_depth=1` doesn't engage the LLR whitening code paths that mattered at Fast tier)."
    };

    let notes_path = ws.join("research/notes/2026-06-09-batch56-llr-whitening-slow-tier.md");
    let body = format!(
        "# Batch 56 — LLR whitening Slow-tier preset on hard_1000\n\n\
         Verification that Batch 53's default-ON graduation extends to \
         the Slow-tier preset (`max_decode_passes = 1, osd_depth = Some(1)`), \
         which is the FTdx10 MiniPC production target.\n\n\
         | Config | Decodes | TPs | FPs | Δ TPs | Precision | Elapsed |\n\
         |---|---:|---:|---:|---:|---:|---:|\n\
         | Slow baseline (OFF) | {tot_b} | {tps_b} | {fps_b} | 0 | {prec_b:.4} | {secs_b:.1}s |\n\
         | Slow whitening ON | {tot_on} | {tps_on} | {fps_on} | {delta:+} | {prec_on:.4} | {secs_on:.1}s |\n\n\
         **Δ FPs**: {delta_fps:+}\n\n\
         ## Decision\n\n{decision}\n\n\
         ## Comparison to Batch 53 (Fast/Moderate tier)\n\n\
         - Batch 53 (mp=2, ldpc=200) on hard_1000: +4 TPs / -713 FPs / +3.3% precision\n\
         - Batch 56 (mp=1, osd_depth=1) on hard_1000: {delta:+} TPs / {delta_fps:+} FPs / precision Δ {:.4}\n",
        prec_on - prec_b,
    );
    std::fs::write(&notes_path, body)?;
    println!("\nDone. Results in {}", notes_path.display());
    Ok(())
}
