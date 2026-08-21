//! `--replay <dir>` integration test: points the real binary at a tiny
//! synthetic WAV directory and confirms it runs the full pipeline and
//! exits cleanly on its own once the input is exhausted (see
//! `REPLAY_GRACE_PERIOD` in `coordinator/replay.rs`).

use assert_cmd::Command;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

fn write_short_wav(path: &std::path::Path, sample_rate: u32, seconds: f32) {
    let n_samples = (sample_rate as f32 * seconds) as usize;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..n_samples {
        let t = i as f32 / sample_rate as f32;
        let v = (0.2 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * i16::MAX as f32) as i16;
        writer.write_sample(v).unwrap();
    }
    writer.finalize().unwrap();
}

/// A throwaway `$HOME` plus an explicit scratch `pancetta.toml`.
///
/// Every test in this file spawns the *real* binary. Left to inherit the
/// developer's environment it would resolve `~/.pancetta/pancetta.toml` and
/// run against the operator's actual station config — enabled uploads,
/// non-default audio, a real callsign — and would read and write the operator's
/// real `~/.pancetta/qsos.adi`, `qso.db`, `tui_state.json` and log directory
/// (every one of those paths is derived from `dirs::home_dir()` at runtime; see
/// `coordinator/mod.rs` and `coordinator/qso.rs`). That makes the test both
/// stateful and destructive to real station data, and flaky in proportion to
/// how customized the developer's config is. Pinning HOME *and* passing an
/// explicit `--config` closes both the config path and the data paths.
struct ScratchStation {
    home: TempDir,
    config: PathBuf,
}

impl ScratchStation {
    /// `extra_toml` is appended to the minimal station config, for tests that
    /// need a specific setting (e.g. `[audio] sample_rate`).
    fn new(extra_toml: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let dot_pancetta = home.path().join(".pancetta");
        std::fs::create_dir_all(&dot_pancetta).unwrap();

        let config = dot_pancetta.join("pancetta.toml");
        std::fs::write(
            &config,
            format!("[station]\ncallsign = \"K5ARH\"\ngrid_square = \"EM12\"\n{extra_toml}"),
        )
        .unwrap();

        Self { home, config }
    }

    /// The real binary, wired to this scratch station and to nothing else.
    fn command(&self) -> Command {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.env("HOME", self.home.path())
            // `dirs` consults these before `$HOME` on Linux/macOS; a developer
            // (or CI image) that sets them would otherwise punch straight
            // through the HOME override.
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_DATA_HOME", self.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            // `dirs::home_dir()` uses the profile directory on Windows.
            .env("USERPROFILE", self.home.path())
            .arg("--config")
            .arg(&self.config);
        cmd
    }

    fn wav_dir(&self) -> PathBuf {
        let dir = self.home.path().join("replay-corpus");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

/// A path inside the scratch home, for conflict tests where clap rejects the
/// invocation before the path is ever opened.
fn unused_path(station: &ScratchStation) -> PathBuf {
    station.home.path().to_path_buf()
}

#[test]
fn replay_mode_runs_full_pipeline_and_exits_cleanly() {
    let station = ScratchStation::new("");
    let dir = station.wav_dir();
    write_short_wav(&dir.join("a_first.wav"), 12000, 1.0);
    write_short_wav(&dir.join("b_second.wav"), 12000, 1.0);

    // Worst-case process lifetime: startup + up to 15s of UTC slot-boundary
    // pre-roll (`replay_preroll_wait`) + 2s of audio + REPLAY_GRACE_PERIOD
    // (20s, sized to cover the final slot's boundary+13s decode window plus
    // the 2000ms decode ceiling) ≈ 39s. 60s leaves headroom for a loaded CI
    // runner without letting a genuine hang run forever.
    let mut cmd = station.command();
    cmd.args(["--headless", "--replay"])
        .arg(&dir)
        .timeout(Duration::from_secs(60));

    cmd.assert().success();
}

/// `--no-audio` short-circuits `start_audio_pipeline` before the replay
/// branch, so the combination used to spawn no feeder at all -- nothing ever
/// set the shutdown signal and the process hung indefinitely. clap now
/// rejects the pair at parse time.
#[test]
fn replay_mode_conflicts_with_no_audio() {
    // No WAV content needed: `conflicts_with` is enforced by clap at parse
    // time, so the directory is never opened -- it only has to be a path.
    let station = ScratchStation::new("");

    let mut cmd = station.command();
    cmd.args(["--headless", "--no-audio", "--replay"])
        .arg(unused_path(&station))
        .timeout(Duration::from_secs(15));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"))
        .stderr(predicates::str::contains("--no-audio"));
}

/// `--test-tx` unconditionally sets the shutdown signal 35s after injecting
/// its frame, truncating any replay corpus longer than that (including the
/// bundled 75s demo corpus) before its final decodes -- and it can't
/// validate real TX either, since `--replay` deliberately skips Hamlib
/// startup. clap now rejects the pair at parse time.
#[test]
fn replay_mode_conflicts_with_test_tx() {
    // No WAV content needed: `conflicts_with_all` is enforced by clap at
    // parse time, so the directory is never opened -- it only has to be a
    // path.
    let station = ScratchStation::new("");

    let mut cmd = station.command();
    cmd.args(["--headless", "--test-tx", "N0CALL N0CALL 73", "--replay"])
        .arg(unused_path(&station))
        .timeout(Duration::from_secs(15));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"))
        .stderr(predicates::str::contains("--test-tx"));
}

/// `ApplicationCoordinator::run` checks `wav_path` before the replay branch,
/// so `--wav X --replay Y` silently ran single-file decode-and-exit playback
/// and ignored the requested replay pipeline -- an undocumented precedence
/// between two genuinely different modes. clap now rejects the pair at parse
/// time.
#[test]
fn replay_mode_conflicts_with_wav() {
    // Neither path is opened: `conflicts_with_all` fires at parse time.
    let station = ScratchStation::new("");

    let mut cmd = station.command();
    cmd.args(["--headless", "--wav"])
        .arg(station.home.path().join("does-not-matter.wav"))
        .arg("--replay")
        .arg(unused_path(&station))
        .timeout(Duration::from_secs(15));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"))
        .stderr(predicates::str::contains("--wav"));
}

#[test]
fn replay_mode_rejects_empty_directory() {
    let station = ScratchStation::new("");

    let mut cmd = station.command();
    cmd.args(["--headless", "--replay"])
        .arg(station.wav_dir())
        .timeout(Duration::from_secs(15));

    cmd.assert().failure();
}

/// A corpus of well-formed but zero-frame WAVs used to be a false success:
/// the feeder had nothing to send, waited out `REPLAY_GRACE_PERIOD` and exited
/// 0, reporting a "successful" replay that processed no audio at all.
#[test]
fn replay_mode_rejects_corpus_with_no_samples() {
    let station = ScratchStation::new("");
    let dir = station.wav_dir();
    write_short_wav(&dir.join("a_empty.wav"), 12000, 0.0);
    write_short_wav(&dir.join("b_empty.wav"), 12000, 0.0);

    let mut cmd = station.command();
    cmd.args(["--headless", "--replay"])
        .arg(&dir)
        .timeout(Duration::from_secs(30));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("no audio samples"));
}

/// 44100 Hz is a valid `[audio] sample_rate` (`AudioConfig::validate_section`
/// accepts it) that the DSP stage cannot decimate to 12 kHz. Replay resamples
/// its corpus to the configured rate, so the DSP worker would reject the input
/// and exit while the feeder paced the whole corpus into a dead channel and
/// then exited 0 -- a run that reports success having decoded nothing, with the
/// supervisor restarting the same doomed worker in the background. It must fail
/// fast with the reason instead.
#[test]
fn replay_mode_rejects_dsp_incompatible_sample_rate() {
    let station = ScratchStation::new("[audio]\nsample_rate = 44100\n");
    let dir = station.wav_dir();
    write_short_wav(&dir.join("a_first.wav"), 12000, 1.0);

    let mut cmd = station.command();
    cmd.args(["--headless", "--replay"])
        .arg(&dir)
        // Must fail before the feed loop starts: well under the ~39s a
        // successful (or falsely-successful) replay of this corpus would take.
        .timeout(Duration::from_secs(30));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("44100"))
        .stderr(predicates::str::contains("12000"))
        .stderr(predicates::str::contains("sample_rate"));
}

/// Guard for the isolation above: the scratch config really is what the
/// binary loads, and the operator's real home is never consulted.
#[test]
fn scratch_station_config_is_the_one_loaded() {
    let station = ScratchStation::new("");

    let mut cmd = station.command();
    cmd.args(["config", "--show"])
        .timeout(Duration::from_secs(30));

    cmd.assert()
        .success()
        .stdout(predicates::str::contains("K5ARH"));
}
