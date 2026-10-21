//! PAN-122: the durable local TX log — one JSON line per frame whose audio
//! the TX worker enqueued, appended with write_all + sync_data by a single
//! writer thread. Separate from the TUI's best-effort `TxFrameLogged` event
//! and from the remote agent's `agent-audit.log`.
//!
//! The TX worker calls [`TxFrameLog::append`] directly at Step 7 — never via
//! the message bus, whose sends are best-effort and drop on a full, missing
//! or disconnected destination (the headless loss this module exists to
//! close). The append waits at most a bounded time for the writer's fsync
//! acknowledgement, so a stalled disk can never hold PTT.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::message_bus::TxOrigin;

/// File name under `~/.pancetta/`.
pub(crate) const TX_LOG_FILE_NAME: &str = "tx-log.jsonl";
/// Record schema version written as `"v"`.
pub(crate) const TX_LOG_SCHEMA_VERSION: u8 = 1;
/// Rotate before an append that would push the active file past this size.
pub(crate) const TX_LOG_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Rotated archives kept (`tx-log.jsonl.1` newest … `.7` oldest).
pub(crate) const TX_LOG_ARCHIVES: usize = 7;
/// How long the TX worker waits for the writer's fsync acknowledgement
/// before carrying on into Step 8 (the job stays queued and is still written).
pub(crate) const TX_LOG_ACK_WAIT: Duration = Duration::from_millis(250);

/// `tracing` target for every line this module emits.
const LOG_TARGET: &str = "pancetta::tx_log";

/// One transmitted frame, serialized as one JSON line (schema v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TxFrameRecord {
    pub(crate) v: u8,
    /// When the frame's audio was handed to the audio output — the same
    /// instant as the TUI's `TxFrameLogged.timestamp`.
    pub(crate) utc: chrono::DateTime<chrono::Utc>,
    /// `"FT8"`, `"FT4"` or `"FT2"`.
    pub(crate) mode: String,
    /// Effective (split-aware) TX dial; `None` while the dial is unknown.
    pub(crate) dial_hz: Option<u64>,
    /// Audio offset; RF = `dial_hz + offset_hz`.
    pub(crate) offset_hz: f64,
    pub(crate) text: String,
    pub(crate) origin: TxOrigin,
    pub(crate) qso_id: Option<String>,
}

impl TxFrameRecord {
    /// `tx_dial_hz == 0` (the unknown-dial sentinel) records `null`.
    pub(crate) fn new(
        utc: chrono::DateTime<chrono::Utc>,
        protocol: pancetta_ft8::Protocol,
        tx_dial_hz: u64,
        offset_hz: f64,
        text: String,
        origin: TxOrigin,
        qso_id: Option<String>,
    ) -> Self {
        Self {
            v: TX_LOG_SCHEMA_VERSION,
            utc,
            mode: protocol.to_string(),
            dial_hz: (tx_dial_hz != 0).then_some(tx_dial_hz),
            offset_hz,
            text,
            origin,
            qso_id,
        }
    }

    /// `<utc> <mode> <offset> Hz "<text>"` — the frame copy an ERROR line carries.
    fn summary(&self) -> String {
        format!(
            "{} {} {:.0} Hz \"{}\"",
            self.utc
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            self.mode,
            self.offset_hz,
            self.text
        )
    }
}

fn frames_summary(records: &[TxFrameRecord]) -> String {
    records
        .iter()
        .map(TxFrameRecord::summary)
        .collect::<Vec<_>>()
        .join(", ")
}

fn log_failed(path: &Path, records: &[TxFrameRecord], cause: &dyn std::fmt::Display) {
    error!(
        target: LOG_TARGET,
        "TX log: could not record {} frame(s) to {}: {}. Frames: [{}]",
        records.len(),
        path.display(),
        cause,
        frames_summary(records)
    );
}

/// Size-based rotation of the active file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RotationPolicy {
    pub(crate) max_file_bytes: u64,
    pub(crate) archives: usize,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: TX_LOG_MAX_FILE_BYTES,
            archives: TX_LOG_ARCHIVES,
        }
    }
}

/// Blocking: returns only after the bytes are written and `sync_data` succeeded.
///
/// The whole batch is serialized into one buffer and written with a single
/// `write_all` (no userspace buffer survives the call, and a bundle stays
/// contiguous). The file is opened per batch, so an operator moving or
/// deleting it never strands a stale handle. Rotation happens first when
/// this batch would push a non-empty active file past `max_file_bytes`, so a
/// batch is never split across files and an empty file is never rotated.
pub(crate) fn append_batch(
    path: &Path,
    policy: RotationPolicy,
    records: &[TxFrameRecord],
) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut buf = Vec::with_capacity(records.len() * 192);
    for record in records {
        serde_json::to_writer(&mut buf, record).map_err(io::Error::other)?;
        buf.push(b'\n');
    }

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let existing_len = match fs::metadata(path) {
        Ok(meta) => Some(meta.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut created = existing_len.is_none();
    let len = existing_len.unwrap_or(0);
    if len > 0 && len + buf.len() as u64 > policy.max_file_bytes {
        // A failed rotation must not cost the record: keep appending to the
        // (oversized) active file and retry the rotation on the next batch.
        match rotate(path, policy.archives) {
            Ok(()) => created = true,
            Err(e) => warn!(
                target: LOG_TARGET,
                "TX log: rotation of {} failed ({}); appending to the active file",
                path.display(),
                e
            ),
        }
    }

    let mut file = ActiveFile {
        file: OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?,
        path,
    };
    append_whole_lines(&mut file, &buf)?;
    file.file.sync_data()?;
    if created {
        sync_parent_dir(path);
    }
    Ok(())
}

/// The file operations [`append_whole_lines`] needs; a trait so a test can
/// inject a file whose write fails part-way through.
trait LogFile: Read + Write + Seek {
    fn truncate_to(&mut self, len: u64) -> io::Result<()>;
}

/// The open active file. It is opened for append, which on Windows grants no
/// `FILE_WRITE_DATA`, so a truncation that the append handle cannot do falls
/// back to a short-lived write handle.
struct ActiveFile<'a> {
    file: fs::File,
    path: &'a Path,
}

impl Read for ActiveFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for ActiveFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for ActiveFile<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl LogFile for ActiveFile<'_> {
    fn truncate_to(&mut self, len: u64) -> io::Result<()> {
        self.file
            .set_len(len)
            .or_else(|_| OpenOptions::new().write(true).open(self.path)?.set_len(len))
    }
}

/// Append `buf` (whole `\n`-terminated lines) so the file stays a sequence
/// of whole JSON lines.
///
/// Review (PR #468): a `write_all` that fails part-way (disk full) leaves a
/// fragment at EOF that the next append would extend into one invalid line.
/// So a failed write is rolled back to the pre-append length; and should
/// that rollback fail too (or a crash tore the tail), the next append sees a
/// tail without a trailing newline and terminates it first, confining the
/// damage to the torn line and keeping every later record parseable.
fn append_whole_lines<F: LogFile>(file: &mut F, buf: &[u8]) -> io::Result<()> {
    let start = file.seek(SeekFrom::End(0))?;
    let mut torn_tail = false;
    if start > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        torn_tail = last[0] != b'\n';
    }
    let written = (|| {
        if torn_tail {
            file.write_all(b"\n")?;
        }
        file.write_all(buf)
    })();
    if let Err(e) = written {
        if let Err(rollback) = file.truncate_to(start) {
            warn!(
                target: LOG_TARGET,
                "TX log: could not roll back a partial append ({}); the next append will terminate the torn line",
                rollback
            );
        }
        return Err(e);
    }
    Ok(())
}

/// Best-effort: make a newly created directory entry durable (Unix only;
/// Windows has no directory handle to fsync).
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Move the active file into the archive chain, non-destructively on failure.
///
/// The active file is moved aside to `tx-log.jsonl.rotating` FIRST — the step
/// a Windows sharing lock (another process holding the file open without
/// delete sharing) blocks — so a blocked rotation fails before any archive is
/// touched. Only then are the archives shifted; a later failure undoes the
/// shifts already made and moves the active file back. A rotation that keeps
/// failing therefore drops at most the one oldest archive it was meant to
/// drop, never walks the whole history off the end. A `.rotating` file left
/// by an interrupted earlier rotation still holds records, so it is archived
/// before the active file is moved aside.
fn rotate(path: &Path, archives: usize) -> io::Result<()> {
    if archives == 0 {
        return remove_if_exists(path);
    }
    let staged = staging_path(path);
    if staged.exists() {
        push_archive(path, &staged, archives)?;
    }
    fs::rename(path, &staged)?;
    if let Err(e) = push_archive(path, &staged, archives) {
        // Restore the active file; if even that fails, the staged file is
        // archived by the next rotation (above), so nothing is lost.
        let _ = fs::rename(&staged, path);
        return Err(e);
    }
    Ok(())
}

/// Drop `.archives`, shift `.1 … .archives-1` up by one, then move `src` to
/// `.1`. On failure every shift already made is undone, so the chain is left
/// as it was (minus the dropped oldest, whose slot then stays free and makes a
/// retry drop nothing more).
fn push_archive(path: &Path, src: &Path, archives: usize) -> io::Result<()> {
    remove_if_exists(&archive_path(path, archives))?;
    let mut shifted = Vec::new();
    let mut result = Ok(());
    for n in (1..archives).rev() {
        let from = archive_path(path, n);
        if from.exists() {
            if let Err(e) = fs::rename(&from, archive_path(path, n + 1)) {
                result = Err(e);
                break;
            }
            shifted.push(n);
        }
    }
    if result.is_ok() {
        result = fs::rename(src, archive_path(path, 1));
    }
    if result.is_err() {
        // `shifted` is descending; undo ascending so each target slot is free.
        for n in shifted.into_iter().rev() {
            let _ = fs::rename(archive_path(path, n + 1), archive_path(path, n));
        }
    }
    result
}

/// `tx-log.jsonl.rotating` — the active file mid-rotation.
fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".rotating");
    PathBuf::from(name)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `tx-log.jsonl.N`.
fn archive_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// What [`TxFrameLog::append`] observed within its wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxLogAppend {
    /// Written and `sync_data` succeeded.
    Durable,
    /// Not acknowledged within the wait; still queued and will be written.
    Pending,
    /// Could not be written (already logged at ERROR with the frame text).
    Failed,
    /// Replay mode: nothing is recorded.
    Disabled,
}

/// Test-only latch the writer thread waits on before each job, so a test
/// can hold the writer genuinely blocked (not instantly failing).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct WriterGate {
    open: Mutex<bool>,
    cv: Condvar,
}

#[cfg(test)]
impl WriterGate {
    pub(crate) fn closed() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
    }
}

struct Job {
    records: Vec<TxFrameRecord>,
    done: tokio::sync::oneshot::Sender<io::Result<()>>,
}

struct Writer {
    path: PathBuf,
    /// `None` when the writer thread could not be spawned.
    jobs: Option<mpsc::Sender<Job>>,
}

/// Handle to the single `pancetta-tx-log` writer thread. Cheap to share via
/// `Arc`; the thread exits once the last handle is dropped and its queue
/// is drained.
pub(crate) struct TxFrameLog {
    /// `None` = disabled (replay).
    writer: Option<Writer>,
}

impl TxFrameLog {
    /// Spawns the `pancetta-tx-log` writer thread with the default rotation.
    pub(crate) fn open(path: PathBuf) -> Self {
        Self::open_with(path, RotationPolicy::default())
    }

    pub(crate) fn open_with(path: PathBuf, policy: RotationPolicy) -> Self {
        Self::spawn(
            path,
            policy,
            #[cfg(test)]
            None,
        )
    }

    /// [`Self::open_with`] whose writer waits on `gate` before every job.
    #[cfg(test)]
    pub(crate) fn open_gated(path: PathBuf, policy: RotationPolicy, gate: Arc<WriterGate>) -> Self {
        Self::spawn(path, policy, Some(gate))
    }

    fn spawn(
        path: PathBuf,
        policy: RotationPolicy,
        #[cfg(test)] gate: Option<Arc<WriterGate>>,
    ) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let thread_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("pancetta-tx-log".to_string())
            .spawn(move || {
                for job in jobs_rx {
                    #[cfg(test)]
                    if let Some(gate) = &gate {
                        gate.wait();
                    }
                    let result = append_batch(&thread_path, policy, &job.records);
                    if let Err(e) = &result {
                        log_failed(&thread_path, &job.records, e);
                    }
                    let _ = job.done.send(result);
                }
            });
        let jobs = match spawned {
            Ok(_) => Some(jobs_tx),
            Err(e) => {
                error!(
                    target: LOG_TARGET,
                    "TX log: could not start the writer thread for {}: {}; transmitted frames will not be recorded",
                    path.display(),
                    e
                );
                None
            }
        };
        Self {
            writer: Some(Writer { path, jobs }),
        }
    }

    /// Replay mode: no thread, no file; every append returns `Disabled`.
    pub(crate) fn disabled() -> Self {
        Self { writer: None }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.path.as_path())
    }

    /// Queue `records` as one batch and wait up to `wait` for the writer's
    /// acknowledgement. Never blocks longer than `wait`, never panics; the
    /// outcome is informational (TX behaviour must not depend on it).
    pub(crate) async fn append(&self, records: Vec<TxFrameRecord>, wait: Duration) -> TxLogAppend {
        let Some(writer) = &self.writer else {
            return TxLogAppend::Disabled;
        };
        let Some(jobs) = &writer.jobs else {
            log_failed(&writer.path, &records, &"the writer thread is not running");
            return TxLogAppend::Failed;
        };
        let summary_on_loss = frames_summary(&records);
        let count = records.len();
        let (done, done_rx) = tokio::sync::oneshot::channel();
        if let Err(mpsc::SendError(job)) = jobs.send(Job { records, done }) {
            log_failed(&writer.path, &job.records, &"the writer thread has stopped");
            return TxLogAppend::Failed;
        }
        match tokio::time::timeout(wait, done_rx).await {
            Ok(Ok(Ok(()))) => TxLogAppend::Durable,
            // The writer thread already logged the failure with the frames.
            Ok(Ok(Err(_))) => TxLogAppend::Failed,
            Ok(Err(_)) => {
                error!(
                    target: LOG_TARGET,
                    "TX log: could not record {} frame(s) to {}: the writer stopped before confirming the write. Frames: [{}]",
                    count,
                    writer.path.display(),
                    summary_on_loss
                );
                TxLogAppend::Failed
            }
            Err(_) => {
                warn!(
                    target: LOG_TARGET,
                    "TX log: write still pending after {} ms; transmitting anyway, the record will be written when the disk responds",
                    wait.as_millis()
                );
                TxLogAppend::Pending
            }
        }
    }
}

/// `<home>/.pancetta/tx-log.jsonl`, where home is `home_override` (tests),
/// else the user's home directory, else `.`. Under `cfg(test)` with no
/// override the fallback is a per-process temp dir, never the real home.
pub(crate) fn tx_log_path(home_override: Option<&Path>) -> PathBuf {
    let home = match home_override {
        Some(home) => home.to_path_buf(),
        None => default_home(),
    };
    home.join(".pancetta").join(TX_LOG_FILE_NAME)
}

#[cfg(not(test))]
fn default_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
fn default_home() -> PathBuf {
    std::env::temp_dir().join(format!("pancetta-test-home-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn rec(text: &str) -> TxFrameRecord {
        TxFrameRecord::new(
            chrono::Utc::now(),
            pancetta_ft8::Protocol::Ft8,
            14_074_000,
            1500.0,
            text.to_string(),
            TxOrigin::Local,
            None,
        )
    }

    fn read_records(path: &Path) -> Vec<TxFrameRecord> {
        match fs::read_to_string(path) {
            Ok(content) => content
                .lines()
                .map(|line| serde_json::from_str(line).expect("each line is one record"))
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read {}: {e}", path.display()),
        }
    }

    #[test]
    fn record_serializes_as_one_json_line_with_schema_v1_fields() {
        let record = TxFrameRecord::new(
            chrono::Utc::now(),
            pancetta_ft8::Protocol::Ft8,
            14_074_000,
            1500.0,
            "CQ W5AU EM10".to_string(),
            TxOrigin::Local,
            None,
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains('\n'), "one record is one line: {json}");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // serde_json's Map sorts keys unless preserve_order is on, so check
        // the field order on the raw text instead.
        let mut sorted = vec![
            "v",
            "utc",
            "mode",
            "dial_hz",
            "offset_hz",
            "text",
            "origin",
            "qso_id",
        ];
        let positions: Vec<usize> = sorted
            .iter()
            .map(|k| {
                json.find(&format!("\"{k}\":"))
                    .unwrap_or_else(|| panic!("missing key {k} in {json}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "keys must appear in schema order: {json}"
        );
        let mut got = keys.clone();
        got.sort_unstable();
        sorted.sort_unstable();
        assert_eq!(got, sorted, "exactly the schema v1 keys");
        assert!(json.starts_with("{\"v\":1,"), "{json}");
        assert!(json.contains("\"mode\":\"FT8\""), "{json}");
        assert!(json.contains("\"dial_hz\":14074000"), "{json}");
        assert!(json.contains("\"offset_hz\":1500.0"), "{json}");
        assert!(json.contains("\"origin\":\"Local\""), "{json}");
        assert!(json.contains("\"qso_id\":null"), "{json}");
        let back: TxFrameRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);

        let unknown_dial = TxFrameRecord::new(
            record.utc,
            pancetta_ft8::Protocol::Ft8,
            0,
            1500.0,
            "CQ W5AU EM10".to_string(),
            TxOrigin::Local,
            None,
        );
        let json = serde_json::to_string(&unknown_dial).unwrap();
        assert!(json.contains("\"dial_hz\":null"), "{json}");
    }

    #[test]
    fn append_batch_writes_every_record_in_order_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let first = vec![rec("CQ N0CALL FN20")];
        let second = vec![rec("K1ABC W5AU -12"), rec("N0XYZ W5AU R-07"), rec("73")];
        append_batch(&path, RotationPolicy::default(), &first).unwrap();
        append_batch(&path, RotationPolicy::default(), &second).unwrap();
        let expected: Vec<TxFrameRecord> = first.into_iter().chain(second).collect();
        assert_eq!(read_records(&path), expected);
    }

    /// An in-memory log file whose writes fail once `budget` bytes are spent,
    /// after writing whatever still fits (a disk filling mid-append).
    struct FillingDisk {
        data: io::Cursor<Vec<u8>>,
        budget: usize,
    }

    impl Read for FillingDisk {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.data.read(buf)
        }
    }

    impl Write for FillingDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.budget == 0 {
                return Err(io::Error::other("no space left on device"));
            }
            let n = buf.len().min(self.budget);
            self.budget -= n;
            self.data.write(&buf[..n])
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FillingDisk {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    impl LogFile for FillingDisk {
        fn truncate_to(&mut self, len: u64) -> io::Result<()> {
            self.data.get_mut().truncate(len as usize);
            Ok(())
        }
    }

    fn lines(records: &[TxFrameRecord]) -> Vec<u8> {
        let mut buf = Vec::new();
        for record in records {
            serde_json::to_writer(&mut buf, record).unwrap();
            buf.push(b'\n');
        }
        buf
    }

    fn parse_lines(bytes: &[u8]) -> Vec<TxFrameRecord> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is one record"))
            .collect()
    }

    #[test]
    fn a_write_that_fails_part_way_is_rolled_back_before_the_next_append() {
        // Review (PR #468): a disk-full partial write must not leave a fragment
        // that the next append concatenates onto.
        let first = vec![rec("CQ N0CALL FN20")];
        let lost = vec![rec("K1ABC W5AU -12")];
        let next = vec![rec("73")];
        let mut file = FillingDisk {
            data: io::Cursor::new(Vec::new()),
            budget: usize::MAX,
        };
        append_whole_lines(&mut file, &lines(&first)).unwrap();
        file.budget = 10;
        assert!(append_whole_lines(&mut file, &lines(&lost)).is_err());
        assert_eq!(file.data.get_ref(), &lines(&first), "fragment rolled back");
        file.budget = usize::MAX;
        append_whole_lines(&mut file, &lines(&next)).unwrap();
        let expected: Vec<TxFrameRecord> = first.into_iter().chain(next).collect();
        assert_eq!(parse_lines(file.data.get_ref()), expected);
    }

    #[test]
    fn a_torn_tail_is_terminated_so_later_records_stay_parseable() {
        // A crash mid-write (or a rollback that itself failed) leaves a line
        // without its newline; the next append must start on a fresh line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let first = vec![rec("CQ N0CALL FN20")];
        append_batch(&path, RotationPolicy::default(), &first).unwrap();
        let mut torn = fs::read(&path).unwrap();
        torn.extend_from_slice(b"{\"v\":1,\"utc\":\"20");
        fs::write(&path, &torn).unwrap();
        let next = vec![rec("73")];
        append_batch(&path, RotationPolicy::default(), &next).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let good: Vec<TxFrameRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let expected: Vec<TxFrameRecord> = first.into_iter().chain(next).collect();
        assert_eq!(good, expected);
        assert_eq!(content.lines().count(), 3, "torn line kept on its own line");
    }

    #[test]
    fn append_batch_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join(TX_LOG_FILE_NAME);
        append_batch(&path, RotationPolicy::default(), &[rec("CQ N0CALL FN20")]).unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn append_batch_is_visible_to_an_independent_reader_before_the_writer_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let log = TxFrameLog::open(path.clone());
        let record = rec("CQ N0CALL FN20");
        assert_eq!(
            log.append(vec![record.clone()], Duration::from_secs(5))
                .await,
            TxLogAppend::Durable
        );
        // `log` is still alive: the bytes must already be in the file.
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(
            content,
            format!("{}\n", serde_json::to_string(&record).unwrap())
        );
        drop(log);
    }

    #[test]
    fn rotation_moves_full_file_to_dot_1_and_keeps_at_most_n_archives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let policy = RotationPolicy {
            max_file_bytes: 300,
            archives: 2,
        };
        let one_len = serde_json::to_string(&rec("FRAME 00")).unwrap().len() + 1;
        assert!(
            (100..=150).contains(&one_len),
            "fixture sizing assumption: {one_len}"
        );
        let all: Vec<TxFrameRecord> = (0..12).map(|i| rec(&format!("FRAME {i:02}"))).collect();
        for record in &all {
            append_batch(&path, policy, std::slice::from_ref(record)).unwrap();
        }
        let a1 = archive_path(&path, 1);
        let a2 = archive_path(&path, 2);
        assert!(a1.exists() && a2.exists(), ".1 and .2 must exist");
        assert!(!archive_path(&path, 3).exists(), ".3 must never exist");
        for p in [&path, &a1, &a2] {
            let len = fs::metadata(p).unwrap().len();
            assert!(len <= 300, "{} is {len} B", p.display());
        }
        let kept: Vec<TxFrameRecord> = [&a2, &a1, &path]
            .iter()
            .flat_map(|p| read_records(p))
            .collect();
        assert!(!kept.is_empty());
        let tail = &all[all.len() - kept.len()..];
        assert_eq!(kept, tail, "newest records, in append order, no gaps");
        assert_eq!(kept.last(), all.last());
    }

    #[test]
    fn rotation_never_splits_a_batch_and_never_rotates_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let policy = RotationPolicy {
            max_file_bytes: 300,
            archives: 2,
        };
        let batch = vec![rec("BUNDLE A"), rec("BUNDLE B"), rec("BUNDLE C")];
        append_batch(&path, policy, &batch).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > 300);
        assert!(!archive_path(&path, 1).exists(), "empty file never rotated");
        assert_eq!(read_records(&path), batch);

        let next = vec![rec("AFTER")];
        append_batch(&path, policy, &next).unwrap();
        assert_eq!(
            read_records(&archive_path(&path, 1)),
            batch,
            "rotated whole"
        );
        assert_eq!(read_records(&path), next);
    }

    fn seed_archives(path: &Path, archives: usize) {
        for n in 1..=archives {
            fs::write(archive_path(path, n), format!("archive {n}\n")).unwrap();
        }
    }

    fn archive_contents(path: &Path, archives: usize) -> Vec<Option<String>> {
        (1..=archives)
            .map(|n| fs::read_to_string(archive_path(path, n)).ok())
            .collect()
    }

    #[test]
    fn a_failing_archive_push_undoes_its_shifts_and_retries_drop_nothing_more() {
        // Review (PR #468): a rotation that fails after shifting archives must
        // not walk the retained history off the end on every retry.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        seed_archives(&path, 3);
        // `src` does not exist, so the final move to `.1` fails after the
        // shifts have run.
        let missing = dir.path().join("missing");
        for _ in 0..5 {
            assert!(push_archive(&path, &missing, 3).is_err());
        }
        assert_eq!(
            archive_contents(&path, 3),
            vec![
                Some("archive 1\n".to_string()),
                Some("archive 2\n".to_string()),
                None,
            ],
            "only the oldest (the one rotation was meant to drop) is gone"
        );
    }

    #[test]
    fn a_failed_rotation_keeps_the_active_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        fs::write(&path, "active\n").unwrap();
        seed_archives(&path, 2);
        // A non-empty directory squatting on `.1`'s target after the shift
        // (`.2`) makes the `.1 -> .2` shift fail.
        fs::remove_file(archive_path(&path, 2)).unwrap();
        fs::create_dir(archive_path(&path, 2)).unwrap();
        fs::write(archive_path(&path, 2).join("x"), "x").unwrap();
        assert!(rotate(&path, 2).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "active\n");
        assert_eq!(
            fs::read_to_string(archive_path(&path, 1)).unwrap(),
            "archive 1\n"
        );
        assert!(!staging_path(&path).exists(), "active file moved back");
    }

    #[test]
    fn a_leftover_staged_file_is_archived_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        fs::write(staging_path(&path), "interrupted\n").unwrap();
        fs::write(&path, "active\n").unwrap();
        rotate(&path, 3).unwrap();
        assert_eq!(
            archive_contents(&path, 3),
            vec![
                Some("active\n".to_string()),
                Some("interrupted\n".to_string()),
                None,
            ]
        );
        assert!(!path.exists() && !staging_path(&path).exists());
    }

    #[tokio::test]
    async fn append_reports_failed_when_the_path_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("plain-file");
        fs::write(&not_a_dir, b"x").unwrap();
        let path = not_a_dir.join(TX_LOG_FILE_NAME);
        assert!(append_batch(&path, RotationPolicy::default(), &[rec("CQ N0CALL FN20")]).is_err());
        let log = TxFrameLog::open(path);
        assert_eq!(
            log.append(vec![rec("CQ N0CALL FN20")], Duration::from_secs(5))
                .await,
            TxLogAppend::Failed
        );
    }

    #[tokio::test]
    async fn blocked_writer_returns_pending_then_writes_in_fifo_order_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let gate = WriterGate::closed();
        let log = TxFrameLog::open_gated(path.clone(), RotationPolicy::default(), gate.clone());
        let a = rec("FRAME A");
        let b = rec("FRAME B");
        assert_eq!(
            log.append(vec![a.clone()], Duration::from_millis(50)).await,
            TxLogAppend::Pending
        );
        assert_eq!(
            log.append(vec![b.clone()], Duration::from_millis(50)).await,
            TxLogAppend::Pending
        );
        assert!(!path.exists(), "a blocked writer has written nothing");

        gate.open();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut records = read_records(&path);
        while records.len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            records = read_records(&path);
        }
        assert_eq!(records, vec![a, b], "both written, in FIFO order");
    }

    #[tokio::test]
    async fn disabled_log_creates_no_file_and_reports_disabled() {
        let log = TxFrameLog::disabled();
        assert_eq!(log.path(), None);
        assert_eq!(
            log.append(vec![rec("CQ N0CALL FN20")], Duration::from_secs(1))
                .await,
            TxLogAppend::Disabled
        );
    }

    #[test]
    fn tx_log_path_prefers_home_override() {
        let home = Path::new("/tmp/some-home");
        assert_eq!(
            tx_log_path(Some(home)),
            home.join(".pancetta").join("tx-log.jsonl")
        );
    }

    #[test]
    fn tx_log_path_never_resolves_to_the_real_home_in_tests() {
        assert!(tx_log_path(None).starts_with(std::env::temp_dir()));
    }

    /// Child half of the crash pair: a trivial pass in normal runs; when
    /// re-run by `record_survives_process_abort_immediately_after_append`
    /// it appends one record and aborts the process at once.
    #[tokio::test]
    async fn crash_child_appends_then_aborts() {
        let Some(path) = std::env::var_os("PANCETTA_TX_LOG_CRASH_CHILD") else {
            return;
        };
        let log = TxFrameLog::open(PathBuf::from(path));
        assert_eq!(
            log.append(vec![crash_record()], Duration::from_secs(5))
                .await,
            TxLogAppend::Durable
        );
        std::process::abort();
    }

    fn crash_record() -> TxFrameRecord {
        TxFrameRecord::new(
            chrono::DateTime::parse_from_rfc3339("2026-10-18T14:30:15.012345678Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            pancetta_ft8::Protocol::Ft8,
            14_074_000,
            1500.0,
            "CQ N0CALL FN20".to_string(),
            TxOrigin::Local,
            None,
        )
    }

    #[test]
    fn record_survives_process_abort_immediately_after_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TX_LOG_FILE_NAME);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "coordinator::tx_log::tests::crash_child_appends_then_aborts",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PANCETTA_TX_LOG_CRASH_CHILD", &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("re-run the test binary as the crash child");
        assert!(!status.success(), "the child must have aborted");
        assert_eq!(read_records(&path), vec![crash_record()]);
    }
}
