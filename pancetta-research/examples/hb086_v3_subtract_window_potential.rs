//! hb086_v3_subtract_window_potential — hb-086 V3 kill-switch diagnostic.
//!
//! V1 (`joint_pair_retry`, GRADUATED 2026-05-28) caught +12 hard-200 by
//! retrying *original* sync_candidates against the post-multipass residual.
//! V2 (soft cancellation, DEFINITIVELY SHELVED 2026-05-31) closed leak (b)
//! — corrupted-residual-LLR candidates — because pancetta's CRC-validated
//! decoded neighbors have delta-function tone posteriors, so soft ≡ hard.
//!
//! V3 attacks the OTHER V1 leak documented in
//! `2026-05-28-hb-086-joint-pair-retry-v1.md`:
//!
//!   leak (a): candidates whose pass-1 `sync_search` never found them.
//!     V1 only retries what `sync_search` already surfaced; sync-search
//!     misses stay missed.
//!
//! From the V2 diagnostic (refreshed corpus): **47.6% of missed truths in
//! the top-20 hard-200 WAVs have ZERO nearby pancetta decode in ±25 Hz**.
//! Those truths are V1-uncoverable: V1's mechanism requires a sync-list
//! candidate at the truth's position, and there isn't one (or the truth
//! isn't even in the sync list at all).
//!
//! V3's mechanism: after multipass + V1 saturate, run ONE MORE localized
//! sync_search on the residual at a **relaxed** threshold, but only at
//! frequency bins within ±N freq_bins of any subtracted position
//! (`tone_symbols.is_some()` on a pancetta decode). The idea: subtraction
//! localizes the noise-floor drop to bins where signal was removed; a
//! relaxed sync at those bins surfaces weak Costas patterns the production
//! threshold rejects. hb-082 already shelved a GLOBAL residual threshold
//! relaxation (no-op because candidates surface naturally above 3.0); V3
//! is structurally different because the relaxation window is bin-targeted.
//!
//! Kill-switch question: of missed truths with NO nearby decode in ±25 Hz
//! (V1's uncoverable subset), what fraction have at least one pancetta
//! decode within ±N freq_bins (the subtract-window where V3 would relax
//! sync)? We sweep N ∈ {4, 6, 8, 12} bins (≈ ±25 / 37.5 / 50 / 75 Hz).
//! PROCEED if any N reaches ≥20%; SHELVE if all are below 20%.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb086_v3_subtract_window_potential

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

const TONE_SPACING_HZ: f64 = 6.25; // FT8 6.25 Hz/bin
const V1_REACH_HZ: f64 = 25.0; // ±25 Hz = ±4 bins — V1's joint-pair direct overlap
const NEIGHBOR_DT_S: f64 = 2.0;
const SLOT_S: f64 = 15.0;
const PROCEED_THRESHOLD_PCT: f64 = 20.0;

/// Sweep widths (freq_bins) for the V3 targeting window.
const V3_WINDOW_BINS: &[usize] = &[4, 6, 8, 12];

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

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

    struct WavStats {
        sha_short: String,
        truth_total: usize,
        recovered: usize,
        missed: usize,
        missed_no_v1_neighbor: usize,
        // counts per V3 window of "no-v1-neighbor" truths that DO have a
        // subtracted-eligible decode within that window
        in_window_per_n: Vec<usize>,
    }
    let mut wav_stats: Vec<WavStats> = Vec::new();
    let mut total_missed: usize = 0;
    let mut total_missed_no_v1: usize = 0;
    // For each V3 window N, total "missed + no-v1-neighbor + has decode within ±N bins"
    let mut in_window_totals: Vec<usize> = vec![0usize; V3_WINDOW_BINS.len()];
    // Diagnostic: of the no-v1-neighbor truths, distance distribution to
    // nearest subtracted-eligible decode (in freq_bins).
    let mut nearest_bin_distances_no_v1: Vec<f64> = Vec::new();

    eprintln!("Decoding top-20 hard-200 WAVs (production config — multipass N=3 + V1 ON)...");
    let cfg = Ft8Config::default();

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
        let pancetta_decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        // Every successful pancetta decode populates `tone_symbols` and is
        // therefore subtract-eligible (the multipass + V1 path subtracts
        // every decode with `tone_symbols.is_some()`). Treat all pancetta
        // decode positions as the V3 "subtract-eligible" set.
        let pancetta: Vec<(f64, f64, String)> = pancetta_decodes
            .iter()
            .filter(|d| d.tone_symbols.is_some())
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset.rem_euclid(SLOT_S),
                    d.text.trim().to_string(),
                )
            })
            .collect();

        let mut recovered = 0usize;
        let mut missed_locally: Vec<(f64, f64, String)> = Vec::new();
        for (tf, td, tm) in &truths {
            let matched = pancetta
                .iter()
                .any(|(_, _, pm)| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed_locally.push((*tf, *td, tm.clone()));
            }
        }

        let mut wav_missed_no_v1 = 0usize;
        let mut wav_in_window_per_n: Vec<usize> = vec![0usize; V3_WINDOW_BINS.len()];

        for (tf, td, _tm) in &missed_locally {
            // V1's direct-overlap reach: any pancetta decode within ±25 Hz
            // AND ±2 s slot-relative-dt. If NONE → V1 uncoverable.
            let has_v1_neighbor = pancetta.iter().any(|(pf, pdt, _)| {
                let df = (pf - tf).abs();
                let dd = (pdt - td).abs();
                df <= V1_REACH_HZ && dd <= NEIGHBOR_DT_S
            });
            if has_v1_neighbor {
                continue;
            }
            wav_missed_no_v1 += 1;

            // Nearest-decode bin distance (across ALL pancetta decodes,
            // ignoring DT — V3's relaxation is bin-targeted, not
            // time-targeted, because the subtract changes the bin's
            // entire time column).
            let mut min_bin_dist = f64::INFINITY;
            for (pf, _pdt, _) in &pancetta {
                let df_bin = (pf - tf).abs() / TONE_SPACING_HZ;
                if df_bin < min_bin_dist {
                    min_bin_dist = df_bin;
                }
            }
            if min_bin_dist.is_finite() {
                nearest_bin_distances_no_v1.push(min_bin_dist);
            }

            for (i, &n_bins) in V3_WINDOW_BINS.iter().enumerate() {
                let window_hz = n_bins as f64 * TONE_SPACING_HZ;
                let in_window = pancetta
                    .iter()
                    .any(|(pf, _pdt, _)| (pf - tf).abs() <= window_hz);
                if in_window {
                    wav_in_window_per_n[i] += 1;
                }
            }
        }

        for (i, &v) in wav_in_window_per_n.iter().enumerate() {
            in_window_totals[i] += v;
        }
        total_missed_no_v1 += wav_missed_no_v1;
        total_missed += missed_locally.len();

        wav_stats.push(WavStats {
            sha_short: sha[..8].to_string(),
            truth_total: truths.len(),
            recovered,
            missed: missed_locally.len(),
            missed_no_v1_neighbor: wav_missed_no_v1,
            in_window_per_n: wav_in_window_per_n,
        });
        eprintln!(
            "  [{idx:2}] {} truth={} rec={} missed={} no-v1-nbr={}",
            &sha[..8],
            truths.len(),
            recovered,
            missed_locally.len(),
            wav_missed_no_v1,
        );
    }

    println!("\n=== hb-086 V3 subtract-window-potential diagnostic (top-20 hard-200) ===\n");
    println!("Per-WAV breakdown (counts of 'no V1 neighbor' truths within ±N bins of any decode):");
    print!(
        "  {:>9} {:>6} {:>6} {:>7} {:>10}",
        "sha", "truth", "rec", "missed", "no-v1-nbr"
    );
    for &n in V3_WINDOW_BINS {
        print!(" {:>9}", format!("in±{}b", n));
    }
    println!();
    for w in &wav_stats {
        print!(
            "  {:>9} {:>6} {:>6} {:>7} {:>10}",
            w.sha_short, w.truth_total, w.recovered, w.missed, w.missed_no_v1_neighbor,
        );
        for v in &w.in_window_per_n {
            print!(" {:>9}", v);
        }
        println!();
    }

    println!(
        "\nAggregate (total missed = {}, total missed-no-v1-neighbor = {})",
        total_missed, total_missed_no_v1
    );

    println!("\nV3 targeting-window potential (over the V1-uncoverable subset):");
    println!(
        "  {:>5} {:>9} {:>11} {:>13}",
        "N bin", "±Hz", "in-window", "pct of no-v1"
    );
    let mut best_pct = 0.0f64;
    let mut best_n = 0usize;
    for (i, &n) in V3_WINDOW_BINS.iter().enumerate() {
        let pct = if total_missed_no_v1 > 0 {
            100.0 * in_window_totals[i] as f64 / total_missed_no_v1 as f64
        } else {
            0.0
        };
        if pct > best_pct {
            best_pct = pct;
            best_n = n;
        }
        let hz = n as f64 * TONE_SPACING_HZ;
        println!(
            "  {:>5} {:>9.2} {:>11} {:>12.1}%",
            n, hz, in_window_totals[i], pct
        );
    }

    // Distance distribution of nearest subtract-eligible decode (in freq_bins).
    if !nearest_bin_distances_no_v1.is_empty() {
        let mut buckets: [(f64, &str, usize); 8] = [
            (4.0, "0-4 bins (≤25 Hz)*", 0),
            (6.0, "4-6 bins (25-37 Hz)", 0),
            (8.0, "6-8 bins (37-50 Hz)", 0),
            (12.0, "8-12 bins (50-75 Hz)", 0),
            (16.0, "12-16 bins (75-100 Hz)", 0),
            (32.0, "16-32 bins (100-200 Hz)", 0),
            (64.0, "32-64 bins (200-400 Hz)", 0),
            (f64::INFINITY, ">64 bins (>400 Hz, isolated)", 0),
        ];
        let mut sorted = nearest_bin_distances_no_v1.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for d in &sorted {
            for b in buckets.iter_mut() {
                if *d <= b.0 {
                    b.2 += 1;
                    break;
                }
            }
        }
        println!("\n  Nearest subtract-eligible-decode freq-bin distance distribution");
        println!(
            "  (across {} no-v1-neighbor truths; * 0-4-bin bucket is residual rounding/dt-only-near):",
            sorted.len()
        );
        for b in &buckets {
            let pct = 100.0 * b.2 as f64 / sorted.len() as f64;
            println!("    {:>28}  {:>5} ({:.1}%)", b.1, b.2, pct);
        }
    }

    println!(
        "\nDecision threshold: PROCEED if any sweep N reaches ≥{:.0}% of no-v1-neighbor truths.",
        PROCEED_THRESHOLD_PCT
    );

    let verdict = if best_pct >= PROCEED_THRESHOLD_PCT {
        format!(
            "PROCEED — at N=±{} bins (±{:.1} Hz), {:.1}% of V1-uncoverable truths have a subtracted-eligible neighbor. Implement V3 with this targeting window.",
            best_n,
            best_n as f64 * TONE_SPACING_HZ,
            best_pct,
        )
    } else {
        format!(
            "SHELVE — best window (N=±{} bins, {:.1}%) does not clear the {:.0}% threshold. V3's targeting mechanism does not fit this corpus: missed truths in the V1-uncoverable subset are mostly in regions where subtraction did not happen (clean regions where sync_search simply missed weak signals).",
            best_n, best_pct, PROCEED_THRESHOLD_PCT,
        )
    };
    println!("\nVerdict: {verdict}");

    Ok(())
}
