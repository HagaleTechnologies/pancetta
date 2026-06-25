//! Batch 102 — FREQ_OSR knob audit (hb-225 mechanism test on the current baseline).
//!
//! hb-225 (ft8mon 2-D sub-bin Costas grid) proposes finer-than-current
//! sub-bin frequency search to surface sync candidates pancetta misses in
//! the band-middle recall hole. Batch 45's sub-Hz freq-dither probe
//! corroborated the *mechanism* (+33 TPs at N=200) but at ~5× decode cost,
//! and that was BEFORE the dt-alignment fix (Batch 88), hash-normalized
//! scoring (Batch 87), and the decode_origin work. Per probe-baseline
//! discipline, re-verify on the CURRENT baseline before committing to the
//! ~250 LOC cached-FFT grid.
//!
//! The decoder already does sub-bin frequency search at `FREQ_OSR = 2`
//! (3.125 Hz sub-bins) + hb-044 parabolic peak refinement. The cheapest
//! direct test of hb-225's *increment* is simply raising `FREQ_OSR` (2 → 4
//! = 1.5625 Hz sub-bins) and measuring the TP/FP delta vs ft8_lib truth.
//! If finer sub-bin resolution surfaces net TPs at acceptable cost, hb-225
//! is justified (and the const bump may itself be the win); if null/FP-
//! dominated, hb-225 drops in priority.
//!
//! This probe decodes a corpus subset at the compiled-in FREQ_OSR and
//! reports hash-normalized TP / FP / miss vs ft8_lib truth. Run it once
//! per FREQ_OSR value (rebuild the decoder between arms — FREQ_OSR is a
//! compile-time const) and diff the headlines.
//!
//! Run:
//!   PANCETTA_OSR_N=50 PANCETTA_OSR_LABEL=osr2 \
//!     cargo run --release -p pancetta-research --example batch102_freq_osr_audit
//!   # then set `const FREQ_OSR: usize = 4;` in decoder.rs, rebuild, re-run
//!   # with PANCETTA_OSR_LABEL=osr4.

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::metrics::hash_normalize_message;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
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

fn load_truth(ws: &Path, sha: &str) -> Vec<String> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    v["decodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["message"].as_str().map(|s| s.to_string()))
        .collect()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let label = std::env::var("PANCETTA_OSR_LABEL").unwrap_or_else(|_| "osr?".to_string());
    let n: usize = std::env::var("PANCETTA_OSR_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let manifest_name = std::env::var("PANCETTA_OSR_MANIFEST")
        .unwrap_or_else(|_| "raw_530_full.manifest.json".to_string());

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8").join(&manifest_name),
    )?)?;
    let entries: Vec<Value> = manifest["entries"].as_array().context("entries")?.clone();
    let take = n.min(entries.len());
    eprintln!(
        "[{label}] {manifest_name}: scoring first {take} of {} entries",
        entries.len()
    );

    let (mut tot, mut tp, mut truth_n, mut found, mut scored) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let t0 = Instant::now();
    for (i, entry) in entries.iter().take(take).enumerate() {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = match load_wav(&expand_tilde(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        scored += 1;
        let truth_raw: Vec<String> = load_truth(&ws, sha);
        let truth_norm: HashSet<String> = truth_raw
            .iter()
            .map(|t| hash_normalize_message(t))
            .collect();

        let mut decoder = Ft8Decoder::new(Ft8Config::default())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        tot += decoded.len();
        let dec_norm: HashSet<String> = decoded
            .iter()
            .map(|d| hash_normalize_message(&d.text))
            .collect();
        for d in &decoded {
            if truth_norm.contains(&hash_normalize_message(&d.text)) {
                tp += 1;
            }
        }
        truth_n += truth_norm.len();
        for t in &truth_norm {
            if dec_norm.contains(t) {
                found += 1;
            }
        }
        if (i + 1) % 50 == 0 {
            eprintln!("  [{label} {}/{take}]", i + 1);
        }
    }
    let fp = tot - tp;
    let prec = tp as f64 / tot.max(1) as f64;
    let miss = 1.0 - found as f64 / truth_n.max(1) as f64;
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "[{label}] scored={scored} decodes={tot} TP={tp} FP={fp} truth={truth_n} \
         prec={prec:.4} found={found} miss={:.2}% wall={wall:.1}s",
        miss * 100.0
    );
    Ok(())
}
