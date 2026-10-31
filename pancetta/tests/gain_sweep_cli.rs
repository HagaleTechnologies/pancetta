//! PAN-244 argument validation against the actual executable. Invalid requests
//! fail before loading station configuration or opening CAT/audio devices.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

struct Scratch {
    home: TempDir,
    cwd: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
        cmd.env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env_remove("PANCETTA_CALLSIGN")
            .current_dir(self.cwd.path())
            .timeout(Duration::from_secs(20));
        cmd
    }

    fn report(&self) -> PathBuf {
        self.cwd.path().join("gain.json")
    }
}

#[test]
fn help_describes_sweep_duration_and_report() {
    let scratch = Scratch::new();
    scratch
        .command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--gain-sweep-seconds"))
        .stdout(predicate::str::contains("1..3600"))
        .stdout(predicate::str::contains("600"))
        .stdout(predicate::str::contains("--gain-sweep-report"));
}

#[test]
fn sweep_requires_headless_and_both_options() {
    let scratch = Scratch::new();
    for args in [
        vec!["--headless", "--gain-sweep-seconds", "600"],
        vec!["--headless", "--gain-sweep-report", "gain.json"],
        vec![
            "--gain-sweep-seconds",
            "600",
            "--gain-sweep-report",
            "gain.json",
        ],
    ] {
        scratch
            .command()
            .args(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("required"));
    }
    assert!(!scratch.report().exists());
}

#[test]
fn sweep_duration_rejects_zero_over_limit_negative_and_noninteger() {
    let scratch = Scratch::new();
    for duration in ["0", "3601", "-1", "1.5", "ten"] {
        scratch
            .command()
            .args([
                "--headless",
                "--gain-sweep-report",
                "gain.json",
                "--gain-sweep-seconds",
                duration,
            ])
            .assert()
            .code(2);
    }
    assert!(!scratch.report().exists());
}

#[test]
fn sweep_rejects_every_incompatible_input_before_accessing_files() {
    let scratch = Scratch::new();
    for extra in [
        vec!["--wav", "does-not-exist.wav"],
        vec!["--replay", "does-not-exist"],
        vec!["--no-audio"],
        vec!["--test-tx", "N0CALL N0CALL 73"],
    ] {
        scratch
            .command()
            .args([
                "--headless",
                "--gain-sweep-seconds",
                "600",
                "--gain-sweep-report",
                "gain.json",
            ])
            .args(extra)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("cannot be used with"));
    }
    assert!(!scratch.report().exists());
}

#[test]
fn existing_report_is_preserved_before_config_or_rig_startup() {
    let scratch = Scratch::new();
    let report = scratch.report();
    let original = b"{\"operator_data\":\"keep this file\"}\n";
    std::fs::write(&report, original).unwrap();
    for duration in [Some("1"), Some("3600"), None] {
        let mut cmd = scratch.command();
        // An invalid config is deliberate. Refusal must happen first, so a
        // valid bounded duration cannot reach config, audio, or CAT startup.
        cmd.args(["--config", "missing-config.toml", "--headless"])
            .arg("--gain-sweep-report")
            .arg(&report)
            .arg("--gain-sweep-seconds");
        if let Some(duration) = duration {
            cmd.arg(duration);
        }
        cmd.assert()
            .code(1)
            .stderr(predicate::str::contains("report path already exists"));
        assert_eq!(std::fs::read(&report).unwrap(), original);
    }
}

#[test]
fn sweep_flags_are_rejected_on_non_receiving_subcommands() {
    let scratch = Scratch::new();
    scratch
        .command()
        .args([
            "--headless",
            "--gain-sweep-seconds",
            "600",
            "--gain-sweep-report",
            "gain.json",
            "info",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("gain"));
    assert!(!scratch.report().exists());
}

#[cfg(unix)]
#[test]
fn a_dangling_report_symlink_is_also_an_existing_destination() {
    let scratch = Scratch::new();
    let report = scratch.report();
    let target = scratch.cwd.path().join("missing-target.json");
    std::os::unix::fs::symlink(&target, &report).unwrap();
    scratch
        .command()
        .args(["--config", "missing-config.toml", "--headless"])
        .arg("--gain-sweep-report")
        .arg(&report)
        .args(["--gain-sweep-seconds", "1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("report path already exists"));
    assert_eq!(std::fs::read_link(report).unwrap(), target);
    assert!(!target.exists());
}
