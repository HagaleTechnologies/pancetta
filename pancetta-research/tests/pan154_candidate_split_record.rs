//! Committed evidence for PAN-154. No corpus WAVs or external jt9 installation needed.
use pancetta_research::candidate_split::CandidateSplitScorecard;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}
fn card() -> CandidateSplitScorecard {
    CandidateSplitScorecard::load(
        &root().join("research/scorecards/pan154-ft4-candidate-split.json"),
    )
    .unwrap()
}
fn json(path: &str) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root().join(path)).unwrap()).unwrap()
}
#[test]
fn every_jt9_truth_miss_is_labeled() {
    let s = card();
    s.check_invariants().unwrap();
    assert!(s.totals.jt9_truth_misses > 0);
    assert_eq!(s.misses.len(), s.totals.jt9_truth_misses);
    for m in s.misses {
        let label = serde_json::to_value(m.label).unwrap();
        assert!(
            ["no candidate produced", "candidate produced, decode failed"]
                .contains(&label.as_str().unwrap())
        );
    }
    assert!(
        (s.totals.no_candidate_produced_pct.unwrap()
            + s.totals.candidate_produced_decode_failed_pct.unwrap()
            - 100.0)
            .abs()
            < 1e-9
    );
}
#[test]
fn counts_match_the_w04_benchmark() {
    let s = card();
    let reference = json("research/scorecards/w04_ft4_scorecard.json");
    let tier = &reference["tiers"]["synth-ft4"];
    assert_eq!(s.totals.wavs, 550);
    assert_eq!(
        s.by_snr_db.len(),
        tier["by_snr_db"].as_array().unwrap().len()
    );
    for b in s.by_snr_db {
        let lookup = |key: &str| {
            tier[key]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["snr_db"].as_f64().unwrap() == b.snr_db)
                .unwrap()["decoded"]
                .as_u64()
                .unwrap() as usize
        };
        assert_eq!(
            b.counts.jt9_truths,
            lookup("jt9_snr_curve"),
            "jt9 bin {}",
            b.snr_db
        );
        assert_eq!(
            b.counts.pancetta_recovered,
            lookup("by_snr_db"),
            "native bin {}",
            b.snr_db
        );
    }
}
#[test]
fn corpus_is_the_committed_ft4_tier() {
    let s = card();
    let manifest = "research/corpus/synth/manifests/ft4_clean.manifest.json";
    assert_eq!(s.corpus.manifest, manifest);
    assert_eq!(
        s.corpus.manifest_sha256,
        format!(
            "{:x}",
            Sha256::digest(std::fs::read(root().join(manifest)).unwrap())
        )
    );
    let prior = json("research/scorecards/pan110-synth-ft4-max-merged.json");
    assert_eq!(
        s.corpus.wav_set_sha256,
        prior["provenance"]["corpus"]["wav_set_sha256"]
            .as_str()
            .unwrap()
    );
}
#[test]
fn decoder_and_truth_provenance_are_recorded() {
    let s = card();
    assert_eq!(s.decoder.path, "native");
    assert_eq!(s.decoder.budget, "unlimited");
    assert_eq!(s.decoder.max_sync_candidates, 200);
    assert_eq!(s.truth.source, "jt9");
    assert!(!s.truth.decoder_identities.is_empty());
    assert!(s
        .truth
        .decoder_identities
        .iter()
        .all(|i| i.contains("jt9") && !i.contains("test fixture")));
}
#[test]
fn experiment_note_quotes_the_scorecard_totals() {
    let paths: Vec<_> = std::fs::read_dir(root().join("research/experiments"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-pan154-ft4-candidate-split.md")
        })
        .collect();
    assert_eq!(paths.len(), 1);
    let text = std::fs::read_to_string(&paths[0]).unwrap();
    assert!(text.contains(&card().total_line()));
    assert!(text.contains("no candidate produced"));
    assert!(text.contains("candidate produced, decode failed"));
}
