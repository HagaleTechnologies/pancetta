//! Batch 28 / Diagnostic E — Decoder determinism sanity check.
//!
//! Run the decoder 5 times on the same WAV with a fresh `Ft8Decoder`
//! per run, and assert byte-identical decode sets. Catches stale-RNG,
//! parallelism-induced races, or accidental nondeterminism that would
//! invalidate every diagnostic in this batch.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch28_determinism

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

fn decode_set(samples: &[f32], cfg: &Ft8Config) -> Result<HashSet<String>> {
    let mut decoder =
        Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
    let decoded = decoder
        .decode_window(samples)
        .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
    Ok(decoded.into_iter().map(|d| d.text).collect())
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("entries")?;
    let top_n: usize = std::env::var("BATCH28_DET_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let runs_per_wav: usize = 5;

    println!("## Batch 28 / Diagnostic E — decoder determinism");
    println!("  {} WAVs × {} runs each", top_n, runs_per_wav);

    let cfg = Ft8Config::default();
    let mut all_match = true;

    for entry in entries.iter().take(top_n) {
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let samples = load_wav(Path::new(wav_path))?;

        let baseline = decode_set(&samples, &cfg)?;
        let mut all_runs_match = true;
        for _ in 1..runs_per_wav {
            let s = decode_set(&samples, &cfg)?;
            if s != baseline {
                all_runs_match = false;
                let unique_to_baseline = baseline.difference(&s).count();
                let unique_to_run = s.difference(&baseline).count();
                println!(
                    "  WAV {}: MISMATCH (baseline-only={}, run-only={})",
                    &sha[..8],
                    unique_to_baseline,
                    unique_to_run
                );
                all_match = false;
            }
        }
        if all_runs_match {
            println!(
                "  WAV {} ({} decodes): all {} runs identical",
                &sha[..8],
                baseline.len(),
                runs_per_wav
            );
        }
    }

    println!();
    if all_match {
        println!("## Verdict: DETERMINISTIC — every WAV produced byte-identical decode sets across all runs");
        println!("    All batch 28 hypothesis tests are reproducible.");
    } else {
        println!("## Verdict: NON-DETERMINISTIC — at least one WAV varies across runs");
        println!("    Invalidates the precision of all batch 28 verdicts; investigate decoder RNG / parallelism.");
    }

    Ok(())
}
