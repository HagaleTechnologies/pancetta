//! Batch 37 / Items AA1-AA6 — post-mp=2 capture-locked frontier characterization.
//!
//! Runs hard-200 with mp=2 (matching Batch 36 B1 production ship).
//! Identifies missed truths that have a companion truth within ±25 Hz
//! and ±1.5s on the same WAV — the capture-effect frontier.
//! Bucketizes by:
//!   AA2: Δfreq to nearest neighbor truth (0-6 / 6-12 / 12-18 / 18-25 Hz)
//!   AA3: amplitude ratio (neighbor.snr - truth.snr in dB)
//!   AA4: missed-truth SNR bucket
//!   AA5: missed-truth message-type bucket
//!   AA6: missed-truth audio frequency bucket
//!
//! Output: JSON of the frontier truth list for BB diagnostics to reuse.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch37_frontier_aa

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
struct FrontierEntry {
    sha: String,
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
    neighbor_text: String,
    neighbor_freq_hz: f64,
    neighbor_dt_s: f64,
    neighbor_snr_db: f64,
    delta_freq_hz: f64,
    delta_snr_db: f64,
}

#[derive(Debug, Clone)]
struct Truth {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

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

fn load_truth(ws: &Path, sha: &str) -> Vec<Truth> {
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
            Some(Truth {
                text: d["message"].as_str()?.to_string(),
                freq_hz: d["freq_hz"].as_f64()?,
                dt_s: d["dt_s"].as_f64().unwrap_or(0.0),
                snr_db: d["snr_db"].as_f64().unwrap_or(0.0),
            })
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

fn dfreq_bucket(df: f64) -> &'static str {
    let a = df.abs();
    if a < 6.0 {
        "0-6"
    } else if a < 12.0 {
        "6-12"
    } else if a < 18.0 {
        "12-18"
    } else {
        "18-25"
    }
}

fn dsnr_bucket(ds: f64) -> &'static str {
    if ds < -3.0 {
        "missed_louder"
    } else if ds < 3.0 {
        "±3dB (equal)"
    } else if ds < 9.0 {
        "+3..+9 dB"
    } else if ds < 15.0 {
        "+9..+15 dB"
    } else {
        ">+15 dB"
    }
}

fn snr_bucket(s: f64) -> &'static str {
    if s >= -5.0 {
        ">=-5"
    } else if s >= -10.0 {
        "-10..-5"
    } else if s >= -15.0 {
        "-15..-10"
    } else if s >= -19.0 {
        "-19..-15"
    } else {
        "<-19"
    }
}

fn freq_bucket(f: f64) -> &'static str {
    if f < 500.0 {
        "<500"
    } else if f < 1000.0 {
        "500-1k"
    } else if f < 1500.0 {
        "1-1.5k"
    } else if f < 2000.0 {
        "1.5-2k"
    } else if f < 2500.0 {
        "2-2.5k"
    } else {
        ">2.5k"
    }
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH37_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 37 — AA1-AA6 post-mp=2 capture-locked frontier");

    // mp=2 decoder config (matches Batch 36 B1 production Fast tier)
    let mut cfg = Ft8Config::default();
    cfg.max_decode_passes = 2;

    let mut frontier: Vec<FrontierEntry> = Vec::new();
    let mut total_missed_strong = 0usize;
    let mut total_truths_strong = 0usize;
    let mut total_missed_overall = 0usize;
    let mut total_truths_overall = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truths = load_truth(&ws, sha);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let decoded_set: HashSet<String> = decoded.iter().map(|d| d.text.clone()).collect();

        for t in &truths {
            total_truths_overall += 1;
            let strong = t.snr_db >= -10.0;
            if strong {
                total_truths_strong += 1;
            }
            if !decoded_set.contains(&t.text) {
                total_missed_overall += 1;
                if strong {
                    total_missed_strong += 1;
                    // Check for capture-effect neighbor within ±25 Hz and ±1.5s
                    let mut best_neighbor: Option<&Truth> = None;
                    let mut best_df = f64::INFINITY;
                    for n in &truths {
                        if n.text == t.text {
                            continue;
                        }
                        let df = (n.freq_hz - t.freq_hz).abs();
                        let ddt = (n.dt_s - t.dt_s).abs();
                        if df <= 25.0 && ddt <= 1.5 && df < best_df {
                            best_df = df;
                            best_neighbor = Some(n);
                        }
                    }
                    if let Some(n) = best_neighbor {
                        frontier.push(FrontierEntry {
                            sha: sha.to_string(),
                            text: t.text.clone(),
                            freq_hz: t.freq_hz,
                            dt_s: t.dt_s,
                            snr_db: t.snr_db,
                            neighbor_text: n.text.clone(),
                            neighbor_freq_hz: n.freq_hz,
                            neighbor_dt_s: n.dt_s,
                            neighbor_snr_db: n.snr_db,
                            delta_freq_hz: n.freq_hz - t.freq_hz,
                            delta_snr_db: n.snr_db - t.snr_db,
                        });
                    }
                }
            }
        }
    }

    println!("\n### AA1 — frontier identification");
    println!("  truths total: {}", total_truths_overall);
    println!(
        "  pancetta mp=2 missed: {} ({:.1}%)",
        total_missed_overall,
        total_missed_overall as f64 / total_truths_overall.max(1) as f64 * 100.0
    );
    println!("  strong-SNR truths (≥-10 dB): {}", total_truths_strong);
    println!(
        "  strong-SNR missed: {} ({:.1}%)",
        total_missed_strong,
        total_missed_strong as f64 / total_truths_strong.max(1) as f64 * 100.0
    );
    println!(
        "  **capture-locked frontier (strong miss + neighbor ≤25 Hz): {}**",
        frontier.len()
    );
    println!(
        "  capture-locked fraction of strong misses: {:.1}%",
        frontier.len() as f64 / total_missed_strong.max(1) as f64 * 100.0
    );

    println!(
        "\n### AA2 — Δfreq distribution (frontier n={})",
        frontier.len()
    );
    let mut df_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        *df_b.entry(dfreq_bucket(f.delta_freq_hz)).or_insert(0) += 1;
    }
    println!("  {:<8} {:>6}", "Δfreq", "count");
    for (k, v) in &df_b {
        println!("  {:<8} {:>6}", k, v);
    }

    println!("\n### AA3 — amplitude-ratio distribution (Δsnr = neighbor - truth)");
    let mut ds_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        *ds_b.entry(dsnr_bucket(f.delta_snr_db)).or_insert(0) += 1;
    }
    println!("  {:<16} {:>6}", "Δsnr_db", "count");
    for (k, v) in &ds_b {
        println!("  {:<16} {:>6}", k, v);
    }

    println!("\n### AA4 — missed-truth SNR distribution");
    let mut s_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        *s_b.entry(snr_bucket(f.snr_db)).or_insert(0) += 1;
    }
    println!("  {:<10} {:>6}", "snr_db", "count");
    for (k, v) in &s_b {
        println!("  {:<10} {:>6}", k, v);
    }

    println!("\n### AA5 — missed-truth message-type distribution");
    let mut t_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        *t_b.entry(classify_msg(&f.text)).or_insert(0) += 1;
    }
    let mut tb: Vec<_> = t_b.iter().collect();
    tb.sort_by(|a, b| b.1.cmp(a.1));
    println!("  {:<12} {:>6}", "type", "count");
    for (k, v) in tb {
        println!("  {:<12} {:>6}", k, v);
    }

    println!("\n### AA6 — missed-truth audio-band distribution");
    let mut f_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &frontier {
        *f_b.entry(freq_bucket(f.freq_hz)).or_insert(0) += 1;
    }
    println!("  {:<8} {:>6}", "freq_hz", "count");
    for (k, v) in &f_b {
        println!("  {:<8} {:>6}", k, v);
    }

    let out_path = ws.join("research/scorecards/batch37_frontier.json");
    let json = serde_json::to_string_pretty(&frontier)?;
    std::fs::write(&out_path, json)?;
    println!("\nWrote frontier JSON: {}", out_path.display());

    Ok(())
}
