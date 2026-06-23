//! hb-057 median-DT-per-callsign history — feasibility / kill-switch diagnostic.
//!
//! Spawned 2026-05-25 from mr-002. Was deferred because multi-pass was off
//! (hb-031). Multi-pass is now BACK: hb-079 coherent multipass GRADUATED,
//! hb-080 N=3 GRADUATED, hb-086 V1 joint-pair-retry GRADUATED. Re-evaluating
//! whether per-callsign DT history fed into sync as a prior is worth the
//! plumbing.
//!
//! **Mechanism premise.** JTDX (Feb-2022 commit "use median filter in
//! average DT calculation") tracks a rolling median DT per recently-heard
//! callsign. Because FT8 stations transmit with a stable per-machine
//! DT offset (clock drift + station processing latency are slowly-varying,
//! while fading-induced sync errors are zero-mean noise), the median-of-N
//! is a robust estimator of "where this callsign actually transmits". A
//! narrow time-window prior around the median can rescue weak decodes that
//! the wide ±2.0s sync search misses (noise integration over the wide
//! window suppresses true peaks).
//!
//! ## What this diagnostic measures
//!
//! On the refreshed top-20 worst hard-200 WAVs:
//!
//!   1. Decode each WAV with production config (multipass N=3 + V1 ON).
//!   2. Build the cross-WAV DT history per callsign: for every callsign
//!      that appears in truth (baseline `dt_s`) across multiple WAVs, record
//!      its DT history. Cross-WAV is the proxy for "recently heard" in
//!      this session-less eval harness.
//!   3. Categorize each callsign by DT variance (max - min):
//!        - stable:    variance < 0.1s
//!        - moderate:  0.1 - 0.3s
//!        - unstable:  > 0.3s
//!   4. For each missed truth whose callsign has DT history in stable or
//!      moderate buckets:
//!        - Compute median historical DT (excluding this missed truth's DT).
//!        - Check whether truth's DT lies within ±0.2s of that median. If
//!          yes, the truth is in the "recoverable by DT-prior" upper bound.
//!
//! Headline numbers:
//!   - % of missed truths whose callsign has DT history at all
//!   - % of those whose DT bucket is stable/moderate
//!   - Estimated maximum recall lift (truths recoverable by DT prior /
//!     total missed)
//!
//! **Kill switch:** if <10% of missed truths have callsigns with
//! stable/moderate DT history, the mechanism's target population is too
//! small. SHELVE.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb057_dt_history_potential

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const PROCEED_THRESHOLD_PCT: f64 = 10.0;
const SLOT_S: f64 = 15.0;
/// Tight DT search window around median-historical DT for the recovery
/// upper bound. Matches the spec's "narrow to ±0.2s of median".
const DT_WINDOW_S: f64 = 0.2;
/// Stable bucket: DT variance (max - min) under this threshold across
/// the callsign's history.
const STABLE_VAR_S: f64 = 0.1;
/// Moderate bucket: between stable threshold and this threshold.
const MODERATE_VAR_S: f64 = 0.3;

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

/// Extract bare-callsign tokens from an FT8 message. The "DT-history-bearing
/// callsign" for a message is the *sender* — i.e., the second token after
/// CQ, or the second token of a directed message. Strategy:
///   - "CQ <modifier?> CALL ..." -> CALL is the sender
///   - "TO FROM ..." (directed) -> FROM is the sender
///
/// Pancetta's DT for a decode comes from the TRANSMITTING station, so the
/// sender is what we want to bucket by.
fn sender_callsign(message: &str) -> Option<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let mut idx = 0;
    if tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
        // After CQ [modifier], next token is sender.
    } else {
        // Directed message: TO FROM ...; sender is the second token.
        idx = 1;
    }
    if idx >= tokens.len() {
        return None;
    }
    let t = tokens[idx];
    if !looks_like_callsign(t) {
        return None;
    }
    Some(t.split('/').next().unwrap_or(t).to_string())
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

/// One (wav_sha, dt_s) sighting of a callsign in the truth set.
#[derive(Clone, Debug)]
struct Sighting {
    wav_sha: String,
    dt_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Bucket {
    Stable,
    Moderate,
    Unstable,
}

fn bucket_for(variance_s: f64) -> Bucket {
    if variance_s < STABLE_VAR_S {
        Bucket::Stable
    } else if variance_s < MODERATE_VAR_S {
        Bucket::Moderate
    } else {
        Bucket::Unstable
    }
}

/// Median of a slice (returns mean of two middle values when even).
fn median(xs: &[f64]) -> f64 {
    let mut v: Vec<f64> = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

struct WavData {
    sha: String,
    truths: Vec<(f64, f64, String)>,           // (freq, dt, message)
    pancetta_decodes: Vec<(f64, f64, String)>, // (freq, dt, message)
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    // ---- Resolve top-20 hard-200 worst WAVs from main scorecard ----
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

    // ---- Pass 1: decode each WAV; collect (truths, pancetta-decodes) ----
    eprintln!(
        "Pass 1: decoding top-20 hard-200 WAVs (production config — multipass N=3 + V1 ON)..."
    );
    let cfg = Ft8Config::default();
    let mut wav_data: Vec<WavData> = Vec::new();

    for (idx, sha) in top20_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));

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

        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };
        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let pancetta_decodes_raw = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let pancetta_decodes: Vec<(f64, f64, String)> = pancetta_decodes_raw
            .iter()
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset.rem_euclid(SLOT_S),
                    d.text.trim().to_string(),
                )
            })
            .collect();

        eprintln!(
            "  [{idx:2}] {} truth={} pancetta={}",
            &sha[..8],
            truths.len(),
            pancetta_decodes.len(),
        );
        wav_data.push(WavData {
            sha: sha.clone(),
            truths,
            pancetta_decodes,
        });
    }

    // ---- Build per-callsign DT history (from TRUTH sightings, cross-WAV) ----
    //
    // We use TRUTH (jt9 baseline) sightings — not pancetta decodes — because:
    //   (a) it's the ground-truth DT each station was actually transmitting at,
    //   (b) if pancetta missed the truth, it has no DT to contribute, so
    //       building from pancetta-only would bias against the exact recovery
    //       population we care about, and
    //   (c) JTDX builds its DT history from successful decodes too, which in
    //       this offline diagnostic are best approximated by truth sightings.
    //
    // In production, the history would build from pancetta's own
    // successful prior-session decodes (a strict subset of truth-equivalent),
    // so this diagnostic is a slight upper bound on real-world coverage —
    // appropriate for a kill-switch.
    let mut sightings: HashMap<String, Vec<Sighting>> = HashMap::new();
    for wav in &wav_data {
        for (_freq, dt, msg) in &wav.truths {
            if let Some(sender) = sender_callsign(msg) {
                sightings.entry(sender).or_default().push(Sighting {
                    wav_sha: wav.sha.clone(),
                    dt_s: *dt,
                });
            }
        }
    }

    // Restrict to callsigns appearing in 2+ DISTINCT WAVs (otherwise no
    // cross-WAV history to leverage).
    let multi_wav_callsigns: HashSet<String> = sightings
        .iter()
        .filter_map(|(c, sl)| {
            let distinct_wavs: HashSet<&str> = sl.iter().map(|s| s.wav_sha.as_str()).collect();
            if distinct_wavs.len() >= 2 {
                Some(c.clone())
            } else {
                None
            }
        })
        .collect();

    // ---- Per-callsign DT stats ----
    #[derive(Clone, Debug)]
    struct CallStats {
        bucket: Bucket,
        median_dt: f64,
        variance: f64,
        sighting_count: usize,
    }
    let mut call_stats: HashMap<String, CallStats> = HashMap::new();
    let mut bucket_counts: HashMap<Bucket, usize> = HashMap::new();
    for c in &multi_wav_callsigns {
        let dts: Vec<f64> = sightings[c].iter().map(|s| s.dt_s).collect();
        let min_dt = dts.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_dt = dts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let variance = max_dt - min_dt;
        let med = median(&dts);
        let b = bucket_for(variance);
        *bucket_counts.entry(b).or_insert(0) += 1;
        call_stats.insert(
            c.clone(),
            CallStats {
                bucket: b,
                median_dt: med,
                variance,
                sighting_count: dts.len(),
            },
        );
    }

    // ---- Determine missed truths + classify each ----
    let mut total_truths = 0usize;
    let mut total_recovered = 0usize;
    let mut missed_total = 0usize;
    let mut missed_with_sender = 0usize;
    let mut missed_with_multi_wav_history = 0usize;
    let mut missed_with_stable_history = 0usize;
    let mut missed_with_moderate_history = 0usize;
    let mut missed_with_unstable_history = 0usize;
    // Of stable+moderate-history missed truths, how many have truth-DT within
    // ±DT_WINDOW_S of the LEAVE-ONE-OUT median of their callsign's other
    // sightings — i.e., would survive a ±0.2s prior gate?
    let mut recoverable_by_prior: usize = 0;
    // Also collect per-WAV breakdown for reporting.
    struct WavRow {
        sha_short: String,
        truth: usize,
        recovered: usize,
        missed: usize,
        miss_w_sender: usize,
        miss_w_history: usize,
        miss_w_stable_or_moderate: usize,
        recoverable: usize,
    }
    let mut wav_rows: Vec<WavRow> = Vec::new();

    for wav in &wav_data {
        let mut recovered = 0usize;
        let mut wav_miss_w_sender = 0usize;
        let mut wav_miss_w_history = 0usize;
        let mut wav_miss_w_stable_moderate = 0usize;
        let mut wav_recoverable = 0usize;
        let mut missed: Vec<(f64, String)> = Vec::new(); // (truth_dt, message)

        for (_freq, t_dt, tm) in &wav.truths {
            let matched = wav
                .pancetta_decodes
                .iter()
                .any(|(_, _, pm)| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed.push((*t_dt, tm.clone()));
            }
        }

        for (t_dt, tm) in &missed {
            let Some(sender) = sender_callsign(tm) else {
                continue;
            };
            wav_miss_w_sender += 1;
            missed_with_sender += 1;
            if !multi_wav_callsigns.contains(&sender) {
                continue;
            }
            wav_miss_w_history += 1;
            missed_with_multi_wav_history += 1;
            let cs = &call_stats[&sender];
            match cs.bucket {
                Bucket::Stable => missed_with_stable_history += 1,
                Bucket::Moderate => missed_with_moderate_history += 1,
                Bucket::Unstable => missed_with_unstable_history += 1,
            }
            if matches!(cs.bucket, Bucket::Stable | Bucket::Moderate) {
                wav_miss_w_stable_moderate += 1;

                // Leave-one-out median: exclude THIS sighting (same wav, same dt)
                // so we don't trivially "predict" the truth from itself.
                let dts: Vec<f64> = sightings[&sender]
                    .iter()
                    .filter(|s| !(s.wav_sha == wav.sha && (s.dt_s - *t_dt).abs() < 1e-9))
                    .map(|s| s.dt_s)
                    .collect();
                if dts.is_empty() {
                    continue;
                }
                let prior_median = median(&dts);
                if (t_dt - prior_median).abs() <= DT_WINDOW_S {
                    wav_recoverable += 1;
                    recoverable_by_prior += 1;
                }
            }
        }

        total_truths += wav.truths.len();
        total_recovered += recovered;
        missed_total += wav.truths.len() - recovered;

        wav_rows.push(WavRow {
            sha_short: wav.sha[..8].to_string(),
            truth: wav.truths.len(),
            recovered,
            missed: wav.truths.len() - recovered,
            miss_w_sender: wav_miss_w_sender,
            miss_w_history: wav_miss_w_history,
            miss_w_stable_or_moderate: wav_miss_w_stable_moderate,
            recoverable: wav_recoverable,
        });
    }

    // ---- Report ----
    println!("\n=== hb-057 DT-history potential (top-20 hard-200) ===\n");
    println!(
        "Corpus: {} WAVs decoded, {} truths total, {} recovered, {} missed",
        wav_data.len(),
        total_truths,
        total_recovered,
        missed_total,
    );

    println!(
        "\nDistinct multi-WAV callsigns (≥2 sightings across different WAVs): {}",
        multi_wav_callsigns.len(),
    );
    let n_stable = bucket_counts.get(&Bucket::Stable).copied().unwrap_or(0);
    let n_moderate = bucket_counts.get(&Bucket::Moderate).copied().unwrap_or(0);
    let n_unstable = bucket_counts.get(&Bucket::Unstable).copied().unwrap_or(0);
    let total_callsigns = (n_stable + n_moderate + n_unstable).max(1);
    println!(
        "  stable    (var <{:.1}s): {:>4} ({:>5.1}%)",
        STABLE_VAR_S,
        n_stable,
        100.0 * n_stable as f64 / total_callsigns as f64,
    );
    println!(
        "  moderate  ({:.1}-{:.1}s):   {:>4} ({:>5.1}%)",
        STABLE_VAR_S,
        MODERATE_VAR_S,
        n_moderate,
        100.0 * n_moderate as f64 / total_callsigns as f64,
    );
    println!(
        "  unstable  (var >{:.1}s): {:>4} ({:>5.1}%)",
        MODERATE_VAR_S,
        n_unstable,
        100.0 * n_unstable as f64 / total_callsigns as f64,
    );

    println!("\nPer-WAV breakdown:");
    println!(
        "  {:>9} {:>6} {:>4} {:>7} {:>9} {:>8} {:>11} {:>9}",
        "sha", "truth", "rec", "missed", "w/sender", "w/hist", "w/stab+mod", "recov-pri",
    );
    for w in &wav_rows {
        println!(
            "  {:>9} {:>6} {:>4} {:>7} {:>9} {:>8} {:>11} {:>9}",
            w.sha_short,
            w.truth,
            w.recovered,
            w.missed,
            w.miss_w_sender,
            w.miss_w_history,
            w.miss_w_stable_or_moderate,
            w.recoverable,
        );
    }

    println!("\nAggregate (denominators in parens = % of total missed truths):");
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            100.0 * n as f64 / d as f64
        }
    };
    println!(
        "  missed total                                    : {} (100.0%)",
        missed_total
    );
    println!(
        "  missed with extractable sender callsign         : {} ({:>5.1}%)",
        missed_with_sender,
        pct(missed_with_sender, missed_total),
    );
    println!(
        "  missed with multi-WAV DT history                : {} ({:>5.1}%)",
        missed_with_multi_wav_history,
        pct(missed_with_multi_wav_history, missed_total),
    );
    println!(
        "    └ stable    bucket                             : {} ({:>5.1}%)",
        missed_with_stable_history,
        pct(missed_with_stable_history, missed_total),
    );
    println!(
        "    └ moderate  bucket                             : {} ({:>5.1}%)",
        missed_with_moderate_history,
        pct(missed_with_moderate_history, missed_total),
    );
    println!(
        "    └ unstable  bucket                             : {} ({:>5.1}%)",
        missed_with_unstable_history,
        pct(missed_with_unstable_history, missed_total),
    );
    let target_pop = missed_with_stable_history + missed_with_moderate_history;
    println!(
        "  TARGET pop (stable+moderate history)            : {} ({:>5.1}%)",
        target_pop,
        pct(target_pop, missed_total),
    );
    println!(
        "  Recoverable by ±{:.1}s DT-prior gate (upper bnd) : {} ({:>5.1}%)",
        DT_WINDOW_S,
        recoverable_by_prior,
        pct(recoverable_by_prior, missed_total),
    );

    let target_pop_pct = pct(target_pop, missed_total);
    let recoverable_pct = pct(recoverable_by_prior, missed_total);
    println!(
        "\nDecision threshold: PROCEED if TARGET-pop ≥ {:.0}% of missed truths.\n  TARGET-pop = {:.1}%, max recall lift = {:.1}%",
        PROCEED_THRESHOLD_PCT, target_pop_pct, recoverable_pct,
    );

    let verdict = if target_pop_pct >= PROCEED_THRESHOLD_PCT {
        format!(
            "PROCEED — {:.1}% of missed truths are sent by callsigns with stable/moderate \
             cross-WAV DT history; ≤{:.1}% upper-bound recall lift if a ±{:.1}s DT-prior \
             narrows the sync time-window. Specify implementation and proceed to design.",
            target_pop_pct, recoverable_pct, DT_WINDOW_S,
        )
    } else {
        format!(
            "SHELVE — only {:.1}% of missed truths fall in the target population \
             (callsigns with stable/moderate cross-WAV DT history). Mechanism does not \
             have enough leverage on this corpus.",
            target_pop_pct,
        )
    };
    println!("\nVerdict: {verdict}");

    Ok(())
}
