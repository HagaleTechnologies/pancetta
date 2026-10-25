//! Exercises the research-only split binary with explicitly synthetic jt9 fixtures.
#![cfg(feature = "research-eval")]

use pancetta_ft8::ProtocolParams;
use pancetta_research::candidate_split::{CandidateSplitScorecard, MissLabel};
use pancetta_research::synth::{
    add_awgn_2500hz_ref, modulate_message_at_protocol, place_in_slot, signal_rms, SynthChannel,
    SynthConfig, SynthEntry, SynthManifest,
};
use pancetta_research::Mode;
use sha2::{Digest, Sha256};
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
    let snr_steps_db: [f64; 2] = [-5.0, -24.0];
    let seed: u64 = 20260707;

    let output_dir = dir.join("wavs");
    std::fs::create_dir_all(&output_dir).unwrap();

    let mut entries = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        for snr_db in &snr_steps_db {
            let seed_for_this_wav = seed
                .wrapping_add(msg_idx as u64)
                .wrapping_add(snr_db.to_bits());
            let base_freq_hz = 1000.0;
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

fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let manifest = generate_tiny_ft4_corpus(dir);
    let m = SynthManifest::load(&manifest).unwrap();
    std::fs::create_dir_all(dir.join("baselines")).unwrap();
    for e in m.entries {
        let sha = format!("{:x}", Sha256::digest(std::fs::read(&e.wav_path).unwrap()));
        let cache = serde_json::json!({"schema_version":1,"wav_path":e.wav_path,"wav_sha256":sha,
            "decoder_identity":"test fixture (not jt9)","elapsed_seconds":0.0,
            "decodes":[{"message":e.encoded_message,"freq_hz":e.base_freq_hz,"dt_s":e.dt_s,"snr_db":e.snr_db}]});
        std::fs::write(
            dir.join("baselines").join(format!("{sha}.json")),
            cache.to_string(),
        )
        .unwrap();
    }
    manifest
}
fn run(dir: &std::path::Path, manifest: &std::path::Path, extra: &[&str]) -> std::process::Output {
    Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "ft4-candidate-split",
            "--",
            "--manifest",
        ])
        .arg(manifest)
        .arg("--baseline-dir")
        .arg(dir.join("baselines"))
        .arg("--output")
        .arg(dir.join("split.json"))
        .args(extra)
        .current_dir(workspace_root())
        .output()
        .unwrap()
}
#[test]
fn split_labels_every_jt9_truth_miss_on_tiny_corpus() {
    let d = tempfile::tempdir().unwrap();
    let m = fixture(d.path());
    let out = run(d.path(), &m, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = CandidateSplitScorecard::load(&d.path().join("split.json")).unwrap();
    assert_eq!(s.totals.wavs, 4);
    assert_eq!(s.totals.jt9_truths, 4);
    assert!(s.totals.pancetta_recovered_jt9_truths >= 1);
    assert!(s.totals.jt9_truth_misses >= 1);
    assert_eq!(
        s.totals.no_candidate_produced + s.totals.candidate_produced_decode_failed,
        s.totals.jt9_truth_misses
    );
    for m in &s.misses {
        let label = serde_json::to_value(m.label).unwrap();
        assert!(
            ["no candidate produced", "candidate produced, decode failed"]
                .contains(&label.as_str().unwrap())
        );
    }
    assert!(String::from_utf8_lossy(&out.stdout).contains("TOTAL jt9-truth misses:"));
}
#[test]
fn split_rechecks_only_no_candidate_misses_when_asked() {
    let d = tempfile::tempdir().unwrap();
    let m = fixture(d.path());
    let out = run(d.path(), &m, &["--cap-lifted-recheck", "400"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = CandidateSplitScorecard::load(&d.path().join("split.json")).unwrap();
    assert!(s.totals.no_candidate_produced > 0);
    for m in s.misses {
        assert_eq!(
            m.cap_lifted.is_some(),
            m.label == MissLabel::NoCandidateProduced
        );
    }
}
#[test]
fn split_refuses_when_a_jt9_baseline_is_missing() {
    let d = tempfile::tempdir().unwrap();
    let m = fixture(d.path());
    let p = std::fs::read_dir(d.path().join("baselines"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(p).unwrap();
    let out = run(d.path(), &m, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("jt9 baseline"));
    assert!(!d.path().join("split.json").exists());
}
#[test]
fn split_refuses_a_non_ft4_manifest() {
    let d = tempfile::tempdir().unwrap();
    let m = fixture(d.path());
    let mut manifest = SynthManifest::load(&m).unwrap();
    manifest.config.mode = Mode::Ft8;
    manifest.save(&m).unwrap();
    let out = run(d.path(), &m, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("FT4"));
    assert!(!d.path().join("split.json").exists());
}
