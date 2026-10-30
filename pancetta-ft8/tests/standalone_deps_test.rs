//! Standalone-consumption gate (PAN-176, Phase 2).
//!
//! `pancetta-ft8` is a dependency other programs (manta is the first) pull to
//! decode FT8/FT4 without any station, audio-device, rig-control or network
//! code. These tests make that guarantee CI-enforced instead of a one-off
//! observation: the default (normal-edge) dependency tree must contain no
//! audio-device, rig-control or network crate.
//!
//! They shell out to the `cargo` on `PATH` (CI provides it); if `cargo` is
//! absent the tests FAIL with an explicit message rather than silently
//! passing.

use std::path::Path;
use std::process::Command;

/// Crates that must never appear in `pancetta-ft8`'s default normal
/// dependency tree: audio-device stacks, rig-control stacks, async runtimes
/// and network clients. Feature-gated opt-ins (`transmit`, `gpio`,
/// `benchmark`) may pull some of these — that is exactly what the default
/// feature set must NOT do.
///
/// Additions to this list require a plan revision (PAN-176): the gate itself
/// must not drift silently.
const BANNED_CRATES: [&str; 14] = [
    // audio device
    "cpal",
    "alsa",
    "alsa-sys",
    "coreaudio",
    // rig control
    "serialport",
    "rppal",
    "hamlib",
    "libudev",
    // async runtime / network
    "tokio",
    "mio",
    "net2",
    "reqwest",
    "hyper",
    "network-interface",
];

/// Crates that `--features transmit` MUST pull — proving the gate in the
/// default-tree test is actually capable of failing (the banned crates exist
/// in this workspace's graph, one feature away).
const TRANSMIT_MUST_PULL: [&str; 3] = ["cpal", "serialport", "tokio"];

fn cargo_tree(extra_args: &[&str]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(manifest_dir)
        .arg("tree")
        .arg("-p")
        .arg("pancetta-ft8")
        .arg("--edges")
        .arg("normal");
    for arg in extra_args {
        cmd.arg(arg);
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{cargo} tree` (cargo must be on PATH): {e}"));
    assert!(
        output.status.success(),
        "`{} tree` failed:\nstdout:\n{}\nstderr:\n{}",
        cargo,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Extract the crate names present in `cargo tree` output: each dependency
/// line renders as `<glyphs> name vX.Y.Z`, so take the token before the
/// first ` v` and strip the tree-drawing characters.
fn crate_names(tree_output: &str) -> Vec<String> {
    tree_output
        .lines()
        .filter_map(|line| {
            let cleaned = line
                .trim_start_matches(|c: char| c == ' ' || c == '│' || c == '├' || c == '└'
                    || c == '─' || c == '┬' || c == '┴' || c == '┼');
            cleaned
                .split_whitespace()
                .next()
                .map(|s| s.to_string())
        })
        .collect()
}

#[test]
fn default_normal_dependency_tree_has_no_audio_rig_or_network_crates() {
    let tree = cargo_tree(&[]);
    let names = crate_names(&tree);

    // The tree must actually be about this crate (guards against a cargo
    // error page or an empty resolution passing vacuously).
    assert!(
        names.iter().any(|n| n == "pancetta-ft8"),
        "cargo tree output does not mention pancetta-ft8:\n{tree}"
    );
    assert!(
        names.len() > 1,
        "cargo tree output suspiciously small:\n{tree}"
    );

    let offenders: Vec<String> = names
        .into_iter()
        .filter(|n| BANNED_CRATES.contains(&n.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "pancetta-ft8's default normal dependency tree contains banned \
         audio-device/rig-control/network crates: {offenders:?}\n\
         full tree:\n{tree}"
    );
}

#[test]
fn transmit_feature_is_opt_in_and_does_pull_audio_deps_only_when_enabled() {
    // Under `--features transmit` the banned crates DO appear — proving the
    // default-tree gate has teeth (the crates are one feature away, not
    // absent from the graph).
    let tree = cargo_tree(&["--features", "transmit"]);
    let names = crate_names(&tree);
    for required in TRANSMIT_MUST_PULL {
        assert!(
            names.iter().any(|n| n == required),
            "`--features transmit` should pull `{required}` but the tree does not \
             list it — the opt-in posture changed:\n{tree}"
        );
    }

    // And with the feature OFF, the same crates must be absent (this is the
    // assertion the default-tree test owns, repeated here against the same
    // captured posture so a failure names both sides).
    let default_tree = cargo_tree(&[]);
    let default_names = crate_names(&default_tree);
    for banned in TRANSMIT_MUST_PULL {
        assert!(
            !default_names.iter().any(|n| n == banned),
            "`{banned}` must be opt-in only; it appears in the default tree"
        );
    }
}

#[test]
fn clean_default_build_compiles_the_c_decoder() {
    // The build script compiles the vendored ft8_lib C sources into
    // `OUT_DIR/libft8_lib.a`. When the submodule is not initialized,
    // build.rs degrades to the stub with a loud warning (build.rs:46) —
    // mirror that: skip with a printed note only in that degraded setup.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let vendored = Path::new(manifest_dir).join("vendor/ft8_lib/ft8/constants.c");
    if !vendored.exists() {
        eprintln!(
            "NOTE: ft8_lib submodule not initialized ({} absent) — \
             skipping the C-decoder artifact check; build.rs has already \
             emitted its degraded-recall warning",
            vendored.display()
        );
        return;
    }
    let artifact = Path::new(env!("OUT_DIR")).join("libft8_lib.a");
    assert!(
        artifact.exists(),
        "expected the C decoder archive at {} (submodule is initialized, so \
         build.rs must have compiled it)",
        artifact.display()
    );
}
