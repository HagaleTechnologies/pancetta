//! Batch 36 / Items B2 + B3 + B4 + B5 — multipass sweep + characterization.
//!
//! Decodes full hard-200 at max_decode_passes ∈ {1, 2, 3, 4}, captures
//! wall-clock for each pass, and characterizes the mp=2-only TPs
//! (those that mp=2 recovers but mp=1 misses) by SNR bucket and
//! message-type bucket.
//!
//! This is the B1-validation + curve-characterization probe behind
//! the Fast-tier max_decode_passes=2 ship.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch36_multipass_sweep

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

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

fn load_truth(ws: &Path, sha: &str) -> Vec<(String, f64)> {
    // (text, snr_db)
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
                d["snr_db"].as_f64().unwrap_or(0.0),
            ))
        })
        .collect()
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

fn snr_bucket(snr: f64) -> &'static str {
    if snr >= -5.0 {
        ">=-5"
    } else if snr >= -10.0 {
        "-10..-5"
    } else if snr >= -15.0 {
        "-15..-10"
    } else if snr >= -19.0 {
        "-19..-15"
    } else if snr >= -22.0 {
        "-22..-19"
    } else {
        "<-22"
    }
}

fn run_pass(
    entries: &[Value],
    top_n: usize,
    ws: &Path,
    mp: usize,
) -> Result<(HashSet<(String, String)>, f64)> {
    // returns (set of (sha, text) decoded, elapsed seconds)
    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = mp;
    let start = Instant::now();
    let mut out = HashSet::new();
    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in decoded {
            out.insert((sha.to_string(), d.text));
        }
    }
    Ok((out, start.elapsed().as_secs_f64()))
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
    let top_n: usize = std::env::var("BATCH36_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    // Build truth lookup: (sha, text) -> snr_db
    let mut truth_lookup: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut truth_set: HashSet<(String, String)> = HashSet::new();
    for entry in entries.iter().take(top_n) {
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        for (text, snr) in load_truth(&ws, sha) {
            truth_lookup.insert((sha.to_string(), text.clone()), snr);
            truth_set.insert((sha.to_string(), text));
        }
    }

    println!("## Batch 36 — multipass sweep + characterization");
    println!("Hard-200 truth size: {}", truth_set.len());

    let mp_values: Vec<usize> = vec![1, 2, 3, 4];
    let mut results: BTreeMap<usize, (HashSet<(String, String)>, f64)> = BTreeMap::new();
    for &mp in &mp_values {
        eprintln!("Running mp={}...", mp);
        let (decoded, elapsed) = run_pass(&entries, top_n, &ws, mp)?;
        results.insert(mp, (decoded, elapsed));
    }

    println!("\n### B2/B3 — multipass sweep summary");
    println!(
        "  {:<5} {:>10} {:>10} {:>12} {:>10} {:>12}",
        "mp", "decodes", "TPs", "precision", "elapsed", "ms/decode"
    );
    for &mp in &mp_values {
        let (set, elapsed) = &results[&mp];
        let tps = set.iter().filter(|t| truth_set.contains(*t)).count();
        let precision = tps as f64 / set.len().max(1) as f64 * 100.0;
        let ms_per = elapsed * 1000.0 / set.len().max(1) as f64;
        println!(
            "  {:<5} {:>10} {:>10} {:>11.1}% {:>9.1}s {:>11.2}",
            mp,
            set.len(),
            tps,
            precision,
            elapsed,
            ms_per
        );
    }

    // mp=2-only TPs: in mp=2 TP set, not in mp=1 TP set
    let mp1_tps: HashSet<_> = results[&1]
        .0
        .iter()
        .filter(|t| truth_set.contains(*t))
        .cloned()
        .collect();
    let mp2_tps: HashSet<_> = results[&2]
        .0
        .iter()
        .filter(|t| truth_set.contains(*t))
        .cloned()
        .collect();
    let mp2_only: Vec<_> = mp2_tps.difference(&mp1_tps).cloned().collect();
    let mp1_only: Vec<_> = mp1_tps.difference(&mp2_tps).cloned().collect();

    println!(
        "\n### B4 — mp=2-only TPs by SNR ({} TPs gained, {} TPs lost vs mp=1)",
        mp2_only.len(),
        mp1_only.len()
    );
    let mut snr_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    for t in &mp2_only {
        let snr = truth_lookup.get(t).copied().unwrap_or(0.0);
        *snr_buckets.entry(snr_bucket(snr)).or_insert(0) += 1;
    }
    println!("  {:<10} {:>6}", "snr_db", "count");
    for (bucket, n) in &snr_buckets {
        println!("  {:<10} {:>6}", bucket, n);
    }

    println!("\n### B5 — mp=2-only TPs by message-type");
    let mut type_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (_, text) in &mp2_only {
        *type_buckets.entry(classify_msg(text)).or_insert(0) += 1;
    }
    println!("  {:<12} {:>6}", "type", "count");
    let mut tb: Vec<_> = type_buckets.iter().collect();
    tb.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, n) in tb {
        println!("  {:<12} {:>6}", cat, n);
    }

    println!("\n### B3 — mp=3 / mp=4 incremental gains over mp=2");
    let mp3_tps: HashSet<_> = results[&3]
        .0
        .iter()
        .filter(|t| truth_set.contains(*t))
        .cloned()
        .collect();
    let mp4_tps: HashSet<_> = results[&4]
        .0
        .iter()
        .filter(|t| truth_set.contains(*t))
        .cloned()
        .collect();
    let mp3_only_vs_mp2 = mp3_tps.difference(&mp2_tps).count();
    let mp4_only_vs_mp3 = mp4_tps.difference(&mp3_tps).count();
    println!("  mp=3 over mp=2: +{} TPs", mp3_only_vs_mp2);
    println!("  mp=4 over mp=3: +{} TPs", mp4_only_vs_mp3);
    println!(
        "  mp=4 wall-clock vs mp=1: {:.1}x",
        results[&4].1 / results[&1].1
    );
    println!(
        "  mp=2 wall-clock vs mp=1: {:.1}x",
        results[&2].1 / results[&1].1
    );

    Ok(())
}
