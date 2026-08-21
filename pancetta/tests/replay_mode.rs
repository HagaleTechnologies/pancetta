//! `--replay <dir>` integration test: points the real binary at a tiny
//! synthetic WAV directory and confirms it runs the full pipeline and
//! exits cleanly on its own once the input is exhausted (see
//! `REPLAY_GRACE_PERIOD` in `coordinator/replay.rs`).

use assert_cmd::Command;
use std::time::Duration;

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

#[test]
fn replay_mode_runs_full_pipeline_and_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    write_short_wav(&dir.path().join("a_first.wav"), 12000, 1.0);
    write_short_wav(&dir.path().join("b_second.wav"), 12000, 1.0);

    // Worst-case process lifetime: startup + up to 15s of UTC slot-boundary
    // pre-roll (`replay_preroll_wait`) + 2s of audio + REPLAY_GRACE_PERIOD
    // (20s, sized to cover the final slot's boundary+13s decode window plus
    // the 2000ms decode ceiling) ≈ 39s. 60s leaves headroom for a loaded CI
    // runner without letting a genuine hang run forever.
    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--replay"])
        .arg(dir.path())
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
    let dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--no-audio", "--replay"])
        .arg(dir.path())
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
    let dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--test-tx", "N0CALL N0CALL 73", "--replay"])
        .arg(dir.path())
        .timeout(Duration::from_secs(15));

    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"))
        .stderr(predicates::str::contains("--test-tx"));
}

#[test]
fn replay_mode_rejects_empty_directory() {
    let dir = tempfile::tempdir().unwrap();

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--replay"])
        .arg(dir.path())
        .timeout(Duration::from_secs(15));

    cmd.assert().failure();
}
