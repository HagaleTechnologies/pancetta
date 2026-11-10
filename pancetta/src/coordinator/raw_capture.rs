//! PAN-393: timed raw 48 kHz capture, one WAV per FT8 slot, into `raw48/`.
//!
//! `[audio.recording] raw_capture_minutes` (0 = off) turns this on for that
//! many minutes after the process started. The DSP worker owns one
//! [`RawCapture`], feeds it every interleaved capture batch after its own
//! dial and mode guards, and never touches a file: completed slots move
//! through a bounded queue to one writer thread ([`RawCaptureWriterHandle`]).
//!
//! Every decision is made by a pure, clock-injected type here
//! ([`RawSlotCutter`], [`tx_overlaps`], [`raw_slot_file_name`],
//! [`resolve_raw_capture`]) so it is unit-tested without audio or threads.
//!
//! Files go into a `raw48/` subdirectory so they stay out of the top-level
//! `*.wav` set the 10 GB decode-window cap, eviction and the corpus offload
//! count. `raw48/` has its own 2 GiB cap ([`RAW_CAPTURE_MAX_BYTES`]); reaching
//! it stops raw capture and never deletes anything.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

/// Subdirectory of the recording directory that holds raw captures.
pub(super) const RAW_CAPTURE_SUBDIR: &str = "raw48";
/// Size cap of `raw48/` (top-level `*.wav` only): 2 GiB.
pub(super) const RAW_CAPTURE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The only capture rate raw capture accepts.
pub(super) const RAW_CAPTURE_RATE_HZ: u32 = 48_000;

const FT8_SLOT_NS: i64 = 15_000_000_000;
/// 15 s at 48 kHz.
const FRAMES_PER_SLOT: usize = 720_000;
/// Wall-clock versus sample-position drift that abandons a slot.
const CONTINUITY_TOLERANCE_NS: i64 = 250_000_000;
const WRITER_QUEUE_DEPTH: usize = 2;
/// hound's largest PCM header (WAVE_FORMAT_EXTENSIBLE); 2-channel 16-bit
/// files use 44 bytes, so the prediction can only overestimate.
const WAV_HEADER_MAX_BYTES: u64 = 68;
/// At most one queue-full warning per this many seconds.
const QUEUE_FULL_WARN_INTERVAL_SECS: u64 = 60;
const MAX_SLOTS_IN_PROGRESS: usize = 2;

const BYTES_PER_MB: f64 = 1_048_576.0;
const BYTES_PER_GB: f64 = 1_073_741_824.0;

// ---------------------------------------------------------------------------
// Operator-facing messages (each pinned by a unit test)
// ---------------------------------------------------------------------------

pub(super) const MSG_RAW_CAPTURE_RESUMED: &str = "Raw 48 kHz capture resumed: FT8 is active";

fn enabled_message(
    dir: &Path,
    minutes: u32,
    until: DateTime<Utc>,
    existing_bytes: u64,
    max_bytes: u64,
) -> String {
    format!(
        "Raw 48 kHz capture enabled: dir={}, length={} min (until {} UTC), existing={:.1} MB, cap={:.1} GB",
        dir.display(),
        minutes,
        until.format("%H:%M:%S"),
        existing_bytes as f64 / BYTES_PER_MB,
        max_bytes as f64 / BYTES_PER_GB,
    )
}

fn finished_message(minutes: u32, until: DateTime<Utc>) -> String {
    format!(
        "Raw 48 kHz capture finished: raw_capture_minutes = {} ended at {} UTC",
        minutes,
        until.format("%H:%M:%S"),
    )
}

fn summary_message(files: u64, bytes: u64, dir: &Path) -> String {
    format!(
        "Raw 48 kHz capture wrote {} files ({:.1} MB) to {}",
        files,
        bytes as f64 / BYTES_PER_MB,
        dir.display(),
    )
}

fn needs_recording_message(minutes: u32) -> String {
    format!(
        "Raw 48 kHz capture off -- audio.recording.raw_capture_minutes = {minutes} needs WAV \
         recording, which is off for this run"
    )
}

fn unsupported_rate_message(rate: u32) -> String {
    format!(
        "Raw 48 kHz capture off -- [audio] sample_rate is {rate} Hz and raw capture needs \
         {RAW_CAPTURE_RATE_HZ} Hz"
    )
}

fn dir_failure_message(dir: &Path, err: &dyn std::fmt::Display) -> String {
    format!(
        "Raw 48 kHz capture off: cannot use {}: {}",
        dir.display(),
        err
    )
}

fn paused_message(mode_label: &str) -> String {
    format!(
        "Raw 48 kHz capture paused: {mode_label} is active and raw capture records FT8 slots only"
    )
}

fn queue_full_message(slot_start: DateTime<Utc>) -> String {
    format!(
        "Raw 48 kHz capture dropped the {} UTC slot: the writer is still busy with earlier \
         slots (slow disk?). Decoding is unaffected.",
        slot_start.format("%H:%M:%S"),
    )
}

pub(super) fn cap_reached_message(dir: &Path, total_bytes: u64, max_bytes: u64) -> String {
    format!(
        "Raw 48 kHz capture stopped: {} holds {:.1} MB and the next file would pass the {:.1} GB \
         cap. Decode-window recording continues.",
        dir.display(),
        total_bytes as f64 / BYTES_PER_MB,
        max_bytes as f64 / BYTES_PER_GB,
    )
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

/// Whether this DSP start runs a raw capture, decided from the key, the
/// PAN-125 recording gate and the capture rate, in that order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RawCaptureDecision {
    /// The key is 0: no log line.
    Off,
    /// The key is set but decode-window recording is off for this run.
    NeedsRecording { minutes: u32 },
    /// The capture device does not run at 48 kHz.
    UnsupportedRate { minutes: u32, rate: u32 },
    /// Capture into `dir` (`<recording dir>/raw48`).
    Enabled { dir: PathBuf, minutes: u32 },
}

pub(super) fn resolve_raw_capture(
    minutes: u32,
    recording: &super::dsp::WavRecording,
    input_rate: u32,
) -> RawCaptureDecision {
    if minutes == 0 {
        return RawCaptureDecision::Off;
    }
    let Some(dir) = recording.dir() else {
        return RawCaptureDecision::NeedsRecording { minutes };
    };
    if input_rate != RAW_CAPTURE_RATE_HZ {
        return RawCaptureDecision::UnsupportedRate {
            minutes,
            rate: input_rate,
        };
    }
    RawCaptureDecision::Enabled {
        dir: dir.join(RAW_CAPTURE_SUBDIR),
        minutes,
    }
}

impl RawCaptureDecision {
    /// The startup line for a refused capture. `None` for `Off` (silent)
    /// and `Enabled` ([`RawCapture::open`] logs that line).
    pub(super) fn startup_message(&self) -> Option<String> {
        match self {
            Self::Off | Self::Enabled { .. } => None,
            Self::NeedsRecording { minutes } => Some(needs_recording_message(*minutes)),
            Self::UnsupportedRate { rate, .. } => Some(unsupported_rate_message(*rate)),
        }
    }

    /// `NeedsRecording` at `info!`, `UnsupportedRate` at `warn!`.
    pub(super) fn log_startup(&self) {
        if let Some(message) = self.startup_message() {
            if matches!(self, Self::UnsupportedRate { .. }) {
                warn!("{message}");
            } else {
                info!("{message}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Capture window
// ---------------------------------------------------------------------------

/// The wall-clock span slots are captured in: a slot whose span starts at
/// or after `from` and whose boundary is before `until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CaptureWindow {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

/// The capture window, fixed by the first DSP start that ran a capture.
/// `start` lives on `DecodePipelineHandles`, which every DSP supervisor
/// restart reuses, so a restart neither extends nor restarts the capture.
pub(super) fn capture_window(
    start: &OnceLock<DateTime<Utc>>,
    minutes: u32,
    now: DateTime<Utc>,
) -> CaptureWindow {
    let from = *start.get_or_init(|| now);
    CaptureWindow {
        from,
        until: from + chrono::Duration::minutes(i64::from(minutes)),
    }
}

// ---------------------------------------------------------------------------
// TX marker
// ---------------------------------------------------------------------------

/// The coordinator's `last_ptt_on_ms` / `last_ptt_off_ms` / `ptt_active`
/// atomics, read together. `last_on_ms == 0` means never keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct PttSnapshot {
    pub last_on_ms: u64,
    pub last_off_ms: u64,
    /// `ptt_active`: the TX worker holds the rig keyed right now.
    pub keyed: bool,
}

/// How long a key-down with no later unkey stamp counts as TX once
/// `ptt_active` is clear (a stamped key-down refused before PTT-on). One
/// FT8 slot, the cap `desense.rs` uses for the same case.
const UNCLOSED_KEY_DOWN_MS: u64 = 15_000;

/// Whether the last key-down interval overlaps `[span_start_ms, span_end_ms)`.
/// The interval is `[on, off)` once unkeyed (`off >= on`), `[on, never)`
/// while keyed (`off < on` and `keyed`), and `[on, on + 15 s)` for a
/// key-down that never got an unkey stamp but is no longer keyed. No pre-roll.
pub(super) fn tx_overlaps(ptt: PttSnapshot, span_start_ms: u64, span_end_ms: u64) -> bool {
    if ptt.last_on_ms == 0 {
        return false;
    }
    let tx_end = if ptt.last_off_ms >= ptt.last_on_ms {
        ptt.last_off_ms
    } else if ptt.keyed {
        u64::MAX
    } else {
        ptt.last_on_ms.saturating_add(UNCLOSED_KEY_DOWN_MS)
    };
    ptt.last_on_ms < span_end_ms && tx_end > span_start_ms
}

// ---------------------------------------------------------------------------
// File name
// ---------------------------------------------------------------------------

/// `raw48_<UTC date>_<UTC slot boundary>[_<band>][_tx].wav`. An unknown or
/// out-of-band dial drops the band, as the 12 kHz files do.
pub(super) fn raw_slot_file_name(slot_start: DateTime<Utc>, dial_hz: u64, tx: bool) -> String {
    let mut name = format!("raw48_{}", slot_start.format("%Y%m%d_%H%M%S"));
    if let Some(band) = super::dsp::band_for_freq_hz(dial_hz) {
        name.push('_');
        name.push_str(band);
    }
    if tx {
        name.push_str("_tx");
    }
    name.push_str(".wav");
    name
}

// ---------------------------------------------------------------------------
// Slot cutter (pure)
// ---------------------------------------------------------------------------

/// What the DSP knows about one batch when it hands it over.
#[derive(Debug, Clone, Copy)]
pub(super) struct BatchContext {
    /// `Utc::now()` when the DSP handles the batch.
    pub end: DateTime<Utc>,
    /// The DSP's `last_known_dial_hz` after its dial guard (0 = unknown).
    pub dial_hz: u64,
    /// The DSP's cached mode is FT8.
    pub ft8_active: bool,
    /// The cached mode's display name, for the pause line.
    pub mode_label: &'static str,
    /// The dial or mode guard flushed on this batch.
    pub flushed: bool,
    pub ptt: PttSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbandonReason {
    /// The DSP's dial or mode guard flushed its own buffer.
    Flush,
    /// The receive-gain epoch changed (PAN-209 gain capture).
    GainChange,
    /// The station left FT8.
    NotFt8,
    /// Audio missing, a stall, a device reopen or a clock jump.
    Discontinuity,
    /// The writer stopped at the `raw48/` cap.
    CapReached,
}

/// One full slot, ready to write.
#[derive(Debug)]
pub(super) struct CompletedSlot {
    pub slot_start: DateTime<Utc>,
    /// Interleaved, `FRAMES_PER_SLOT * channels`.
    pub samples: Vec<f32>,
    pub channels: u16,
    pub dial_hz: u64,
    pub tx: bool,
}

#[derive(Debug, Default)]
pub(super) struct PushOutcome {
    pub completed: Vec<CompletedSlot>,
    pub abandoned: Vec<(DateTime<Utc>, AbandonReason)>,
}

struct SlotInProgress {
    slot_ns: i64,
    span_ns: i64,
    buf: Vec<f32>,
    frames: usize,
    tx: bool,
}

/// Cuts the interleaved 48 kHz capture stream into one slot per FT8
/// boundary. Each slot is anchored to the wall clock at its own start (no
/// drift accumulates), takes exactly [`FRAMES_PER_SLOT`] frames, and is
/// abandoned when its wall-clock and sample positions part by more than
/// [`CONTINUITY_TOLERANCE_NS`].
pub(super) struct RawSlotCutter {
    channels: usize,
    lead_ns: i64,
    from_ns: i64,
    until_ns: i64,
    last_batch_end_ns: Option<i64>,
    last_started_span_ns: i64,
    in_progress: Vec<SlotInProgress>,
}

fn ns(t: DateTime<Utc>) -> i64 {
    t.timestamp_nanos_opt().unwrap_or(i64::MAX)
}

fn from_ns(t: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(t)
}

fn frames_to_ns(frames: usize) -> i64 {
    (frames as i64) * 1_000_000_000 / i64::from(RAW_CAPTURE_RATE_HZ)
}

fn ns_to_ms(t: i64) -> u64 {
    (t.max(0) / 1_000_000) as u64
}

impl RawSlotCutter {
    pub(super) fn new(channels: u16, lead_secs: f64, window: CaptureWindow) -> Self {
        Self {
            channels: usize::from(channels.max(1)),
            lead_ns: (lead_secs * 1e9).round() as i64,
            from_ns: ns(window.from),
            until_ns: ns(window.until),
            last_batch_end_ns: None,
            last_started_span_ns: i64::MIN,
            in_progress: Vec::with_capacity(MAX_SLOTS_IN_PROGRESS),
        }
    }

    pub(super) fn in_progress(&self) -> usize {
        self.in_progress.len()
    }

    pub(super) fn abandon_all(
        &mut self,
        reason: AbandonReason,
    ) -> Vec<(DateTime<Utc>, AbandonReason)> {
        self.in_progress
            .drain(..)
            .map(|s| (from_ns(s.slot_ns), reason))
            .collect()
    }

    /// Feed one interleaved batch. `ctx.end` is when the DSP handled it.
    pub(super) fn push(&mut self, interleaved: &[f32], ctx: &BatchContext) -> PushOutcome {
        let mut out = PushOutcome::default();
        let ch = self.channels;
        let end_ns = ns(ctx.end);
        if !interleaved.len().is_multiple_of(ch) {
            // A ragged batch cannot be framed: nothing in progress survives.
            self.last_batch_end_ns = Some(end_ns);
            out.abandoned = self.abandon_all(AbandonReason::Discontinuity);
            return out;
        }
        let n = interleaved.len() / ch;
        let start_ns = end_ns - frames_to_ns(n);
        let prev_end_ns = self.last_batch_end_ns.replace(end_ns).unwrap_or(start_ns);
        if ctx.flushed {
            // The batch that exposed the dial or mode change is not kept.
            out.abandoned = self.abandon_all(AbandonReason::Flush);
            return out;
        }
        if !ctx.ft8_active {
            out.abandoned = self.abandon_all(AbandonReason::NotFt8);
            return out;
        }

        // Does a slot's span start fall in this batch's wall-clock range?
        // (slot_ns, frame offset) of a slot that starts in this batch.
        let mut new_slot: Option<(i64, usize)> = None;
        let k = (end_ns + self.lead_ns).div_euclid(FT8_SLOT_NS);
        let slot_ns = k * FT8_SLOT_NS;
        let span_ns = slot_ns - self.lead_ns;
        if span_ns > prev_end_ns
            && span_ns > self.last_started_span_ns
            && span_ns >= self.from_ns
            && slot_ns < self.until_ns
        {
            self.last_started_span_ns = span_ns;
            if start_ns - span_ns <= CONTINUITY_TOLERANCE_NS {
                let offset = ((span_ns - start_ns) as f64 * f64::from(RAW_CAPTURE_RATE_HZ) / 1e9)
                    .round()
                    .clamp(0.0, n as f64) as usize;
                if self.in_progress.len() >= MAX_SLOTS_IN_PROGRESS {
                    let oldest = self.in_progress.remove(0);
                    out.abandoned
                        .push((from_ns(oldest.slot_ns), AbandonReason::Discontinuity));
                }
                self.in_progress.push(SlotInProgress {
                    slot_ns,
                    span_ns,
                    buf: Vec::with_capacity(FRAMES_PER_SLOT * ch),
                    frames: 0,
                    tx: false,
                });
                new_slot = Some((slot_ns, offset));
            } else {
                // Audio is missing at the span start: skip this slot.
                out.abandoned
                    .push((from_ns(slot_ns), AbandonReason::Discontinuity));
            }
        }

        let mut i = 0;
        while i < self.in_progress.len() {
            let slot = &mut self.in_progress[i];
            let from = match new_slot {
                Some((ns_start, offset)) if ns_start == slot.slot_ns => offset,
                _ => 0,
            };
            let take = (n - from).min(FRAMES_PER_SLOT - slot.frames);
            slot.buf
                .extend_from_slice(&interleaved[from * ch..(from + take) * ch]);
            slot.frames += take;
            slot.tx |= tx_overlaps(
                ctx.ptt,
                ns_to_ms(slot.span_ns),
                ns_to_ms(slot.span_ns + FT8_SLOT_NS),
            );
            let wall = start_ns + frames_to_ns(from + take);
            let sample = slot.span_ns + frames_to_ns(slot.frames);
            if (wall - sample).abs() > CONTINUITY_TOLERANCE_NS {
                let gone = self.in_progress.remove(i);
                out.abandoned
                    .push((from_ns(gone.slot_ns), AbandonReason::Discontinuity));
                continue;
            }
            if slot.frames == FRAMES_PER_SLOT {
                let done = self.in_progress.remove(i);
                out.completed.push(CompletedSlot {
                    slot_start: from_ns(done.slot_ns),
                    samples: done.buf,
                    channels: ch as u16,
                    dial_hz: ctx.dial_hz,
                    tx: done.tx,
                });
                continue;
            }
            i += 1;
        }
        out
    }

    /// No slot is in progress and every slot the window allows has been
    /// started or skipped.
    pub(super) fn finished(&self, now: DateTime<Utc>) -> bool {
        self.in_progress.is_empty() && ns(now) + self.lead_ns >= self.until_ns
    }
}

// ---------------------------------------------------------------------------
// Writer (synchronous core)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WriteOutcome {
    Written { name: String, bytes: u64 },
    DeadAir,
    CapReached { total: u64 },
    Stopped,
    Failed(String),
}

/// Writes completed slots into `raw48/` and enforces its cap.
pub(super) struct RawSlotWriter {
    dir: PathBuf,
    total_bytes: u64,
    max_bytes: u64,
    files_written: u64,
    bytes_written: u64,
    stopped: bool,
}

impl RawSlotWriter {
    /// Create `dir`, remove stale top-level `*.wav.part` files a crash left,
    /// and measure the top-level `*.wav` bytes already there.
    pub(super) fn open(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let is_part = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".wav.part"));
            if is_part && path.is_file() {
                if let Err(e) = std::fs::remove_file(&path) {
                    debug!(
                        "Raw 48 kHz capture: cannot remove stale {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        // A partial scan still counts every readable WAV. A scan that cannot
        // list the folder at all fails the open: starting the cap from 0
        // could write past it.
        let (total_bytes, _complete) = super::dsp::WavRecorder::dir_size(&dir)?;
        Ok(Self {
            dir,
            total_bytes,
            max_bytes,
            files_written: 0,
            bytes_written: 0,
            stopped: false,
        })
    }

    pub(super) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub(super) fn write(&mut self, slot: &CompletedSlot) -> WriteOutcome {
        if self.stopped {
            return WriteOutcome::Stopped;
        }
        let name = raw_slot_file_name(slot.slot_start, slot.dial_hz, slot.tx);
        if super::dsp::is_dead_air(&slot.samples) {
            debug!(
                "Raw 48 kHz capture skipped {}: dead air (peak {:.6} < {})",
                name,
                super::dsp::window_peak(&slot.samples),
                super::dsp::DEAD_AIR_PEAK_THRESHOLD
            );
            return WriteOutcome::DeadAir;
        }
        let predicted = slot.samples.len() as u64 * 2 + WAV_HEADER_MAX_BYTES;
        if self.total_bytes.saturating_add(predicted) > self.max_bytes {
            self.stopped = true;
            warn!(
                "{}",
                cap_reached_message(&self.dir, self.total_bytes, self.max_bytes)
            );
            return WriteOutcome::CapReached {
                total: self.total_bytes,
            };
        }
        let final_path = self.dir.join(&name);
        let part_path = self.dir.join(format!("{name}.part"));
        match write_wav(&part_path, slot).and_then(|()| {
            std::fs::rename(&part_path, &final_path)?;
            Ok(std::fs::metadata(&final_path)?.len())
        }) {
            Ok(bytes) => {
                self.total_bytes += bytes;
                self.bytes_written += bytes;
                self.files_written += 1;
                debug!("Raw 48 kHz capture wrote {} ({} bytes)", name, bytes);
                WriteOutcome::Written { name, bytes }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part_path);
                warn!(
                    "Raw 48 kHz capture could not write {}: {}",
                    final_path.display(),
                    e
                );
                WriteOutcome::Failed(e.to_string())
            }
        }
    }
}

fn write_wav(path: &Path, slot: &CompletedSlot) -> std::io::Result<()> {
    let spec = hound::WavSpec {
        channels: slot.channels,
        sample_rate: RAW_CAPTURE_RATE_HZ,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let to_io = |e: hound::Error| match e {
        hound::Error::IoError(io) => io,
        other => std::io::Error::other(other.to_string()),
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(to_io)?;
    {
        let mut i16_writer = w.get_i16_writer(slot.samples.len() as u32);
        for &s in &slot.samples {
            i16_writer.write_sample((s * i16::MAX as f32) as i16);
        }
        i16_writer.flush().map_err(to_io)?;
    }
    w.finalize().map_err(to_io)
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmitOutcome {
    Queued,
    QueueFull,
    Closed,
}

/// One writer thread behind a bounded queue. `submit` never blocks; `Drop`
/// closes the queue and joins, so queued slots are written before the DSP
/// worker finishes and a restarted worker never races the previous writer.
pub(super) struct RawCaptureWriterHandle {
    tx: Option<SyncSender<CompletedSlot>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RawCaptureWriterHandle {
    pub(super) fn spawn(writer: RawSlotWriter) -> std::io::Result<Self> {
        Self::spawn_inner(writer, None)
    }

    #[cfg(test)]
    fn spawn_gated(writer: RawSlotWriter, gate: Receiver<()>) -> std::io::Result<Self> {
        Self::spawn_inner(writer, Some(gate))
    }

    fn spawn_inner(mut writer: RawSlotWriter, gate: Option<Receiver<()>>) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel::<CompletedSlot>(WRITER_QUEUE_DEPTH);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("raw48-writer".to_string())
            .spawn(move || {
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
                while let Ok(slot) = rx.recv() {
                    writer.write(&slot);
                    if writer.stopped {
                        thread_stopped.store(true, Ordering::Release);
                    }
                }
                info!(
                    "{}",
                    summary_message(writer.files_written, writer.bytes_written, &writer.dir)
                );
            })?;
        Ok(Self {
            tx: Some(tx),
            stopped,
            thread: Some(thread),
        })
    }

    pub(super) fn submit(&self, slot: CompletedSlot) -> SubmitOutcome {
        match &self.tx {
            None => SubmitOutcome::Closed,
            Some(tx) => match tx.try_send(slot) {
                Ok(()) => SubmitOutcome::Queued,
                Err(TrySendError::Full(_)) => SubmitOutcome::QueueFull,
                Err(TrySendError::Disconnected(_)) => SubmitOutcome::Closed,
            },
        }
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

impl Drop for RawCaptureWriterHandle {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                warn!("Raw 48 kHz capture writer thread panicked");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// What the DSP worker holds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RawCaptureState {
    Running,
    Done,
}

/// Cutter + writer + the log-once state. The DSP drops it on `Done`.
pub(super) struct RawCapture {
    cutter: RawSlotCutter,
    writer: RawCaptureWriterHandle,
    minutes: u32,
    until: DateTime<Utc>,
    paused: bool,
    last_queue_full_warn: Option<std::time::Instant>,
    /// Slots handed to the writer queue.
    submitted: u64,
}

impl RawCapture {
    /// Open the writer and log the enabled line, or log why not and return
    /// `None` (window already over at `now`, or `dir` unusable).
    pub(super) fn open(
        dir: PathBuf,
        minutes: u32,
        window: CaptureWindow,
        channels: u16,
        lead_secs: f64,
        max_bytes: u64,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        let cutter = RawSlotCutter::new(channels, lead_secs, window);
        if cutter.finished(now) {
            info!("{}", finished_message(minutes, window.until));
            return None;
        }
        let writer = match RawSlotWriter::open(dir.clone(), max_bytes) {
            Ok(w) => w,
            Err(e) => {
                warn!("{}", dir_failure_message(&dir, &e));
                return None;
            }
        };
        let existing = writer.total_bytes();
        let writer = match RawCaptureWriterHandle::spawn(writer) {
            Ok(h) => h,
            Err(e) => {
                warn!("{}", dir_failure_message(&dir, &e));
                return None;
            }
        };
        info!(
            "{}",
            enabled_message(&dir, minutes, window.until, existing, max_bytes)
        );
        Some(Self {
            cutter,
            writer,
            minutes,
            until: window.until,
            paused: false,
            last_queue_full_warn: None,
            submitted: 0,
        })
    }

    pub(super) fn on_batch(&mut self, interleaved: &[f32], ctx: &BatchContext) -> RawCaptureState {
        if ctx.ft8_active {
            if self.paused {
                self.paused = false;
                info!("{MSG_RAW_CAPTURE_RESUMED}");
            }
        } else if !self.paused {
            self.paused = true;
            info!("{}", paused_message(ctx.mode_label));
        }
        if self.writer.stopped() {
            // The writer logged the cap line; slots in progress would never
            // be written, so drop them and end the capture now.
            self.abandon(AbandonReason::CapReached);
            debug!(
                "Raw 48 kHz capture: {} slots queued before the cap",
                self.submitted
            );
            return RawCaptureState::Done;
        }
        let out = self.cutter.push(interleaved, ctx);
        for (slot_start, reason) in out.abandoned {
            debug!(
                "Raw 48 kHz capture abandoned the {} UTC slot: {:?}",
                slot_start.format("%H:%M:%S"),
                reason
            );
        }
        for slot in out.completed {
            let slot_start = slot.slot_start;
            let submit = self.writer.submit(slot);
            if submit == SubmitOutcome::Queued {
                self.submitted += 1;
            }
            if submit == SubmitOutcome::QueueFull {
                let due = self.last_queue_full_warn.is_none_or(|t| {
                    t.elapsed() >= std::time::Duration::from_secs(QUEUE_FULL_WARN_INTERVAL_SECS)
                });
                if due {
                    self.last_queue_full_warn = Some(std::time::Instant::now());
                    warn!("{}", queue_full_message(slot_start));
                } else {
                    debug!("{}", queue_full_message(slot_start));
                }
            }
        }
        if self.cutter.finished(ctx.end) {
            info!("{}", finished_message(self.minutes, self.until));
            debug!("Raw 48 kHz capture: {} slots queued", self.submitted);
            return RawCaptureState::Done;
        }
        RawCaptureState::Running
    }

    /// Close the capture without blocking the caller: the writer thread's
    /// join (it finishes up to two queued slots first) runs on a short-lived
    /// thread, so the real-time DSP loop never waits on the disk. If no
    /// thread can be spawned, the capture is closed inline instead.
    pub(super) fn close_in_background(self) {
        let spawned = std::thread::Builder::new()
            .name("raw48-close".to_string())
            .spawn(move || drop(self));
        if let Err(e) = spawned {
            warn!("Raw 48 kHz capture: closing inline ({e})");
        }
    }

    pub(super) fn abandon(&mut self, reason: AbandonReason) {
        for (slot_start, reason) in self.cutter.abandon_all(reason) {
            debug!(
                "Raw 48 kHz capture abandoned the {} UTC slot: {:?}",
                slot_start.format("%H:%M:%S"),
                reason
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test_stream {
    //! Synthetic capture stream shared by the cutter, writer and glue tests.

    use super::*;

    pub(super) const SEC: i64 = 1_000_000_000;
    pub(super) const MS: i64 = 1_000_000;

    pub(super) fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    pub(super) fn window(from: DateTime<Utc>, minutes: i64) -> CaptureWindow {
        CaptureWindow {
            from,
            until: from + chrono::Duration::minutes(minutes),
        }
    }

    /// One batch as the test harness builds it, before delivery.
    pub(super) struct Batch {
        pub idx: u64,
        /// Wall time the batch's last frame was really captured.
        pub true_end_ns: i64,
        /// What the DSP reads from the clock (tests may shift it).
        pub end_ns: i64,
        pub dial_hz: u64,
        pub ft8_active: bool,
        pub flushed: bool,
        pub ptt: PttSnapshot,
        /// `false` drops the batch (a hole in the audio).
        pub deliver: bool,
    }

    pub(super) struct Feed {
        pub channels: u16,
        pub frames: usize,
        /// Frames per second of wall time the device produces.
        pub device_rate: f64,
        pub start: DateTime<Utc>,
    }

    #[derive(Default)]
    pub(super) struct Run {
        pub completed: Vec<CompletedSlot>,
        /// Wall-clock error of each completed slot's first frame (stereo
        /// feeds only; the frame index is encoded in the samples).
        pub start_errors_ns: Vec<i64>,
        pub abandoned: Vec<(DateTime<Utc>, AbandonReason)>,
        pub max_in_progress: usize,
    }

    /// Stereo feeds encode the frame index exactly: left = (idx % 4096) /
    /// 8192, right = idx / 4096. Mono feeds carry a 1 kHz tone at 0.1.
    pub(super) fn samples_for(channels: u16, first_frame: u64, frames: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * usize::from(channels));
        for f in 0..frames as u64 {
            let idx = first_frame + f;
            if channels >= 2 {
                v.push((idx % 4096) as f32 / 8192.0 + 0.01);
                v.push((idx / 4096) as f32);
                v.extend(std::iter::repeat_n(0.0, usize::from(channels) - 2));
            } else {
                let phase = idx as f64 * 1000.0 / 48_000.0;
                v.push((0.1 * (2.0 * std::f64::consts::PI * phase).sin()) as f32);
            }
        }
        v
    }

    pub(super) fn decode_index(samples: &[f32]) -> u64 {
        let lo = ((samples[0] - 0.01) * 8192.0).round() as u64;
        let hi = samples[1] as u64;
        hi * 4096 + lo
    }

    /// Drive `cutter` for `duration_s` of wall time. `adjust` may change
    /// what the DSP sees for each batch.
    pub(super) fn run(
        cutter: &mut RawSlotCutter,
        feed: &Feed,
        duration_s: f64,
        mut adjust: impl FnMut(&mut Batch),
    ) -> Run {
        let mut run = Run::default();
        let start_ns = ns(feed.start);
        let batch_ns = feed.frames as f64 / feed.device_rate * 1e9;
        let total = (duration_s * 1e9 / batch_ns).ceil() as u64;
        for idx in 0..total {
            let first_frame = idx * feed.frames as u64;
            let true_end_ns = start_ns + ((idx + 1) as f64 * batch_ns).round() as i64;
            let mut b = Batch {
                idx,
                true_end_ns,
                end_ns: true_end_ns,
                dial_hz: 14_074_000,
                ft8_active: true,
                flushed: false,
                ptt: PttSnapshot::default(),
                deliver: true,
            };
            adjust(&mut b);
            if !b.deliver {
                continue;
            }
            let ctx = BatchContext {
                end: from_ns(b.end_ns),
                dial_hz: b.dial_hz,
                ft8_active: b.ft8_active,
                mode_label: if b.ft8_active { "FT8" } else { "FT4" },
                flushed: b.flushed,
                ptt: b.ptt,
            };
            let samples = samples_for(feed.channels, first_frame, feed.frames);
            let out = cutter.push(&samples, &ctx);
            run.max_in_progress = run.max_in_progress.max(cutter.in_progress());
            for slot in out.completed {
                if feed.channels >= 2 {
                    // Frame `idx` was captured `idx / device_rate` after the stream start.
                    let idx = decode_index(&slot.samples);
                    let true_ns = start_ns + (idx as f64 / feed.device_rate * 1e9).round() as i64;
                    let span_ns = ns(slot.slot_start) - cutter.lead_ns;
                    run.start_errors_ns.push(true_ns - span_ns);
                }
                run.completed.push(slot);
            }
            run.abandoned.extend(out.abandoned);
        }
        run
    }

    pub(super) fn ctx_at(end: DateTime<Utc>) -> BatchContext {
        BatchContext {
            end,
            dial_hz: 14_074_000,
            ft8_active: true,
            mode_label: "FT8",
            flushed: false,
            ptt: PttSnapshot::default(),
        }
    }

    /// TX schedule: key-down 80 ms before the boundary of every slot `k`
    /// for which `keyed(k)`, off 13.3 s after it. The snapshot at `at_ns`.
    pub(super) fn ptt_at(at_ns: i64, keyed: impl Fn(i64) -> bool) -> PttSnapshot {
        let mut snap = PttSnapshot::default();
        let k_now = (at_ns + 80 * MS).div_euclid(15 * SEC);
        for k in (k_now - 3)..=k_now {
            if !keyed(k) {
                continue;
            }
            let on = k * 15 * SEC - 80 * MS;
            let off = k * 15 * SEC + 13_300 * MS;
            if on <= at_ns {
                snap.last_on_ms = (on / MS) as u64;
            }
            if off <= at_ns {
                snap.last_off_ms = (off / MS) as u64;
            }
        }
        snap.keyed = snap.last_on_ms > snap.last_off_ms;
        snap
    }
}

#[cfg(test)]
mod cutter_tests {
    use super::test_stream::*;
    use super::*;
    use chrono::Timelike;

    fn realtime_stereo(start: DateTime<Utc>) -> Feed {
        Feed {
            channels: 2,
            frames: 512,
            device_rate: 48_000.0,
            start,
        }
    }

    fn slot_times(run: &Run) -> Vec<DateTime<Utc>> {
        run.completed.iter().map(|s| s.slot_start).collect()
    }

    #[test]
    fn slots_are_720000_frames_on_the_ft8_grid() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 3));
        let run = run(&mut cutter, &realtime_stereo(start), 200.0, |_| {});
        assert_eq!(run.completed.len(), 12);
        assert_eq!(run.completed[0].slot_start, t("2026-11-10T12:00:15Z"));
        assert_eq!(run.completed[11].slot_start, t("2026-11-10T12:03:00Z"));
        for slot in &run.completed {
            assert_eq!(slot.samples.len(), 1_440_000);
            assert_eq!(slot.channels, 2);
            assert!([0, 15, 30, 45].contains(&slot.slot_start.second()));
            assert_eq!(slot.slot_start.nanosecond(), 0);
        }
    }

    #[test]
    fn first_partial_slot_is_skipped() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 1));
        let feed = realtime_stereo(t("2026-11-10T11:59:59Z"));
        let run_a = run(&mut cutter, &feed, 40.0, |_| {});
        assert_eq!(run_a.completed[0].slot_start, t("2026-11-10T12:00:15Z"));

        let start = t("2026-11-10T12:00:14.8Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 1));
        let feed = realtime_stereo(t("2026-11-10T11:59:59Z"));
        let run_b = run(&mut cutter, &feed, 50.0, |_| {});
        assert_eq!(run_b.completed[0].slot_start, t("2026-11-10T12:00:30Z"));
    }

    #[test]
    fn capture_stops_after_its_length() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 30));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let run = run(&mut cutter, &feed, 30.0 * 60.0 + 10.0, |_| {});
        assert_eq!(run.completed.len(), 120);
        assert_eq!(run.completed[0].slot_start, t("2026-11-10T12:00:15Z"));
        assert_eq!(run.completed[119].slot_start, t("2026-11-10T12:30:00Z"));
        // The last slot's span ended at 12:30:14.5; nothing is left to start.
        assert_eq!(cutter.in_progress(), 0);
        assert!(cutter.finished(t("2026-11-10T12:30:15Z")));
    }

    #[test]
    fn finished_is_false_while_the_last_slot_is_in_progress() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 1));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        // Stop mid-way through the 12:01:00 slot (span 12:00:59.5-12:01:14.5).
        let run = run(&mut cutter, &feed, 60.0, |_| {});
        assert_eq!(cutter.in_progress(), 1);
        assert!(!cutter.finished(t("2026-11-10T12:01:07.123Z")));
        assert_eq!(
            run.completed.last().unwrap().slot_start,
            t("2026-11-10T12:00:45Z")
        );
    }

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        ((*seed >> 11) as f64) / ((1u64 << 53) as f64)
    }

    #[test]
    fn slot_start_stays_within_one_batch_under_jitter_and_drift() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 30));
        let feed = Feed {
            channels: 2,
            frames: 512,
            device_rate: 48_000.0 * (1.0 + 80e-6),
            start,
        };
        let mut seed = 7u64;
        let run = run(&mut cutter, &feed, 30.0 * 60.0 + 10.0, |b| {
            b.end_ns += ((lcg(&mut seed) * 2.0 - 1.0) * 3.0 * MS as f64) as i64;
        });
        assert_eq!(run.completed.len(), 120);
        assert!(run.abandoned.is_empty(), "{:?}", run.abandoned);
        let max_err = run.start_errors_ns.iter().map(|e| e.abs()).max().unwrap();
        assert!(max_err <= 11 * MS, "max start error {} ns", max_err);
    }

    #[test]
    fn late_dsp_reads_keep_every_slot_and_bound_the_start_error() {
        // A 20 ms DSP stall every 200 batches: the stalled batch and the
        // queued ones behind it are read late, back to back, until caught up.
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 30));
        let feed = Feed {
            channels: 2,
            frames: 512,
            device_rate: 48_000.0 * (1.0 + 80e-6),
            start,
        };
        let mut seed = 11u64;
        let mut stall_until = i64::MIN;
        let mut last_read = i64::MIN;
        let run = run(&mut cutter, &feed, 30.0 * 60.0 + 10.0, |b| {
            if b.idx % 200 == 199 {
                stall_until = b.true_end_ns + 20 * MS;
            }
            let jitter = ((lcg(&mut seed) * 2.0 - 1.0) * 3.0 * MS as f64) as i64;
            let read = (b.true_end_ns + jitter)
                .max(stall_until)
                .max(last_read + 100_000);
            last_read = read;
            b.end_ns = read;
        });
        assert_eq!(run.completed.len(), 120);
        assert!(run.abandoned.is_empty(), "{:?}", run.abandoned);
        let max_err = run.start_errors_ns.iter().map(|e| e.abs()).max().unwrap();
        assert!(max_err <= 21 * MS, "max start error {} ns", max_err);
        let within_one_batch = run
            .start_errors_ns
            .iter()
            .filter(|e| e.abs() <= 11 * MS)
            .count();
        assert!(
            within_one_batch >= 110,
            "{within_one_batch} of 120 within one batch"
        );
    }

    #[test]
    fn stub_pacing_keeps_every_slot() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 30));
        let feed = Feed {
            channels: 1,
            frames: 4096,
            device_rate: 4096.0 / 0.085,
            start,
        };
        let run = run(&mut cutter, &feed, 30.0 * 60.0 + 10.0, |_| {});
        assert_eq!(run.completed.len(), 120);
    }

    #[test]
    fn half_rate_stream_is_rejected() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 5));
        let feed = Feed {
            channels: 1,
            frames: 256,
            device_rate: 25_600.0,
            start,
        };
        let run = run(&mut cutter, &feed, 5.0 * 60.0 + 10.0, |_| {});
        assert_eq!(run.completed.len(), 0);
        assert!(!run.abandoned.is_empty());
    }

    fn run_with_hole(hole_ms: i64) -> Run {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 30));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let hole_from = ns(start) + 100 * SEC;
        run(&mut cutter, &feed, 30.0 * 60.0 + 10.0, |b| {
            if b.true_end_ns > hole_from && b.true_end_ns <= hole_from + hole_ms * MS {
                b.deliver = false;
            }
        })
    }

    #[test]
    fn hole_over_250_ms_abandons_only_that_slot() {
        let run = run_with_hole(320);
        assert_eq!(run.completed.len(), 119);
        assert_eq!(run.abandoned.len(), 1);
        assert_eq!(run.abandoned[0].1, AbandonReason::Discontinuity);

        let run = run_with_hole(200);
        assert_eq!(
            run.completed.len(),
            120,
            "a hole under 250 ms is not detected"
        );
    }

    #[test]
    fn flush_abandons_the_slot_and_keeps_the_next() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 1));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let flush_at = ns(t("2026-11-10T12:00:20Z"));
        let mut flushed_once = false;
        let run = run(&mut cutter, &feed, 70.0, |b| {
            if !flushed_once && b.true_end_ns >= flush_at {
                b.flushed = true;
                flushed_once = true;
            }
        });
        assert_eq!(
            run.abandoned,
            vec![(t("2026-11-10T12:00:15Z"), AbandonReason::Flush)]
        );
        assert_eq!(run.completed[0].slot_start, t("2026-11-10T12:00:30Z"));
    }

    #[test]
    fn gain_change_abandons_in_progress_slots() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 1));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let first = run(&mut cutter, &feed, 12.0, |_| {});
        assert!(first.completed.is_empty());
        assert_eq!(cutter.in_progress(), 1);
        let gone = cutter.abandon_all(AbandonReason::GainChange);
        assert_eq!(
            gone,
            vec![(t("2026-11-10T12:00:15Z"), AbandonReason::GainChange)]
        );
        let feed = Feed {
            start: start + chrono::Duration::milliseconds(12_000),
            ..feed
        };
        let rest = run(&mut cutter, &feed, 26.0, |_| {});
        assert_eq!(rest.completed.len(), 1);
        assert_eq!(rest.completed[0].slot_start, t("2026-11-10T12:00:30Z"));
    }

    #[test]
    fn not_ft8_pauses_capture() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 3));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let off_from = ns(t("2026-11-10T12:00:20Z"));
        let off_until = off_from + 40 * SEC; // FT8 again from 12:01:00
        let run = run(&mut cutter, &feed, 120.0, |b| {
            if b.true_end_ns >= off_from && b.true_end_ns < off_until {
                b.ft8_active = false;
            }
        });
        assert_eq!(
            run.abandoned,
            vec![(t("2026-11-10T12:00:15Z"), AbandonReason::NotFt8)]
        );
        // 12:01:00's span started at 12:00:59.5, still FT4: the next is 12:01:15.
        assert_eq!(run.completed[0].slot_start, t("2026-11-10T12:01:15Z"));
    }

    #[test]
    fn clock_jumps_abandon_and_never_repeat_a_slot() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 30));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let jump_fwd = ns(t("2026-11-10T12:00:20Z"));
        let jump_back = ns(t("2026-11-10T12:05:20Z"));
        let run = run(&mut cutter, &feed, 20.0 * 60.0, |b| {
            if b.true_end_ns >= jump_fwd {
                b.end_ns += 2 * SEC;
            }
            if b.true_end_ns >= jump_back {
                b.end_ns -= 600 * SEC;
            }
        });
        assert!(run
            .abandoned
            .iter()
            .any(|(s, r)| *s == t("2026-11-10T12:00:15Z") && *r == AbandonReason::Discontinuity));
        let times = slot_times(&run);
        for pair in times.windows(2) {
            assert!(pair[1] > pair[0], "slot repeated or went back: {:?}", pair);
        }
        assert!(!times.is_empty());
    }

    #[test]
    fn at_most_two_slots_in_progress() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 10));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0 * 0.995,
            start,
        };
        let run = run(&mut cutter, &feed, 10.0 * 60.0 + 10.0, |_| {});
        assert_eq!(run.max_in_progress, 2);
        assert!(!run.completed.is_empty());
    }

    #[test]
    fn band_comes_from_the_dial_at_completion() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 1));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        let known_from = ns(t("2026-11-10T12:00:16.5Z"));
        let run = run(&mut cutter, &feed, 30.0, |b| {
            b.dial_hz = if b.true_end_ns < known_from {
                0
            } else {
                14_074_000
            };
        });
        assert_eq!(run.completed[0].slot_start, t("2026-11-10T12:00:15Z"));
        assert_eq!(run.completed[0].dial_hz, 14_074_000);
    }

    #[test]
    fn ragged_batch_abandons_everything() {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(2, 0.5, window(start, 1));
        let _ = run(&mut cutter, &realtime_stereo(start), 10.0, |_| {});
        assert_eq!(cutter.in_progress(), 1);
        let out = cutter.push(&[0.1, 0.2, 0.3], &ctx_at(t("2026-11-10T12:00:17.2Z")));
        assert_eq!(out.abandoned.len(), 1);
        assert_eq!(cutter.in_progress(), 0);
    }
}

#[cfg(test)]
mod tx_marker_tests {
    use super::test_stream::*;
    use super::*;

    /// A consistent snapshot: keyed exactly when the last stamp is a key-down.
    fn snap(on: u64, off: u64) -> PttSnapshot {
        PttSnapshot {
            last_on_ms: on,
            last_off_ms: off,
            keyed: on > off,
        }
    }

    #[test]
    fn a_refused_key_down_marks_only_its_own_slot() {
        // Stamped key-down, refused before PTT-on, no unkey stamp: `ptt_active`
        // is clear, so it counts for one slot, not forever.
        let refused = PttSnapshot {
            last_on_ms: 100_420,
            last_off_ms: 98_800,
            keyed: false,
        };
        assert!(tx_overlaps(refused, 100_000, 115_000));
        assert!(!tx_overlaps(refused, 130_000, 145_000));
        // Still keyed: open-ended.
        let keyed = PttSnapshot {
            keyed: true,
            ..refused
        };
        assert!(tx_overlaps(keyed, 130_000, 145_000));
    }

    #[test]
    fn tx_overlaps_cases() {
        let (a, b) = (100_000, 115_000);
        assert!(!tx_overlaps(snap(0, 0), a, b), "never keyed");
        assert!(tx_overlaps(snap(100_420, 113_800), a, b), "this slot's TX");
        assert!(
            !tx_overlaps(snap(85_420, 98_800), a, b),
            "previous slot's TX"
        );
        assert!(
            !tx_overlaps(snap(115_420, 113_800), a, b),
            "next slot's key-down, still keyed"
        );
        assert!(
            tx_overlaps(snap(99_900, 100_100), a, b),
            "straddles the span start"
        );
        assert!(
            tx_overlaps(snap(114_999, 113_800), a, b),
            "keyed since 114,999"
        );
        assert!(!tx_overlaps(snap(115_000, 113_800), a, b), "half-open span");
    }

    fn run_tx(keyed: fn(i64) -> bool, snapshot_delay_ms: i64) -> Run {
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cutter = RawSlotCutter::new(1, 0.5, window(start, 10));
        let feed = Feed {
            channels: 1,
            frames: 512,
            device_rate: 48_000.0,
            start,
        };
        run(&mut cutter, &feed, 10.0 * 60.0 + 10.0, |b| {
            b.ptt = ptt_at(b.true_end_ns + snapshot_delay_ms * MS, keyed);
        })
    }

    fn slot_index(s: &CompletedSlot) -> i64 {
        s.slot_start.timestamp().div_euclid(15)
    }

    #[test]
    fn tx_in_back_to_back_slots_is_marked_with_a_late_snapshot() {
        let run = run_tx(|_| true, 600);
        assert_eq!(run.completed.len(), 40);
        assert!(run.completed.iter().all(|s| s.tx));
    }

    #[test]
    fn tx_every_other_slot_marks_exactly_those_slots() {
        let run = run_tx(|k| k % 2 == 0, 0);
        assert_eq!(run.completed.len(), 40);
        for slot in &run.completed {
            assert_eq!(slot.tx, slot_index(slot) % 2 == 0, "{}", slot.slot_start);
        }
    }
}

#[cfg(test)]
mod name_tests {
    use super::test_stream::t;
    use super::*;

    #[test]
    fn names_carry_slot_time_band_and_tx() {
        let s = t("2026-11-10T12:00:00Z");
        assert_eq!(
            raw_slot_file_name(s, 14_074_000, false),
            "raw48_20261110_120000_20m.wav"
        );
        assert_eq!(
            raw_slot_file_name(s, 14_074_000, true),
            "raw48_20261110_120000_20m_tx.wav"
        );
        assert_eq!(raw_slot_file_name(s, 0, false), "raw48_20261110_120000.wav");
        assert_eq!(
            raw_slot_file_name(s, 0, true),
            "raw48_20261110_120000_tx.wav"
        );
    }

    #[test]
    fn names_stay_out_of_ft8_globs() {
        let s = t("2026-11-10T12:00:00Z");
        assert!(!raw_slot_file_name(s, 14_074_000, true).starts_with("ft8_"));
    }
}

#[cfg(test)]
mod window_tests {
    use super::test_stream::t;
    use super::*;

    #[test]
    fn capture_window_is_fixed_by_the_first_start() {
        let lock = OnceLock::new();
        let first = t("2026-11-10T12:00:07Z");
        let a = capture_window(&lock, 30, first);
        let b = capture_window(&lock, 30, first + chrono::Duration::minutes(10));
        assert_eq!(a, b);
        assert_eq!(a.from, first);
        assert_eq!(a.until, first + chrono::Duration::minutes(30));
    }
}

#[cfg(test)]
mod writer_tests {
    use super::test_stream::*;
    use super::*;

    const STEREO_FILE: u64 = 2_880_044;

    pub(super) fn slot(start: &str, channels: u16, dial_hz: u64, tx: bool) -> CompletedSlot {
        let n = FRAMES_PER_SLOT * usize::from(channels);
        let samples = (0..n)
            .map(|i| 0.1 * ((i as f32) * 0.013).sin())
            .collect::<Vec<f32>>();
        CompletedSlot {
            slot_start: t(start),
            samples,
            channels,
            dial_hz,
            tx,
        }
    }

    fn wavs(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn writes_a_48k_file_of_the_exact_size() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let mut w = RawSlotWriter::open(dir.clone(), RAW_CAPTURE_MAX_BYTES).unwrap();
        let out = w.write(&slot("2026-11-10T12:00:00Z", 2, 14_074_000, false));
        assert_eq!(
            out,
            WriteOutcome::Written {
                name: "raw48_20261110_120000_20m.wav".into(),
                bytes: STEREO_FILE
            }
        );
        let path = dir.join("raw48_20261110_120000_20m.wav");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), STEREO_FILE);
        let r = hound::WavReader::open(&path).unwrap();
        let spec = r.spec();
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        assert_eq!(r.duration(), 720_000);

        let out = w.write(&slot("2026-11-10T12:00:15Z", 1, 0, false));
        assert!(matches!(
            out,
            WriteOutcome::Written {
                bytes: 1_440_044,
                ..
            }
        ));
    }

    #[test]
    fn samples_round_trip_like_the_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = RawSlotWriter::open(tmp.path().to_path_buf(), RAW_CAPTURE_MAX_BYTES).unwrap();
        let s = slot("2026-11-10T12:00:00Z", 2, 0, false);
        w.write(&s);
        let mut r = hound::WavReader::open(tmp.path().join("raw48_20261110_120000.wav")).unwrap();
        let back: Vec<i16> = r.samples::<i16>().take(100).map(|x| x.unwrap()).collect();
        let want: Vec<i16> = s.samples[..100]
            .iter()
            .map(|&x| (x * i16::MAX as f32) as i16)
            .collect();
        assert_eq!(back, want);
    }

    #[test]
    fn no_part_file_is_left_and_stale_parts_are_removed_at_open() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join("raw48_x.wav.part"), b"stale").unwrap();
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        let mut w = RawSlotWriter::open(dir.clone(), RAW_CAPTURE_MAX_BYTES).unwrap();
        assert!(!dir.join("raw48_x.wav.part").exists());
        assert!(dir.join("notes.txt").exists());
        w.write(&slot("2026-11-10T12:00:00Z", 1, 0, true));
        assert_eq!(
            wavs(&dir),
            vec!["notes.txt", "raw48_20261110_120000_tx.wav"]
        );
    }

    #[test]
    fn dead_air_slot_is_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = RawSlotWriter::open(tmp.path().to_path_buf(), RAW_CAPTURE_MAX_BYTES).unwrap();
        let mut s = slot("2026-11-10T12:00:00Z", 1, 0, false);
        s.samples.iter_mut().for_each(|x| *x = 0.0);
        assert_eq!(w.write(&s), WriteOutcome::DeadAir);
        assert!(wavs(tmp.path()).is_empty());
    }

    #[test]
    fn open_creates_the_directory_and_counts_existing_wavs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let w = RawSlotWriter::open(dir.clone(), RAW_CAPTURE_MAX_BYTES).unwrap();
        assert!(dir.is_dir());
        assert_eq!(w.total_bytes(), 0);
        std::fs::write(dir.join("a.wav"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("b.wav"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("c.wav"), vec![0u8; 5000]).unwrap();
        let w = RawSlotWriter::open(dir, RAW_CAPTURE_MAX_BYTES).unwrap();
        assert_eq!(w.total_bytes(), 2000);
    }

    #[test]
    fn cap_stops_before_the_file_that_would_pass_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = RawSlotWriter::open(tmp.path().to_path_buf(), 2 * STEREO_FILE + 100).unwrap();
        assert!(matches!(
            w.write(&slot("2026-11-10T12:00:00Z", 2, 0, false)),
            WriteOutcome::Written { .. }
        ));
        assert!(matches!(
            w.write(&slot("2026-11-10T12:00:15Z", 2, 0, false)),
            WriteOutcome::Written { .. }
        ));
        assert_eq!(
            w.write(&slot("2026-11-10T12:00:30Z", 2, 0, false)),
            WriteOutcome::CapReached {
                total: 2 * STEREO_FILE
            }
        );
        assert_eq!(
            w.write(&slot("2026-11-10T12:00:45Z", 2, 0, false)),
            WriteOutcome::Stopped
        );
        assert_eq!(wavs(tmp.path()).len(), 2);
    }

    #[test]
    fn full_directory_at_open_stops_at_the_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let max = 10 * STEREO_FILE;
        let seed = std::fs::File::create(tmp.path().join("seed.wav")).unwrap();
        seed.set_len(max - 1000).unwrap();
        let mut w = RawSlotWriter::open(tmp.path().to_path_buf(), max).unwrap();
        assert!(matches!(
            w.write(&slot("2026-11-10T12:00:00Z", 1, 0, false)),
            WriteOutcome::CapReached { .. }
        ));
    }

    #[test]
    fn cap_message_names_dir_size_cap_and_continuation() {
        let dir = Path::new("/rec/raw48");
        assert_eq!(
            cap_reached_message(dir, 2_145_603_174, RAW_CAPTURE_MAX_BYTES),
            "Raw 48 kHz capture stopped: /rec/raw48 holds 2046.2 MB and the next file would \
             pass the 2.0 GB cap. Decode-window recording continues."
        );
    }

    #[test]
    fn queue_full_drops_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let w = RawSlotWriter::open(tmp.path().to_path_buf(), RAW_CAPTURE_MAX_BYTES).unwrap();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let h = RawCaptureWriterHandle::spawn_gated(w, gate_rx).unwrap();
        let t0 = std::time::Instant::now();
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:00Z", 1, 0, false)),
            SubmitOutcome::Queued
        );
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:15Z", 1, 0, false)),
            SubmitOutcome::Queued
        );
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:30Z", 1, 0, false)),
            SubmitOutcome::QueueFull
        );
        assert!(t0.elapsed() < std::time::Duration::from_millis(50));
        gate_tx.send(()).unwrap();
        drop(h);
        assert_eq!(wavs(tmp.path()).len(), 2);
    }

    #[test]
    fn drop_writes_queued_slots_and_joins() {
        let tmp = tempfile::tempdir().unwrap();
        let w = RawSlotWriter::open(tmp.path().to_path_buf(), RAW_CAPTURE_MAX_BYTES).unwrap();
        let h = RawCaptureWriterHandle::spawn(w).unwrap();
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:00Z", 1, 0, false)),
            SubmitOutcome::Queued
        );
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:15Z", 1, 0, false)),
            SubmitOutcome::Queued
        );
        drop(h);
        assert_eq!(
            wavs(tmp.path()),
            vec!["raw48_20261110_120000.wav", "raw48_20261110_120015.wav"]
        );
    }

    #[test]
    fn stopped_flag_is_visible_to_the_submitter() {
        let tmp = tempfile::tempdir().unwrap();
        let w = RawSlotWriter::open(tmp.path().to_path_buf(), 1000).unwrap();
        let h = RawCaptureWriterHandle::spawn(w).unwrap();
        assert!(!h.stopped());
        assert_eq!(
            h.submit(slot("2026-11-10T12:00:00Z", 1, 0, false)),
            SubmitOutcome::Queued
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !h.stopped() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(h.stopped());
    }

    #[test]
    fn operator_messages_are_exact() {
        let dir = Path::new("/home/op/.pancetta/recordings/raw48");
        let until = t("2026-11-10T12:56:19Z");
        assert_eq!(
            enabled_message(dir, 30, until, 0, RAW_CAPTURE_MAX_BYTES),
            "Raw 48 kHz capture enabled: dir=/home/op/.pancetta/recordings/raw48, length=30 min \
             (until 12:56:19 UTC), existing=0.0 MB, cap=2.0 GB"
        );
        assert_eq!(
            finished_message(30, until),
            "Raw 48 kHz capture finished: raw_capture_minutes = 30 ended at 12:56:19 UTC"
        );
        assert_eq!(
            summary_message(120, 120 * STEREO_FILE, dir),
            "Raw 48 kHz capture wrote 120 files (329.6 MB) to /home/op/.pancetta/recordings/raw48"
        );
        assert_eq!(
            paused_message("FT4"),
            "Raw 48 kHz capture paused: FT4 is active and raw capture records FT8 slots only"
        );
        assert_eq!(
            MSG_RAW_CAPTURE_RESUMED,
            "Raw 48 kHz capture resumed: FT8 is active"
        );
        assert_eq!(
            needs_recording_message(30),
            "Raw 48 kHz capture off -- audio.recording.raw_capture_minutes = 30 needs WAV \
             recording, which is off for this run"
        );
        assert_eq!(
            unsupported_rate_message(96_000),
            "Raw 48 kHz capture off -- [audio] sample_rate is 96000 Hz and raw capture needs \
             48000 Hz"
        );
        assert_eq!(
            dir_failure_message(dir, &"permission denied"),
            "Raw 48 kHz capture off: cannot use /home/op/.pancetta/recordings/raw48: permission denied"
        );
        assert_eq!(
            queue_full_message(t("2026-11-10T12:27:00Z")),
            "Raw 48 kHz capture dropped the 12:27:00 UTC slot: the writer is still busy with \
             earlier slots (slow disk?). Decoding is unaffected."
        );
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::coordinator::dsp::WavRecording;

    #[test]
    fn zero_minutes_is_off_whatever_the_rest_says() {
        let rec = WavRecording::Enabled {
            dir: PathBuf::from("/rec"),
        };
        let d = resolve_raw_capture(0, &rec, 48_000);
        assert_eq!(d, RawCaptureDecision::Off);
        assert_eq!(d.startup_message(), None);
    }

    #[test]
    fn minutes_without_recording_are_ignored() {
        for rec in [
            WavRecording::DisabledByConfig,
            WavRecording::SuppressedReplay,
            WavRecording::SuppressedNoAudio,
            WavRecording::NoHomeDir {
                directory: "~/rec".into(),
            },
        ] {
            let d = resolve_raw_capture(30, &rec, 48_000);
            assert_eq!(d, RawCaptureDecision::NeedsRecording { minutes: 30 });
            assert_eq!(
                d.startup_message().as_deref(),
                Some(
                    "Raw 48 kHz capture off -- audio.recording.raw_capture_minutes = 30 needs \
                     WAV recording, which is off for this run"
                )
            );
        }
    }

    #[test]
    fn non_48k_rate_is_refused() {
        let rec = WavRecording::Enabled {
            dir: PathBuf::from("/rec"),
        };
        let d = resolve_raw_capture(30, &rec, 96_000);
        assert_eq!(
            d,
            RawCaptureDecision::UnsupportedRate {
                minutes: 30,
                rate: 96_000
            }
        );
        assert_eq!(
            d.startup_message().as_deref(),
            Some(
                "Raw 48 kHz capture off -- [audio] sample_rate is 96000 Hz and raw capture \
                 needs 48000 Hz"
            )
        );
    }

    #[test]
    fn enabled_capture_goes_into_raw48() {
        let rec = WavRecording::Enabled {
            dir: PathBuf::from("/rec"),
        };
        let d = resolve_raw_capture(30, &rec, 48_000);
        assert_eq!(
            d,
            RawCaptureDecision::Enabled {
                dir: PathBuf::from("/rec/raw48"),
                minutes: 30
            }
        );
        assert_eq!(d.startup_message(), None);
    }
}

#[cfg(test)]
mod raw_capture_glue_tests {
    use super::test_stream::*;
    use super::*;

    /// Feed synthetic real-time batches (no sleeping). After each batch the
    /// feed waits until the writer has caught up with every queued slot (or
    /// stopped), so a fast synthetic feed never overruns the 2-slot queue.
    fn feed_capture(
        cap: &mut RawCapture,
        dir: &Path,
        start: DateTime<Utc>,
        duration_s: f64,
    ) -> Option<DateTime<Utc>> {
        let frames = 512usize;
        let batch_ns = frames as f64 / 48_000.0 * 1e9;
        let total = (duration_s * 1e9 / batch_ns).ceil() as u64;
        for idx in 0..total {
            let end = from_ns(ns(start) + ((idx + 1) as f64 * batch_ns).round() as i64);
            let samples = samples_for(1, idx * frames as u64, frames);
            let state = cap.on_batch(&samples, &ctx_at(end));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while (raw_wavs(dir).len() as u64) < cap.submitted
                && !cap.writer.stopped()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if state == RawCaptureState::Done {
                return Some(end);
            }
        }
        None
    }

    fn raw_wavs(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("raw48_") && n.ends_with(".wav"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn capture_writes_files_and_reports_done() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let start = t("2026-11-10T12:00:07.123Z");
        let w = window(start, 1);
        let mut cap =
            RawCapture::open(dir.clone(), 1, w, 1, 0.5, RAW_CAPTURE_MAX_BYTES, start).unwrap();
        let done_at = feed_capture(&mut cap, &dir, start, 120.0).expect("capture reports Done");
        assert!(done_at >= t("2026-11-10T12:01:14.5Z"));
        // The DSP's close path: the join runs on its own thread.
        cap.close_in_background();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while raw_wavs(&dir).len() < 4 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 12:00:15, :30, :45 and 12:01:00 (the last boundary before 12:01:07.123).
        assert_eq!(
            raw_wavs(&dir),
            vec![
                "raw48_20261110_120015_20m.wav",
                "raw48_20261110_120030_20m.wav",
                "raw48_20261110_120045_20m.wav",
                "raw48_20261110_120100_20m.wav",
            ]
        );
    }

    #[test]
    fn cap_stop_stops_new_slots() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let start = t("2026-11-10T12:00:07.123Z");
        let w = window(start, 5);
        // Room for exactly one mono file.
        let max = 1_440_044 + 68 + 10;
        let mut cap = RawCapture::open(dir.clone(), 5, w, 1, 0.5, max, start).unwrap();
        let done_at = feed_capture(&mut cap, &dir, start, 120.0);
        assert!(done_at.is_some(), "a stopped writer ends the capture");
        assert!(cap.writer.stopped());
        assert_eq!(cap.cutter.in_progress(), 0, "nothing is left buffering");
        drop(cap);
        assert_eq!(raw_wavs(&dir), vec!["raw48_20261110_120015_20m.wav"]);
    }

    #[test]
    fn window_already_over_at_open_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let start = t("2026-11-10T12:00:07.123Z");
        let w = window(start, 1);
        let later = start + chrono::Duration::minutes(5);
        assert!(
            RawCapture::open(dir.clone(), 1, w, 1, 0.5, RAW_CAPTURE_MAX_BYTES, later).is_none()
        );
        assert!(!dir.exists(), "no writer was opened");
    }

    #[test]
    fn pause_and_resume_follow_the_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("raw48");
        let start = t("2026-11-10T12:00:07.123Z");
        let mut cap = RawCapture::open(
            dir,
            1,
            window(start, 1),
            1,
            0.5,
            RAW_CAPTURE_MAX_BYTES,
            start,
        )
        .unwrap();
        let mut ctx = ctx_at(start + chrono::Duration::seconds(1));
        ctx.ft8_active = false;
        ctx.mode_label = "FT4";
        cap.on_batch(&samples_for(1, 0, 512), &ctx);
        assert!(cap.paused);
        ctx.ft8_active = true;
        ctx.end += chrono::Duration::milliseconds(11);
        cap.on_batch(&samples_for(1, 512, 512), &ctx);
        assert!(!cap.paused);
    }
}
