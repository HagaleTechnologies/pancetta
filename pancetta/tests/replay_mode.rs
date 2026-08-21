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

    let mut cmd = Command::cargo_bin("pancetta").unwrap();
    cmd.args(["--headless", "--replay"])
        .arg(dir.path())
        .timeout(Duration::from_secs(30));

    cmd.assert().success();
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
