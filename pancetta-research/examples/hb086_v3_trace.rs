//! hb086_v3_trace — instrumented run of V3 on 3 top-20 hard-200 WAVs.
//!
//! Decodes 3 of the worst WAVs with V3 enabled at relax_db=-2.0 and the
//! debug instrumentation in `joint_residual_localized_sync_pass` turned
//! on (via env var). Reports per-WAV V3 funnel counts so we can see if
//! the mechanism is (a) finding zero candidates, (b) finding candidates
//! that all collide with existing, or (c) finding new candidates but
//! they fail LDPC/CRC/plausibility.
//!
//! Run: PANCETTA_HB086_V3_DEBUG=1 cargo run --release \
//!      -p pancetta-research --example hb086_v3_trace

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
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

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;
    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(3)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    for &relax_db in &[-0.5_f64, -1.0, -1.5, -2.0] {
        eprintln!("\n#### relax_db = {} ####", relax_db);
        let mut cfg = Ft8Config::default();
        cfg.joint_residual_sync_relax_db = relax_db;
        cfg.joint_residual_sync_window_bins = 8;

        for sha in &top_hashes {
            let Some(p) = path_by_sha.get(sha) else {
                continue;
            };
            let samples = load_wav(&PathBuf::from(p))?;
            let mut decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            eprintln!("=== WAV {} ===", &sha[..8]);
            let decoded = decoder
                .decode_window(&samples)
                .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
            eprintln!("  total decoded: {}", decoded.len());
        }
    }
    Ok(())
}
