//! fp_filter_callsign_continuity — MVP FP filter
//!
//! Builds a corpus-wide callsign set from the jt9 baselines in
//! research/baselines/ft8/, decodes hard-200 with pancetta, then filters
//! decodes by whether their callsigns appear elsewhere in the corpus.
//!
//! Per hb-039: 97% of isolated-novel callsigns are singletons (likely FPs).
//! This filter is the application of that finding as a precision filter:
//! a decode whose callsigns appear nowhere else in the 1121 jt9 baselines
//! is much more likely to be a CRC-coincidence FP than a real rare-DX
//! decode.
//!
//! This is an EVAL-HARNESS-ONLY filter: it requires the full corpus
//! baselines to be present. For real-time production application, a
//! different signal source would be needed (operator log + rolling
//! window + cqdx.io API).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example fp_filter_callsign_continuity

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    if idx < tokens.len() && tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
    }
    for t in tokens.iter().skip(idx).take(2) {
        if looks_like_callsign(t) {
            // Strip /R, /P, etc. suffix for matching
            let bare = t.split('/').next().unwrap_or(t);
            out.push(bare.to_string());
        }
    }
    out
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    })
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let baselines_dir = workspace.join("research/baselines/ft8");
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");

    // Load corpus-wide callsign set from all baselines.
    eprintln!("Loading callsigns from {}...", baselines_dir.display());
    let mut corpus_calls: HashSet<String> = HashSet::new();
    let mut per_wav_truth: HashMap<String, HashSet<String>> = HashMap::new();
    let mut baselines = 0;
    for entry in std::fs::read_dir(&baselines_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let sha = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let s = std::fs::read_to_string(&path)?;
        let v: Value = serde_json::from_str(&s)?;
        let decodes = v
            .get("decodes")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let mut truth = HashSet::new();
        for d in &decodes {
            if let Some(m) = d.get("message").and_then(|m| m.as_str()) {
                truth.insert(m.trim().to_string());
                for cs in callsigns_in(m) {
                    corpus_calls.insert(cs);
                }
            }
        }
        per_wav_truth.insert(sha, truth);
        baselines += 1;
    }
    eprintln!(
        "  loaded {baselines} baselines; corpus has {} unique callsigns",
        corpus_calls.len()
    );

    // Decode hard-200 with pancetta production.
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;
    eprintln!("Decoding {} WAVs...", entries.len());

    let cfg = Ft8Config::default();
    let mut total_decodes = 0usize;
    let mut total_filtered_in = 0usize; // pass the filter
    let mut total_filtered_out = 0usize; // dropped by filter

    let mut total_truth_recovered = 0usize;
    let mut total_truth_recovered_post_filter = 0usize;
    let mut total_novel = 0usize;
    let mut total_novel_post_filter = 0usize;

    for (i, entry) in entries.iter().enumerate() {
        if i % 40 == 0 {
            eprintln!(
                "  {}/{} (post-filter: rec={} novel={})",
                i,
                entries.len(),
                total_truth_recovered_post_filter,
                total_novel_post_filter
            );
        }
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let sha = entry
            .get("wav_sha256")
            .and_then(|s| s.as_str())
            .context("entry missing wav_sha256")?
            .to_string();
        let truth = per_wav_truth.get(&sha).cloned().unwrap_or_default();
        let samples = match load_wav(&PathBuf::from(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decodes = decoder.decode_window(&samples).unwrap_or_default();
        total_decodes += decodes.len();

        for d in &decodes {
            let text = d.text.trim();
            let is_truth = truth
                .iter()
                .any(|t| t == text || t.contains(text) || text.contains(t));
            let calls = callsigns_in(text);
            let passes_filter = if calls.is_empty() {
                false // no callsign at all → drop (likely garbage)
            } else {
                calls.iter().any(|c| corpus_calls.contains(c))
            };
            if passes_filter {
                total_filtered_in += 1;
                if is_truth {
                    total_truth_recovered_post_filter += 1;
                } else {
                    total_novel_post_filter += 1;
                }
            } else {
                total_filtered_out += 1;
            }
            if is_truth {
                total_truth_recovered += 1;
            } else {
                total_novel += 1;
            }
        }
    }

    println!();
    println!("FP filter (callsign continuity) on hard-200");
    println!("============================================");
    println!("Total pancetta decodes:       {total_decodes}");
    println!(
        "  filter PASS (kept):         {total_filtered_in}  ({:.1}%)",
        100.0 * total_filtered_in as f64 / total_decodes.max(1) as f64
    );
    println!(
        "  filter FAIL (dropped):      {total_filtered_out}  ({:.1}%)",
        100.0 * total_filtered_out as f64 / total_decodes.max(1) as f64
    );
    println!();
    println!("Breakdown by truth membership:");
    println!(
        "  truth-matching pre-filter:  {total_truth_recovered}  post-filter: {total_truth_recovered_post_filter}  ({:+})",
        total_truth_recovered_post_filter as i64 - total_truth_recovered as i64
    );
    println!(
        "  novel pre-filter:           {total_novel}  post-filter: {total_novel_post_filter}  ({:+})",
        total_novel_post_filter as i64 - total_novel as i64
    );
    println!();
    let real_dropped = total_truth_recovered - total_truth_recovered_post_filter;
    let novel_dropped = total_novel - total_novel_post_filter;
    let fp_kill_rate = 100.0 * novel_dropped as f64 / total_novel.max(1) as f64;
    let recall_cost = 100.0 * real_dropped as f64 / total_truth_recovered.max(1) as f64;
    println!("FP reduction: -{novel_dropped} novels ({fp_kill_rate:.1}% of pre-filter novels)");
    println!(
        "Recall cost:  -{real_dropped} real decodes ({recall_cost:.2}% of pre-filter real decodes)"
    );
    println!();
    println!("Interpretation:");
    println!("  Filter is eval-harness-only (requires corpus baselines).");
    println!("  Real-time production needs a different signal source");
    println!("  (operator log + rolling window + cqdx.io API).");
    Ok(())
}
