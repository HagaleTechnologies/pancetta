//! Committed evidence for PAN-162. Needs only the vendored ft8_lib submodule
//! (initialised in every container), no private recordings.
use pancetta_research::impulsiveness::SurveyReport;
use pancetta_research::storm_remeasure::{self as sr, OffairScorecard, StormRemeasureScorecard};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn storm_card() -> StormRemeasureScorecard {
    StormRemeasureScorecard::load(root().join("research/scorecards/pan162-impulse-llr-storm.json"))
        .unwrap()
}

#[test]
fn storm_record_matches_the_preregistration() {
    let card = storm_card();
    card.check_invariants().unwrap();
    assert_eq!(card.boot_n, sr::BOOT_N);
    assert_eq!(card.boot_seed, sr::BOOT_SEED);
    assert_eq!(card.bars, sr::Bars::PREREGISTERED);
    assert_eq!(card.k_order, sr::K_ORDER);
    let names: Vec<&str> = card.plants.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, sr::PLANTS.map(|(n, _)| n));
    for p in &card.plants {
        assert_eq!(p.trials, sr::trials_for(&p.name).unwrap(), "{}", p.name);
        // Pre-registered grid, or an extension of it that check_grid accepted.
        let pre = sr::preregistered_grid(&p.name).unwrap();
        assert!(
            p.grid.windows(pre.len()).any(|w| w == pre.as_slice()),
            "{}: grid {:?} lacks the pre-registered grid",
            p.name,
            p.grid
        );
    }
}

#[test]
fn offair_record_rehashes_the_vendored_corpus() {
    let card =
        OffairScorecard::load(root().join("research/scorecards/pan162-impulse-llr-offair.json"))
            .unwrap();
    card.check_invariants().unwrap();
    assert_eq!(card.wavs.len(), 60);
    assert_eq!(card.missing_truth, 0);
    let storm = storm_card();
    assert_eq!(card.delta_tp_k6, storm.offair_delta_tp_k6);
    assert_eq!(card.delta_tp_k3, storm.offair_delta_tp_k3);
    for w in &card.wavs {
        let bytes = std::fs::read(root().join(&w.path)).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            w.sha256,
            "{}",
            w.path
        );
        let txt = Path::new(&w.path).with_extension("txt");
        assert!(
            root().join(txt).exists(),
            "{} has no reference list",
            w.path
        );
    }
}

#[test]
fn offair_survey_is_ranked() {
    let report = SurveyReport::load(
        root().join("research/corpus/characterizations/pan162-impulse-survey-offair.json"),
    )
    .unwrap();
    assert!(report.is_ranked());
    assert!(!report.days.is_empty());
    assert_eq!(report.skipped_wrong_rate, 7);
    assert_eq!(report.unreadable, 0);
    let ranked: usize = report.days.iter().map(|d| d.slots + d.silent_slots).sum();
    assert_eq!(ranked + report.undated_slots, report.wavs.len());
}

#[test]
fn experiment_doc_quotes_the_verdict_line() {
    let docs: Vec<PathBuf> = std::fs::read_dir(root().join("research/experiments"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-pan162-impulse-llr-storm.md")
        })
        .collect();
    assert_eq!(docs.len(), 1, "{docs:?}");
    let doc = std::fs::read_to_string(&docs[0]).unwrap();
    let line = storm_card().verdict_line();
    assert!(doc.contains(&line), "doc lacks `{line}`");
}
