//! Batch 33 / Diagnostic Z + AA + BB + CC — Characterize strong-signal
//! misses on hard-200.
//!
//! Batch 30 O found pancetta misses 24% of jt9 truths at SNR ≥ -10 dB.
//! This diagnostic dissects those misses across four axes in a single
//! pass (efficiency: one decode per WAV):
//!
//! * **Z (dump)**: collect every (truth, snr, freq, dt, sha) for missed
//!   strong truths.
//! * **AA (capture-effect)**: for each miss, count companion truths
//!   within ±25 Hz (capture-effect locks them).
//! * **BB (message-type)**: for each missed truth text, classify by
//!   message-type / structural category.
//! * **CC (sync-vs-payload)**: for each miss, check if pancetta emitted
//!   ANY decode within ±5 Hz / ±0.3 s of the truth (sync OK, payload
//!   wrong) vs no nearby emission (sync failure).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch33_strong_misses

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

#[derive(Debug, Clone)]
struct TruthDecode {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

fn load_baseline(ws: &Path, sha: &str) -> Vec<TruthDecode> {
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
            Some(TruthDecode {
                text: d["message"].as_str()?.to_string(),
                freq_hz: d["freq_hz"].as_f64()?,
                dt_s: d["dt_s"].as_f64()?,
                snr_db: d["snr_db"].as_f64().unwrap_or(0.0),
            })
        })
        .collect()
}

/// Categorize a message text for BB.
fn classify_message(text: &str) -> &'static str {
    let upper = text.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    if tokens.is_empty() {
        return "empty";
    }
    if tokens[0] == "CQ" {
        if tokens.len() >= 2 && tokens[1] == "DX" {
            return "cq_dx";
        }
        // CQ <region> <callsign> (e.g., CQ EU)
        if tokens.len() >= 2
            && tokens[1].len() <= 3
            && tokens[1].chars().all(|c| c.is_ascii_alphabetic())
        {
            return "cq_directional";
        }
        return "cq_plain";
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
        && last.chars().nth(1).is_some_and(|c| c == '-' || c == '+')
    {
        return "report_with_r";
    }
    // 4-char grid pattern at end
    if last.len() == 4
        && last.chars().nth(0).is_some_and(|c| c.is_ascii_alphabetic())
        && last.chars().nth(1).is_some_and(|c| c.is_ascii_alphabetic())
        && last.chars().nth(2).is_some_and(|c| c.is_ascii_digit())
        && last.chars().nth(3).is_some_and(|c| c.is_ascii_digit())
    {
        return "grid_exchange";
    }
    "other"
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH33_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let strong_threshold_db: f64 = std::env::var("BATCH33_STRONG_DB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-10.0);
    let capture_freq_window_hz: f64 = std::env::var("BATCH33_CAPTURE_HZ")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25.0);

    println!("## Batch 33 — strong-signal miss characterization");
    println!(
        "  hard-200 WAVs: {}  strong-threshold: SNR ≥ {} dB  capture-window: ±{} Hz",
        top_n, strong_threshold_db, capture_freq_window_hz
    );

    let cfg = Ft8Config::default();

    // Aggregate state
    let mut strong_truths_total = 0usize;
    let mut strong_truths_recovered = 0usize;
    let mut strong_misses: Vec<(String, String, f64, f64, f64)> = Vec::new(); // (sha8, text, freq, dt, snr)

    // AA
    let mut capture_locked = 0usize;
    let mut capture_free = 0usize;

    // BB
    let mut miss_type_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut total_type_counts: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new(); // (truth_total, recovered)

    // CC
    let mut nearby_emit = 0usize;
    let mut no_emit = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_baseline(&ws, sha);

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let recovered_set: HashSet<String> = decoded.iter().map(|d| d.text.clone()).collect();

        for t in &truth {
            let cat = classify_message(&t.text);
            let row = total_type_counts.entry(cat).or_insert((0, 0));
            row.0 += 1;
            if recovered_set.contains(&t.text) {
                row.1 += 1;
            }

            if t.snr_db < strong_threshold_db {
                continue;
            }
            strong_truths_total += 1;
            if recovered_set.contains(&t.text) {
                strong_truths_recovered += 1;
                continue;
            }
            // It's a strong miss.
            strong_misses.push((
                sha[..8].to_string(),
                t.text.clone(),
                t.freq_hz,
                t.dt_s,
                t.snr_db,
            ));

            // AA: capture-effect check
            let companion = truth.iter().any(|other| {
                other.text != t.text
                    && (other.freq_hz - t.freq_hz).abs() <= capture_freq_window_hz
                    && (other.dt_s - t.dt_s).abs() <= 1.0
            });
            if companion {
                capture_locked += 1;
            } else {
                capture_free += 1;
            }

            // BB: message-type bucket
            *miss_type_counts.entry(cat).or_insert(0) += 1;

            // CC: sync-vs-payload check
            let nearby = decoded.iter().any(|d| {
                (d.frequency_offset - t.freq_hz).abs() <= 5.0
                    && (d.time_offset - t.dt_s).abs() <= 0.3
            });
            if nearby {
                nearby_emit += 1;
            } else {
                no_emit += 1;
            }
        }
    }

    println!("\n### Z — strong-signal miss totals");
    let recall = strong_truths_recovered as f64 / strong_truths_total.max(1) as f64 * 100.0;
    println!(
        "  Strong truths (≥{} dB): {} total, {} recovered, {} missed  → {:.1}% recall",
        strong_threshold_db,
        strong_truths_total,
        strong_truths_recovered,
        strong_truths_total - strong_truths_recovered,
        recall
    );

    // CSV dump of first 30 (sample)
    println!("\n  Sample of strong misses (top-30 by SNR descending):");
    let mut sorted_misses = strong_misses.clone();
    sorted_misses.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "    {:<10} {:<35} {:>8} {:>6} {:>6}",
        "sha", "text", "freq", "dt", "snr"
    );
    for (sha8, text, freq, dt, snr) in sorted_misses.iter().take(30) {
        println!(
            "    {:<10} {:<35} {:>8.0} {:>6.2} {:>+6.1}",
            sha8,
            text.chars().take(35).collect::<String>(),
            freq,
            dt,
            snr
        );
    }

    println!("\n### AA — capture-effect lockdown");
    let total_misses = capture_locked + capture_free;
    let locked_pct = capture_locked as f64 / total_misses.max(1) as f64 * 100.0;
    println!(
        "  Misses with companion truth within ±{} Hz / ±1 s: {} ({:.1}%)",
        capture_freq_window_hz, capture_locked, locked_pct
    );
    println!(
        "  Misses with NO nearby companion (sync/coverage gap): {} ({:.1}%)",
        capture_free,
        100.0 - locked_pct
    );

    println!("\n### BB — message-type distribution");
    println!("  type             | truths | recovered | strong-miss");
    println!("  ---------------- | ------ | --------- | -----------");
    for (cat, (truth_total, recovered)) in &total_type_counts {
        let strong_miss = *miss_type_counts.get(cat).unwrap_or(&0);
        println!(
            "  {:<16} | {:>6} | {:>9} | {:>11}",
            cat, truth_total, recovered, strong_miss
        );
    }

    println!("\n### CC — sync-vs-payload split");
    let total_cc = nearby_emit + no_emit;
    let nearby_pct = nearby_emit as f64 / total_cc.max(1) as f64 * 100.0;
    println!(
        "  Misses with pancetta emission within ±5 Hz / ±0.3 s: {} ({:.1}%)",
        nearby_emit, nearby_pct
    );
    println!(
        "  Misses with NO nearby pancetta emission: {} ({:.1}%)",
        no_emit,
        100.0 - nearby_pct
    );
    println!("    → 'nearby emission' means sync probably worked but the wrong");
    println!("       text was emitted (parse / message-type mismatch).");
    println!("    → 'no emission' means sync didn't surface this position at all.");

    Ok(())
}
