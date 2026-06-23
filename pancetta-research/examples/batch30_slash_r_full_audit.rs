//! Batch 30 / Diagnostic L — Full hard-200 /R-filter impact audit.
//!
//! Batch 29 H found 62/62 /R emissions on top-20 hard-200 are FPs.
//! This diagnostic extends to the full 200 WAVs and quantifies the
//! impact of the wired filter (Batch 30 production wire):
//!
//!   1. Total pancetta decodes vs truth
//!   2. /R emissions emitted, /R truths in baseline
//!   3. Production wire impact: FPs eliminated, recall preserved
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch30_slash_r_full_audit

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_qso::callsign_continuity::has_high_risk_fp_pattern;
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
        .map(|a| {
            a.iter()
                .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH30_L_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 30 / Diagnostic L — /R-filter full hard-200 audit (top-{top_n})");

    let cfg = Ft8Config::default();

    let mut total_decodes = 0usize;
    let mut total_tp = 0usize;
    let mut slash_r_decodes = 0usize;
    let mut slash_r_tp = 0usize;
    let mut slash_r_truths = 0usize;
    let mut fps_eliminated_by_filter = 0usize;
    let mut tps_lost_by_filter = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        for t in &truth {
            if has_high_risk_fp_pattern(t) {
                slash_r_truths += 1;
            }
        }

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        for d in decoded {
            total_decodes += 1;
            let is_tp = truth.contains(&d.text);
            if is_tp {
                total_tp += 1;
            }
            if has_high_risk_fp_pattern(&d.text) {
                slash_r_decodes += 1;
                if is_tp {
                    slash_r_tp += 1;
                    tps_lost_by_filter += 1;
                } else {
                    fps_eliminated_by_filter += 1;
                }
            }
        }
    }

    let overall_fp_count = total_decodes - total_tp;

    println!("\n  Total pancetta decodes:        {}", total_decodes);
    println!("  Total truth-matched (TP):       {}", total_tp);
    println!("  Total FP (pancetta - truth):    {}", overall_fp_count);
    println!();
    println!("  /R-pattern emissions:           {}", slash_r_decodes);
    println!("  /R-pattern TPs (rovers):        {}", slash_r_tp);
    println!("  /R-pattern truths in baseline:  {}", slash_r_truths);
    println!();
    println!("## Filter impact (Batch 30 production wire)");
    println!(
        "  FPs eliminated by filter:       {}",
        fps_eliminated_by_filter
    );
    println!("  TPs lost by filter:             {}", tps_lost_by_filter);

    let fp_elim_pct = if overall_fp_count > 0 {
        fps_eliminated_by_filter as f64 / overall_fp_count as f64 * 100.0
    } else {
        0.0
    };
    let tp_loss_pct = if total_tp > 0 {
        tps_lost_by_filter as f64 / total_tp as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "  FP reduction:                   {:.2}% of all FPs",
        fp_elim_pct
    );
    println!(
        "  Recall cost:                    {:.4}% of TPs",
        tp_loss_pct
    );

    println!();
    if tps_lost_by_filter == 0 && fps_eliminated_by_filter > 0 {
        println!("## Verdict: PRODUCTION-WIN — filter eliminates {} FPs with ZERO recall cost on full hard-200",
            fps_eliminated_by_filter);
    } else if tps_lost_by_filter > 0 {
        println!("## Verdict: TRADE-OFF — eliminates {} FPs but loses {} TPs ({:.2}% recall cost); consider config flag",
            fps_eliminated_by_filter, tps_lost_by_filter, tp_loss_pct);
    } else {
        println!("## Verdict: NO-OP — no /R patterns emitted on this corpus; filter is dormant");
    }

    Ok(())
}
