//! Async-safe QSO logging system using the async database
//!
//! This module provides async-safe QSO logging with proper Send/Sync support
//! for tokio spawns, using the sqlx-based async database.

use crate::adif::{AdifFile, AdifProcessor};
use crate::async_database::{AsyncDatabaseError, QsoDatabase, QsoFilter, QueryOptions, SortOrder};
use crate::qso_manager::{QsoEvent, QsoManager};
use crate::states::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Logger configuration types (formerly in logger.rs, now the canonical location)
// ---------------------------------------------------------------------------

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

    /// LOTW (Logbook of the World) integration
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

// ---------------------------------------------------------------------------
// Default impls for config types
// ---------------------------------------------------------------------------

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
            min_duration: 30,
            require_both_reports: true,
            require_grid_squares: false,
            log_incomplete: false,
            auto_export: false,
            log_interval: 5000,
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
            backup_interval: 24,
            retain_count: 7,
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

/// Database statistics
#[derive(Debug, Clone)]
pub struct DatabaseStatistics {
    pub total_qsos: usize,
    pub unique_callsigns: usize,
    pub bands_worked: usize,
    pub modes_worked: usize,
    pub countries_worked: usize,
    pub oldest_qso: Option<DateTime<Utc>>,
    pub newest_qso: Option<DateTime<Utc>>,
    pub database_size_bytes: u64,
}

/// Async logger errors
#[derive(Debug, Error)]
pub enum AsyncLoggerError {
    #[error("Database error: {0}")]
    Database(#[from] AsyncDatabaseError),

    #[error("ADIF error: {0}")]
    Adif(#[from] crate::adif::AdifError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {message}")]
    Configuration { message: String },
}

/// Async QSO logger with Send/Sync support
#[derive(Clone)]
pub struct QsoLogger {
    /// Configuration
    config: Arc<LoggerConfig>,

    /// Async database connection
    database: Arc<QsoDatabase>,

    /// ADIF processor
    adif_processor: Arc<AdifProcessor>,

    /// QSO manager reference
    qso_manager: Arc<QsoManager>,

    /// Event subscription
    event_receiver: Arc<RwLock<Option<broadcast::Receiver<QsoEvent>>>>,

    /// Export history
    export_history: Arc<RwLock<Vec<ExportResult>>>,

    /// Import history
    import_history: Arc<RwLock<Vec<ImportResult>>>,
}

impl QsoLogger {
    /// Create a new async QSO logger
    pub async fn new(
        config: LoggerConfig,
        qso_manager: QsoManager,
    ) -> Result<Self, AsyncLoggerError> {
        let database = QsoDatabase::open(&config.database_path).await?;

        Ok(Self {
            config: Arc::new(config),
            database: Arc::new(database),
            adif_processor: Arc::new(AdifProcessor::new()),
            qso_manager: Arc::new(qso_manager),
            event_receiver: Arc::new(RwLock::new(None)),
            export_history: Arc::new(RwLock::new(Vec::new())),
            import_history: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Start the logger with background tasks
    pub async fn start(&self) -> Result<(), AsyncLoggerError> {
        info!("Starting async QSO logger");

        // Subscribe to QSO events
        let receiver = self.qso_manager.subscribe();
        *self.event_receiver.write().await = Some(receiver);

        // Start background tasks (now properly Send/Sync!)
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

    /// Log a QSO progress record
    pub async fn log_qso(&self, progress: &QsoProgress) -> Result<(), AsyncLoggerError> {
        // Check if QSO already exists
        match self.database.get_qso(progress.metadata.qso_id).await {
            Ok(_) => {
                // Update existing
                self.database.update_qso(progress).await?;
                debug!("Updated QSO: {}", progress.metadata.qso_id);
            }
            Err(AsyncDatabaseError::QsoNotFound { .. }) => {
                // Insert new
                self.database.insert_qso(progress).await?;
                info!("Logged new QSO: {}", progress.metadata.qso_id);
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    /// Export QSOs to ADIF format
    pub async fn export_adif<P: AsRef<Path>>(
        &self,
        output_path: P,
        filter: Option<&QsoFilter>,
    ) -> Result<ExportResult, AsyncLoggerError> {
        let start_time = Utc::now();

        // Get QSOs to export
        let default_filter = QsoFilter::default();
        let filter = filter.unwrap_or(&default_filter);
        let options = QueryOptions::default();
        let records = self.database.search_qsos(filter, &options).await?;

        // Convert to ADIF format
        let mut adif_file = AdifFile {
            header: Default::default(),
            records: Vec::new(),
        };

        for progress in &records {
            let adif_qso = self
                .adif_processor
                .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
            let adif_record = self.adif_processor.qso_to_record(&adif_qso);
            adif_file.records.push(adif_record);
        }

        // Write to file
        let adif_content = self.adif_processor.generate_string(&adif_file)?;
        tokio::fs::write(&output_path, &adif_content).await?;

        let file_size = adif_content.len() as u64;
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
    ) -> Result<ImportResult, AsyncLoggerError> {
        let start_time = Utc::now();

        // Read ADIF file
        let adif_content = tokio::fs::read_to_string(&input_path).await?;
        let adif_file = self.adif_processor.parse_string(&adif_content)?;

        let _total_count = adif_file.records.len();
        let mut imported_count = 0;
        let mut duplicate_count = 0;
        let mut error_count = 0;

        // Import each QSO
        for record in &adif_file.records {
            match self.adif_processor.record_to_qso(record) {
                Ok(adif_qso) => {
                    // Convert to QsoProgress
                    let progress = self.adif_qso_to_progress(&adif_qso);

                    // Try to insert
                    match self.database.insert_qso(&progress).await {
                        Ok(_) => imported_count += 1,
                        Err(AsyncDatabaseError::DuplicateQso { .. }) => duplicate_count += 1,
                        Err(e) => {
                            error!("Error importing QSO: {}", e);
                            error_count += 1;
                        }
                    }
                }
                Err(e) => {
                    error!("Error parsing ADIF record: {}", e);
                    error_count += 1;
                }
            }
        }

        let duration_ms = (Utc::now() - start_time).num_milliseconds() as u64;

        let import_result = ImportResult {
            file_path: input_path.as_ref().to_path_buf(),
            imported_count,
            skipped_count: duplicate_count,
            error_count,
            timestamp: start_time,
            duration_ms,
            errors: Vec::new(), // TODO: Collect actual error messages
        };

        // Record import history
        self.import_history
            .write()
            .await
            .push(import_result.clone());

        info!(
            "Imported {} QSOs from ADIF ({} duplicates, {} errors) in {}ms",
            imported_count, duplicate_count, error_count, duration_ms
        );

        Ok(import_result)
    }

    /// Get database statistics
    pub async fn get_statistics(&self) -> Result<DatabaseStatistics, AsyncLoggerError> {
        let total_qsos = self.database.get_qso_count().await? as usize;

        // Get date range
        let filter = QsoFilter::default();
        let oldest_options = QueryOptions {
            limit: Some(1),
            sort_order: SortOrder::Ascending,
            ..QueryOptions::default()
        };

        let oldest = self
            .database
            .search_qsos(&filter, &oldest_options)
            .await?
            .first()
            .map(|q| q.metadata.start_time);

        let newest_options = QueryOptions {
            limit: Some(1),
            sort_order: SortOrder::Descending,
            ..QueryOptions::default()
        };

        let newest = self
            .database
            .search_qsos(&filter, &newest_options)
            .await?
            .first()
            .map(|q| q.metadata.start_time);

        Ok(DatabaseStatistics {
            total_qsos,
            unique_callsigns: 0, // Would need additional query
            bands_worked: 0,     // Would need additional query
            modes_worked: 0,     // Would need additional query
            countries_worked: 0, // Would need additional query
            oldest_qso: oldest,
            newest_qso: newest,
            database_size_bytes: 0, // Would need file system check
        })
    }

    /// Create a backup of the database
    pub async fn create_backup(&self) -> Result<PathBuf, AsyncLoggerError> {
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let backup_filename = format!("qso_backup_{}.db", timestamp);
        let backup_path = self.config.backup.backup_directory.join(backup_filename);

        // Ensure backup directory exists
        tokio::fs::create_dir_all(&self.config.backup.backup_directory).await?;

        // Perform backup
        self.database.backup(&backup_path).await?;

        info!("Created database backup: {}", backup_path.display());

        // Clean old backups if configured
        if self.config.backup.retain_count > 0 {
            self.cleanup_old_backups().await?;
        }

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
                            Err(broadcast::error::RecvError::Closed) => {
                                warn!("QSO event channel closed");
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!("Skipped {} QSO events due to lag", skipped);
                            }
                            _ => {}
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
        _qso_id: QsoId,
        metadata: QsoMetadata,
    ) -> Result<(), AsyncLoggerError> {
        // Extract signal reports from metadata (populated during state transition)
        let their_report = metadata.reports.received.unwrap_or(0);
        let our_report = metadata.reports.sent.unwrap_or(0);
        let grid_square = metadata.grids.theirs.clone();
        let completed_at = metadata.end_time.unwrap_or_else(Utc::now);
        let duration_seconds = (completed_at - metadata.start_time).num_seconds().max(0) as u32;

        // Create a QsoProgress from the metadata
        let progress = QsoProgress {
            state: QsoState::Completed {
                their_callsign: metadata.their_callsign.clone().unwrap_or_default(),
                their_report,
                our_report,
                frequency: metadata.frequency,
                grid_square,
                completed_at,
                duration_seconds,
            },
            state_history: vec![],
            messages: vec![],
            metadata,
        };

        self.log_qso(&progress).await?;
        Ok(())
    }

    async fn cleanup_old_backups(&self) -> Result<(), AsyncLoggerError> {
        let mut entries = tokio::fs::read_dir(&self.config.backup.backup_directory).await?;
        let mut backups = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("db") {
                if let Ok(metadata) = entry.metadata().await {
                    if let Ok(modified) = metadata.modified() {
                        backups.push((path, modified));
                    }
                }
            }
        }

        // Sort by modification time (newest first)
        backups.sort_by(|a, b| b.1.cmp(&a.1));

        // Remove old backups
        while backups.len() > self.config.backup.retain_count as usize {
            if let Some((path, _)) = backups.pop() {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    error!("Error removing old backup {}: {}", path.display(), e);
                } else {
                    info!("Removed old backup: {}", path.display());
                }
            }
        }

        Ok(())
    }

    fn adif_qso_to_progress(&self, adif_qso: &crate::adif::AdifQso) -> QsoProgress {
        QsoProgress {
            state: QsoState::Completed {
                their_callsign: adif_qso.call.clone(),
                their_report: adif_qso
                    .rst_rcvd
                    .as_ref()
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(0),
                our_report: adif_qso
                    .rst_sent
                    .as_ref()
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(0),
                frequency: adif_qso.freq * 1_000_000.0,
                grid_square: adif_qso.gridsquare.clone(),
                completed_at: adif_qso.qso_date_off.unwrap_or(adif_qso.qso_date),
                duration_seconds: 0,
            },
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id: uuid::Uuid::new_v4(),
                our_callsign: if adif_qso.station_callsign.is_empty() {
                    "UNKNOWN".to_string()
                } else {
                    adif_qso.station_callsign.clone()
                },
                their_callsign: Some(adif_qso.call.clone()),
                frequency: adif_qso.freq * 1_000_000.0,
                mode: adif_qso.mode.clone(),
                start_time: adif_qso.qso_date,
                end_time: adif_qso.qso_date_off,
                reports: SignalReports {
                    sent: adif_qso.rst_sent.as_ref().and_then(|r| r.parse().ok()),
                    received: adif_qso.rst_rcvd.as_ref().and_then(|r| r.parse().ok()),
                },
                grids: GridSquares {
                    ours: adif_qso.my_gridsquare.clone(),
                    theirs: adif_qso.gridsquare.clone(),
                },
                contest_info: if adif_qso.contest_id.is_some() {
                    Some(ContestInfo {
                        contest_name: adif_qso.contest_id.clone().unwrap_or_default(),
                        category: String::new(),
                        serials: ContestSerials {
                            sent: adif_qso.stx,
                            received: adif_qso.srx,
                        },
                        points: 0,
                        multiplier: None,
                    })
                } else {
                    None
                },
                tags: std::collections::HashMap::new(),
                notes: adif_qso.notes.clone(),
                tx_parity: None,
                initiated_by: Default::default(),
                role: Default::default(),
                call_count: 0,
                first_call_at: None,
                last_call_at: None,
                progressed_this_cycle: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qso_manager::QsoManagerConfig;

    fn test_logger_config() -> LoggerConfig {
        let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let db_path = tmp_dir.keep().join("test_qso.db");
        LoggerConfig {
            database_path: db_path,
            ..LoggerConfig::default()
        }
    }

    #[tokio::test]
    async fn test_async_logger_creation() {
        let config = test_logger_config();
        let qso_manager = QsoManager::new(QsoManagerConfig::default());

        let logger = QsoLogger::new(config, qso_manager).await;
        assert!(logger.is_ok());
    }

    #[tokio::test]
    async fn test_async_logger_is_send_sync() {
        // This test verifies that QsoLogger implements Send + Sync
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<QsoLogger>();
    }

    #[tokio::test]
    async fn test_async_spawns_work() {
        let config = test_logger_config();
        let qso_manager = QsoManager::new(QsoManagerConfig::default());

        let logger = QsoLogger::new(config, qso_manager).await.unwrap();

        // This should now compile without Send/Sync errors!
        let logger_clone = logger.clone();
        let handle = tokio::spawn(async move { logger_clone.get_statistics().await });

        let result = handle.await;
        assert!(result.is_ok());
    }
}
