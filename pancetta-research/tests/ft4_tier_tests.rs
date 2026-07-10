//! End-to-end: the `synth-ft4` eval tier decodes a small generated FT4
//! synth corpus and populates the SNR-bin sensitivity curve.
//!
//! Workstream 0, Task W0.4 (2026-07-07) — FT4 evaluation tier. FT4 decode
//! sensitivity had never been measured before this task (only encode
//! round-trips existed). This test generates a tiny FT4 corpus (a
//! handful of messages x SNR steps, not the full 550-file production
//! corpus) directly via the library's protocol-generic synth functions
//! (`modulate_message_at_protocol` + `add_awgn_2500hz_ref`, the SAME
//! noise-scaling formula Task W0.2 calibrated for FT8 — reused verbatim,
//! not reimplemented), then runs the real `eval` binary's `synth-ft4`
//! tier over it via a `--synth-ft4-manifest` override (mirrors the
//! `--noise-manifest` pattern in `noise_tier_tests.rs`).
//!
//! Gated behind `--features research-eval` because it spawns `cargo run
//! --release` and rebuilds the eval binary — slow + side-effecting test,
//! matching the convention in `eval_fixtures.rs` / `noise_tier_tests.rs`.

#![cfg(feature = "research-eval")]

use pancetta_ft8::ProtocolParams;
use pancetta_research::scorecard::Scorecard;
use pancetta_research::synth::{
    add_awgn_2500hz_ref, modulate_message_at_protocol, place_in_slot, signal_rms, SynthChannel,
    SynthConfig, SynthEntry, SynthManifest,
};
use pancetta_research::Mode;
use std::process::Command;

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Build a tiny FT4 synth corpus (2 messages x 2 SNR steps = 4 WAVs) and
/// save its manifest to `dir`. Returns the manifest path.
///
/// Mirrors `bin/gen_synth.rs`'s FT4 path (protocol params, slot geometry,
/// 2500 Hz AWGN convention) at a much smaller scale so the test is fast.
fn generate_tiny_ft4_corpus(dir: &std::path::Path) -> std::path::PathBuf {
    const LEAD_IN_S: f64 = 0.5;
    const SLOT_LEN_SAMPLES: usize = 90_000; // 7.5s @ 12kHz, matches gen_synth.rs's FT4 slot.
    let params = ProtocolParams::ft4();

    let messages = ["CQ K1AAA FN42", "CQ K1AAB EN91"];
    // -5 dB is far above the FT4 sensitivity threshold measured in the
    // real W0.4 experiment (jt9 50%-recovery ~-18.5 dB, pancetta
    // ~-14.5 dB) — used as a "must decode" sanity check immune to the
    // per-message statistical variance seen right at the recall
    // boundary. -20 dB is comfortably below threshold (0% expected).
    let snr_steps_db: [f64; 2] = [-5.0, -20.0];
    let seed: u64 = 20260707;

    let output_dir = dir.join("wavs");
    std::fs::create_dir_all(&output_dir).unwrap();

    let mut entries = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        for snr_db in &snr_steps_db {
            let seed_for_this_wav = seed
                .wrapping_add(msg_idx as u64)
                .wrapping_add(snr_db.to_bits());
            let base_freq_hz = 1500.0;
            let dt_s = 0.0;

            let base_signal =
                modulate_message_at_protocol(msg, base_freq_hz, &params).expect("modulate");
            let rms = signal_rms(&base_signal);
            let mut samples = place_in_slot(&base_signal, dt_s, LEAD_IN_S, SLOT_LEN_SAMPLES);
            add_awgn_2500hz_ref(&mut samples, rms, *snr_db, seed_for_this_wav);

            let slug: String = msg
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let filename = format!("{slug}__{:+.1}dB.wav", snr_db);
            let wav_path = output_dir.join(&filename);
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 12_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut w = hound::WavWriter::create(&wav_path, spec).unwrap();
            for &s in &samples {
                let clamped = s.clamp(-1.0, 1.0);
                w.write_sample((clamped * 32767.0) as i16).unwrap();
            }
            w.finalize().unwrap();

            entries.push(SynthEntry {
                // NOTE: `load_synth_corpus` resolves this via
                // `workspace_root.join(&e.wav_path)` — since this test's
                // WAVs live in a tempdir OUTSIDE the real workspace root,
                // this must be the ABSOLUTE path (`Path::join` on an
                // absolute path discards the base and returns the
                // absolute path itself), not a workspace-relative one
                // like production manifests use.
                wav_path: wav_path.clone(),
                encoded_message: msg.to_string(),
                snr_db: *snr_db,
                channel: SynthChannel::Awgn,
                drift_hz_per_sec: 0.0,
                seed_for_this_wav,
                base_freq_hz,
                dt_s,
            });
        }
    }

    let config = SynthConfig {
        schema_version: SynthConfig::CURRENT_SCHEMA_VERSION,
        label: "ft4_test_tiny".to_string(),
        messages: messages.iter().map(|s| s.to_string()).collect(),
        snr_steps_db: snr_steps_db.to_vec(),
        channel: SynthChannel::Awgn,
        drift_steps_hz_per_sec: Vec::new(),
        seed,
        output_dir: dir.to_path_buf(),
        mode: Mode::Ft4,
    };
    let manifest = SynthManifest {
        schema_version: SynthManifest::CURRENT_SCHEMA_VERSION,
        config,
        entries,
    };
    let manifest_path = dir.join("ft4_test_tiny.manifest.json");
    manifest.save(&manifest_path).unwrap();
    manifest_path
}

#[test]
fn synth_ft4_tier_populates_snr_curve_over_tiny_corpus() {
    let workspace = workspace_root();
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = generate_tiny_ft4_corpus(dir.path());

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
            "synth-ft4",
            "--mode",
            "ft4",
            "--synth-ft4-manifest",
        ])
        .arg(&manifest_path)
        .arg("--output")
        .arg(scorecard_out.path())
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn eval");
    assert!(status.success(), "eval binary failed on synth-ft4 tier");

    let card = Scorecard::load(scorecard_out.path()).expect("scorecard must be loadable");
    let tier = card
        .tiers
        .get("synth-ft4")
        .expect("synth-ft4 tier must be present in the scorecard");
    assert_eq!(tier.wavs_processed, 4, "tier must process all 4 WAVs");
    assert!(
        !tier.by_snr_db.is_empty(),
        "by_snr_db must be populated with at least one SNR bin"
    );
    // -5 dB is far above the measured FT4 recall threshold; the tier
    // should recover it (this is the harness's own sanity check that FT4
    // decoding isn't fundamentally broken, not a production SNR-threshold
    // assertion — see the W0.4 experiment log for the real curve).
    let strong_bin = tier
        .by_snr_db
        .iter()
        .find(|b| (b.snr_db - -5.0).abs() < 0.01)
        .expect("-5 dB bin must be present");
    assert!(
        strong_bin.decoded > 0,
        "pancetta must decode at least one -5 dB FT4 WAV (clean, strong signal)"
    );
}

/// Requiring `--mode ft4` for the `synth-ft4` tier is deliberate: the
/// tier decodes FT4 audio, and without `Protocol::Ft4` the wrapped
/// decoder would demodulate it as FT8 (wrong symbol/tone geometry).
#[test]
fn synth_ft4_tier_refuses_wrong_mode() {
    let workspace = workspace_root();
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = generate_tiny_ft4_corpus(dir.path());

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
            "synth-ft4",
            "--mode",
            "ft8",
            "--synth-ft4-manifest",
        ])
        .arg(&manifest_path)
        .arg("--output")
        .arg(scorecard_out.path())
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn eval");
    assert!(
        !status.success(),
        "eval binary must refuse synth-ft4 tier under --mode ft8"
    );
}
