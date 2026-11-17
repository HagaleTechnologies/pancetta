//! Registry packaging gate (PAN-384).
//!
//! `pancetta-ft8` publishes to crates.io together with `pancetta-core`. These
//! tests pin what the `.crate` ships (the ft8_lib C sources `build.rs`
//! compiles, the license files and notices, and nothing outside the
//! `include` whitelist), the registry metadata, the set of publishable
//! workspace crates, and the `build.rs` guard that refuses a packaged crate
//! without its C sources.
//!
//! Like `standalone_deps_test.rs`, they shell out to `$CARGO` (fallback
//! `cargo` on `PATH`) and FAIL with an explicit message when it cannot run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("pancetta-ft8 lives inside the workspace")
        .to_path_buf()
}

fn submodule_present() -> bool {
    manifest_dir()
        .join("vendor/ft8_lib/ft8/constants.c")
        .exists()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `cargo package --list` for one package, one path per line.
fn package_list(package: &str) -> Vec<String> {
    let output = Command::new(cargo())
        .current_dir(manifest_dir())
        .args(["package", "--list", "--allow-dirty", "--offline"])
        .args(["--color", "never"])
        .args(["-p", package])
        .output()
        .unwrap_or_else(|e| panic!("failed to run `cargo package --list -p {package}`: {e}"));
    assert!(
        output.status.success(),
        "`cargo package --list -p {package}` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cargo package --list prints UTF-8")
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect()
}

fn workspace_metadata() -> serde_json::Value {
    let output = Command::new(cargo())
        .current_dir(manifest_dir())
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .args(["--color", "never"])
        .output()
        .expect("failed to run `cargo metadata`");
    assert!(
        output.status.success(),
        "`cargo metadata` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON")
}

fn package<'a>(metadata: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    metadata["packages"]
        .as_array()
        .expect("metadata has packages")
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("package {name} is in the workspace"))
}

#[test]
fn package_contains_the_c_decoder_sources_and_licenses() {
    let list: BTreeSet<String> = package_list("pancetta-ft8").into_iter().collect();

    let always = [
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "THIRD-PARTY-NOTICES.md",
        "README.md",
        "build.rs",
        "mingw_stpcpy_shim.h",
        "assets/neural_osd_weights.bin",
        "assets/neural_osd_weights.provenance.json",
        "src/lib.rs",
        "examples/decode_cycle.rs",
    ];
    // The C list mirrors the files `build.rs` compiles (build.rs:12-23),
    // plus the ft8_lib license.
    let vendor = [
        "vendor/ft8_lib/ft8/constants.c",
        "vendor/ft8_lib/ft8/encode.c",
        "vendor/ft8_lib/ft8/decode.c",
        "vendor/ft8_lib/ft8/message.c",
        "vendor/ft8_lib/ft8/ldpc.c",
        "vendor/ft8_lib/ft8/crc.c",
        "vendor/ft8_lib/ft8/text.c",
        "vendor/ft8_lib/fft/kiss_fft.c",
        "vendor/ft8_lib/fft/kiss_fftr.c",
        "vendor/ft8_lib/common/monitor.c",
        "vendor/ft8_lib/LICENSE",
    ];

    let mut missing: Vec<&str> = always
        .iter()
        .copied()
        .filter(|p| !list.contains(*p))
        .collect();
    if submodule_present() {
        missing.extend(vendor.iter().copied().filter(|p| !list.contains(*p)));
    } else {
        eprintln!(
            "NOTE: pancetta-ft8/vendor/ft8_lib is not checked out; skipping the \
             vendor/ assertions. CI's packaging step (scripts/ci/check-publish-package.sh) \
             hard-fails in this case."
        );
    }
    assert!(
        missing.is_empty(),
        "the pancetta-ft8 package is missing {missing:?}; package list:\n{list:#?}"
    );
}

#[test]
fn package_ships_only_whitelisted_paths() {
    const EXACT: [&str; 12] = [
        ".cargo_vcs_info.json",
        "Cargo.lock",
        "Cargo.toml",
        "Cargo.toml.orig",
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "THIRD-PARTY-NOTICES.md",
        "build.rs",
        "mingw_stpcpy_shim.h",
        "examples/decode_cycle.rs",
        "vendor/ft8_lib/LICENSE",
    ];
    const PREFIXES: [&str; 5] = [
        "src/",
        "assets/",
        "vendor/ft8_lib/ft8/",
        "vendor/ft8_lib/fft/",
        "vendor/ft8_lib/common/",
    ];
    let offending: Vec<String> = package_list("pancetta-ft8")
        .into_iter()
        .filter(|p| !EXACT.contains(&p.as_str()) && !PREFIXES.iter().any(|pre| p.starts_with(pre)))
        .collect();
    assert!(
        offending.is_empty(),
        "the pancetta-ft8 package ships paths outside the PAN-384 whitelist: {offending:?}"
    );
}

/// One throwaway crate holding a copy of the real `build.rs`, with or
/// without the `Cargo.toml.orig` marker that cargo writes into packaged
/// crates. Returns (success, stderr).
fn build_guard_probe(case: &str, packaged: bool) -> (bool, String) {
    let dir = std::env::temp_dir().join(format!(
        "pancetta-ft8-build-guard-{}-{case}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create probe dir");

    let manifest = r#"[package]
name = "ft8-build-guard-probe"
version = "0.0.0"
edition = "2021"
publish = false

[build-dependencies]
cc = "1.2"

[workspace]
"#;
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write probe manifest");
    if packaged {
        std::fs::write(dir.join("Cargo.toml.orig"), manifest).expect("write Cargo.toml.orig");
    }
    std::fs::write(dir.join("src/lib.rs"), "").expect("write probe lib.rs");
    std::fs::copy(manifest_dir().join("build.rs"), dir.join("build.rs")).expect("copy build.rs");
    // The workspace lockfile pins `cc`, so the probe builds offline.
    std::fs::copy(repo_root().join("Cargo.lock"), dir.join("Cargo.lock")).expect("copy Cargo.lock");

    let output = Command::new(cargo())
        .current_dir(&dir)
        .args(["build", "--offline", "--color", "never"])
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to run `cargo build` for the build-guard probe");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    (output.status.success(), stderr)
}

#[test]
fn packaged_build_without_c_sources_fails_instead_of_stubbing() {
    // Case A: a git checkout without the submodule keeps building the stub.
    let (ok, stderr) = build_guard_probe("checkout", false);
    assert!(
        ok,
        "a checkout without vendor/ft8_lib must still build the stub:\n{stderr}"
    );
    assert!(
        stderr.contains("ft8lib_stub"),
        "the stub build must keep its warning:\n{stderr}"
    );

    // Case B: a packaged crate (Cargo.toml.orig present) without the C
    // sources must fail instead of shipping the stub decoder.
    let (ok, stderr) = build_guard_probe("packaged", true);
    assert!(
        !ok,
        "a packaged crate without vendor/ft8_lib must fail its build script:\n{stderr}"
    );
    assert!(
        stderr.contains("packaged pancetta-ft8 is missing vendor/ft8_lib"),
        "the failure must name the missing C sources:\n{stderr}"
    );
}

#[test]
fn only_core_and_ft8_are_publishable() {
    let metadata = workspace_metadata();
    let mut publishable: Vec<String> = metadata["packages"]
        .as_array()
        .expect("metadata has packages")
        .iter()
        // `"publish": null` means publishable to any registry; `[]` means none.
        .filter(|p| {
            p["publish"].is_null() || p["publish"].as_array().is_some_and(|a| !a.is_empty())
        })
        .map(|p| p["name"].as_str().expect("package name").to_string())
        .collect();
    publishable.sort();
    assert_eq!(
        publishable,
        ["pancetta-core", "pancetta-ft8"],
        "exactly pancetta-core and pancetta-ft8 may be publishable (PAN-384)"
    );

    let root = read(&repo_root().join("Cargo.toml"));
    let mut in_workspace_package = false;
    let mut publish_false = false;
    for line in root.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_workspace_package = line == "[workspace.package]";
            continue;
        }
        if in_workspace_package && line.replace(' ', "") == "publish=false" {
            publish_false = true;
        }
    }
    assert!(
        publish_false,
        "the root Cargo.toml's [workspace.package] must keep `publish = false`"
    );
}

/// Checked against https://crates.io/api/v1/category_slugs on 2026-11-17.
const VERIFIED_CATEGORY_SLUGS: [&str; 4] =
    ["multimedia", "multimedia::audio", "encoding", "science"];

#[test]
fn registry_metadata_is_complete() {
    let metadata = workspace_metadata();
    let ft8 = package(&metadata, "pancetta-ft8");
    let core = package(&metadata, "pancetta-core");

    for pkg in [ft8, core] {
        let name = pkg["name"].as_str().expect("package name");
        assert_eq!(pkg["license"], "MIT OR Apache-2.0", "{name} license");
        assert_eq!(
            pkg["repository"], "https://github.com/HagaleTechnologies/pancetta",
            "{name} repository"
        );
        assert_eq!(pkg["rust_version"], "1.88", "{name} rust-version");
        assert!(
            pkg["readme"]
                .as_str()
                .is_some_and(|r| r.ends_with("README.md")),
            "{name} readme must be README.md, got {}",
            pkg["readme"]
        );
        assert_eq!(
            pkg["metadata"]["docs"]["rs"]["targets"],
            serde_json::json!(["x86_64-unknown-linux-gnu"]),
            "{name} [package.metadata.docs.rs] targets"
        );

        let keywords = pkg["keywords"].as_array().expect("keywords array");
        assert!(
            !keywords.is_empty() && keywords.len() <= 5,
            "{name} needs 1-5 keywords"
        );
        for kw in keywords {
            let kw = kw.as_str().expect("keyword string");
            assert!(
                kw.len() <= 20
                    && kw.starts_with(|c: char| c.is_ascii_alphabetic())
                    && kw
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{name} keyword {kw:?} is not a valid crates.io keyword"
            );
        }

        let categories = pkg["categories"].as_array().expect("categories array");
        assert!(!categories.is_empty(), "{name} needs categories");
        for cat in categories {
            let cat = cat.as_str().expect("category string");
            assert!(
                VERIFIED_CATEGORY_SLUGS.contains(&cat),
                "{name} category {cat:?} is not in the verified slug list"
            );
        }
    }

    let ft8_desc = ft8["description"].as_str().expect("ft8 description");
    assert!(
        ft8_desc.contains("FT8") && ft8_desc.contains("FT4") && !ft8_desc.contains("for Pancetta"),
        "pancetta-ft8 description: {ft8_desc:?}"
    );
    let core_desc = core["description"].as_str().expect("core description");
    assert!(
        core_desc.contains("pancetta-ft8"),
        "pancetta-core description: {core_desc:?}"
    );

    let core_version = core["version"].as_str().expect("core version");
    let core_dep = ft8["dependencies"]
        .as_array()
        .expect("ft8 dependencies")
        .iter()
        .find(|d| d["name"] == "pancetta-core" && d["kind"].is_null())
        .expect("pancetta-ft8 has a normal dependency on pancetta-core");
    assert_eq!(
        core_dep["req"],
        format!("^{core_version}"),
        "pancetta-ft8's pancetta-core requirement must equal pancetta-core's version"
    );

    for lib in [
        manifest_dir().join("src/lib.rs"),
        repo_root().join("pancetta-core/src/lib.rs"),
    ] {
        assert!(
            read(&lib).contains("#![deny(clippy::incompatible_msrv)]"),
            "{} must deny clippy::incompatible_msrv",
            lib.display()
        );
    }
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

const KISSFFT_LINES: [&str; 4] = [
    "Copyright (c) 2003-2010, Mark Borgerding. All rights reserved.",
    "Redistribution and use in source and binary forms, with or without",
    "Neither the name of the copyright holder nor the names of its",
    "THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\"",
];

#[test]
fn third_party_notices_carry_every_bundled_license() {
    let notices = read(&manifest_dir().join("THIRD-PARTY-NOTICES.md"));
    assert!(notices.contains("Copyright (c) 2018 Kārlis Goba"));
    assert!(notices.contains("Permission is hereby granted, free of charge"));
    let ft8_lib_license = manifest_dir().join("vendor/ft8_lib/LICENSE");
    if ft8_lib_license.exists() {
        for line in read(&ft8_lib_license)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            assert!(
                notices.contains(line),
                "THIRD-PARTY-NOTICES.md is missing ft8_lib LICENSE line {line:?}"
            );
        }
    }
    for line in KISSFFT_LINES {
        assert!(
            notices.contains(line),
            "pancetta-ft8/THIRD-PARTY-NOTICES.md is missing kissfft line {line:?}"
        );
    }

    let root = read(&repo_root().join("THIRD-PARTY-NOTICES.md"));
    for line in KISSFFT_LINES {
        assert!(
            root.contains(line),
            "the root THIRD-PARTY-NOTICES.md is missing kissfft line {line:?}"
        );
    }
    assert!(root.contains("cty.dat"), "root notices must credit cty.dat");
    assert!(
        !root.contains("see `pancetta-ft8/vendor/ft8_lib/fft/` for that"),
        "the root notices must not point to a kissfft notice that is not there"
    );
}

/// The notice ships inside the `.crate`, so its account of what the package
/// bundles from `vendor/ft8_lib/` must cover the real package list: every
/// bundled directory is named, the bundled C files `build.rs` does not
/// compile are named, and no C file it does compile is named among them.
#[test]
fn third_party_notices_describe_every_bundled_vendor_file() {
    if !submodule_present() {
        eprintln!(
            "NOTE: pancetta-ft8/vendor/ft8_lib is not checked out; skipping the \
             notice's vendor/ description check (CI's packaging step hard-fails here)"
        );
        return;
    }
    let notices = read(&manifest_dir().join("THIRD-PARTY-NOTICES.md"));
    let build_rs = read(&manifest_dir().join("build.rs"));
    for path in package_list("pancetta-ft8") {
        let Some(rel) = path.strip_prefix("vendor/ft8_lib/") else {
            continue;
        };
        if let Some((dir, _)) = rel.split_once('/') {
            assert!(
                notices.contains(&format!("`{dir}/`")),
                "THIRD-PARTY-NOTICES.md does not name the bundled directory `{dir}/`"
            );
        }
        if !rel.ends_with(".c") {
            continue;
        }
        // build.rs names each compiled file as format!("{}/<rel>", ft8_dir).
        let compiled = build_rs.contains(&format!("/{rel}\""));
        assert_eq!(
            notices.contains(&format!("`{rel}`")),
            !compiled,
            "THIRD-PARTY-NOTICES.md must name `{rel}` exactly when it ships but \
             build.rs does not compile it (compiled: {compiled})"
        );
    }
}

#[test]
fn publishing_runbook_names_the_two_package_commands() {
    let runbook = read(&repo_root().join("docs/decoder-crate.md"));
    for needle in [
        "cargo publish --dry-run -p pancetta-core -p pancetta-ft8",
        "cargo publish -p pancetta-core -p pancetta-ft8",
        "git submodule update --init",
        "pancetta-ft8-v",
        "pancetta-core-v",
        "cty.dat",
    ] {
        assert!(
            runbook.contains(needle),
            "docs/decoder-crate.md must contain {needle:?}"
        );
    }
    assert!(
        !runbook.contains("`publish = false` stays"),
        "docs/decoder-crate.md still says `publish = false` stays"
    );
}

/// The test, bench and example targets of `package` that its `include`
/// whitelist leaves out. `cargo publish` prints one `warning: ignoring ...`
/// line for each.
fn targets_left_out_of_the_package(metadata: &serde_json::Value, name: &str) -> usize {
    let pkg = package(metadata, name);
    let root = Path::new(
        pkg["manifest_path"]
            .as_str()
            .expect("manifest_path is a string"),
    )
    .parent()
    .expect("a manifest has a parent directory")
    .to_path_buf();
    let shipped: BTreeSet<String> = package_list(name).into_iter().collect();
    pkg["targets"]
        .as_array()
        .expect("package has targets")
        .iter()
        .filter(|t| {
            t["kind"].as_array().is_some_and(|k| {
                k.iter()
                    .any(|k| k == "test" || k == "bench" || k == "example")
            })
        })
        .filter(|t| {
            let src = Path::new(t["src_path"].as_str().expect("src_path is a string"));
            let rel = src.strip_prefix(&root).unwrap_or(src);
            !shipped.contains(&rel.to_string_lossy().replace('\\', "/"))
        })
        .count()
}

/// Runbook step 3 is the operator's expected output for an irreversible
/// publish. It must state the rule for the `warning: ignoring ...` lines, and
/// any count it gives must equal the real number of left-out targets.
#[test]
fn publishing_runbook_expects_the_real_ignored_target_warnings() {
    let runbook = read(&repo_root().join("docs/decoder-crate.md"));
    assert!(
        runbook.contains("is not included in the published package"),
        "docs/decoder-crate.md must say which `warning: ignoring ...` lines the dry-run prints"
    );
    let metadata = workspace_metadata();
    let left_out = targets_left_out_of_the_package(&metadata, "pancetta-core")
        + targets_left_out_of_the_package(&metadata, "pancetta-ft8");
    for (idx, _) in runbook.match_indices("`warning: ignoring") {
        let before = runbook[..idx].trim_end();
        let digits = &before[before.trim_end_matches(|c: char| c.is_ascii_digit()).len()..];
        if let Ok(stated) = digits.parse::<usize>() {
            assert_eq!(
                stated, left_out,
                "docs/decoder-crate.md expects {stated} `warning: ignoring` lines, but the \
                 dry-run prints {left_out} (one per left-out test, bench and example target)"
            );
        }
    }
}
