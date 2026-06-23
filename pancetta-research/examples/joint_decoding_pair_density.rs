//! joint_decoding_pair_density — hb-086 kill-switch diagnostic.
//!
//! Per `docs/superpowers/specs/2026-05-27-joint-decoding-design.md`: before
//! committing 3-5 sessions to joint multi-candidate decoding, confirm the
//! mechanism fits the corpus. For each MISSED truth on the top-20 worst
//! hard-200 WAVs, check whether there's a NEARBY recovered pancetta decode
//! (suggesting interference-pair structure). If ≥30% of misses are
//! pair-likely, proceed to V1 implementation; if <30%, shelve hb-086.
//!
//! Pair-likely definition:
//!   freq proximity: |truth.freq_hz − pancetta_recovered.freq_hz| ≤ 50 Hz
//!     (one tone-band ≈ 8 tones × 6.25 Hz)
//!   slot-relative-dt proximity: ∃ slot s s.t. |truth.dt_s − (pancetta.time
//!     mod 15)| ≤ 2.0 s when slot_idx(pancetta) = s (relaxes the
//!     unknown-slot problem — truth has dt relative to ITS slot, pancetta
//!     is in a 90s buffer so we modulo).
//!
//! Run: cargo run --release -p pancetta-research --example joint_decoding_pair_density

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
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

const FREQ_PROXIMITY_HZ: f64 = 50.0; // one tone-band
const DT_PROXIMITY_S: f64 = 2.0; // within ~one symbol's window
const SLOT_S: f64 = 15.0;
const PAIR_DENSITY_THRESHOLD: f64 = 30.0; // hb-086 kill-switch

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    // 1. Pull top-20 wav_hashes from main.json.
    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top20_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(20)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();

    // 2. Map sha → wav_path from hard-200 manifest.
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    // Pair-likely accumulator across all 20 WAVs.
    let mut total_missed = 0usize;
    let mut pair_likely_freq_only = 0usize;
    let mut pair_likely_freq_and_dt = 0usize;
    let mut nearest_freq_distances: Vec<f64> = Vec::new();

    // Per-WAV breakdown for the report.
    struct WavStats {
        sha_short: String,
        truth_total: usize,
        recovered: usize,
        missed: usize,
        missed_pair_likely: usize,
    }
    let mut wav_stats: Vec<WavStats> = Vec::new();

    eprintln!("Decoding top-20 hard-200 WAVs (production config — hb-080 N=3)...");
    let cfg = Ft8Config::default();

    for (idx, sha) in top20_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));

        // Load truths (freq_hz, dt_s, message).
        let baseline: Value = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)?;
        let truths: Vec<(f64, f64, String)> = baseline["decodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        Some((
                            d.get("freq_hz")?.as_f64()?,
                            d.get("dt_s")?.as_f64()?,
                            d.get("message")?.as_str()?.trim().to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Decode WAV.
        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };
        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let pancetta_decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        // Pancetta decodes' (freq, time_offset, text) triples; time mod 15 = slot-dt.
        let pancetta: Vec<(f64, f64, String)> = pancetta_decodes
            .iter()
            .map(|d| (d.frequency_offset, d.time_offset, d.text.trim().to_string()))
            .collect();

        // Classify each truth as recovered (text matches any pancetta) or missed.
        let mut recovered = 0usize;
        let mut missed_locally = Vec::new();
        for (tf, td, tm) in &truths {
            // Eval-style substring match (matches eval's matcher).
            let matched = pancetta
                .iter()
                .any(|(_, _, pm)| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed_locally.push((*tf, *td, tm.clone()));
            }
        }

        // For each missed truth, find nearest pancetta decode by freq.
        let mut wav_pair_likely = 0usize;
        for (tf, td, _tm) in &missed_locally {
            let mut min_freq_dist = f64::INFINITY;
            let mut min_freq_and_dt_dist = f64::INFINITY;
            for (pf, pt, _) in &pancetta {
                let df = (pf - tf).abs();
                if df < min_freq_dist {
                    min_freq_dist = df;
                }
                // dt-proximity: compute slot-relative pancetta dt = pt mod SLOT_S,
                // compare against truth.dt_s.
                let slot_dt = pt.rem_euclid(SLOT_S);
                let dd = (slot_dt - td).abs();
                if df <= FREQ_PROXIMITY_HZ && dd <= DT_PROXIMITY_S && df < min_freq_and_dt_dist {
                    min_freq_and_dt_dist = df;
                }
            }
            nearest_freq_distances.push(min_freq_dist);
            if min_freq_dist <= FREQ_PROXIMITY_HZ {
                pair_likely_freq_only += 1;
                wav_pair_likely += 1;
            }
            if min_freq_and_dt_dist.is_finite() {
                pair_likely_freq_and_dt += 1;
            }
            total_missed += 1;
        }

        wav_stats.push(WavStats {
            sha_short: sha[..8].to_string(),
            truth_total: truths.len(),
            recovered,
            missed: missed_locally.len(),
            missed_pair_likely: wav_pair_likely,
        });
        eprintln!(
            "  [{idx:2}] {} truth={} rec={} missed={} pair-likely(freq)={}",
            &sha[..8],
            truths.len(),
            recovered,
            missed_locally.len(),
            wav_pair_likely,
        );
    }

    // Report.
    println!("\n=== hb-086 pair-density diagnostic (top-20 hard-200) ===\n");
    println!("Per-WAV breakdown:");
    println!(
        "  {:>9} {:>6} {:>6} {:>7} {:>14}",
        "sha", "truth", "rec", "missed", "pair-likely"
    );
    for w in &wav_stats {
        let pct = if w.missed > 0 {
            100.0 * w.missed_pair_likely as f64 / w.missed as f64
        } else {
            0.0
        };
        println!(
            "  {:>9} {:>6} {:>6} {:>7} {:>9} ({:.0}%)",
            w.sha_short, w.truth_total, w.recovered, w.missed, w.missed_pair_likely, pct
        );
    }

    println!("\nAggregate:");
    println!("  Total missed truths: {}", total_missed);
    if total_missed > 0 {
        let pct_freq = 100.0 * pair_likely_freq_only as f64 / total_missed as f64;
        let pct_fd = 100.0 * pair_likely_freq_and_dt as f64 / total_missed as f64;
        println!(
            "  Pair-likely (freq ≤{:.0} Hz):        {} ({:.1}%)",
            FREQ_PROXIMITY_HZ, pair_likely_freq_only, pct_freq
        );
        println!(
            "  Pair-likely (freq AND dt ≤{:.1}s):   {} ({:.1}%)",
            DT_PROXIMITY_S, pair_likely_freq_and_dt, pct_fd
        );

        // Distance distribution for the missed truths' nearest pancetta decode.
        let mut buckets: [(f64, &str, usize); 6] = [
            (12.5, "≤12.5 Hz (overlap)", 0),
            (25.0, "12.5-25 Hz (adjacent)", 0),
            (50.0, "25-50 Hz (same band)", 0),
            (100.0, "50-100 Hz (near)", 0),
            (200.0, "100-200 Hz", 0),
            (f64::INFINITY, ">200 Hz (isolated)", 0),
        ];
        nearest_freq_distances
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for d in &nearest_freq_distances {
            for b in buckets.iter_mut() {
                if *d <= b.0 {
                    b.2 += 1;
                    break;
                }
            }
        }
        println!("\nNearest-pancetta-decode freq-distance distribution:");
        let total = nearest_freq_distances.len();
        for b in &buckets {
            let pct = if total > 0 {
                100.0 * b.2 as f64 / total as f64
            } else {
                0.0
            };
            println!("  {:>22}  {:5} ({:.1}%)", b.1, b.2, pct);
        }

        println!(
            "\nDecision threshold (per hb-086 spec): {:.0}%",
            PAIR_DENSITY_THRESHOLD
        );
        let verdict = if pct_freq >= PAIR_DENSITY_THRESHOLD {
            "PROCEED to hb-086 V1 implementation"
        } else {
            "SHELVE hb-086 — mechanism doesn't fit the residual wall"
        };
        println!("Verdict: {verdict}");
    }
    Ok(())
}
