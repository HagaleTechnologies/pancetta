//! PAN-334: every flag `pancetta --help` (or a subcommand's `--help`) lists
//! does what it says. JSON logs are real; dead flags and stubs are gone and
//! fail at parse time instead of being silently ignored.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

const DEMO_WAV: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../assets/demo-wav/170923_082000.wav"
);

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
            .env_remove("RUST_LOG")
            .current_dir(self.cwd.path())
            .timeout(Duration::from_secs(60));
        cmd
    }

    /// Every line of every daily log file under the scratch home.
    fn log_file_lines(&self) -> Vec<String> {
        let dir = self.home.path().join(".pancetta").join("logs");
        let mut lines = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with("pancetta.log") {
                lines.extend(read_lines(&path));
            }
        }
        lines
    }

    fn write_config(&self, body: &str) -> PathBuf {
        let path = self.cwd.path().join("station.toml");
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// One JSON object with a string `level`, a string `target` and a `fields`
/// object holding the string `message` (tracing-subscriber's JSON layout).
fn assert_json_log_line(line: &str) -> serde_json::Value {
    let value: serde_json::Value =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("not a JSON object ({e}): {line}"));
    assert!(value["level"].is_string(), "no level: {line}");
    assert!(value["target"].is_string(), "no target: {line}");
    assert!(value["fields"].is_object(), "no fields: {line}");
    assert!(
        value["fields"]["message"].is_string(),
        "no fields.message: {line}"
    );
    value
}

#[test]
fn json_log_format_writes_one_object_per_line_to_stderr_and_the_log_file() {
    let scratch = Scratch::new();
    let out = scratch
        .command()
        .args(["--headless", "--log-format", "json", "--wav", DEMO_WAV])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let stderr = String::from_utf8(out.stderr).unwrap();
    let console: Vec<_> = stderr.lines().map(assert_json_log_line).collect();
    assert!(!console.is_empty(), "no JSON log lines on stderr");
    assert!(
        console.iter().any(|v| v["fields"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Starting Pancetta")),
        "startup line missing from stderr"
    );

    let file = scratch.log_file_lines();
    assert!(!file.is_empty(), "log file is empty");
    for line in &file {
        assert_json_log_line(line);
    }

    // Logs leave stdout to command output (here, the --wav decode lines).
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().all(|l| !l.trim_start().starts_with('{')),
        "JSON log line leaked to stdout: {stdout}"
    );
}

#[test]
fn text_log_format_is_unchanged() {
    let scratch = Scratch::new();
    let out = scratch
        .command()
        .args(["--headless", "--wav", DEMO_WAV])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Starting Pancetta"), "{stdout}");
    assert!(out.stderr.is_empty(), "text mode wrote to stderr");
    let file = scratch.log_file_lines();
    assert!(!file.is_empty());
    assert!(
        file.iter().all(|l| !l.starts_with('{')),
        "text file has JSON"
    );
}

#[test]
fn json_log_format_keeps_config_warnings_as_json() {
    let scratch = Scratch::new();
    let config =
        scratch.write_config("[station]\ncallsign = \"K5ARH\"\ngrid = \"EM10\"\nbogus_key = 1\n");
    let out = scratch
        .command()
        .arg("--config")
        .arg(&config)
        .args(["--headless", "--log-format", "json", "--wav", DEMO_WAV])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let console: Vec<_> = stderr.lines().map(assert_json_log_line).collect();
    assert!(
        console.iter().any(|v| {
            v["level"] == "WARN"
                && v["fields"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("bogus_key")
        }),
        "unknown-key warning missing from the JSON stream: {stderr}"
    );

    // Text mode still prints the plain warning for a person at a terminal.
    scratch
        .command()
        .arg("--config")
        .arg(&config)
        .args(["--headless", "--wav", DEMO_WAV])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "WARNING: Unknown config key `bogus_key`",
        ));
}

#[test]
fn metrics_flags_are_gone() {
    let scratch = Scratch::new();
    let help = scratch.command().arg("--help").output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(!help.contains("--metrics"), "{help}");

    // `--wav` makes the pre-fix binary exit by itself instead of running on.
    for args in [
        vec!["--headless", "--metrics", "--wav", DEMO_WAV],
        vec!["--headless", "--metrics-port", "9090", "--wav", DEMO_WAV],
    ] {
        Scratch::new()
            .command()
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

/// Subcommand names as `pancetta --help` lists them.
fn subcommands(scratch: &Scratch) -> Vec<String> {
    let out = scratch.command().arg("--help").output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    help.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_help_screen_lists_a_removed_flag_or_subcommand() {
    let scratch = Scratch::new();
    let names = subcommands(&scratch);
    assert!(names.iter().any(|n| n == "test-audio"), "{names:?}");
    assert!(!names.iter().any(|n| n == "benchmark"), "{names:?}");

    let mut screens = vec![(String::new(), vec!["--help".to_owned()])];
    for name in &names {
        screens.push((name.clone(), vec![name.clone(), "--help".to_owned()]));
    }
    for (name, args) in screens {
        let out = scratch.command().args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}");
        let help = String::from_utf8(out.stdout).unwrap();
        for flag in [
            "--metrics",
            "--metrics-port",
            "--frequency",
            "--power",
            "--device",
            "--duration",
        ] {
            let listed = help
                .split(|c: char| c.is_whitespace() || c == ',')
                .any(|word| word == flag);
            assert!(!listed, "`{name} --help` still lists {flag}");
        }
        if name != "export" {
            // `export --callsign` is a real filter; nothing else has one.
            assert!(
                !help.contains("--callsign"),
                "`{name} --help` lists --callsign"
            );
        }
        assert!(!help.contains("not yet implemented"), "`{name} --help`");
    }
}

#[test]
fn removed_flags_and_subcommands_fail_at_parse_time() {
    for args in [
        vec![
            "run",
            "--callsign",
            "K1ZZZ",
            "--headless",
            "--wav",
            DEMO_WAV,
        ],
        vec![
            "run",
            "--frequency",
            "14074000",
            "--headless",
            "--wav",
            DEMO_WAV,
        ],
        vec!["run", "--power", "5", "--headless", "--wav", DEMO_WAV],
        vec!["test-audio", "--device", "foo"],
        vec!["test-audio", "--duration", "3"],
        vec!["benchmark"],
        vec!["benchmark-decode", DEMO_WAV, "--format", "josn"],
    ] {
        Scratch::new()
            .command()
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("not yet implemented").not());
    }
}

#[test]
fn receiving_service_flags_are_rejected_on_other_subcommands() {
    for (args, flag) in [
        (vec!["info", "--wav", "x.wav"], "--wav"),
        (vec!["info", "--replay", "dir"], "--replay"),
        (vec!["info", "--test-tx", "N0CALL N0CALL 73"], "--test-tx"),
        (vec!["doctor", "--audio-device", "foo"], "--audio-device"),
        (vec!["config", "--show", "--no-audio"], "--no-audio"),
        (vec!["info", "--test-tx-offset", "900"], "--test-tx"),
    ] {
        Scratch::new()
            .command()
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(flag));
    }
}

#[test]
fn test_audio_lists_devices_with_or_without_list() {
    for args in [vec!["test-audio"], vec!["test-audio", "--list"]] {
        Scratch::new()
            .command()
            .args(&args)
            .assert()
            .success()
            .stdout(predicate::str::contains("Audio input devices:"))
            .stdout(predicate::str::contains("Audio output devices:"));
    }
}

#[test]
fn run_subcommand_still_starts_the_station() {
    Scratch::new()
        .command()
        .args(["run", "--headless", "--wav", DEMO_WAV])
        .assert()
        .success()
        .stdout(predicate::str::contains("Starting Pancetta"));
}

/// No CLI path may print "not yet implemented" (`test-rig --ptt` with CAT PTT
/// did, and its serial-port test cannot be driven without a rig).
#[test]
fn the_cli_source_has_no_not_yet_implemented_stub() {
    let main_rs = include_str!("../src/main.rs");
    assert!(!main_rs.contains("not yet implemented"));
}
