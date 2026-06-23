//! Batch 42 — WSJT-X multi-interval decoding emulation (corrected).
//!
//! WSJT-X 2.2+ decodes FT8 at three intervals within the 15s slot.
//! Decoder requires ≥12.64s of audio per decode. So "intervals" become
//! SLIDING DECODE WINDOWS within the 15s WAV:
//!
//! - window starting at sample 0 (standard pancetta behavior)
//! - window starting at +0.5s offset
//! - window starting at +1.0s offset
//! - window starting at +1.5s offset
//! - window starting at +2.0s offset (latest possible)
//!
//! Each window sees DIFFERENT noise floor estimates and DIFFERENT
//! sync candidates. Union of decodes across windows should surface
//! more TPs than single-window.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch42_multi_interval

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: usize = 12_000;
const WINDOW_SAMPLES: usize = 151_680; // 12.64s × 12kHz

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

fn decode_window_at(
    samples: &[f32],
    start_s: f64,
    cfg: &Ft8Config,
) -> Result<Option<HashSet<String>>> {
    let start_n = (start_s * SAMPLE_RATE as f64) as usize;
    if start_n + WINDOW_SAMPLES > samples.len() {
        return Ok(None);
    }
    let window = &samples[start_n..start_n + WINDOW_SAMPLES];
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(window)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(Some(decoded.into_iter().map(|d| d.text).collect()))
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

    // sliding-window offsets
    let offsets: Vec<f64> = vec![0.0, 0.5, 1.0, 1.5, 2.0];
    let mut per_offset_tps = vec![0usize; offsets.len()];
    let mut per_offset_total = vec![0usize; offsets.len()];
    let mut union_tps_count = 0usize;
    let mut baseline_tps_count = 0usize;
    let mut union_decode_count = 0usize;

    let mut count = 0usize;
    for entry in entries.iter() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let truth = load_truth(&ws, sha);

        let mut union: HashSet<String> = HashSet::new();
        let mut baseline_set: HashSet<String> = HashSet::new();
        for (i, &off) in offsets.iter().enumerate() {
            if let Some(decoded) = decode_window_at(&samples, off, &base)? {
                per_offset_total[i] += decoded.len();
                for t in &decoded {
                    if truth.contains(t) {
                        per_offset_tps[i] += 1;
                    }
                }
                if off == 0.0 {
                    baseline_set = decoded.clone();
                }
                union.extend(decoded);
            }
        }

        union_decode_count += union.len();
        for t in &union {
            if truth.contains(t) {
                union_tps_count += 1;
            }
        }
        for t in &baseline_set {
            if truth.contains(t) {
                baseline_tps_count += 1;
            }
        }

        count += 1;
        if count % 50 == 0 {
            eprintln!("  WAVs: {}/{}", count, entries.len());
        }
    }

    println!("## Batch 42 — Multi-interval (sliding decode window) emulation");
    println!("\n### Per-offset (each is an INDEPENDENT decode)");
    println!(
        "  {:<10} {:>10} {:>10} {:>10}",
        "start_s", "decodes", "TPs", "precision"
    );
    for (i, &off) in offsets.iter().enumerate() {
        let prec = per_offset_tps[i] as f64 / per_offset_total[i].max(1) as f64 * 100.0;
        println!(
            "  {:<10.2} {:>10} {:>10} {:>9.1}%",
            off, per_offset_total[i], per_offset_tps[i], prec
        );
    }

    println!("\n### Sliding-window UNION vs single-window (start=0) baseline");
    println!("  union decodes:  {}", union_decode_count);
    println!("  union TPs:      {}", union_tps_count);
    println!("  baseline TPs:   {}", baseline_tps_count);
    let delta = union_tps_count as i64 - baseline_tps_count as i64;
    println!("  Δ from union over baseline: {:+} TPs", delta);
    if delta > 10 {
        println!("  → SHIPPABLE: union surfaces {} extra TPs", delta);
    }

    Ok(())
}
