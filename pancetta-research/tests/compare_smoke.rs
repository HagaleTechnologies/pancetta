//! End-to-end: compare binary correctly identifies wins, regressions, and
//! no-change between scorecards constructed by hand.

#![cfg(feature = "research-eval")]

use pancetta_research::scorecard::{
    BuildInfo, CompositeInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard,
    TierResult,
};
use pancetta_research::Mode;
use serde_json::json;
use std::collections::BTreeMap;
use std::process::Command;

fn make_scorecard(score: f64, pass_rate: f64, snr50: f64) -> Scorecard {
    let mut tiers = BTreeMap::new();
    tiers.insert(
        "fixtures".to_string(),
        TierResult {
            wavs_processed: 13,
            fixtures_total: Some(13),
            fixtures_passed: Some(13),
            pass_rate: Some(pass_rate),
            ..Default::default()
        },
    );
    tiers.insert(
        "synth-clean".to_string(),
        TierResult {
            wavs_processed: 60,
            snr_at_50pct_recovery_db: Some(snr50),
            ..Default::default()
        },
    );
    let mut weights = BTreeMap::new();
    weights.insert("fixtures_pass_rate".to_string(), 0.15);
    weights.insert("snr_50pct_synth_clean".to_string(), 0.30);
    Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: chrono::Utc::now(),
        mode: Mode::Ft8,
        git: GitInfo {
            branch: "test".into(),
            head_sha: "abc1234".into(),
            main_merge_base: "abc1234".into(),
            dirty: false,
        },
        build: BuildInfo {
            rustc_version: "1.85.0".into(),
            release: true,
            features: vec![],
        },
        harness: HarnessInfo {
            harness_version: "test".into(),
            host: "darwin/arm64".into(),
            cores_used: 1,
            elapsed_seconds: 0.0,
        },
        config: ConfigInfo {
            decoder: json!({"placeholder": "config"}),
            seed: 42,
            tiers_run: vec!["fixtures".into(), "synth-clean".into()],
            fp_filter_active: false,
        },
        tiers,
        composite: CompositeInfo {
            weights,
            score,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: String::new(),
    }
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn compare_detects_improvement() {
    let a = tempfile::NamedTempFile::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    make_scorecard(0.50, 1.0, -20.0).save(a.path()).unwrap();
    make_scorecard(0.55, 1.0, -22.0).save(b.path()).unwrap();

    let output = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "compare",
            "--",
        ])
        .arg(a.path())
        .arg(b.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");
    assert!(output.status.success(), "compare should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("WINS:"), "should report wins");
    assert!(stdout.contains("SNR@50%"), "should mention SNR delta");
    assert!(
        stdout.contains("REGRESSIONS:\n  (none)"),
        "no regressions expected; got: {stdout}"
    );
}

#[test]
fn compare_detects_regression() {
    let a = tempfile::NamedTempFile::new().unwrap();
    let b = tempfile::NamedTempFile::new().unwrap();
    make_scorecard(0.55, 1.0, -22.0).save(a.path()).unwrap();
    make_scorecard(0.50, 0.85, -20.0).save(b.path()).unwrap();

    let output = Command::new("cargo")
        .args([
            "run",
            "--release",
            "-q",
            "-p",
            "pancetta-research",
            "--bin",
            "compare",
            "--",
        ])
        .arg(a.path())
        .arg(b.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("REGRESSIONS:"), "should report regressions");
    assert!(
        stdout.contains("pass_rate"),
        "should mention pass_rate delta"
    );
}
