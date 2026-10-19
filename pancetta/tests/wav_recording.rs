//! PAN-125 end-to-end: WAV recording of decode windows is off unless the
//! operator turns it on with `[audio.recording] auto_record = true`, and
//! `--no-audio` never records regardless of config. (The `--replay` half of
//! the never-record scenario lives in `tests/replay_mode.rs`, which already
//! owns the corpus harness.)
//!
//! Every test here spawns the *real* binary against a scratch `$HOME` (see
//! `tests/replay_mode.rs` for why that isolation is load-bearing: the binary
//! derives its data paths from `dirs::home_dir()` at runtime). Output goes to
//! a regular file because a pipe can't be allowed to stall the child during
//! startup — same harness shape as `tests/shutdown_signals.rs`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Also reap on assertion failure or a startup timeout.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The real binary on a scratch home: `PANCETTA_*` env stripped (then
/// `PANCETTA_STUB_AUDIO=1` optionally re-set), HOME/XDG/USERPROFILE pinned,
/// stdout+stderr to `<home>/output.log`, `RUST_LOG=info`.
struct ScratchRun {
    home: tempfile::TempDir,
    output: PathBuf,
    child: ChildGuard,
}

impl ScratchRun {
    /// `extra_toml` is appended to the minimal station config.
    fn spawn(extra_toml: &str, args: &[&str], stub_audio: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let dot_pancetta = home.path().join(".pancetta");
        fs::create_dir_all(&dot_pancetta).unwrap();
        let config = dot_pancetta.join("pancetta.toml");
        fs::write(
            &config,
            format!(
                "[station]\ncallsign = \"K5ARH\"\ngrid_square = \"EM12\"\n{extra_toml}"
            ),
        )
        .unwrap();

        // Regular files cannot fill a pipe and stall the child during startup.
        let output_path = home.path().join("output.log");
        let output = File::create(&output_path).unwrap();
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("pancetta"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PANCETTA_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("XDG_DATA_HOME", home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", home.path().join(".cache"))
            .env("RUST_LOG", "info")
            .arg("--config")
            .arg(&config)
            .arg("--headless")
            .args(args);
        if stub_audio {
            command.env("PANCETTA_STUB_AUDIO", "1");
        }
        command
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        let child = ChildGuard(command.spawn().unwrap());
        Self {
            home,
            output: output_path,
            child,
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.output).unwrap_or_default()
    }

    /// Poll every 50 ms until `predicate` holds; fail with the full child
    /// log on timeout or early child exit. Assertions here only match
    /// message substrings: the console layer emits ANSI codes around the
    /// level and target, but each message is contiguous.
    fn wait_for(
        &mut self,
        what: &str,
        timeout: Duration,
        mut predicate: impl FnMut(&Self) -> bool,
    ) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            assert!(
                self.child.0.try_wait().unwrap().is_none(),
                "child exited early while waiting for {what}:\n{}",
                self.log()
            );
            if predicate(self) {
                return self.log();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Every `*.wav` under `root`, optionally excluding one subtree (the
/// replay-corpus directory, or the configured recording directory).
fn wavs_under(root: &Path, exclude: Option<&Path>) -> Vec<PathBuf> {
    fn walk(dir: &Path, exclude: Option<&Path>, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if exclude.is_some_and(|x| path.starts_with(x)) {
                continue;
            }
            if path.is_dir() {
                walk(&path, exclude, out);
            } else if path.extension().is_some_and(|e| e == "wav") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, exclude, &mut out);
    out
}

/// Scenario 1: recording is off unless the operator turns it on.
#[test]
fn default_config_decodes_without_writing_any_wav() {
    // The stub emits bare mono audio, so the DSP stage must be told
    // `input_channels = 1` or its channel split halves the stream
    // (wiki/pages/audio-channel-framing.md).
    let mut run = ScratchRun::spawn("[audio]\ninput_channels = 1\n", &[], true);
    run.wait_for("DSP: FT8 window #1", Duration::from_secs(60), |r| {
        r.log().contains("DSP: FT8 window #1")
    });

    let home = run.home.path();
    assert!(
        wavs_under(home, None).is_empty(),
        "no [audio.recording] section: a decoding run must write no WAV"
    );
    assert!(
        !home.join(".pancetta/recordings").exists(),
        "the pre-PAN-125 hardcoded recordings dir must not be created"
    );
    assert!(
        !home.join("Documents/Pancetta/Recordings").exists(),
        "the default recording directory must not be created either"
    );

    let log = run.log();
    assert!(
        log.contains("WAV recording disabled"),
        "startup must state the recording decision:\n{log}"
    );
    assert!(
        log.contains("audio.recording.auto_record = true"),
        "the disabled line must name the setting that enables recording:\n{log}"
    );
}

/// Scenario 2: an operator who enables recording gets it where they asked.
#[test]
fn enabled_recording_writes_windows_into_the_configured_directory() {
    let extra = "[audio]\ninput_channels = 1\n\
                 [audio.recording]\nauto_record = true\n\
                 directory = \"~/stub-recordings\"\n";
    let mut run = ScratchRun::spawn(extra, &[], true);
    let rec_dir = run.home.path().join("stub-recordings");
    run.wait_for(
        "a recorded window in ~/stub-recordings",
        Duration::from_secs(60),
        |r| {
            wavs_under(&rec_dir, None)
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ft8_")))
        },
    );

    let log = run.log();
    assert!(
        log.contains(&format!("WAV recording enabled: dir={}", rec_dir.display())),
        "the enabled line must name the expanded directory:\n{log}"
    );
    assert!(log.contains("cap=10.0 GB"), "the cap must stay at 10 GB:\n{log}");

    // Everything landed inside the configured directory; the old hardcoded
    // path and the default directory were never created.
    assert!(
        wavs_under(run.home.path(), Some(&rec_dir)).is_empty(),
        "nothing may be written outside the configured directory"
    );
    assert!(!run.home.path().join(".pancetta/recordings").exists());

    // The recording is a 12 kHz mono 16-bit WAV of one decode window.
    let recorded = wavs_under(&rec_dir, None);
    assert!(!recorded.is_empty());
    let spec = hound::WavReader::open(&recorded[0]).unwrap().spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, 12000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);
}

/// Scenario 3 (no-audio half): `--no-audio` never records, regardless of
/// config.
#[test]
fn no_audio_never_records_even_when_enabled() {
    let extra = "[audio.recording]\nauto_record = true\n\
                 directory = \"~/.pancetta/recordings\"\n";
    let mut run = ScratchRun::spawn(extra, &["--no-audio"], false);
    run.wait_for(
        "the --no-audio WAV suppression line",
        Duration::from_secs(60),
        |r| r.log().contains("WAV recording suppressed -- --no-audio"),
    );

    let home = run.home.path();
    assert!(
        !home.join(".pancetta/recordings").exists(),
        "--no-audio must not create the recordings directory"
    );
    assert!(
        wavs_under(home, None).is_empty(),
        "--no-audio must not write any WAV"
    );
}
