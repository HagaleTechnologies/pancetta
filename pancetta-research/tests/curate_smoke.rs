//! End-to-end: `curate` binary produces 3 valid manifests when run against
//! a few synth WAVs (the only deterministic corpus we control).
//!
//! Gated on research-eval since it spawns cargo run --bin.

#![cfg(feature = "research-eval")]

use pancetta_research::curated::CuratedManifest;
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn curate_produces_three_manifests() {
    let workspace = workspace_root();
    let source = workspace.join("research/corpus/synth/wavs/clean");
    if !source.exists() {
        // Synth corpus not generated yet. Skip rather than fail — the
        // operator can pre-populate by running gen-synth.
        eprintln!("warn: synth wav dir missing at {}; skipping", source.display());
        return;
    }
    let out_dir = tempfile::tempdir().expect("tempdir");
    let out_prefix = out_dir.path().to_path_buf();

    let status = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "curate",
            "--",
            "--source-dir",
        ])
        .arg(&source)
        .arg("--output-prefix")
        .arg(&out_prefix)
        .arg("--sample-size")
        .arg("30")
        .arg("--seed")
        .arg("42")
        .current_dir(&workspace)
        .status()
        .expect("curate must run");
    assert!(status.success(), "curate failed");

    for label in ["hard_200", "hard_1000", "wild_50"] {
        let path = out_prefix.join(format!("{label}.manifest.json"));
        let manifest = CuratedManifest::load(&path)
            .unwrap_or_else(|e| panic!("manifest {label} must load: {e}"));
        assert_eq!(manifest.label, label);
        assert!(!manifest.entries.is_empty(), "{label} should have entries");
        // Every entry has the required fields.
        for e in &manifest.entries {
            assert!(!e.wav_sha256.is_empty(), "wav_sha256 must be set");
            assert!(e.wav_path.is_absolute(), "wav_path must be absolute");
        }
    }
}
