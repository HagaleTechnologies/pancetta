//! Batch 66 — generate ft8_lib-FFI truth labels for a manifest of WAVs.
//!
//! For each WAV in the input manifest, runs `Ft8Decoder::decode_window_ft8lib`
//! (the WSJT-X mainline reference decoder via FFI) and writes the
//! resulting decoded-message texts to a baseline JSON file at
//! `research/baselines/ft8/<sha256>.ft8lib.json`.
//!
//! The output files share the schema of the existing
//! `research/baselines/ft8/<sha>.json` truth files. Consumers can
//! choose pancetta-truth or ft8lib-truth by switching the path.
//!
//! Why ft8_lib truth?
//!   Pancetta-derived truth excludes any decode pancetta-without-flag-X
//!   misses, which biases the measurement against flag-X. ft8_lib is
//!   developed independently of pancetta's research direction, so its
//!   decodes are a neutral reference. The ft8_lib + pancetta UNION is
//!   even better — anything either decoder produces is a candidate
//!   real signal — but a pure ft8_lib pass is the natural starting
//!   point and cheap to produce.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch66_ft8lib_truth -- \
//!     --manifest research/corpus/curated/ft8/repeat_heavy_530.manifest.json

use anyhow::{Context, Result};
use pancetta_ft8::Ft8Decoder;
use serde_json::{json, Value};
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
    let args: Vec<String> = std::env::args().collect();
    let mut manifest: Option<String> = None;
    let mut suffix: String = "ft8lib".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                manifest = Some(args[i + 1].clone());
                i += 2;
            }
            "--suffix" => {
                suffix = args[i + 1].clone();
                i += 2;
            }
            _ => i += 1,
        }
    }
    let manifest = manifest.context("--manifest required")?;

    let ws = workspace_root()?;
    let manifest_path = if manifest.starts_with('/') {
        PathBuf::from(&manifest)
    } else {
        ws.join(&manifest)
    };
    let m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    let entries: Vec<&Value> = m["entries"]
        .as_array()
        .context("manifest has no entries")?
        .iter()
        .collect();

    let baselines_dir = ws.join("research/baselines/ft8");
    std::fs::create_dir_all(&baselines_dir)?;

    eprintln!("ft8_lib truth labeling: {} entries", entries.len());
    let t0 = std::time::Instant::now();
    let mut total_decodes = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        let wav_path = entry["wav_path"]
            .as_str()
            .context("entry missing wav_path")?;
        let sha = entry["wav_sha256"]
            .as_str()
            .context("entry missing wav_sha256")?;
        let out_path = baselines_dir.join(format!("{}.{}.json", sha, suffix));
        if out_path.exists() {
            continue;
        }
        let samples = match load_wav(Path::new(wav_path)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  skip {wav_path}: {e}");
                continue;
            }
        };
        let decoded = Ft8Decoder::decode_window_ft8lib(&samples);
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
            "wav_path": wav_path,
            "wav_sha256": sha,
            "decodes": dec_array,
        });
        std::fs::write(&out_path, serde_json::to_string(&baseline)?)?;
        total_decodes += decoded.len();
        if (i + 1) % 100 == 0 {
            eprintln!(
                "  [{}/{}] {:.1}s elapsed, {total_decodes} total decodes",
                i + 1,
                entries.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "done in {:.1}s — {total_decodes} ft8_lib decodes across {} slots",
        t0.elapsed().as_secs_f64(),
        entries.len()
    );
    Ok(())
}
