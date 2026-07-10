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

/// Workstream 0 (2026-07-06) — the FP-on-noise hard gate. Unlike every
/// other metric `compare` reports (advisory only), an increase in
/// `false_positives_total` on a shared tier must fail the process with a
/// nonzero exit code, printed prominently, regardless of every other
/// metric moving in B's favor.
#[test]
fn compare_hard_gate_fails_on_fp_increase() {
    let mut a = make_scorecard(0.50, 1.0, -20.0);
    let mut b = make_scorecard(0.60, 1.0, -22.0); // every advisory metric improves in B

    a.tiers.insert(
        "noise_1000".to_string(),
        TierResult {
            wavs_processed: 1000,
            false_positives_total: Some(0),
            noise_files_decoded: Some(0),
            ..Default::default()
        },
    );
    b.tiers.insert(
        "noise_1000".to_string(),
        TierResult {
            wavs_processed: 1000,
            false_positives_total: Some(3), // regression: decoder now hallucinates
            noise_files_decoded: Some(2),
            ..Default::default()
        },
    );

    let a_file = tempfile::NamedTempFile::new().unwrap();
    let b_file = tempfile::NamedTempFile::new().unwrap();
    a.save(a_file.path()).unwrap();
    b.save(b_file.path()).unwrap();

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
        .arg(a_file.path())
        .arg(b_file.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");

    assert!(
        !output.status.success(),
        "compare must exit nonzero when false_positives_total increases"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("HARD GATE FAILURE"),
        "must print a prominent hard-gate banner; got: {stdout}"
    );
    assert!(
        stdout.contains("false_positives_total") && stdout.contains("0 → 3"),
        "must show the exact before/after FP counts; got: {stdout}"
    );
}

/// The gate must NOT false-positive itself: when the noise tier is
/// unchanged (or improves), `compare` must still exit 0.
#[test]
fn compare_hard_gate_passes_when_fp_unchanged() {
    let mut a = make_scorecard(0.50, 1.0, -20.0);
    let mut b = make_scorecard(0.55, 1.0, -22.0);

    a.tiers.insert(
        "noise_1000".to_string(),
        TierResult {
            wavs_processed: 1000,
            false_positives_total: Some(0),
            noise_files_decoded: Some(0),
            ..Default::default()
        },
    );
    b.tiers.insert(
        "noise_1000".to_string(),
        TierResult {
            wavs_processed: 1000,
            false_positives_total: Some(0),
            noise_files_decoded: Some(0),
            ..Default::default()
        },
    );

    let a_file = tempfile::NamedTempFile::new().unwrap();
    let b_file = tempfile::NamedTempFile::new().unwrap();
    a.save(a_file.path()).unwrap();
    b.save(b_file.path()).unwrap();

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
        .arg(a_file.path())
        .arg(b_file.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");

    assert!(
        output.status.success(),
        "compare must exit 0 when the noise tier is unchanged"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("HARD GATE FAILURE"),
        "must not print the hard-gate banner when nothing regressed; got: {stdout}"
    );
}

/// Task W0.3 (2026-07-06) — the unverified-novel standing-gate term:
/// unverified-novel growth must fail the comparison when it exceeds
/// 2×ΔTP (verified-TP increase), even though every advisory metric moves
/// in B's favor.
#[test]
fn compare_hard_gate_fails_when_unverified_novels_outgrow_tp() {
    let mut a = make_scorecard(0.50, 1.0, -20.0);
    let mut b = make_scorecard(0.60, 1.0, -22.0); // every advisory metric improves in B

    a.tiers.insert(
        "curated-hard-200".to_string(),
        TierResult {
            wavs_processed: 200,
            truth_decodes_recovered: Some(5000),
            novels_unverified: Some(100),
            ..Default::default()
        },
    );
    b.tiers.insert(
        "curated-hard-200".to_string(),
        TierResult {
            wavs_processed: 200,
            truth_decodes_recovered: Some(5010), // ΔTP = +10
            novels_unverified: Some(150),        // Δunverified = +50 > 2*10=20
            ..Default::default()
        },
    );

    let a_file = tempfile::NamedTempFile::new().unwrap();
    let b_file = tempfile::NamedTempFile::new().unwrap();
    a.save(a_file.path()).unwrap();
    b.save(b_file.path()).unwrap();

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
        .arg(a_file.path())
        .arg(b_file.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");

    assert!(
        !output.status.success(),
        "compare must exit nonzero when unverified-novels outgrow 2xDeltaTP"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("UNVERIFIED-NOVEL GROWTH"),
        "must print the unverified-novel hard-gate banner; got: {stdout}"
    );
}

/// The unverified-novel gate must NOT false-positive when growth stays
/// within the 2xDeltaTP allowance.
#[test]
fn compare_hard_gate_passes_when_unverified_novels_within_allowance() {
    let mut a = make_scorecard(0.50, 1.0, -20.0);
    let mut b = make_scorecard(0.55, 1.0, -22.0);

    a.tiers.insert(
        "curated-hard-200".to_string(),
        TierResult {
            wavs_processed: 200,
            truth_decodes_recovered: Some(5000),
            novels_unverified: Some(100),
            ..Default::default()
        },
    );
    b.tiers.insert(
        "curated-hard-200".to_string(),
        TierResult {
            wavs_processed: 200,
            truth_decodes_recovered: Some(5010), // ΔTP = +10
            novels_unverified: Some(115),        // Δunverified = +15 <= 2*10=20
            ..Default::default()
        },
    );

    let a_file = tempfile::NamedTempFile::new().unwrap();
    let b_file = tempfile::NamedTempFile::new().unwrap();
    a.save(a_file.path()).unwrap();
    b.save(b_file.path()).unwrap();

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
        .arg(a_file.path())
        .arg(b_file.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");

    assert!(
        output.status.success(),
        "compare must exit 0 when unverified-novel growth stays within 2xDeltaTP"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("UNVERIFIED-NOVEL GROWTH"),
        "must not print the unverified-novel hard-gate banner; got: {stdout}"
    );
}

/// Scorecards with no `novels_unverified` data at all (e.g. pre-W0.3
/// scorecards) must never trigger the gate — nothing to compare against.
#[test]
fn compare_hard_gate_inert_when_no_novel_classification_data() {
    let a = make_scorecard(0.50, 1.0, -20.0);
    let b = make_scorecard(0.60, 1.0, -22.0);

    let a_file = tempfile::NamedTempFile::new().unwrap();
    let b_file = tempfile::NamedTempFile::new().unwrap();
    a.save(a_file.path()).unwrap();
    b.save(b_file.path()).unwrap();

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
        .arg(a_file.path())
        .arg(b_file.path())
        .current_dir(workspace_root())
        .output()
        .expect("compare must run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("UNVERIFIED-NOVEL GROWTH"));
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
