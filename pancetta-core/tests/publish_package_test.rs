//! Registry packaging gate for pancetta-core (PAN-384).
//!
//! `pancetta-core` publishes to crates.io together with `pancetta-ft8`, which
//! depends on it. These tests pin what its `.crate` ships, its own version,
//! its license files and its third-party notice. They shell out to `$CARGO`
//! (fallback `cargo` on `PATH`) and FAIL with an explicit message when it
//! cannot run.

use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("pancetta-core lives inside the workspace")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn package_ships_only_sources_readme_and_licenses() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .current_dir(manifest_dir())
        .args(["package", "--list", "--allow-dirty", "--offline"])
        .args(["--color", "never"])
        .args(["-p", "pancetta-core"])
        .output()
        .expect("failed to run `cargo package --list -p pancetta-core`");
    assert!(
        output.status.success(),
        "`cargo package --list -p pancetta-core` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list: Vec<String> = String::from_utf8(output.stdout)
        .expect("cargo package --list prints UTF-8")
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect();

    const EXACT: [&str; 8] = [
        ".cargo_vcs_info.json",
        "Cargo.lock",
        "Cargo.toml",
        "Cargo.toml.orig",
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "THIRD-PARTY-NOTICES.md",
    ];
    let offending: Vec<&String> = list
        .iter()
        .filter(|p| !EXACT.contains(&p.as_str()) && !(p.starts_with("src/") && p.ends_with(".rs")))
        .collect();
    assert!(
        offending.is_empty(),
        "the pancetta-core package ships paths outside the PAN-384 whitelist: {offending:?}"
    );
    for required in ["src/lib.rs", "README.md", "LICENSE-MIT", "LICENSE-APACHE"] {
        assert!(
            list.iter().any(|p| p == required),
            "the pancetta-core package is missing {required}; package list:\n{list:#?}"
        );
    }
}

#[test]
fn manifest_sets_its_own_version() {
    let manifest = read(&manifest_dir().join("Cargo.toml"));
    assert!(
        manifest.lines().any(|l| l.starts_with("version = ")),
        "pancetta-core/Cargo.toml must set its own `version = \"...\"` (PAN-384)"
    );
    assert!(
        !manifest.contains("version.workspace = true"),
        "pancetta-core's version must not follow the workspace (PAN-384)"
    );
}

#[test]
fn license_files_match_the_repository_root() {
    for name in ["LICENSE-MIT", "LICENSE-APACHE"] {
        let path = manifest_dir().join(name);
        let meta =
            std::fs::symlink_metadata(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            meta.file_type().is_file(),
            "{} must be a regular file, not a symlink",
            path.display()
        );
        let ours = std::fs::read(&path).expect("read crate license");
        let root = std::fs::read(repo_root().join(name)).expect("read root license");
        assert_eq!(
            ours,
            root,
            "{} must match the repository root's",
            path.display()
        );
    }
}

#[test]
fn third_party_notices_credit_cty_dat() {
    let notices = read(&manifest_dir().join("THIRD-PARTY-NOTICES.md"));
    for needle in ["cty.dat", "AD1C", "country-files.com"] {
        assert!(
            notices.contains(needle),
            "pancetta-core/THIRD-PARTY-NOTICES.md must contain {needle:?}"
        );
    }
}
