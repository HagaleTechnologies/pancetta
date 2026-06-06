//! Batch 38 / hb-218a Session 1 — perfect-SIC α-sweep + phase-sweep probe.
//!
//! For the SIC-victim subset of the Batch 37 frontier (truths whose
//! neighbor pancetta decoded but truth didn't surface from the residual),
//! tests whether an α-fit + phase-shift perfect SIC subtraction recovers
//! them.
//!
//! Steps per truth:
//!   1. Encode neighbor.text → 79 FT8 symbols → 12.64s waveform at
//!      neighbor.freq_hz, positioned at neighbor.dt_s into the 15s WAV
//!   2. α-sweep: residual = signal - α × neighbor_wave for α ∈ {0.5..1.5}
//!   3. Phase-sweep (sample-level shift): try shifts ∈ {0,2,4,6} samples
//!      ≈ θ ∈ {0, π/2, π, 3π/2} at ~1500 Hz carrier
//!   4. Sanity: with α=1, does the subtraction kill the NEIGHBOR's own
//!      decode? If not, subtraction is no-op (phase mismatch).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch38_hb218a_alpha_sweep

use anyhow::{Context, Result};
use pancetta_ft8::encoder::Ft8Encoder;
use pancetta_ft8::modulator::Ft8Modulator;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const SAMPLE_RATE: u32 = 12_000;

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

fn build_neighbor_wave(
    text: &str,
    freq_hz: f64,
    dt_s: f64,
    wav_len: usize,
) -> Result<Option<Vec<f32>>> {
    let mut encoder = Ft8Encoder::new();
    let symbols = match encoder.encode_message(text, None) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    if !(200.0..=4000.0).contains(&freq_hz) {
        return Ok(None);
    }
    let mut modulator = Ft8Modulator::new(SAMPLE_RATE, freq_hz, 1.0)?;
    let raw_wave = match modulator.modulate_symbols(&symbols, 0.0) {
        Ok(w) => w,
        Err(_) => return Ok(None),
    };
    let dt_samples = (dt_s * SAMPLE_RATE as f64).round() as i64;
    let mut out = vec![0.0f32; wav_len];
    if dt_samples >= 0 {
        let start = dt_samples as usize;
        let end = (start + raw_wave.len()).min(wav_len);
        if end > start {
            let copy_len = end - start;
            out[start..end].copy_from_slice(&raw_wave[..copy_len]);
        }
    } else {
        let skip = (-dt_samples) as usize;
        if skip < raw_wave.len() {
            let avail = raw_wave.len() - skip;
            let end = avail.min(wav_len);
            out[..end].copy_from_slice(&raw_wave[skip..skip + end]);
        }
    }
    Ok(Some(out))
}

fn decode_default(samples: &[f32]) -> Result<HashSet<String>> {
    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
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
    let mut sha_to_decoded: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        sha_to_wav.insert(sha.to_string(), PathBuf::from(wav_path));
    }

    let probe_n: usize = std::env::var("BATCH38_PROBE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    eprintln!("Probe sample size cap: {}", probe_n);

    // mp=2 decoded sets (to identify SIC victims = neighbor was decoded).
    eprintln!("Re-decoding hard-200 with mp=2 to identify SIC victims...");
    let mut cfg_mp2 = Ft8Config::default();
    cfg_mp2.max_decode_passes = 2;
    let needed_shas: HashSet<String> = frontier.iter().map(|f| f.sha.clone()).collect();
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
        sha_to_decoded.insert(
            sha.to_string(),
            decoded.into_iter().map(|d| d.text).collect(),
        );
    }

    let sic_victims: Vec<&FrontierEntry> = frontier
        .iter()
        .filter(|f| {
            f.delta_snr_db > -3.0
                && sha_to_decoded
                    .get(&f.sha)
                    .map(|s| s.contains(&f.neighbor_text))
                    .unwrap_or(false)
        })
        .collect();

    println!("## Batch 38 — hb-218a Session 1 perfect-SIC α-sweep");
    println!(
        "SIC-victim universe (Batch 37 CC1 textbook): {}",
        sic_victims.len()
    );
    println!("Probing top-{}", probe_n.min(sic_victims.len()));

    // === α-sweep at zero phase shift ===
    let alphas: Vec<f64> = vec![0.5, 0.7, 0.9, 1.0, 1.1, 1.3, 1.5];
    let mut recovered_at_alpha: BTreeMap<u32, usize> = BTreeMap::new();
    let mut recovered_any_alpha = 0usize;
    let mut encodable_count = 0usize;
    let mut probes_executed = 0usize;

    for victim in sic_victims.iter().take(probe_n) {
        let Some(wav_path) = sha_to_wav.get(&victim.sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let Some(neighbor_wave) = build_neighbor_wave(
            &victim.neighbor_text,
            victim.neighbor_freq_hz,
            victim.neighbor_dt_s,
            samples.len(),
        )?
        else {
            continue;
        };
        encodable_count += 1;
        probes_executed += 1;

        let mut victim_recovered = false;
        for &alpha in &alphas {
            let residual: Vec<f32> = samples
                .iter()
                .zip(&neighbor_wave)
                .map(|(s, n)| s - (alpha as f32) * n)
                .collect();
            let decoded = decode_default(&residual)?;
            if decoded.contains(&victim.text) {
                *recovered_at_alpha
                    .entry((alpha * 100.0) as u32)
                    .or_insert(0) += 1;
                victim_recovered = true;
            }
        }
        if victim_recovered {
            recovered_any_alpha += 1;
        }
    }

    println!("\n### α-sweep (zero phase) — recovery counts");
    println!("  {:<6} {:>10}", "α", "recovered");
    for &a in &alphas {
        let n = recovered_at_alpha
            .get(&((a * 100.0) as u32))
            .copied()
            .unwrap_or(0);
        println!("  {:<6.2} {:>10}", a, n);
    }
    println!(
        "  ANY-α: {}/{} ({:.1}%)",
        recovered_any_alpha,
        probes_executed,
        recovered_any_alpha as f64 / probes_executed.max(1) as f64 * 100.0
    );
    println!("  (encodable: {} of {})", encodable_count, probe_n);

    // === Sanity check: does α=1 subtraction kill the neighbor's own decode? ===
    println!("\n### Sanity — α=1 subtraction kills neighbor's own decode?");
    let mut sanity_killed = 0usize;
    let mut sanity_probed = 0usize;
    for victim in sic_victims.iter().take(20) {
        let Some(wav_path) = sha_to_wav.get(&victim.sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let Some(neighbor_wave) = build_neighbor_wave(
            &victim.neighbor_text,
            victim.neighbor_freq_hz,
            victim.neighbor_dt_s,
            samples.len(),
        )?
        else {
            continue;
        };
        let residual: Vec<f32> = samples
            .iter()
            .zip(&neighbor_wave)
            .map(|(s, n)| s - n)
            .collect();
        let decoded = decode_default(&residual)?;
        if !decoded.contains(&victim.neighbor_text) {
            sanity_killed += 1;
        }
        sanity_probed += 1;
    }
    let kill_rate = sanity_killed as f64 / sanity_probed.max(1) as f64;
    println!(
        "  α=1 kills neighbor decode in: {}/{} ({:.1}%)",
        sanity_killed,
        sanity_probed,
        kill_rate * 100.0
    );
    if kill_rate < 0.3 {
        println!("  → subtraction is MOSTLY NO-OP. Phase/freq mismatch dominates.");
    } else {
        println!("  → subtraction has real effect. α-sweep zeros suggest residual is genuinely degraded.");
    }

    // === Phase sweep — sample-level shift approximating phase rotation ===
    println!("\n### Phase sweep (sample shifts ≈ θ ∈ {{0, π/2, π, 3π/2}} at ~1500 Hz)");
    let shifts: Vec<i64> = vec![0, 2, 4, 6];
    let mut phase_recovered: BTreeMap<i64, usize> = BTreeMap::new();
    let mut phase_killed_neighbor: BTreeMap<i64, usize> = BTreeMap::new();
    let mut phase_probed = 0usize;

    for victim in sic_victims.iter().take(probe_n) {
        let Some(wav_path) = sha_to_wav.get(&victim.sha) else {
            continue;
        };
        let samples = load_wav(wav_path)?;
        let Some(neighbor_wave) = build_neighbor_wave(
            &victim.neighbor_text,
            victim.neighbor_freq_hz,
            victim.neighbor_dt_s,
            samples.len(),
        )?
        else {
            continue;
        };
        phase_probed += 1;
        for &shift in &shifts {
            let s_us = shift as usize;
            let mut shifted = vec![0.0f32; samples.len()];
            if s_us < shifted.len() {
                let copy_len = samples.len() - s_us;
                shifted[s_us..].copy_from_slice(&neighbor_wave[..copy_len]);
            }
            let residual: Vec<f32> = samples.iter().zip(&shifted).map(|(s, n)| s - n).collect();
            let decoded = decode_default(&residual)?;
            if decoded.contains(&victim.text) {
                *phase_recovered.entry(shift).or_insert(0) += 1;
            }
            if !decoded.contains(&victim.neighbor_text) {
                *phase_killed_neighbor.entry(shift).or_insert(0) += 1;
            }
        }
    }

    println!(
        "  {:<8} {:>10} {:>12}",
        "shift", "victim_rec", "neighb_kill"
    );
    for &shift in &shifts {
        let r = phase_recovered.get(&shift).copied().unwrap_or(0);
        let k = phase_killed_neighbor.get(&shift).copied().unwrap_or(0);
        println!("  {:<8} {:>10} {:>12}", shift, r, k);
    }
    println!("  (phase_probed = {})", phase_probed);

    // === Decision ===
    let total_recoveries =
        recovered_any_alpha + phase_recovered.values().map(|n| *n).sum::<usize>();
    println!(
        "\n### Decision: total unique recovery events: {} of {} probes",
        total_recoveries, probes_executed
    );
    let rate = total_recoveries as f64 / probes_executed.max(1) as f64;
    if rate > 0.5 {
        println!("  → recovery rate >50% → hb-218a PROCEED Session 2");
    } else if rate > 0.2 {
        println!("  → recovery rate {:.1}% MARGINAL", rate * 100.0);
    } else {
        println!(
            "  → recovery rate {:.1}% → hb-218a SHELVE-LEAN; pivot to hb-218c for next batch",
            rate * 100.0
        );
    }

    Ok(())
}
