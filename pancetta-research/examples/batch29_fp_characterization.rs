//! Batch 29 / Diagnostic F — FP-pattern characterization on noise corpus.
//!
//! Batch 28 D found 71% of pure-noise windows emit ≥1 CRC-passing decode.
//! This diagnostic CHARACTERIZES those FPs: what are they?
//!
//! Hypotheses:
//! * Are FP messages uniformly distributed across message types
//!   (CQ / report / 73 / etc.) or biased toward certain types?
//! * Do FP callsigns cluster around any prefix family, or are they
//!   uniform over the 28-bit callsign hash space?
//! * Do FPs cluster in (freq, dt) or are they uniformly scattered?
//! * What fraction of FP callsigns appear in the hard-200 truth set
//!   (i.e., would the hb-062 callsign-continuity filter reject them)?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch29_fp_characterization

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;
const SLOT_S: usize = 15;
const WINDOW_SAMPLES: usize = SAMPLE_RATE * SLOT_S;

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn gaussian_noise(rng: &mut StdRng, n: usize, sigma: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u1: f32 = rng.gen_range(f32::EPSILON..1.0);
        let u2: f32 = rng.gen_range(0.0..1.0);
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (2.0 * std::f32::consts::PI * u2).cos();
        let z1 = mag * (2.0 * std::f32::consts::PI * u2).sin();
        out.push(z0 * sigma);
        i += 1;
        if i < n {
            out.push(z1 * sigma);
            i += 1;
        }
    }
    out
}

fn classify_msg_type(text: &str) -> &'static str {
    let t = text.trim();
    if t.starts_with("CQ ") {
        "CQ"
    } else if t.ends_with(" 73") || t.ends_with(" RR73") {
        "73/RR73"
    } else if t.contains(" R-") || t.contains(" R+") {
        "Rreport"
    } else if t.contains(" -") || t.contains(" +") {
        // Heuristic: ends in numeric SNR token
        let last = t.split_whitespace().last().unwrap_or("");
        if last.starts_with('-') || last.starts_with('+') {
            "report"
        } else {
            "grid_or_other"
        }
    } else {
        "grid_or_other"
    }
}

/// Extract callsigns (callsign1 + callsign2) from FT8 message text.
/// Returns a Vec of callsigns mentioned. Handles "CQ K1ABC FN42" → K1ABC.
fn extract_callsigns(text: &str) -> Vec<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::new();
    let mut start = 0;
    if tokens.first().copied() == Some("CQ") {
        start = 1;
        if tokens.get(1).copied() == Some("DX") {
            start = 2;
        }
    }
    for (i, t) in tokens.iter().enumerate().skip(start) {
        // Skip likely grid / SNR / 73 tokens
        if *t == "73" || *t == "RR73" {
            continue;
        }
        if t.starts_with('-') || t.starts_with('+') {
            continue;
        }
        if t.starts_with('R') && (t.contains('-') || t.contains('+')) {
            continue;
        }
        // 4-char grid pattern (letter-letter-digit-digit)
        if t.len() == 4
            && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && t.chars().nth(1).is_some_and(|c| c.is_ascii_alphabetic())
            && t.chars().nth(2).is_some_and(|c| c.is_ascii_digit())
            && t.chars().nth(3).is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        // Callsign heuristic: 3-8 chars, has at least one digit
        let base = t.split('/').next().unwrap_or(t);
        if base.len() >= 3
            && base.len() <= 8
            && base.chars().any(|c| c.is_ascii_digit())
            && base.chars().any(|c| c.is_ascii_alphabetic())
        {
            out.push(base.to_string());
        }
        let _ = i;
    }
    out
}

fn first_letter_prefix(callsign: &str) -> char {
    let chars: Vec<char> = callsign.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    chars.get(i).copied().unwrap_or('?')
}

/// Load all truth callsigns from hard-200 baselines.
fn load_hard200_truth_callsigns(ws: &Path) -> Result<HashSet<String>> {
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut out = HashSet::new();
    for e in manifest["entries"].as_array().context("entries")? {
        let sha = e["wav_sha256"].as_str().unwrap_or("");
        let bpath = ws
            .join("research/baselines/ft8")
            .join(format!("{}.json", sha));
        let Ok(txt) = std::fs::read_to_string(&bpath) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&txt) else {
            continue;
        };
        let Some(arr) = v["decodes"].as_array() else {
            continue;
        };
        for d in arr {
            if let Some(msg) = d["message"].as_str() {
                for c in extract_callsigns(msg) {
                    out.insert(c);
                }
            }
        }
    }
    Ok(out)
}

fn main() -> Result<()> {
    let ws = workspace_root()?;

    let n_per_sigma: usize = std::env::var("BATCH29_F_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let seed: u64 = std::env::var("BATCH29_F_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    let sigmas: Vec<f32> = std::env::var("BATCH29_F_SIGMAS")
        .unwrap_or_else(|_| "0.01,0.05,0.1,0.5".to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    println!(
        "## Batch 29 / Diagnostic F — FP characterization on {} pure-noise windows (per σ in {:?})",
        n_per_sigma, sigmas
    );

    println!("\nLoading hard-200 truth callsign set...");
    let truth_callsigns = load_hard200_truth_callsigns(&ws)?;
    println!(
        "  {} unique callsigns in hard-200 truth",
        truth_callsigns.len()
    );

    let cfg = Ft8Config::default();

    let mut all_fp_texts: Vec<String> = Vec::new();
    let mut all_fp_freqs: Vec<f64> = Vec::new();
    let mut all_fp_dts: Vec<f64> = Vec::new();
    let mut msg_type_counts: HashMap<&'static str, usize> = HashMap::new();
    let mut prefix_letter_counts: HashMap<char, usize> = HashMap::new();
    let mut callsigns_in_truth = 0usize;
    let mut callsigns_total = 0usize;

    for &sigma in &sigmas {
        let mut rng = StdRng::seed_from_u64(seed.wrapping_add((sigma * 1000.0) as u64));
        for _ in 0..n_per_sigma {
            let noise = gaussian_noise(&mut rng, WINDOW_SAMPLES, sigma);
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let decoded = decoder
                .decode_window(&noise)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            for d in decoded {
                all_fp_texts.push(d.text.clone());
                all_fp_freqs.push(d.frequency_offset);
                all_fp_dts.push(d.time_offset);
                *msg_type_counts
                    .entry(classify_msg_type(&d.text))
                    .or_insert(0) += 1;
                for c in extract_callsigns(&d.text) {
                    callsigns_total += 1;
                    *prefix_letter_counts
                        .entry(first_letter_prefix(&c))
                        .or_insert(0) += 1;
                    if truth_callsigns.contains(&c) {
                        callsigns_in_truth += 1;
                    }
                }
            }
        }
    }

    println!("\nTotal FPs collected: {}", all_fp_texts.len());

    // -- Message type distribution --
    println!("\n### Message-type distribution");
    let mut tcounts: Vec<_> = msg_type_counts.iter().collect();
    tcounts.sort_by(|a, b| b.1.cmp(a.1));
    for (t, n) in &tcounts {
        println!(
            "  {:<14} {:>5}  ({:>5.1}%)",
            t,
            n,
            **n as f64 / all_fp_texts.len().max(1) as f64 * 100.0
        );
    }

    // -- Callsign-prefix-letter distribution --
    println!("\n### Callsign prefix-letter distribution (first non-digit letter)");
    let mut pcounts: Vec<_> = prefix_letter_counts.iter().collect();
    pcounts.sort_by(|a, b| b.1.cmp(a.1));
    for (c, n) in pcounts.iter().take(15) {
        println!(
            "  {:<3} {:>5}  ({:>5.1}%)",
            c,
            n,
            **n as f64 / callsigns_total.max(1) as f64 * 100.0
        );
    }

    // -- Freq/dt distribution --
    println!("\n### (freq, dt) distribution");
    if !all_fp_freqs.is_empty() {
        let mut fs = all_fp_freqs.clone();
        fs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut ts = all_fp_dts.clone();
        ts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        println!(
            "  freq_hz: min={:.0} p50={:.0} p95={:.0} max={:.0}",
            fs[0],
            fs[fs.len() / 2],
            fs[fs.len() * 95 / 100],
            fs[fs.len() - 1]
        );
        println!(
            "  dt_s:    min={:.2} p50={:.2} p95={:.2} max={:.2}",
            ts[0],
            ts[ts.len() / 2],
            ts[ts.len() * 95 / 100],
            ts[ts.len() - 1]
        );
    }

    // -- FP filter coverage --
    println!("\n### FP-filter coverage (hb-062 callsign-continuity check)");
    let pct = if callsigns_total == 0 {
        0.0
    } else {
        callsigns_in_truth as f64 / callsigns_total as f64 * 100.0
    };
    println!(
        "  FP callsigns also in hard-200 truth: {} / {} ({:.2}%)",
        callsigns_in_truth, callsigns_total, pct
    );
    if pct < 1.0 {
        println!(
            "  Verdict: FP filter would catch ~{:.0}% of noise FPs by callsign-continuity alone",
            100.0 - pct
        );
    } else {
        println!(
            "  Verdict: significant overlap — FP filter alone catches only ~{:.0}%",
            100.0 - pct
        );
    }

    Ok(())
}
