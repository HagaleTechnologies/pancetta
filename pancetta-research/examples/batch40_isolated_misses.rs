//! Batch 40 — non-capture-locked strong-miss characterization.
//!
//! Population: missed truths at strong SNR (≥-10 dB) with NO neighbor
//! truth within ±25 Hz and ±1.5s on the same WAV. These are isolated
//! strong misses — the "surprising" coverage gap unrelated to
//! capture-effect.
//!
//! AA: bucket by SNR / msg-type / audio-band / dt
//! BB: probe sync-relaxation mechanisms (min_sync_score, max_sync_candidates,
//!     V3 wide-window) — count strong-miss recovery
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch40_isolated_misses

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Truth {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

#[derive(Debug, Clone)]
struct MissEntry {
    sha: String,
    truth: Truth,
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

fn load_truths(ws: &Path, sha: &str) -> Vec<Truth> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
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

fn snr_bucket(s: f64) -> &'static str {
    if s >= 0.0 {
        ">=0"
    } else if s >= -5.0 {
        "-5..0"
    } else {
        "-10..-5"
    }
}

fn dt_bucket(dt: f64) -> &'static str {
    if dt < 0.0 {
        "<0"
    } else if dt < 0.5 {
        "0..0.5"
    } else if dt < 1.0 {
        "0.5..1"
    } else if dt < 1.5 {
        "1..1.5"
    } else if dt < 2.0 {
        "1.5..2"
    } else {
        ">=2"
    }
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;

    // === AA: identify the 357 isolated strong misses ===
    eprintln!("Pass 1: mp=2 baseline; identify isolated strong misses...");
    let mut cfg_mp2 = Ft8Config::default();
    cfg_mp2.max_decode_passes = 2;

    let mut isolated_misses: Vec<MissEntry> = Vec::new();
    let mut sha_to_wav: HashMap<String, PathBuf> = HashMap::new();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        sha_to_wav.insert(sha.to_string(), PathBuf::from(wav_path));

        let samples = load_wav(Path::new(wav_path))?;
        let truths = load_truths(&ws, sha);
        let mut decoder = Ft8Decoder::new(cfg_mp2.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let decoded_set: HashSet<String> = decoded.into_iter().map(|d| d.text).collect();

        for t in &truths {
            if t.snr_db < -10.0 {
                continue;
            }
            if decoded_set.contains(&t.text) {
                continue;
            }
            // Check no neighbor truth in ±25 Hz ±1.5s
            let has_neighbor = truths.iter().any(|n| {
                n.text != t.text
                    && (n.freq_hz - t.freq_hz).abs() <= 25.0
                    && (n.dt_s - t.dt_s).abs() <= 1.5
            });
            if !has_neighbor {
                isolated_misses.push(MissEntry {
                    sha: sha.to_string(),
                    truth: t.clone(),
                });
            }
        }
    }

    println!("## Batch 40 — non-capture-locked strong-miss audit");
    println!("Isolated strong-miss universe: {}", isolated_misses.len());

    println!("\n### AA1+2 — SNR distribution");
    let mut snr_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for m in &isolated_misses {
        *snr_b.entry(snr_bucket(m.truth.snr_db)).or_insert(0) += 1;
    }
    println!("  {:<10} {:>6}", "snr_db", "count");
    for (k, v) in &snr_b {
        println!("  {:<10} {:>6}", k, v);
    }

    println!("\n### AA3 — message-type distribution");
    let mut t_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for m in &isolated_misses {
        *t_b.entry(classify_msg(&m.truth.text)).or_insert(0) += 1;
    }
    let mut tv: Vec<_> = t_b.iter().collect();
    tv.sort_by(|a, b| b.1.cmp(a.1));
    println!("  {:<12} {:>6}", "type", "count");
    for (k, v) in tv {
        println!("  {:<12} {:>6}", k, v);
    }

    println!("\n### AA4 — audio-band distribution");
    let mut f_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for m in &isolated_misses {
        *f_b.entry(freq_bucket(m.truth.freq_hz)).or_insert(0) += 1;
    }
    println!("  {:<8} {:>6}", "freq_hz", "count");
    for (k, v) in &f_b {
        println!("  {:<8} {:>6}", k, v);
    }

    println!("\n### AA5 — dt distribution");
    let mut d_b: BTreeMap<&'static str, usize> = BTreeMap::new();
    for m in &isolated_misses {
        *d_b.entry(dt_bucket(m.truth.dt_s)).or_insert(0) += 1;
    }
    println!("  {:<8} {:>6}", "dt_s", "count");
    for (k, v) in &d_b {
        println!("  {:<8} {:>6}", k, v);
    }

    // === BB1: lowered min_sync_score ===
    println!("\n### BB1 — min_sync_score=1.0 probe (vs default 3.0)");
    let mut cfg_lowsync = cfg_mp2.clone();
    cfg_lowsync.min_sync_score = 1.0;

    let needed: HashSet<String> = isolated_misses.iter().map(|m| m.sha.clone()).collect();
    let mut sha_to_lowsync: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_decodes_lowsync = 0usize;
    for sha in &needed {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let mut decoder = Ft8Decoder::new(cfg_lowsync.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_decodes_lowsync += decoded.len();
        sha_to_lowsync.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }
    let mut lowsync_recovered = 0usize;
    for m in &isolated_misses {
        if sha_to_lowsync
            .get(&m.sha)
            .map(|s| s.contains(&m.truth.text))
            .unwrap_or(false)
        {
            lowsync_recovered += 1;
        }
    }
    println!(
        "  total decodes: {}, isolated-miss recovered: {}/{} ({:.1}%)",
        total_decodes_lowsync,
        lowsync_recovered,
        isolated_misses.len(),
        lowsync_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );

    // === BB2: max_sync_candidates bumped ===
    println!("\n### BB2 — max_sync_candidates=600 probe (vs default 300)");
    let mut cfg_cand = cfg_mp2.clone();
    cfg_cand.max_sync_candidates = 600;
    let mut sha_to_cand: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_decodes_cand = 0usize;
    for sha in &needed {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let mut decoder = Ft8Decoder::new(cfg_cand.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_decodes_cand += decoded.len();
        sha_to_cand.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }
    let mut cand_recovered = 0usize;
    for m in &isolated_misses {
        if sha_to_cand
            .get(&m.sha)
            .map(|s| s.contains(&m.truth.text))
            .unwrap_or(false)
        {
            cand_recovered += 1;
        }
    }
    println!(
        "  total decodes: {}, isolated-miss recovered: {}/{} ({:.1}%)",
        total_decodes_cand,
        cand_recovered,
        isolated_misses.len(),
        cand_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );

    // === BB3: V3 wide-window ===
    println!("\n### BB3 — V3 wide-window (relax=-3.0 / window_bins=20)");
    let mut cfg_v3wide = cfg_mp2.clone();
    cfg_v3wide.joint_residual_sync_relax_db = -3.0;
    cfg_v3wide.joint_residual_sync_window_bins = 20;
    let mut sha_to_v3w: HashMap<String, HashSet<String>> = HashMap::new();
    for sha in &needed {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let mut decoder = Ft8Decoder::new(cfg_v3wide.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        sha_to_v3w.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }
    let mut v3w_recovered = 0usize;
    for m in &isolated_misses {
        if sha_to_v3w
            .get(&m.sha)
            .map(|s| s.contains(&m.truth.text))
            .unwrap_or(false)
        {
            v3w_recovered += 1;
        }
    }
    println!(
        "  isolated-miss recovered: {}/{} ({:.1}%)",
        v3w_recovered,
        isolated_misses.len(),
        v3w_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );

    println!("\n### Combined recovery summary");
    println!(
        "  isolated-miss universe: {} truths across {} WAVs",
        isolated_misses.len(),
        needed.len()
    );
    println!(
        "  min_sync_score=1.0:    +{} truths ({:.1}%)",
        lowsync_recovered,
        lowsync_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );
    println!(
        "  max_sync_candidates=600: +{} truths ({:.1}%)",
        cand_recovered,
        cand_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );
    println!(
        "  V3 wide (-3.0 / 20): +{} truths ({:.1}%)",
        v3w_recovered,
        v3w_recovered as f64 / isolated_misses.len().max(1) as f64 * 100.0
    );

    Ok(())
}
