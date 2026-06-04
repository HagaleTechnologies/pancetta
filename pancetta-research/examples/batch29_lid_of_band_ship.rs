//! Batch 29 / Diagnostic G — hb-156 lid-of-band manifest ship-fix.
//!
//! Batch 28's hb-156 attempt filtered at `manifest.entries.mean_decoded_snr_db`
//! which is per-WAV pancetta-relative SNR — wrong granularity. This
//! diagnostic reads PER-TRUTH-DECODE jt9 SNR from the baseline JSONs,
//! filters WAVs where ANY truth has snr_db ≤ threshold, and ships
//! `research/corpus/curated/ft8/lid_of_band.manifest.json`.
//!
//! Sources: hard_200 + wild_100.
//!
//! Output: lid_of_band.manifest.json with per-WAV summary including
//! `min_truth_snr_db` and `n_truths_at_threshold`. Each entry can be
//! evaluated as its own slot — the eval harness can pick this up as a
//! new tier.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_lid_of_band_ship

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_baseline(ws: &Path, sha: &str) -> Option<Value> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let txt = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&txt).ok()
}

#[derive(Serialize)]
struct LidEntry {
    wav_path: String,
    wav_sha256: String,
    /// Source tier (curated-hard-200 or wild-100).
    source_tier: String,
    /// Minimum per-decode SNR among jt9 truths on this WAV.
    min_truth_snr_db: f64,
    /// Number of jt9 truths at or below the SNR threshold.
    n_truths_at_or_below_threshold: usize,
    /// Total jt9 truths on this WAV.
    n_truths_total: usize,
}

#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    label: &'static str,
    generated_at: String,
    snr_threshold_db: f64,
    sources: Vec<&'static str>,
    entries: Vec<LidEntry>,
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let threshold: f64 = std::env::var("BATCH29_LID_SNR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-19.0);

    println!(
        "## Batch 29 / Diagnostic G — lid-of-band manifest (SNR ≤ {} dB)",
        threshold
    );

    let mut all_entries: Vec<LidEntry> = Vec::new();
    let mut total_wavs_examined = 0usize;
    let mut wavs_with_weak_truth = 0usize;

    for (manifest_file, tier_name) in [
        ("hard_200.manifest.json", "curated-hard-200"),
        ("wild_100.manifest.json", "wild-100"),
    ] {
        let path = ws.join(format!("research/corpus/curated/ft8/{manifest_file}"));
        let Ok(txt) = std::fs::read_to_string(&path) else {
            continue;
        };
        let m: Value = serde_json::from_str(&txt)?;
        let Some(entries) = m["entries"].as_array() else {
            continue;
        };
        for e in entries {
            total_wavs_examined += 1;
            let wav_path = e["wav_path"].as_str().unwrap_or("").to_string();
            let sha = e["wav_sha256"].as_str().unwrap_or("").to_string();
            let Some(b) = load_baseline(&ws, &sha) else {
                continue;
            };
            let Some(decodes) = b["decodes"].as_array() else {
                continue;
            };
            let mut min_snr = f64::INFINITY;
            let mut n_below = 0usize;
            let n_total = decodes.len();
            for d in decodes {
                if let Some(snr) = d["snr_db"].as_f64() {
                    if snr < min_snr {
                        min_snr = snr;
                    }
                    if snr <= threshold {
                        n_below += 1;
                    }
                }
            }
            if n_below > 0 && min_snr.is_finite() {
                wavs_with_weak_truth += 1;
                all_entries.push(LidEntry {
                    wav_path,
                    wav_sha256: sha,
                    source_tier: tier_name.to_string(),
                    min_truth_snr_db: min_snr,
                    n_truths_at_or_below_threshold: n_below,
                    n_truths_total: n_total,
                });
            }
        }
    }

    println!(
        "  WAVs examined: {} (hard-200 + wild-100)",
        total_wavs_examined
    );
    println!(
        "  WAVs with at least one truth at SNR ≤ {} dB: {}",
        threshold, wavs_with_weak_truth
    );

    if all_entries.is_empty() {
        println!("  Verdict: NO-SHIP — no WAVs have weak truths at this threshold");
        return Ok(());
    }

    // Sort by min_truth_snr_db ascending (weakest first).
    all_entries.sort_by(|a, b| {
        a.min_truth_snr_db
            .partial_cmp(&b.min_truth_snr_db)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n  Top-10 weakest-truth WAVs:");
    println!("    sha          min_snr  n_at/total  tier");
    for e in all_entries.iter().take(10) {
        println!(
            "    {:8}    {:>6.1}  {:>3}/{:>3}  {}",
            &e.wav_sha256[..8],
            e.min_truth_snr_db,
            e.n_truths_at_or_below_threshold,
            e.n_truths_total,
            e.source_tier
        );
    }

    let total_at = all_entries
        .iter()
        .map(|e| e.n_truths_at_or_below_threshold)
        .sum::<usize>();
    let total_truths = all_entries.iter().map(|e| e.n_truths_total).sum::<usize>();
    println!(
        "\n  Total weak truths in shipped manifest: {} of {} ({:.1}%)",
        total_at,
        total_truths,
        total_at as f64 / total_truths.max(1) as f64 * 100.0
    );

    let manifest = Manifest {
        schema_version: 1,
        label: "lid_of_band",
        generated_at: chrono::Utc::now().to_rfc3339(),
        snr_threshold_db: threshold,
        sources: vec!["curated-hard-200", "wild-100"],
        entries: all_entries,
    };
    let out_path = ws.join("research/corpus/curated/ft8/lid_of_band.manifest.json");
    let json = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(&out_path, json)?;
    println!("\n  SHIPPED: {}", out_path.display());
    println!("  Verdict: SHIPPED — hb-156 lid-of-band tier available as new eval-tier source");

    Ok(())
}
