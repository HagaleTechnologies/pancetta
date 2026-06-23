//! Batch 28 / Diagnostic C — Corpus analysis: per-callsign stability,
//! prefix distribution, weak-SNR tier construction.
//!
//! Three hypothesis touches on a single jt9-baseline pass:
//!
//! * **hb-156** (lid-of-band weak-signal tier): filter hard-200 + wild-100
//!   entries by jt9-reported `mean_decoded_snr_db ≤ -19 dB` (loose
//!   threshold to maximize population near jt9's noisy SNR floor).
//!   Ship `research/corpus/curated/ft8/lid_of_band_50.manifest.json`.
//!
//! * **hb-174** (within-session DT/freq drift per callsign): for each
//!   callsign appearing in hard-200 jt9 baselines ≥ 3 times across
//!   distinct WAVs, measure (dt_s, freq_hz) spread. If most callsigns
//!   have tight spread (< 0.2 s, < 5 Hz), per-callsign tracking has
//!   surface area; if spread is wide (signal drifts a lot), it doesn't.
//!
//! * **hb-130** (operational-value-weighted recall — corpus-side
//!   feasibility): tabulate jt9 truths by callsign-prefix continent
//!   (NA / EU / Asia / SA / Africa / Oceania / other). If 90%+ of
//!   truths are NA+EU, opv_recall on hard-200 trivially collapses to
//!   plain recall → SHELVE the metric on this corpus.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch28_corpus_analysis

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
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

/// Extract callsign1 (the FIRST callsign) from a jt9 message text.
/// FT8 messages: "CQ K1ABC FN42" → K1ABC; "K1ABC W9XYZ -10" → K1ABC;
/// "CQ DX K1ABC FN42" → K1ABC (skip "DX"). Strips suffixes like /P, /1.
fn extract_callsign1(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut idx = 0;
    if tokens.is_empty() {
        return None;
    }
    if tokens[0] == "CQ" {
        idx = 1;
        // Skip "DX" or 3-char direction tokens after CQ
        if idx < tokens.len() && (tokens[idx] == "DX" || tokens[idx].len() == 2) {
            // Only skip "DX" specifically; 2-char tokens are also a region hint
            if tokens[idx] == "DX" {
                idx += 1;
            }
        }
    }
    if idx >= tokens.len() {
        return None;
    }
    let raw = tokens[idx];
    let base = raw.split('/').next().unwrap_or(raw);
    // Sanity: at least 3 chars, contains at least one digit
    if base.len() >= 3 && base.chars().any(|c| c.is_ascii_digit()) {
        Some(base.to_string())
    } else {
        None
    }
}

/// Classify a callsign prefix to continent. Crude proxy using the
/// first letter + digit pattern. Heuristic based on ITU/CEPT prefix
/// allocations.
fn continent_of(callsign: &str) -> &'static str {
    let upper: String = callsign.chars().filter(|c| !c.is_whitespace()).collect();
    if upper.is_empty() {
        return "other";
    }
    let chars: Vec<char> = upper.chars().collect();

    // Strip leading digits (e.g. "9A1AB" → strip "9")
    let mut start = 0;
    while start < chars.len() && chars[start].is_ascii_digit() {
        start += 1;
    }
    if start >= chars.len() {
        return "other";
    }
    let p0 = chars[start];

    // First two non-digit chars form the prefix root
    let p1 = chars.get(start + 1).copied().unwrap_or(' ');

    match p0 {
        // North America
        'K' | 'N' | 'W' => "NA",
        'A' => {
            // AA-AL are US; AM-AO Spain; AP-AS Pakistan; AT-AW India; AX Australia; AY-AZ Argentina
            if matches!(p1, 'A'..='L') {
                "NA"
            } else if matches!(p1, 'M'..='O') {
                "EU"
            } else if matches!(p1, 'X') {
                "OC"
            } else if matches!(p1, 'Y' | 'Z') {
                "SA"
            } else {
                "AS"
            }
        }
        'V' => {
            // VA-VG, VO, VY = Canada (NA); VE = Canada; VK = Australia (OC); VR Hong Kong; VU India; VR Hong Kong
            if matches!(p1, 'A'..='G' | 'O' | 'Y') {
                "NA"
            } else if matches!(p1, 'K') {
                "OC"
            } else if matches!(p1, 'U' | 'R') {
                "AS"
            } else {
                "other"
            }
        }
        // Mexico (XE-XI), Latin America heavy
        'X' => "NA", // XE = Mexico, dominant in this letter-block on FT8
        // EU
        'G' | 'M' | 'F' | 'I' | 'O' | 'D' | 'P' | 'S' | 'U' | 'E' | 'H' | 'L' | 'R' | 'T' => "EU",
        // SA
        'C' => {
            // CE Chile, CO Cuba, CP Bolivia, CT/CN/CU Portugal etc
            if matches!(p1, 'A' | 'E' | 'O' | 'P' | 'X') {
                "SA"
            } else {
                "EU"
            }
        }
        // Africa: most ZS, ZD, 5Z, 6W, etc — caught by other prefixes
        'Z' => {
            if matches!(p1, 'S' | 'B' | 'D') {
                "AF"
            } else {
                "OC"
            }
        }
        'J' => "AS", // JA Japan, JG/JH/JI/etc
        'B' => "AS", // China BG/BH/BY
        'Y' => {
            if matches!(
                p1,
                'B' | 'C' | 'D' | 'E' | 'F' | 'G' | 'H' | 'I' | 'J' | 'K'
            ) {
                "AS" // YB-YH Indonesia
            } else {
                "EU"
            }
        }
        _ => "other",
    }
}

#[derive(Default, Debug)]
struct CallsignStats {
    occurrences: Vec<(f64, f64, f64)>, // (freq_hz, dt_s, snr_db)
}

impl CallsignStats {
    fn freq_spread(&self) -> f64 {
        if self.occurrences.len() < 2 {
            return 0.0;
        }
        let fs: Vec<f64> = self.occurrences.iter().map(|t| t.0).collect();
        let max = fs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = fs.iter().cloned().fold(f64::INFINITY, f64::min);
        max - min
    }
    fn dt_spread(&self) -> f64 {
        if self.occurrences.len() < 2 {
            return 0.0;
        }
        let ts: Vec<f64> = self.occurrences.iter().map(|t| t.1).collect();
        let max = ts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = ts.iter().cloned().fold(f64::INFINITY, f64::min);
        max - min
    }
}

#[derive(Serialize)]
struct LidOfBandEntry {
    wav_path: String,
    wav_sha256: String,
    mean_decoded_snr_db: f64,
    source_tier: &'static str,
}

#[derive(Serialize)]
struct LidOfBandManifest {
    schema_version: u32,
    label: &'static str,
    generated_at: String,
    snr_threshold_db: f64,
    entries: Vec<LidOfBandEntry>,
}

fn main() -> Result<()> {
    let ws = workspace_root()?;

    let mut all_callsign_stats: HashMap<String, CallsignStats> = HashMap::new();
    let mut continent_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut total_truths = 0usize;

    let mut lid_entries: Vec<LidOfBandEntry> = Vec::new();
    let snr_threshold: f64 = -19.0;

    // Walk both hard-200 and wild-100 — wider pool for hb-156 filter.
    for (manifest_name, tier_label) in [("hard_200", "curated-hard-200"), ("wild_100", "wild-100")]
    {
        let path = ws.join(format!(
            "research/corpus/curated/ft8/{}.manifest.json",
            manifest_name
        ));
        let Ok(txt) = std::fs::read_to_string(&path) else {
            continue;
        };
        let m: Value = serde_json::from_str(&txt)?;
        let Some(entries) = m["entries"].as_array() else {
            continue;
        };
        for e in entries {
            let wav_path = e["wav_path"].as_str().unwrap_or("").to_string();
            let sha = e["wav_sha256"].as_str().unwrap_or("").to_string();
            let entry_snr = e["score_breakdown"]["mean_decoded_snr_db"].as_f64();

            // hb-156: filter by SNR threshold
            if let Some(snr) = entry_snr {
                if snr <= snr_threshold {
                    lid_entries.push(LidOfBandEntry {
                        wav_path: wav_path.clone(),
                        wav_sha256: sha.clone(),
                        mean_decoded_snr_db: snr,
                        source_tier: tier_label,
                    });
                }
            }

            // hb-174 + hb-130: only hard_200 for the per-callsign analysis
            if tier_label != "curated-hard-200" {
                continue;
            }
            let Some(b) = load_baseline(&ws, &sha) else {
                continue;
            };
            let Some(decodes) = b["decodes"].as_array() else {
                continue;
            };
            for d in decodes {
                let Some(text) = d["message"].as_str() else {
                    continue;
                };
                let Some(freq) = d["freq_hz"].as_f64() else {
                    continue;
                };
                let Some(dt) = d["dt_s"].as_f64() else {
                    continue;
                };
                let snr_jt9 = d["snr_db"].as_f64().unwrap_or(0.0);
                total_truths += 1;

                let Some(call) = extract_callsign1(text) else {
                    continue;
                };
                let cont = continent_of(&call);
                *continent_counts.entry(cont).or_insert(0) += 1;

                all_callsign_stats
                    .entry(call)
                    .or_default()
                    .occurrences
                    .push((freq, dt, snr_jt9));
            }
        }
    }

    // -- hb-156 manifest ship --
    println!("## Batch 28 / Diagnostic C");
    println!(
        "\n### hb-156 — lid-of-band weak-signal tier (SNR ≤ {} dB)",
        snr_threshold
    );
    println!("  Filter sources: curated-hard-200 + wild-100");
    println!("  Entries collected: {}", lid_entries.len());
    if lid_entries.is_empty() {
        println!("  Verdict: NO-SHIP — no entries below threshold in source manifests; relax threshold or expand source pool");
    } else {
        let out_path = ws.join("research/corpus/curated/ft8/lid_of_band.manifest.json");
        let manifest = LidOfBandManifest {
            schema_version: 1,
            label: "lid_of_band",
            generated_at: chrono::Utc::now().to_rfc3339(),
            snr_threshold_db: snr_threshold,
            entries: lid_entries,
        };
        let json = serde_json::to_vec_pretty(&manifest)?;
        std::fs::write(&out_path, json)?;
        println!("  Written: {}", out_path.display());
        println!(
            "  Verdict: SHIPPED — lid_of_band.manifest.json available as new eval-tier source"
        );
    }

    // -- hb-174 callsign stability --
    println!(
        "\n### hb-174 — per-callsign DT/freq stability (callsigns with ≥3 occurrences on hard-200)"
    );
    let mut stats_summary: Vec<(String, usize, f64, f64)> = Vec::new();
    for (call, s) in &all_callsign_stats {
        if s.occurrences.len() >= 3 {
            stats_summary.push((
                call.clone(),
                s.occurrences.len(),
                s.freq_spread(),
                s.dt_spread(),
            ));
        }
    }
    stats_summary.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "  Eligible callsigns (≥3 occurrences): {}",
        stats_summary.len()
    );
    if !stats_summary.is_empty() {
        let tight_freq = stats_summary.iter().filter(|s| s.2 < 5.0).count();
        let tight_dt = stats_summary.iter().filter(|s| s.3 < 0.2).count();
        let tight_both = stats_summary
            .iter()
            .filter(|s| s.2 < 5.0 && s.3 < 0.2)
            .count();
        let median_freq_spread = {
            let mut v: Vec<f64> = stats_summary.iter().map(|s| s.2).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[v.len() / 2]
        };
        let median_dt_spread = {
            let mut v: Vec<f64> = stats_summary.iter().map(|s| s.3).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[v.len() / 2]
        };
        println!(
            "  Median freq_spread: {:.1} Hz   Median dt_spread: {:.3} s",
            median_freq_spread, median_dt_spread
        );
        println!(
            "  Tight (freq<5 Hz):  {} / {} ({:.0}%)",
            tight_freq,
            stats_summary.len(),
            tight_freq as f64 / stats_summary.len() as f64 * 100.0
        );
        println!(
            "  Tight (dt<0.2 s):   {} / {} ({:.0}%)",
            tight_dt,
            stats_summary.len(),
            tight_dt as f64 / stats_summary.len() as f64 * 100.0
        );
        println!(
            "  Tight (both):       {} / {} ({:.0}%)",
            tight_both,
            stats_summary.len(),
            tight_both as f64 / stats_summary.len() as f64 * 100.0
        );
        // Top-10 widest spreads (most drifty callsigns)
        let mut by_freq_spread = stats_summary.clone();
        by_freq_spread.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        println!("  Top-5 widest freq spreads:");
        for (c, n, f, d) in by_freq_spread.iter().take(5) {
            println!(
                "    {:>10}  n={:>3}  Δfreq={:>6.1} Hz  Δdt={:>6.3} s",
                c, n, f, d
            );
        }
        if tight_both as f64 / stats_summary.len() as f64 >= 0.7 {
            println!("  Verdict: PROCEED — ≥70% of repeating callsigns have tight (freq, dt); per-callsign tracking has structural footing");
        } else if tight_both as f64 / stats_summary.len() as f64 >= 0.4 {
            println!("  Verdict: WEAK PROCEED — moderate stability; per-callsign tracking would need outlier handling");
        } else {
            println!("  Verdict: SHELVE — callsigns drift too much across hard-200 for direct (freq, dt) tracking");
        }
    }

    // -- hb-130 prefix distribution --
    println!("\n### hb-130 — callsign-prefix continent distribution on hard-200 truths");
    println!("  Total truths: {}", total_truths);
    let mut entries: Vec<_> = continent_counts.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1));
    for (cont, n) in &entries {
        println!(
            "    {:<8} {:>6}  ({:>5.1}%)",
            cont,
            n,
            **n as f64 / total_truths.max(1) as f64 * 100.0
        );
    }
    let na_eu = continent_counts.get("NA").unwrap_or(&0) + continent_counts.get("EU").unwrap_or(&0);
    let na_eu_pct = na_eu as f64 / total_truths.max(1) as f64;
    println!("  NA+EU fraction: {:.1}%", na_eu_pct * 100.0);
    if na_eu_pct >= 0.95 {
        println!("  Verdict: SHELVE — corpus is ≥95% NA+EU; operational-value weighting trivially collapses to plain recall on hard-200");
    } else if na_eu_pct >= 0.85 {
        println!("  Verdict: WEAK SHELVE — 85-95% NA+EU; meaningful weighting unlikely to discriminate hypothesis ranking");
    } else {
        println!("  Verdict: PROCEED — sufficient prefix diversity for opv_recall to differ from plain recall on hard-200");
    }

    Ok(())
}
