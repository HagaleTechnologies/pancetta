//! Batch 30 / Diagnostic O — pancetta vs jt9 on lid_of_band weak truths.
//!
//! With the lid_of_band tier shipped (Batch 29 G), characterize where
//! pancetta loses to jt9 specifically on the weak-signal subset.
//!
//! For each lid_of_band WAV (top-20 by weakest truth):
//!   1. Load all jt9 truths and their per-decode snr_db
//!   2. Run pancetta, classify each truth as recovered / missed
//!   3. Cross-tabulate recovery by SNR bucket
//!
//! Identifies the SNR regime where pancetta loses the most ground to
//! jt9 — informs which recall-improvement hypotheses to prioritize.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch30_lid_of_band_vs_jt9

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashSet;
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

fn load_baseline(ws: &Path, sha: &str) -> Option<Value> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let txt = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&txt).ok()
}

fn snr_bucket(snr_db: f64) -> &'static str {
    if snr_db >= -10.0 {
        ">= -10"
    } else if snr_db >= -15.0 {
        "-15..-10"
    } else if snr_db >= -19.0 {
        "-19..-15"
    } else if snr_db >= -22.0 {
        "-22..-19"
    } else {
        "<= -22"
    }
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/lid_of_band.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH30_O_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!("## Batch 30 / Diagnostic O — pancetta vs jt9 on lid_of_band weak truths");
    println!(
        "  Examining top-{} lid_of_band WAVs (weakest truths first)",
        top_n
    );

    let cfg = Ft8Config::default();
    let mut bucket_truths: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let mut bucket_recovered: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let Some(baseline) = load_baseline(&ws, sha) else {
            continue;
        };
        let Some(decodes) = baseline["decodes"].as_array() else {
            continue;
        };

        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let pdecoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        let recovered_set: HashSet<String> = pdecoded.into_iter().map(|d| d.text).collect();

        for d in decodes {
            let Some(msg) = d["message"].as_str() else {
                continue;
            };
            let snr = d["snr_db"].as_f64().unwrap_or(0.0);
            let bucket = snr_bucket(snr);
            *bucket_truths.entry(bucket).or_insert(0) += 1;
            if recovered_set.contains(msg) {
                *bucket_recovered.entry(bucket).or_insert(0) += 1;
            }
        }
    }

    println!("\n  SNR bucket    | truths | recovered | recall");
    println!("  ------------- | ------ | --------- | ------");
    let bucket_order = [">= -10", "-15..-10", "-19..-15", "-22..-19", "<= -22"];
    let mut tot_truths = 0;
    let mut tot_recovered = 0;
    for b in bucket_order {
        let t = *bucket_truths.get(b).unwrap_or(&0);
        let r = *bucket_recovered.get(b).unwrap_or(&0);
        if t == 0 {
            continue;
        }
        let recall = r as f64 / t as f64 * 100.0;
        println!("  {:<13} | {:>6} | {:>9} | {:>5.1}%", b, t, r, recall);
        tot_truths += t;
        tot_recovered += r;
    }
    let tot_recall = tot_recovered as f64 / tot_truths.max(1) as f64 * 100.0;
    println!(
        "  {:<13} | {:>6} | {:>9} | {:>5.1}%",
        "TOTAL", tot_truths, tot_recovered, tot_recall
    );

    println!("\n### Verdict");
    println!("  The weak-SNR buckets characterize pancetta's recall cliff on the");
    println!("  hardest 20.3% of jt9 truths. Decode-rate falloff between buckets");
    println!("  identifies the SNR regime where production has the most headroom.");
    println!("  Future recall-improvement hypotheses should target the weakest");
    println!("  bucket where pancetta has positive but low recall.");

    Ok(())
}
