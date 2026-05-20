//! End-to-end: gen-synth produces WAVs the decoder recovers correctly at
//! comfortable SNRs. This is a sensitivity sanity check — confirms the
//! signal-gen → decode pipeline works, not a specific sensitivity claim.

#![cfg(feature = "research-eval")]

use pancetta_research::corpus::load_synth_corpus;
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn synth_corpus_decodes_at_comfortable_snr() {
    let workspace = workspace_root();
    let manifest_path = workspace.join("research/corpus/synth/manifests/clean.manifest.json");

    // If the manifest doesn't exist, regenerate it via gen-synth.
    if !manifest_path.exists() {
        let config = workspace.join("research/corpus/synth/manifests/clean.config.json");
        let status = Command::new("cargo")
            .args([
                "run",
                "--release",
                "-q",
                "-p",
                "pancetta-research",
                "--bin",
                "gen-synth",
                "--",
                "--config",
            ])
            .arg(&config)
            .arg("--output")
            .arg(&manifest_path)
            .current_dir(&workspace)
            .status()
            .expect("gen-synth must run");
        assert!(status.success(), "gen-synth failed");
    }

    let entries = load_synth_corpus(&workspace, &manifest_path)
        .expect("manifest must load");
    assert!(!entries.is_empty(), "manifest should have entries");

    // For each comfortable-SNR entry (>= -14 dB), decoder should recover
    // the exact message. This is a sanity gate; real sensitivity numbers
    // come from the eval binary.
    let decoder = Ft8Decoder::with_default_config();
    let mut comfortable_total = 0;
    let mut comfortable_recovered = 0;
    for entry in &entries {
        if entry.snr_db < -14.0 {
            continue;
        }
        comfortable_total += 1;
        let decodes = decoder
            .decode_wav(&entry.wav_path)
            .expect("decode must not error on synth wav");
        if decodes.iter().any(|d| d.message.contains(&entry.encoded_message)) {
            comfortable_recovered += 1;
        }
    }
    assert!(comfortable_total > 0, "should have some comfortable-SNR entries");
    let rate = comfortable_recovered as f64 / comfortable_total as f64;
    println!(
        "comfortable-SNR recovery: {comfortable_recovered}/{comfortable_total} = {rate:.2}"
    );
    assert!(
        rate >= 0.80,
        "decoder should recover ≥80% of comfortable-SNR synth (≥-14 dB), got {rate:.2} ({comfortable_recovered}/{comfortable_total})"
    );
}
