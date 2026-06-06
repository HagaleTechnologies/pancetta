//! Batch 35 / Items D + E + F + H — combined coverage audits.
//!
//! Single decode pass on hard-200; bucket truths + emissions:
//!
//!   D  corpus baseline:   recovered / novel / vs_wsjtx_pct snapshot
//!   E  per-prefix recall: US / EU / DX
//!   F  per-message-type recall: compare to Batch 33's pre-fix numbers
//!   H  novel emissions by message type: where are the 1582 novels?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch35_coverage_combined

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
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

fn load_truth(ws: &Path, sha: &str) -> Vec<(String, f64, f64, f64)> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    let Some(arr) = v["decodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            Some((
                d["message"].as_str()?.to_string(),
                d["freq_hz"].as_f64()?,
                d["dt_s"].as_f64()?,
                d["snr_db"].as_f64().unwrap_or(0.0),
            ))
        })
        .collect()
}

fn first_callsign(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut idx = 0;
    if tokens.first().copied() == Some("CQ") {
        idx = 1;
        if tokens.get(1).copied() == Some("DX") {
            idx = 2;
        }
    }
    let raw = *tokens.get(idx)?;
    let base = raw.split('/').next().unwrap_or(raw);
    if base.len() >= 3 && base.chars().any(|c| c.is_ascii_digit()) {
        Some(base.to_string())
    } else {
        None
    }
}

fn prefix_region(callsign: &str) -> &'static str {
    let upper: String = callsign.to_uppercase();
    let chars: Vec<char> = upper.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    let p0 = chars.get(i).copied().unwrap_or('?');
    let p1 = chars.get(i + 1).copied().unwrap_or(' ');
    match p0 {
        'K' | 'N' | 'W' => "US",
        'A' if matches!(p1, 'A'..='L') => "US",
        'X' => "MEX/CA", // XE Mexico, dominant in this block
        'V' if matches!(p1, 'A'..='G' | 'O' | 'Y' | 'E') => "CAN",
        // EU
        'G' | 'M' | 'F' | 'I' | 'O' | 'D' | 'P' | 'S' | 'U' | 'E' | 'H' | 'L' | 'R' | 'T' => "EU",
        'J' => "JA", // Japan
        'B' => "CN", // China
        // SA
        'C' if matches!(p1, 'E' | 'O' | 'P' | 'X') => "SA",
        'Y' if matches!(p1, 'V' | 'S') => "SA",
        'L' if matches!(p1, 'U') => "SA",
        // AF
        'Z' if matches!(p1, 'S' | 'B' | 'D') => "AF",
        // OC
        'V' if matches!(p1, 'K') => "OC",
        'Z' if matches!(p1, 'L') => "OC",
        _ => "DX-other",
    }
}

fn classify_msg(text: &str) -> &'static str {
    let upper = text.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    if tokens.is_empty() {
        return "empty";
    }
    if tokens[0] == "CQ" {
        return "cq";
    }
    let last = tokens.last().copied().unwrap_or("");
    if last == "73" {
        return "73";
    }
    if last == "RR73" {
        return "rr73";
    }
    if last == "RRR" {
        return "rrr";
    }
    if last.starts_with('-') || last.starts_with('+') {
        return "report";
    }
    if last.starts_with('R')
        && last.len() >= 3
        && last[1..].starts_with(|c: char| c == '-' || c == '+')
    {
        return "report_r";
    }
    if last.len() == 4 {
        let chars: Vec<char> = last.chars().collect();
        if chars[0].is_ascii_alphabetic()
            && chars[1].is_ascii_alphabetic()
            && chars[2].is_ascii_digit()
            && chars[3].is_ascii_digit()
        {
            return "grid";
        }
    }
    "other"
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH35_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 35 — combined coverage audit (post hb-217)");

    let cfg = Ft8Config::default();
    let mut total_pancetta = 0usize;
    let mut total_tp = 0usize;
    let mut total_truth = 0usize;
    let mut prefix_truth: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut msg_type_truth: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    let mut novel_by_type: BTreeMap<&'static str, usize> = BTreeMap::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let truth_set: HashSet<String> = truth.iter().map(|t| t.0.clone()).collect();
        total_truth += truth.len();

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        for d in &decoded {
            total_pancetta += 1;
            if truth_set.contains(&d.text) {
                total_tp += 1;
            } else {
                let cat = classify_msg(&d.text);
                *novel_by_type.entry(cat).or_insert(0) += 1;
            }
        }

        let recovered: HashSet<String> = decoded.into_iter().map(|d| d.text).collect();

        for (text, _, _, _) in &truth {
            if let Some(c) = first_callsign(text) {
                let region = prefix_region(&c);
                let row = prefix_truth.entry(region).or_insert((0, 0));
                row.0 += 1;
                if recovered.contains(text) {
                    row.1 += 1;
                }
            }
            let cat = classify_msg(text);
            let row = msg_type_truth.entry(cat).or_insert((0, 0));
            row.0 += 1;
            if recovered.contains(text) {
                row.1 += 1;
            }
        }
    }

    println!("\n### D — corpus baseline snapshot (post hb-217)");
    println!("  Total pancetta decodes: {}", total_pancetta);
    println!(
        "  TP (recovered):         {} ({:.1}%)",
        total_tp,
        total_tp as f64 / total_pancetta.max(1) as f64 * 100.0
    );
    println!("  FP/novel:               {}", total_pancetta - total_tp);
    println!("  jt9 truth total:        {}", total_truth);
    println!(
        "  vs_jt9 recall:          {:.1}%",
        total_tp as f64 / total_truth.max(1) as f64 * 100.0
    );

    println!("\n### E — recall by prefix region");
    println!(
        "  {:<10} {:>6} {:>10} {:>6}",
        "region", "truth", "recovered", "recall"
    );
    let mut items: Vec<_> = prefix_truth.iter().collect();
    items.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (region, (t, r)) in items {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:<10} {:>6} {:>10} {:>5.1}%", region, t, r, pct);
    }

    println!("\n### F — recall by message type (verifies hb-217 didn't regress non-RR73)");
    println!(
        "  {:<12} {:>6} {:>10} {:>6}",
        "type", "truth", "recovered", "recall"
    );
    let mut mt: Vec<_> = msg_type_truth.iter().collect();
    mt.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (cat, (t, r)) in mt {
        let pct = *r as f64 / (*t).max(1) as f64 * 100.0;
        println!("  {:<12} {:>6} {:>10} {:>5.1}%", cat, t, r, pct);
    }

    println!("\n### H — novel emissions by message type");
    println!("  {:<12} {:>6}", "type", "count");
    let mut nb: Vec<_> = novel_by_type.iter().collect();
    nb.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, n) in nb {
        println!("  {:<12} {:>6}", cat, n);
    }

    Ok(())
}
