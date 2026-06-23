use chrono::Utc;
use pancetta_research::scorecard::{
    BuildInfo, CompositeInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard,
    TierResult,
};
use pancetta_research::Mode;
use serde_json::json;
use std::collections::BTreeMap;

fn sample_scorecard() -> Scorecard {
    let mut tiers = BTreeMap::new();
    tiers.insert(
        "fixtures".to_string(),
        TierResult {
            wavs_processed: 82,
            fixtures_total: Some(82),
            fixtures_passed: Some(80),
            fixtures_failed: Some(2),
            pass_rate: Some(0.9756),
            ..Default::default()
        },
    );
    let mut weights = BTreeMap::new();
    weights.insert("fixtures_pass_rate".to_string(), 1.0);
    Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: Utc::now(),
        mode: Mode::Ft8,
        git: GitInfo {
            branch: "main".to_string(),
            head_sha: "abc1234".to_string(),
            main_merge_base: "abc1234".to_string(),
            dirty: false,
        },
        build: BuildInfo {
            rustc_version: "1.85.0".to_string(),
            release: true,
            features: vec!["transmit".into(), "research-eval".into()],
        },
        harness: HarnessInfo {
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            host: "darwin/arm64".to_string(),
            cores_used: 10,
            elapsed_seconds: 12.5,
        },
        config: ConfigInfo {
            decoder: json!({"placeholder": "decoder config snapshot"}),
            seed: 42,
            tiers_run: vec!["fixtures".to_string()],
            fp_filter_active: false,
        },
        tiers,
        composite: CompositeInfo {
            weights,
            score: 0.9756,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: "Smoke test scorecard.".to_string(),
    }
}

#[test]
fn scorecard_round_trips_to_disk() {
    let card = sample_scorecard();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    card.save(tmp.path()).unwrap();
    let back = Scorecard::load(tmp.path()).unwrap();
    assert_eq!(card.schema_version, back.schema_version);
    assert_eq!(card.mode, back.mode);
    assert_eq!(card.tiers.len(), back.tiers.len());
    assert!(back.tiers.contains_key("fixtures"));
    assert_eq!(card.composite.score, back.composite.score);
}

#[test]
fn scorecard_load_rejects_wrong_schema_version() {
    let mut card = sample_scorecard();
    card.schema_version = 999;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    card.save(tmp.path()).unwrap();
    let err = Scorecard::load(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("schema_version"));
}

#[test]
fn scorecard_json_omits_empty_optional_fields() {
    let card = sample_scorecard();
    let json = serde_json::to_string(&card).unwrap();
    // Empty Vec fields should be skipped, not serialized as [].
    assert!(!json.contains("\"by_snr_db\":[]"));
    assert!(!json.contains("\"failures\":[]"));
    // Optional fields that are None should be skipped.
    assert!(!json.contains("\"truth_decodes_total\":null"));
    assert!(!json.contains("\"false_positives_total\":"));
    assert!(!json.contains("\"novel_decodes\":"));
}
