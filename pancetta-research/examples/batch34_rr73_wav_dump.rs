//! Batch 34 / Phase 1B — dump pancetta's full decoded output on a specific
//! RR73-missed WAV vs jt9 truth.
//!
//! Take sha=17c4b25a (contains "WB3FME KB9AVX RR73" at 791 Hz, +14 dB —
//! pancetta missed) and see EXACTLY what pancetta produces AND what jt9
//! truths exist on the same WAV. Look for capture-effect signatures.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch34_rr73_wav_dump

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

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let target_sha: String =
        std::env::var("BATCH34_SHA").unwrap_or_else(|_| "17c4b25a".to_string());

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let entry = entries
        .iter()
        .find(|e| {
            e["wav_sha256"]
                .as_str()
                .unwrap_or("")
                .starts_with(&target_sha)
        })
        .context("sha not in manifest")?;
    let wav_path = entry["wav_path"].as_str().context("wav_path")?;
    let full_sha = entry["wav_sha256"].as_str().context("sha")?;

    println!("## Batch 34 / Phase 1B — full WAV dump");
    println!("  sha: {}", full_sha);
    println!("  wav: {}", wav_path);

    // Load jt9 truth
    let bpath = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", full_sha));
    let bv: Value = serde_json::from_str(&std::fs::read_to_string(&bpath)?)?;
    let truths = bv["decodes"].as_array().context("decodes")?;
    println!("\n### jt9 truth ({} decodes)", truths.len());
    println!("    {:<35} {:>6} {:>6} {:>5}", "text", "freq", "dt", "snr");
    let mut truth_list: Vec<(String, f64, f64, f64)> = Vec::new();
    for t in truths {
        let text = t["message"].as_str().unwrap_or("").to_string();
        let freq = t["freq_hz"].as_f64().unwrap_or(0.0);
        let dt = t["dt_s"].as_f64().unwrap_or(0.0);
        let snr = t["snr_db"].as_f64().unwrap_or(0.0);
        truth_list.push((text, freq, dt, snr));
    }
    truth_list.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    for (text, freq, dt, snr) in &truth_list {
        let mark = if text.ends_with(" RR73") {
            "RR73"
        } else {
            "    "
        };
        println!(
            "  {} {:<35} {:>6.0} {:>+6.2} {:>+5.0}",
            mark,
            text.chars().take(35).collect::<String>(),
            freq,
            dt,
            snr
        );
    }

    let samples = load_wav(Path::new(wav_path))?;
    let cfg = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(cfg).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(&samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

    println!("\n### pancetta decoded ({} messages)", decoded.len());
    println!("    {:<35} {:>6} {:>6} {:>5}", "text", "freq", "dt", "snr");
    let mut p: Vec<(String, f64, f64, f32)> = decoded
        .iter()
        .map(|d| (d.text.clone(), d.frequency_offset, d.time_offset, d.snr_db))
        .collect();
    p.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    for (text, freq, dt, snr) in &p {
        let in_truth = truth_list.iter().any(|t| t.0 == *text);
        let mark = if in_truth { "TP" } else { "FP" };
        println!(
            "  {} {:<35} {:>6.0} {:>+6.2} {:>+5.0}",
            mark,
            text.chars().take(35).collect::<String>(),
            freq,
            dt,
            snr
        );
    }

    // Look for nearby positions: for each RR73 truth, what's within ±15 Hz?
    println!("\n### Near-RR73-position analysis");
    for (text, freq, dt, snr) in &truth_list {
        if !text.ends_with(" RR73") {
            continue;
        }
        println!(
            "\n  truth RR73 → {} @ {:.0} Hz {:.2} s {:+.0} dB",
            text, freq, dt, snr
        );
        for (t2, f2, dt2, snr2) in &truth_list {
            if t2 == text {
                continue;
            }
            if (f2 - freq).abs() <= 15.0 && (dt2 - dt).abs() <= 1.0 {
                println!(
                    "    nearby truth: {:<35} @ {:.0} Hz {:+.2} s {:+.0} dB",
                    t2.chars().take(35).collect::<String>(),
                    f2,
                    dt2,
                    snr2
                );
            }
        }
        for (pt, pf, pdt, psnr) in &p {
            if (pf - freq).abs() <= 15.0 && (pdt - dt).abs() <= 1.0 {
                println!(
                    "    pancetta near: {:<35} @ {:.0} Hz {:+.2} s {:+.0} dB",
                    pt.chars().take(35).collect::<String>(),
                    pf,
                    pdt,
                    psnr
                );
            }
        }
    }

    Ok(())
}
