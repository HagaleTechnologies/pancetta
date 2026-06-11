//! Batch 71 — generate ft8_lib FFI truth labels for an entire directory of WAVs.
//!
//! Unlike `batch66_ft8lib_truth` which works from a manifest, this
//! walks a directory glob and labels every matching WAV. Used to
//! produce comprehensive ft8_lib truth coverage of all 25k recordings
//! at `~/.pancetta/recordings/`.
//!
//! Each existing file is skipped (idempotent — safe to re-run).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch71_ft8lib_truth_all -- \
//!     --dir /Users/thagale/.pancetta/recordings

use anyhow::{Context, Result};
use pancetta_ft8::Ft8Decoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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

fn sha256_of(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut dir: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                dir = Some(args[i + 1].clone());
                i += 2;
            }
            "--filter" => {
                filter = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }
    let dir = dir.context("--dir required")?;
    let ws = workspace_root()?;
    let baselines = ws.join("research/baselines/ft8");
    std::fs::create_dir_all(&baselines)?;

    let mut wavs: Vec<PathBuf> = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("wav") {
            continue;
        }
        if let Some(f) = &filter {
            if !p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .contains(f)
            {
                continue;
            }
        }
        wavs.push(p);
    }
    wavs.sort();
    eprintln!("found {} WAVs to label", wavs.len());

    let t0 = std::time::Instant::now();
    let mut new_files = 0usize;
    let mut total_decodes = 0usize;
    let mut skipped = 0usize;
    for (i, path) in wavs.iter().enumerate() {
        // Compute sha. We must SHA the file regardless of whether the
        // label exists, because we need the sha to know the label name.
        let sha = match sha256_of(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let out = baselines.join(format!("{sha}.ft8lib.json"));
        if out.exists() {
            skipped += 1;
            if (i + 1) % 500 == 0 {
                eprintln!(
                    "  [{}/{}] new={new_files} skipped={skipped} decodes={total_decodes} ({:.0}s)",
                    i + 1,
                    wavs.len(),
                    t0.elapsed().as_secs_f64()
                );
            }
            continue;
        }
        let samples = match load_wav(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let decoded = Ft8Decoder::decode_window_ft8lib(&samples);
        total_decodes += decoded.len();
        let dec_array: Vec<Value> = decoded
            .iter()
            .map(|d| {
                json!({
                    "message": d.text,
                    "snr_db": d.snr_db,
                    "freq_hz": d.frequency_offset,
                })
            })
            .collect();
        let baseline = json!({
            "schema_version": 1,
            "source": "ft8_lib_ffi",
            "wav_path": path.to_string_lossy(),
            "wav_sha256": sha,
            "decodes": dec_array,
        });
        std::fs::write(&out, serde_json::to_string(&baseline)?)?;
        new_files += 1;
        if (i + 1) % 500 == 0 {
            eprintln!(
                "  [{}/{}] new={new_files} skipped={skipped} decodes={total_decodes} ({:.0}s)",
                i + 1,
                wavs.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "done in {:.1}s — wrote {new_files} new files, skipped {skipped} existing, {total_decodes} ft8_lib decodes",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
