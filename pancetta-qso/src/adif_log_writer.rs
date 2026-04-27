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

use crate::adif::{AdifError, AdifProcessor, AdifQso};
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
                tokio::fs::create_dir_all(parent).await.map_err(|source| {
                    AdifLogError::Io {
                        path: parent.to_path_buf(),
                        source,
                    }
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
        let mut f = self.file.lock().await;
        f.write_all(header.as_bytes())
            .await
            .map_err(|source| AdifLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        f.flush()
            .await
            .map_err(|source| AdifLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }

    /// Append one ADIF record for `qso` and flush to the OS buffer.
    ///
    /// Thread-safe: the underlying file handle is guarded by a `Mutex`.
    pub async fn append(&self, qso: &QsoMetadata) -> AdifLogResult<()> {
        let adif_qso: AdifQso = self
            .processor
            .qso_to_adif(qso, qso.contest_info.as_ref());
        let record = self
            .processor
            .generate_record(&adif_qso)
            .map_err(|e| AdifLogError::Format(e.to_string()))?;

        let mut f = self.file.lock().await;
        f.write_all(record.as_bytes())
            .await
            .map_err(|source| AdifLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        f.flush()
            .await
            .map_err(|source| AdifLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }

    /// Return the path this writer is managing.
    pub fn path(&self) -> &Path {
        &self.path
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
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
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
            eor_count,
            2,
            "expected 2 records, got {}\n--- file ---\n{}",
            eor_count,
            contents
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
}
