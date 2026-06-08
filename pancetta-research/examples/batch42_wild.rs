//! Batch 42 — wild probes (Tier 3).
//!
//! 1. Reverse decode — decode time-reversed WAV. FT8 is asymmetric in
//!    time but the Costas array is at positions {0..7, 36..43, 72..79};
//!    reversing places the trailing Costas where the leading one was.
//!    Almost certainly hurts but cheap to try.
//! 2. Frequency dithering — shift WAV in frequency by {-1.5, -0.75, 0,
//!    +0.75, +1.5} Hz via complex baseband mixing, decode each, union.
//!    Tests whether sub-bin offsets surface different sync candidates.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch42_wild

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f64 = 12_000.0;

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

fn load_truth(ws: &Path, sha: &str) -> HashSet<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return HashSet::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn decode_default(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

/// Frequency shift by Δf Hz via real-valued cosine modulation. Note:
/// this is NOT a clean SSB shift — it produces image at -Δf too. For
/// small Δf and small bandwidth, the image is far enough away to not
/// confuse pancetta's sync.
///
/// More correct would be complex baseband mixing, but pancetta only
/// accepts real samples. As a probe, simple cosine modulation is
/// adequate: the actual decoder will only lock onto positive-freq
/// content matching Costas tones.
fn freq_shift(samples: &[f32], delta_hz: f64) -> Vec<f32> {
    let omega = 2.0 * std::f64::consts::PI * delta_hz / SAMPLE_RATE;
    samples
        .iter()
        .enumerate()
        .map(|(n, x)| {
            let phase = omega * n as f64;
            (*x as f64 * 2.0 * phase.cos()) as f32
        })
        .collect()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries: Vec<Value> = manifest["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .cloned()
        .collect();

    let mut base = Ft8Config::default();
    base.max_decode_passes = 2;
    base.ldpc_iterations = 200;

    eprintln!("baseline…");
    let mut baseline_tps = 0usize;
    let mut baseline_total = 0usize;
    let mut sha_to_baseline: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let decoded = decode_default(&samples, &base)?;
        baseline_total += decoded.len();
        for t in &decoded {
            if truth.contains(t) {
                baseline_tps += 1;
            }
        }
        sha_to_baseline.insert(sha.to_string(), decoded);
    }

    println!("## Batch 42 — wild probes");
    println!(
        "\n### Baseline (mp=2, ldpc=200): {} decodes / {} TPs",
        baseline_total, baseline_tps
    );

    // === Reverse decode (first 50 WAVs to keep cheap) ===
    println!("\n### Reverse decode (first 50 WAVs)");
    let mut rev_tps = 0usize;
    let mut rev_total = 0usize;
    let mut rev_n = 0usize;
    for entry in entries.iter().take(50) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let reversed: Vec<f32> = samples.into_iter().rev().collect();
        let truth = load_truth(&ws, sha);
        let decoded = decode_default(&reversed, &base)?;
        rev_total += decoded.len();
        for t in &decoded {
            if truth.contains(t) {
                rev_tps += 1;
            }
        }
        rev_n += 1;
    }
    println!(
        "  {:<30} {:>5} dec / {:>4} TPs (n={})",
        "reverse", rev_total, rev_tps, rev_n
    );

    // === Frequency dithering (all 200 WAVs, union) ===
    println!("\n### Frequency dithering: shifts {{-1.5, -0.75, 0, +0.75, +1.5}} Hz, union TPs");
    let shifts: Vec<f64> = vec![-1.5, -0.75, 0.0, 0.75, 1.5];
    let mut union_tps = 0usize;
    let mut union_size = 0usize;
    let mut per_shift_tps = vec![0usize; shifts.len()];
    let mut count = 0usize;
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);
        let mut union: HashSet<String> = HashSet::new();
        for (i, &df) in shifts.iter().enumerate() {
            let shifted = if df.abs() < 0.01 {
                samples.clone()
            } else {
                freq_shift(&samples, df)
            };
            let decoded = decode_default(&shifted, &base)?;
            for t in &decoded {
                if truth.contains(t) {
                    per_shift_tps[i] += 1;
                }
            }
            union.extend(decoded);
        }
        union_size += union.len();
        for t in &union {
            if truth.contains(t) {
                union_tps += 1;
            }
        }
        count += 1;
        if count % 50 == 0 {
            eprintln!("  freq-dither WAVs: {}/{}", count, entries.len());
        }
    }

    println!("  {:<30} per-shift TPs: {:?}", "freq-dither", per_shift_tps);
    let delta = union_tps as i64 - baseline_tps as i64;
    println!(
        "  union decodes: {}, union TPs: {}, Δ from baseline: {:+}",
        union_size, union_tps, delta
    );
    if delta > 10 {
        println!("  → freq-dither UNION ships +{} TPs", delta);
    }

    Ok(())
}
