//! QSO logging system with ADIF export/import and history management
//!
//! This module provides comprehensive QSO logging capabilities including
//! automatic logging, ADIF export/import, backup management, and integration
//! with external logging software.

use crate::adif::{AdifError, AdifFile, AdifProcessor};
use crate::async_database::{AsyncDatabaseError, AsyncQsoDatabase};
use crate::database::{DatabaseError, QsoFilter, QueryOptions};
use crate::qso_manager::{QsoEvent, QsoManager};
use crate::states::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

/// Logging system errors
#[derive(Debug, Error)]
pub enum LoggerError {
    #[error("Database error: {source}")]
    Database { source: DatabaseError },

    #[error("Async database error: {source}")]
    AsyncDatabase { source: AsyncDatabaseError },

    #[error("ADIF error: {source}")]
    Adif { source: AdifError },

    #[error("IO error: {source}")]
    Io { source: std::io::Error },

    #[error("QSO manager error: {source}")]
    QsoManager {
        source: crate::qso_manager::QsoManagerError,
    },

    #[error("Configuration error: {message}")]
    Configuration { message: String },

    #[error("Export error: {message}")]
    Export { message: String },

    #[error("Import error: {message}")]
    Import { message: String },

    #[error("Backup error: {message}")]
    Backup { message: String },

    #[error("Validation error: {message}")]
    Validation { message: String },
}

/// Logger configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggerConfig {
    /// Database file path
    pub database_path: PathBuf,

    /// Automatic logging settings
    pub auto_logging: AutoLoggingConfig,

    /// Export/import settings
    pub export_import: ExportImportConfig,

    /// Backup settings
    pub backup: BackupConfig,

    /// Integration settings
    pub integrations: IntegrationConfig,

    /// Validation settings
    pub validation: ValidationConfig,
}

/// Automatic logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoLoggingConfig {
    /// Enable automatic logging of completed QSOs
    pub enabled: bool,

    /// Minimum QSO duration for automatic logging (seconds)
    pub min_duration: u32,

    /// Require both signal reports for logging
    pub require_both_reports: bool,

    /// Require grid squares for logging
    pub require_grid_squares: bool,

    /// Log incomplete QSOs (for analysis)
    pub log_incomplete: bool,

    /// Auto-export after logging
    pub auto_export: bool,

    /// Log frequency for QSOs (milliseconds)
    pub log_interval: u64,
}

/// Export/import configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportImportConfig {
    /// Default export directory
    pub export_directory: PathBuf,

    /// Export formats to generate
    pub export_formats: Vec<ExportFormat>,

    /// Export file naming pattern
    pub file_naming: FileNamingPattern,

    /// Include progress data in exports
    pub include_progress_data: bool,

    /// Compress exports
    pub compress_exports: bool,

    /// Import validation level
    pub import_validation: ImportValidationLevel,
}

/// Export formats
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    /// ADIF 3.0 format
    Adif,

    /// Cabrillo contest format
    Cabrillo,

    /// CSV format
    Csv,

    /// JSON format
    Json,

    /// Ham Radio Deluxe XML
    HrdXml,

    /// Logger32 format
    Logger32,
}

/// File naming patterns
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileNamingPattern {
    /// Use timestamp: YYYYMMDD_HHMMSS
    Timestamp,

    /// Use callsign and date: CALLSIGN_YYYYMMDD
    CallsignDate,

    /// Use contest name and date: CONTEST_YYYYMMDD
    ContestDate,

    /// Custom pattern with placeholders
    Custom(String),
}

/// Import validation levels
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImportValidationLevel {
    /// Strict validation - reject any invalid data
    Strict,

    /// Lenient validation - attempt to fix minor issues
    Lenient,

    /// Minimal validation - import almost everything
    Minimal,
}

/// Backup configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    /// Enable automatic backups
    pub enabled: bool,

    /// Backup directory
    pub backup_directory: PathBuf,

    /// Backup interval (hours)
    pub backup_interval: u32,

    /// Number of backups to retain
    pub retain_count: u32,

    /// Compress backups
    pub compress: bool,

    /// Include ADIF exports in backups
    pub include_exports: bool,
}

/// Integration configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IntegrationConfig {
    /// Ham Radio Deluxe integration
    pub ham_radio_deluxe: Option<HrdIntegration>,

    /// Logger32 integration
    pub logger32: Option<Logger32Integration>,

    /// Lotw (Logbook of the World) integration
    pub lotw: Option<LotwIntegration>,

    /// QRZ.com integration
    pub qrz: Option<QrzIntegration>,

    /// eQSL integration
    pub eqsl: Option<EqslIntegration>,
}

/// Ham Radio Deluxe integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HrdIntegration {
    pub enabled: bool,
    pub database_path: PathBuf,
    pub sync_interval: u32,
}

/// Logger32 integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logger32Integration {
    pub enabled: bool,
    pub database_path: PathBuf,
    pub sync_interval: u32,
}

/// LOTW integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LotwIntegration {
    pub enabled: bool,
    pub username: String,
    pub password: String,
    pub auto_upload: bool,
}

/// QRZ.com integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrzIntegration {
    pub enabled: bool,
    pub username: String,
    pub password: String,
    pub lookup_callsigns: bool,
}

/// eQSL integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqslIntegration {
    pub enabled: bool,
    pub username: String,
    pub password: String,
    pub auto_upload: bool,
}

/// Validation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// Validate callsigns against database
    pub validate_callsigns: bool,

    /// Validate grid squares
    pub validate_grid_squares: bool,

    /// Validate frequency assignments
    pub validate_frequencies: bool,

    /// Check for duplicates
    pub check_duplicates: bool,

    /// Duplicate check time window (hours)
    pub duplicate_window_hours: u32,
}

/// Export result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    /// Export format
    pub format: ExportFormat,

    /// Output file path
    pub file_path: PathBuf,

    /// Number of QSOs exported
    pub qso_count: usize,

    /// Export timestamp
    pub timestamp: DateTime<Utc>,

    /// File size in bytes
    pub file_size: u64,

    /// Export duration in milliseconds
    pub duration_ms: u64,
}

/// Import result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    /// Input file path
    pub file_path: PathBuf,

    /// Number of QSOs imported
    pub imported_count: usize,

    /// Number of QSOs skipped (duplicates)
    pub skipped_count: usize,

    /// Number of QSOs with errors
    pub error_count: usize,

    /// Import timestamp
    pub timestamp: DateTime<Utc>,

    /// Import duration in milliseconds
    pub duration_ms: u64,

    /// Validation errors
    pub errors: Vec<String>,
}

/// QSO logger implementation
pub struct QsoLogger {
    /// Configuration
    config: LoggerConfig,

    /// Database connection
    database: AsyncQsoDatabase,

    /// ADIF processor
    adif_processor: AdifProcessor,

    /// QSO manager reference
    qso_manager: QsoManager,

    /// Event subscription
    event_receiver: RwLock<Option<broadcast::Receiver<QsoEvent>>>,

    /// Export history
    export_history: RwLock<Vec<ExportResult>>,

    /// Import history
    import_history: RwLock<Vec<ImportResult>>,
}

impl QsoLogger {
    /// Create a new QSO logger
    pub async fn new(config: LoggerConfig, qso_manager: QsoManager) -> Result<Self, LoggerError> {
        info!(
            "Initializing QSO logger with database: {:?}",
            config.database_path
        );

        // Ensure database directory exists
        if let Some(parent) = config.database_path.parent() {
            fs::create_dir_all(parent).map_err(|e| LoggerError::Io { source: e })?;
        }

        // Open database
        let database = AsyncQsoDatabase::open(&config.database_path)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        let logger = Self {
            config,
            database,
            adif_processor: AdifProcessor::new(),
            qso_manager,
            event_receiver: RwLock::new(None),
            export_history: RwLock::new(Vec::new()),
            import_history: RwLock::new(Vec::new()),
        };

        Ok(logger)
    }

    /// Start the logger
    pub async fn start(&self) -> Result<(), LoggerError> {
        info!("Starting QSO logger");

        // Subscribe to QSO events
        let receiver = self.qso_manager.subscribe();
        *self.event_receiver.write().await = Some(receiver);

        // Start background tasks
        if self.config.auto_logging.enabled {
            let logger = self.clone();
            tokio::spawn(async move {
                logger.auto_logging_loop().await;
            });
            info!("Automatic logging enabled");
        }

        if self.config.backup.enabled {
            let logger = self.clone();
            tokio::spawn(async move {
                logger.backup_loop().await;
            });
            info!("Automatic backups enabled");
        }

        Ok(())
    }

    /// Log a completed QSO
    pub async fn log_qso(&self, progress: &QsoProgress) -> Result<i64, LoggerError> {
        debug!("Logging QSO: {}", progress.metadata.qso_id);

        // Validate QSO before logging
        self.validate_qso(progress).await?;

        // Insert into database
        let id = self
            .database
            .insert_qso(progress)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        info!(
            "Logged QSO: {} -> {} (id: {})",
            progress.metadata.our_callsign,
            progress
                .metadata
                .their_callsign
                .as_deref()
                .unwrap_or("UNKNOWN"),
            id
        );

        // Auto-export if enabled
        if self.config.auto_logging.auto_export {
            self.auto_export_latest().await?;
        }

        Ok(id)
    }

    /// Update an existing QSO
    pub async fn update_qso(&self, progress: &QsoProgress) -> Result<(), LoggerError> {
        debug!("Updating QSO: {}", progress.metadata.qso_id);

        self.validate_qso(progress).await?;

        self.database
            .update_qso(progress)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        Ok(())
    }

    /// Export QSOs to ADIF format
    pub async fn export_adif<P: AsRef<Path>>(
        &self,
        output_path: P,
        filter: Option<&QsoFilter>,
    ) -> Result<ExportResult, LoggerError> {
        let start_time = Utc::now();
        info!("Exporting QSOs to ADIF: {:?}", output_path.as_ref());

        // Get QSOs to export
        let default_filter = QsoFilter::default();
        let filter = filter.unwrap_or(&default_filter);
        let options = QueryOptions::default();
        let records = self
            .database
            .search_qsos_records(filter, &options)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        // Convert to ADIF format
        let mut adif_qsos = Vec::new();
        for record in &records {
            adif_qsos.push(record.adif_data.clone());
        }

        // Create ADIF file
        let adif_file = AdifFile {
            header: crate::adif::AdifHeader {
                version: "3.1.0".to_string(),
                program_id: "pancetta-qso".to_string(),
                program_version: "0.1.0".to_string(),
                created_timestamp: start_time,
                fields: HashMap::new(),
            },
            records: adif_qsos
                .iter()
                .map(|qso| self.adif_processor.qso_to_record(qso))
                .collect(),
        };

        // Write to file
        let adif_data = self
            .adif_processor
            .generate_string(&adif_file)
            .map_err(|e| LoggerError::Adif { source: e })?;

        fs::write(&output_path, &adif_data).map_err(|e| LoggerError::Io { source: e })?;

        let file_size = adif_data.len() as u64;
        let duration_ms = (Utc::now() - start_time).num_milliseconds() as u64;

        let export_result = ExportResult {
            format: ExportFormat::Adif,
            file_path: output_path.as_ref().to_path_buf(),
            qso_count: records.len(),
            timestamp: start_time,
            file_size,
            duration_ms,
        };

        // Record export history
        self.export_history
            .write()
            .await
            .push(export_result.clone());

        info!(
            "Exported {} QSOs to ADIF in {}ms",
            records.len(),
            duration_ms
        );

        Ok(export_result)
    }

    /// Import QSOs from ADIF format
    pub async fn import_adif<P: AsRef<Path>>(
        &self,
        input_path: P,
    ) -> Result<ImportResult, LoggerError> {
        let start_time = Utc::now();
        info!("Importing QSOs from ADIF: {:?}", input_path.as_ref());

        // Read and parse ADIF file
        let adif_data =
            fs::read_to_string(&input_path).map_err(|e| LoggerError::Io { source: e })?;

        let adif_file = self
            .adif_processor
            .parse_string(&adif_data)
            .map_err(|e| LoggerError::Adif { source: e })?;

        let mut imported_count = 0;
        let mut skipped_count = 0;
        let mut error_count = 0;
        let mut errors = Vec::new();

        for (index, record) in adif_file.records.iter().enumerate() {
            match self.import_adif_record(record).await {
                Ok(ImportRecordResult::Imported) => imported_count += 1,
                Ok(ImportRecordResult::Skipped) => skipped_count += 1,
                Err(e) => {
                    error_count += 1;
                    errors.push(format!("Record {}: {}", index + 1, e));

                    if errors.len() > 100 {
                        // Limit error list size
                        errors.push("... (additional errors truncated)".to_string());
                        break;
                    }
                }
            }
        }

        let duration_ms = (Utc::now() - start_time).num_milliseconds() as u64;

        let import_result = ImportResult {
            file_path: input_path.as_ref().to_path_buf(),
            imported_count,
            skipped_count,
            error_count,
            timestamp: start_time,
            duration_ms,
            errors,
        };

        // Record import history
        self.import_history
            .write()
            .await
            .push(import_result.clone());

        info!(
            "Imported {} QSOs, skipped {}, errors {} in {}ms",
            imported_count, skipped_count, error_count, duration_ms
        );

        Ok(import_result)
    }

    /// Export QSOs to CSV format
    pub async fn export_csv<P: AsRef<Path>>(
        &self,
        output_path: P,
        filter: Option<&QsoFilter>,
    ) -> Result<ExportResult, LoggerError> {
        let start_time = Utc::now();
        info!("Exporting QSOs to CSV: {:?}", output_path.as_ref());

        // Get QSOs to export
        let default_filter = QsoFilter::default();
        let filter = filter.unwrap_or(&default_filter);
        let options = QueryOptions::default();
        let records = self
            .database
            .search_qsos_records(filter, &options)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        // Generate CSV content
        let mut csv_content = String::new();

        // Header
        csv_content.push_str(
            "QSO_DATE,TIME_ON,CALL,MODE,FREQ,BAND,RST_SENT,RST_RCVD,GRIDSQUARE,COMMENT\n",
        );

        // Records
        for record in &records {
            let qso = &record.adif_data;
            csv_content.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                qso.qso_date.format("%Y%m%d"),
                qso.qso_date.format("%H%M%S"),
                qso.call,
                qso.mode,
                qso.freq,
                qso.band,
                qso.rst_sent.as_deref().unwrap_or(""),
                qso.rst_rcvd.as_deref().unwrap_or(""),
                qso.gridsquare.as_deref().unwrap_or(""),
                qso.comment.as_deref().unwrap_or("")
            ));
        }

        // Write to file
        fs::write(&output_path, &csv_content).map_err(|e| LoggerError::Io { source: e })?;

        let file_size = csv_content.len() as u64;
        let duration_ms = (Utc::now() - start_time).num_milliseconds() as u64;

        let export_result = ExportResult {
            format: ExportFormat::Csv,
            file_path: output_path.as_ref().to_path_buf(),
            qso_count: records.len(),
            timestamp: start_time,
            file_size,
            duration_ms,
        };

        self.export_history
            .write()
            .await
            .push(export_result.clone());

        info!(
            "Exported {} QSOs to CSV in {}ms",
            records.len(),
            duration_ms
        );

        Ok(export_result)
    }

    /// Get QSO statistics
    pub async fn get_statistics(&self) -> Result<crate::database::DatabaseStats, LoggerError> {
        self.database
            .get_statistics()
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })
    }

    /// Search QSOs
    pub async fn search_qsos(
        &self,
        filter: &QsoFilter,
        options: &QueryOptions,
    ) -> Result<Vec<crate::database::QsoDatabaseRecord>, LoggerError> {
        self.database
            .search_qsos_records(filter, options)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })
    }

    /// Get export history
    pub async fn get_export_history(&self) -> Vec<ExportResult> {
        self.export_history.read().await.clone()
    }

    /// Get import history
    pub async fn get_import_history(&self) -> Vec<ImportResult> {
        self.import_history.read().await.clone()
    }

    /// Create backup
    pub async fn create_backup(&self) -> Result<PathBuf, LoggerError> {
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let backup_filename = format!("qso_backup_{}.db", timestamp);
        let backup_path = self.config.backup.backup_directory.join(backup_filename);

        info!("Creating backup: {:?}", backup_path);

        // Ensure backup directory exists
        fs::create_dir_all(&self.config.backup.backup_directory)
            .map_err(|e| LoggerError::Io { source: e })?;

        self.database
            .backup(&backup_path)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        // Clean up old backups
        self.cleanup_old_backups().await?;

        info!("Created backup: {:?}", backup_path);

        Ok(backup_path)
    }

    // Private helper methods

    async fn auto_logging_loop(&self) {
        let mut interval_timer =
            interval(Duration::from_millis(self.config.auto_logging.log_interval));

        // Get the receiver once and move it out
        let receiver_opt = {
            let mut guard = self.event_receiver.write().await;
            guard.take()
        };

        if let Some(mut receiver) = receiver_opt {
            loop {
                tokio::select! {
                    _ = interval_timer.tick() => {
                        // Periodic check for QSOs to log
                    }

                    event = receiver.recv() => {
                        match event {
                            Ok(QsoEvent::QsoCompleted { qso_id, metadata }) => {
                                if let Err(e) = self.handle_qso_completed(qso_id, metadata).await {
                                    error!("Error handling QSO completion: {}", e);
                                }
                            }
                            Ok(_) => {} // Other events not relevant
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!("Skipped {} QSO events due to lag", skipped);
                            }
                        }
                    }
                }
            }

            // Put the receiver back when done
            *self.event_receiver.write().await = Some(receiver);
        }
    }

    async fn backup_loop(&self) {
        let backup_interval = Duration::from_secs(self.config.backup.backup_interval as u64 * 3600);
        let mut interval_timer = interval(backup_interval);

        loop {
            interval_timer.tick().await;

            if let Err(e) = self.create_backup().await {
                error!("Backup failed: {}", e);
            }
        }
    }

    async fn handle_qso_completed(
        &self,
        qso_id: QsoId,
        metadata: QsoMetadata,
    ) -> Result<(), LoggerError> {
        // Get the completed QSO from the manager
        let progress = self
            .qso_manager
            .get_qso(qso_id)
            .await
            .map_err(|e| LoggerError::QsoManager { source: e })?;

        // Check if it meets auto-logging criteria
        if self.should_auto_log(&progress).await {
            self.log_qso(&progress).await?;
        }

        Ok(())
    }

    async fn should_auto_log(&self, progress: &QsoProgress) -> bool {
        let config = &self.config.auto_logging;

        // Check QSO completion
        if !progress.state.is_terminal() {
            return false;
        }

        // Check minimum duration
        if let Some(duration) = progress
            .metadata
            .end_time
            .zip(Some(progress.metadata.start_time))
            .map(|(end, start)| (end - start).num_seconds())
        {
            if duration < config.min_duration as i64 {
                return false;
            }
        }

        // Check signal reports requirement
        if config.require_both_reports {
            if progress.metadata.reports.sent.is_none()
                || progress.metadata.reports.received.is_none()
            {
                return false;
            }
        }

        // Check grid squares requirement
        if config.require_grid_squares {
            if progress.metadata.grids.ours.is_none() || progress.metadata.grids.theirs.is_none() {
                return false;
            }
        }

        true
    }

    async fn validate_qso(&self, progress: &QsoProgress) -> Result<(), LoggerError> {
        let config = &self.config.validation;

        // Validate callsign
        if config.validate_callsigns {
            if let Some(ref callsign) = progress.metadata.their_callsign {
                if !self.is_valid_callsign(callsign) {
                    return Err(LoggerError::Validation {
                        message: format!("Invalid callsign: {}", callsign),
                    });
                }
            }
        }

        // Check for duplicates
        if config.check_duplicates {
            if let Some(ref callsign) = progress.metadata.their_callsign {
                if let Some(duplicate_id) = self
                    .database
                    .check_duplicate(
                        callsign,
                        progress.metadata.frequency,
                        progress.metadata.start_time,
                        config.duplicate_window_hours,
                    )
                    .await
                    .map_err(|e| LoggerError::AsyncDatabase { source: e })?
                {
                    return Err(LoggerError::Validation {
                        message: format!("Duplicate QSO detected: {}", duplicate_id),
                    });
                }
            }
        }

        Ok(())
    }

    fn is_valid_callsign(&self, callsign: &str) -> bool {
        // Basic callsign validation
        callsign.len() >= 3
            && callsign.len() <= 10
            && callsign
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/')
    }

    async fn auto_export_latest(&self) -> Result<(), LoggerError> {
        if self
            .config
            .export_import
            .export_formats
            .contains(&ExportFormat::Adif)
        {
            let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
            let filename = format!("qso_export_{}.adi", timestamp);
            let output_path = self.config.export_import.export_directory.join(filename);

            // Ensure export directory exists
            fs::create_dir_all(&self.config.export_import.export_directory)
                .map_err(|e| LoggerError::Io { source: e })?;

            self.export_adif(output_path, None).await?;
        }

        Ok(())
    }

    async fn import_adif_record(
        &self,
        record: &crate::adif::AdifRecord,
    ) -> Result<ImportRecordResult, LoggerError> {
        // Convert ADIF record to QSO
        let adif_qso = self
            .adif_processor
            .record_to_qso(record)
            .map_err(|e| LoggerError::Adif { source: e })?;

        let metadata = self.adif_processor.adif_to_qso(&adif_qso);

        // Check for duplicates
        if let Some(ref callsign) = metadata.their_callsign {
            if let Some(_) = self
                .database
                .check_duplicate(
                    callsign,
                    metadata.frequency,
                    metadata.start_time,
                    24, // 24-hour duplicate window
                )
                .await
                .map_err(|e| LoggerError::AsyncDatabase { source: e })?
            {
                return Ok(ImportRecordResult::Skipped);
            }
        }

        // Create QSO progress for import
        let progress = QsoProgress {
            state: QsoState::Completed {
                their_callsign: metadata.their_callsign.clone().unwrap_or_default(),
                their_report: metadata.reports.received.unwrap_or(-15),
                our_report: metadata.reports.sent.unwrap_or(-15),
                frequency: metadata.frequency,
                grid_square: metadata.grids.theirs.clone(),
                completed_at: metadata.end_time.unwrap_or(metadata.start_time),
                duration_seconds: metadata
                    .end_time
                    .map(|end| (end - metadata.start_time).num_seconds() as u32)
                    .unwrap_or(0),
            },
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        // Insert QSO
        self.database
            .insert_qso(&progress)
            .await
            .map_err(|e| LoggerError::AsyncDatabase { source: e })?;

        Ok(ImportRecordResult::Imported)
    }

    async fn cleanup_old_backups(&self) -> Result<(), LoggerError> {
        let backup_dir = &self.config.backup.backup_directory;

        // Read backup directory
        let mut backups = Vec::new();
        if let Ok(entries) = fs::read_dir(backup_dir) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file()
                        && entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("qso_backup_")
                    {
                        backups.push((
                            entry.path(),
                            metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                        ));
                    }
                }
            }
        }

        // Sort by modification time (newest first)
        backups.sort_by(|a, b| b.1.cmp(&a.1));

        // Remove old backups
        if backups.len() > self.config.backup.retain_count as usize {
            for (path, _) in backups
                .into_iter()
                .skip(self.config.backup.retain_count as usize)
            {
                if let Err(e) = fs::remove_file(&path) {
                    warn!("Failed to remove old backup {:?}: {}", path, e);
                } else {
                    debug!("Removed old backup: {:?}", path);
                }
            }
        }

        Ok(())
    }
}

impl Clone for QsoLogger {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            database: self.database.clone(), // AsyncQsoDatabase supports Clone
            adif_processor: AdifProcessor::new(),
            qso_manager: self.qso_manager.clone(),
            event_receiver: RwLock::new(None),
            export_history: RwLock::new(Vec::new()),
            import_history: RwLock::new(Vec::new()),
        }
    }
}

/// Import record result
#[derive(Debug, Clone, PartialEq)]
enum ImportRecordResult {
    Imported,
    Skipped,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from("qso.db"),
            auto_logging: AutoLoggingConfig::default(),
            export_import: ExportImportConfig::default(),
            backup: BackupConfig::default(),
            integrations: IntegrationConfig::default(),
            validation: ValidationConfig::default(),
        }
    }
}

impl Default for AutoLoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration: 30, // 30 seconds minimum
            require_both_reports: true,
            require_grid_squares: false,
            log_incomplete: false,
            auto_export: false,
            log_interval: 5000, // 5 seconds
        }
    }
}

impl Default for ExportImportConfig {
    fn default() -> Self {
        Self {
            export_directory: PathBuf::from("exports"),
            export_formats: vec![ExportFormat::Adif],
            file_naming: FileNamingPattern::Timestamp,
            include_progress_data: false,
            compress_exports: false,
            import_validation: ImportValidationLevel::Lenient,
        }
    }
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backup_directory: PathBuf::from("backups"),
            backup_interval: 24, // 24 hours
            retain_count: 7,     // Keep 7 backups
            compress: false,
            include_exports: true,
        }
    }
}


impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            validate_callsigns: true,
            validate_grid_squares: true,
            validate_frequencies: true,
            check_duplicates: true,
            duplicate_window_hours: 24,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qso_manager::{
        AutoSequenceConfig, DuplicateCheckConfig, QsoManagerConfig, TimeoutConfig,
    };
    use tempfile::tempdir;
    use uuid::Uuid;

    fn test_logger_config() -> LoggerConfig {
        LoggerConfig {
            database_path: ":memory:".into(),
            auto_logging: AutoLoggingConfig {
                enabled: true,
                min_duration: 10,
                require_both_reports: false,
                require_grid_squares: false,
                log_incomplete: true,
                auto_export: false,
                log_interval: 1000,
            },
            export_import: ExportImportConfig {
                export_directory: "/tmp/exports".into(),
                ..Default::default()
            },
            backup: BackupConfig {
                enabled: false,
                backup_directory: "/tmp/backups".into(),
                ..Default::default()
            },
            integrations: IntegrationConfig::default(),
            validation: ValidationConfig {
                check_duplicates: false,
                ..Default::default()
            },
        }
    }

    fn test_qso_manager_config() -> QsoManagerConfig {
        QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            timeouts: TimeoutConfig::default(),
            contest_mode: None,
            auto_sequence: AutoSequenceConfig::default(),
            duplicate_checking: DuplicateCheckConfig::default(),
        }
    }

    async fn create_test_logger() -> QsoLogger {
        let qso_manager = QsoManager::new(test_qso_manager_config());
        QsoLogger::new(test_logger_config(), qso_manager)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_logger_creation() {
        let logger = create_test_logger().await;
        assert!(logger.config.auto_logging.enabled);
    }

    #[tokio::test]
    async fn test_qso_logging() {
        let logger = create_test_logger().await;

        // Create test QSO
        let qso_id = Uuid::new_v4();
        let now = Utc::now();

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some("K1DEF".to_string()),
            frequency: 14074000.0,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: Some(now + chrono::Duration::minutes(2)),
            reports: SignalReports {
                sent: Some(-15),
                received: Some(-12),
            },
            grids: GridSquares {
                ours: Some("FN42".to_string()),
                theirs: Some("FN31".to_string()),
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: Some("Test QSO".to_string()),
        };

        let progress = QsoProgress {
            state: QsoState::Completed {
                their_callsign: "K1DEF".to_string(),
                their_report: -12,
                our_report: -15,
                frequency: 14074000.0,
                grid_square: Some("FN31".to_string()),
                completed_at: now,
                duration_seconds: 120,
            },
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        // Log QSO
        let id = logger.log_qso(&progress).await.unwrap();
        assert!(id > 0);

        // Verify it was logged
        let stats = logger.get_statistics().await.unwrap();
        assert_eq!(stats.total_qsos, 1);
    }

    #[tokio::test]
    async fn test_adif_export() {
        let logger = create_test_logger().await;
        let temp_dir = tempdir().unwrap();
        let export_path = temp_dir.path().join("test_export.adi");

        // Create and log a test QSO first
        let qso_id = Uuid::new_v4();
        let now = Utc::now();

        let metadata = QsoMetadata {
            qso_id,
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some("K1DEF".to_string()),
            frequency: 14074000.0,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: Some(now + chrono::Duration::minutes(2)),
            reports: SignalReports {
                sent: Some(-15),
                received: Some(-12),
            },
            grids: GridSquares {
                ours: Some("FN42".to_string()),
                theirs: Some("FN31".to_string()),
            },
            contest_info: None,
            tags: HashMap::new(),
            notes: None,
        };

        let progress = QsoProgress {
            state: QsoState::Completed {
                their_callsign: "K1DEF".to_string(),
                their_report: -12,
                our_report: -15,
                frequency: 14074000.0,
                grid_square: Some("FN31".to_string()),
                completed_at: now,
                duration_seconds: 120,
            },
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        logger.log_qso(&progress).await.unwrap();

        // Export to ADIF
        let result = logger.export_adif(&export_path, None).await.unwrap();
        assert_eq!(result.qso_count, 1);
        assert!(export_path.exists());

        // Verify file contents
        let exported_data = fs::read_to_string(&export_path).unwrap();
        assert!(exported_data.contains("K1DEF"));
        assert!(exported_data.contains("FT8"));
    }
}
