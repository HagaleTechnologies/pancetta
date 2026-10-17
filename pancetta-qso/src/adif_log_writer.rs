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
//! deliberately omitted here. A bulk import ([`AdifLogWriter::append_record`]
//! per record) calls [`AdifLogWriter::sync_all`] once after its last record.

use crate::adif::{AdifError, AdifProcessor, AdifQso, AdifRecord};
use crate::QsoMetadata;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

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
}

impl AdifLogWriter {
    /// Open (or create) the ADIF log at `path`.
    ///
    /// - If the file does not exist, creates it (including parent directories)
    ///   and writes the ADIF file header.
    /// - If it already exists, opens it in append mode — no truncation, no
    ///   duplicate header.
    pub async fn open(path: impl AsRef<Path>) -> AdifLogResult<Self> {
        let path = path.as_ref().to_path_buf();

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

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|source| AdifLogError::Io {
                path: path.clone(),
                source,
            })?;

        let processor = AdifProcessor::new();
        let writer = Self {
            path: path.clone(),
            file: Mutex::new(file),
            processor,
        };

        if !already_exists {
            writer.write_header().await?;
        }

        Ok(writer)
    }

    /// Write the ADIF file header (called once when the file is first created).
    async fn write_header(&self) -> AdifLogResult<()> {
        let header = self.processor.generate_header();
        self.write_flushed(header.as_bytes()).await
    }

    /// One `write_all` on the `O_APPEND` handle, then flush to the OS buffer.
    async fn write_flushed(&self, bytes: &[u8]) -> AdifLogResult<()> {
        let mut f = self.file.lock().await;
        f.write_all(bytes)
            .await
            .map_err(|source| AdifLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        f.flush().await.map_err(|source| AdifLogError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    /// Append one ADIF record for `qso` and flush to the OS buffer.
    ///
    /// Thread-safe: the underlying file handle is guarded by a `Mutex`.
    pub async fn append(&self, qso: &QsoMetadata) -> AdifLogResult<()> {
        let adif_qso: AdifQso = self.processor.qso_to_adif(qso, qso.contest_info.as_ref());
        let record = self
            .processor
            .generate_record(&adif_qso)
            .map_err(|e| AdifLogError::Format(e.to_string()))?;
        self.write_flushed(record.as_bytes()).await
    }

    /// Append `record` with its fields exactly as given (an imported contact,
    /// not a QSO pancetta completed), flushing to the OS buffer. One write per
    /// record, like `append`, so a running station's own appends never interleave
    /// inside it.
    pub async fn append_record(&self, record: &AdifRecord) -> AdifLogResult<()> {
        let rendered = self.processor.render_record(record)?;
        self.write_flushed(rendered.as_bytes()).await
    }

    /// fsync the log. A bulk import calls this once after its last record.
    pub async fn sync_all(&self) -> AdifLogResult<()> {
        let f = self.file.lock().await;
        f.sync_all().await.map_err(|source| AdifLogError::Io {
            path: self.path.clone(),
            source,
        })
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

    #[tokio::test]
    async fn append_record_writes_the_fields_verbatim() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        let writer = AdifLogWriter::open(&path).await.unwrap();
        let record = imported_record();

        writer.append_record(&record).await.unwrap();
        writer.sync_all().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents.matches("<EOH>").count(), 1, "{contents}");
        assert!(!contents.contains("Using cqdx.io & Pancetta"), "{contents}");
        let file = AdifProcessor::new().parse_string(&contents).unwrap();
        assert_eq!(file.records.len(), 1, "{contents}");
        assert_eq!(field_map(&file.records[0]), field_map(&record));
        assert_eq!(file.records[0].fields["QSL_RCVD"].value, "Y");
        assert_eq!(file.records[0].fields["COMMENT"].value, "from N1MM");
    }

    #[tokio::test]
    async fn append_record_after_live_appends_keeps_one_header() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("qsos.adi");
        {
            let live = AdifLogWriter::open(&path).await.unwrap();
            live.append(&fixture_metadata()).await.unwrap();
            live.append(&fixture_metadata()).await.unwrap();
        }
        let import = AdifLogWriter::open(&path).await.unwrap();
        import.append_record(&imported_record()).await.unwrap();
        import.sync_all().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents.matches("<EOH>").count(), 1, "{contents}");
        let file = AdifProcessor::new().parse_string(&contents).unwrap();
        let calls: Vec<&str> = file
            .records
            .iter()
            .map(|r| r.fields["CALL"].value.as_str())
            .collect();
        assert_eq!(calls, ["W1AW", "W1AW", "K1ABC"]);
    }
}
