//! Programmatic guarantee that no GitHub Actions workflow references the
//! research harness. Runs the same grep the bash script does.

use std::path::Path;
use std::process::Command;

#[test]
fn no_workflow_references_research() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let workflows = workspace.join(".github/workflows");
    if !workflows.exists() {
        // No workflows directory; nothing to guard.
        return;
    }
    let status = Command::new(workspace.join("scripts/research-env.sh"))
        .arg("--guard-ci")
        .current_dir(&workspace)
        .status()
        .expect("failed to spawn research-env.sh --guard-ci");
    assert!(
        status.success(),
        "research-env.sh --guard-ci failed; a workflow file references the research harness."
    );
}
