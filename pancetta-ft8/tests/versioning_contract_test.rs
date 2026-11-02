//! Versioning and documentation contract (PAN-176, Phase 3).
//!
//! `pancetta-ft8` is pinned by other programs via a `pancetta-ft8-v<version>`
//! git tag, so the crate's `Cargo.toml` must carry its own explicit version
//! (decoupled from the workspace) and the README must document the contract
//! (dependency line, `decode_cycle`, the dependency-tree guarantee and the
//! parity tolerances). These tests pin all of that.

/// The crate's own manifest, as written (not the resolved metadata).
fn crate_manifest() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("pancetta-ft8/Cargo.toml is readable")
}

fn crate_readme() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("pancetta-ft8/README.md is readable")
}

#[test]
fn crate_version_matches_documented_tag_scheme() {
    // The manifest carries an explicit version, not the workspace
    // placeholder.
    let manifest = crate_manifest();
    assert!(
        manifest.lines().any(|l| l.starts_with("version = ")),
        "pancetta-ft8/Cargo.toml must set an explicit `version`:\n{manifest}"
    );
    assert!(
        !manifest.contains("version.workspace = true"),
        "pancetta-ft8/Cargo.toml must not inherit the workspace version"
    );

    // The version parses and matches the tag scheme documented in the
    // README (`pancetta-ft8-v<semver>`).
    let version = env!("CARGO_PKG_VERSION");
    let parts: Vec<&str> = version.split('.').collect();
    assert_eq!(parts.len(), 3, "version {version} is not semver");
    for p in &parts {
        assert!(
            p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty(),
            "version segment {p:?} is not numeric"
        );
    }

    let readme = crate_readme();
    let needle = format!("pancetta-ft8-v{version}");
    assert!(
        readme.contains(&needle),
        "README must document the pin tag `{needle}` for the current version"
    );
}

#[test]
fn readme_carries_the_decoder_contract() {
    let readme = crate_readme();
    let assert_contains = |needle: &str| {
        assert!(
            readme.contains(needle),
            "README's decoder contract must mention {needle:?}"
        );
    };

    // The dependency line a consumer types (git pin by tag).
    assert_contains("pancetta-ft8 = { git = ");
    // The API call.
    assert_contains("decode_cycle");
    assert_contains("Protocol::Ft4");
    // The submodule note for git consumers on older cargo.
    assert_contains("git submodule update --init");
    // The no-audio/rig/network guarantee, with the verification command.
    assert_contains("cargo tree -p pancetta-ft8 --edges normal");
    // The parity tolerances: text/frequency exact; SNR ±1.0 dB; DT ±0.15 s,
    // slot-boundary-relative.
    assert_contains("1.0 dB");
    assert_contains("0.15 s");
    assert_contains("slot boundary");
}
