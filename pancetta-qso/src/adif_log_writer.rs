//! Append-only writer for the ADIF source-of-truth log.
//!
//! Owns `~/.pancetta/qsos.adi` (or whatever path the coordinator hands it).
//! On first open, writes the ADIF header. Every subsequent [`AdifLogWriter::append`]
//! call writes one fully-formed ADIF record terminated by `<EOR>`.
//!
//! ## Durability contract
//!
//! Each record is followed by `f.flush().await` (OS buffer → kernel), which is
//! sufficient for the FT8 hot path: at most one QSO completes per 15-second
//! window. `sync_data` (fsync) would be roughly 10 000× more expensive and is
//! deliberately omitted here.
//!
//! The header and each record are written while holding the
//! [`LogbookLock`] that `pancetta import` also takes (PAN-214), so a QSO
//! completed during an import lands after the import's block, never inside
//! it. A QSO is never dropped for the lock: after [`LIVE_LOCK_WAIT`], or when
//! the lock file cannot be used, the record is written without it. An import
//! writes its records as one block ([`prepare_import_block`], then
//! [`write_import_block`], which fsyncs once), and `pancetta import --undo`
//! removes that block in place ([`locate_import_block`],
//! [`remove_import_block`]).

use crate::adif::{AdifError, AdifProcessor, AdifQso, AdifRecord};
use crate::logbook_lock::{LogbookGuard, LogbookLock};
use crate::QsoMetadata;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// How long a live QSO append waits for an import to release the logbook
/// before writing anyway (PAN-214).
pub const LIVE_LOCK_WAIT: Duration = Duration::from_secs(60);

/// Errors that can occur in [`AdifLogWriter`].
#[derive(Debug, Error)]
pub enum AdifLogError {
    /// An I/O operation on the log file failed.
    #[error("ADIF I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// ADIF record formatting failed.
    #[error("ADIF formatting failed: {0}")]
    Format(String),

    /// The logbook is not as long as an import block or removal expects, so
    /// it changed since it was read; nothing was written (PAN-214).
    #[error("{path} is {found} bytes long, not the expected {expected}; nothing was written")]
    LogbookMoved {
        path: PathBuf,
        expected: u64,
        found: u64,
    },
}

impl From<AdifError> for AdifLogError {
    fn from(e: AdifError) -> Self {
        AdifLogError::Format(e.to_string())
    }
}

/// Convenience `Result` alias for [`AdifLogError`].
pub type AdifLogResult<T> = Result<T, AdifLogError>;

/// Append-only writer that maintains `~/.pancetta/qsos.adi` (or any path).
///
/// The file is the durable, vendor-neutral source of truth for completed QSOs.
/// Create one instance per process via [`AdifLogWriter::open`] and call
/// [`AdifLogWriter::append`] each time a QSO completes.
pub struct AdifLogWriter {
    path: PathBuf,
    file: Mutex<File>,
    processor: AdifProcessor,
    /// The logbook lock shared with `pancetta import`; `None` when its file
    /// could not be opened, and then the writer logs without it (PAN-214).
    lock: Option<LogbookLock>,
    /// How long one write waits for the lock before writing anyway.
    lock_wait: Duration,
}

impl AdifLogWriter {
    /// Open (or create) the ADIF log at `path`.
    ///
    /// - If the file does not exist, creates it (including parent directories)
    ///   and writes the ADIF file header.
    /// - If it already exists, opens it in append mode — no truncation, no
    ///   duplicate header.
    ///
    /// The existence check, the open and the header are one turn at the
    /// logbook lock (PAN-214).
    pub async fn open(path: impl AsRef<Path>) -> AdifLogResult<Self> {
        let path = path.as_ref().to_path_buf();
        let lock = LogbookLock::open(&path)
            .inspect_err(|e| {
                warn!(
                    "Cannot open the logbook lock {}: {e}; logging to {} without it",
                    LogbookLock::path_for(&path).display(),
                    path.display()
                )
            })
            .ok();
        let processor = AdifProcessor::new();

        let file = {
            let _turn = turn(lock.as_ref(), LIVE_LOCK_WAIT, &path, "its header").await;

            // Check existence *before* opening so we know whether to write the header.
            let already_exists = tokio::fs::try_exists(&path).await.unwrap_or(false);

            // Create parent directories if needed.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|source| AdifLogError::Io {
                            path: parent.to_path_buf(),
                            source,
                        })?;
                }
            }

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|source| AdifLogError::Io {
                    path: path.clone(),
                    source,
                })?;

            if !already_exists {
                let header = processor.generate_header();
                write_and_flush(&mut file, header.as_bytes(), &path).await?;
            }
            file
        };

        Ok(Self {
            path,
            file: Mutex::new(file),
            processor,
            lock,
            lock_wait: LIVE_LOCK_WAIT,
        })
    }

    /// One `write_all` on the `O_APPEND` handle, then flush to the OS buffer,
    /// during one turn at the logbook lock.
    async fn write_flushed(&self, bytes: &[u8], what: &str) -> AdifLogResult<()> {
        let mut f = self.file.lock().await;
        let _turn = turn(self.lock.as_ref(), self.lock_wait, &self.path, what).await;
        write_and_flush(&mut f, bytes, &self.path).await
    }

    /// Append one ADIF record for `qso` and flush to the OS buffer.
    ///
    /// Thread-safe: the underlying file handle is guarded by a `Mutex`. Waits
    /// up to [`LIVE_LOCK_WAIT`] while `pancetta import` holds the logbook,
    /// then writes regardless (PAN-214).
    pub async fn append(&self, qso: &QsoMetadata) -> AdifLogResult<()> {
        let adif_qso: AdifQso = self.processor.qso_to_adif(qso, qso.contest_info.as_ref());
        let record = self
            .processor
            .generate_record(&adif_qso)
            .map_err(|e| AdifLogError::Format(e.to_string()))?;
        let what = match qso.their_callsign.as_deref() {
            Some(call) => format!("the QSO with {call}"),
            None => "the QSO".to_string(),
        };
        self.write_flushed(record.as_bytes(), &what).await
    }

    /// Return the path this writer is managing.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Set the station transmit power (watts) stamped into every `TX_PWR`
    /// field this writer renders from here on. Pass the operator's
    /// configured `[station] power_watts`; `0` omits `TX_PWR` entirely.
    pub fn set_station_power_watts(&mut self, watts: u32) {
        self.processor.set_station_power_watts(watts);
    }
}

/// Wait up to `wait` for the logbook lock before writing `what` to `logbook`.
/// `None` means write without it: there is no lock file, the lock stayed busy,
/// or locking failed. A live QSO is never dropped for the lock (PAN-214).
async fn turn<'a>(
    lock: Option<&'a LogbookLock>,
    wait: Duration,
    logbook: &Path,
    what: &str,
) -> Option<LogbookGuard<'a>> {
    let lock = lock?;
    let on_busy = || {
        info!(
            "{} is busy (pancetta import?) — {what} is written as soon as it is free",
            logbook.display()
        )
    };
    match lock.lock_within(wait, on_busy).await {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            warn!(
                "{} is still busy after {} s; writing {what} anyway",
                logbook.display(),
                wait.as_secs_f64()
            );
            None
        }
        Err(e) => {
            warn!(
                "Cannot lock {}: {e}; writing {what} without the lock",
                lock.path().display()
            );
            None
        }
    }
}

async fn write_and_flush(f: &mut File, bytes: &[u8], path: &Path) -> AdifLogResult<()> {
    let io = |source| AdifLogError::Io {
        path: path.to_path_buf(),
        source,
    };
    f.write_all(bytes).await.map_err(io)?;
    f.flush().await.map_err(io)
}

/// An import's records, rendered and placed (PAN-214). The caller holds the
/// [`LogbookLock`] from prepare to write, so `offset` stays true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBlock {
    /// Byte offset in the logbook where the first imported record starts.
    pub offset: u64,
    /// The ADIF header, written first, when the logbook is missing or empty.
    pub header: Option<String>,
    /// The records exactly as they are appended.
    pub added: String,
}

/// Render `records` with their fields exactly as read (imported contacts, not
/// QSOs pancetta completed) and place them after the logbook's current end:
/// header `Some(processor.generate_header())` when the logbook is missing or
/// empty, `offset` = existing length + header length (PAN-214).
pub fn prepare_import_block(
    path: &Path,
    processor: &AdifProcessor,
    records: &[AdifRecord],
) -> AdifLogResult<ImportBlock> {
    let existing = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(source) => {
            return Err(AdifLogError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let header = (existing == 0).then(|| processor.generate_header());
    let added = records
        .iter()
        .map(|record| processor.render_record(record))
        .collect::<Result<String, _>>()?;
    let offset = existing + header.as_ref().map_or(0, |h| h.len() as u64);
    Ok(ImportBlock {
        offset,
        header,
        added,
    })
}

/// One write (header first when there is one), then fsync. Refuses, writing
/// nothing, when the logbook's length is no longer `offset` minus the header:
/// something wrote to it after [`prepare_import_block`] (PAN-214).
pub fn write_import_block(path: &Path, block: &ImportBlock) -> AdifLogResult<()> {
    let io = |source| AdifLogError::Io {
        path: path.to_path_buf(),
        source,
    };
    let header = block.header.as_deref().unwrap_or_default();
    let expected = block
        .offset
        .checked_sub(header.len() as u64)
        .ok_or_else(|| AdifLogError::Format("import block offset precedes its header".into()))?;
    if expected == 0 {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| AdifLogError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    // Only a block for a new logbook may create the file.
    let mut file = std::fs::OpenOptions::new()
        .create(expected == 0)
        .append(true)
        .open(path)
        .map_err(io)?;
    let found = file.metadata().map_err(io)?.len();
    if found != expected {
        return Err(AdifLogError::LogbookMoved {
            path: path.to_path_buf(),
            expected,
            found,
        });
    }
    let bytes = [header.as_bytes(), block.added.as_bytes()].concat();
    file.write_all(&bytes).map_err(io)?;
    file.sync_all().map_err(io)
}

/// Where `added` sits in `current`: at `offset` if it matches there exactly,
/// else its only occurrence. `None` when it is absent (edited) or found more
/// than once, since then which copy is the import's is unknown (PAN-214).
pub fn locate_import_block(current: &[u8], offset: u64, added: &str) -> Option<usize> {
    let added = added.as_bytes();
    let at_offset = usize::try_from(offset).ok().filter(|&at| {
        at.checked_add(added.len())
            .and_then(|end| current.get(at..end))
            == Some(added)
    });
    if at_offset.is_some() {
        return at_offset;
    }
    if added.is_empty() {
        return None;
    }
    let find_from = |start: usize| {
        current
            .get(start..)?
            .windows(added.len())
            .position(|window| window == added)
            .map(|i| start + i)
    };
    let first = find_from(0)?;
    find_from(first + 1).is_none().then_some(first)
}

/// Rewrite `path` in place without `current[at..at + len]`: write the tail at
/// `at`, `set_len`, fsync. Never a rename, so a running station's `O_APPEND`
/// handle stays valid and its next QSO lands after the shortened file. The
/// caller holds the [`LogbookLock`] and has backed `current` up; refuses,
/// writing nothing, when the file is no longer `current`'s length (PAN-214).
pub fn remove_import_block(
    path: &Path,
    current: &[u8],
    at: usize,
    len: usize,
) -> AdifLogResult<()> {
    let io = |source| AdifLogError::Io {
        path: path.to_path_buf(),
        source,
    };
    let end = at
        .checked_add(len)
        .filter(|&end| end <= current.len())
        .ok_or_else(|| AdifLogError::Format("import block lies outside the logbook".into()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io)?;
    let found = file.metadata().map_err(io)?.len();
    if found != current.len() as u64 {
        return Err(AdifLogError::LogbookMoved {
            path: path.to_path_buf(),
            expected: current.len() as u64,
            found,
        });
    }
    file.seek(SeekFrom::Start(at as u64)).map_err(io)?;
    file.write_all(&current[end..]).map_err(io)?;
    file.set_len((current.len() - len) as u64).map_err(io)?;
    file.sync_all().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::states::{GridSquares, QsoMetadata, SignalReports};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn fixture_metadata() -> QsoMetadata {
        QsoMetadata {
            qso_id: uuid::Uuid::new_v4(),
            our_callsign: "K1ABC".to_string(),
            their_callsign: Some("W1AW".to_string()),
            frequency: 14_074_000.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            start_time: chrono::Utc::now(),
            end_time: Some(chrono::Utc::now()),
            reports: SignalReports {
                sent: Some(-12),
                received: Some(-15),
            },
            grids: GridSquares {
                ours: Some("EM10".to_string()),
                theirs: Some("FN42".to_string()),
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            remote_client_key_id: None,
            tx_parity_provisional: false,
        }
    }

    #[tokio::test]
    async fn appends_one_record_per_qso_completed() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let writer = AdifLogWriter::open(&path).await.unwrap();

        writer.append(&fixture_metadata()).await.unwrap();
        writer.append(&fixture_metadata()).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let eor_count = contents.matches("<EOR>").count();
        assert_eq!(
            eor_count, 2,
            "expected 2 records, got {}\n--- file ---\n{}",
            eor_count, contents
        );
    }

    #[tokio::test]
    async fn second_open_appends_does_not_truncate() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        {
            let w1 = AdifLogWriter::open(&path).await.unwrap();
            w1.append(&fixture_metadata()).await.unwrap();
        }
        {
            let w2 = AdifLogWriter::open(&path).await.unwrap();
            w2.append(&fixture_metadata()).await.unwrap();
        }
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        // Exactly one header (from first open) + 2 records
        assert_eq!(
            contents.matches("<EOR>").count(),
            2,
            "expected 2 records after two separate opens\n--- file ---\n{}",
            contents
        );
        assert_eq!(
            contents.matches("<EOH>").count(),
            1,
            "expected exactly one header\n--- file ---\n{}",
            contents
        );
    }

    #[tokio::test]
    async fn first_open_writes_adif_header() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let _w = AdifLogWriter::open(&path).await.unwrap();
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            contents.contains("<ADIF_VER:"),
            "missing <ADIF_VER: tag\n--- file ---\n{}",
            contents
        );
        assert!(
            contents.contains("<EOH>"),
            "missing <EOH> tag\n--- file ---\n{}",
            contents
        );
    }

    /// PAN-127: pancetta's own log reads back record for record, including a
    /// non-ASCII COMMENT (byte-counted) that used to swallow the next field.
    #[tokio::test]
    async fn parse_string_reads_a_log_written_by_adif_log_writer() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let writer = AdifLogWriter::open(&path).await.unwrap();
        let mut with_notes = fixture_metadata();
        with_notes.notes = Some("Müñoz".to_string());

        writer.append(&fixture_metadata()).await.unwrap();
        writer.append(&with_notes).await.unwrap();
        writer.append(&fixture_metadata()).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let processor = AdifProcessor::new();
        let file = processor.parse_string(&contents).unwrap();
        assert_eq!(file.records.len(), 3, "--- file ---\n{contents}");
        for record in &file.records {
            let qso = processor.record_to_qso(record).unwrap();
            assert_eq!(qso.call, "W1AW");
        }
        let comment = &file.records[1].fields["COMMENT"].value;
        assert!(comment.starts_with("Müñoz"), "COMMENT was {comment:?}");
        assert_eq!(file.records[1].fields["CALL"].value, "W1AW");
    }

    /// An imported contact as another logger wrote it.
    fn imported_record() -> AdifRecord {
        let adif = "<CALL:5>K1ABC <QSO_DATE:8>20240102 <TIME_ON:4>0815 <MODE:3>FT8 \
                    <BAND:3>40M <QSL_RCVD:1>Y <APP_N1MM_ID:4>1234 \
                    <COMMENT:9>from N1MM <EOR>\n";
        let mut file = AdifProcessor::new().parse_string(adif).unwrap();
        file.records.remove(0)
    }

    fn field_map(record: &AdifRecord) -> HashMap<String, String> {
        record
            .fields
            .iter()
            .map(|(name, f)| (name.clone(), f.value.clone()))
            .collect()
    }

    /// `qso` with W1AW swapped for `call`.
    fn metadata_with(call: &str) -> QsoMetadata {
        let mut qso = fixture_metadata();
        qso.their_callsign = Some(call.to_string());
        qso
    }

    fn calls_in(contents: &str) -> Vec<String> {
        AdifProcessor::new()
            .parse_string(contents)
            .unwrap()
            .records
            .iter()
            .map(|r| r.fields["CALL"].value.clone())
            .collect()
    }

    /// PAN-214: a live QSO waits while an import holds the logbook lock,
    /// then lands after it.
    #[tokio::test]
    async fn append_waits_for_a_held_logbook_lock() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let writer = std::sync::Arc::new(AdifLogWriter::open(&path).await.unwrap());
        let import = LogbookLock::open(&path).unwrap();
        let held = import.try_lock().unwrap().expect("the lock is free");

        let append = tokio::spawn({
            let writer = writer.clone();
            async move { writer.append(&fixture_metadata()).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("W1AW"),
            "written under a held lock:\n{contents}"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(5), append)
            .await
            .expect("append finishes once the lock is free")
            .unwrap()
            .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(calls_in(&contents), ["W1AW"]);
        assert!(contents.trim_end().ends_with("<EOR>"), "{contents}");
    }

    /// PAN-214: a QSO is never dropped; once the wait runs out it is written
    /// without the lock.
    #[tokio::test]
    async fn append_writes_anyway_when_the_lock_stays_busy() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let mut writer = AdifLogWriter::open(&path).await.unwrap();
        writer.lock_wait = Duration::from_millis(200);
        let import = LogbookLock::open(&path).unwrap();
        let _held = import.try_lock().unwrap().expect("the lock is free");

        tokio::time::timeout(Duration::from_secs(2), writer.append(&fixture_metadata()))
            .await
            .expect("append gives up waiting")
            .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(calls_in(&contents), ["W1AW"]);
    }

    /// PAN-214: an unusable lock file never stops live logging.
    #[tokio::test]
    async fn append_logs_without_a_usable_lock_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        std::fs::create_dir(LogbookLock::path_for(&path)).unwrap();

        let writer = AdifLogWriter::open(&path).await.unwrap();
        writer.append(&fixture_metadata()).await.unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("<EOH>").count(), 1, "{contents}");
        assert_eq!(calls_in(&contents), ["W1AW"]);
    }

    #[test]
    fn an_import_block_on_a_new_logbook_starts_after_the_header() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let processor = AdifProcessor::new();
        let record = imported_record();

        let block = prepare_import_block(&path, &processor, std::slice::from_ref(&record)).unwrap();

        let header = block.header.clone().expect("a new logbook gets a header");
        assert_eq!(header, processor.generate_header());
        assert_eq!(block.offset, header.len() as u64);
        assert_eq!(block.added, processor.render_record(&record).unwrap());
        write_import_block(&path, &block).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, format!("{header}{}", block.added));
        let file = processor.parse_string(&contents).unwrap();
        assert_eq!(file.records.len(), 1, "{contents}");
        assert_eq!(field_map(&file.records[0]), field_map(&record));
    }

    #[tokio::test]
    async fn an_import_block_follows_live_appends_with_one_header() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let live = AdifLogWriter::open(&path).await.unwrap();
        live.append(&fixture_metadata()).await.unwrap();
        let processor = AdifProcessor::new();

        let block = prepare_import_block(&path, &processor, &[imported_record()]).unwrap();

        assert_eq!(block.header, None);
        assert_eq!(block.offset, std::fs::metadata(&path).unwrap().len());
        write_import_block(&path, &block).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("<EOH>").count(), 1, "{contents}");
        assert_eq!(calls_in(&contents), ["W1AW", "K1ABC"]);
        assert_eq!(&contents[block.offset as usize..], block.added);
    }

    #[test]
    fn write_import_block_refuses_a_logbook_that_moved() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let processor = AdifProcessor::new();
        std::fs::write(&path, processor.generate_header()).unwrap();
        let block = prepare_import_block(&path, &processor, &[imported_record()]).unwrap();
        let external = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:4>1200 <EOR>\n";
        let moved = format!("{}{external}", processor.generate_header());
        std::fs::write(&path, &moved).unwrap();

        assert!(write_import_block(&path, &block).is_err());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), moved);
    }

    /// `[pre][block][post]` as written by an import and a live append after it.
    fn logbook_with_block() -> (String, String, String) {
        let processor = AdifProcessor::new();
        let pre = format!(
            "{}<CALL:5>K9XYZ <QSO_DATE:8>20240101 <TIME_ON:4>1100 <MODE:3>FT8 <EOR>\n",
            processor.generate_header()
        );
        let block = processor.render_record(&imported_record()).unwrap();
        let post = "<CALL:4>W1AW <QSO_DATE:8>20240103 <TIME_ON:4>0900 <MODE:3>FT8 <EOR>\n";
        (pre, block, post.to_string())
    }

    #[test]
    fn removing_a_block_keeps_what_was_logged_after_it() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let (pre, block, post) = logbook_with_block();
        let current = format!("{pre}{block}{post}");
        std::fs::write(&path, &current).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };

        let at = locate_import_block(current.as_bytes(), pre.len() as u64, &block)
            .expect("the block is where the receipt says");
        assert_eq!(at, pre.len());
        remove_import_block(&path, current.as_bytes(), at, block.len()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{pre}{post}")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        }
    }

    #[test]
    fn a_shifted_block_is_found_once_and_removed() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let (pre, block, post) = logbook_with_block();
        // The receipt's offset is stale: something before the block grew.
        let stale_offset = pre.len() as u64;
        let grown = format!("{pre}{post}");
        let current = format!("{grown}{block}{post}");
        std::fs::write(&path, &current).unwrap();

        let at = locate_import_block(current.as_bytes(), stale_offset, &block)
            .expect("the block occurs once");
        assert_eq!(at, grown.len());
        remove_import_block(&path, current.as_bytes(), at, block.len()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{grown}{post}")
        );

        let twice = format!("{pre}{post}{block}{block}");
        std::fs::write(&path, &twice).unwrap();
        assert_eq!(
            locate_import_block(twice.as_bytes(), stale_offset, &block),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), twice);
    }

    #[test]
    fn an_edited_block_is_not_removed() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let (pre, block, post) = logbook_with_block();
        let edited_block = block.replacen("K1ABC", "K1ABD", 1);
        let current = format!("{pre}{edited_block}{post}");
        std::fs::write(&path, &current).unwrap();

        assert_eq!(
            locate_import_block(current.as_bytes(), pre.len() as u64, &block),
            None
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
    }

    /// PAN-214: undo rewrites `qsos.adi` in place, so a station that keeps
    /// its `O_APPEND` handle open appends after the shortened file.
    #[tokio::test]
    async fn a_running_writer_keeps_appending_after_a_removal() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let live = AdifLogWriter::open(&path).await.unwrap();
        let processor = AdifProcessor::new();
        let block = prepare_import_block(&path, &processor, &[imported_record()]).unwrap();
        write_import_block(&path, &block).unwrap();
        live.append(&fixture_metadata()).await.unwrap();
        let current = std::fs::read(&path).unwrap();
        let pre = current[..block.offset as usize].to_vec();
        let tail = current[block.offset as usize + block.added.len()..].to_vec();

        let at = locate_import_block(&current, block.offset, &block.added).unwrap();
        remove_import_block(&path, &current, at, block.added.len()).unwrap();
        live.append(&metadata_with("N0XYZ")).await.unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert!(contents.starts_with(&[pre, tail].concat()));
        let contents = String::from_utf8(contents).unwrap();
        assert_eq!(calls_in(&contents), ["W1AW", "N0XYZ"]);
    }
}
