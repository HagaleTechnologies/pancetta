//! DSP component startup.
//!
//! Reads 48 kHz audio batches from the audio relay, decimates to 12 kHz
//! mono via simple subsampling (FT8 sits well below the 6 kHz Nyquist),
//! and emits FT8-aligned 12.64-second windows aligned to UTC slot
//! boundaries (:00/:15/:30/:45 + 13 s for headroom).
//!
//! In parallel, computes a 1 Hz waterfall via a 2048-point Hann-windowed
//! FFT and an audio-level RMS meter — both forwarded to the TUI.
//!
//! Validation captures (PAN-125): only when
//! `[audio.recording] auto_record = true` on a live-audio run, every full
//! window is written to `audio.recording.directory` (a leading `~`
//! expanded; empty means `~/.pancetta/recordings`) as `ft8_*.wav` for
//! off-line decoder comparison — `--replay` and `--no-audio` never record.
//! Disk usage is capped at 10 GB; oldest files are evicted when the cap is
//! hit. The decision is made once per start by [`resolve_wav_recording`].
//!
//! Task W5.3 (decoder-tp-sensitivity plan): the emitted window's lead-in
//! before the slot boundary — real retained audio, not zero-padding — is
//! [`super::WINDOW_LEAD_SECS`] by default, widened to
//! [`super::EXTENDED_WINDOW_LEAD_SECS`] when
//! `[decoder].extended_capture_window_enabled` is set. See
//! [`boundary_anchored_slice`] and `resolve_window_lead_secs` in
//! `coordinator/mod.rs`.

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{debug, info, span, warn, Level};

use crate::message_bus::ComponentId;

/// Maximum total disk space for WAV recordings (bytes).
const WAV_RECORDING_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GB

/// Sample rate the FT8 stages consume. The DSP worker reaches it by *integer*
/// subsampling of the capture stream (`decimation_factor = input_rate / 12000`),
/// so the configured input rate must be a whole multiple of this.
pub(crate) const DSP_TARGET_SAMPLE_RATE_HZ: u32 = 12_000;

/// Whether `rate` can drive the DSP decimation path in
/// [`start_dsp_pipeline`].
///
/// `pancetta-config` accepts a wider set of rates than the DSP stage can
/// decimate (44100/22050/... are all valid `[audio] sample_rate` values), so
/// this predicate is the narrower, DSP-specific constraint. It is shared with
/// the `--replay` feeder (`coordinator/replay.rs`), which resamples its corpus
/// to `[audio] sample_rate` and would otherwise hand the DSP worker an input it
/// rejects — the worker would exit and the run would "succeed" with zero
/// decodes.
pub(crate) fn dsp_supports_input_rate(rate: u32) -> bool {
    rate >= DSP_TARGET_SAMPLE_RATE_HZ && rate.is_multiple_of(DSP_TARGET_SAMPLE_RATE_HZ)
}

/// The operator-facing explanation for a rate [`dsp_supports_input_rate`]
/// rejects. Shared so the DSP worker's error and `--replay`'s fail-fast bail
/// name the same constraint and the same supported rates.
///
/// The rate list here is the intersection of what `AudioConfig::validate_section`
/// accepts for `[audio] sample_rate` and what's a whole multiple of
/// `DSP_TARGET_SAMPLE_RATE_HZ` -- naming a rate here that config validation
/// would itself reject (e.g. 12000, 24000) would just trade one error for
/// another.
pub(crate) fn unsupported_input_rate_message(rate: u32) -> String {
    format!(
        "Audio sample rate {rate} Hz is not evenly divisible by {DSP_TARGET_SAMPLE_RATE_HZ} Hz. \
         Of the [audio] sample_rate values pancetta accepts, use 48000 (the default), 96000, or 192000."
    )
}

/// Peak absolute amplitude of an f32 audio window (0.0 if empty).
fn window_peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// A window is "dead air" (rig off/unplugged → digital silence) when its peak
/// is below this threshold. Empirically: silence ≈ 0.0002, real FT8 ≥ 0.05.
const DEAD_AIR_PEAK_THRESHOLD: f32 = 0.001;

fn is_dead_air(samples: &[f32]) -> bool {
    window_peak(samples) < DEAD_AIR_PEAK_THRESHOLD
}

/// Map a rig dial frequency (Hz) to a ham-band label for recording
/// filenames. Returns `None` outside the standard HF/VHF amateur bands
/// (or when the frequency is unknown, i.e. 0 — no rig / not yet polled),
/// in which case the filename carries no band suffix and stays backward
/// compatible with the legacy `ft8_<date>_<time>.wav` form.
fn band_for_freq_hz(hz: u64) -> Option<&'static str> {
    let khz = hz / 1000;
    Some(match khz {
        1_800..=2_000 => "160m",
        3_500..=4_000 => "80m",
        5_300..=5_410 => "60m",
        7_000..=7_300 => "40m",
        10_100..=10_150 => "30m",
        14_000..=14_350 => "20m",
        18_068..=18_168 => "17m",
        21_000..=21_450 => "15m",
        24_890..=24_990 => "12m",
        28_000..=29_700 => "10m",
        50_000..=54_000 => "6m",
        144_000..=148_000 => "2m",
        _ => return None,
    })
}

/// Decide whether the in-flight audio window must be flushed because the rig
/// dial moved, and what the new reference dial is (PAN-70).
///
/// `last_known_hz` is the last nonzero dial reading the DSP observed — the
/// dial the audio buffered since the last flush is attributed to (0 = not yet
/// established). `current_hz` is the dial read this iteration (0 = no rig /
/// not yet polled).
///
/// Returns `(new_last_known_hz, should_flush)`:
///   - A `current_hz == 0` read keeps the reference and the buffer: an
///     unknown reading is not evidence that the dial moved.
///   - When `last_known_hz == 0`, the reference seeds to `current_hz` and no
///     flush is requested (there is no prior dial to have captured on).
///   - Any other inequality, however small and whether or not it crosses a
///     band edge, flushes and advances the reference. A window may only carry
///     one dial snapshot, so audio buffered on the old dial must never be
///     emitted under the new one.
///
/// The comparison is exact on purpose: no deadband. See
/// `docs/DECISIONS/config-and-platform.md` (PAN-70) for the rejected 100 Hz
/// tolerance and the accepted decode loss while the operator tunes.
fn dial_flush_decision(last_known_hz: u64, current_hz: u64) -> (u64, bool) {
    if current_hz == 0 {
        return (last_known_hz, false);
    }
    if last_known_hz == 0 {
        return (current_hz, false);
    }
    (current_hz, current_hz != last_known_hz)
}

/// What [`apply_dial_change`] discarded when an observed dial change flushed
/// the capture buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DialFlush {
    /// The reference dial the discarded audio was attributed to.
    previous_hz: u64,
    /// Samples cleared from the capture buffer, including the batch that
    /// exposed the change.
    discarded_samples: usize,
    /// The move also crossed a band edge (per [`super::is_band_change`]), so
    /// the waterfall noise-floor history was cleared too.
    band_changed: bool,
}

/// Apply this batch's dial reading to the DSP capture state (PAN-70).
///
/// Called once per audio batch, after the batch is appended to `ft8_buffer`
/// and before the waterfall row or decode window is built. On an observed
/// change between two known dials ([`dial_flush_decision`]) it clears every
/// buffered sample — the batch that exposed the change included, since its
/// acquisition dial is unknown — and restarts the waterfall sample cursor.
/// `bin_history` (the waterfall noise floor) is cleared only when the move
/// crosses a band edge, judged against the OLD reference; an intra-band nudge
/// keeps the floor warm. The reference always advances to the decision's
/// result.
///
/// The guarantee covers DSP-observed transitions only: audio batches carry no
/// dial tag, so audio queued upstream before a retune, CAT readback latency,
/// and an A→B→A move that completes between two reads are not detected.
fn apply_dial_change(
    cur_dial_hz: u64,
    last_known_dial_hz: &mut u64,
    ft8_buffer: &mut Vec<f32>,
    last_live_wf_samples: &mut usize,
    bin_history: &mut Vec<std::collections::VecDeque<f32>>,
) -> Option<DialFlush> {
    let previous_hz = *last_known_dial_hz;
    let (new_ref, flush) = dial_flush_decision(previous_hz, cur_dial_hz);
    *last_known_dial_hz = new_ref;
    if !flush {
        return None;
    }
    let band_changed = super::is_band_change(previous_hz, cur_dial_hz);
    let discarded_samples = ft8_buffer.len();
    ft8_buffer.clear();
    *last_live_wf_samples = 0;
    if band_changed {
        // A new band's spectrum must not be judged against the old band's
        // noise floor.
        bin_history.clear();
    }
    Some(DialFlush {
        previous_hz,
        discarded_samples,
        band_changed,
    })
}

/// Decide whether the in-flight FT8 audio window must be flushed because the
/// active operating mode changed (the operator cycled FT8/FT4/FT2 live).
///
/// `cached_mode`/`cur_mode` are `pancetta_config::OperatingMode::as_u8()`
/// values. Unlike [`dial_flush_decision`] there is no "unknown baseline"
/// case — the DSP thread always starts with a real mode read from config at
/// spawn time, so a simple inequality is the whole decision.
fn mode_flush_decision(cached_mode: u8, cur_mode: u8) -> bool {
    cached_mode != cur_mode
}

/// PAN-114: re-anchor the decode-window trigger after a wall-clock jump.
///
/// `next_window_time` is always "the next decode phase strictly after some
/// earlier `now`", so it never sits more than one slot ahead, and the DSP
/// thread (fed every audio batch) never passes it by a whole slot. Outside
/// that band the wall clock jumped: a backward NTP step left it up to the
/// size of the step in the future (measured: 613 s of silence after a 600 s
/// step), and a sleep/wake or forward step left it far in the past.
///
/// Returns `(next_phase_with_period(now, decode_phase, slot_ns), Some(jump))`
/// on a jump and `(next_window_time, None)` otherwise, so the no-jump emit
/// path is untouched.
fn reanchor_decode_window(
    now: chrono::DateTime<chrono::Utc>,
    next_window_time: chrono::DateTime<chrono::Utc>,
    decode_phase: chrono::Duration,
    slot_ns: i64,
) -> (
    chrono::DateTime<chrono::Utc>,
    Option<pancetta_core::slot_clock::WallClockJump>,
) {
    match pancetta_core::slot_clock::classify_deadline_jump(now, next_window_time, slot_ns) {
        Some(jump) => (
            pancetta_core::slot::next_phase_with_period(now, decode_phase, slot_ns),
            Some(jump),
        ),
        None => (next_window_time, None),
    }
}

/// Manage WAV recording of 12kHz mono FT8 windows for decoder validation.
///
/// Writes one WAV file per FT8 window (12.64 seconds @ 12kHz mono i16).
/// Each file is ~303 KB.  When total usage exceeds [`WAV_RECORDING_MAX_BYTES`],
/// the oldest files are deleted to make room.
struct WavRecorder {
    dir: std::path::PathBuf,
    /// Running estimate of the top-level `*.wav` bytes in `dir`. Files moved
    /// out externally (the NAS offload job) are not subtracted, so it can
    /// only overestimate; it is re-measured from disk before any eviction.
    total_bytes: u64,
    max_bytes: u64,
    /// Test seam (PAN-211): a one-shot action run at the top of eviction
    /// iteration `.0` (counted from 0), after the at-cap measurement and
    /// before that iteration's re-measure. Stands in for the NAS offload
    /// moving a file out mid-eviction.
    #[cfg(test)]
    eviction_hook: Option<(usize, Box<dyn FnOnce() + Send>)>,
}

impl WavRecorder {
    fn open(dir: std::path::PathBuf, max_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;

        // Sum existing recording sizes
        // A partial scan still counts every readable WAV, the best lower
        // bound available at startup.
        let total_bytes = Self::dir_size(&dir).map(|(bytes, _)| bytes).unwrap_or(0);
        info!(
            "WAV recording enabled: dir={}, existing={:.1} MB, cap={:.1} GB",
            dir.display(),
            total_bytes as f64 / 1_048_576.0,
            max_bytes as f64 / 1_073_741_824.0,
        );
        Ok(Self {
            dir,
            total_bytes,
            max_bytes,
            #[cfg(test)]
            eviction_hook: None,
        })
    }

    /// Bytes in top-level `*.wav` files: the same flat scope
    /// [`Self::delete_oldest`] evicts from, so curated fixture subdirectories
    /// (e.g. `noise_1000/`) and non-WAV files never count toward the cap.
    ///
    /// Returns the readable total and whether every entry was read, so a
    /// partial scan is never mistaken for the real total. A file that vanished
    /// between listing and stat (the offload moved it) is simply gone and
    /// doesn't make the scan incomplete.
    fn dir_size(dir: &std::path::Path) -> std::io::Result<(u64, bool)> {
        let mut total = 0;
        let mut complete = true;
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "wav") {
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => total += m.len(),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => complete = false,
            }
        }
        Ok((total, complete))
    }

    /// Write a 12kHz mono f32 window to a timestamped WAV file.
    ///
    /// `dial_freq_hz` is the rig's current dial frequency; when it maps to a
    /// known ham band the filename gains a `_<band>` suffix
    /// (`ft8_<date>_<time>_<band>.wav`) so the corpus is band-stratifiable.
    /// `0`/unknown → no suffix (legacy `ft8_<date>_<time>.wav`, still parsed
    /// by all downstream tooling, which splits on `_` and ignores extras).
    fn write_window(
        &mut self,
        samples: &[f32],
        timestamp: &chrono::DateTime<chrono::Utc>,
        dial_freq_hz: u64,
    ) {
        // Skip dead-air windows (rig off/unplugged → digital silence).
        if is_dead_air(samples) {
            debug!(
                target: "dsp.record",
                "skip dead-air window (peak {:.5} < {})",
                window_peak(samples),
                DEAD_AIR_PEAK_THRESHOLD
            );
            return;
        }

        // External removals (the NAS offload) aren't tracked, so the running
        // total only overestimates. At the cap, re-measure before deleting
        // anything, and again before every further deletion: the offload can
        // keep moving files out mid-eviction (PAN-211). If a scan is partial
        // or fails, the real size is unknown. Skip this window rather than
        // evict from a stale estimate, and retry on the next one.
        if self.total_bytes >= self.max_bytes && !self.remeasure() {
            return;
        }

        // Enforce cap by deleting oldest files
        while self.total_bytes >= self.max_bytes {
            #[cfg(test)]
            self.run_eviction_hook();
            if !self.remeasure() {
                return;
            }
            if self.total_bytes < self.max_bytes {
                break;
            }
            if !self.delete_oldest() {
                warn!("WAV recorder: cannot free space, skipping write");
                return;
            }
        }

        let filename = match band_for_freq_hz(dial_freq_hz) {
            Some(band) => format!("ft8_{}_{}.wav", timestamp.format("%Y%m%d_%H%M%S"), band),
            None => format!("ft8_{}.wav", timestamp.format("%Y%m%d_%H%M%S")),
        };
        let path = self.dir.join(&filename);

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 12000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        match hound::WavWriter::create(&path, spec) {
            Ok(mut writer) => {
                for &s in samples {
                    let _ = writer.write_sample((s * i16::MAX as f32) as i16);
                }
                if let Err(e) = writer.finalize() {
                    warn!("WAV recorder: finalize error: {}", e);
                    return;
                }
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.total_bytes += file_size;
                debug!(
                    "WAV recorded: {} ({:.0} KB, total {:.1} MB)",
                    filename,
                    file_size as f64 / 1024.0,
                    self.total_bytes as f64 / 1_048_576.0,
                );
            }
            Err(e) => {
                warn!("WAV recorder: create error: {}", e);
            }
        }
    }

    /// Replace the running estimate with a fresh scan of `dir`. Returns
    /// false, leaving the estimate as-is, when the real size is unknown (a
    /// partial scan or an unreadable directory).
    fn remeasure(&mut self) -> bool {
        match Self::dir_size(&self.dir) {
            Ok((measured, true)) => {
                self.total_bytes = measured;
                true
            }
            Ok((_, false)) => {
                warn!(
                    "WAV recorder: partial re-measure of {}, skipping write",
                    self.dir.display()
                );
                false
            }
            Err(e) => {
                warn!(
                    "WAV recorder: cannot re-measure {}: {}, skipping write",
                    self.dir.display(),
                    e
                );
                false
            }
        }
    }

    /// Fire [`Self::eviction_hook`] once its iteration comes up.
    #[cfg(test)]
    fn run_eviction_hook(&mut self) {
        let Some((skip, _)) = self.eviction_hook.as_mut() else {
            return;
        };
        if *skip > 0 {
            *skip -= 1;
            return;
        }
        if let Some((_, action)) = self.eviction_hook.take() {
            action();
        }
    }

    /// Delete the oldest WAV file to free space. Returns false if no files remain.
    fn delete_oldest(&mut self) -> bool {
        let mut entries: Vec<_> = std::fs::read_dir(&self.dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "wav"))
            .collect();

        entries.sort_by_key(|e| e.file_name());

        if let Some(oldest) = entries.first() {
            let size = oldest.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(oldest.path()).is_ok() {
                self.total_bytes = self.total_bytes.saturating_sub(size);
                info!(
                    "WAV recorder: deleted {} ({:.0} KB) to free space",
                    oldest.file_name().to_string_lossy(),
                    size as f64 / 1024.0,
                );
                return true;
            }
        }
        false
    }
}

/// Operator-facing startup line when `[audio.recording]` leaves recording
/// off (PAN-125). Names the setting that turns it back on.
pub(crate) const MSG_WAV_RECORDING_DISABLED: &str =
    "WAV recording disabled -- set audio.recording.auto_record = true to record live decode windows";
/// Operator-facing startup line when `--replay` refuses an enabled
/// recording config (house phrasing: `Replay mode: X suppressed -- reason`).
pub(crate) const MSG_WAV_RECORDING_REPLAY: &str =
    "Replay mode: WAV recording suppressed -- replayed windows are not live captures";
/// Operator-facing startup line when `--no-audio` refuses an enabled
/// recording config.
pub(crate) const MSG_WAV_RECORDING_NO_AUDIO: &str =
    "WAV recording suppressed -- --no-audio has no live audio to record";

/// Whether this run records decode windows, and where. Decided once per
/// component start from `[audio.recording]` plus the run mode; both WAV
/// writers (this file's [`WavRecorder`], `audio.rs`'s raw diagnostic)
/// obey it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WavRecording {
    /// `auto_record = true` on a live-audio run: record into `dir`.
    Enabled { dir: std::path::PathBuf },
    /// `auto_record` unset/false: record nothing.
    DisabledByConfig,
    /// `--replay`: never write replayed windows back out as fresh captures.
    SuppressedReplay,
    /// `--no-audio`: no live audio exists to record.
    SuppressedNoAudio,
    /// The configured `directory` needs `~` expanded but no home directory
    /// is resolvable; recording stays off rather than writing somewhere
    /// arbitrary.
    NoHomeDir { directory: String },
}

/// Config first (so the default case always names the setting), then the
/// unconditional `--replay` / `--no-audio` refusals, then directory
/// resolution. See [`ApplicationCoordinator::wav_recording`] for the
/// process-level entry point.
pub(crate) fn resolve_wav_recording(
    recording: &pancetta_config::RecordingConfig,
    replay: bool,
    no_audio: bool,
    home: Option<&std::path::Path>,
) -> WavRecording {
    if !recording.auto_record {
        return WavRecording::DisabledByConfig;
    }
    if replay {
        return WavRecording::SuppressedReplay;
    }
    if no_audio {
        return WavRecording::SuppressedNoAudio;
    }
    match expand_recording_dir(&recording.directory, home) {
        Some(dir) => WavRecording::Enabled { dir },
        None => WavRecording::NoHomeDir {
            directory: recording.directory.trim().to_string(),
        },
    }
}

/// Expand a leading `~` (`~`, `~/x`, `~\x`) against `home`; anything else
/// passes through trimmed. `~user` is not expanded. The config loader never
/// expands `~` inside values (its tests pin that), so this is the only
/// place. An empty/whitespace directory means the location pancetta always
/// recorded to before PAN-125: `<home>/.pancetta/recordings`.
///
/// Returns `None` only when the value needs `home` and none exists.
fn expand_recording_dir(
    directory: &str,
    home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let trimmed = directory.trim();
    if trimmed.is_empty() {
        return home.map(|h| h.join(".pancetta").join("recordings"));
    }
    if let Some(rest) = trimmed.strip_prefix('~') {
        if let Some(home) = home {
            if rest.is_empty() {
                return Some(home.to_path_buf());
            }
            if let Some(rel) = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\')) {
                return Some(home.join(rel));
            }
        }
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            return None; // needs a home directory to expand against
        }
    }
    // `~user/...` and every non-tilde value pass through as written.
    Some(std::path::PathBuf::from(trimmed))
}

impl WavRecording {
    pub(crate) fn dir(&self) -> Option<&std::path::Path> {
        match self {
            Self::Enabled { dir } => Some(dir),
            _ => None,
        }
    }

    /// The one startup log line for this decision. `None` for `Enabled`:
    /// [`WavRecorder::open`] logs that line with the directory.
    pub(crate) fn startup_message(&self) -> Option<String> {
        match self {
            Self::Enabled { .. } => None,
            Self::DisabledByConfig => Some(MSG_WAV_RECORDING_DISABLED.to_string()),
            Self::SuppressedReplay => Some(MSG_WAV_RECORDING_REPLAY.to_string()),
            Self::SuppressedNoAudio => Some(MSG_WAV_RECORDING_NO_AUDIO.to_string()),
            Self::NoHomeDir { directory } => Some(format!(
                "WAV recording disabled -- cannot expand audio.recording.directory = \
                 \"{directory}\": no home directory"
            )),
        }
    }

    /// Emit the startup line (`NoHomeDir` at `warn!`, the rest `info!`).
    fn log_startup(&self) {
        if let Some(message) = self.startup_message() {
            if matches!(self, Self::NoHomeDir { .. }) {
                warn!("{message}");
            } else {
                info!("{message}");
            }
        }
    }
}

impl super::ApplicationCoordinator {
    /// The recording decision for this process: config, then the
    /// `--replay` / `--no-audio` overrides, then directory resolution.
    /// Uses [`Self::replay_mode`] — the single replay predicate — not a
    /// second replay check.
    pub(crate) fn wav_recording(
        &self,
        recording: &pancetta_config::RecordingConfig,
    ) -> WavRecording {
        resolve_wav_recording(
            recording,
            self.replay_mode(),
            self.no_audio,
            dirs::home_dir().as_deref(),
        )
    }
}

impl super::ApplicationCoordinator {
    /// Start DSP pipeline with point-to-point channels
    ///
    /// Simple direct pipeline: resample 48kHz->12kHz on a dedicated thread,
    /// accumulate FT8-sized windows, and send to the decoder.
    pub(crate) async fn start_dsp_pipeline(&mut self) -> Result<()> {
        let (
            audio_rx,
            dsp_to_ft8_tx,
            live_waterfall_tx,
            audio_level_tx,
            health_dsp_windows,
            health_last_rms,
        ) = {
            let h = self.decode_handles()?;
            (
                h.audio_to_dsp_rx.clone(),
                h.dsp_to_ft8_tx.clone(),
                h.waterfall_tx.clone(),
                h.audio_level_tx.clone(),
                h.health_dsp_windows.clone(),
                h.health_last_rms.clone(),
            )
        };
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();

        info!("Starting DSP component");

        let shutdown = self.shutdown_signal.clone();
        let message_count = self.message_count.clone();
        // Current rig dial frequency (Hz), updated by hamlib FrequencyResponse.
        // Read per-window so recordings can be band-stamped. 0 = unknown
        // (no rig / not yet polled) → recording filename omits the band.
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let receive_gain_epoch = self.receive_gain_epoch.clone();
        // Live mode atomic — polled once per audio batch alongside the dial
        // frequency so a runtime FT8/FT4/FT2 switch (Shift+M) resizes the
        // decode window without a restart.
        let active_protocol_mode = self.active_protocol_mode();

        let config = self.config.read().await;
        let input_rate = config.audio.sample_rate;
        let _input_channels = config.audio.input_channels as u16;
        // Derive the active protocol's DSP timing once at startup. mode=FT8
        // yields exactly the historical 12.64s window / 13s decode phase /
        // 15×rate overlap (asserted by `derive_dsp_timing_ft8_byte_identical`).
        let active_operating_mode = config
            .rig
            .operating_mode()
            .unwrap_or(pancetta_config::OperatingMode::Ft8);
        let protocol = super::protocol_from_mode(active_operating_mode);
        // Task W5.3: resolve the window lead-in once at spawn time. `ft8.rs`
        // reads the same config field independently at its own spawn and
        // must resolve to the same value — see `resolve_window_lead_secs`.
        let window_lead_secs = super::resolve_window_lead_secs(&config.decoder);
        // PAN-125: decide WAV recording here, under the config guard — the
        // worker closure below only ever sees the resolved directory.
        let wav_recording = self.wav_recording(&config.audio.recording);
        drop(config);
        wav_recording.log_startup();
        let wav_recording_dir = wav_recording.dir().map(std::path::Path::to_path_buf);
        let timing =
            super::derive_dsp_timing(&pancetta_ft8::ProtocolParams::from_protocol(protocol));
        let dsp_window_samples = timing.window_samples;
        let dsp_decode_phase = timing.decode_phase;
        let dsp_overlap_samples = timing.overlap_samples;
        let dsp_slot_ns = timing.slot_ns;
        // Seed value for the closure's mutable `cached_mode_u8`, moved in
        // alongside the timing locals so the initial cache matches the same
        // config read that produced `protocol`/`timing` above.
        let seed_mode_u8 = active_operating_mode.as_u8();

        let handle = tokio::task::spawn_blocking(move || {
            // FT8 timing: transmissions start at 0/15/30/45 second marks.
            // We need 12.64 seconds of audio at 12kHz = 151,680 samples per window.
            // We align window capture to UTC 15-second boundaries for best decode.
            if !dsp_supports_input_rate(input_rate) {
                return Err(anyhow::anyhow!(unsupported_input_rate_message(input_rate)));
            }
            let decimation_factor = (input_rate / DSP_TARGET_SAMPLE_RATE_HZ) as usize;
            const FT8_SAMPLE_RATE: usize = 12000;
            // Protocol-derived timing, captured from `DspTiming` above.
            // Mutable so a live mode switch (Shift+M) can resize the decode
            // window / decode phase / overlap / slot period without
            // restarting this thread — see the mode-change guard below.
            // (`dsp_window_samples` itself is folded into `ft8_window_samples`
            // below — that's the only one actually read past this point.)
            let mut dsp_decode_phase = dsp_decode_phase;
            let mut dsp_overlap_samples = dsp_overlap_samples;
            let mut dsp_slot_ns = dsp_slot_ns;
            // Protocol-derived window length (FT8 → 151_680 = 12.64s×12kHz;
            // FT4 → 60_480 = 5.04s×12kHz).
            let mut ft8_window_samples = dsp_window_samples;
            // Cached mode (`OperatingMode::as_u8()`), seeded from the same
            // config read that produced `protocol`/`timing` above. Compared
            // each iteration against the live `active_protocol_mode` atomic
            // to detect a runtime switch.
            let mut cached_mode_u8: u8 = seed_mode_u8;

            let mut decimate_counter: usize = 0;

            let mut ft8_buffer: Vec<f32> = Vec::with_capacity(ft8_window_samples * 2);
            let mut window_count: u64 = 0;
            let mut batch_count: u64 = 0;
            let _waiting_for_boundary = true;

            // Dial-change guard (PAN-70): the last nonzero dial reading, i.e.
            // the dial the audio accumulating in `ft8_buffer` is attributed
            // to. When the operator (or the autonomous engine) retunes, the
            // shared `operating_frequency_hz` atomic flips, but `ft8_buffer`
            // still holds up to a full window of audio from the OLD dial. A
            // window carries one dial snapshot, so emitting that mixed audio
            // would report old-dial stations at the new dial (phantom decodes
            // on a band change, frequencies off by the retune delta within a
            // band). `apply_dial_change` flushes on every observed change
            // between two known dials. 0 = not yet established (no rig /
            // pre-first-read); the first nonzero read seeds it.
            let mut last_known_dial_hz: u64 = 0;
            let mut last_gain_epoch = receive_gain_epoch.load(Ordering::Acquire);

            // WAV recorder for decoder validation (PAN-125): present only
            // when `[audio.recording] auto_record = true` resolved to a
            // directory above. An unusable directory stays non-fatal.
            let mut wav_recorder = wav_recording_dir.and_then(|dir| {
                match WavRecorder::open(dir.clone(), WAV_RECORDING_MAX_BYTES) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        warn!(
                            "WAV recording disabled: cannot use {}: {}",
                            dir.display(),
                            e
                        );
                        None
                    }
                }
            });

            // Live waterfall state
            let mut last_live_wf_samples: usize = 0;
            const BIN_HISTORY_LEN: usize = 60;
            const NOISE_FLOOR_DB_SCALE: f32 = 12.0;
            const MIN_HISTORY_FOR_FLOOR: usize = 5;
            let mut bin_history: Vec<std::collections::VecDeque<f32>> = Vec::new();
            let mut live_wf_planner = rustfft::FftPlanner::<f32>::new();
            let live_wf_fft = live_wf_planner.plan_fft_forward(2048);

            info!(
                "DSP: {}Hz -> {}Hz mono (decimate {}:1, subsample), window={}",
                input_rate, FT8_SAMPLE_RATE, decimation_factor, ft8_window_samples
            );

            // Continuously capture audio -- don't wait for boundaries.
            // FT8 has both even (0/30s) and odd (15/45s) time slots.
            // We send overlapping windows: one at each slot mark.
            // The decoder handles time alignment internally via Costas sync.
            // Schedule decode `decode_phase` past the slot start (FT8 → 13s,
            // 0.36s after the 12.64s message end; FT4 → 6.5s). Slots start at
            // the protocol's `slot_ns` cadence aligned to UTC.
            let mut next_window_time = pancetta_core::slot::next_phase_with_period(
                chrono::Utc::now(),
                dsp_decode_phase,
                dsp_slot_ns,
            );
            info!(
                "DSP: first window at {}",
                next_window_time.format("%H:%M:%S")
            );

            while !shutdown.load(Ordering::Acquire) {
                match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(samples) => {
                        // Do not build the first new-band window from audio captured
                        // before both gain levels finished. Drain queued batches too.
                        if super::hamlib::gain_capture_changed(
                            receive_gain_epoch.load(Ordering::Acquire),
                            &mut last_gain_epoch,
                        ) {
                            ft8_buffer.clear();
                            last_live_wf_samples = 0;
                            for _ in audio_rx.try_iter() {}
                            continue;
                        }
                        message_count.fetch_add(1, Ordering::Relaxed);
                        batch_count += 1;

                        // Extract left channel from interleaved stereo.
                        // cpal delivers interleaved [L, R, L, R, ...] where
                        // the right channel is near-silent on the FTdx10 USB codec.
                        let mono: Vec<f32> = if _input_channels > 1 {
                            samples
                                .chunks(_input_channels as usize)
                                .map(|ch| ch[0])
                                .collect()
                        } else {
                            samples
                        };

                        // One-time diagnostic: log first batch stats
                        if batch_count == 1 {
                            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / mono.len() as f32)
                                .sqrt();
                            info!(
                                "DSP first batch: {} samples, RMS={:.6}, first 5 values: {:?}",
                                mono.len(),
                                rms,
                                &mono[..5.min(mono.len())]
                            );
                        }

                        // Decimate by taking every Nth sample (simple subsampling).
                        // FT8 signals occupy 0–3 kHz, well below the 6 kHz Nyquist
                        // of the 12 kHz target rate, so anti-alias filtering is
                        // unnecessary and the previous 65-tap FIR was attenuating
                        // signals (ft8_lib decoded 86 from naive decimation vs 1
                        // from the FIR output on the same live audio).
                        for &sample in &mono {
                            decimate_counter += 1;
                            if decimate_counter >= decimation_factor {
                                decimate_counter = 0;
                                ft8_buffer.push(sample);
                            }
                        }

                        // Dial-change guard (PAN-70): if the dial moved since
                        // the audio now buffered was captured — across a band
                        // edge or by 1 Hz within a band — discard the in-flight
                        // audio, this batch included, so no window mixes two
                        // dials under one label. This one read is also the
                        // snapshot the emitted `DecodeWindow` carries.
                        let cur_dial_hz = operating_frequency_hz.load(Ordering::Relaxed);
                        if let Some(flush) = apply_dial_change(
                            cur_dial_hz,
                            &mut last_known_dial_hz,
                            &mut ft8_buffer,
                            &mut last_live_wf_samples,
                            &mut bin_history,
                        ) {
                            if flush.band_changed {
                                info!(
                                    target: "operator.override",
                                    "DSP: band change {} Hz -> {} Hz — flushing {} in-flight \
                                     samples to avoid phantom decodes on the new band",
                                    flush.previous_hz,
                                    cur_dial_hz,
                                    flush.discarded_samples
                                );
                            } else {
                                debug!(
                                    "DSP: dial change {} Hz -> {} Hz within the band — \
                                     discarded {} in-flight samples so no window mixes \
                                     two dials",
                                    flush.previous_hz, cur_dial_hz, flush.discarded_samples
                                );
                            }
                        }

                        // Mode-change guard: if the operator cycled the
                        // station-wide mode (Shift+M) since the audio now
                        // buffered was captured, the buffered samples are the
                        // wrong frame geometry for the new mode's decoder —
                        // flush and resize, mirroring the dial-change guard
                        // immediately above.
                        let cur_mode_u8 = active_protocol_mode.load(Ordering::Relaxed);
                        if mode_flush_decision(cached_mode_u8, cur_mode_u8) {
                            let new_protocol = super::protocol_from_mode(
                                pancetta_config::OperatingMode::from_u8(cur_mode_u8),
                            );
                            let new_timing = super::derive_dsp_timing(
                                &pancetta_ft8::ProtocolParams::from_protocol(new_protocol),
                            );
                            info!(
                                target: "operator.override",
                                "DSP: mode change {} -> {} — flushing {} in-flight \
                                 samples and resizing the decode window",
                                super::mode_str(pancetta_config::OperatingMode::from_u8(
                                    cached_mode_u8
                                )),
                                super::mode_str(pancetta_config::OperatingMode::from_u8(
                                    cur_mode_u8
                                )),
                                ft8_buffer.len()
                            );
                            ft8_window_samples = new_timing.window_samples;
                            dsp_decode_phase = new_timing.decode_phase;
                            dsp_overlap_samples = new_timing.overlap_samples;
                            dsp_slot_ns = new_timing.slot_ns;
                            ft8_buffer.clear();
                            last_live_wf_samples = 0;
                            bin_history.clear();
                            next_window_time = pancetta_core::slot::next_phase_with_period(
                                chrono::Utc::now(),
                                dsp_decode_phase,
                                dsp_slot_ns,
                            );
                            cached_mode_u8 = cur_mode_u8;
                        }

                        // Live waterfall: emit one spectrum row per second using rustfft.
                        // We keep a simple sample counter to trigger every ~1 second.
                        const LIVE_WF_INTERVAL: usize = 12000; // 1 second at 12kHz
                        const LIVE_WF_FFT_SIZE: usize = 2048;
                        if ft8_buffer.len() >= LIVE_WF_FFT_SIZE {
                            let samples_since_last =
                                ft8_buffer.len().saturating_sub(last_live_wf_samples);
                            if samples_since_last >= LIVE_WF_INTERVAL {
                                last_live_wf_samples = ft8_buffer.len();

                                let wf_start = ft8_buffer.len() - LIVE_WF_FFT_SIZE;
                                let wf_slice = &ft8_buffer[wf_start..];

                                // Use rustfft for a quick spectrum
                                let mut input: Vec<rustfft::num_complex::Complex<f32>> = wf_slice
                                    .iter()
                                    .enumerate()
                                    .map(|(i, &s)| {
                                        // Apply Hann window
                                        let w = 0.5
                                            * (1.0
                                                - (2.0 * std::f32::consts::PI * i as f32
                                                    / LIVE_WF_FFT_SIZE as f32)
                                                    .cos());
                                        rustfft::num_complex::Complex::new(s * w, 0.0)
                                    })
                                    .collect();
                                live_wf_fft.process(&mut input);

                                // Extract 0-3000 Hz bins and convert to dB
                                let freq_res = FT8_SAMPLE_RATE as f32 / LIVE_WF_FFT_SIZE as f32;
                                let bin_end = (3000.0 / freq_res) as usize;
                                let bin_end = bin_end.min(LIVE_WF_FFT_SIZE / 2);

                                let powers: Vec<f32> = (0..=bin_end)
                                    .map(|i| {
                                        10.0 * (input[i].norm_sqr() / LIVE_WF_FFT_SIZE as f32
                                            + 1e-12)
                                            .log10()
                                    })
                                    .collect();

                                // Lazy-init history with the right number of bins on the first row
                                // so we don't have to know `bin_end + 1` before the FFT runs.
                                if bin_history.len() != powers.len() {
                                    bin_history = (0..powers.len())
                                        .map(|_| {
                                            std::collections::VecDeque::with_capacity(
                                                BIN_HISTORY_LEN,
                                            )
                                        })
                                        .collect();
                                }

                                // Push current dB powers into per-bin history (drop oldest if full).
                                for (i, &p) in powers.iter().enumerate() {
                                    if bin_history[i].len() >= BIN_HISTORY_LEN {
                                        bin_history[i].pop_front();
                                    }
                                    bin_history[i].push_back(p);
                                }

                                // Output row: signal-above-floor in 0..1 (0..NOISE_FLOOR_DB_SCALE dB above
                                // the rolling per-bin median). Until each bin has MIN_HISTORY_FOR_FLOOR
                                // samples, emit zero so the waterfall starts dim instead of with garbage.
                                let row: Vec<f32> = powers
                                    .iter()
                                    .enumerate()
                                    .map(|(i, &p)| {
                                        if bin_history[i].len() < MIN_HISTORY_FOR_FLOOR {
                                            return 0.0;
                                        }
                                        let history: Vec<f32> =
                                            bin_history[i].iter().copied().collect();
                                        let median = rolling_median(&history);
                                        ((p - median).max(0.0) / NOISE_FLOOR_DB_SCALE)
                                            .clamp(0.0, 1.0)
                                    })
                                    .collect();
                                let _ = live_waterfall_tx.try_send(vec![row]);

                                // Send audio level (RMS) to TUI — once per second
                                let rms = (wf_slice.iter().map(|s| s * s).sum::<f32>()
                                    / wf_slice.len() as f32)
                                    .sqrt();
                                let _ = audio_level_tx.try_send(rms);
                            }
                        }

                        // Decode as early as possible after the FT8 message completes.
                        // FT8 messages are 12.64s long, starting at :00/:15/:30/:45.
                        // We trigger at 13s past the slot start (0.36s after message
                        // end) to maximize response time for QSO management.
                        //
                        // The emitted window is *anchored to the UTC slot boundary*,
                        // not to `now`: sample 0 corresponds to
                        // `slot_boundary − WINDOW_LEAD_SECS`. This makes the decoder's
                        // reported `time_offset` boundary-relative (after the matching
                        // `−WINDOW_LEAD_SECS` correction in ft8.rs), so a station that
                        // transmits on the slot boundary reads DT ≈ 0 instead of the
                        // ≈ +2 s the old "last 15 s of buffer" slice produced. Anchoring
                        // to the *scheduled* boundary (derived from `next_window_time`)
                        // rather than `now` also removes the 0–1 s emit-trigger jitter
                        // from the DT value. See `WINDOW_LEAD_SECS` in `coordinator/mod.rs`.
                        // Overlap retained between windows = one full cycle
                        // (FT8 → 180_000 = 15s×12kHz; FT4 → 90_000 = 7.5s×12kHz).
                        let ideal_samples = dsp_overlap_samples;
                        let now = chrono::Utc::now();
                        let (reanchored, jump) = reanchor_decode_window(
                            now,
                            next_window_time,
                            dsp_decode_phase,
                            dsp_slot_ns,
                        );
                        if let Some(jump) = jump {
                            next_window_time = reanchored;
                            if matches!(jump, pancetta_core::slot_clock::WallClockJump::Forward(_))
                            {
                                // Same flush as an intra-band dial change
                                // (above): the buffer spans the jump.
                                // `bin_history` (the waterfall noise floor) is
                                // kept — the band did not change.
                                ft8_buffer.clear();
                                last_live_wf_samples = 0;
                            }
                            warn!(
                                target: "pancetta::slot_clock",
                                "DSP decode window re-anchored to UTC after a wall-clock jump: {jump:?}"
                            );
                        }
                        if ft8_buffer.len() >= ft8_window_samples && now >= next_window_time {
                            // Scheduled slot boundary for this window: next_window_time
                            // is the decode-phase instant, `decode_phase` past the
                            // boundary (FT8 → :13/:28/:43/:58; FT4 → :6.5 cadence).
                            let slot_boundary = next_window_time - dsp_decode_phase;
                            let (start, send_len) = boundary_anchored_slice(
                                ft8_buffer.len(),
                                now,
                                slot_boundary,
                                FT8_SAMPLE_RATE,
                                ft8_window_samples,
                                window_lead_secs,
                            );
                            let window: Vec<f32> = ft8_buffer[start..start + send_len].to_vec();
                            // Keep overlap for next window — retain enough for the
                            // full cycle's ideal window at the next boundary
                            let keep = ideal_samples;
                            if ft8_buffer.len() > keep {
                                ft8_buffer.drain(..ft8_buffer.len() - keep);
                                last_live_wf_samples = ft8_buffer.len();
                            }
                            window_count += 1;
                            let rms = (window.iter().map(|s| s * s).sum::<f32>()
                                / window.len() as f32)
                                .sqrt();
                            info!(
                                "DSP: FT8 window #{} (RMS={:.4}) at {}",
                                window_count,
                                rms,
                                now.format("%H:%M:%S.%3f")
                            );

                            if super::hamlib::gain_capture_changed(
                                receive_gain_epoch.load(Ordering::Acquire),
                                &mut last_gain_epoch,
                            ) {
                                ft8_buffer.clear();
                                last_live_wf_samples = 0;
                                for _ in audio_rx.try_iter() {}
                                continue;
                            }

                            // Record window to WAV for decoder validation,
                            // band-stamped with the current rig dial frequency.
                            if let Some(ref mut recorder) = wav_recorder {
                                let dial_hz = operating_frequency_hz.load(Ordering::Relaxed);
                                recorder.write_window(&window, &now, dial_hz);
                            }

                            match super::pipeline::forward_or_drop(
                                &dsp_to_ft8_tx,
                                super::pipeline::DecodeWindow {
                                    samples: window,
                                    // `cur_dial_hz`: this batch's dial read, the
                                    // same one the dial-change guard above just
                                    // checked (PAN-67 round 1). Because that guard
                                    // flushes on every observed change between two
                                    // known dials (PAN-70), no sample in this window
                                    // was appended while the DSP read a different
                                    // known dial. 0 still means unknown.
                                    dial_hz: cur_dial_hz,
                                },
                                super::pipeline::DECODE_FORWARD_TIMEOUT,
                            ) {
                                super::pipeline::ForwardOutcome::Sent => {}
                                super::pipeline::ForwardOutcome::Dropped => {
                                    // A restarted/slow decoder must not wedge the DSP stage:
                                    // bounded backpressure intentionally sacrifices this slot,
                                    // then the next slot is still eligible for decoding.
                                    warn!(
                                        "DSP: FT8 stage not draining -- dropped one decode window"
                                    );
                                }
                                super::pipeline::ForwardOutcome::Disconnected => return Ok(()),
                            }
                            health_dsp_windows.fetch_add(1, Ordering::Relaxed);
                            health_last_rms.store(rms.to_bits(), Ordering::Relaxed);
                            // Schedule next decode at `decode_phase` into the
                            // next slot (FT8 → 13s; FT4 → 6.5s).
                            next_window_time = pancetta_core::slot::next_phase_with_period(
                                chrono::Utc::now(),
                                dsp_decode_phase,
                                dsp_slot_ns,
                            );
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!(
                            "DSP: audio channel disconnected after {} batches",
                            batch_count
                        );
                        break;
                    }
                }
            }

            info!(
                "DSP stopped ({} batches, {} windows sent)",
                batch_count, window_count
            );
            Ok(())
        });

        self.named_task_handles.push((ComponentId::Dsp, handle));
        info!("DSP component started");
        Ok(())
    }
}

/// Compute the slice `(start, len)` of the FT8 capture buffer for one decode
/// window, anchored so that the window's sample 0 corresponds to
/// `slot_boundary − lead_secs`.
///
/// The buffer is a rolling capture of decimated 12 kHz audio; its newest
/// sample (`buffer_len - 1`) corresponds to the wall-clock `emit_now` (the
/// time of the most recently received audio batch). We map that real-time
/// anchor back into the buffer:
///
/// * The desired window start time is `slot_boundary − lead_secs`.
/// * Samples from the buffer end back to that start time is
///   `(emit_now − (slot_boundary − lead_secs)) · SAMPLE_RATE`.
/// * The window extends from there to the buffer end, so it always contains
///   the full FT8 message span (boundary .. boundary+12.64 s) plus the lead.
///
/// Returns a `(start, len)` slice that is always in-bounds. If the buffer is
/// shorter than the ideal anchored window (early startup, or `emit_now`
/// preceding the boundary), the slice is clamped to `[0, buffer_len)` and the
/// window simply begins later than the lead — the decoder still recovers the
/// message via Costas sync, and DT is reported relative to the actual sample 0
/// (handled identically downstream).
///
/// `min_len` (the FT8 message span in samples) is used only as a floor when
/// clamping so we never hand the decoder a window too short to contain a
/// message.
///
/// `lead_secs` is normally [`super::WINDOW_LEAD_SECS`]; Task W5.3 threads
/// [`super::resolve_window_lead_secs`]'s result through instead so the
/// `[decoder].extended_capture_window_enabled` flag can widen it — the
/// caller (and `ft8.rs`'s matching DT correction) must use the SAME
/// resolved value or the reported DT would drift from the true slot-relative
/// offset.
fn boundary_anchored_slice(
    buffer_len: usize,
    emit_now: chrono::DateTime<chrono::Utc>,
    slot_boundary: chrono::DateTime<chrono::Utc>,
    sample_rate: usize,
    min_len: usize,
    lead_secs: f64,
) -> (usize, usize) {
    // Seconds from the desired window start (slot_boundary − lead) to emit_now.
    let secs_from_start = (emit_now - slot_boundary)
        .num_nanoseconds()
        .map(|ns| ns as f64 / 1_000_000_000.0)
        .unwrap_or(0.0)
        + lead_secs;

    // Number of samples back from the buffer end to the window start. Negative
    // (emit_now before the anchor) collapses to "from the buffer end".
    let samples_back = (secs_from_start * sample_rate as f64).round();
    let samples_back = if samples_back.is_finite() && samples_back > 0.0 {
        samples_back as usize
    } else {
        0
    };

    // Clamp the slice length to what the buffer holds, but never below min_len
    // (when the buffer itself is shorter than min_len the caller's
    // `>= FT8_WINDOW_SAMPLES` gate has already been satisfied, so this floor is
    // a no-op in practice; it is belt-and-suspenders for the unit tests).
    let len = samples_back.min(buffer_len).max(min_len.min(buffer_len));
    let start = buffer_len - len;
    (start, len)
}

/// Rolling median over a recent window of dB powers. Used as a per-bin
/// noise-floor estimate so the waterfall renders signal-above-floor
/// instead of per-row min/max stretch (which hid signals at all amplitudes).
fn rolling_median(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f32> = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[sorted.len() / 2]
}

#[cfg(test)]
mod dial_flush_tests {
    use super::dial_flush_decision;
    use crate::coordinator::is_band_change;

    // Representative dial frequencies (Hz).
    const M20: u64 = 14_074_000; // 20 m FT8
    const M40: u64 = 7_074_000; // 40 m FT8
    /// Mid-20 m reference, so ±100 kHz stays inside the band (14.000–14.350 MHz).
    const M20_MID: u64 = 14_175_000;
    /// Intra-band retune magnitudes (Hz) PAN-70 must flush on, in either
    /// direction — including the 1/50/99 Hz moves a 100 Hz deadband would keep.
    const INTRA_BAND_DELTAS: [u64; 7] = [1, 50, 99, 100, 2_000, 16_000, 100_000];

    #[test]
    fn unknown_reads_before_any_dial_keep_the_unknown_reference() {
        assert_eq!(dial_flush_decision(0, 0), (0, false));
    }

    #[test]
    fn seeds_reference_on_first_real_read_without_flushing() {
        let (new_ref, flush) = dial_flush_decision(0, M20);
        assert_eq!(new_ref, M20, "reference seeds to the first dial read");
        assert!(!flush, "seeding must not flush — there is no prior dial");
    }

    #[test]
    fn cross_band_move_flushes_and_advances_reference() {
        for (from, to) in [(M20, M40), (M40, M20)] {
            let (new_ref, flush) = dial_flush_decision(from, to);
            assert!(flush, "{from} -> {to} must flush the in-flight window");
            assert_eq!(new_ref, to, "reference advances to the new dial");
        }
    }

    /// PAN-70: replaces `intra_band_fine_tune_holds_buffer`. Every intra-band
    /// move, however small, flushes — otherwise the buffered old-dial audio is
    /// emitted under the new dial and reported off by the retune delta.
    #[test]
    fn every_intra_band_delta_flushes_and_advances_reference() {
        for delta in INTRA_BAND_DELTAS {
            for to in [M20_MID + delta, M20_MID - delta] {
                assert!(
                    !is_band_change(M20_MID, to),
                    "fixture premise: {M20_MID} -> {to} stays inside 20 m"
                );
                let (new_ref, flush) = dial_flush_decision(M20_MID, to);
                assert!(flush, "intra-band move {M20_MID} -> {to} must flush");
                assert_eq!(new_ref, to, "reference advances to {to}");
            }
        }
    }

    /// Exact comparison applies outside the band table too, including a move
    /// small enough that `is_band_change`'s out-of-band threshold ignores it.
    #[test]
    fn unequal_out_of_band_dials_flush() {
        const OOB: u64 = 12_000_000; // between 30 m and 20 m
        for to in [OOB + 1, OOB - 1, OOB + 500_000, M20] {
            let (new_ref, flush) = dial_flush_decision(OOB, to);
            assert!(flush, "{OOB} -> {to} must flush");
            assert_eq!(new_ref, to);
        }
        let (new_ref, flush) = dial_flush_decision(M20, OOB);
        assert!(flush, "leaving the band table must flush");
        assert_eq!(new_ref, OOB);
    }

    #[test]
    fn zero_current_read_is_ignored_once_seeded() {
        // A transient 0 read (rig dropped out) must not flush or reset.
        let (new_ref, flush) = dial_flush_decision(M20, 0);
        assert!(!flush, "a 0 dial read must not flush");
        assert_eq!(new_ref, M20, "reference survives a transient 0 read");
    }

    #[test]
    fn same_frequency_holds_buffer() {
        let (new_ref, flush) = dial_flush_decision(M20, M20);
        assert!(!flush);
        assert_eq!(new_ref, M20);
    }
}

/// PAN-70: the capture-state effects of an observed dial change, exercised
/// through [`super::apply_dial_change`] — the same helper the DSP loop calls
/// after appending each batch.
#[cfg(test)]
mod dial_change_state_tests {
    use super::{apply_dial_change, DialFlush};
    use std::collections::VecDeque;

    const M20: u64 = 14_074_000;
    const M40: u64 = 7_074_000;
    /// Samples in one simulated audio batch.
    const BATCH: usize = 1_200;

    /// The worker-local state `apply_dial_change` mutates.
    struct CaptureState {
        last_known_dial_hz: u64,
        ft8_buffer: Vec<f32>,
        last_live_wf_samples: usize,
        bin_history: Vec<VecDeque<f32>>,
    }

    impl CaptureState {
        /// A buffer of distinguishable old samples, a nonzero waterfall cursor
        /// and a warm per-bin noise-floor history, attributed to `dial_hz`.
        fn seeded(dial_hz: u64) -> Self {
            Self {
                last_known_dial_hz: dial_hz,
                ft8_buffer: (0..10_000).map(|i| i as f32 * 1e-4).collect(),
                last_live_wf_samples: 7_500,
                bin_history: (0..4)
                    .map(|bin| (0..6).map(|row| (bin * 10 + row) as f32).collect())
                    .collect(),
            }
        }

        /// One loop iteration: append a batch (marked with `marker`), then
        /// apply this batch's dial read — production order.
        fn batch(&mut self, marker: f32, cur_dial_hz: u64) -> Option<DialFlush> {
            self.ft8_buffer.extend(std::iter::repeat_n(marker, BATCH));
            apply_dial_change(
                cur_dial_hz,
                &mut self.last_known_dial_hz,
                &mut self.ft8_buffer,
                &mut self.last_live_wf_samples,
                &mut self.bin_history,
            )
        }
    }

    #[test]
    fn intra_band_change_discards_audio_and_keeps_noise_floor() {
        let mut st = CaptureState::seeded(M20);
        let history_before = st.bin_history.clone();
        let buffered = st.ft8_buffer.len();

        let flush = st.batch(9.0, M20 + 1);

        assert_eq!(
            flush,
            Some(DialFlush {
                previous_hz: M20,
                discarded_samples: buffered + BATCH,
                band_changed: false,
            })
        );
        assert!(
            st.ft8_buffer.is_empty(),
            "no old-dial sample, nor the batch that exposed the change, may survive"
        );
        assert_eq!(st.last_live_wf_samples, 0, "waterfall cursor restarts");
        assert_eq!(st.last_known_dial_hz, M20 + 1, "reference advances");
        assert_eq!(
            st.bin_history, history_before,
            "an intra-band nudge keeps the noise-floor history"
        );
    }

    #[test]
    fn cross_band_change_also_clears_noise_floor() {
        let mut st = CaptureState::seeded(M20);
        let flush = st.batch(9.0, M40).expect("a band change flushes");
        assert!(flush.band_changed);
        assert_eq!(flush.previous_hz, M20);
        assert!(st.ft8_buffer.is_empty());
        assert_eq!(st.last_live_wf_samples, 0);
        assert_eq!(st.last_known_dial_hz, M40);
        assert!(st.bin_history.is_empty(), "a new band starts a fresh floor");
    }

    #[test]
    fn equal_and_unknown_reads_leave_state_untouched() {
        for cur in [M20, 0] {
            let mut st = CaptureState::seeded(M20);
            let history_before = st.bin_history.clone();
            let buffered = st.ft8_buffer.len();
            assert_eq!(st.batch(9.0, cur), None, "dial read {cur} must not flush");
            assert_eq!(st.ft8_buffer.len(), buffered + BATCH, "audio retained");
            assert_eq!(st.last_live_wf_samples, 7_500, "cursor retained");
            assert_eq!(st.last_known_dial_hz, M20, "reference retained");
            assert_eq!(st.bin_history, history_before, "history retained");
        }
    }

    #[test]
    fn first_known_read_seeds_without_flushing() {
        let mut st = CaptureState::seeded(0);
        let buffered = st.ft8_buffer.len();
        assert_eq!(st.batch(9.0, M20), None);
        assert_eq!(st.last_known_dial_hz, M20);
        assert_eq!(st.ft8_buffer.len(), buffered + BATCH);
    }

    /// A→B then B again: the flush happens once, and audio accumulated on B
    /// afterwards is kept (the reference really advanced).
    #[test]
    fn repeated_new_dial_keeps_new_audio() {
        let mut st = CaptureState::seeded(M20);
        assert!(st.batch(1.0, M20 + 2_000).is_some());
        assert!(st.batch(2.0, M20 + 2_000).is_none());
        assert!(st.batch(3.0, M20 + 2_000).is_none());
        assert_eq!(st.ft8_buffer.len(), 2 * BATCH);
        assert!(st.ft8_buffer[..BATCH].iter().all(|&s| s == 2.0));
        assert!(st.ft8_buffer[BATCH..].iter().all(|&s| s == 3.0));
    }

    /// A→B→A: returning to the old dial is itself a change; the B audio must
    /// not be emitted under A.
    #[test]
    fn return_to_previous_dial_flushes_again() {
        let mut st = CaptureState::seeded(M20);
        assert!(st.batch(1.0, M20 + 50).is_some());
        assert!(st.batch(2.0, M20 + 50).is_none());
        let back = st.batch(3.0, M20).expect("B -> A flushes");
        assert_eq!(back.previous_hz, M20 + 50);
        assert_eq!(back.discarded_samples, 2 * BATCH);
        assert!(st.ft8_buffer.is_empty());
        assert_eq!(st.last_known_dial_hz, M20);
    }

    /// A VFO knob turned 1 Hz per batch flushes on every step.
    #[test]
    fn repeated_one_hertz_steps_flush_each_time() {
        let mut st = CaptureState::seeded(M20);
        for step in 1..=5u64 {
            let flush = st
                .batch(step as f32, M20 + step)
                .expect("each step flushes");
            assert_eq!(flush.previous_hz, M20 + step - 1);
            assert!(st.ft8_buffer.is_empty());
            assert_eq!(st.last_known_dial_hz, M20 + step);
        }
    }

    /// A→0→B: the unknown read holds everything; B is compared against A.
    #[test]
    fn unknown_read_between_different_dials_flushes_on_arrival() {
        let mut st = CaptureState::seeded(M20);
        let buffered = st.ft8_buffer.len();
        assert!(st.batch(1.0, 0).is_none());
        assert_eq!(st.ft8_buffer.len(), buffered + BATCH);
        let flush = st.batch(2.0, M20 + 99).expect("A -> 0 -> B flushes at B");
        assert_eq!(flush.previous_hz, M20);
        assert_eq!(flush.discarded_samples, buffered + 2 * BATCH);
        assert!(st.ft8_buffer.is_empty());
        assert_eq!(st.last_known_dial_hz, M20 + 99);
    }

    /// A→0→A: an unknown read in between is not a change.
    #[test]
    fn unknown_read_between_same_dial_keeps_audio() {
        let mut st = CaptureState::seeded(M20);
        let buffered = st.ft8_buffer.len();
        assert!(st.batch(1.0, 0).is_none());
        assert!(st.batch(2.0, M20).is_none());
        assert_eq!(st.ft8_buffer.len(), buffered + 2 * BATCH);
        assert_eq!(st.last_known_dial_hz, M20);
    }
}

/// PAN-70 end-to-end regression with real FT8 audio: a complete message
/// captured on one dial must never be decoded from a window labelled with a
/// later, different dial. Built from the PAN-70 research reproduction and
/// `anchored_slice_tests::synthetic_boundary_signal_reports_dt_near_zero`.
///
/// Each test drives audio through the production per-batch order — append a
/// batch, read the live dial atomic once, [`super::apply_dial_change`], then
/// the emit gate with the real [`super::boundary_anchored_slice`], the real
/// FT8 timing constants and slot scheduler, and the real decoder. Times are
/// explicit sample indices; nothing sleeps and no rig is involved.
#[cfg(test)]
mod dial_change_signal_tests {
    use super::{apply_dial_change, boundary_anchored_slice};
    use crate::coordinator::pipeline::DecodeWindow;
    use crate::coordinator::{derive_dsp_timing, WINDOW_LEAD_SECS};
    use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
    use std::sync::atomic::{AtomicU64, Ordering};

    const SR: usize = 12_000;
    /// 0.1 s of 12 kHz audio per loop iteration.
    const BATCH: usize = SR / 10;
    /// Capture start (s past the epoch): 2 s before the slot-1 boundary at
    /// 105 s, the layout `anchored_slice_tests` validates.
    const START_SECS: f64 = 103.0;
    /// Slot boundaries (UTC-aligned multiples of 15 s).
    const SLOT1_SECS: f64 = 105.0;
    const SLOT2_SECS: f64 = 120.0;
    /// The dial the slot-1 message is transmitted and captured on.
    const DIAL_A: u64 = 14_074_000;
    /// Retune observed in the batch ending 14.8 s into the capture, i.e.
    /// 12.8 s after the slot-1 boundary: after the 12.64 s message has ended
    /// and before the 13 s decode deadline.
    const RETUNE_AT_SAMPLE: usize = 14 * SR + 8 * BATCH;

    fn utc(secs: f64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH
            + chrono::Duration::nanoseconds((secs * 1_000_000_000.0).round() as i64)
    }

    /// `text` modulated at 2000 Hz audio (+500 Hz over the 1500 Hz base).
    fn message_audio(text: &str) -> Vec<f32> {
        let symbols = Ft8Encoder::new()
            .encode_message(text, None)
            .expect("fixture message encodes");
        Ft8Modulator::new_default()
            .expect("modulator")
            .modulate_symbols(&symbols, 500.0)
            .expect("fixture message modulates")
    }

    /// Silence from `START_SECS` lasting `len` samples, with each message
    /// starting exactly on its slot boundary.
    fn session_audio(len: usize, messages: &[(f64, &str)]) -> Vec<f32> {
        let mut audio = vec![0.0f32; len];
        for &(boundary_secs, text) in messages {
            let msg = message_audio(text);
            let at = ((boundary_secs - START_SECS) * SR as f64).round() as usize;
            audio[at..at + msg.len()].copy_from_slice(&msg);
        }
        audio
    }

    struct Emitted {
        window: DecodeWindow,
        texts: Vec<String>,
    }

    impl Emitted {
        fn heard(&self, call: &str) -> bool {
            self.texts.iter().any(|t| t.contains(call))
        }
    }

    /// The DSP worker's capture state, driven batch by batch.
    struct Capture {
        live_dial_hz: AtomicU64,
        last_known_dial_hz: u64,
        ft8_buffer: Vec<f32>,
        last_live_wf_samples: usize,
        bin_history: Vec<std::collections::VecDeque<f32>>,
        next_window_time: chrono::DateTime<chrono::Utc>,
        decoder: Ft8Decoder,
    }

    impl Capture {
        /// Capture already seeded on `seed_dial_hz`, before `START_SECS`.
        fn new(seed_dial_hz: u64) -> Self {
            let timing = Self::timing();
            Self {
                live_dial_hz: AtomicU64::new(seed_dial_hz),
                last_known_dial_hz: seed_dial_hz,
                ft8_buffer: Vec::new(),
                last_live_wf_samples: 0,
                bin_history: Vec::new(),
                next_window_time: pancetta_core::slot::next_phase_with_period(
                    utc(START_SECS),
                    timing.decode_phase,
                    timing.slot_ns,
                ),
                decoder: Ft8Decoder::new(Ft8Config::default()).expect("decoder"),
            }
        }

        fn timing() -> crate::coordinator::DspTiming {
            derive_dsp_timing(&pancetta_ft8::ProtocolParams::from_protocol(
                pancetta_ft8::Protocol::Ft8,
            ))
        }

        /// One DSP loop iteration ending at absolute sample `end`: append the
        /// batch, read the live dial once, apply the dial change, then emit
        /// if the buffer and the deadline allow.
        fn step(&mut self, batch: &[f32], end: usize) -> Option<Emitted> {
            let timing = Self::timing();
            self.ft8_buffer.extend_from_slice(batch);
            let cur_dial_hz = self.live_dial_hz.load(Ordering::Relaxed);
            apply_dial_change(
                cur_dial_hz,
                &mut self.last_known_dial_hz,
                &mut self.ft8_buffer,
                &mut self.last_live_wf_samples,
                &mut self.bin_history,
            );
            let now = utc(START_SECS + end as f64 / SR as f64);
            if self.ft8_buffer.len() < timing.window_samples || now < self.next_window_time {
                return None;
            }
            let slot_boundary = self.next_window_time - timing.decode_phase;
            let (start, len) = boundary_anchored_slice(
                self.ft8_buffer.len(),
                now,
                slot_boundary,
                SR,
                timing.window_samples,
                WINDOW_LEAD_SECS,
            );
            let window = DecodeWindow {
                samples: self.ft8_buffer[start..start + len].to_vec(),
                dial_hz: cur_dial_hz,
            };
            if self.ft8_buffer.len() > timing.overlap_samples {
                let excess = self.ft8_buffer.len() - timing.overlap_samples;
                self.ft8_buffer.drain(..excess);
                self.last_live_wf_samples = self.ft8_buffer.len();
            }
            self.next_window_time = pancetta_core::slot::next_phase_with_period(
                now,
                timing.decode_phase,
                timing.slot_ns,
            );
            let texts = self
                .decoder
                .decode_window(&window.samples)
                .expect("decode_window must not error on the fixture")
                .into_iter()
                .map(|m| m.text)
                .collect();
            Some(Emitted { window, texts })
        }

        /// Feed `audio` in 0.1 s batches, publishing `dial_at(batch_end)` to
        /// the live atomic before each batch's read (the Hamlib poll's
        /// store). Returns every emitted window.
        fn run(&mut self, audio: &[f32], dial_at: impl Fn(usize) -> u64) -> Vec<Emitted> {
            assert_eq!(audio.len() % BATCH, 0, "fixture uses whole batches");
            let mut emitted = Vec::new();
            for (i, batch) in audio.chunks(BATCH).enumerate() {
                let end = (i + 1) * BATCH;
                self.live_dial_hz.store(dial_at(end), Ordering::Relaxed);
                emitted.extend(self.step(batch, end));
            }
            emitted
        }
    }

    /// Slot 1 only: capture through the 13 s deadline (15 s of audio).
    fn slot1_run(new_dial_hz: u64) -> (Capture, Vec<Emitted>) {
        let audio = session_audio(15 * SR, &[(SLOT1_SECS, "CQ K5ARH EM12")]);
        let mut cap = Capture::new(DIAL_A);
        let emitted = cap.run(&audio, |end| {
            if end >= RETUNE_AT_SAMPLE {
                new_dial_hz
            } else {
                DIAL_A
            }
        });
        (cap, emitted)
    }

    /// Control: with the dial unchanged the fixture decodes at the 13 s
    /// deadline under the capture dial, so the retune cases below cannot pass
    /// merely because the fixture never decodes.
    #[test]
    fn unchanged_dial_decodes_under_capture_dial() {
        let (_, emitted) = slot1_run(DIAL_A);
        assert_eq!(emitted.len(), 1, "one window at the slot-1 deadline");
        assert!(
            emitted[0].heard("K5ARH"),
            "control must decode: {:?}",
            emitted[0].texts
        );
        assert_eq!(emitted[0].window.dial_hz, DIAL_A);
    }

    /// THE PAN-70 CASE: a complete message on DIAL_A, then an intra-band
    /// retune observed after the message ends but before the deadline. The
    /// old audio is discarded, so it can't be emitted under the new dial.
    /// Pre-PAN-70 this decoded K5ARH labelled with the new dial, i.e.
    /// reported off by exactly the retune delta.
    #[test]
    fn intra_band_retune_never_labels_old_audio_with_new_dial() {
        for delta in [1i64, -50, 16_000, -1_000, 100_000] {
            let new_dial = DIAL_A.checked_add_signed(delta).expect("dial in range");
            assert!(
                !crate::coordinator::is_band_change(DIAL_A, new_dial),
                "fixture premise: {delta:+} Hz stays inside 20 m"
            );
            let (cap, emitted) = slot1_run(new_dial);
            for w in &emitted {
                assert!(
                    !(w.heard("K5ARH") && w.window.dial_hz != DIAL_A),
                    "Δ{delta:+}: old-dial K5ARH emitted under dial {}",
                    w.window.dial_hz
                );
            }
            // Only batches appended after the one that exposed the retune
            // remain: nothing captured on DIAL_A survived.
            assert_eq!(
                cap.ft8_buffer.len(),
                15 * SR - RETUNE_AT_SAMPLE,
                "Δ{delta:+}: pre-retune samples survived the flush"
            );
            assert_eq!(cap.last_known_dial_hz, new_dial);
        }
    }

    /// After the flush, a clean message on the new dial in the next slot is
    /// decoded under the new dial once enough new audio has accumulated, and
    /// a window already built keeps its snapshot when the live dial moves on.
    #[test]
    fn new_dial_signal_decodes_after_flush_with_stable_snapshot() {
        const DIAL_B: u64 = DIAL_A + 16_000;
        let audio = session_audio(
            30 * SR,
            &[(SLOT1_SECS, "CQ K5ARH EM12"), (SLOT2_SECS, "CQ K2BBB EM12")],
        );
        let mut cap = Capture::new(DIAL_A);
        let emitted = cap.run(&audio, |end| {
            if end >= RETUNE_AT_SAMPLE {
                DIAL_B
            } else {
                DIAL_A
            }
        });

        assert!(!emitted.is_empty(), "the new dial must produce a window");
        for w in &emitted {
            assert_eq!(w.window.dial_hz, DIAL_B, "every window follows the retune");
            assert!(!w.heard("K5ARH"), "old-dial audio emitted under {DIAL_B}");
        }
        let recovered = emitted
            .iter()
            .find(|w| w.heard("K2BBB"))
            .expect("the clean new-dial message must decode after the flush");

        // The live dial moves on after the window was built: the next batch
        // flushes the buffer, but the built window keeps its snapshot.
        let snapshot = recovered.window.clone();
        cap.live_dial_hz.store(DIAL_B + 1, Ordering::Relaxed);
        let more = cap.step(&[0.0; BATCH], 30 * SR + BATCH);
        assert!(more.is_none());
        assert!(cap.ft8_buffer.is_empty(), "the later retune flushed again");
        assert_eq!(recovered.window.dial_hz, DIAL_B, "snapshot unchanged");
        assert_eq!(recovered.window.samples, snapshot.samples);
    }
}

#[cfg(test)]
mod mode_flush_tests {
    use super::mode_flush_decision;

    #[test]
    fn mode_flush_decision_same_mode_no_flush() {
        assert!(!mode_flush_decision(0, 0));
    }

    #[test]
    fn mode_flush_decision_changed_mode_flushes() {
        assert!(mode_flush_decision(0, 1)); // FT8 -> FT4
        assert!(mode_flush_decision(1, 2)); // FT4 -> FT2
        assert!(mode_flush_decision(2, 0)); // FT2 -> FT8
    }
}

#[cfg(test)]
mod anchored_slice_tests {
    use super::boundary_anchored_slice;
    use crate::coordinator::{EXTENDED_WINDOW_LEAD_SECS, WINDOW_LEAD_SECS};

    const SR: usize = 12_000;
    // FT8 message span in samples (12.64 s) — the slice floor.
    const MSG_SAMPLES: usize = (SR as f64 * 12.64) as usize; // 151_680

    fn utc(secs: f64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH
            + chrono::Duration::nanoseconds((secs * 1_000_000_000.0) as i64)
    }

    /// With a generously long buffer and a nominal emit at boundary+13 s, the
    /// window's sample 0 lands at slot_boundary − WINDOW_LEAD_SECS. We verify
    /// by reconstructing the wall-clock time of sample 0 from the buffer end.
    #[test]
    fn sample_zero_lands_at_boundary_minus_lead() {
        let boundary = utc(100.0);
        let emit_now = boundary + chrono::Duration::milliseconds(13_000); // exactly :13
                                                                          // Buffer holds 20 s of audio, newest sample == emit_now.
        let buffer_len = SR * 20;
        let (start, len) = boundary_anchored_slice(
            buffer_len,
            emit_now,
            boundary,
            SR,
            MSG_SAMPLES,
            WINDOW_LEAD_SECS,
        );
        // Wall-clock of sample 0 = emit_now − len/SR.
        let sample0_secs_before_emit = len as f64 / SR as f64;
        let sample0_time_secs = 13.0 - sample0_secs_before_emit; // relative to boundary
        let expected = -WINDOW_LEAD_SECS;
        assert!(
            (sample0_time_secs - expected).abs() < 0.001,
            "sample0 at {sample0_time_secs}s rel boundary, expected {expected}s (start={start}, len={len})"
        );
        // Window must contain the full message span.
        assert!(
            len >= MSG_SAMPLES,
            "window {len} shorter than message {MSG_SAMPLES}"
        );
        assert_eq!(start + len, buffer_len, "window must end at buffer end");
    }

    /// The dt correction: a station on the boundary decodes at slice-relative
    /// time_offset == WINDOW_LEAD_SECS (because sample 0 is `lead` before the
    /// boundary), and the coordinator subtracts WINDOW_LEAD_SECS → DT ≈ 0.
    #[test]
    fn dt_correction_yields_zero_for_boundary_station() {
        // Decoder reports time_offset relative to window sample 0. Sample 0 is
        // WINDOW_LEAD_SECS before the boundary, so a boundary-aligned signal's
        // first symbol begins WINDOW_LEAD_SECS into the window.
        let decoder_time_offset = WINDOW_LEAD_SECS;
        let corrected = decoder_time_offset - WINDOW_LEAD_SECS;
        assert!(corrected.abs() < 0.1, "corrected DT {corrected} not ≈ 0");
    }

    /// Emit jitter (firing late at :13.4 instead of :13.0) does NOT shift the
    /// reported DT, because the anchor is the scheduled boundary, not `now`:
    /// the window simply grows by the jitter amount, keeping sample 0 fixed at
    /// slot_boundary − lead.
    #[test]
    fn emit_jitter_does_not_move_sample_zero() {
        let boundary = utc(100.0);
        let buffer_len = SR * 20;

        let (_, len_on_time) = boundary_anchored_slice(
            buffer_len,
            boundary + chrono::Duration::milliseconds(13_000),
            boundary,
            SR,
            MSG_SAMPLES,
            WINDOW_LEAD_SECS,
        );
        let (_, len_late) = boundary_anchored_slice(
            buffer_len,
            boundary + chrono::Duration::milliseconds(13_400), // 0.4 s late
            boundary,
            SR,
            MSG_SAMPLES,
            WINDOW_LEAD_SECS,
        );
        // Sample-0 wall-clock time is (emit_now − len/SR). For it to be fixed at
        // boundary − lead across both emits, len must grow by exactly the jitter
        // (0.4 s → 4800 samples).
        let sample0_on_time = 13.0 - len_on_time as f64 / SR as f64;
        let sample0_late = 13.4 - len_late as f64 / SR as f64;
        assert!(
            (sample0_on_time - sample0_late).abs() < 0.001,
            "sample0 moved under jitter: {sample0_on_time} vs {sample0_late}"
        );
        assert!((sample0_on_time + WINDOW_LEAD_SECS).abs() < 0.001);
    }

    /// Short buffer (just over the message span): the slice clamps to the whole
    /// buffer rather than indexing out of bounds, still delivering the message.
    #[test]
    fn short_buffer_clamps_in_bounds() {
        let boundary = utc(100.0);
        let emit_now = boundary + chrono::Duration::milliseconds(13_000);
        let buffer_len = MSG_SAMPLES + 100; // only just enough for a message
        let (start, len) = boundary_anchored_slice(
            buffer_len,
            emit_now,
            boundary,
            SR,
            MSG_SAMPLES,
            WINDOW_LEAD_SECS,
        );
        assert!(start + len <= buffer_len, "slice out of bounds");
        assert!(len >= MSG_SAMPLES, "clamped window dropped the message");
    }

    /// End-to-end synthetic check: build a realistic 15 s capture buffer with a
    /// modulated FT8 message whose first symbol begins EXACTLY at the simulated
    /// slot boundary, run the SAME `boundary_anchored_slice` the live pipeline
    /// uses, decode the slice, apply the live `−WINDOW_LEAD_SECS` DT correction,
    /// and assert the reported DT is ≈0 (not ≈ +2 s) AND the message still
    /// decodes (decode-count non-regression: a message on the boundary is
    /// recovered intact).
    #[test]
    fn synthetic_boundary_signal_reports_dt_near_zero() {
        use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};

        // Modulate "CQ K5ARH EM12" at +500 Hz offset (→ 2000 Hz; base is 1500).
        let mut enc = Ft8Encoder::new();
        let symbols = enc.encode_message("CQ K5ARH EM12", None).unwrap();
        let mut modu = Ft8Modulator::new_default().unwrap();
        let msg_audio = modu.modulate_symbols(&symbols, 500.0).unwrap();

        // Build a 15 s buffer (180000 samples). The live capture has sample 0
        // ≈ slot_boundary − 2 s, i.e. the message (which starts AT the boundary)
        // begins 2 s into the buffer. Reproduce that layout exactly.
        let buffer_len = SR * 15;
        let boundary = utc(100.0);
        let emit_now = boundary + chrono::Duration::milliseconds(13_000); // :13 emit
        let pre_boundary_secs = 2.0_f64; // buffer sample 0 = boundary − 2 s
        let msg_start = (pre_boundary_secs * SR as f64) as usize;

        let mut buffer = vec![0.0f32; buffer_len];
        for (i, &s) in msg_audio.iter().enumerate() {
            if msg_start + i < buffer.len() {
                buffer[msg_start + i] = s;
            }
        }

        // Anchored slice exactly as the live pipeline computes it.
        let (start, len) = boundary_anchored_slice(
            buffer.len(),
            emit_now,
            boundary,
            SR,
            MSG_SAMPLES,
            WINDOW_LEAD_SECS,
        );
        let window = &buffer[start..start + len];

        // Window must still contain the whole message (non-regression).
        assert!(
            len >= MSG_SAMPLES,
            "window {len} < message span {MSG_SAMPLES}"
        );

        let mut dec = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let msgs = dec.decode_window(window).unwrap();
        let found = msgs.iter().find(|m| m.text.contains("K5ARH"));
        assert!(
            found.is_some(),
            "boundary message did not decode from anchored window (n={})",
            msgs.len()
        );

        // Apply the live DT correction (mirrors ft8.rs: time_offset − lead).
        let corrected = found.unwrap().time_offset - WINDOW_LEAD_SECS;
        assert!(
            corrected.abs() <= 0.1,
            "corrected DT {corrected:.3}s not ≈ 0 (raw {:.3}s)",
            found.unwrap().time_offset
        );
    }

    // ------------------------------------------------------------------
    // Task W5.3 (decoder-tp-sensitivity plan): capture window covers the
    // real dt range. TDD evidence that (a) a dt=-0.8s signal genuinely
    // cannot decode today via the real production `boundary_anchored_slice`
    // + default `WINDOW_LEAD_SECS`, (b) it CAN decode once the lead is
    // widened to `EXTENDED_WINDOW_LEAD_SECS` (Task W5.3's flag), and (c) a
    // dt=+2.2s signal (the brief's other named case) ALREADY decodes today
    // with the default lead — confirming the trailing/positive edge did not
    // need widening, only the negative side did.
    // ------------------------------------------------------------------

    /// Build a synthetic "continuous rolling capture" buffer with a real
    /// modulated FT8 message whose true start is `dt_secs` relative to the
    /// slot boundary, run the REAL `boundary_anchored_slice` (given
    /// `lead_secs`) exactly as the live pipeline would, and report whether
    /// "CQ K5ARH EM12" decodes from the resulting window.
    ///
    /// Mirrors `synthetic_boundary_signal_reports_dt_near_zero`'s realism
    /// (real `Ft8Encoder`/`Ft8Modulator`, real `boundary_anchored_slice`,
    /// real `Ft8Decoder::decode_window`) but parametrized over dt and lead,
    /// and models the buffer as ending exactly at `emit_now` (the real
    /// pipeline's invariant: the rolling capture's newest sample IS "now").
    fn dt_signal_decodes(dt_secs: f64, lead_secs: f64) -> bool {
        use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};

        let mut enc = Ft8Encoder::new();
        let symbols = enc.encode_message("CQ K5ARH EM12", None).unwrap();
        let mut modu = Ft8Modulator::new_default().unwrap();
        let msg_audio = modu.modulate_symbols(&symbols, 500.0).unwrap();

        // Boundary at absolute t=10s within a buffer that ends exactly at
        // emit_now (decode_phase=13s past boundary, the FT8 default) — the
        // real ft8_buffer's invariant (newest sample == "now").
        let boundary_secs = 10.0_f64;
        let emit_now_secs = boundary_secs + 13.0;
        let buffer_len = (emit_now_secs * SR as f64).round() as usize;
        let mut buffer = vec![0.0f32; buffer_len];
        let msg_start = ((boundary_secs + dt_secs) * SR as f64).round() as isize;
        for (i, &s) in msg_audio.iter().enumerate() {
            let idx = msg_start + i as isize;
            if idx >= 0 && (idx as usize) < buffer.len() {
                buffer[idx as usize] = s;
            }
        }

        let boundary = utc(boundary_secs);
        let emit_now = utc(emit_now_secs);
        let (start, len) =
            boundary_anchored_slice(buffer.len(), emit_now, boundary, SR, MSG_SAMPLES, lead_secs);
        let window = &buffer[start..start + len];

        let mut dec = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let msgs = dec.decode_window(window).unwrap_or_default();
        msgs.iter().any(|m| m.text.contains("K5ARH"))
    }

    /// RED (today's real limitation): a station transmitting 0.8s before the
    /// slot boundary is NOT recoverable with the current default 0.5s lead
    /// — the buffer's sample 0 (slot_boundary − 0.5s) is genuinely later
    /// than the message's true start (slot_boundary − 0.8s), so part of the
    /// message's audio is simply absent from the window handed to the
    /// decoder. This is the real, empirically-measured gap this task fixes.
    #[test]
    fn dt_minus_0_8s_does_not_decode_with_default_lead() {
        assert!(
            !dt_signal_decodes(-0.8, WINDOW_LEAD_SECS),
            "dt=-0.8s must NOT decode with the default 0.5s lead (today's real capture-window limitation)"
        );
    }

    /// GREEN: the same dt=-0.8s signal DOES decode once the lead is widened
    /// to `EXTENDED_WINDOW_LEAD_SECS` (1.0s) — using REAL audio genuinely
    /// present earlier in the rolling buffer, not zero-padding (Batch 36 C3
    /// already showed silence-padding is net-negative; this test's buffer
    /// contains the actual modulated signal at that position, not silence).
    #[test]
    fn dt_minus_0_8s_decodes_with_extended_lead() {
        assert!(
            dt_signal_decodes(-0.8, EXTENDED_WINDOW_LEAD_SECS),
            "dt=-0.8s must decode once the capture window's lead is widened (Task W5.3)"
        );
    }

    /// Extended lead reaches comfortably past the brief's -1.0s dt target
    /// (structural floor ≈ -(lead + 0.16s) ≈ -1.16s with the 1.0s extended
    /// lead), not just the -0.8s test case.
    #[test]
    fn dt_minus_1_0s_decodes_with_extended_lead() {
        assert!(
            dt_signal_decodes(-1.0, EXTENDED_WINDOW_LEAD_SECS),
            "dt=-1.0s must decode with the extended lead (brief's target dt range floor)"
        );
    }

    /// Non-negative control: the brief's other named test case (dt=+2.2s)
    /// ALREADY decodes today with the UNCHANGED default lead — real trailing
    /// audio plus LDPC redundancy already cover it (confirmed by direct
    /// investigation before implementing anything). This is a regression
    /// guard, not a fix target: it must keep passing unchanged by this task.
    #[test]
    fn dt_plus_2_2s_already_decodes_with_default_lead() {
        assert!(
            dt_signal_decodes(2.2, WINDOW_LEAD_SECS),
            "dt=+2.2s must already decode with the default lead (non-regression control)"
        );
    }

    /// Widening the lead must never regress the already-working positive
    /// side (the extended lead only moves the window's START earlier; the
    /// END — and therefore every positive-dt signal's coverage — is
    /// unchanged).
    #[test]
    fn dt_plus_2_2s_still_decodes_with_extended_lead() {
        assert!(
            dt_signal_decodes(2.2, EXTENDED_WINDOW_LEAD_SECS),
            "dt=+2.2s must still decode with the extended lead (no regression on the trailing side)"
        );
    }

    /// Boundary station (dt=0) must still decode identically regardless of
    /// which lead is in effect — a basic sanity/non-regression check for
    /// both configurations.
    #[test]
    fn dt_zero_decodes_with_either_lead() {
        assert!(
            dt_signal_decodes(0.0, WINDOW_LEAD_SECS),
            "dt=0 default lead"
        );
        assert!(
            dt_signal_decodes(0.0, EXTENDED_WINDOW_LEAD_SECS),
            "dt=0 extended lead"
        );
    }
}

#[cfg(test)]
mod band_tests {
    use super::band_for_freq_hz;

    #[test]
    fn maps_common_bands() {
        assert_eq!(band_for_freq_hz(14_074_000), Some("20m"));
        assert_eq!(band_for_freq_hz(7_074_000), Some("40m"));
        assert_eq!(band_for_freq_hz(3_573_000), Some("80m"));
        assert_eq!(band_for_freq_hz(28_074_000), Some("10m"));
        assert_eq!(band_for_freq_hz(50_313_000), Some("6m"));
    }

    #[test]
    fn unknown_and_zero_have_no_band() {
        assert_eq!(band_for_freq_hz(0), None); // no rig / not polled
        assert_eq!(band_for_freq_hz(12_000_000), None); // between 30m and 20m
        assert_eq!(band_for_freq_hz(100_000_000), None); // FM broadcast, not ham
    }

    #[test]
    fn band_edges_inclusive() {
        assert_eq!(band_for_freq_hz(14_000_000), Some("20m"));
        assert_eq!(band_for_freq_hz(14_350_000), Some("20m"));
        assert_eq!(band_for_freq_hz(13_999_000), None);
        assert_eq!(band_for_freq_hz(14_351_000), None);
    }
}

#[cfg(test)]
mod median_tests {
    use super::rolling_median;

    #[test]
    fn empty_returns_zero() {
        assert_eq!(rolling_median(&[]), 0.0);
    }

    #[test]
    fn single_value_is_itself() {
        assert_eq!(rolling_median(&[7.0]), 7.0);
    }

    #[test]
    fn odd_length_picks_middle() {
        assert_eq!(rolling_median(&[1.0, 5.0, 3.0]), 3.0);
    }

    #[test]
    fn even_length_picks_upper_middle() {
        // For waterfall use (noise-floor estimation), upper-middle is fine —
        // we don't need the strict midpoint average.
        assert_eq!(rolling_median(&[1.0, 2.0, 3.0, 4.0]), 3.0);
    }

    #[test]
    fn ignores_nan() {
        // partial_cmp returns None for NaN; we use unwrap_or(Equal). Just
        // verify it doesn't panic.
        let xs = [1.0, f32::NAN, 3.0, 2.0];
        let m = rolling_median(&xs);
        assert!(m.is_finite());
    }
}

#[cfg(test)]
mod dead_air_tests {
    use super::{is_dead_air, window_peak, DEAD_AIR_PEAK_THRESHOLD};

    fn silent_buf(len: usize) -> Vec<f32> {
        vec![0.0002_f32; len]
    }

    fn signal_buf(peak: f32, len: usize) -> Vec<f32> {
        // Alternating +peak / -peak so we exercise the abs() path.
        (0..len)
            .map(|i| if i % 2 == 0 { peak } else { -peak })
            .collect()
    }

    // --- window_peak ---

    #[test]
    fn peak_of_silent_buffer_is_approximately_silence_floor() {
        let buf = silent_buf(1000);
        let p = window_peak(&buf);
        assert!((p - 0.0002).abs() < 1e-7, "expected peak ≈ 0.0002, got {p}");
    }

    #[test]
    fn peak_of_signal_buffer_matches_amplitude() {
        let buf = signal_buf(0.2, 1000);
        let p = window_peak(&buf);
        assert!((p - 0.2).abs() < 1e-7, "expected peak ≈ 0.2, got {p}");
    }

    #[test]
    fn peak_of_empty_buffer_is_zero() {
        assert_eq!(window_peak(&[]), 0.0);
    }

    // --- is_dead_air ---

    #[test]
    fn dead_air_true_for_silence_floor() {
        // Peak ≈ 0.0002, which is below threshold 0.001.
        assert!(
            is_dead_air(&silent_buf(1000)),
            "silence floor must be detected as dead air"
        );
    }

    #[test]
    fn dead_air_false_for_weak_ft8_signal() {
        // Weakest real FT8 ≥ 0.05 — well above threshold.
        assert!(
            !is_dead_air(&signal_buf(0.05, 1000)),
            "0.05 peak must NOT be detected as dead air"
        );
    }

    #[test]
    fn dead_air_false_for_strong_ft8_signal() {
        assert!(
            !is_dead_air(&signal_buf(0.2, 1000)),
            "0.2 peak must NOT be detected as dead air"
        );
    }

    #[test]
    fn dead_air_true_for_empty_buffer() {
        // Empty → peak = 0.0 < 0.001 → dead air.
        assert!(is_dead_air(&[]), "empty buffer must be treated as dead air");
    }

    #[test]
    fn threshold_boundary_just_below_is_dead_air() {
        // A single sample at exactly (threshold − ε) is dead air.
        let just_below = DEAD_AIR_PEAK_THRESHOLD - 1e-6;
        assert!(is_dead_air(&[just_below]));
    }

    #[test]
    fn threshold_boundary_at_threshold_is_not_dead_air() {
        // A single sample at exactly threshold is NOT dead air (strict <).
        assert!(!is_dead_air(&[DEAD_AIR_PEAK_THRESHOLD]));
    }
}

#[cfg(test)]
mod reanchor_decode_window_tests {
    use super::reanchor_decode_window;
    use pancetta_core::slot_clock::WallClockJump;

    const FT8_SLOT_NS: i64 = 15_000_000_000;

    fn decode_phase() -> chrono::Duration {
        chrono::Duration::seconds(13)
    }

    fn utc(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::UNIX_EPOCH + chrono::Duration::seconds(secs)
    }

    fn window_after(t: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
        pancetta_core::slot::next_phase_with_period(t, decode_phase(), FT8_SLOT_NS)
    }

    #[test]
    fn decode_window_reanchors_after_backward_step() {
        let t = utc(1_000_000_005);
        let next_window_time = window_after(t);
        let now = t - chrono::Duration::seconds(600);
        let (reanchored, jump) =
            reanchor_decode_window(now, next_window_time, decode_phase(), FT8_SLOT_NS);
        assert_eq!(reanchored, window_after(now));
        assert!(reanchored > now);
        assert!(reanchored - now <= chrono::Duration::seconds(15));
        assert!(matches!(jump, Some(WallClockJump::Backward(_))));
    }

    #[test]
    fn decode_window_reports_forward_jump_for_flush() {
        let next_window_time = window_after(utc(1_000_000_005));
        let now = next_window_time + chrono::Duration::seconds(3600);
        let (reanchored, jump) =
            reanchor_decode_window(now, next_window_time, decode_phase(), FT8_SLOT_NS);
        assert!(matches!(jump, Some(WallClockJump::Forward(_))));
        assert!(reanchored > now);
        assert!(reanchored - now <= chrono::Duration::seconds(15));
    }

    #[test]
    fn decode_window_unchanged_without_jump() {
        let next_window_time = window_after(utc(1_000_000_005));
        let now = next_window_time + chrono::Duration::milliseconds(40);
        assert_eq!(
            reanchor_decode_window(now, next_window_time, decode_phase(), FT8_SLOT_NS),
            (next_window_time, None)
        );
    }
}

#[cfg(test)]
mod wav_recorder_tests {
    use super::*;

    const FILE: u64 = 1000;

    fn seed(dir: &std::path::Path, names: &[&str]) {
        for n in names {
            std::fs::write(dir.join(n), vec![0u8; FILE as usize]).unwrap();
        }
    }

    fn wav_names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".wav"))
            .collect();
        v.sort();
        v
    }

    fn tone() -> Vec<f32> {
        (0..1200).map(|i| (i as f32 * 0.3).sin() * 0.5).collect()
    }

    fn ts() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap()
    }

    /// sophon 2026-10-15: the NAS offload moved files out from under the
    /// recorder, which still believed it was at the cap and deleted the
    /// oldest remaining recording on every window (1,462 deletions while the
    /// dir shrank 10 GB → 3.1 GB).
    #[test]
    fn external_removal_is_not_mistaken_for_a_full_buffer() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav"]);
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();

        // Offload job moves two files away.
        std::fs::remove_file(tmp.path().join("ft8_a.wav")).unwrap();
        std::fs::remove_file(tmp.path().join("ft8_b.wav")).unwrap();

        rec.write_window(&tone(), &ts(), 0);

        let names = wav_names(tmp.path());
        assert!(
            names.contains(&"ft8_c.wav".to_string()),
            "evicted despite free space: {names:?}"
        );
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn real_overflow_still_evicts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav"]);
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();

        rec.write_window(&tone(), &ts(), 0);

        let names = wav_names(tmp.path());
        assert!(
            !names.contains(&"ft8_a.wav".to_string()),
            "oldest not evicted: {names:?}"
        );
        assert!(names.contains(&"ft8_b.wav".to_string()));
        assert!(names.contains(&"ft8_c.wav".to_string()));
    }

    /// Curated fixture subdirectories (noise_1000/) and non-WAV files are
    /// outside the recorder's flat eviction scope, so they must not count
    /// toward the cap either.
    #[test]
    fn size_counts_only_top_level_wavs() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), &["ft8_a.wav", "notes.txt"]);
        std::fs::create_dir(tmp.path().join("noise_1000")).unwrap();
        seed(
            &tmp.path().join("noise_1000"),
            &["n1.wav", "n2.wav", "n3.wav"],
        );
        assert_eq!(WavRecorder::dir_size(tmp.path()).unwrap(), (FILE, true));
    }

    /// A WAV that can't be stat'd (here a symlink loop: ELOOP for every user,
    /// root included) marks the scan incomplete but keeps the readable total.
    #[cfg(unix)]
    fn seed_with_unstatable(dir: &std::path::Path) {
        seed(dir, &["ft8_a.wav", "ft8_b.wav"]);
        std::os::unix::fs::symlink("ft8_loop.wav", dir.join("ft8_loop.wav")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn partial_scan_reports_incomplete_with_readable_total() {
        let tmp = tempfile::tempdir().unwrap();
        seed_with_unstatable(tmp.path());
        assert_eq!(
            WavRecorder::dir_size(tmp.path()).unwrap(),
            (2 * FILE, false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_partial_scan_keeps_readable_usage() {
        let tmp = tempfile::tempdir().unwrap();
        seed_with_unstatable(tmp.path());
        let rec = WavRecorder::open(tmp.path().to_path_buf(), 10 * FILE).unwrap();
        assert_eq!(rec.total_bytes, 2 * FILE);
    }

    /// A partial re-measure at the cap must neither evict from the stale
    /// estimate (which could delete every readable recording) nor write past
    /// an unknown size: skip the window and keep everything.
    #[cfg(unix)]
    #[test]
    fn partial_remeasure_skips_the_write_and_evicts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        seed_with_unstatable(tmp.path());
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();
        rec.total_bytes = 50 * FILE; // stale at-cap estimate, far above what's on disk
        let before = wav_names(tmp.path());

        rec.write_window(&tone(), &ts(), 0);

        assert_eq!(
            wav_names(tmp.path()),
            before,
            "partial re-measure changed the buffer"
        );
    }

    /// An unreadable directory must not be mistaken for an empty one: the
    /// at-cap estimate survives, so eviction isn't bypassed.
    #[test]
    fn failed_remeasure_keeps_the_at_cap_estimate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recordings");
        let mut rec = WavRecorder::open(dir.clone(), 3 * FILE).unwrap();
        seed(&dir, &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav"]);
        rec.total_bytes = 3 * FILE;
        std::fs::remove_dir_all(&dir).unwrap();

        rec.write_window(&tone(), &ts(), 0);

        assert!(
            rec.total_bytes >= 3 * FILE,
            "estimate reset to {}",
            rec.total_bytes
        );
    }

    /// Name `write_window` gives the window recorded at [`ts`] with no band.
    fn new_wav() -> String {
        format!("ft8_{}.wav", ts().format("%Y%m%d_%H%M%S"))
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        v.sort();
        v
    }

    /// Arm the recorder to run `action` at the top of eviction iteration
    /// `iteration`, after the at-cap measurement.
    fn at_eviction(
        rec: &mut WavRecorder,
        iteration: usize,
        action: impl FnOnce() + Send + 'static,
    ) {
        rec.eviction_hook = Some((iteration, Box::new(action)));
    }

    /// The NAS offload's move: rename `name` out of `dir` into `nas`.
    fn offload(dir: &std::path::Path, nas: &std::path::Path, name: &str) -> impl FnOnce() + Send {
        let (from, to) = (dir.join(name), nas.join(name));
        move || std::fs::rename(from, to).unwrap()
    }

    /// PAN-211: at exactly the cap, the offload moves `b` out after the
    /// at-cap measurement. The real size is now under the cap, so nothing
    /// may be deleted.
    #[test]
    fn offload_move_after_measurement_at_exact_cap_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let nas = tempfile::tempdir().unwrap();
        seed(tmp.path(), &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav"]);
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();
        at_eviction(&mut rec, 0, offload(tmp.path(), nas.path(), "ft8_b.wav"));

        rec.write_window(&tone(), &ts(), 0);

        assert!(rec.eviction_hook.is_none(), "offload move never ran");
        assert_eq!(wav_names(nas.path()), sorted(&["ft8_b.wav"]));
        assert_eq!(
            wav_names(tmp.path()),
            sorted(&["ft8_a.wav", "ft8_c.wav", &new_wav()]),
            "evicted for bytes the offload already removed"
        );
    }

    /// PAN-211: over the cap by one file, the offload moves `b` out after the
    /// at-cap measurement. One deletion (`a`) covers the real overflow; `c`
    /// must not die to pay for `b`'s bytes.
    #[test]
    fn offload_move_after_measurement_deletes_only_the_real_overflow() {
        let tmp = tempfile::tempdir().unwrap();
        let nas = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav", "ft8_d.wav"],
        );
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();
        at_eviction(&mut rec, 0, offload(tmp.path(), nas.path(), "ft8_b.wav"));

        rec.write_window(&tone(), &ts(), 0);

        assert!(rec.eviction_hook.is_none(), "offload move never ran");
        assert_eq!(wav_names(nas.path()), sorted(&["ft8_b.wav"]));
        assert_eq!(
            wav_names(tmp.path()),
            sorted(&["ft8_c.wav", "ft8_d.wav", &new_wav()]),
            "over-evicted: paid for bytes the offload already removed"
        );
    }

    /// PAN-211: the offload moves `b` out after the recorder has already
    /// deleted `a`. The real size is then under the cap, so the second
    /// deletion must not happen.
    #[test]
    fn offload_move_between_deletions_stops_the_next_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let nas = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav", "ft8_d.wav"],
        );
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();
        at_eviction(&mut rec, 1, offload(tmp.path(), nas.path(), "ft8_b.wav"));

        rec.write_window(&tone(), &ts(), 0);

        assert!(rec.eviction_hook.is_none(), "offload move never ran");
        assert_eq!(wav_names(nas.path()), sorted(&["ft8_b.wav"]));
        assert_eq!(
            wav_names(tmp.path()),
            sorted(&["ft8_c.wav", "ft8_d.wav", &new_wav()]),
            "over-evicted: paid for bytes the offload already removed"
        );
    }

    /// A partial re-measure between deletions is as conservative as one at
    /// the cap: stop deleting, skip the write, and keep the last estimate.
    #[cfg(unix)]
    #[test]
    fn partial_remeasure_between_deletions_stops_and_skips_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        seed(
            tmp.path(),
            &["ft8_a.wav", "ft8_b.wav", "ft8_c.wav", "ft8_d.wav"],
        );
        let mut rec = WavRecorder::open(tmp.path().to_path_buf(), 3 * FILE).unwrap();
        let dir = tmp.path().to_path_buf();
        at_eviction(&mut rec, 1, move || {
            std::os::unix::fs::symlink("ft8_loop.wav", dir.join("ft8_loop.wav")).unwrap();
        });

        rec.write_window(&tone(), &ts(), 0);

        assert!(rec.eviction_hook.is_none(), "fault never injected");
        assert_eq!(
            wav_names(tmp.path()),
            sorted(&["ft8_b.wav", "ft8_c.wav", "ft8_d.wav", "ft8_loop.wav"]),
            "deleted or wrote past a partial re-measure"
        );
        assert_eq!(rec.total_bytes, 3 * FILE, "last estimate not kept");
    }
}

#[cfg(test)]
mod wav_recording_gate_tests {
    use super::*;

    fn rec(auto_record: bool, directory: &str) -> pancetta_config::RecordingConfig {
        pancetta_config::RecordingConfig {
            auto_record,
            directory: directory.into(),
            ..Default::default()
        }
    }

    fn home() -> std::path::PathBuf {
        std::path::PathBuf::from("/home/op")
    }

    #[test]
    fn default_config_is_disabled() {
        assert_eq!(
            resolve_wav_recording(&Default::default(), false, false, Some(&home())),
            WavRecording::DisabledByConfig
        );
    }

    #[test]
    fn disabled_wins_over_replay_and_no_audio() {
        let off = rec(false, "~/rec");
        assert_eq!(
            resolve_wav_recording(&off, true, false, Some(&home())),
            WavRecording::DisabledByConfig
        );
        assert_eq!(
            resolve_wav_recording(&off, false, true, Some(&home())),
            WavRecording::DisabledByConfig
        );
    }

    #[test]
    fn enabled_expands_leading_tilde() {
        assert_eq!(
            resolve_wav_recording(
                &rec(true, "~/.pancetta/recordings"),
                false,
                false,
                Some(&home())
            ),
            WavRecording::Enabled {
                dir: home().join(".pancetta").join("recordings")
            }
        );
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "~"), false, false, Some(&home())),
            WavRecording::Enabled { dir: home() }
        );
    }

    #[test]
    fn windows_style_tilde_expands() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "~\\rec"), false, false, Some(&home())),
            WavRecording::Enabled {
                dir: home().join("rec")
            }
        );
    }

    #[test]
    fn absolute_and_relative_pass_through() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "/abs/rec"), false, false, Some(&home())),
            WavRecording::Enabled {
                dir: "/abs/rec".into()
            }
        );
        assert_eq!(
            resolve_wav_recording(&rec(true, "rec"), false, false, Some(&home())),
            WavRecording::Enabled { dir: "rec".into() }
        );
        // `~user` is not expanded (same as the config loader's tilde).
        assert_eq!(
            resolve_wav_recording(&rec(true, "~user/rec"), false, false, Some(&home())),
            WavRecording::Enabled {
                dir: "~user/rec".into()
            }
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "  ~/rec  "), false, false, Some(&home())),
            WavRecording::Enabled {
                dir: home().join("rec")
            }
        );
    }

    #[test]
    fn empty_directory_falls_back_to_dot_pancetta_recordings() {
        for directory in ["", "   "] {
            assert_eq!(
                resolve_wav_recording(&rec(true, directory), false, false, Some(&home())),
                WavRecording::Enabled {
                    dir: home().join(".pancetta").join("recordings")
                },
                "directory = {directory:?}"
            );
        }
    }

    #[test]
    fn replay_overrides_enabled() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "~/rec"), true, false, Some(&home())),
            WavRecording::SuppressedReplay
        );
    }

    #[test]
    fn no_audio_overrides_enabled() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "~/rec"), false, true, Some(&home())),
            WavRecording::SuppressedNoAudio
        );
    }

    #[test]
    fn tilde_without_home_disables() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "~/rec"), false, false, None),
            WavRecording::NoHomeDir {
                directory: "~/rec".to_string()
            }
        );
    }

    #[test]
    fn absolute_without_home_still_records() {
        assert_eq!(
            resolve_wav_recording(&rec(true, "/abs/rec"), false, false, None),
            WavRecording::Enabled {
                dir: "/abs/rec".into()
            }
        );
    }

    #[test]
    fn startup_messages_name_the_setting() {
        let disabled = WavRecording::DisabledByConfig.startup_message();
        assert_eq!(
            disabled.as_deref(),
            Some(MSG_WAV_RECORDING_DISABLED),
            "exact string: end-to-end tests and docs pin it"
        );
        assert!(
            disabled
                .as_ref()
                .is_some_and(|m| m.contains("audio.recording.auto_record = true")),
            "the disabled line must name the setting: {disabled:?}"
        );

        let replay = WavRecording::SuppressedReplay.startup_message();
        assert_eq!(replay.as_deref(), Some(MSG_WAV_RECORDING_REPLAY));
        assert!(replay.is_some_and(|m| m.starts_with("Replay mode: ")));

        assert_eq!(
            WavRecording::SuppressedNoAudio.startup_message().as_deref(),
            Some(MSG_WAV_RECORDING_NO_AUDIO)
        );

        // `open()` logs the enabled line with the directory.
        assert_eq!(
            WavRecording::Enabled { dir: "/rec".into() }.startup_message(),
            None
        );
    }
}
