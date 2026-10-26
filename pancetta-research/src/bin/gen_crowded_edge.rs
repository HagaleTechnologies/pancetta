//! Deterministic PAN-161 multi-signal, temporal-edge corpus generator.
use anyhow::{Context, Result};
use pancetta_research::synth::{generate_crowded_edge, CrowdedEdgeConfig, CrowdedEdgeManifest};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut config_path = None;
    let mut manifest_path = None;
    let mut output_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = Some(PathBuf::from(args.next().context("--config value missing")?)),
            "--output-manifest" => manifest_path = Some(PathBuf::from(args.next().context("--output-manifest value missing")?)),
            "--output-dir" => output_dir = Some(PathBuf::from(args.next().context("--output-dir value missing")?)),
            _ => anyhow::bail!("unknown argument: {arg}"),
        }
    }
    let config_path = config_path.context("--config required")?;
    let manifest_path = manifest_path.context("--output-manifest required")?;
    let config: CrowdedEdgeConfig = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    let entries_and_bytes = generate_crowded_edge(&config)?;
    let manifest = CrowdedEdgeManifest {
        schema_version: CrowdedEdgeManifest::VERSION,
        generator_version: config.generator_version,
        config,
        entries: entries_and_bytes.iter().map(|(entry, _)| entry.clone()).collect(),
    };
    manifest.validate()?;
    let canonical = serde_json::to_vec_pretty(&manifest)?;
    if manifest_path.exists() {
        anyhow::ensure!(std::fs::read(&manifest_path)? == canonical, "existing manifest differs: {}", manifest_path.display());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    for (entry, bytes) in &entries_and_bytes {
        let path = match &output_dir {
            Some(dir) => dir.join(entry.wav_path.file_name().unwrap()),
            None => root.join(&entry.wav_path),
        };
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    }
    if !manifest_path.exists() {
        if let Some(parent) = manifest_path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&manifest_path, canonical)?;
    }
    println!("generated {} WAVs; manifest {}", manifest.entries.len(), manifest_path.display());
    Ok(())
}
