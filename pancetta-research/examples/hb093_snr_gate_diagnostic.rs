//! hb-093 — Per-position residual SNR pre-decode gate diagnostic.
//!
//! Two-part V3-doctrine kill-switch:
//!
//! Part 1 (efficiency): of the candidates the gate evaluates in the
//! `joint_pair_retry` pass, what fraction would be filtered at each
//! candidate threshold? PROCEED if ≥30% filtered at any threshold.
//!
//! Part 2 (decodability): of the filtered candidates at a given
//! threshold, what fraction actually DID decode through LDPC+CRC+plausibility
//! in production (gate-off baseline)? PROCEED if ≤2% of gated-out
//! candidates were actually decoded.
//!
//! Methodology: run the decoder with `residual_snr_diagnostic = true` on
//! the top-5 worst hard-200 WAVs (production config, gate disabled). The
//! diagnostic captures, per candidate evaluated in `joint_pair_retry_pass`,
//! the tuple `(sync_score, residual_snr_db, decoded_ok)`. We then sweep
//! gate thresholds offline and compute both metrics.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb093_snr_gate_diagnostic

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

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

const TOP_N_WAVS: usize = 5;
// SNR is the WAV-relative (bandwidth-corrected) dB returned by
// `par_estimate_snr_spectrogram` — for noise-only positions on typical
// hard-200 residuals this sits in the −15 to −30 range. Sweep covers the
// iter-spec values plus denser low-end for sweet-spot tuning.
const THRESHOLDS_DB: &[f64] = &[
    -3.0, -5.0, -7.0, -10.0, -12.0, -15.0, -18.0, -20.0, -22.0, -25.0, -28.0, -30.0,
];

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    // 1. Pull top-N wav_hashes from main.json's per_wav_top_failures.
    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(TOP_N_WAVS)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();
    if top_hashes.is_empty() {
        anyhow::bail!("no top-failures in main.json");
    }

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

    // 3. Build production config with the diagnostic flag ON. Gate stays
    // OFF (None) — we want the baseline production behavior so we capture
    // ALL candidates the gate WOULD see at decision time, and per-candidate
    // ground truth on whether each actually decoded.
    let cfg = Ft8Config {
        residual_snr_diagnostic: true,
        residual_snr_gate_db: None,
        ..Ft8Config::default()
    };

    let mut all_records: Vec<(f64, f32, bool)> = Vec::new();
    let mut wav_summaries: Vec<(String, usize, usize, f64)> = Vec::new(); // (sha8, n_cand, n_decoded, elapsed_ms)

    eprintln!(
        "hb-093 diagnostic: decoding top-{TOP_N_WAVS} hard-200 WAVs (production + diagnostic ON, gate OFF)..."
    );

    for (idx, sha) in top_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };

        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let t0 = Instant::now();
        let _decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let records = decoder.take_residual_snr_diagnostic();
        let n_candidates = records.len();
        let n_decoded = records.iter().filter(|(_, _, ok)| *ok).count();
        wav_summaries.push((sha[..8].to_string(), n_candidates, n_decoded, elapsed_ms));
        eprintln!(
            "  [{idx:2}] {} candidates_in_pair_retry={} decoded_in_pair_retry={} wall_ms={:.1}",
            &sha[..8],
            n_candidates,
            n_decoded,
            elapsed_ms
        );
        all_records.extend(records);
    }

    println!();
    println!("=== Per-WAV summary ===");
    println!(
        "{:<10} {:>10} {:>10} {:>10}",
        "wav_sha8", "cand", "decoded", "wall_ms"
    );
    for (sha8, c, d, e) in &wav_summaries {
        println!("{:<10} {:>10} {:>10} {:>10.1}", sha8, c, d, e);
    }

    let total_candidates = all_records.len();
    if total_candidates == 0 {
        println!();
        println!("WARNING: zero candidates surfaced in joint_pair_retry across all WAVs.");
        println!("DECISION: SHELVE — gate has nothing to do on this corpus subset.");
        return Ok(());
    }
    let total_decoded = all_records.iter().filter(|(_, _, ok)| *ok).count();

    // SNR distribution summary.
    let mut snrs: Vec<f32> = all_records.iter().map(|(_, s, _)| *s).collect();
    snrs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let snr_min = snrs.first().copied().unwrap_or(0.0);
    let snr_max = snrs.last().copied().unwrap_or(0.0);
    let snr_med = snrs[snrs.len() / 2];
    let snr_q10 = snrs[snrs.len() / 10];
    let snr_q25 = snrs[snrs.len() / 4];
    let snr_q75 = snrs[(snrs.len() * 3) / 4];
    let snr_q90 = snrs[(snrs.len() * 9) / 10];

    println!();
    println!("=== SNR distribution (per-candidate residual SNR, dB) ===");
    println!(
        "n={} min={:.1} q10={:.1} q25={:.1} median={:.1} q75={:.1} q90={:.1} max={:.1}",
        snrs.len(),
        snr_min,
        snr_q10,
        snr_q25,
        snr_med,
        snr_q75,
        snr_q90,
        snr_max
    );

    let mut decoded_snrs: Vec<f32> = all_records
        .iter()
        .filter(|(_, _, ok)| *ok)
        .map(|(_, s, _)| *s)
        .collect();
    decoded_snrs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!();
    if !decoded_snrs.is_empty() {
        let d_min = decoded_snrs.first().copied().unwrap_or(0.0);
        let d_q10 = decoded_snrs[decoded_snrs.len() / 10];
        let d_med = decoded_snrs[decoded_snrs.len() / 2];
        let d_max = decoded_snrs.last().copied().unwrap_or(0.0);
        println!(
            "Decoded-only SNR: n={} min={:.1} q10={:.1} median={:.1} max={:.1}",
            decoded_snrs.len(),
            d_min,
            d_q10,
            d_med,
            d_max
        );
    } else {
        println!("Decoded-only SNR: n=0 (no pair-retry decodes on this WAV subset)");
    }

    println!();
    println!("=== Threshold sweep ===");
    println!(
        "{:>9} {:>12} {:>10} {:>16} {:>16}",
        "thr_dB", "filtered", "filter_%", "filt_decoded", "lost_pct_of_dec"
    );
    println!("(filter_% = of all candidates; filt_decoded = those that DID decode in baseline; lost_pct_of_dec = lost / total_decoded)");

    let mut best_proceed: Option<(f64, f64, f64)> = None;
    let mut best_efficiency_proceed: Option<f64> = None;
    for &thr in THRESHOLDS_DB {
        let filtered: usize = all_records
            .iter()
            .filter(|(_, snr, _)| (*snr as f64) < thr)
            .count();
        let filt_decoded: usize = all_records
            .iter()
            .filter(|(_, snr, ok)| (*snr as f64) < thr && *ok)
            .count();
        let filter_pct = filtered as f64 / total_candidates as f64 * 100.0;
        let lost_pct_of_dec = if total_decoded == 0 {
            0.0
        } else {
            filt_decoded as f64 / total_decoded as f64 * 100.0
        };
        println!(
            "{:>9.1} {:>12} {:>9.1}% {:>16} {:>15.2}%",
            thr, filtered, filter_pct, filt_decoded, lost_pct_of_dec
        );
        if filter_pct >= 30.0 {
            best_efficiency_proceed.get_or_insert(thr);
        }
        if filter_pct >= 30.0 && lost_pct_of_dec <= 2.0 {
            match best_proceed {
                None => best_proceed = Some((thr, filter_pct, lost_pct_of_dec)),
                Some((_, best_filter, _)) if filter_pct > best_filter => {
                    best_proceed = Some((thr, filter_pct, lost_pct_of_dec))
                }
                _ => {}
            }
        }
    }

    println!();
    println!("=== Decision (V3-doctrine kill switch) ===");
    println!(
        "Part 1 (efficiency, filter_% ≥ 30): {}",
        match best_efficiency_proceed {
            Some(thr) => format!("PROCEED at thr ≤ {thr:.1} dB"),
            None => "FAIL — no threshold filters ≥30% of candidates".into(),
        }
    );
    let part2_msg = if let Some((thr, fp, lp)) = best_proceed {
        format!("PROCEED at thr={thr:.1} dB (filter={fp:.1}%, lost_decodes={lp:.2}% ≤ 2%)")
    } else if total_decoded == 0 {
        "WARN — no pair-retry decodes in this WAV subset; lost_pct undefined".into()
    } else {
        "FAIL — every threshold that filters ≥30% also drops >2% of baseline decodes".into()
    };
    println!("Part 2 (decodability, lost_pct_of_dec ≤ 2): {part2_msg}");

    println!();
    match best_proceed {
        Some((thr, fp, lp)) => {
            println!("=> PROCEED to implementation. Recommended initial gate threshold:");
            println!("   --residual-snr-gate-db {thr:.1}");
            println!(
                "   (filters {fp:.1}% of joint_pair_retry candidates, drops {lp:.2}% of pair-retry decodes)"
            );
        }
        None => {
            if best_efficiency_proceed.is_some() && total_decoded == 0 {
                println!(
                    "=> EDGE CASE: gate has work (efficiency PROCEED) but no decodes in pair-retry on this WAV subset."
                );
                println!(
                    "   Consider expanding the WAV subset, or proceed cautiously to a hard-200 production sweep."
                );
            } else if best_efficiency_proceed.is_some() {
                println!(
                    "=> SHELVE: gate filters candidates efficiently but is unsafe (too many gated-out positions actually decode)."
                );
            } else {
                println!(
                    "=> SHELVE: residual SNR distribution doesn't separate decodable from noise positions cleanly enough to filter ≥30%."
                );
            }
        }
    }

    Ok(())
}
