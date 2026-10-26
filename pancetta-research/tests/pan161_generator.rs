use pancetta_research::synth::{generate_crowded_edge, CrowdedEdgeConfig, CrowdedEdgeManifest};
use sha2::{Digest, Sha256};

#[test]
fn crowded_edge_generator_manifest_is_reproducible_and_nonoverlapping() {
    let config = CrowdedEdgeConfig {
        schema_version: 1, generator_version: 1, seed: 161,
        output_dir: "research/corpus/synth/wavs/pan161".into(),
        repetitions: 1, target_snr_db: vec![-16.0], target_dt_s: vec![-0.6],
        distractor_count: 4, distractor_snr_db: -8.0, noise_snr_db: -16.0,
        noise_windows: 1, in_window_controls: 1,
    };
    let first = generate_crowded_edge(&config).unwrap();
    let second = generate_crowded_edge(&config).unwrap();
    let manifest = CrowdedEdgeManifest {
        schema_version: 1, generator_version: 1, config,
        entries: first.iter().map(|(entry, _)| entry.clone()).collect(),
    };
    manifest.validate().unwrap();
    assert_eq!(first, second);
    assert_eq!(serde_json::to_vec(&manifest).unwrap(), serde_json::to_vec(&manifest).unwrap());
    for (entry, bytes) in &first {
        assert_eq!(entry.sha256, format!("{:x}", Sha256::digest(bytes)));
        assert!(entry.peak_before_quantization <= 0.9);
        if entry.kind == "noise" { assert!(entry.signals.is_empty()); continue; }
        assert!(entry.signals.len() >= 4);
        let mut messages = std::collections::HashSet::new();
        for signal in &entry.signals {
            assert!(messages.insert(&signal.message));
            assert!((300.0..=3400.0).contains(&signal.freq_hz));
        }
        for a in &entry.signals {
            for b in &entry.signals {
                if a.message != b.message { assert!((a.freq_hz - b.freq_hz).abs() > 50.0); }
            }
        }
        if entry.kind == "edge" { assert!(entry.signals[0].capture_dt_s < 0.0); }
        if entry.kind == "control" { assert!(entry.signals[0].capture_dt_s >= 0.0); }
    }
}
