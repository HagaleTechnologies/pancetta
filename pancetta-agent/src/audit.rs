//! Bounded JSONL audit log for the pancetta station agent.
//!
//! Security posture (dispensa ADR-0002 §5 — operator attribution + audit):
//! every armed-TX state change, TX request/denial, local-kill toggle, and
//! local-consent change is recorded as one JSON object per line (JSONL). The
//! log exists so an operator (or an auditor) can reconstruct what the remote
//! path did and *who* it was attributed to. It is a bounded diagnostic
//! history, not an immutable or exhaustive ledger (PAN-104).
//!
//! Design invariants:
//! - **Clock-injected timestamps.** [`AuditEvent::ts_unix_ms`] is supplied by
//!   the caller; this module never reads a wall clock. Rate allowances and
//!   maintenance cadence use a monotonic clock (injectable in tests), so a
//!   backward or future event timestamp can never refill an allowance.
//! - **Bounded storage.** The active file and its `.1`..`.3` siblings (`.1`
//!   newest) are each at most [`MAX_FILE_BYTES`]; a write that would overflow
//!   the active file rotates it first. Operator and detail strings are capped
//!   (UTF-8 safe, `...[truncated]` suffix) before serialization and the final
//!   JSONL record is capped too. A pre-existing managed file over the limit is
//!   reset to zero bytes before any further append, and the discarded byte
//!   count is reported as a `Summary` record.
//! - **Bounded attempts.** Each original [`AuditKind`] has its own token
//!   bucket (burst [`RATE_BURST`], refill [`RATE_REFILL_PER_SEC`]/s). An event
//!   over its allowance is not written; it is counted, and [`AuditLog::maintain`]
//!   later writes one `Summary` record with the counts and timestamp bounds.
//! - **Failures are surfaced, never fatal.** [`AuditLog::append`] returns a
//!   typed [`AuditError`], every failure marks the shared health (read through
//!   [`AuditLog::health`] by the readiness strip) and logs
//!   [`UNAVAILABLE_MESSAGE`] at ERROR on the first failure and at most once per
//!   minute after. Nothing here panics, and no disarm or PTT-off path may wait
//!   on a successful write: callers release authority locks and PTT first.
//! - **One writer.** Clones share a single mutex around the whole
//!   inspect/normalize/rotate/write transaction, plus the buckets, counters
//!   and health. Health reads never take the writer lock.
//!
//! In-memory omission counts are lost if the process crashes before the next
//! maintenance writes them. `sync_data` confirms each record's file write; it
//! does not make directory renames transactional across power loss.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Maximum bytes in the active file and in each archive.
const MAX_FILE_BYTES: u64 = 1_048_576;
/// Number of retained archives (`.1` newest .. `.3` oldest).
const ARCHIVE_COUNT: usize = 3;
/// Operator-callsign cap, in UTF-8 bytes, including the truncation marker.
const MAX_OPERATOR_BYTES: usize = 64;
/// Detail cap, in UTF-8 bytes, including the truncation marker.
const MAX_DETAIL_BYTES: usize = 1024;
/// Serialized record cap, including the trailing newline.
const MAX_RECORD_BYTES: usize = 8192;
/// Per-kind token-bucket burst.
const RATE_BURST: u32 = 20;
/// Per-kind token-bucket refill, records per second.
const RATE_REFILL_PER_SEC: u32 = 10;
/// Minimum spacing between maintenance batches; also the minimum time a
/// failure episode stays visible.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5);
/// Minimum spacing between repeated ERROR reports while failure persists.
const ERROR_REPEAT_INTERVAL: Duration = Duration::from_secs(60);
/// Suffix appended to a truncated field, inside its cap.
const TRUNCATION_MARKER: &str = "...[truncated]";
/// Bytes read per step while looking backward for the last complete line.
const TAIL_SCAN_CHUNK: usize = 4096;

/// Operator-facing wording for a failed audit write. The readiness strip and
/// the headless ERROR line both use it.
pub const UNAVAILABLE_MESSAGE: &str = "Remote audit unavailable; events may be missing";
/// Headless INFO line when a failure episode clears.
const RECOVERED_MESSAGE: &str = "Remote audit recording recovered";

/// Category of an auditable agent event.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AuditKind {
    /// The armed-TX state machine transitioned into the armed state.
    Armed,
    /// The armed-TX state machine transitioned out of the armed state
    /// (operator disarm, TTL expiry, or heartbeat loss).
    Disarmed,
    /// A remote TX was requested (attributed to an operator callsign).
    TxRequested,
    /// A remote TX was denied by the safety gate (with a reason in `detail`).
    TxDenied,
    /// The station-local kill switch was engaged or cleared.
    LocalKill,
    /// The station-local consent gate (`remote_tx_enabled`) changed.
    LocalConsentChanged,
    /// Written by the audit log itself: events coalesced by the rate limit or
    /// not confirmed written, and retention resets. Never accepted from
    /// [`AuditLog::append`]; its operator is always `None`.
    Summary,
}

/// The six caller-supplied kinds, in enum order (bucket and counter index).
const EVENT_KINDS: [AuditKind; 6] = [
    AuditKind::Armed,
    AuditKind::Disarmed,
    AuditKind::TxRequested,
    AuditKind::TxDenied,
    AuditKind::LocalKill,
    AuditKind::LocalConsentChanged,
];

fn kind_index(kind: &AuditKind) -> Option<usize> {
    match kind {
        AuditKind::Armed => Some(0),
        AuditKind::Disarmed => Some(1),
        AuditKind::TxRequested => Some(2),
        AuditKind::TxDenied => Some(3),
        AuditKind::LocalKill => Some(4),
        AuditKind::LocalConsentChanged => Some(5),
        AuditKind::Summary => None,
    }
}

/// A single audit record.
///
/// `ts_unix_ms` is injected by the caller — this type never reads a clock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// Event timestamp in unix milliseconds, supplied by the caller.
    pub ts_unix_ms: i64,
    /// Category of the event.
    pub kind: AuditKind,
    /// Operator the event is attributed to, if any.
    pub operator_callsign: Option<String>,
    /// Free-form human-readable detail (e.g. a denial reason).
    pub detail: String,
}

/// What [`AuditLog::append`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAppendOutcome {
    /// Written, flushed and `sync_data`'d.
    Recorded,
    /// Over its kind's rate allowance: counted for the next `Summary` record.
    Coalesced,
}

/// The step of the audit transaction that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditOp {
    /// `Summary` passed to `append` (reserved for the log itself).
    Validate,
    /// The shared writer state was poisoned.
    Lock,
    /// Storage was already failed and the event was over its allowance.
    Unavailable,
    /// The event could not be serialized within the record cap.
    Encode,
    /// Reading a managed file's metadata or tail.
    Inspect,
    /// Opening the active file.
    Open,
    /// Writing the record.
    Write,
    /// Flushing the record.
    Flush,
    /// `sync_data` on the record.
    Sync,
    /// Removing or renaming an archive.
    Rotate,
    /// Resetting or trimming a managed file.
    Truncate,
    /// Restoring the pre-write length after a failed write.
    Rollback,
}

/// A failed audit operation. Its [`AuditOp`] names the bounded step; the
/// message carries the underlying cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditError {
    /// The step that failed.
    pub op: AuditOp,
    /// The underlying cause.
    pub message: String,
}

impl AuditError {
    fn new(op: AuditOp, message: impl Into<String>) -> Self {
        Self {
            op,
            message: message.into(),
        }
    }

    fn io(op: AuditOp, e: &io::Error) -> Self {
        Self::new(op, e.to_string())
    }
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audit {:?} failed: {}", self.op, self.message)
    }
}

impl std::error::Error for AuditError {}

/// Shared audit health, read without touching the writer lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditHealthSnapshot {
    /// A failure episode is open: some events may be missing.
    pub unavailable: bool,
    /// Events whose recording could not be confirmed, since startup.
    pub failed_events: u64,
    /// The step that failed most recently, if any.
    pub last_failure: Option<AuditOp>,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    max_file_bytes: u64,
    archives: usize,
    max_operator_bytes: usize,
    max_detail_bytes: usize,
    max_record_bytes: usize,
    burst: u32,
    refill_per_sec: u32,
    maintenance_interval: Duration,
    error_repeat: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            archives: ARCHIVE_COUNT,
            max_operator_bytes: MAX_OPERATOR_BYTES,
            max_detail_bytes: MAX_DETAIL_BYTES,
            max_record_bytes: MAX_RECORD_BYTES,
            burst: RATE_BURST,
            refill_per_sec: RATE_REFILL_PER_SEC,
            maintenance_interval: MAINTENANCE_INTERVAL,
            error_repeat: ERROR_REPEAT_INTERVAL,
        }
    }
}

type MonotonicClock = Arc<dyn Fn() -> Duration + Send + Sync>;

#[derive(Clone, Copy, Debug)]
struct Bucket {
    /// Thousandths of a token.
    milli_tokens: u64,
    last_refill: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
struct KindCounts {
    coalesced: u64,
    failed: u64,
    first_ts: i64,
    last_ts: i64,
    saturated: bool,
}

impl KindCounts {
    fn is_pending(&self) -> bool {
        self.coalesced > 0 || self.failed > 0
    }

    fn note_ts(&mut self, ts: i64) {
        if self.is_pending() {
            self.first_ts = self.first_ts.min(ts);
            self.last_ts = self.last_ts.max(ts);
        } else {
            self.first_ts = ts;
            self.last_ts = ts;
        }
    }

    fn add_coalesced(&mut self, ts: i64) {
        self.note_ts(ts);
        self.saturated |= self.coalesced == u64::MAX;
        self.coalesced = self.coalesced.saturating_add(1);
    }

    fn add_failed(&mut self, ts: i64) {
        self.note_ts(ts);
        self.saturated |= self.failed == u64::MAX;
        self.failed = self.failed.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RetentionNotice {
    discarded_bytes: u64,
    reset: bool,
    pending: bool,
}

#[derive(Debug)]
struct WriterState {
    normalized: bool,
    tail_invalid: bool,
    buckets: [Bucket; 6],
    pending: [KindCounts; 6],
    /// Slot 0 is the active file; slots 1..=archives are the archives.
    retention: Vec<RetentionNotice>,
    last_maintenance: Option<Duration>,
}

#[derive(Debug, Default)]
struct HealthState {
    failed_since: Option<Duration>,
    last_report: Option<Duration>,
    failed_events: u64,
    last_failure: Option<AuditOp>,
}

struct Shared {
    limits: Limits,
    clock: MonotonicClock,
    writer: Mutex<WriterState>,
    health: Mutex<HealthState>,
    #[cfg(test)]
    faults: Mutex<Vec<AuditOp>>,
}

/// A bounded, rotating JSONL audit log backed by files on disk.
///
/// Clones share one writer, so rotation and the byte bounds hold across every
/// holder. Files are opened per write and closed before any rename.
#[derive(Clone)]
pub struct AuditLog {
    path: PathBuf,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLog")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl AuditLog {
    /// Create an audit log that writes to `path` (plus `.1`..`.3` archives).
    ///
    /// Nothing is opened or created until the first [`append`](Self::append);
    /// tests inject a tempfile path here.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let start = Instant::now();
        Self::with_options(path, Limits::default(), Arc::new(move || start.elapsed()))
    }

    fn with_options(path: impl Into<PathBuf>, limits: Limits, clock: MonotonicClock) -> Self {
        let now = clock();
        let full = u64::from(limits.burst) * 1000;
        let bucket = Bucket {
            milli_tokens: full,
            last_refill: now,
        };
        Self {
            path: path.into(),
            shared: Arc::new(Shared {
                limits,
                clock,
                writer: Mutex::new(WriterState {
                    normalized: false,
                    tail_invalid: false,
                    buckets: [bucket; 6],
                    pending: [KindCounts::default(); 6],
                    retention: vec![RetentionNotice::default(); limits.archives + 1],
                    last_maintenance: None,
                }),
                health: Mutex::new(HealthState::default()),
                #[cfg(test)]
                faults: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The active file's path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Current audit health. Takes only the short-held health lock, never the
    /// writer lock, so a stalled write cannot block a readiness snapshot.
    pub fn health(&self) -> AuditHealthSnapshot {
        let h = self
            .shared
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AuditHealthSnapshot {
            unavailable: h.failed_since.is_some(),
            failed_events: h.failed_events,
            last_failure: h.last_failure,
        }
    }

    /// Append one event as a single JSON line (`{...}\n`).
    ///
    /// Returns [`AuditAppendOutcome::Coalesced`] when the event's kind is over
    /// its allowance (counted for a later `Summary`), and an [`AuditError`]
    /// when the record could not be confirmed written. Every error has already
    /// marked the shared health before it is returned. Never panics.
    pub fn append(&self, ev: &AuditEvent) -> Result<AuditAppendOutcome, AuditError> {
        let Some(idx) = kind_index(&ev.kind) else {
            return Err(AuditError::new(
                AuditOp::Validate,
                "Summary records are written only by the audit log",
            ));
        };
        let now = (self.shared.clock)();
        let mut st = match self.shared.writer.lock() {
            Ok(g) => g,
            Err(_) => {
                let e = AuditError::new(AuditOp::Lock, "audit writer state poisoned");
                self.note_failure(&e, now, 1);
                return Err(e);
            }
        };
        if !self.take_token(&mut st, idx, now) {
            if self.health().unavailable {
                st.pending[idx].add_failed(ev.ts_unix_ms);
                let e = AuditError::new(
                    AuditOp::Unavailable,
                    "audit storage unavailable and event over its allowance",
                );
                self.note_failure(&e, now, 1);
                return Err(e);
            }
            st.pending[idx].add_coalesced(ev.ts_unix_ms);
            return Ok(AuditAppendOutcome::Coalesced);
        }
        let written = encode_record(ev, &self.shared.limits)
            .and_then(|line| self.write_line(&mut st, line.as_bytes()));
        match written {
            Ok(()) => Ok(AuditAppendOutcome::Recorded),
            Err(e) => {
                st.pending[idx].add_failed(ev.ts_unix_ms);
                self.note_failure(&e, now, 1);
                Err(e)
            }
        }
    }

    /// Best-effort [`append`](Self::append) for control paths: the outcome is
    /// not needed by the caller because a failure has already been recorded in
    /// the shared health (readiness strip + bounded ERROR log), and control
    /// handling must continue regardless.
    pub fn record(&self, ev: &AuditEvent) {
        if let Err(e) = self.append(ev) {
            tracing::debug!(target: "agent.audit", error = %e, kind = ?ev.kind, "audit event not recorded");
        }
    }

    /// Write pending `Summary` records and repair storage, at most once per
    /// [`MAINTENANCE_INTERVAL`]. `now_unix_ms` stamps the summaries.
    ///
    /// A healthy log with nothing pending does no I/O. A writer busy in
    /// another thread is skipped rather than waited on. A failure episode
    /// clears only after storage is repaired, every pending count is written,
    /// and at least one interval has passed since the failure began.
    pub fn maintain(&self, now_unix_ms: i64) -> Result<(), AuditError> {
        let now = (self.shared.clock)();
        let mut st = match self.shared.writer.try_lock() {
            Ok(g) => g,
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Poisoned(_)) => {
                let e = AuditError::new(AuditOp::Lock, "audit writer state poisoned");
                self.note_failure(&e, now, 0);
                return Err(e);
            }
        };
        let unavailable = self.health().unavailable;
        let pending =
            st.pending.iter().any(KindCounts::is_pending) || st.retention.iter().any(|r| r.pending);
        if !unavailable && !pending && !st.tail_invalid {
            return Ok(());
        }
        if st
            .last_maintenance
            .is_some_and(|last| now.saturating_sub(last) < self.shared.limits.maintenance_interval)
        {
            return Ok(());
        }
        st.last_maintenance = Some(now);
        match self.flush_pending(&mut st, now_unix_ms) {
            Ok(()) => {
                self.maybe_recover(now);
                Ok(())
            }
            Err(e) => {
                self.note_failure(&e, now, 0);
                Err(e)
            }
        }
    }

    fn flush_pending(&self, st: &mut WriterState, now_unix_ms: i64) -> Result<(), AuditError> {
        if !st.normalized || st.tail_invalid {
            self.normalize(st)?;
        }
        let mut wrote = false;
        for slot in 0..st.retention.len() {
            let notice = st.retention[slot];
            if !notice.pending {
                continue;
            }
            let ev = summary_event(now_unix_ms, retention_detail(slot, &notice));
            let line = encode_record(&ev, &self.shared.limits)?;
            self.write_line(st, line.as_bytes())?;
            st.retention[slot] = RetentionNotice::default();
            wrote = true;
        }
        if st.pending.iter().any(KindCounts::is_pending) {
            let ev = summary_event(now_unix_ms, counts_detail(&st.pending));
            let line = encode_record(&ev, &self.shared.limits)?;
            self.write_line(st, line.as_bytes())?;
            st.pending = [KindCounts::default(); 6];
            wrote = true;
        }
        if !wrote {
            // Nothing to record: prove the active file is reachable again.
            self.check(AuditOp::Open)
                .and_then(|()| {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                })
                .map_err(|e| AuditError::io(AuditOp::Open, &e))?;
        }
        Ok(())
    }

    fn maybe_recover(&self, now: Duration) {
        let recovered = {
            let mut h = self
                .shared
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match h.failed_since {
                Some(since)
                    if now.saturating_sub(since) >= self.shared.limits.maintenance_interval =>
                {
                    h.failed_since = None;
                    h.last_report = None;
                    true
                }
                _ => false,
            }
        };
        if recovered {
            tracing::info!(target: "agent.audit", path = %self.path.display(), "{RECOVERED_MESSAGE}");
        }
    }

    fn note_failure(&self, e: &AuditError, now: Duration, events: u64) {
        let report = {
            let mut h = self
                .shared
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            h.failed_events = h.failed_events.saturating_add(events);
            h.last_failure = Some(e.op);
            if h.failed_since.is_none() {
                h.failed_since = Some(now);
            }
            let due = h
                .last_report
                .is_none_or(|t| now.saturating_sub(t) >= self.shared.limits.error_repeat);
            if due {
                h.last_report = Some(now);
            }
            due.then_some(h.failed_events)
        };
        if let Some(failed_events) = report {
            tracing::error!(
                target: "agent.audit",
                operation = ?e.op,
                error = %e.message,
                failed_events,
                path = %self.path.display(),
                "{UNAVAILABLE_MESSAGE}"
            );
        }
    }

    fn take_token(&self, st: &mut WriterState, idx: usize, now: Duration) -> bool {
        let limits = &self.shared.limits;
        let full = u64::from(limits.burst) * 1000;
        let b = &mut st.buckets[idx];
        let elapsed = now.saturating_sub(b.last_refill);
        let refill = elapsed
            .as_millis()
            .saturating_mul(u128::from(limits.refill_per_sec));
        b.milli_tokens = u64::try_from(refill)
            .unwrap_or(u64::MAX)
            .saturating_add(b.milli_tokens)
            .min(full);
        b.last_refill = b.last_refill.max(now);
        if b.milli_tokens >= 1000 {
            b.milli_tokens -= 1000;
            true
        } else {
            false
        }
    }

    fn slot_path(&self, slot: usize) -> PathBuf {
        if slot == 0 {
            return self.path.clone();
        }
        let mut s: OsString = self.path.clone().into_os_string();
        s.push(format!(".{slot}"));
        PathBuf::from(s)
    }

    /// Length of a managed file; `None` when absent. Anything but a regular
    /// file is an error — it is never truncated or removed.
    fn file_len(&self, slot: usize) -> Result<Option<u64>, AuditError> {
        let path = self.slot_path(slot);
        match self.check(AuditOp::Inspect).and_then(|()| path.metadata()) {
            Ok(m) if m.is_file() => Ok(Some(m.len())),
            Ok(_) => Err(AuditError::new(
                AuditOp::Inspect,
                format!("{} is not a regular file", path.display()),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuditError::io(AuditOp::Inspect, &e)),
        }
    }

    /// Bring every managed file within the cap and drop an incomplete
    /// trailing record from the active file.
    fn normalize(&self, st: &mut WriterState) -> Result<(), AuditError> {
        let max = self.shared.limits.max_file_bytes;
        for slot in 0..st.retention.len() {
            let Some(len) = self.file_len(slot)? else {
                continue;
            };
            if len > max {
                self.truncate_to(slot, 0)?;
                let notice = &mut st.retention[slot];
                notice.discarded_bytes = notice.discarded_bytes.saturating_add(len);
                notice.reset = true;
                notice.pending = true;
                tracing::warn!(
                    target: "agent.audit",
                    path = %self.slot_path(slot).display(),
                    discarded_bytes = len,
                    "audit retention: reset an oversized audit file"
                );
            } else if slot == 0 && len > 0 {
                let keep = self.complete_prefix_len(len)?;
                if keep < len {
                    self.truncate_to(0, keep)?;
                    let notice = &mut st.retention[0];
                    notice.discarded_bytes = notice.discarded_bytes.saturating_add(len - keep);
                    notice.pending = true;
                }
            }
        }
        st.normalized = true;
        st.tail_invalid = false;
        Ok(())
    }

    fn complete_prefix_len(&self, len: u64) -> Result<u64, AuditError> {
        let inspect = |e: io::Error| AuditError::io(AuditOp::Inspect, &e);
        let mut f = File::open(&self.path).map_err(inspect)?;
        let mut buf = [0u8; TAIL_SCAN_CHUNK];
        let mut end = len;
        while end > 0 {
            let start = end.saturating_sub(TAIL_SCAN_CHUNK as u64);
            let n = (end - start) as usize;
            f.seek(SeekFrom::Start(start)).map_err(inspect)?;
            f.read_exact(&mut buf[..n]).map_err(inspect)?;
            if let Some(pos) = buf[..n].iter().rposition(|&b| b == b'\n') {
                return Ok(start + pos as u64 + 1);
            }
            end = start;
        }
        Ok(0)
    }

    fn truncate_to(&self, slot: usize, len: u64) -> Result<(), AuditError> {
        self.check(AuditOp::Truncate)
            .and_then(|()| {
                let f = OpenOptions::new().write(true).open(self.slot_path(slot))?;
                f.set_len(len)?;
                f.sync_data()
            })
            .map_err(|e| AuditError::io(AuditOp::Truncate, &e))
    }

    /// Shift `.N-1`→`.N` … active→`.1`, removing the oldest first. Only an
    /// absent slot (`NotFound`) is ignored.
    fn rotate(&self) -> Result<(), AuditError> {
        let archives = self.shared.limits.archives;
        if archives == 0 {
            return self.truncate_to(0, 0);
        }
        let ignore_absent = |r: io::Result<()>| match r {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(AuditError::io(AuditOp::Rotate, &e))
            }
            _ => Ok(()),
        };
        ignore_absent(
            self.check(AuditOp::Rotate)
                .and_then(|()| std::fs::remove_file(self.slot_path(archives))),
        )?;
        for slot in (0..archives).rev() {
            ignore_absent(std::fs::rename(
                self.slot_path(slot),
                self.slot_path(slot + 1),
            ))?;
        }
        Ok(())
    }

    /// The write transaction. Caller holds the writer lock.
    fn write_line(&self, st: &mut WriterState, line: &[u8]) -> Result<(), AuditError> {
        if !st.normalized || st.tail_invalid {
            self.normalize(st)?;
        }
        let max = self.shared.limits.max_file_bytes;
        let mut pre_len = self.file_len(0)?.unwrap_or(0);
        if pre_len > 0 && pre_len.saturating_add(line.len() as u64) > max {
            self.rotate()?;
            pre_len = self.file_len(0)?.unwrap_or(0);
            if pre_len > 0 {
                return Err(AuditError::new(
                    AuditOp::Rotate,
                    "active file still present after rotation",
                ));
            }
        }
        let mut f = self
            .check(AuditOp::Open)
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
            })
            .map_err(|e| AuditError::io(AuditOp::Open, &e))?;
        let result = self
            .write_record(&mut f, line)
            .map_err(|e| AuditError::io(AuditOp::Write, &e))
            .and_then(|()| {
                self.check(AuditOp::Flush)
                    .and_then(|()| f.flush())
                    .map_err(|e| AuditError::io(AuditOp::Flush, &e))
            })
            .and_then(|()| {
                self.check(AuditOp::Sync)
                    .and_then(|()| f.sync_data())
                    .map_err(|e| AuditError::io(AuditOp::Sync, &e))
            });
        drop(f);
        if let Err(e) = result {
            if self
                .check(AuditOp::Rollback)
                .and_then(|()| {
                    let f = OpenOptions::new().write(true).open(&self.path)?;
                    f.set_len(pre_len)
                })
                .is_err()
            {
                st.tail_invalid = true;
            }
            return Err(e);
        }
        Ok(())
    }

    fn write_record(&self, f: &mut File, line: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if self.check(AuditOp::Write).is_err() {
            // Partial write: half the record reaches the file, then an error.
            f.write_all(&line[..line.len() / 2])?;
            return Err(io::Error::other("injected partial write"));
        }
        f.write_all(line)
    }

    /// Fault-injection point for tests; a no-op in production builds.
    #[cfg(not(test))]
    #[inline(always)]
    fn check(&self, _op: AuditOp) -> io::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn check(&self, op: AuditOp) -> io::Result<()> {
        let mut faults = self.shared.faults.lock().unwrap();
        match faults.iter().position(|f| *f == op) {
            Some(i) => {
                faults.remove(i);
                Err(io::Error::other(format!("injected {op:?} fault")))
            }
            None => Ok(()),
        }
    }
}

fn summary_event(ts_unix_ms: i64, detail: String) -> AuditEvent {
    AuditEvent {
        ts_unix_ms,
        kind: AuditKind::Summary,
        operator_callsign: None,
        detail,
    }
}

/// `audit summary: <Kind> coalesced=N failed=N first_ts=T last_ts=T` for each
/// nonzero kind in enum order, joined with `; `.
fn counts_detail(pending: &[KindCounts; 6]) -> String {
    let parts: Vec<String> = EVENT_KINDS
        .iter()
        .zip(pending.iter())
        .filter(|(_, c)| c.is_pending())
        .map(|(kind, c)| {
            let mut s = format!(
                "{kind:?} coalesced={} failed={} first_ts={} last_ts={}",
                c.coalesced, c.failed, c.first_ts, c.last_ts
            );
            if c.saturated {
                s.push_str(" saturated=true");
            }
            s
        })
        .collect();
    format!("audit summary: {}", parts.join("; "))
}

fn retention_detail(slot: usize, notice: &RetentionNotice) -> String {
    let what = if slot == 0 {
        "active file".to_string()
    } else {
        format!("archive={slot}")
    };
    let action = if notice.reset {
        "reset oversized"
    } else {
        "trimmed incomplete trailing record from"
    };
    format!(
        "audit retention: {action} {what}; discarded_bytes={}",
        notice.discarded_bytes
    )
}

/// Cap `s` at `cap` UTF-8 bytes, ending in [`TRUNCATION_MARKER`] when cut.
fn truncate_field(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let (keep, marker) = if cap >= TRUNCATION_MARKER.len() {
        (cap - TRUNCATION_MARKER.len(), TRUNCATION_MARKER)
    } else {
        (cap, "")
    };
    let mut end = keep;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &s[..end])
}

/// Serialize a capped copy of `ev` as one JSONL record within the record cap.
fn encode_record(ev: &AuditEvent, limits: &Limits) -> Result<String, AuditError> {
    let operator_callsign = ev
        .operator_callsign
        .as_deref()
        .map(|op| truncate_field(op, limits.max_operator_bytes));
    let mut detail_cap = limits.max_detail_bytes;
    loop {
        let bounded = AuditEvent {
            ts_unix_ms: ev.ts_unix_ms,
            kind: ev.kind.clone(),
            operator_callsign: operator_callsign.clone(),
            detail: truncate_field(&ev.detail, detail_cap),
        };
        let mut line = serde_json::to_string(&bounded)
            .map_err(|e| AuditError::new(AuditOp::Encode, e.to_string()))?;
        line.push('\n');
        if line.len() <= limits.max_record_bytes {
            return Ok(line);
        }
        if detail_cap == 0 {
            return Err(AuditError::new(
                AuditOp::Encode,
                "record exceeds the serialized-size cap",
            ));
        }
        // JSON escaping expanded the detail past the record cap.
        detail_cap /= 2;
    }
}

/// The default production audit-log path: `~/.pancetta/agent-audit.log`.
///
/// Falls back to `$HOME/.pancetta/...`, then to the current directory if no
/// home can be resolved (audit should still land *somewhere* rather than error).
pub fn default_audit_path() -> PathBuf {
    let home = dirs::home_dir()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".pancetta").join("agent-audit.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::{AtomicU64, Ordering};

    // Deterministic unique suffix for temp files — NOT a clock, NOT random.
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A tempfile path (and its archives) with automatic cleanup on drop.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path =
                std::env::temp_dir().join(format!("pancetta-agent-audit-test-{tag}-{pid}-{n}.log"));
            let tp = Self(path);
            // Ensure a clean slate if a prior run left files behind.
            tp.cleanup();
            tp
        }
        fn path(&self) -> &PathBuf {
            &self.0
        }
        fn slot(&self, n: usize) -> PathBuf {
            if n == 0 {
                return self.0.clone();
            }
            let mut s = self.0.clone().into_os_string();
            s.push(format!(".{n}"));
            PathBuf::from(s)
        }
        fn cleanup(&self) {
            for n in 0..=ARCHIVE_COUNT + 1 {
                let p = self.slot(n);
                let _ = std::fs::remove_file(&p);
                let _ = std::fs::remove_dir_all(&p);
            }
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    /// A manually advanced monotonic clock.
    #[derive(Clone, Default)]
    struct FakeClock(Arc<Mutex<Duration>>);

    impl FakeClock {
        fn advance(&self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }
        fn as_clock(&self) -> MonotonicClock {
            let inner = self.0.clone();
            Arc::new(move || *inner.lock().unwrap())
        }
    }

    fn small_limits(max_file_bytes: u64) -> Limits {
        Limits {
            max_file_bytes,
            max_record_bytes: max_file_bytes.min(MAX_RECORD_BYTES as u64) as usize,
            ..Limits::default()
        }
    }

    fn log_with(tp: &TempPath, limits: Limits, clock: &FakeClock) -> AuditLog {
        AuditLog::with_options(tp.path().clone(), limits, clock.as_clock())
    }

    fn inject(log: &AuditLog, op: AuditOp) {
        log.shared.faults.lock().unwrap().push(op);
    }

    fn ev(ts: i64, kind: AuditKind, op: Option<&str>, detail: &str) -> AuditEvent {
        AuditEvent {
            ts_unix_ms: ts,
            kind,
            operator_callsign: op.map(str::to_string),
            detail: detail.to_string(),
        }
    }

    fn read_lines(path: &PathBuf) -> Vec<String> {
        let f = std::fs::File::open(path).expect("open audit file");
        BufReader::new(f)
            .lines()
            .map(|l| l.expect("read line"))
            .collect()
    }

    fn read_events(path: &PathBuf) -> Vec<AuditEvent> {
        read_lines(path)
            .iter()
            .map(|l| serde_json::from_str(l).expect("every line parses"))
            .collect()
    }

    fn managed_sizes(tp: &TempPath) -> Vec<Option<u64>> {
        (0..=ARCHIVE_COUNT + 1)
            .map(|n| std::fs::metadata(tp.slot(n)).ok().map(|m| m.len()))
            .collect()
    }

    #[test]
    fn appends_n_events_as_n_parseable_lines_in_order() {
        let tp = TempPath::new("order");
        let log = AuditLog::new(tp.path().clone());

        let events = vec![
            ev(1000, AuditKind::Armed, Some("N0CALL"), "armed"),
            ev(2000, AuditKind::TxRequested, Some("N0CALL"), "tx#1"),
            ev(3000, AuditKind::TxDenied, Some("N0CALL"), "heartbeat lost"),
            ev(4000, AuditKind::Disarmed, Some("N0CALL"), "ttl expired"),
        ];
        for e in &events {
            assert_eq!(log.append(e), Ok(AuditAppendOutcome::Recorded));
        }

        let lines = read_lines(tp.path());
        assert_eq!(lines.len(), events.len(), "one line per event");

        for (line, expected) in lines.iter().zip(events.iter()) {
            let parsed: AuditEvent = serde_json::from_str(line).expect("parse line");
            assert_eq!(&parsed, expected, "line parses back to the exact event");
        }
        // Order preserved.
        assert_eq!(read_events(tp.path()), events);
        assert!(!log.health().unavailable);
    }

    #[test]
    fn each_line_is_a_single_json_object_with_no_embedded_newline() {
        let tp = TempPath::new("single-line");
        let log = AuditLog::new(tp.path().clone());
        log.append(&ev(
            42,
            AuditKind::LocalConsentChanged,
            None,
            "multi\nword\tdetail with spaces",
        ))
        .unwrap();
        let bytes = std::fs::read(tp.path()).unwrap();
        // Exactly one trailing newline, none in the middle.
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(*bytes.last().unwrap(), b'\n');
    }

    #[test]
    fn round_trip_preserves_all_fields_including_none_operator() {
        let tp = TempPath::new("roundtrip");
        let log = AuditLog::new(tp.path().clone());
        let with_op = ev(123, AuditKind::LocalKill, Some("W1XYZ"), "kill engaged");
        let no_op = ev(456, AuditKind::TxDenied, None, "not armed");
        log.append(&with_op).unwrap();
        log.append(&no_op).unwrap();

        let lines = read_lines(tp.path());
        let a: AuditEvent = serde_json::from_str(&lines[0]).unwrap();
        let b: AuditEvent = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(a, with_op);
        assert_eq!(b, no_op);
        assert_eq!(b.operator_callsign, None);
    }

    #[test]
    fn small_records_are_byte_identical_to_plain_serde() {
        let tp = TempPath::new("bytes");
        let log = AuditLog::new(tp.path().clone());
        let e = ev(
            1000,
            AuditKind::TxRequested,
            Some("N0CALL"),
            "callStation K1ABC",
        );
        log.append(&e).unwrap();
        let expected = format!("{}\n", serde_json::to_string(&e).unwrap());
        assert_eq!(std::fs::read_to_string(tp.path()).unwrap(), expected);
    }

    #[test]
    fn every_kind_serializes_and_round_trips() {
        let tp = TempPath::new("kinds");
        let log = AuditLog::new(tp.path().clone());
        for (i, k) in EVENT_KINDS.iter().enumerate() {
            log.append(&ev(i as i64, k.clone(), None, "d")).unwrap();
        }
        let lines = read_lines(tp.path());
        assert_eq!(lines.len(), EVENT_KINDS.len());
        for (line, k) in lines.iter().zip(EVENT_KINDS.iter()) {
            let parsed: AuditEvent = serde_json::from_str(line).unwrap();
            assert_eq!(&parsed.kind, k);
        }
    }

    #[test]
    fn summary_kind_is_refused_from_callers() {
        let tp = TempPath::new("summary-refused");
        let log = AuditLog::new(tp.path().clone());
        let err = log
            .append(&ev(1, AuditKind::Summary, Some("N0CALL"), "forged"))
            .unwrap_err();
        assert_eq!(err.op, AuditOp::Validate);
        assert!(!tp.path().exists(), "nothing written for a refused summary");
    }

    #[test]
    fn append_reports_an_error_when_path_parent_is_a_file() {
        // "<file>/child.log" must fail at the IO layer, return a typed error,
        // and mark health unavailable — not panic and not stay silent.
        let tp = TempPath::new("parent-is-file");
        std::fs::write(tp.path(), b"i am a file, not a directory").unwrap();
        let log = AuditLog::new(tp.path().join("child.log"));
        let err = log
            .append(&ev(1, AuditKind::Armed, None, "not recorded"))
            .unwrap_err();
        assert!(matches!(err.op, AuditOp::Inspect | AuditOp::Open), "{err}");
        let h = log.health();
        assert!(h.unavailable);
        assert_eq!(h.failed_events, 1);
        assert!(log
            .append(&ev(2, AuditKind::Disarmed, None, "also"))
            .is_err());
        assert_eq!(log.health().failed_events, 2);
    }

    #[test]
    fn append_reports_an_error_on_nonexistent_deep_parent() {
        let tp = TempPath::new("deep");
        let missing = tp.path().join("nope").join("still-nope").join("audit.log");
        let log = AuditLog::new(missing);
        let err = log
            .append(&ev(1, AuditKind::TxDenied, None, "not recorded"))
            .unwrap_err();
        assert_eq!(err.op, AuditOp::Open);
        assert!(log.health().unavailable);
    }

    #[test]
    fn production_limits_are_the_documented_values() {
        let l = Limits::default();
        assert_eq!(l.max_file_bytes, 1_048_576);
        assert_eq!(l.archives, 3);
        assert_eq!(l.max_operator_bytes, 64);
        assert_eq!(l.max_detail_bytes, 1024);
        assert_eq!(l.max_record_bytes, 8192);
        assert_eq!((l.burst, l.refill_per_sec), (20, 10));
        assert_eq!(l.maintenance_interval, Duration::from_secs(5));
        assert_eq!(l.error_repeat, Duration::from_secs(60));
        // Worst-case escaping of capped fields always fits the record cap.
        let worst = ev(
            i64::MIN,
            AuditKind::LocalConsentChanged,
            Some(&"\u{1}".repeat(500)),
            &"\u{1}".repeat(5000),
        );
        let line = encode_record(&worst, &l).unwrap();
        assert!(line.len() <= 8192);
        let back: AuditEvent = serde_json::from_str(line.trim_end()).unwrap();
        assert!(back.detail.ends_with(TRUNCATION_MARKER));
        assert!(back.operator_callsign.unwrap().len() <= 64);
    }

    #[test]
    fn fields_truncate_on_utf8_boundaries_inside_their_caps() {
        let s = "é".repeat(40); // 80 bytes
        let t = truncate_field(&s, 64);
        assert!(t.len() <= 64);
        assert!(t.ends_with(TRUNCATION_MARKER));
        assert!(t
            .trim_end_matches(TRUNCATION_MARKER)
            .chars()
            .all(|c| c == 'é'));
        assert_eq!(truncate_field("short", 64), "short");
        assert_eq!(truncate_field("abcdef", 3), "abc");

        let l = Limits::default();
        let e = ev(
            1,
            AuditKind::TxDenied,
            Some(&"W".repeat(200)),
            &"x".repeat(5000),
        );
        let back: AuditEvent =
            serde_json::from_str(encode_record(&e, &l).unwrap().trim_end()).unwrap();
        assert_eq!(back.operator_callsign.unwrap().len(), 64);
        assert_eq!(back.detail.len(), 1024);
    }

    #[test]
    fn rotates_before_overflow_and_keeps_newest_first_archive_order() {
        let tp = TempPath::new("rotate");
        let clock = FakeClock::default();
        // Each record below is exactly 100 bytes; a 300-byte file holds 3.
        let log = log_with(&tp, small_limits(300), &clock);
        let record = |i: i64| {
            let base = ev(i, AuditKind::LocalKill, None, "");
            let pad = 100 - encode_record(&base, &Limits::default()).unwrap().len();
            ev(i, AuditKind::LocalKill, None, &"p".repeat(pad))
        };
        assert_eq!(
            encode_record(&record(1), &Limits::default()).unwrap().len(),
            100
        );
        for i in 1..=3 {
            log.append(&record(i)).unwrap();
        }
        // Exactly at the cap: no rotation yet.
        assert_eq!(managed_sizes(&tp)[0], Some(300));
        assert_eq!(managed_sizes(&tp)[1], None);
        // One more record would overflow: rotate first.
        log.append(&record(4)).unwrap();
        assert_eq!(managed_sizes(&tp)[..2], [Some(100), Some(300)]);
        // Keep going well past every archive.
        for i in 5..=20 {
            clock.advance(Duration::from_secs(1));
            log.append(&record(i)).unwrap();
        }
        let sizes = managed_sizes(&tp);
        assert!(sizes[..=ARCHIVE_COUNT].iter().all(|s| s.unwrap() <= 300));
        assert_eq!(sizes[ARCHIVE_COUNT + 1], None, "no fifth managed file");
        let ts = |slot: usize| -> Vec<i64> {
            read_events(&tp.slot(slot))
                .iter()
                .map(|e| e.ts_unix_ms)
                .collect()
        };
        assert_eq!(ts(0), vec![19, 20]);
        assert_eq!(ts(1), vec![16, 17, 18], ".1 is the newest archive");
        assert_eq!(ts(2), vec![13, 14, 15]);
        assert_eq!(ts(3), vec![10, 11, 12]);
    }

    #[test]
    fn cloned_handles_share_one_writer_and_the_byte_bound() {
        let tp = TempPath::new("clones");
        let clock = FakeClock::default();
        let limits = Limits {
            burst: 10_000,
            ..small_limits(2_000)
        };
        let log = log_with(&tp, limits, &clock);
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let kind = EVENT_KINDS[(t + i) % 6].clone();
                        log.append(&ev(i as i64, kind, Some("N0CALL"), "concurrent"))
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let sizes = managed_sizes(&tp);
        assert!(sizes[..=ARCHIVE_COUNT].iter().all(|s| s.unwrap() <= 2_000));
        assert_eq!(sizes[ARCHIVE_COUNT + 1], None);
        for slot in 0..=ARCHIVE_COUNT {
            let _ = read_events(&tp.slot(slot)); // every line is complete JSON
        }
        // Clones share failure health.
        let other = log.clone();
        inject(&log, AuditOp::Open);
        assert!(log.append(&ev(1, AuditKind::Armed, None, "x")).is_err());
        assert!(other.health().unavailable);
    }

    #[test]
    fn oversized_legacy_files_are_reset_and_reported() {
        let tp = TempPath::new("legacy");
        std::fs::write(tp.path(), vec![b'a'; 1500]).unwrap();
        std::fs::write(tp.slot(2), vec![b'b'; 1200]).unwrap();
        // A gap at .1 and .3 is fine.
        let clock = FakeClock::default();
        let log = log_with(&tp, small_limits(1000), &clock);
        log.append(&ev(1, AuditKind::Armed, Some("N0CALL"), "fresh"))
            .unwrap();
        let sizes = managed_sizes(&tp);
        assert_eq!(sizes[2], Some(0), "oversized archive reset to zero");
        assert_eq!(
            read_events(tp.path()).len(),
            1,
            "active reset, then appended"
        );
        log.maintain(10_000).unwrap();
        let details: Vec<String> = read_events(tp.path())
            .into_iter()
            .filter(|e| e.kind == AuditKind::Summary)
            .map(|e| e.detail)
            .collect();
        assert_eq!(
            details,
            vec![
                "audit retention: reset oversized active file; discarded_bytes=1500".to_string(),
                "audit retention: reset oversized archive=2; discarded_bytes=1200".to_string(),
            ]
        );
    }

    #[test]
    fn archive_slot_that_is_a_directory_is_an_error_not_removed() {
        let tp = TempPath::new("dir-slot");
        std::fs::create_dir(tp.slot(3)).unwrap();
        let clock = FakeClock::default();
        let log = log_with(&tp, small_limits(300), &clock);
        let err = log.append(&ev(1, AuditKind::Armed, None, "x")).unwrap_err();
        assert_eq!(err.op, AuditOp::Inspect);
        assert!(tp.slot(3).is_dir(), "a directory is never removed");
        assert!(log.health().unavailable);
    }

    #[test]
    fn incomplete_trailing_record_is_trimmed_on_reopen() {
        let tp = TempPath::new("partial-tail");
        let good = format!(
            "{}\n",
            serde_json::to_string(&ev(1, AuditKind::Armed, None, "ok")).unwrap()
        );
        let fragment = "{\"ts_unix_ms\":2,\"ki";
        std::fs::write(tp.path(), format!("{good}{fragment}")).unwrap();
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        log.append(&ev(3, AuditKind::Disarmed, None, "after"))
            .unwrap();
        let events = read_events(tp.path());
        assert_eq!(events.len(), 2);
        log.maintain(4).unwrap();
        let last = read_events(tp.path()).pop().unwrap();
        assert_eq!(last.kind, AuditKind::Summary);
        assert_eq!(
            last.detail,
            format!(
                "audit retention: trimmed incomplete trailing record from active file; discarded_bytes={}",
                fragment.len()
            )
        );
    }

    #[test]
    fn each_storage_step_failure_is_typed_and_marks_health() {
        for op in [
            AuditOp::Inspect,
            AuditOp::Open,
            AuditOp::Flush,
            AuditOp::Sync,
        ] {
            let tp = TempPath::new("faults");
            let clock = FakeClock::default();
            let log = log_with(&tp, Limits::default(), &clock);
            log.append(&ev(1, AuditKind::Armed, None, "before"))
                .unwrap();
            inject(&log, op);
            let err = log
                .append(&ev(2, AuditKind::Armed, None, "fails"))
                .unwrap_err();
            assert_eq!(err.op, op);
            assert!(log.health().unavailable, "{op:?}");
            assert_eq!(log.health().last_failure, Some(op));
            // Rolled back: only the first record remains.
            assert_eq!(read_events(tp.path()).len(), 1, "{op:?}");
        }
    }

    #[test]
    fn rotation_failure_is_typed_and_never_overflows() {
        let tp = TempPath::new("rotate-fault");
        let clock = FakeClock::default();
        let log = log_with(&tp, small_limits(200), &clock);
        log.append(&ev(1, AuditKind::Armed, None, &"x".repeat(120)))
            .unwrap();
        inject(&log, AuditOp::Rotate);
        let err = log
            .append(&ev(2, AuditKind::Armed, None, &"y".repeat(120)))
            .unwrap_err();
        assert_eq!(err.op, AuditOp::Rotate);
        assert!(
            managed_sizes(&tp)[0].unwrap() <= 200,
            "no uncapped fallback"
        );
    }

    #[test]
    fn partial_write_is_rolled_back_or_repaired() {
        let tp = TempPath::new("partial");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        log.append(&ev(1, AuditKind::Armed, None, "first")).unwrap();
        let first_len = managed_sizes(&tp)[0].unwrap();

        // Partial write, rollback succeeds.
        inject(&log, AuditOp::Write);
        assert_eq!(
            log.append(&ev(2, AuditKind::Armed, None, "half"))
                .unwrap_err()
                .op,
            AuditOp::Write
        );
        assert_eq!(managed_sizes(&tp)[0], Some(first_len));

        // Partial write, rollback fails: the fragment is repaired before the
        // next successful append.
        inject(&log, AuditOp::Write);
        inject(&log, AuditOp::Rollback);
        assert!(log.append(&ev(3, AuditKind::Armed, None, "half")).is_err());
        assert!(managed_sizes(&tp)[0].unwrap() > first_len, "fragment left");
        clock.advance(Duration::from_secs(5));
        log.append(&ev(4, AuditKind::Armed, None, "next")).unwrap();
        let ts: Vec<i64> = read_events(tp.path())
            .iter()
            .map(|e| e.ts_unix_ms)
            .collect();
        assert_eq!(ts, vec![1, 4], "fragment trimmed, every line parses");
    }

    #[test]
    fn rate_limit_coalesces_per_kind_and_summarizes_after_silence() {
        let tp = TempPath::new("coalesce");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        let mut recorded = 0;
        let mut coalesced = 0;
        for _ in 0..100 {
            match log
                .append(&ev(
                    1000,
                    AuditKind::TxRequested,
                    Some("N0CALL"),
                    "callStation K1ABC",
                ))
                .unwrap()
            {
                AuditAppendOutcome::Recorded => recorded += 1,
                AuditAppendOutcome::Coalesced => coalesced += 1,
            }
        }
        assert_eq!((recorded, coalesced), (20, 80));
        // Another kind has its own bucket.
        assert_eq!(
            log.append(&ev(1001, AuditKind::Disarmed, None, "d"))
                .unwrap(),
            AuditAppendOutcome::Recorded
        );
        // Silence, then maintenance writes the exact summary.
        clock.advance(Duration::from_secs(5));
        log.maintain(6000).unwrap();
        let last = read_events(tp.path()).pop().unwrap();
        let line = serde_json::to_string(&last).unwrap();
        assert_eq!(
            line,
            r#"{"ts_unix_ms":6000,"kind":"Summary","operator_callsign":null,"detail":"audit summary: TxRequested coalesced=80 failed=0 first_ts=1000 last_ts=1000"}"#
        );
        // Counts were cleared: the next maintenance writes nothing.
        let before = read_lines(tp.path()).len();
        clock.advance(Duration::from_secs(5));
        log.maintain(11_000).unwrap();
        assert_eq!(read_lines(tp.path()).len(), before);
    }

    #[test]
    fn attempt_bound_holds_across_all_kinds_and_event_timestamps_never_refill() {
        let tp = TempPath::new("bound");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        let t_secs = 10u64;
        let mut attempts = 0u64;
        // 100 events per kind every 100 ms, with wild timestamps and strings.
        for step in 0..(t_secs * 10) {
            for (k, kind) in EVENT_KINDS.iter().enumerate() {
                for j in 0..100 {
                    let ts = if j % 2 == 0 {
                        i64::MAX - j
                    } else {
                        -j
                    };
                    let op = format!("OP{step}-{k}-{j}");
                    if log.append(&ev(ts, kind.clone(), Some(&op), &op)).unwrap()
                        == AuditAppendOutcome::Recorded
                    {
                        attempts += 1;
                    }
                }
            }
            clock.advance(Duration::from_millis(100));
        }
        assert!(attempts <= 120 + 60 * t_secs, "attempts={attempts}");
        // Fixed state: six buckets and six counters, whatever the strings.
        let st = log.shared.writer.lock().unwrap();
        assert_eq!(st.buckets.len(), 6);
        assert_eq!(st.pending.len(), 6);
        assert!(st.pending.iter().all(|c| c.coalesced > 0));
    }

    #[test]
    fn arm_disarm_churn_is_rate_limited_but_counted() {
        let tp = TempPath::new("churn");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        for i in 0..50 {
            log.append(&ev(i, AuditKind::Armed, Some("N0CALL"), "armed"))
                .unwrap();
            log.append(&ev(i, AuditKind::Disarmed, Some("N0CALL"), "disarm"))
                .unwrap();
        }
        clock.advance(Duration::from_secs(5));
        log.maintain(99).unwrap();
        let last = read_events(tp.path()).pop().unwrap();
        assert_eq!(
            last.detail,
            "audit summary: Armed coalesced=30 failed=0 first_ts=20 last_ts=49; \
             Disarmed coalesced=30 failed=0 first_ts=20 last_ts=49"
        );
        assert_eq!(last.operator_callsign, None);
    }

    #[test]
    fn failed_summary_keeps_counts_and_recovery_needs_a_recorded_summary() {
        let tp = TempPath::new("recover");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        log.append(&ev(5000, AuditKind::Armed, None, "ok")).unwrap();
        clock.advance(Duration::from_secs(1));
        inject(&log, AuditOp::Open);
        assert!(log
            .append(&ev(7000, AuditKind::Disarmed, None, "x"))
            .is_err());
        assert!(log.health().unavailable);

        // The summary write fails too: counts stay pending, health stays bad.
        clock.advance(Duration::from_secs(5));
        inject(&log, AuditOp::Open);
        assert!(log.maintain(9000).is_err());
        assert!(log.health().unavailable);
        // An ordinary successful append alone does not clear the episode.
        log.append(&ev(9500, AuditKind::LocalKill, None, "ok"))
            .unwrap();
        assert!(log.health().unavailable);

        clock.advance(Duration::from_secs(5));
        log.maintain(11_000).unwrap();
        assert!(
            !log.health().unavailable,
            "recovered after a recorded summary"
        );
        assert_eq!(log.health().failed_events, 1);
        let last = serde_json::to_string(&read_events(tp.path()).pop().unwrap()).unwrap();
        assert_eq!(
            last,
            r#"{"ts_unix_ms":11000,"kind":"Summary","operator_callsign":null,"detail":"audit summary: Disarmed coalesced=0 failed=1 first_ts=7000 last_ts=7000"}"#
        );
    }

    #[test]
    fn failure_stays_visible_for_one_interval_and_recurs_as_a_new_episode() {
        let tp = TempPath::new("episode");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        inject(&log, AuditOp::Open);
        assert!(log.append(&ev(1, AuditKind::Armed, None, "x")).is_err());
        // Maintenance right away writes the summary but keeps the warning up.
        log.maintain(2).unwrap();
        assert!(log.health().unavailable);
        // Within the interval, maintenance is throttled.
        clock.advance(Duration::from_secs(2));
        log.maintain(3).unwrap();
        assert!(log.health().unavailable);
        clock.advance(Duration::from_secs(3));
        log.maintain(4).unwrap();
        assert!(!log.health().unavailable);
        // A later failure opens a new episode.
        inject(&log, AuditOp::Open);
        assert!(log.append(&ev(5, AuditKind::Armed, None, "x")).is_err());
        assert!(log.health().unavailable);
    }

    #[test]
    fn over_allowance_events_while_failed_count_as_failed_not_coalesced() {
        let tp = TempPath::new("failed-vs-coalesced");
        std::fs::write(tp.path(), b"").unwrap();
        let clock = FakeClock::default();
        let log = AuditLog::with_options(
            tp.path().join("child.log"),
            Limits::default(),
            clock.as_clock(),
        );
        for i in 0..25 {
            let r = log.append(&ev(i, AuditKind::TxDenied, None, "x"));
            assert!(r.is_err(), "never disguised as success while failed");
        }
        let st = log.shared.writer.lock().unwrap();
        assert_eq!(st.pending[3].failed, 25);
        assert_eq!(st.pending[3].coalesced, 0);
        drop(st);
        assert_eq!(log.health().failed_events, 25);
    }

    #[test]
    fn maintenance_is_capped_at_five_summaries_and_never_recurses() {
        let tp = TempPath::new("five");
        for slot in 0..=ARCHIVE_COUNT {
            std::fs::write(tp.slot(slot), vec![b'z'; 1200]).unwrap();
        }
        let clock = FakeClock::default();
        let limits = Limits {
            burst: 1,
            ..small_limits(1000)
        };
        let log = log_with(&tp, limits, &clock);
        for i in 0..3 {
            log.append(&ev(i, AuditKind::TxRequested, None, "r"))
                .unwrap();
        }
        log.maintain(1).unwrap();
        let summaries: usize = (0..=ARCHIVE_COUNT)
            .filter_map(|s| std::fs::read_to_string(tp.slot(s)).ok())
            .map(|f| f.lines().filter(|l| l.contains("\"Summary\"")).count())
            .sum();
        assert_eq!(summaries, 5, "four retention notices + one count summary");

        // A failed summary write keeps its counts and is never summarized.
        log.append(&ev(9, AuditKind::Armed, None, "a")).unwrap();
        assert_eq!(
            log.append(&ev(10, AuditKind::Armed, None, "a")).unwrap(),
            AuditAppendOutcome::Coalesced
        );
        inject(&log, AuditOp::Open);
        clock.advance(Duration::from_secs(5));
        assert!(log.maintain(2).is_err());
        let st = log.shared.writer.lock().unwrap();
        assert_eq!(st.pending[0].coalesced, 1);
        assert!(st.pending[1..].iter().all(|c| !c.is_pending()));
        assert!(st.retention.iter().all(|r| !r.pending));
    }

    #[test]
    fn counters_saturate_with_an_explicit_marker() {
        let mut pending = [KindCounts::default(); 6];
        pending[2].coalesced = u64::MAX - 1;
        pending[2].add_coalesced(7);
        pending[2].add_coalesced(9);
        assert_eq!(pending[2].coalesced, u64::MAX);
        assert!(pending[2].saturated);
        assert!(counts_detail(&pending).ends_with("saturated=true"));
    }

    #[test]
    fn unused_healthy_log_does_no_io_on_maintenance() {
        let tp = TempPath::new("unused");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        log.maintain(1).unwrap();
        assert!(!tp.path().exists());
    }

    #[test]
    fn health_reads_do_not_wait_on_a_held_writer() {
        let tp = TempPath::new("health-lock");
        let log = AuditLog::new(tp.path().clone());
        let _writer = log.shared.writer.lock().unwrap();
        // Would deadlock if health() took the writer lock.
        assert!(!log.health().unavailable);
        // Maintenance skips a busy writer instead of waiting.
        assert_eq!(log.maintain(1), Ok(()));
    }

    /// Both ticket scenarios end to end: sustained traffic through shared
    /// clones rotates within the byte bound, an injected storage failure is
    /// surfaced and recovers, and every omitted event is accounted for.
    #[test]
    fn sustained_traffic_stays_bounded_and_failures_are_accounted() {
        let tp = TempPath::new("sustained");
        let clock = FakeClock::default();
        let max = 32_768;
        let log = log_with(&tp, small_limits(max), &clock);
        let clones: Vec<AuditLog> = (0..3).map(|_| log.clone()).collect();
        let (mut recorded, mut coalesced, mut failed) = (0u64, 0u64, 0u64);
        let mut saw_unavailable = false;
        for step in 0..600i64 {
            if step == 300 {
                inject(&log, AuditOp::Open);
            }
            for c in &clones {
                for kind in [AuditKind::TxRequested, AuditKind::TxDenied] {
                    for _ in 0..5 {
                        match c.append(&ev(step, kind.clone(), Some("N0CALL"), "flood")) {
                            Ok(AuditAppendOutcome::Recorded) => recorded += 1,
                            Ok(AuditAppendOutcome::Coalesced) => coalesced += 1,
                            Err(_) => failed += 1,
                        }
                    }
                }
            }
            saw_unavailable |= log.health().unavailable;
            clock.advance(Duration::from_millis(100));
            if step % 50 == 0 {
                let _ = clones[step as usize % 3].maintain(step * 100);
            }
        }
        clock.advance(Duration::from_secs(5));
        log.maintain(60_000).unwrap();
        assert!(saw_unavailable, "the injected failure was surfaced");
        assert!(!log.health().unavailable, "recovered with no new traffic");
        assert!(failed >= 1 && coalesced > 0);

        let sizes = managed_sizes(&tp);
        assert!(sizes[..=ARCHIVE_COUNT].iter().all(|s| s.unwrap() <= max));
        assert!(sizes[ARCHIVE_COUNT].is_some(), "rotated into every archive");
        assert_eq!(sizes[ARCHIVE_COUNT + 1], None);

        let events: Vec<AuditEvent> = (0..=ARCHIVE_COUNT)
            .rev()
            .flat_map(|slot| read_events(&tp.slot(slot)))
            .collect();
        let ordinary = events.iter().filter(|e| e.kind != AuditKind::Summary);
        assert_eq!(ordinary.count() as u64, recorded);
        let count = |key: &str| -> u64 {
            events
                .iter()
                .filter(|e| e.kind == AuditKind::Summary)
                .flat_map(|e| e.detail.split([' ', ';']))
                .filter_map(|tok| tok.strip_prefix(key))
                .map(|n| n.parse::<u64>().unwrap())
                .sum()
        };
        assert_eq!(count("coalesced="), coalesced);
        assert_eq!(count("failed="), failed);
        assert!(events
            .iter()
            .filter(|e| e.kind == AuditKind::Summary)
            .all(|e| e.operator_callsign.is_none()));
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn headless_error_is_reported_once_a_minute_and_recovery_once() {
        let buf = Captured::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let tp = TempPath::new("headless");
        let clock = FakeClock::default();
        let log = log_with(&tp, Limits::default(), &clock);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..10 {
                inject(&log, AuditOp::Open);
                let _ = log.append(&ev(1, AuditKind::TxDenied, None, "x"));
            }
            clock.advance(Duration::from_secs(59));
            inject(&log, AuditOp::Open);
            let _ = log.append(&ev(2, AuditKind::TxDenied, None, "x"));
            clock.advance(Duration::from_secs(1));
            inject(&log, AuditOp::Open);
            let _ = log.append(&ev(3, AuditKind::TxDenied, None, "x"));
            clock.advance(Duration::from_secs(5));
            log.maintain(4).unwrap();
            log.maintain(5).unwrap();
        });
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let errors: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(UNAVAILABLE_MESSAGE))
            .collect();
        assert_eq!(errors.len(), 2, "{text}");
        assert!(errors.iter().all(|l| l.contains("ERROR")), "{text}");
        assert!(errors[0].contains("failed_events=1"), "{text}");
        assert!(errors[1].contains("failed_events=12"), "{text}");
        assert_eq!(text.matches(RECOVERED_MESSAGE).count(), 1, "{text}");
        assert!(!log.health().unavailable);
    }

    #[test]
    fn default_audit_path_ends_with_expected_suffix() {
        let p = default_audit_path();
        assert!(p.ends_with("agent-audit.log"), "path = {}", p.display());
        assert!(
            p.to_string_lossy().contains(".pancetta"),
            "path = {}",
            p.display()
        );
    }
}
