//! Batch 29 / Diagnostic K — hard-200 regression sanity check.
//!
//! Decode top-50 hard-200 with current main config + `Ft8Config::default()`,
//! count truth-matched decodes, compare to last scorecard's tier totals.
//! Sanity-check that the diagnostic infrastructure agrees with the
//! production-eval harness.
//!
//! Reports:
//! * top-50 raw decode total
//! * top-50 truth-matched decode total
//! * extrapolation: top-50_rate × (200/50)
//! * comparison to main.json's full hard-200 `truth_decodes_recovered`
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_regression_check

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

    let scorecard: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let baseline_recovered = scorecard["tiers"]["curated-hard-200"]["truth_decodes_recovered"]
        .as_u64()
        .unwrap_or(0) as usize;
    let baseline_wsjtx = scorecard["tiers"]["curated-hard-200"]["wsjtx_decoded"]
        .as_u64()
        .unwrap_or(0) as usize;

    let top_n: usize = std::env::var("BATCH29_K_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    println!("## Batch 29 / Diagnostic K — hard-200 regression check (top-{top_n})");
    println!(
        "  Reference (last scorecard): recovered={} / wsjtx_total={}",
        baseline_recovered, baseline_wsjtx
    );

    let cfg = Ft8Config::default();
    let mut total_decodes = 0usize;
    let mut total_tp = 0usize;
    let mut total_truth = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        total_truth += truth.len();

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            total_decodes += 1;
            if truth.contains(&d.text) {
                total_tp += 1;
            }
        }
    }

    println!("\n  Top-{} results:", top_n);
    println!("    pancetta decodes:        {}", total_decodes);
    println!("    pancetta truth-matched:  {}", total_tp);
    println!("    jt9 truth total:         {}", total_truth);
    println!(
        "    recall on this slice:    {:.1}%",
        total_tp as f64 / total_truth.max(1) as f64 * 100.0
    );

    // Linear extrapolation to full 200
    let extrap_factor = entries.len() as f64 / top_n as f64;
    let extrap_recovered = (total_tp as f64 * extrap_factor).round() as usize;
    let extrap_wsjtx = (total_truth as f64 * extrap_factor).round() as usize;
    println!(
        "    extrapolated to 200:     recovered ~{}, jt9 ~{}",
        extrap_recovered, extrap_wsjtx
    );

    let extrap_pct = extrap_recovered as f64 / baseline_recovered.max(1) as f64 * 100.0;
    let diff_pct = (extrap_pct - 100.0).abs();
    println!(
        "    extrapolated / baseline: {:.1}%  (drift = {:.1}%)",
        extrap_pct, diff_pct
    );

    if diff_pct < 10.0 {
        println!("\n## Verdict: NO REGRESSION — extrapolation within ±10% of baseline");
    } else if diff_pct < 25.0 {
        println!("\n## Verdict: MILD DRIFT ({:.1}%) — sampling variance or partial drift; full eval recommended", diff_pct);
    } else {
        println!(
            "\n## Verdict: POTENTIAL REGRESSION ({:.1}% drift) — run full eval to confirm",
            diff_pct
        );
    }

    Ok(())
}
