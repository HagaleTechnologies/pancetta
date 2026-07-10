//! End-to-end: the `noise_1000` eval tier decodes a small generated noise
//! corpus and populates `false_positives_total` / `noise_files_decoded`.
//!
//! Workstream 0 (2026-07-06) — the FP-on-noise measurement guardrail.
//! Every WAV in this corpus is pure noise (+ optional birdie); it
//! contains no FT8 signal, so ANY decode the tier reports is a false
//! positive. This test runs the tier over 5 freshly generated WAVs (not
//! the full 1000-file production corpus) via a `--noise-manifest`
//! override, and asserts the scorecard fields the tier is supposed to
//! populate actually show up (not that they're zero — a real regression
//! that made the decoder hallucinate should still show up as Some(n),
//! not vanish).
//!
//! Gated behind `--features research-eval` because it spawns `cargo run
//! --release` and rebuilds the eval binary — slow + side-effecting test,
//! matching the convention in `eval_fixtures.rs` / `synth_roundtrip.rs`.

#![cfg(feature = "research-eval")]

use pancetta_research::gen_noise::{generate_noise_corpus_with_manifest, NoiseConfig};
use pancetta_research::scorecard::Scorecard;
use std::process::Command;

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn noise_tier_populates_fp_fields_over_five_wavs() {
    let workspace = workspace_root();
    let dir = tempfile::tempdir().unwrap();
    let config = NoiseConfig {
        count: 5,
        seed: 4242,
        birdie_fraction: 0.4,
    };
    let manifest = generate_noise_corpus_with_manifest(dir.path(), config, "noise_test_tiny")
        .expect("corpus generation must succeed");
    assert_eq!(manifest.entries.len(), 5);
    let manifest_path = dir.path().join("noise_test_tiny.manifest.json");
    manifest.save(&manifest_path).unwrap();

    let scorecard_out = tempfile::NamedTempFile::new().unwrap();
    let status = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "eval",
            "--",
            "--tier",
            "noise_1000",
            "--mode",
            "ft8",
            "--noise-manifest",
        ])
        .arg(&manifest_path)
        .arg("--output")
        .arg(scorecard_out.path())
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn eval");
    assert!(status.success(), "eval binary failed on noise_1000 tier");

    let card = Scorecard::load(scorecard_out.path()).expect("scorecard must be loadable");
    let tier = card
        .tiers
        .get("noise_1000")
        .expect("noise_1000 tier must be present in the scorecard");
    assert_eq!(
        tier.wavs_processed, 5,
        "tier must process all 5 generated WAVs"
    );
    assert!(
        tier.false_positives_total.is_some(),
        "false_positives_total must be populated (Some), even if 0"
    );
    assert!(
        tier.noise_files_decoded.is_some(),
        "noise_files_decoded must be populated (Some), even if 0"
    );
}
