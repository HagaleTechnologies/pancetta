//! End-to-end: run the eval binary against the fixtures tier and verify the
//! scorecard file lands on disk with a populated `fixtures` tier.
//!
//! Gated behind `--features research-eval` because it spawns `cargo run --release`
//! and rebuilds the eval binary — slow + side-effecting test.

#![cfg(feature = "research-eval")]

use pancetta_research::scorecard::Scorecard;
use std::process::Command;

#[test]
fn eval_fixtures_produces_valid_scorecard() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let tmp = tempfile::NamedTempFile::new().unwrap();

    // Run the eval binary via `cargo run` so it picks up the current build.
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
            "fixtures",
            "--mode",
            "ft8",
            "--output",
        ])
        .arg(tmp.path())
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn eval");
    assert!(status.success(), "eval binary failed");

    let card = Scorecard::load(tmp.path()).expect("scorecard must be loadable");
    assert_eq!(card.schema_version, Scorecard::CURRENT_SCHEMA_VERSION);
    let fixtures = card.tiers.get("fixtures").expect("fixtures tier present");
    assert!(fixtures.wavs_processed > 0, "no fixtures discovered");
    assert!(
        fixtures.pass_rate.unwrap() >= 0.0 && fixtures.pass_rate.unwrap() <= 1.0,
        "pass_rate out of range: {:?}",
        fixtures.pass_rate,
    );
    // We don't assert pass_rate == 1.0 because some fixtures may legitimately
    // not be decodable by the default config; this test only smokes the binary
    // end-to-end. Truth-validated pass/fail accounting happens inside the eval
    // binary via truth.json (FixtureCategory::Exact / AnyDecode / Skip).
}
