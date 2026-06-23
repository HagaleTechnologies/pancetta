//! Verify FixtureTruth round-trips through JSON and that the committed
//! `research/corpus/fixtures/ft8/truth.json` parses correctly.

use pancetta_research::truth::{FixtureCategory, FixtureEntry, FixtureTruth};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn truth_round_trips_to_disk() {
    let mut fixtures = BTreeMap::new();
    fixtures.insert(
        "generated/ft8_cq.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::Exact,
            expect: vec!["CQ TEST K1ABC FN42".to_string()],
            notes: "Test fixture.".to_string(),
        },
    );
    fixtures.insert(
        "wsjt/170709_135615.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::AnyDecode,
            expect: vec!["any-decode".to_string()],
            notes: "WSJT-X golden.".to_string(),
        },
    );
    let truth = FixtureTruth {
        schema_version: FixtureTruth::CURRENT_SCHEMA_VERSION,
        fixtures,
    };
    let json = serde_json::to_string_pretty(&truth).unwrap();
    let back: FixtureTruth = serde_json::from_str(&json).unwrap();
    assert_eq!(back.fixtures.len(), 2);
    assert_eq!(
        back.get("generated/ft8_cq.wav").unwrap().category,
        FixtureCategory::Exact
    );
}

#[test]
fn committed_truth_json_parses_and_covers_all_fixtures() {
    let path = workspace_root().join("research/corpus/fixtures/ft8/truth.json");
    let truth = FixtureTruth::load(&path).expect("committed truth.json must parse");

    // All 13 fixtures must be present in truth.json.
    let expected_keys = [
        "generated/ft8_cq.wav",
        "generated/ft8_report.wav",
        "generated/ft8_rr73.wav",
        "wsjt/170709_135615.wav",
        "wsjt/181201_180245.wav",
        "wsjt/210703_133430.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
        "basicft8/live_now.wav",
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
    ];
    for key in expected_keys {
        assert!(
            truth.get(key).is_some(),
            "truth.json missing fixture: {key}"
        );
    }
}

#[test]
fn truth_load_rejects_wrong_schema_version() {
    let mut fixtures = BTreeMap::new();
    fixtures.insert(
        "x.wav".to_string(),
        FixtureEntry {
            category: FixtureCategory::Skip,
            expect: vec![],
            notes: String::new(),
        },
    );
    let mut truth = FixtureTruth {
        schema_version: 999,
        fixtures,
    };
    truth.schema_version = 999;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), serde_json::to_string(&truth).unwrap()).unwrap();
    let err = FixtureTruth::load(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("schema_version"));
}
