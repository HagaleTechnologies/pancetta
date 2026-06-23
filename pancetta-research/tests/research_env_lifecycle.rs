//! Integration smoke for research-env.sh lifecycle subcommands.
//! Gated on research-eval feature since it shells out.

#![cfg(feature = "research-eval")]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn run_env(args: &[&str]) -> std::process::Output {
    Command::new(workspace_root().join("scripts/research-env.sh"))
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("research-env.sh must run")
}

#[test]
fn status_prints_empty_when_no_experiments() {
    let out = run_env(&["--status"]);
    assert!(out.status.success(), "--status should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("== Experiments =="),
        "expected status banner; got: {s}"
    );
    // Either "(no experiments yet — bootstrap..." OR a real experiment listing.
    // Both are valid — the test runs against whatever's on disk.
}

#[test]
fn pin_requires_slug() {
    let out = run_env(&["--pin"]);
    assert!(!out.status.success(), "--pin without slug should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage:"), "expected usage message; got: {err}");
}

#[test]
fn finalize_requires_slug() {
    let out = run_env(&["--finalize"]);
    assert!(!out.status.success(), "--finalize without slug should fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("usage:"), "expected usage message; got: {err}");
}

#[test]
fn cleanup_dry_run_by_default() {
    let out = run_env(&["--cleanup"]);
    assert!(out.status.success(), "--cleanup should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("dry-run") || s.contains("Cleanup"),
        "expected dry-run output; got: {s}"
    );
}
