use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::interval;
use tracing::{info, warn};

use super::util::resample_linear;
use super::wav_playback::read_wav_mono;

/// List `.wav` files in `dir`, sorted by filename. Sequential-capture
/// filenames (`170923_082000.wav`, `170923_082015.wav`, ...) sort
/// chronologically this way; callers rely on that order to replay a
/// directory as a continuous recording.
pub(crate) fn list_wav_files_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        anyhow::bail!("--replay path is not a directory: {}", dir.display());
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Failed to read replay directory {}: {}", dir.display(), e))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("wav"))
                .unwrap_or(false)
        })
        .collect();

    if files.is_empty() {
        anyhow::bail!("No .wav files found in replay directory: {}", dir.display());
    }

    files.sort();
    Ok(files)
}

/// Read every `.wav` file in `dir` (in filename order), mix each to mono,
/// resample each to `target_rate`, and concatenate into one continuous
/// sample stream -- as if the directory were one long recording.
pub(crate) fn load_replay_samples(dir: &Path, target_rate: u32) -> Result<Vec<f32>> {
    let files = list_wav_files_sorted(dir)?;

    let mut all_samples = Vec::new();
    for path in &files {
        let (mono, native_rate) = read_wav_mono(path)?;
        let resampled = if native_rate != target_rate {
            resample_linear(&mono, native_rate, target_rate)
        } else {
            mono
        };
        all_samples.extend(resampled);
    }

    Ok(all_samples)
}

/// Grace period after the last replay chunk is fed before triggering
/// shutdown -- long enough for in-flight FT8 decode windows to finish,
/// short enough to keep `--replay` usable in CI and for VHS recordings.
const REPLAY_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// How long to wait, from `now`, until the next true UTC FT8 slot boundary
/// (:00/:15/:30/:45). Replayed audio is a real off-air capture naturally
/// aligned to slot boundaries when it was recorded; the live pipeline's
/// decoder buffers incoming audio into windows keyed to the real wall clock,
/// so starting the feed loop mid-slot would misalign every subsequent
/// window. Waiting for the next boundary re-establishes that alignment.
pub(crate) fn replay_preroll_wait(now: chrono::DateTime<chrono::Utc>) -> Duration {
    let next_boundary = pancetta_core::slot::next_slot_start(now, chrono::Duration::zero());
    pancetta_core::slot::duration_until(next_boundary, now)
}

impl super::ApplicationCoordinator {
    /// Feed every `.wav` file in `replay_dir` (in filename order) into the
    /// pipeline as if it were live audio: resampled to the configured audio
    /// sample rate, chunked to the configured buffer size, and paced in
    /// real time via the same `audio_to_dsp_tx` channel a real `cpal::Device`
    /// or `PANCETTA_STUB_AUDIO` would use. Before the feed loop starts, waits
    /// for the next true UTC slot boundary (see [`replay_preroll_wait`]) so
    /// the replayed audio's phase lines up with the live decoder's
    /// wall-clock-keyed 15s windows. Once the directory is exhausted,
    /// waits [`REPLAY_GRACE_PERIOD`] then triggers the same graceful
    /// shutdown Ctrl+C uses, so `--replay` is a finite, self-terminating
    /// mode suitable for scripted demos and automated tests.
    pub(crate) async fn start_replay_pipeline(
        &mut self,
        replay_dir: PathBuf,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
    ) -> Result<()> {
        self.audio_path_supervised = false;

        let config = self.config.read().await;
        let sample_rate = config.audio.sample_rate;
        let buffer_size = config.audio.buffer_size as usize;
        drop(config);

        info!(
            "Starting audio component in REPLAY mode: {}",
            replay_dir.display()
        );
        let samples = load_replay_samples(&replay_dir, sample_rate)?;
        info!(
            "Replay: loaded {} samples ({:.1}s) at {} Hz",
            samples.len(),
            samples.len() as f64 / sample_rate as f64,
            sample_rate
        );

        let shutdown = self.shutdown_signal.clone();
        let last_timestamp = self.last_audio_timestamp.clone();

        let handle = tokio::spawn(async move {
            let preroll_wait = replay_preroll_wait(chrono::Utc::now());
            if preroll_wait > Duration::ZERO {
                info!(
                    "Replay: waiting {:?} for next UTC slot boundary before starting feed",
                    preroll_wait
                );
            }
            tokio::time::sleep(preroll_wait).await;

            let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
            let mut process_interval = interval(Duration::from_millis(buffer_duration_ms.max(5)));

            let mut offset = 0usize;
            while offset < samples.len() && !shutdown.load(Ordering::Acquire) {
                process_interval.tick().await;

                let end = (offset + buffer_size).min(samples.len());
                let chunk = samples[offset..end].to_vec();
                offset = end;

                last_timestamp.store(super::now_epoch_ms(), Ordering::Relaxed);

                match super::pipeline::forward_or_drop_async(
                    &audio_to_dsp_tx,
                    chunk,
                    super::pipeline::DECODE_FORWARD_TIMEOUT,
                )
                .await
                {
                    super::pipeline::ForwardOutcome::Sent => {}
                    super::pipeline::ForwardOutcome::Dropped => {
                        warn!("Replay: DSP stage not draining -- dropped one batch");
                    }
                    super::pipeline::ForwardOutcome::Disconnected => break,
                }
            }

            info!(
                "Replay complete -- waiting {:?} before shutdown",
                REPLAY_GRACE_PERIOD
            );
            tokio::time::sleep(REPLAY_GRACE_PERIOD).await;
            shutdown.store(true, Ordering::Release);
            info!("Replay: triggered shutdown");

            Ok(())
        });

        self.named_task_handles
            .push((crate::message_bus::ComponentId::Audio, handle));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn write_test_wav(path: &Path, sample_rate: u32, n_samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..n_samples {
            let v = ((i % 100) as i16) - 50;
            writer.write_sample(v).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn list_wav_files_sorted_orders_by_filename_and_ignores_non_wav() {
        let dir = tempfile::tempdir().unwrap();
        write_test_wav(&dir.path().join("170923_082030.wav"), 12000, 10);
        write_test_wav(&dir.path().join("170923_082000.wav"), 12000, 10);
        write_test_wav(&dir.path().join("170923_082015.wav"), 12000, 10);
        std::fs::write(dir.path().join("notes.txt"), b"ignore me").unwrap();

        let files = list_wav_files_sorted(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "170923_082000.wav",
                "170923_082015.wav",
                "170923_082030.wav",
            ]
        );
    }

    #[test]
    fn list_wav_files_sorted_rejects_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_wav_files_sorted(dir.path()).is_err());
    }

    #[test]
    fn list_wav_files_sorted_rejects_non_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("not_a_dir.wav");
        write_test_wav(&file_path, 12000, 10);
        assert!(list_wav_files_sorted(&file_path).is_err());
    }

    #[test]
    fn load_replay_samples_concatenates_files_in_order_and_resamples() {
        let dir = tempfile::tempdir().unwrap();
        // Native rate 8000 Hz; request 12000 Hz target so resampling is exercised.
        write_test_wav(&dir.path().join("a_first.wav"), 8000, 80);
        write_test_wav(&dir.path().join("b_second.wav"), 8000, 80);

        let samples = load_replay_samples(dir.path(), 12000).unwrap();

        // 80 samples @ 8000 Hz resampled to 12000 Hz -> 120 samples each (linear
        // resampler truncates via integer division: 80 * 12000 / 8000 = 120).
        assert_eq!(samples.len(), 240);
    }

    #[test]
    fn replay_preroll_wait_lands_on_next_boundary_mid_slot() {
        // 7s into a slot that started at :00:00 -> next boundary is :00:15,
        // so the wait should be exactly 8s.
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 7).unwrap();
        let wait = replay_preroll_wait(now);
        assert_eq!(wait, Duration::from_secs(8));
    }

    #[test]
    fn replay_preroll_wait_on_exact_boundary_waits_a_full_slot() {
        // next_slot_start with a zero min_lead always advances to the *next*
        // boundary even when `now` is exactly on one (see
        // pancetta-core/src/slot.rs `slot_boundary_aligned_picks_next`), so
        // starting replay exactly at :00:00 should wait the full 15s.
        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let wait = replay_preroll_wait(now);
        assert_eq!(wait, Duration::from_secs(15));
    }
}
