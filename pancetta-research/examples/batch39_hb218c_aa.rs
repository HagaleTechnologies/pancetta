//! Batch 39 / hb-218c Session 1 — louder-missed anomaly characterization.
//!
//! From Batch 37 frontier (974 capture-locked truths), filter to the
//! 238 where the missed truth is LOUDER than its decoded-or-missed
//! neighbor (delta_snr_db < -3.0).
//!
//! AA1: sub-bucket the 238 by |Δsnr| magnitude
//! AA2: FFT-energy probe at truth.freq vs neighbor.freq — is the
//!      truth actually higher-energy in the pancetta-visible spectrum?
//!      (Cross-checks jt9's SNR estimate.)
//! BB1: re-run with V3 relax=-3.0 + window=12, count truths recovered
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch39_hb218c_aa

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
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

/// Compute total energy in a ±50 Hz band around `freq_hz` using a sliding
/// FFT (Goertzel-style for one bin would be ideal; here we use a simple
/// DFT over short windows and sum). Approximate but adequate for relative
/// comparison.
fn band_energy(samples: &[f32], center_freq: f64, sample_rate: f64) -> f64 {
    // Frame: 1024-sample windows, 50% overlap
    let frame = 1024;
    let hop = 512;
    let mut total = 0.0f64;
    let f_low = center_freq - 50.0;
    let f_high = center_freq + 50.0;
    let bin_low = (f_low * frame as f64 / sample_rate).floor() as usize;
    let bin_high = (f_high * frame as f64 / sample_rate).ceil() as usize;

    let mut start = 0;
    while start + frame <= samples.len() {
        // Hann window + magnitude-squared in target bins (manual DFT for
        // the target bins only — much cheaper than full FFT for a small
        // bin range).
        for bin in bin_low..=bin_high {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            let omega = 2.0 * std::f64::consts::PI * bin as f64 / frame as f64;
            for n in 0..frame {
                let hann =
                    0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / (frame - 1) as f64).cos();
                let x = samples[start + n] as f64 * hann;
                re += x * (omega * n as f64).cos();
                im += x * (omega * n as f64).sin();
            }
            total += re * re + im * im;
        }
        start += hop;
    }
    total
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let frontier_path = ws.join("research/scorecards/batch37_frontier.json");
    let frontier_json = std::fs::read_to_string(&frontier_path)?;
    let frontier: Vec<FrontierEntry> = serde_json::from_str(&frontier_json)?;

    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let mut sha_to_wav: HashMap<String, PathBuf> = HashMap::new();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        sha_to_wav.insert(sha.to_string(), PathBuf::from(wav_path));
    }

    // Filter: louder-missed (truth louder than neighbor)
    let louder_missed: Vec<&FrontierEntry> =
        frontier.iter().filter(|f| f.delta_snr_db < -3.0).collect();

    println!("## Batch 39 — hb-218c louder-missed anomaly");
    println!("Universe: {} louder-missed truths", louder_missed.len());

    // AA1 sub-bucket by |Δsnr|
    println!("\n### AA1 — sub-bucket by truth-louder Δsnr magnitude");
    let mut bucket: BTreeMap<&'static str, usize> = BTreeMap::new();
    for v in &louder_missed {
        let mag = -v.delta_snr_db;
        let b = if mag < 6.0 {
            "3-6 dB louder"
        } else if mag < 12.0 {
            "6-12 dB louder"
        } else if mag < 20.0 {
            "12-20 dB louder"
        } else {
            "20+ dB louder"
        };
        *bucket.entry(b).or_insert(0) += 1;
    }
    println!("  {:<18} {:>6}", "Δsnr (truth-louder)", "count");
    for (k, v) in &bucket {
        println!("  {:<18} {:>6}", k, v);
    }

    // AA2 FFT-energy cross-check (first 50 cases to keep run-time bounded)
    let aa2_n: usize = std::env::var("BATCH39_AA2_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    eprintln!("\nAA2: FFT-energy probe on top-{} louder-missed...", aa2_n);

    let mut consistent_with_jt9 = 0usize;
    let mut inconsistent = 0usize;
    let mut energy_delta_db_buckets: BTreeMap<&'static str, usize> = BTreeMap::new();

    for v in louder_missed.iter().take(aa2_n) {
        let Some(wav_path) = sha_to_wav.get(&v.sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let truth_e = band_energy(&samples, v.freq_hz, 12000.0);
        let neighbor_e = band_energy(&samples, v.neighbor_freq_hz, 12000.0);

        // Convert energy ratio to dB
        let energy_db = 10.0 * (truth_e / neighbor_e.max(1e-30)).log10();
        // jt9-claimed: truth is louder by |delta_snr_db|, so energy_db should be ~|delta_snr_db|
        // if jt9 is consistent. If energy_db is negative or near zero, jt9's SNR estimate is suspect.

        if energy_db > 1.0 {
            consistent_with_jt9 += 1;
        } else {
            inconsistent += 1;
        }

        let b = if energy_db < -3.0 {
            "<-3 (truth quieter!)"
        } else if energy_db < 0.0 {
            "-3..0 (truth ~ neighbor)"
        } else if energy_db < 3.0 {
            "0-3 (truth slightly louder)"
        } else if energy_db < 9.0 {
            "3-9 (truth louder)"
        } else {
            "9+ (truth much louder)"
        };
        *energy_delta_db_buckets.entry(b).or_insert(0) += 1;
    }

    println!(
        "\n### AA2 — FFT-energy(truth.freq) vs FFT-energy(neighbor.freq) (n={})",
        aa2_n
    );
    println!(
        "  consistent with jt9 (energy_db > +1): {}",
        consistent_with_jt9
    );
    println!("  INCONSISTENT (energy_db ≤ +1):       {}", inconsistent);
    println!("\n  energy_db bucket counts:");
    println!("  {:<28} {:>6}", "energy(truth) − energy(neigh)", "count");
    for (k, v) in &energy_delta_db_buckets {
        println!("  {:<28} {:>6}", k, v);
    }
    let incons_rate = inconsistent as f64 / (consistent_with_jt9 + inconsistent).max(1) as f64;
    if incons_rate > 0.5 {
        println!(
            "  → {:.0}% of louder-missed truths are NOT actually higher-energy at freq band. jt9 SNR estimate is unreliable for these cases.",
            incons_rate * 100.0
        );
    } else {
        println!(
            "  → {:.0}% are consistent; only {:.0}% inconsistent. Anomaly is mostly real.",
            (1.0 - incons_rate) * 100.0,
            incons_rate * 100.0
        );
    }

    // BB1 — V3 force-decode at louder-missed coords
    eprintln!("\nBB1: V3 relax=-3.0 + window=12 on all louder-missed WAVs...");
    let mut cfg_v3 = Ft8Config::default();
    cfg_v3.max_decode_passes = 2;
    cfg_v3.joint_residual_sync_relax_db = -3.0;
    cfg_v3.joint_residual_sync_window_bins = 12; // wider than default 8

    let needed_shas: HashSet<String> = louder_missed.iter().map(|f| f.sha.clone()).collect();
    let mut sha_to_v3: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_v3 = 0usize;

    for sha in &needed_shas {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let mut decoder =
            Ft8Decoder::new(cfg_v3.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        total_v3 += decoded.len();
        sha_to_v3.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }

    let mut louder_recovered = 0usize;
    for v in &louder_missed {
        if sha_to_v3
            .get(&v.sha)
            .map(|s| s.contains(&v.text))
            .unwrap_or(false)
        {
            louder_recovered += 1;
        }
    }

    println!(
        "\n### BB1 — V3 relax=-3.0 window=12 on {} louder-missed WAVs",
        needed_shas.len()
    );
    println!("  total V3 decodes: {}", total_v3);
    println!(
        "  louder-missed recovered: {}/{} ({:.1}%)",
        louder_recovered,
        louder_missed.len(),
        louder_recovered as f64 / louder_missed.len().max(1) as f64 * 100.0
    );

    let recovery_rate = louder_recovered as f64 / louder_missed.len().max(1) as f64 * 100.0;
    if recovery_rate > 25.0 {
        println!("  → hb-218c PROCEED Session 2: V3 surfaces a meaningful fraction");
    } else if recovery_rate > 5.0 {
        println!("  → MARGINAL ({:.1}%) — deeper probe needed", recovery_rate);
    } else {
        println!(
            "  → hb-218c SHELVE-LEAN ({:.1}%): V3 at -3.0 doesn't recover the louder-missed either",
            recovery_rate
        );
    }

    Ok(())
}
