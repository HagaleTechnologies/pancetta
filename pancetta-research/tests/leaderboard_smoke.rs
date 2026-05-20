//! End-to-end: leaderboard reads history/ + main.json and emits a sorted
//! markdown table.

#![cfg(feature = "research-eval")]

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn leaderboard_prints_table_with_main_json() {
    let workspace = workspace_root();
    let out = Command::new("cargo")
        .args([
            "run", "--release", "-q", "-p", "pancetta-research", "--bin", "leaderboard",
        ])
        .current_dir(&workspace)
        .output()
        .expect("leaderboard must run");
    assert!(out.status.success(), "leaderboard should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("# Decoder Research Leaderboard"),
        "expected header; got: {s}"
    );
    // main.json from Plan 2 should appear in the table.
    assert!(
        s.contains("main") || s.contains("0.3"),
        "expected a row for main.json or its score; got: {s}"
    );
}
