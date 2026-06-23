//! Batch 38 / hb-218a Session 1 verification — hb-086 V3 inheritance probe.
//!
//! Batch 38's naive perfect-SIC subtraction approach failed (0/43
//! recovered) because jt9's 1 Hz frequency resolution × 12.64s of signal
//! produces ~6 cycles of phase drift, making the synthesized subtraction
//! destructive at random phase. The proper test of "is there decodable
//! signal in the post-SIC residual" needs pancetta's INTERNAL SIC.
//!
//! This probe inherits that test from hb-086 V3, which already shipped
//! the `joint_residual_sync_relax_db` knob: relax min_sync_score in a
//! localized window around each subtracted-eligible decode (the exact
//! mechanism hb-218a would need to surface decodable weak residual at
//! the truth's expected coords).
//!
//! hb-086 V3 was tested at hard-200 wide and shelved with 0 additional
//! decoded messages. Here we narrow to the 479 textbook SIC-victim
//! truths from Batch 37 CC1 and check: with V3 at -2.0 dB relax (the
//! most-aggressive previously-tested setting), do ANY of the SIC-victim
//! truths surface?
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch38_hb218a_v3_inherit

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

    // mp=2 baseline (to identify SIC-victims; same as Batch 38 part 1)
    eprintln!("Pass 1: mp=2 baseline...");
    let mut cfg_mp2 = Ft8Config::default();
    cfg_mp2.max_decode_passes = 2;
    let needed_shas: HashSet<String> = frontier.iter().map(|f| f.sha.clone()).collect();
    let mut sha_to_mp2: HashMap<String, HashSet<String>> = HashMap::new();
    for sha in &needed_shas {
        let Some(wav_path) = sha_to_wav.get(sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let mut decoder = Ft8Decoder::new(cfg_mp2.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        sha_to_mp2.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }

    // SIC-victim subset (Batch 37 CC1 textbook)
    let sic_victims: Vec<&FrontierEntry> = frontier
        .iter()
        .filter(|f| {
            f.delta_snr_db > -3.0
                && sha_to_mp2
                    .get(&f.sha)
                    .map(|s| s.contains(&f.neighbor_text))
                    .unwrap_or(false)
        })
        .collect();

    println!("## Batch 38 — hb-218a V3-inheritance probe");
    println!("SIC-victim universe: {}", sic_victims.len());

    // V3-enabled config: mp=2 + joint_residual_sync_relax_db = -2.0
    eprintln!("Pass 2: mp=2 + V3 relax_db=-2.0 + window_bins=8...");
    let mut cfg_v3 = Ft8Config::default();
    cfg_v3.max_decode_passes = 2;
    cfg_v3.joint_residual_sync_relax_db = -2.0;
    cfg_v3.joint_residual_sync_window_bins = 8;

    let mut sha_to_v3: HashMap<String, HashSet<String>> = HashMap::new();
    let mut total_v3_decodes = 0usize;
    let mut total_mp2_decodes = 0usize;
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
        total_v3_decodes += decoded.len();
        let set: HashSet<String> = decoded.into_iter().map(|d| d.text).collect();
        sha_to_v3.insert(sha.to_string(), set);
        if let Some(mp2) = sha_to_mp2.get(sha) {
            total_mp2_decodes += mp2.len();
        }
    }

    // Count how many of the SIC-victim truths surfaced under V3 but not mp=2
    let mut v3_only_victim_recovery = 0usize;
    let mut victim_in_mp2 = 0usize;
    let mut victim_in_v3 = 0usize;
    for victim in &sic_victims {
        let in_mp2 = sha_to_mp2
            .get(&victim.sha)
            .map(|s| s.contains(&victim.text))
            .unwrap_or(false);
        let in_v3 = sha_to_v3
            .get(&victim.sha)
            .map(|s| s.contains(&victim.text))
            .unwrap_or(false);
        if in_mp2 {
            victim_in_mp2 += 1;
        }
        if in_v3 {
            victim_in_v3 += 1;
        }
        if in_v3 && !in_mp2 {
            v3_only_victim_recovery += 1;
        }
    }

    println!(
        "\n### V3 vs mp=2 decode totals (across {} WAVs)",
        needed_shas.len()
    );
    println!("  mp=2 decodes total: {}", total_mp2_decodes);
    println!("  V3 decodes total:   {}", total_v3_decodes);
    let extra = total_v3_decodes as i64 - total_mp2_decodes as i64;
    println!("  V3 adds:            {} decodes", extra);

    println!("\n### SIC-victim recovery (n={} truths)", sic_victims.len());
    println!("  in mp=2 baseline: {} (sanity; expected 0)", victim_in_mp2);
    println!("  in V3 (relax=-2.0): {}", victim_in_v3);
    println!("  V3-only NEW recoveries: {}", v3_only_victim_recovery);

    let recovery_rate = v3_only_victim_recovery as f64 / sic_victims.len().max(1) as f64 * 100.0;
    println!("\n### V3-inheritance recovery rate: {:.1}%", recovery_rate);
    if recovery_rate < 5.0 {
        println!(
            "  → hb-218a SHELVED by inheritance — the residual at SIC-victim coords does not contain decodable signal under V3 relaxation. Direct evidence for hb-218a's central question is null at relax=-2.0."
        );
    } else {
        println!("  → some recovery; hb-218a may have residual headroom worth Session 2");
    }

    Ok(())
}
