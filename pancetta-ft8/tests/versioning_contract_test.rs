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

// --- Registry readiness (PAN-384) -------------------------------------------
//
// The README becomes the crates.io page and cannot change after a publish,
// so it must show the registry dependency line, state the build requirements,
// carry a correct license section and avoid links that only resolve inside
// the repository.

fn crate_lib_rs() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("pancetta-ft8/src/lib.rs is readable")
}

/// `<major>.<minor>` of the crate version, the registry requirement a
/// consumer types.
fn major_minor() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.');
    let major = parts.next().expect("version has a major part");
    let minor = parts.next().expect("version has a minor part");
    format!("{major}.{minor}")
}

#[test]
fn readme_is_ready_for_the_registry() {
    let readme = crate_readme();
    let rust_version = env!("CARGO_PKG_RUST_VERSION");
    assert!(
        !rust_version.is_empty(),
        "pancetta-ft8/Cargo.toml must set `rust-version`"
    );

    for needle in [
        format!("pancetta-ft8 = \"{}\"", major_minor()),
        "MSVC".to_string(),
        "C compiler".to_string(),
        format!("Rust {rust_version}"),
    ] {
        assert!(
            readme.contains(&needle),
            "README must contain {needle:?} for the crates.io page"
        );
    }

    let heading = "## License";
    let start = readme
        .find(heading)
        .unwrap_or_else(|| panic!("README must have a `{heading}` section"));
    let body = &readme[start + heading.len()..];
    let section = &body[..body.find("\n## ").unwrap_or(body.len())];
    for needle in [
        "MIT OR Apache-2.0",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "THIRD-PARTY-NOTICES.md",
        "BSD-3-Clause",
    ] {
        assert!(
            section.contains(needle),
            "README's license section must mention {needle:?}:\n{section}"
        );
    }

    for banned in [
        "](../",
        "/blob/main/",
        "/blob/HEAD/",
        "publish = false",
        "Registry publication stays off",
        "see the LICENSE file",
        "### Module Structure",
        "### Test Coverage",
        "## Dependencies",
        "## Future Enhancements",
    ] {
        assert!(
            !readme.contains(banned),
            "README must not contain {banned:?}: it is false or broken on the crates.io page"
        );
    }
}

/// Every concrete `pancetta-ft8-v<x.y.z>` in `text`, skipping the
/// `pancetta-ft8-v*` glob and the `pancetta-ft8-v<version>` placeholder.
fn pinned_tag_versions(text: &str) -> Vec<String> {
    let marker = "pancetta-ft8-v";
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        rest = &rest[at + marker.len()..];
        let raw: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let version = raw.trim_end_matches('.');
        if version.starts_with(|c: char| c.is_ascii_digit()) {
            found.push(version.to_string());
        }
    }
    found
}

#[test]
fn every_pinned_tag_names_the_current_version() {
    let version = env!("CARGO_PKG_VERSION");
    for (name, text) in [
        ("README.md", crate_readme()),
        ("src/lib.rs", crate_lib_rs()),
    ] {
        for pinned in pinned_tag_versions(&text) {
            assert_eq!(
                pinned, version,
                "{name} pins `pancetta-ft8-v{pinned}`, but the crate version is {version}"
            );
        }
    }

    let runbook_link = format!(
        "https://github.com/HagaleTechnologies/pancetta/blob/pancetta-ft8-v{version}/docs/decoder-crate.md"
    );
    assert!(
        crate_readme().contains(&runbook_link),
        "README must link the decoder contract at the release tag: {runbook_link}"
    );
}

#[test]
fn crate_docs_show_the_registry_dependency_line() {
    let lib = crate_lib_rs();
    let line = format!("pancetta-ft8 = \"{}\"", major_minor());
    assert!(
        lib.contains(&line),
        "src/lib.rs crate docs must show the registry line `{line}`"
    );
    for doc_line in lib
        .lines()
        .filter(|l| l.contains("PTT control") || l.contains("FCC Part 97"))
    {
        assert!(
            doc_line.contains("`transmit`"),
            "crate docs must say this needs the `transmit` feature: {doc_line}"
        );
    }
}
