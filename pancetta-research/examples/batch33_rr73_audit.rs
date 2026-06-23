//! Batch 33 / Diagnostic DD probe — what does pancetta emit at RR73 truth positions?
//!
//! For every hard-200 WAV's RR73 truth, find pancetta's emission(s) within
//! ±5 Hz / ±0.3 s. Reveals whether pancetta:
//!   (a) decodes the message but with a different text (format bug),
//!   (b) emits nothing (sync failure / coverage gap), or
//!   (c) catches it (the 2/503).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch33_rr73_audit

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone)]
struct TruthDecode {
    text: String,
    freq_hz: f64,
    dt_s: f64,
    snr_db: f64,
}

fn load_baseline(ws: &Path, sha: &str) -> Vec<TruthDecode> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return Vec::new();
    };
    let Some(arr) = v["decodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            Some(TruthDecode {
                text: d["message"].as_str()?.to_string(),
                freq_hz: d["freq_hz"].as_f64()?,
                dt_s: d["dt_s"].as_f64()?,
                snr_db: d["snr_db"].as_f64().unwrap_or(0.0),
            })
        })
        .collect()
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH33_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    println!("## Batch 33 — RR73 emission audit");

    let cfg = Ft8Config::default();
    let mut total_rr73_truths = 0usize;
    let mut exact_match = 0usize;
    let mut nearby_other_emit = 0usize;
    let mut no_emit = 0usize;
    let mut printed = 0usize;

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let truth = load_baseline(&ws, sha);
        let rr73_truths: Vec<&TruthDecode> =
            truth.iter().filter(|t| t.text.ends_with(" RR73")).collect();
        if rr73_truths.is_empty() {
            continue;
        }
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        for t in &rr73_truths {
            total_rr73_truths += 1;
            // Check exact text match
            let exact = decoded.iter().find(|d| d.text == t.text);
            if exact.is_some() {
                exact_match += 1;
                continue;
            }
            // Check nearby in position (±5 Hz, ±0.3 s)
            let nearby: Vec<&pancetta_ft8::DecodedMessage> = decoded
                .iter()
                .filter(|d| {
                    (d.frequency_offset - t.freq_hz).abs() <= 5.0
                        && (d.time_offset - t.dt_s).abs() <= 0.3
                })
                .collect();

            if !nearby.is_empty() {
                nearby_other_emit += 1;
                if printed < 20 {
                    println!(
                        "  {}  truth:  {:<35} ({:.0} Hz, {:.2} s, {:+.0} dB)",
                        &sha[..8],
                        t.text,
                        t.freq_hz,
                        t.dt_s,
                        t.snr_db
                    );
                    for d in &nearby {
                        println!(
                            "             pancetta: {:<35} ({:.0} Hz, {:.2} s, {:+.0} dB)",
                            d.text, d.frequency_offset, d.time_offset, d.snr_db
                        );
                    }
                    println!();
                    printed += 1;
                }
            } else {
                no_emit += 1;
            }
        }
    }

    println!("\n### Summary");
    println!("  Total RR73 truths: {}", total_rr73_truths);
    println!(
        "  Exact text match: {} ({:.1}%)",
        exact_match,
        exact_match as f64 / total_rr73_truths.max(1) as f64 * 100.0
    );
    println!(
        "  Nearby-but-different text: {} ({:.1}%)",
        nearby_other_emit,
        nearby_other_emit as f64 / total_rr73_truths.max(1) as f64 * 100.0
    );
    println!(
        "  NO nearby emission: {} ({:.1}%)",
        no_emit,
        no_emit as f64 / total_rr73_truths.max(1) as f64 * 100.0
    );

    // Also: total RR73 emissions pancetta produced (anywhere)
    println!("\n### Total RR73 emissions across hard-200 (pancetta vs jt9)");
    let mut total_pancetta_rr73 = 0usize;
    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        for d in &decoded {
            if d.text.ends_with(" RR73") {
                total_pancetta_rr73 += 1;
            }
        }
    }
    println!(
        "  pancetta emits ' RR73'-suffix messages: {}",
        total_pancetta_rr73
    );
    println!(
        "  jt9 truth has ' RR73'-suffix messages:  {}",
        total_rr73_truths
    );

    Ok(())
}
