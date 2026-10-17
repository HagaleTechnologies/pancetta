//! Durable pending-upload queue for per-QSO logbook uploads (PAN-57).
//!
//! Pure building blocks, with no tokio tasks and no coordinator dependency:
//!
//! - [`OutboxStore`] is the queue file, `~/.pancetta/upload_outbox.json`. An
//!   upload is written there before its first attempt and removed only when
//!   the service accepts it (or reports a duplicate). The file holds QSO data
//!   only: credentials are added by the clients at send time. A file that
//!   cannot be read is renamed to `<file>.corrupt-<unix-seconds>`, never
//!   deleted.
//! - [`RetrySchedule`] is the capped exponential backoff of
//!   `[network.reliability.retry]`, followed by a slow lane.
//! - [`Lane`] is the retry state of one destination. Callers always pass
//!   `now`, so every transition is testable without a clock.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Queue file name under `~/.pancetta/`.
pub const OUTBOX_FILE_NAME: &str = "upload_outbox.json";
/// The only queue-file layout this build reads and writes.
pub const OUTBOX_SCHEMA_VERSION: u32 = 1;
/// Most entries the queue holds; new uploads are refused past this.
pub const OUTBOX_MAX_ENTRIES: usize = 5_000;
/// Longest `last_error` kept in the file, in characters.
pub const LAST_ERROR_MAX_CHARS: usize = 300;

/// A service the queue delivers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadDestination {
    /// ClubLog real-time upload.
    #[serde(rename = "clublog")]
    ClubLog,
    /// QRZ Logbook `ACTION=INSERT`.
    QrzLogbook,
    /// eQSL.cc ADIF import.
    Eqsl,
    /// cqdx.io `POST /api/v1/qsos`.
    Cqdx,
}

impl UploadDestination {
    /// Every destination, in display order.
    pub const ALL: [Self; 4] = [Self::ClubLog, Self::QrzLogbook, Self::Eqsl, Self::Cqdx];

    /// Operator-facing service name.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClubLog => "ClubLog",
            Self::QrzLogbook => "QRZ Logbook",
            Self::Eqsl => "eQSL",
            Self::Cqdx => "cqdx.io",
        }
    }

    /// The config section holding this destination's credentials.
    pub fn config_section(self) -> &'static str {
        match self {
            Self::ClubLog => "[network.clublog]",
            Self::QrzLogbook => "[network.qrz_logbook]",
            Self::Eqsl => "[network.eqsl]",
            Self::Cqdx => "[network.cqdx]",
        }
    }
}

/// Whether an entry is waiting to be sent or held until the next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    /// Sent when its destination is ready.
    Pending,
    /// The service refused this record; released at the next start.
    Held,
}

/// One queued upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEntry {
    /// Queue-entry id.
    pub id: Uuid,
    /// The QSO this upload belongs to.
    pub qso_id: Uuid,
    /// Where it goes.
    pub destination: UploadDestination,
    /// The contacted station, for operator-facing text.
    pub call: String,
    /// What is sent: one ADIF record, or cqdx.io `QsoRecord` JSON. Never a
    /// credential.
    pub payload: String,
    /// When it entered the queue.
    pub enqueued_at: DateTime<Utc>,
    /// Attempts made so far.
    pub attempts: u32,
    /// When the last attempt finished.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// Why the last attempt failed (no credentials).
    pub last_error: Option<String>,
    /// Pending or held.
    pub state: EntryState,
}

/// An upload to add to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUpload {
    /// The QSO this upload belongs to.
    pub qso_id: Uuid,
    /// Where it goes.
    pub destination: UploadDestination,
    /// The contacted station.
    pub call: String,
    /// What is sent.
    pub payload: String,
}

/// What [`OutboxStore::add`] did with a batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AddResult {
    /// New entries queued.
    pub added: usize,
    /// Already queued for the same QSO and destination.
    pub already_queued: usize,
    /// Refused because the queue is full.
    pub refused_full: usize,
}

/// An unreadable queue file that was set aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    /// Where the unreadable file now is (its original path when the rename
    /// failed).
    pub moved_to: PathBuf,
    /// Why it could not be read.
    pub reason: String,
    /// The rename failed: the unreadable file is still at the queue path and
    /// must not be overwritten this session.
    pub rename_failed: bool,
}

/// Per-destination queue counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationCounts {
    /// Entries waiting to be sent.
    pub pending: usize,
    /// Entries held until the next start.
    pub held: usize,
    /// The newest failure text among pending entries.
    pub last_error: Option<String>,
    /// The call of the most recently held entry.
    pub last_held_call: Option<String>,
    /// The failure text of the most recently held entry.
    pub last_held_error: Option<String>,
}

#[derive(Serialize)]
struct FileRef<'a> {
    schema_version: u32,
    entries: &'a [OutboxEntry],
}

#[derive(Deserialize)]
struct FileOwned {
    schema_version: u32,
    entries: Vec<OutboxEntry>,
}

/// The queue file and its entries. One owner per process writes it.
#[derive(Debug)]
pub struct OutboxStore {
    path: PathBuf,
    capacity: usize,
    entries: Vec<OutboxEntry>,
}

impl OutboxStore {
    /// Open the queue at `path` with the default capacity.
    pub fn open(path: PathBuf) -> (Self, Option<Quarantine>) {
        Self::open_with_capacity(path, OUTBOX_MAX_ENTRIES)
    }

    /// Open the queue at `path`. A missing file is an empty queue and creates
    /// nothing; an unreadable one is renamed aside and reported.
    pub fn open_with_capacity(path: PathBuf, capacity: usize) -> (Self, Option<Quarantine>) {
        let mut store = Self {
            path,
            capacity,
            entries: Vec::new(),
        };
        let bytes = match std::fs::read(&store.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (store, None),
            Err(e) => {
                let q = store.quarantine(format!("could not read: {e}"));
                return (store, Some(q));
            }
        };
        let reason = match serde_json::from_slice::<FileOwned>(&bytes) {
            Ok(f) if f.schema_version == OUTBOX_SCHEMA_VERSION => {
                store.entries = f.entries;
                return (store, None);
            }
            Ok(f) => format!("unknown schema_version {}", f.schema_version),
            Err(e) => format!("not valid queue JSON: {e}"),
        };
        let q = store.quarantine(reason);
        (store, Some(q))
    }

    fn quarantine(&self, reason: String) -> Quarantine {
        let secs = Utc::now().timestamp();
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".corrupt-{secs}"));
        let target = PathBuf::from(name);
        match std::fs::rename(&self.path, &target) {
            Ok(()) => Quarantine {
                moved_to: target,
                reason,
                rename_failed: false,
            },
            Err(e) => Quarantine {
                moved_to: self.path.clone(),
                reason: format!("{reason}; could not move it aside: {e}"),
                rename_failed: true,
            },
        }
    }

    /// The queue as pretty JSON, ready for [`OutboxStore::write_atomic`].
    /// An error is returned rather than empty bytes, so a failed
    /// serialization can never overwrite the queue with nothing.
    pub fn to_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec_pretty(&FileRef {
            schema_version: OUTBOX_SCHEMA_VERSION,
            entries: &self.entries,
        })
    }

    /// Write `bytes` to `path` atomically: a `<file>.tmp` (owner-only on
    /// Unix), flushed to disk, then renamed over `path`.
    pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    }

    /// The queue file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue a batch. An upload already queued for the same QSO and
    /// destination is skipped; past capacity, new uploads are refused.
    pub fn add(&mut self, batch: Vec<NewUpload>, now: DateTime<Utc>) -> AddResult {
        let mut result = AddResult::default();
        for up in batch {
            if self
                .entries
                .iter()
                .any(|e| e.qso_id == up.qso_id && e.destination == up.destination)
            {
                result.already_queued += 1;
                continue;
            }
            if self.entries.len() >= self.capacity {
                result.refused_full += 1;
                continue;
            }
            self.entries.push(OutboxEntry {
                id: Uuid::new_v4(),
                qso_id: up.qso_id,
                destination: up.destination,
                call: up.call,
                payload: up.payload,
                enqueued_at: now,
                attempts: 0,
                last_attempt_at: None,
                last_error: None,
                state: EntryState::Pending,
            });
            result.added += 1;
        }
        result
    }

    /// Return every held entry to pending (done once at start). Returns how
    /// many were released.
    pub fn release_held(&mut self) -> usize {
        let mut n = 0;
        for e in &mut self.entries {
            if e.state == EntryState::Held {
                e.state = EntryState::Pending;
                n += 1;
            }
        }
        n
    }

    /// The next pending entry for `d`: never-attempted entries first, in
    /// queue order, then the one tried longest ago.
    pub fn next_for(&self, d: UploadDestination) -> Option<&OutboxEntry> {
        let mut pending = self
            .entries
            .iter()
            .filter(|e| e.destination == d && e.state == EntryState::Pending);
        let mut best: Option<&OutboxEntry> = None;
        for e in pending.by_ref() {
            if e.attempts == 0 {
                return Some(e);
            }
            best = match best {
                Some(b) if b.last_attempt_at <= e.last_attempt_at => Some(b),
                _ => Some(e),
            };
        }
        best
    }

    /// Look an entry up by id.
    pub fn get(&self, id: Uuid) -> Option<&OutboxEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The service accepted the entry: remove it.
    pub fn delivered(&mut self, id: Uuid) -> Option<OutboxEntry> {
        let pos = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(pos))
    }

    /// Record a failed attempt; the entry stays pending.
    pub fn record_failure(&mut self, id: Uuid, now: DateTime<Utc>, text: &str) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.attempts = e.attempts.saturating_add(1);
            e.last_attempt_at = Some(now);
            e.last_error = Some(truncate_chars(text, LAST_ERROR_MAX_CHARS));
        }
    }

    /// Record a refused attempt and hold the entry until the next start.
    pub fn hold(&mut self, id: Uuid, now: DateTime<Utc>, text: &str) {
        self.record_failure(id, now, text);
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.state = EntryState::Held;
        }
    }

    /// Pending and held counts, and the latest failure texts, for `d`.
    pub fn counts(&self, d: UploadDestination) -> DestinationCounts {
        let mut c = DestinationCounts::default();
        let mut last_pending: Option<&OutboxEntry> = None;
        let mut last_held: Option<&OutboxEntry> = None;
        for e in self.entries.iter().filter(|e| e.destination == d) {
            let slot = match e.state {
                EntryState::Pending => {
                    c.pending += 1;
                    &mut last_pending
                }
                EntryState::Held => {
                    c.held += 1;
                    &mut last_held
                }
            };
            if e.last_error.is_some()
                && slot.is_none_or(|s: &OutboxEntry| s.last_attempt_at <= e.last_attempt_at)
            {
                *slot = Some(e);
            }
        }
        c.last_error = last_pending.and_then(|e| e.last_error.clone());
        if let Some(h) = last_held {
            c.last_held_call = Some(h.call.clone());
            c.last_held_error = h.last_error.clone();
        }
        c
    }

    /// Entries in the queue.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, in queue order.
    pub fn entries(&self) -> &[OutboxEntry] {
        &self.entries
    }
}

/// Cut `s` to `max` characters, marking a cut with `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// How long to wait before the next attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDelay {
    /// A quick retry inside the burst.
    Burst(Duration),
    /// The burst is used up: the slow-lane interval.
    Slow(Duration),
}

/// Capped exponential backoff, then a slow lane.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrySchedule {
    max_attempts: u32,
    base_ms: f64,
    max_ms: f64,
    multiplier: f64,
    jitter: f64,
    slow: Duration,
}

impl RetrySchedule {
    /// Build from the `[network.reliability.retry]` fields and the slow-lane
    /// interval.
    pub fn new(
        max_attempts: u32,
        base_delay_ms: u64,
        max_delay_ms: u64,
        backoff_multiplier: f32,
        jitter_factor: f32,
        slow_retry: Duration,
    ) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            base_ms: base_delay_ms as f64,
            max_ms: max_delay_ms as f64,
            multiplier: backoff_multiplier as f64,
            jitter: jitter_factor as f64,
            slow: slow_retry,
        }
    }

    /// Wait after the `failures`-th consecutive failure (1-based). `unit` in
    /// [-1, 1] picks the jitter.
    pub fn delay_after(&self, failures: u32, unit: f64) -> RetryDelay {
        if failures >= self.max_attempts {
            return RetryDelay::Slow(self.slow);
        }
        let exp = i32::try_from(failures.saturating_sub(1)).unwrap_or(i32::MAX);
        let raw = self.base_ms * self.multiplier.powi(exp);
        let capped = raw.min(self.max_ms).max(0.0);
        let unit = if unit.is_finite() {
            unit.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        // Cap again after the jitter: `max_delay_ms` bounds the actual wait.
        let jittered = (capped * (1.0 + self.jitter * unit))
            .max(0.0)
            .min(self.max_ms);
        let ms = if jittered.is_finite() {
            jittered.round() as u64
        } else {
            self.max_ms as u64
        };
        RetryDelay::Burst(Duration::from_millis(ms))
    }
}

/// Retry state of one destination.
#[derive(Debug, Clone, Default)]
pub struct Lane {
    failures: u32,
    next_due: Option<Instant>,
    in_flight: bool,
    slow: bool,
    login_rejected: Option<String>,
}

impl Lane {
    /// An attempt may start now.
    pub fn ready(&self, now: Instant) -> bool {
        !self.in_flight
            && self.login_rejected.is_none()
            && self.next_due.is_none_or(|due| now >= due)
    }

    /// An attempt started.
    pub fn started(&mut self) {
        self.in_flight = true;
    }

    /// The service accepted an upload. Returns true when the lane was in the
    /// slow lane (a recovery).
    pub fn delivered(&mut self, now: Instant, spacing: Duration) -> bool {
        let recovered = self.slow;
        self.in_flight = false;
        self.failures = 0;
        self.slow = false;
        self.next_due = Some(now + spacing);
        recovered
    }

    /// A transient failure: wait by the schedule.
    pub fn failed(&mut self, now: Instant, s: &RetrySchedule, unit: f64) -> RetryDelay {
        self.in_flight = false;
        self.failures = self.failures.saturating_add(1);
        let delay = s.delay_after(self.failures, unit);
        let wait = match delay {
            RetryDelay::Burst(d) => d,
            RetryDelay::Slow(d) => {
                self.slow = true;
                d
            }
        };
        self.next_due = Some(now + wait);
        delay
    }

    /// The service refused one record but is answering: keep moving.
    pub fn record_rejected(&mut self, now: Instant, spacing: Duration) {
        self.in_flight = false;
        self.failures = 0;
        self.slow = false;
        self.next_due = Some(now + spacing);
    }

    /// The service refused the account: stop for this session.
    pub fn login_rejected(&mut self, text: String) {
        self.in_flight = false;
        self.login_rejected = Some(text);
    }

    /// The quick retries are used up and the lane waits on the slow interval.
    pub fn is_retrying_slowly(&self) -> bool {
        self.slow
    }

    /// Consecutive transient failures since the last answer from the
    /// service (across every entry of this destination).
    pub fn consecutive_failures(&self) -> u32 {
        self.failures
    }

    /// Why the service refused the account, if it did.
    pub fn login_rejection(&self) -> Option<&str> {
        self.login_rejected.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> RetrySchedule {
        RetrySchedule::new(3, 1000, 10_000, 2.0, 0.1, Duration::from_secs(300))
    }

    // ----- Retry schedule ---------------------------------------------------

    #[test]
    fn default_schedule_is_two_quick_retries_then_slow_lane() {
        let s = defaults();
        assert_eq!(
            s.delay_after(1, 0.0),
            RetryDelay::Burst(Duration::from_millis(1000))
        );
        assert_eq!(
            s.delay_after(2, 0.0),
            RetryDelay::Burst(Duration::from_millis(2000))
        );
        assert_eq!(
            s.delay_after(3, 0.0),
            RetryDelay::Slow(Duration::from_secs(300))
        );
        assert_eq!(
            s.delay_after(9, 0.0),
            RetryDelay::Slow(Duration::from_secs(300))
        );
    }

    #[test]
    fn burst_waits_are_capped() {
        let s = RetrySchedule::new(6, 1000, 5000, 2.0, 0.0, Duration::from_secs(300));
        let ms = |n| match s.delay_after(n, 0.0) {
            RetryDelay::Burst(d) => d.as_millis(),
            RetryDelay::Slow(_) => panic!("slow at {n}"),
        };
        assert_eq!(
            [ms(1), ms(2), ms(3), ms(4), ms(5)],
            [1000, 2000, 4000, 5000, 5000]
        );
        assert_eq!(
            s.delay_after(6, 0.0),
            RetryDelay::Slow(Duration::from_secs(300))
        );
    }

    #[test]
    fn jitter_stays_within_its_fraction() {
        let s = defaults();
        assert_eq!(
            s.delay_after(1, -1.0),
            RetryDelay::Burst(Duration::from_millis(900))
        );
        assert_eq!(
            s.delay_after(1, 1.0),
            RetryDelay::Burst(Duration::from_millis(1100))
        );
        assert_eq!(
            s.delay_after(1, 0.5),
            RetryDelay::Burst(Duration::from_millis(1050))
        );
    }

    #[test]
    fn jitter_never_pushes_a_wait_past_the_cap() {
        let s = RetrySchedule::new(6, 1000, 10_000, 2.0, 0.1, Duration::from_secs(300));
        // 1000 × 2^4 = 16000 → capped at 10000; +10% jitter must not exceed it.
        assert_eq!(
            s.delay_after(5, 1.0),
            RetryDelay::Burst(Duration::from_millis(10_000))
        );
        assert_eq!(
            s.delay_after(5, -1.0),
            RetryDelay::Burst(Duration::from_millis(9000))
        );
    }

    #[test]
    fn single_attempt_goes_straight_to_slow_lane() {
        let s = RetrySchedule::new(1, 1000, 10_000, 2.0, 0.1, Duration::from_secs(300));
        assert_eq!(
            s.delay_after(1, 0.0),
            RetryDelay::Slow(Duration::from_secs(300))
        );
    }

    #[test]
    fn huge_multiplier_does_not_overflow() {
        let s = RetrySchedule::new(100, 1000, 10_000, 1e30, 0.1, Duration::from_secs(300));
        match s.delay_after(50, 1.0) {
            RetryDelay::Burst(d) => assert!(d <= Duration::from_millis(11_000), "{d:?}"),
            RetryDelay::Slow(_) => {}
        }
    }

    // ----- Lane -------------------------------------------------------------

    #[test]
    fn fresh_lane_is_ready() {
        assert!(Lane::default().ready(Instant::now()));
    }

    #[test]
    fn in_flight_lane_is_not_ready() {
        let mut lane = Lane::default();
        lane.started();
        assert!(!lane.ready(Instant::now()));
    }

    #[test]
    fn transient_failure_delays_the_lane_by_the_schedule() {
        let s = defaults();
        let now = Instant::now();
        let mut lane = Lane::default();
        lane.started();
        let d = lane.failed(now, &s, 0.0);
        assert_eq!(d, RetryDelay::Burst(Duration::from_millis(1000)));
        assert!(!lane.ready(now + Duration::from_millis(999)));
        assert!(lane.ready(now + Duration::from_millis(1000)));
        assert!(!lane.is_retrying_slowly());
    }

    #[test]
    fn delivery_resets_failures_and_spaces_the_next_send() {
        let s = defaults();
        let now = Instant::now();
        let mut lane = Lane::default();
        lane.started();
        lane.failed(now, &s, 0.0);
        lane.started();
        let spacing = Duration::from_secs(1);
        assert!(!lane.delivered(now, spacing));
        assert!(!lane.ready(now + Duration::from_millis(999)));
        assert!(lane.ready(now + spacing));
        // Failures were reset: the next failure is the first again.
        lane.started();
        assert_eq!(
            lane.failed(now, &s, 0.0),
            RetryDelay::Burst(Duration::from_millis(1000))
        );
    }

    #[test]
    fn delivery_after_slow_lane_reports_recovery() {
        let s = defaults();
        let now = Instant::now();
        let mut lane = Lane::default();
        for _ in 0..3 {
            lane.started();
            lane.failed(now, &s, 0.0);
        }
        assert!(lane.is_retrying_slowly());
        assert_eq!(lane.consecutive_failures(), 3);
        lane.started();
        assert!(lane.delivered(now, Duration::from_secs(1)));
        assert_eq!(lane.consecutive_failures(), 0);
        assert!(!lane.is_retrying_slowly());
    }

    #[test]
    fn login_rejected_lane_never_becomes_ready_again() {
        let mut lane = Lane::default();
        lane.started();
        lane.login_rejected("bad key".to_string());
        assert_eq!(lane.login_rejection(), Some("bad key"));
        assert!(!lane.ready(Instant::now() + Duration::from_secs(86_400)));
    }

    #[test]
    fn record_rejection_keeps_the_lane_moving() {
        let s = defaults();
        let now = Instant::now();
        let mut lane = Lane::default();
        lane.started();
        lane.failed(now, &s, 0.0);
        lane.started();
        let spacing = Duration::from_millis(5);
        lane.record_rejected(now, spacing);
        assert!(lane.ready(now + spacing));
        lane.started();
        assert_eq!(
            lane.failed(now, &s, 0.0),
            RetryDelay::Burst(Duration::from_millis(1000))
        );
    }

    // ----- Store ------------------------------------------------------------

    fn up(qso: Uuid, d: UploadDestination, call: &str) -> NewUpload {
        NewUpload {
            qso_id: qso,
            destination: d,
            call: call.to_string(),
            payload: format!("<CALL:{}>{}<EOR>", call.len(), call),
        }
    }

    fn path_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(OUTBOX_FILE_NAME)
    }

    #[test]
    fn missing_file_opens_empty_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let (store, q) = OutboxStore::open(path.clone());
        assert!(q.is_none());
        assert_eq!(store.len(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn add_persist_reopen_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let (mut store, _) = OutboxStore::open(path.clone());
        let now = Utc::now();
        store.add(
            vec![
                up(Uuid::new_v4(), UploadDestination::ClubLog, "K1ABC"),
                up(Uuid::new_v4(), UploadDestination::QrzLogbook, "JA1XYZ"),
            ],
            now,
        );
        let id = store.entries()[1].id;
        store.hold(id, now, "QRZ Logbook insert failed: Missing band");
        OutboxStore::write_atomic(store.path(), &store.to_bytes().unwrap()).unwrap();
        let (again, q) = OutboxStore::open(path.clone());
        assert!(q.is_none());
        assert_eq!(again.entries(), store.entries());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1"), "{text}");
        assert!(text.contains("\"destination\": \"qrz_logbook\""), "{text}");
        assert!(text.contains("\"destination\": \"clublog\""), "{text}");
        assert!(text.contains("\"state\": \"held\""), "{text}");
    }

    #[test]
    fn garbled_file_is_quarantined_with_its_bytes_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        std::fs::write(&path, b"{ not json").unwrap();
        let (store, q) = OutboxStore::open(path.clone());
        let q = q.expect("quarantined");
        assert!(!q.rename_failed);
        assert_eq!(store.len(), 0);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&q.moved_to).unwrap(), b"{ not json");
        assert!(q
            .moved_to
            .to_string_lossy()
            .contains("upload_outbox.json.corrupt-"));
    }

    #[test]
    fn unknown_schema_version_is_quarantined_not_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let body = br#"{"schema_version": 2, "entries": []}"#;
        std::fs::write(&path, body).unwrap();
        let (store, q) = OutboxStore::open(path.clone());
        let q = q.expect("quarantined");
        assert!(q.reason.contains("schema_version 2"), "{}", q.reason);
        assert_eq!(store.len(), 0);
        assert_eq!(std::fs::read(&q.moved_to).unwrap(), body);
    }

    #[test]
    fn leftover_temp_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        std::fs::write(dir.path().join("upload_outbox.json.tmp"), b"garbage").unwrap();
        let (store, q) = OutboxStore::open(path);
        assert!(q.is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn same_qso_and_destination_is_not_queued_twice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open(path_in(&dir));
        let qso = Uuid::new_v4();
        let r1 = store.add(vec![up(qso, UploadDestination::Eqsl, "K1ABC")], Utc::now());
        let r2 = store.add(
            vec![
                up(qso, UploadDestination::Eqsl, "K1ABC"),
                up(qso, UploadDestination::ClubLog, "K1ABC"),
            ],
            Utc::now(),
        );
        assert_eq!(r1.added, 1);
        assert_eq!(r2.added, 1);
        assert_eq!(r2.already_queued, 1);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn full_store_refuses_new_entries_and_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open_with_capacity(path_in(&dir), 2);
        let r = store.add(
            vec![
                up(Uuid::new_v4(), UploadDestination::ClubLog, "A1A"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "B1B"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "C1C"),
            ],
            Utc::now(),
        );
        assert_eq!(r.added, 2);
        assert_eq!(r.refused_full, 1);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn next_for_prefers_untried_then_least_recently_tried() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open(path_in(&dir));
        let t0 = Utc::now();
        store.add(
            vec![
                up(Uuid::new_v4(), UploadDestination::ClubLog, "A1A"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "B1B"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "C1C"),
            ],
            t0,
        );
        let ids: Vec<Uuid> = store.entries().iter().map(|e| e.id).collect();
        assert_eq!(
            store.next_for(UploadDestination::ClubLog).unwrap().id,
            ids[0]
        );
        store.record_failure(ids[0], t0 + chrono::Duration::seconds(10), "x");
        assert_eq!(
            store.next_for(UploadDestination::ClubLog).unwrap().id,
            ids[1]
        );
        store.record_failure(ids[1], t0 + chrono::Duration::seconds(5), "x");
        assert_eq!(
            store.next_for(UploadDestination::ClubLog).unwrap().id,
            ids[2]
        );
        store.record_failure(ids[2], t0 + chrono::Duration::seconds(20), "x");
        // All tried: the one tried longest ago (B at +5 s).
        assert_eq!(
            store.next_for(UploadDestination::ClubLog).unwrap().id,
            ids[1]
        );
        assert!(store.next_for(UploadDestination::Eqsl).is_none());
    }

    #[test]
    fn held_entries_are_skipped_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open(path_in(&dir));
        store.add(
            vec![up(Uuid::new_v4(), UploadDestination::Eqsl, "A1A")],
            Utc::now(),
        );
        let id = store.entries()[0].id;
        store.hold(id, Utc::now(), "HTTP 400");
        assert!(store.next_for(UploadDestination::Eqsl).is_none());
        assert_eq!(store.release_held(), 1);
        assert_eq!(store.next_for(UploadDestination::Eqsl).unwrap().id, id);
    }

    #[test]
    fn last_error_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open(path_in(&dir));
        store.add(
            vec![up(Uuid::new_v4(), UploadDestination::ClubLog, "A1A")],
            Utc::now(),
        );
        let id = store.entries()[0].id;
        store.record_failure(id, Utc::now(), &"x".repeat(1000));
        let err = store.entries()[0].last_error.clone().unwrap();
        assert_eq!(err.chars().count(), LAST_ERROR_MAX_CHARS + 1);
        assert!(err.ends_with('…'));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir);
        let (mut store, _) = OutboxStore::open(path.clone());
        store.add(
            vec![up(Uuid::new_v4(), UploadDestination::ClubLog, "A1A")],
            Utc::now(),
        );
        OutboxStore::write_atomic(&path, &store.to_bytes().unwrap()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn counts_report_pending_held_and_last_error_per_destination() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = OutboxStore::open(path_in(&dir));
        let t0 = Utc::now();
        store.add(
            vec![
                up(Uuid::new_v4(), UploadDestination::ClubLog, "A1A"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "B1B"),
                up(Uuid::new_v4(), UploadDestination::ClubLog, "C1C"),
                up(Uuid::new_v4(), UploadDestination::Eqsl, "D1D"),
            ],
            t0,
        );
        let ids: Vec<Uuid> = store.entries().iter().map(|e| e.id).collect();
        store.record_failure(ids[0], t0, "older");
        store.record_failure(ids[1], t0 + chrono::Duration::seconds(1), "newer");
        store.hold(ids[2], t0, "HTTP 400");
        let c = store.counts(UploadDestination::ClubLog);
        assert_eq!(c.pending, 2);
        assert_eq!(c.held, 1);
        assert_eq!(c.last_error.as_deref(), Some("newer"));
        assert_eq!(c.last_held_call.as_deref(), Some("C1C"));
        assert_eq!(c.last_held_error.as_deref(), Some("HTTP 400"));
        let e = store.counts(UploadDestination::Eqsl);
        assert_eq!((e.pending, e.held, e.last_error), (1, 0, None));
        assert_eq!(
            store.counts(UploadDestination::Cqdx),
            DestinationCounts::default()
        );
    }
}
