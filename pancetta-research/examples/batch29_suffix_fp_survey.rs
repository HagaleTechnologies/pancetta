//! Batch 29 / Diagnostic H — hb-058 /R + Field-Day FP-pattern survey.
//!
//! JTDX's lineage of FP filters explicitly drops decodes matching common
//! noise-emergent patterns: `/R` callsigns, Field-Day-style abbreviated
//! contest messages. Measure on top-20 hard-200:
//!
//!   1. Total pancetta decodes; total /R-containing; total Field-Day-like
//!   2. Compare against jt9 truth:
//!      - /R decodes that are in truth (legit) vs not (FPs)
//!      - FD decodes ditto
//!   3. PROCEED to filter implementation if FP rate within /R or FD
//!      subsets is significantly higher than overall FP rate.
//!
//! Field-Day-style heuristic: 1A-style emissions (digit + letter combo
//! at end of message) or contest exchanges that look like "ABCD 5A NV"
//! patterns. We use a lightweight regex: token like `1A`..`9Z` or
//! `1ABC`..`9ZZZ` in any position.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_suffix_fp_survey

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

fn contains_slash_r(text: &str) -> bool {
    text.split_whitespace()
        .any(|t| t.ends_with("/R") || t.ends_with("/R1") || t.ends_with("/R2"))
}

/// Heuristic for Field-Day-style contest exchange tokens:
/// `1A`..`9Z` (digit + letter), `1AA`..`9ZZ`, or `1A1`..`9Z9`.
fn contains_fd_token(text: &str) -> bool {
    for t in text.split_whitespace() {
        let chars: Vec<char> = t.chars().collect();
        if chars.len() < 2 || chars.len() > 4 {
            continue;
        }
        if !chars[0].is_ascii_digit() {
            continue;
        }
        if !chars[1].is_ascii_alphabetic() {
            continue;
        }
        // Looks like a Field-Day class/section token.
        return true;
    }
    false
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH29_H_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!(
        "## Batch 29 / Diagnostic H — /R + Field-Day pattern FP survey (top-{top_n} hard-200)"
    );

    let cfg = Ft8Config::default();

    let mut total_decodes = 0usize;
    let mut total_tp = 0usize;
    let mut slash_r_total = 0usize;
    let mut slash_r_tp = 0usize;
    let mut fd_total = 0usize;
    let mut fd_tp = 0usize;

    // Also: same patterns in truth (to gauge expected legit rate).
    let mut truth_total = 0usize;
    let mut slash_r_in_truth = 0usize;
    let mut fd_in_truth = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        truth_total += truth.len();
        for t in &truth {
            if contains_slash_r(t) {
                slash_r_in_truth += 1;
            }
            if contains_fd_token(t) {
                fd_in_truth += 1;
            }
        }

        for d in decoded {
            total_decodes += 1;
            let is_tp = truth.contains(&d.text);
            if is_tp {
                total_tp += 1;
            }
            if contains_slash_r(&d.text) {
                slash_r_total += 1;
                if is_tp {
                    slash_r_tp += 1;
                }
            }
            if contains_fd_token(&d.text) {
                fd_total += 1;
                if is_tp {
                    fd_tp += 1;
                }
            }
        }
    }

    let overall_fp_rate = if total_decodes == 0 {
        0.0
    } else {
        (total_decodes - total_tp) as f64 / total_decodes as f64
    };
    println!("\nOverall:");
    println!(
        "  Decodes: {}  TP: {}  FP rate: {:.2}%",
        total_decodes,
        total_tp,
        overall_fp_rate * 100.0
    );
    println!(
        "  Truth-only: {} messages on hard-200 top-{}; /R-bearing: {}; FD-bearing: {}",
        truth_total, top_n, slash_r_in_truth, fd_in_truth
    );

    println!("\n### /R pattern");
    let slash_r_fp_rate = if slash_r_total == 0 {
        0.0
    } else {
        (slash_r_total - slash_r_tp) as f64 / slash_r_total as f64
    };
    println!(
        "  Pancetta emissions: {} ({:.2}%-of-decodes)  TPs: {}  FP rate: {:.2}%",
        slash_r_total,
        slash_r_total as f64 / total_decodes.max(1) as f64 * 100.0,
        slash_r_tp,
        slash_r_fp_rate * 100.0
    );

    println!("\n### Field-Day pattern");
    let fd_fp_rate = if fd_total == 0 {
        0.0
    } else {
        (fd_total - fd_tp) as f64 / fd_total as f64
    };
    println!(
        "  Pancetta emissions: {} ({:.2}%-of-decodes)  TPs: {}  FP rate: {:.2}%",
        fd_total,
        fd_total as f64 / total_decodes.max(1) as f64 * 100.0,
        fd_tp,
        fd_fp_rate * 100.0
    );

    println!("\n### Verdict");
    if slash_r_total > 0 && slash_r_fp_rate > overall_fp_rate * 1.5 && slash_r_in_truth == 0 {
        println!("  hb-058 /R: PROCEED — /R emissions have FP rate {:.1}× overall, and 0 /R truth on this corpus", slash_r_fp_rate / overall_fp_rate.max(0.0001));
    } else if slash_r_in_truth > 0 {
        println!("  hb-058 /R: SHELVE-CAREFUL — {} legitimate /R truths on this corpus; cannot blanket-reject", slash_r_in_truth);
    } else {
        println!(
            "  hb-058 /R: SHELVE — pancetta emits no /R decodes on this corpus; nothing to filter"
        );
    }

    if fd_total > 0 && fd_fp_rate > overall_fp_rate * 1.5 && fd_in_truth == 0 {
        println!(
            "  hb-058 FD: PROCEED — FD-style emissions have FP rate {:.1}× overall, and 0 FD truth",
            fd_fp_rate / overall_fp_rate.max(0.0001)
        );
    } else if fd_in_truth > 0 {
        println!("  hb-058 FD: SHELVE-CAREFUL — {} legitimate FD truths on this corpus; cannot blanket-reject", fd_in_truth);
    } else {
        println!("  hb-058 FD: SHELVE — no FD pattern emissions on this corpus");
    }

    Ok(())
}
