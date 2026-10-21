use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
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

    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Failed to read replay directory {}: {}", dir.display(), e))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to read an entry in replay directory {}: {} \
                 (a transient or failing mount can surface here -- a silently \
                 incomplete corpus would shift every later FT8 slot)",
                dir.display(),
                e
            )
        })?;

    let mut files: Vec<PathBuf> = entries
        .into_iter()
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
///
/// Rejects a corpus that yields no samples at all. `list_wav_files_sorted`
/// only proves the directory holds *files* with a `.wav` extension; a
/// directory of valid-but-zero-frame WAVs (or files so short that resampling
/// truncates them away) passes that check and then feeds nothing. The feeder
/// would sit through `REPLAY_GRACE_PERIOD` and exit 0, reporting a
/// "successful" replay that decoded nothing -- the same false success the
/// empty-directory check exists to prevent.
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

    if all_samples.is_empty() {
        anyhow::bail!(
            "Replay directory contains {} .wav file(s) but no audio samples: {}",
            files.len(),
            dir.display()
        );
    }

    Ok(all_samples)
}

/// Grace period after the last replay chunk is fed before triggering
/// shutdown -- long enough for the *final* slot's decode window to fire and
/// be decoded, short enough to keep `--replay` usable in CI and for VHS
/// recordings.
///
/// Derivation (FT8; see `coordinator/dsp.rs`): the DSP stage emits one decode
/// window per 15s slot at `slot_boundary + 13s` (`dsp_decode_phase`). Audio
/// fed after a slot's :13 emit point is only covered by the *next* slot's
/// window, so the worst-case wait from "last sample handed to DSP" to "the
/// window covering it fires" is one full slot period minus epsilon -- ~15s
/// (last sample landing at :13+e waits until the next :28). On top of that
/// the FT8 stage clamps each window to a hard 2000ms decode ceiling
/// (`decode_budget_ceiling_ms` in `coordinator/ft8.rs`), plus a little slack
/// for channel hand-off and the TUI's next render. 15 + 2 + margin => 20s.
///
/// This was 5s, which was shorter than the 13s decode phase alone: the last
/// slot of a replay could never reach the decoder before shutdown.
const REPLAY_GRACE_PERIOD: Duration = Duration::from_secs(20);

/// Wall-clock offset, measured from the instant the feed loop started, at
/// which the chunk *beginning* at sample index `samples_sent` is due.
///
/// Real-time pacing is expressed as an absolute deadline per chunk rather
/// than as a fixed-period timer so per-chunk rounding error can never
/// accumulate. The previous implementation computed a single millisecond
/// period -- `(buffer_size * 1000 / sample_rate) as u64` -- which *truncates*:
/// at the default 48000 Hz / 512-sample buffer the true period is 10.667ms
/// but the timer fired every 10ms, delivering audio 6.67% faster than real
/// time (~1s of drift per 15s FT8 slot). Because `coordinator/dsp.rs`
/// positions every decode window against the real wall clock
/// (`boundary_anchored_slice` keys off `now`), that drift walks the replayed
/// audio out from under the window the decoder extracts.
pub(crate) fn replay_chunk_deadline(samples_sent: u64, sample_rate: u32) -> Duration {
    debug_assert!(sample_rate > 0, "sample_rate must be non-zero");
    let nanos = samples_sent as u128 * 1_000_000_000u128 / sample_rate.max(1) as u128;
    Duration::from_nanos(nanos.min(u64::MAX as u128) as u64)
}

/// Expand a mono chunk into the interleaved multi-channel framing the DSP
/// stage expects, by duplicating each sample across all `channels` channels.
///
/// The DSP stage (`coordinator/dsp.rs`) unconditionally treats every buffer
/// arriving on `audio_to_dsp_tx` as interleaved `[c0, c1, c0, c1, ...]` frames
/// and extracts channel 0 with
/// `samples.chunks(input_channels).map(|ch| ch[0])` -- that is what a real
/// `cpal` capture stream delivers when `[audio] input_channels = 2` (the
/// project default; most rig CODECs are 2-channel).
///
/// The replay feeder produces genuinely mono audio. Handing that stream to the
/// DSP stage raw made it discard every second *real* sample while still
/// treating the survivors as spanning the same wall-clock interval: the
/// decoder saw audio at half the true sample rate, i.e. every tone shifted an
/// octave up and every symbol half as long. No FT8 decode is possible through
/// that corruption regardless of timing, decode effort, or signal quality --
/// which is exactly what `--replay` exhibited (`Msgs: 0`, forever).
///
/// Mimicking the hardware framing here, rather than special-casing replay
/// inside `dsp.rs`, keeps the DSP stage's live-audio contract untouched and
/// makes replay's output byte-for-byte what a real 2-channel capture of the
/// same mono content would look like.
pub(crate) fn interleave_mono(mono: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    if channels == 1 {
        return mono.to_vec();
    }
    let mut out = Vec::with_capacity(mono.len() * channels);
    for &sample in mono {
        for _ in 0..channels {
            out.push(sample);
        }
    }
    out
}

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

/// How often [`wait_for_qso_consumer`] re-checks the shutdown flag while the
/// QSO component is still seeding (the flag is a plain `AtomicBool`).
const QSO_CONSUMER_SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Why [`wait_for_qso_consumer`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QsoConsumerGate {
    /// The QSO task reached its receiver loop (or exited without reaching
    /// it): feed.
    Released,
    /// Shutdown was requested first: feed nothing.
    Shutdown,
}

/// PAN-42: hold the replay feed until the QSO component's task has finished
/// its worked-history seed and is about to drain `ComponentId::Qso`.
///
/// `run()` starts this feeder in `start_pipeline()` before it starts the QSO
/// component, and under `--replay` that task rebuilds an in-memory index from
/// the whole `qsos.adi` before its receiver loop starts. Without this wait a
/// large logbook let decoded frames queue unread behind the seed -- processed
/// late, or never if `REPLAY_GRACE_PERIOD` shutdown aborted the task first --
/// so replay results depended on logbook size.
pub(crate) async fn wait_for_qso_consumer(
    ready: &mut tokio::sync::watch::Receiver<bool>,
    shutdown: &std::sync::atomic::AtomicBool,
) -> QsoConsumerGate {
    if *ready.borrow_and_update() {
        return QsoConsumerGate::Released;
    }
    info!("Replay: waiting for the QSO component to finish seeding logbook history before starting feed");
    let started = tokio::time::Instant::now();
    loop {
        if shutdown.load(Ordering::Acquire) {
            return QsoConsumerGate::Shutdown;
        }
        match tokio::time::timeout(QSO_CONSUMER_SHUTDOWN_POLL, ready.wait_for(|r| *r)).await {
            Ok(Ok(_)) => {
                info!(
                    "Replay: QSO component ready after {:.1?}; starting feed",
                    started.elapsed()
                );
                return QsoConsumerGate::Released;
            }
            // Every sender is gone: nothing can ever release us, and a feed
            // that never starts would hang the run instead of ending it.
            Ok(Err(_)) => return QsoConsumerGate::Released,
            Err(_) => continue,
        }
    }
}

/// PAN-42: the QSO task's half of [`wait_for_qso_consumer`]. Stores `true`
/// on [`release`](Self::release) or on drop, whichever comes first, so an
/// early return, panic or abort before the receiver loop still releases the
/// feed (exactly today's behaviour on those paths) rather than hanging it.
pub(crate) struct QsoConsumerReadyGuard(tokio::sync::watch::Sender<bool>);

impl QsoConsumerReadyGuard {
    pub(crate) fn new(ready: tokio::sync::watch::Sender<bool>) -> Self {
        Self(ready)
    }

    /// Call immediately before the QSO task's receiver loop.
    pub(crate) fn release(self) {}
}

impl Drop for QsoConsumerReadyGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

impl super::ApplicationCoordinator {
    /// Feed every `.wav` file in `replay_dir` (in filename order) into the
    /// pipeline as if it were live audio: resampled to the configured audio
    /// sample rate, chunked to the configured buffer size, interleaved to
    /// `[audio] input_channels` (see [`interleave_mono`]), and paced in
    /// real time via the same `audio_to_dsp_tx` channel a real `cpal::Device`
    /// or `PANCETTA_STUB_AUDIO` would use. Before the feed loop starts, waits
    /// for the next true UTC slot boundary (see [`replay_preroll_wait`]) so
    /// the replayed audio's phase lines up with the live decoder's
    /// wall-clock-keyed 15s windows. Once the directory is exhausted,
    /// waits [`REPLAY_GRACE_PERIOD`] then triggers the same graceful
    /// shutdown Ctrl+C uses, so `--replay` is a finite, self-terminating
    /// mode suitable for scripted demos and automated tests. Before the
    /// pre-roll it also waits for the QSO component to finish seeding
    /// logbook history (see [`wait_for_qso_consumer`]), so no audio reaches
    /// the decoder while the QSO engine is still rebuilding a large
    /// `qsos.adi`.
    ///
    /// `health_audio_alive` is the same flag the real-device relay sets (see
    /// `audio.rs`); the replay feeder sets it too, so a `--replay` session
    /// does not render a false "AUDIO DEAD" alarm in the TUI health badge.
    pub(crate) async fn start_replay_pipeline(
        &mut self,
        replay_dir: PathBuf,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
        health_audio_alive: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        self.audio_path_supervised = false;

        let config = self.config.read().await;
        let sample_rate = config.audio.sample_rate;
        let buffer_size = config.audio.buffer_size as usize;
        // The DSP stage de-interleaves against this same setting; the feeder
        // must therefore emit frames, not bare mono samples. See
        // [`interleave_mono`].
        let input_channels = config.audio.input_channels as u16;
        drop(config);

        info!(
            "Starting audio component in REPLAY mode: {}",
            replay_dir.display()
        );

        // Fail fast on a rate the DSP stage cannot decimate.
        //
        // `pancetta-config` accepts rates the DSP worker rejects (44100 and
        // 22050 are both valid `[audio] sample_rate` values; see
        // `AudioConfig::validate_section`). The feeder resamples its corpus to
        // `[audio] sample_rate` so the DSP stage's channel-extraction and
        // decimation see exactly what a real capture device would deliver --
        // which means an incompatible rate is fed faithfully into a worker
        // that returns `unsupported_input_rate_message` and exits immediately.
        // The feeder then paces the whole corpus into a dead channel, waits
        // out REPLAY_GRACE_PERIOD and exits 0, while the supervisor restarts
        // the same doomed worker in the background: a "successful" replay with
        // zero decodes and no clear reason why. Bail here, before the feed
        // loop is spawned, so the operator gets the real cause.
        if !super::dsp::dsp_supports_input_rate(sample_rate) {
            anyhow::bail!(
                "--replay cannot run at the configured [audio] sample_rate. {} \
                 Replay resamples its corpus to the configured rate, so the DSP \
                 stage would reject it and decode nothing. Of the rates \
                 [audio] sample_rate itself accepts, use 48000 (the default), \
                 96000, or 192000.",
                super::dsp::unsupported_input_rate_message(sample_rate),
            );
        }

        let samples = load_replay_samples(&replay_dir, sample_rate)?;
        info!(
            "Replay: loaded {} samples ({:.1}s) at {} Hz",
            samples.len(),
            samples.len() as f64 / sample_rate as f64,
            sample_rate
        );

        let shutdown = self.shutdown_signal.clone();
        let last_timestamp = self.last_audio_timestamp.clone();
        let mut qso_consumer_ready = self.qso_consumer_ready.subscribe();

        let handle = tokio::spawn(async move {
            // PAN-42: seed first, then pre-roll, so the slot-boundary
            // alignment below is computed from when the feed can really start.
            if wait_for_qso_consumer(&mut qso_consumer_ready, &shutdown).await
                == QsoConsumerGate::Shutdown
            {
                info!("Replay: shutdown requested before the QSO component was ready; nothing fed");
                return Ok(());
            }
            let preroll_wait = replay_preroll_wait(chrono::Utc::now());
            if preroll_wait > Duration::ZERO {
                info!(
                    "Replay: waiting {:?} for next UTC slot boundary before starting feed",
                    preroll_wait
                );
            }
            tokio::time::sleep(preroll_wait).await;

            // Absolute-deadline pacing: every chunk's due time is recomputed
            // from `feed_start` plus the exact duration of all audio already
            // sent, so rounding is bounded at one nanosecond per chunk instead
            // of compounding. See [`replay_chunk_deadline`].
            let feed_start = tokio::time::Instant::now();

            let mut offset = 0usize;
            while offset < samples.len() && !shutdown.load(Ordering::Acquire) {
                tokio::time::sleep_until(
                    feed_start + replay_chunk_deadline(offset as u64, sample_rate),
                )
                .await;

                let end = (offset + buffer_size).min(samples.len());
                // Pacing (`offset`, `replay_chunk_deadline`) stays in mono
                // frames -- one frame is one instant of audio regardless of
                // how many channels carry it -- while the buffer handed to the
                // DSP stage is the interleaved expansion of those frames.
                let chunk = interleave_mono(&samples[offset..end], input_channels);
                offset = end;

                last_timestamp.store(super::now_epoch_ms(), Ordering::Relaxed);

                match super::pipeline::forward_or_drop_async(
                    &audio_to_dsp_tx,
                    chunk,
                    super::pipeline::DECODE_FORWARD_TIMEOUT,
                )
                .await
                {
                    super::pipeline::ForwardOutcome::Sent => {
                        // Mirror the real-device relay (`audio.rs`): audio is
                        // genuinely flowing into the DSP stage, so the health
                        // badge must not read "AUDIO DEAD". Deliberately never
                        // reset to false when the feed ends -- unlike a wedged
                        // capture device, an exhausted replay directory is the
                        // expected terminal state and shutdown follows within
                        // REPLAY_GRACE_PERIOD.
                        health_audio_alive.store(true, Ordering::Relaxed);
                    }
                    super::pipeline::ForwardOutcome::Dropped => {
                        warn!("Replay: DSP stage not draining -- dropped one batch");
                    }
                    super::pipeline::ForwardOutcome::Disconnected => break,
                }
            }

            info!(
                "Replay: fed {} samples ({:.2}s of audio) in {:.2}s wall-clock",
                offset,
                offset as f64 / sample_rate as f64,
                feed_start.elapsed().as_secs_f64(),
            );
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

    /// A directory of valid-but-empty WAVs passes `list_wav_files_sorted`
    /// (the files exist and end in `.wav`) but yields nothing to feed. Left
    /// unchecked the feeder waits out the grace period and exits 0 --
    /// a "successful" replay that processed no audio at all.
    #[test]
    fn load_replay_samples_rejects_corpus_with_no_audio_samples() {
        let dir = tempfile::tempdir().unwrap();
        write_test_wav(&dir.path().join("a_empty.wav"), 12000, 0);
        write_test_wav(&dir.path().join("b_empty.wav"), 12000, 0);

        // The files themselves are well-formed and discoverable...
        assert_eq!(list_wav_files_sorted(dir.path()).unwrap().len(), 2);

        // ...but a corpus with zero samples is not a replayable corpus.
        let err = load_replay_samples(dir.path(), 12000).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no audio samples"),
            "error should name the empty-corpus cause, got: {msg}"
        );
    }

    /// `--replay` resamples its corpus to `[audio] sample_rate`; a rate the
    /// DSP stage cannot decimate must be rejected before the feed loop starts
    /// (see `start_replay_pipeline`), not fed into a worker that exits.
    #[test]
    fn replay_rejects_sample_rates_the_dsp_stage_cannot_decimate() {
        use super::super::dsp::dsp_supports_input_rate;

        // Valid `[audio] sample_rate` values (per
        // `AudioConfig::validate_section`) that the DSP decimation path cannot
        // use -- these are the rates `--replay` must reject up front.
        for rate in [8000u32, 11025, 16000, 22050, 44100, 88200, 176400] {
            assert!(
                !dsp_supports_input_rate(rate),
                "{rate} Hz is not a whole multiple of 12000 Hz"
            );
        }
        // ...and the config-accepted rates that do decimate cleanly.
        for rate in [48000u32, 96000, 192000] {
            assert!(dsp_supports_input_rate(rate), "{rate} Hz must be accepted");
        }
    }

    /// Round-8 regression: a corrupt file anywhere in the corpus must abort
    /// the load, not be silently shortened and then have the NEXT file
    /// concatenated onto the gap -- which collapses archive time and shifts
    /// every subsequent FT8 frame's alignment while the run still exits 0.
    #[test]
    fn load_replay_samples_rejects_a_truncated_corpus_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("a_truncated.wav");
        write_test_wav(&bad, 12000, 500);
        write_test_wav(&dir.path().join("b_intact.wav"), 12000, 500);

        // Control: intact, the corpus loads and concatenates.
        assert_eq!(load_replay_samples(dir.path(), 12000).unwrap().len(), 1000);

        // Chop the tail off the first file's data chunk.
        let full_len = std::fs::metadata(&bad).unwrap().len();
        let file = std::fs::OpenOptions::new().write(true).open(&bad).unwrap();
        file.set_len(full_len - 200).unwrap();
        drop(file);

        let err = load_replay_samples(dir.path(), 12000).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("a_truncated.wav"),
            "error must name the corrupt corpus file, got: {msg}"
        );
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

    /// The regression this function exists for: the old
    /// `(buffer_size * 1000 / sample_rate) as u64` millisecond period
    /// truncated 10.667ms to 10ms at the project-default 48000 Hz / 512
    /// buffer, running replayed audio 6.67% fast. The absolute deadline for
    /// the Nth chunk must track true audio time, not a truncated period.
    #[test]
    fn replay_chunk_deadline_does_not_truncate_default_config_period() {
        const RATE: u32 = 48_000;
        const BUF: u64 = 512;

        // One buffer of audio at 48kHz is 10.6666...ms, not 10ms.
        let one_chunk = replay_chunk_deadline(BUF, RATE);
        assert_eq!(one_chunk, Duration::from_nanos(10_666_666));

        // The old truncating math would have put chunk 1407 (the last chunk of
        // a 15s slot) at 14.07s instead of 15.0s -- ~930ms of drift inside a
        // single FT8 slot. The deadline form must land on true audio time.
        let slot_chunks = 15 * RATE as u64 / BUF; // 1406.25 -> 1406 whole chunks
        let deadline = replay_chunk_deadline(slot_chunks * BUF, RATE);
        let drift = 15.0 - deadline.as_secs_f64();
        assert!(
            drift < 0.011,
            "one slot of chunks should be within one chunk of 15s, got {deadline:?}"
        );
    }

    #[test]
    fn replay_chunk_deadline_is_cumulative_and_starts_at_zero() {
        const RATE: u32 = 12_000;
        // First chunk is due immediately (matches the old `interval`, whose
        // first tick completed without delay).
        assert_eq!(replay_chunk_deadline(0, RATE), Duration::ZERO);
        // Deadlines are absolute offsets from feed start, so they scale
        // linearly in the number of samples already sent -- there is no
        // per-chunk residual to accumulate.
        assert_eq!(replay_chunk_deadline(12_000, RATE), Duration::from_secs(1));
        assert_eq!(
            replay_chunk_deadline(120_000, RATE),
            Duration::from_secs(10)
        );
        assert_eq!(
            replay_chunk_deadline(6_000, RATE),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn replay_grace_period_covers_final_slot_decode() {
        // The DSP stage emits a slot's decode window at boundary+13s and the
        // FT8 stage clamps each window to a 2000ms ceiling; the worst-case
        // wait from last-sample-fed to window-fired is one full 15s slot.
        // The grace period must exceed 15s + 2s or the last slot of a replay
        // can never reach the decoder before shutdown.
        assert!(
            REPLAY_GRACE_PERIOD >= Duration::from_secs(17),
            "grace period {REPLAY_GRACE_PERIOD:?} is shorter than one slot plus the decode ceiling"
        );
    }

    /// The exact de-interleave `coordinator/dsp.rs` performs on every buffer
    /// it receives. Duplicated here (rather than imported) so this test pins
    /// the *contract* between the two stages: whatever the feeder emits, this
    /// operation must recover the original mono stream unchanged.
    fn dsp_extract_channel_zero(buffer: &[f32], input_channels: u16) -> Vec<f32> {
        if input_channels > 1 {
            buffer
                .chunks(input_channels as usize)
                .map(|ch| ch[0])
                .collect()
        } else {
            buffer.to_vec()
        }
    }

    #[test]
    fn interleave_mono_round_trips_through_dsp_channel_extraction() {
        let mono: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();

        // The regression: feeding raw mono while the DSP stage de-interleaves
        // against input_channels=2 silently drops every second real sample.
        let raw_mono_through_dsp = dsp_extract_channel_zero(&mono, 2);
        assert_eq!(
            raw_mono_through_dsp.len(),
            mono.len() / 2,
            "raw mono handed to a 2-channel DSP stage must lose half its samples \
             -- this is the bug the interleaving fix exists to prevent"
        );

        // With interleaving, every channel count round-trips exactly.
        for channels in [1u16, 2, 4, 8] {
            let framed = interleave_mono(&mono, channels);
            assert_eq!(framed.len(), mono.len() * channels as usize);
            assert_eq!(
                dsp_extract_channel_zero(&framed, channels),
                mono,
                "input_channels={channels} must round-trip the mono stream unchanged"
            );
        }
    }

    #[test]
    fn interleave_mono_duplicates_each_sample_across_every_channel() {
        assert_eq!(
            interleave_mono(&[1.0, 2.0, 3.0], 2),
            vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0]
        );
        // Mono config is a pass-through, not a copy of a copy.
        assert_eq!(interleave_mono(&[1.0, 2.0], 1), vec![1.0, 2.0]);
        // A zero channel count is rejected by config validation
        // (pancetta-config/src/audio.rs), but must not panic or produce an
        // empty buffer if it ever reaches here.
        assert_eq!(interleave_mono(&[1.0, 2.0], 0), vec![1.0, 2.0]);
        assert!(interleave_mono(&[], 2).is_empty());
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

    // ---- PAN-42: QSO-consumer gate ----

    #[tokio::test]
    async fn pan42_gate_holds_the_feed_until_the_qso_consumer_is_ready() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sd = shutdown.clone();
        let gate = tokio::spawn(async move { wait_for_qso_consumer(&mut rx, &sd).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !gate.is_finished(),
            "the feed must wait while the QSO task is seeding"
        );
        tx.send_replace(true);
        let out = tokio::time::timeout(Duration::from_secs(2), gate)
            .await
            .expect("gate must open promptly once the QSO consumer is ready")
            .unwrap();
        assert_eq!(out, QsoConsumerGate::Released);
    }

    #[tokio::test]
    async fn pan42_gate_returns_shutdown_while_waiting() {
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sd = shutdown.clone();
        let gate = tokio::spawn(async move { wait_for_qso_consumer(&mut rx, &sd).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.store(true, Ordering::Release);
        let out = tokio::time::timeout(Duration::from_secs(2), gate)
            .await
            .expect("Ctrl-C during the seed must not hang the feeder")
            .unwrap();
        assert_eq!(out, QsoConsumerGate::Shutdown);
    }

    #[tokio::test]
    async fn pan42_gate_passes_straight_through_once_already_ready() {
        let (tx, mut rx) = tokio::sync::watch::channel(true);
        let shutdown = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            wait_for_qso_consumer(&mut rx, &shutdown).await,
            QsoConsumerGate::Released
        );
        drop(tx);
    }

    #[tokio::test]
    async fn pan42_guard_releases_on_drop_without_explicit_release() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        {
            let _guard = QsoConsumerReadyGuard::new(tx.clone());
            assert!(!*rx.borrow());
        }
        assert!(*rx.borrow(), "an early exit must still release the feed");
        let guard = QsoConsumerReadyGuard::new(tx);
        guard.release();
        assert!(*rx.borrow());
    }

    /// PAN-42: with the QSO consumer never becoming ready, the real feeder
    /// must feed nothing and exit promptly on shutdown instead of sleeping
    /// through the pre-roll and grace period.
    #[tokio::test]
    async fn pan42_feeder_feeds_nothing_while_the_qso_consumer_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        write_test_wav(&dir.path().join("a.wav"), 12000, 12000);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut coordinator = super::super::ApplicationCoordinator::new(
            pancetta_config::Config::default(),
            None,
            true,  // no_audio (start_replay_pipeline is called directly below)
            true,  // headless
            false, // metrics
            9090,
            None,                           // no WAV
            Some(dir.path().to_path_buf()), // --replay
            None,                           // no test-tx
            1500.0,
            shutdown.clone(),
            Vec::new(),
            std::env::temp_dir().join("pancetta-test-config.toml"),
            true,
        )
        .await
        .unwrap();
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<f32>>(1024);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
        coordinator
            .start_replay_pipeline(dir.path().to_path_buf(), audio_tx, alive.clone())
            .await
            .unwrap();
        let (_, handle) = coordinator.named_task_handles.pop().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("feeder must exit promptly when shut down before the QSO consumer is ready")
            .unwrap()
            .unwrap();
        assert!(
            audio_rx.try_recv().is_err(),
            "no audio may be fed before the QSO consumer is ready"
        );
        assert!(!alive.load(Ordering::Relaxed));
    }
}
