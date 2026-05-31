//! corpus_survey_2026_05_30 — characterize today's K5ARH 20m capture.
//!
//! Walks `~/.pancetta/recordings/ft8_20260530_*.wav`, samples 20 evenly-spaced
//! WAVs through the session, runs both pancetta-ft8 and jt9 on each, and
//! emits a JSON summary + per-WAV detail to
//! `research/corpus/surveys/2026-05-30/`.
//!
//! Used to decide whether today's capture warrants ingestion into the
//! curated corpus (hard-200, wild-50, etc.). See journal entry
//! `research/experiments/2026-05-30-corpus-survey.md`.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example corpus_survey_2026_05_30

use anyhow::Context;
use pancetta_research::{Decode, DecoderUnderTest, Ft8Decoder, Jt9Decoder};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Serialize)]
struct PerWavResult {
    wav_filename: String,
    /// HHMMSS-style timestamp parsed from the filename.
    slot_hhmmss: String,
    file_bytes: u64,
    pancetta_decode_count: usize,
    jt9_decode_count: usize,
    /// Decodes (by message text) present in BOTH pancetta and jt9.
    intersection_count: usize,
    /// Decodes only in pancetta.
    pancetta_only_count: usize,
    /// Decodes only in jt9.
    jt9_only_count: usize,
    /// jt9 freq distribution within the slot: min/median/max Hz.
    jt9_freq_min_hz: Option<f64>,
    jt9_freq_median_hz: Option<f64>,
    jt9_freq_max_hz: Option<f64>,
    /// jt9 SNR distribution: min/median.
    jt9_snr_min_db: Option<f64>,
    jt9_snr_median_db: Option<f64>,
    /// Unique callsigns extracted across jt9 + pancetta.
    unique_callsigns: usize,
    /// Counts by message type ({"cq": N, "grid": N, "report": N, "rrr_73": N, "other": N}).
    msg_type_cq: usize,
    msg_type_report: usize,
    msg_type_rr73: usize,
    msg_type_other: usize,
    /// Errors encountered (if any), per decoder.
    pancetta_error: Option<String>,
    jt9_error: Option<String>,
    /// Wall-clock per decoder (seconds).
    pancetta_elapsed_s: f64,
    jt9_elapsed_s: f64,
}

#[derive(Serialize)]
struct SurveySummary {
    sampled_wav_count: usize,
    total_wavs_today: usize,
    total_size_bytes_today: u64,
    pancetta_decode_count_total: usize,
    jt9_decode_count_total: usize,
    intersection_total: usize,
    /// Agreement % = intersection / max(pancetta, jt9).
    agreement_pct: f64,
    pancetta_p10: f64,
    pancetta_p50: f64,
    pancetta_p90: f64,
    pancetta_mean: f64,
    jt9_p10: f64,
    jt9_p50: f64,
    jt9_p90: f64,
    jt9_mean: f64,
    jt9_max: f64,
    unique_callsigns_total: usize,
    elapsed_total_s: f64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn extract_callsigns(msg: &str) -> Vec<String> {
    // Naive: split on whitespace, keep tokens that look like callsigns
    // (3-7 chars, has at least one digit and one letter, A-Z0-9 only).
    // Strips leading "CQ" markers.
    msg.split_whitespace()
        .filter_map(|tok| {
            let t = tok.trim_start_matches('<').trim_end_matches('>');
            if t.len() < 3 || t.len() > 7 {
                return None;
            }
            let has_digit = t.chars().any(|c| c.is_ascii_digit());
            let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
            let only_alnum_slash = t.chars().all(|c| c.is_ascii_alphanumeric() || c == '/');
            if has_digit && has_alpha && only_alnum_slash && t != "CQ" {
                Some(t.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn classify_msg(msg: &str) -> &'static str {
    let u = msg.trim();
    if u.starts_with("CQ ") || u.starts_with("CQ_") {
        "cq"
    } else if u.contains(" RR73") || u.ends_with(" 73") || u.contains(" RRR") {
        "rr73"
    } else if u
        .split_whitespace()
        .last()
        .map(|t| {
            t.starts_with('R')
                && t.len() >= 3
                && t.chars().nth(1).is_some_and(|c| c == '-' || c == '+')
                || (t.starts_with('-') || t.starts_with('+'))
                    && t[1..].chars().all(|c| c.is_ascii_digit())
        })
        .unwrap_or(false)
    {
        "report"
    } else {
        "other"
    }
}

fn sample_wavs(all: &[PathBuf], n_even: usize, n_random: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if all.is_empty() {
        return out;
    }
    // Evenly spaced indices through the sorted list.
    for i in 0..n_even {
        let idx = (i * (all.len() - 1)) / (n_even - 1).max(1);
        out.push(all[idx].clone());
    }
    // n_random "random" extras — pseudo-random via nanos modulo to avoid the
    // rand crate. Reproducibility isn't critical for survey work.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(42);
    let mut x = seed.max(1);
    for _ in 0..n_random {
        // xorshift step
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let idx = (x as usize) % all.len();
        if !out.iter().any(|p| p == &all[idx]) {
            out.push(all[idx].clone());
        }
    }
    out.sort();
    out
}

fn run_one(wav: &Path, pancetta: &Ft8Decoder, jt9: &Jt9Decoder) -> PerWavResult {
    let filename = wav
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let slot_hhmmss = filename
        .strip_prefix("ft8_20260530_")
        .and_then(|s| s.strip_suffix(".wav"))
        .unwrap_or("")
        .to_string();
    let file_bytes = std::fs::metadata(wav).map(|m| m.len()).unwrap_or(0);

    let t0 = Instant::now();
    let (pancetta_decodes, pancetta_error) = match pancetta.decode_wav(wav) {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let pancetta_elapsed_s = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let (jt9_decodes, jt9_error) = match jt9.decode_wav(wav) {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    let jt9_elapsed_s = t1.elapsed().as_secs_f64();

    let panc_texts: BTreeSet<String> = pancetta_decodes.iter().map(|d| d.message.clone()).collect();
    let jt9_texts: BTreeSet<String> = jt9_decodes.iter().map(|d| d.message.clone()).collect();
    let intersection_count = panc_texts.intersection(&jt9_texts).count();
    let pancetta_only_count = panc_texts.difference(&jt9_texts).count();
    let jt9_only_count = jt9_texts.difference(&panc_texts).count();

    let jt9_freqs: Vec<f64> = {
        let mut v: Vec<f64> = jt9_decodes.iter().map(|d| d.freq_hz).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    };
    let jt9_snrs: Vec<f64> = {
        let mut v: Vec<f64> = jt9_decodes.iter().map(|d| d.snr_db).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    };

    let mut all_calls: BTreeSet<String> = BTreeSet::new();
    let mut msg_type_counts = (0usize, 0usize, 0usize, 0usize);
    let combined: Vec<&Decode> = pancetta_decodes.iter().chain(jt9_decodes.iter()).collect();
    let mut seen_msg_for_classify: BTreeSet<String> = BTreeSet::new();
    for d in &combined {
        for c in extract_callsigns(&d.message) {
            all_calls.insert(c);
        }
        if seen_msg_for_classify.insert(d.message.clone()) {
            match classify_msg(&d.message) {
                "cq" => msg_type_counts.0 += 1,
                "report" => msg_type_counts.1 += 1,
                "rr73" => msg_type_counts.2 += 1,
                _ => msg_type_counts.3 += 1,
            }
        }
    }

    PerWavResult {
        wav_filename: filename,
        slot_hhmmss,
        file_bytes,
        pancetta_decode_count: pancetta_decodes.len(),
        jt9_decode_count: jt9_decodes.len(),
        intersection_count,
        pancetta_only_count,
        jt9_only_count,
        jt9_freq_min_hz: jt9_freqs.first().copied(),
        jt9_freq_median_hz: if jt9_freqs.is_empty() {
            None
        } else {
            Some(jt9_freqs[jt9_freqs.len() / 2])
        },
        jt9_freq_max_hz: jt9_freqs.last().copied(),
        jt9_snr_min_db: jt9_snrs.first().copied(),
        jt9_snr_median_db: if jt9_snrs.is_empty() {
            None
        } else {
            Some(jt9_snrs[jt9_snrs.len() / 2])
        },
        unique_callsigns: all_calls.len(),
        msg_type_cq: msg_type_counts.0,
        msg_type_report: msg_type_counts.1,
        msg_type_rr73: msg_type_counts.2,
        msg_type_other: msg_type_counts.3,
        pancetta_error,
        jt9_error,
        pancetta_elapsed_s,
        jt9_elapsed_s,
    }
}

fn main() -> anyhow::Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf();
    let out_dir = workspace.join("research/corpus/surveys/2026-05-30");
    std::fs::create_dir_all(&out_dir)?;

    let recordings_dir = std::env::var("PANCETTA_RECORDINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/thagale".to_string());
            PathBuf::from(home).join(".pancetta/recordings")
        });

    // Glob today's WAVs.
    let mut all_today: Vec<PathBuf> = std::fs::read_dir(&recordings_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with("ft8_20260530_") && s.ends_with(".wav"))
                .unwrap_or(false)
        })
        .collect();
    all_today.sort();
    let total_count = all_today.len();
    let total_bytes: u64 = all_today
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    println!(
        "Found {} WAVs from today, {:.1} MB total",
        total_count,
        total_bytes as f64 / 1024.0 / 1024.0
    );

    // Filter out tiny / partial WAVs (< 300 KB).
    all_today.retain(|p| {
        std::fs::metadata(p)
            .map(|m| m.len() >= 300_000)
            .unwrap_or(false)
    });
    println!("After filtering <300KB partials: {}", all_today.len());

    // Sample 20 (16 evenly-spaced + 4 random).
    let sample_count: usize = std::env::var("SAMPLE_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let random_extras = (sample_count / 5).max(2);
    let n_even = sample_count - random_extras;
    let sampled = sample_wavs(&all_today, n_even, random_extras);
    println!("Sampled {} WAVs for jt9/pancetta baseline", sampled.len());

    let pancetta = Ft8Decoder::with_default_config();
    let jt9 = Jt9Decoder::default();

    let t_start = Instant::now();
    let mut per_wav: Vec<PerWavResult> = Vec::with_capacity(sampled.len());
    println!();
    println!(
        "{:<32} | {:>5} | {:>5} | {:>4} | {:>5} | {:>5}",
        "wav (HHMMSS)", "panc", "jt9", "int", "calls", "secs"
    );
    println!("{:-<70}", "");
    for (i, wav) in sampled.iter().enumerate() {
        let r = run_one(wav, &pancetta, &jt9);
        let total_s = r.pancetta_elapsed_s + r.jt9_elapsed_s;
        println!(
            "{:<32} | {:>5} | {:>5} | {:>4} | {:>5} | {:>5.1}",
            r.slot_hhmmss,
            r.pancetta_decode_count,
            r.jt9_decode_count,
            r.intersection_count,
            r.unique_callsigns,
            total_s,
        );
        per_wav.push(r);
        // Snapshot progress every 5 WAVs.
        if (i + 1) % 5 == 0 {
            let _ = std::fs::write(
                out_dir.join("per_wav.partial.json"),
                serde_json::to_string_pretty(&per_wav)?,
            );
        }
    }
    let elapsed_total_s = t_start.elapsed().as_secs_f64();

    // Aggregate.
    let mut panc_counts: Vec<f64> = per_wav
        .iter()
        .map(|r| r.pancetta_decode_count as f64)
        .collect();
    panc_counts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut jt9_counts: Vec<f64> = per_wav.iter().map(|r| r.jt9_decode_count as f64).collect();
    jt9_counts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let panc_total: usize = per_wav.iter().map(|r| r.pancetta_decode_count).sum();
    let jt9_total: usize = per_wav.iter().map(|r| r.jt9_decode_count).sum();
    let inter_total: usize = per_wav.iter().map(|r| r.intersection_count).sum();
    let agreement_pct = if panc_total.max(jt9_total) > 0 {
        100.0 * inter_total as f64 / panc_total.max(jt9_total) as f64
    } else {
        0.0
    };
    let unique_calls_total: usize = per_wav.iter().map(|r| r.unique_callsigns).sum();

    let summary = SurveySummary {
        sampled_wav_count: per_wav.len(),
        total_wavs_today: total_count,
        total_size_bytes_today: total_bytes,
        pancetta_decode_count_total: panc_total,
        jt9_decode_count_total: jt9_total,
        intersection_total: inter_total,
        agreement_pct,
        pancetta_p10: percentile(&panc_counts, 10.0),
        pancetta_p50: percentile(&panc_counts, 50.0),
        pancetta_p90: percentile(&panc_counts, 90.0),
        pancetta_mean: mean(&panc_counts),
        jt9_p10: percentile(&jt9_counts, 10.0),
        jt9_p50: percentile(&jt9_counts, 50.0),
        jt9_p90: percentile(&jt9_counts, 90.0),
        jt9_mean: mean(&jt9_counts),
        jt9_max: jt9_counts.last().copied().unwrap_or(0.0),
        unique_callsigns_total: unique_calls_total,
        elapsed_total_s,
    };

    std::fs::write(
        out_dir.join("per_wav.json"),
        serde_json::to_string_pretty(&per_wav)?,
    )?;
    std::fs::write(
        out_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    let _ = std::fs::remove_file(out_dir.join("per_wav.partial.json"));

    println!();
    println!("--- summary ---");
    println!("sampled: {}/{} WAVs", per_wav.len(), total_count);
    println!(
        "pancetta total: {} decodes (mean {:.1}/slot, p50 {:.0}, p90 {:.0})",
        panc_total, summary.pancetta_mean, summary.pancetta_p50, summary.pancetta_p90
    );
    println!(
        "jt9      total: {} decodes (mean {:.1}/slot, p50 {:.0}, p90 {:.0}, max {:.0})",
        jt9_total, summary.jt9_mean, summary.jt9_p50, summary.jt9_p90, summary.jt9_max
    );
    println!(
        "intersection: {} ({:.1}% agreement)",
        inter_total, summary.agreement_pct
    );
    println!("unique callsigns across sample: {}", unique_calls_total);
    println!("elapsed: {:.1}s", elapsed_total_s);
    println!("written to: {}", out_dir.display());
    Ok(())
}
