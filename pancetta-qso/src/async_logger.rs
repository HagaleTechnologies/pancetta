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
use tokio::time::{interval, interval_at, Duration, Instant};
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

    /// Layer 2 timeline persistence (docs/observability-diagnostics-plan.md):
    /// when true, a completed QSO's `state_history`/`messages` are written
    /// into the `qsos` row's `progress_data`, and a failed QSO's timeline is
    /// written into the separate `qso_events` table (failed QSOs never get a
    /// row in `qsos` — that table is the confirmed-contact log). Mirrors
    /// `pancetta_config::DatabaseConfig::persist_qso_timeline` — threaded in
    /// by the coordinator at startup. Defaults to `false`: an absent
    /// `[database]` config section changes nothing.
    pub persist_qso_timeline: bool,
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
            persist_qso_timeline: false,
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

/// How long after `start()` the first automatic backup waits. Startup is
/// when the coordinator may rebuild `qso.db` from ADIF; backing up in that
/// window raced the rebuild ("file is not a database" / "malformed").
/// Short enough that a station restarted more often than
/// `backup_interval` still gets backups.
const STARTUP_BACKUP_DELAY: Duration = Duration::from_secs(300);

/// Delay before the first automatic backup: [`STARTUP_BACKUP_DELAY`], or the
/// configured period if that is shorter.
fn first_backup_delay(period: Duration) -> Duration {
    STARTUP_BACKUP_DELAY.min(period)
}

/// A relative `backup_directory` is anchored next to an absolute database
/// path (`~/.pancetta/backups` for `~/.pancetta/qso.db`), or at the fixed
/// state directory `~/.pancetta` when the database path is relative or has
/// no directory (`:memory:`) — never relative to the process cwd (PAN-126).
/// An absolute directory is used as-is. Only a home-less environment keeps
/// the relative path.
fn resolve_backup_directory(dir: &Path, database_path: &Path) -> PathBuf {
    if dir.is_absolute() {
        return dir.to_path_buf();
    }
    match database_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() && parent.is_absolute() => parent.join(dir),
        _ => match dirs::home_dir() {
            Some(home) => home.join(".pancetta").join(dir),
            None => dir.to_path_buf(),
        },
    }
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
        let backup_directory = self.backup_directory();
        let backup_path = backup_directory.join(backup_filename);

        // Ensure backup directory exists
        tokio::fs::create_dir_all(&backup_directory).await?;

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
                            Ok(QsoEvent::QsoCompleted {
                                qso_id,
                                metadata,
                                state_history,
                                messages,
                            }) => {
                                if let Err(e) = self
                                    .handle_qso_completed(qso_id, metadata, state_history, messages)
                                    .await
                                {
                                    error!("Error handling QSO completion: {}", e);
                                }
                            }
                            Ok(QsoEvent::QsoFailed {
                                qso_id,
                                reason,
                                metadata,
                                state_history,
                                messages,
                                ..
                            }) => {
                                if let Err(e) = self
                                    .handle_qso_failed(qso_id, reason, metadata, state_history, messages)
                                    .await
                                {
                                    error!("Error handling QSO failure: {}", e);
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

    fn backup_directory(&self) -> PathBuf {
        resolve_backup_directory(
            &self.config.backup.backup_directory,
            &self.config.database_path,
        )
    }

    async fn backup_loop(&self) {
        let backup_interval = Duration::from_secs(self.config.backup.backup_interval as u64 * 3600);
        // Not `interval()`: its first tick completes immediately.
        let mut interval_timer = interval_at(
            Instant::now() + first_backup_delay(backup_interval),
            backup_interval,
        );

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
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    ) -> Result<(), AsyncLoggerError> {
        // Extract signal reports from metadata (populated during state transition)
        let their_report = metadata.reports.received.unwrap_or(0);
        let our_report = metadata.reports.sent.unwrap_or(0);
        let grid_square = metadata.grids.theirs.clone();
        let completed_at = metadata.end_time.unwrap_or_else(Utc::now);
        let duration_seconds = (completed_at - metadata.start_time).num_seconds().max(0) as u32;

        // Layer 2 timeline persistence (docs/observability-diagnostics-plan.md
        // §"Persist the timeline"): only populate the real state_history /
        // messages when the operator has opted in via
        // `[database].persist_qso_timeline`. Off by default so a QSO row's
        // `progress_data` blob stays exactly as small as it is today.
        let (state_history, messages) = if self.config.persist_qso_timeline {
            (state_history, messages)
        } else {
            (Vec::new(), Vec::new())
        };

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
            state_history: state_history.clone(),
            messages: messages.clone(),
            metadata: metadata.clone(),
        };

        self.log_qso(&progress).await?;
        debug!("Logged completed QSO {} to database", qso_id);

        // Also record into `qso_events` (same table failed QSOs use) so a
        // caller reconstructing "what happened" for ANY terminal QSO —
        // completed or failed — has one uniform read path
        // (`QsoDatabase::get_qso_timeline`), rather than having to know
        // whether to look in `qsos.progress_data` or `qso_events` depending
        // on outcome.
        if self.config.persist_qso_timeline {
            self.database
                .insert_qso_timeline(
                    qso_id,
                    metadata.their_callsign.as_deref(),
                    "completed",
                    None,
                    &state_history,
                    &messages,
                )
                .await?;
        }

        Ok(())
    }

    /// Persist a failed QSO's timeline (Layer 2). Unlike `handle_qso_completed`,
    /// this never touches the `qsos` contact-log table — a failed QSO is not a
    /// logged contact (it would corrupt duplicate-checks / ADIF export /
    /// worked-station seeding, all of which read that table). Instead it goes
    /// into the separate `qso_events` table, gated by the same
    /// `persist_qso_timeline` flag as the completed path. A no-op when the
    /// flag is off — failed QSOs get no persistent record at all, matching
    /// today's behavior.
    async fn handle_qso_failed(
        &self,
        qso_id: QsoId,
        reason: QsoFailureReason,
        metadata: QsoMetadata,
        state_history: Vec<StateTransition>,
        messages: Vec<QsoMessage>,
    ) -> Result<(), AsyncLoggerError> {
        if !self.config.persist_qso_timeline {
            return Ok(());
        }

        self.database
            .insert_qso_timeline(
                qso_id,
                metadata.their_callsign.as_deref(),
                "failed",
                Some(&format!("{reason:?}")),
                &state_history,
                &messages,
            )
            .await?;
        debug!(
            "Persisted timeline for failed QSO {} ({:?}, {} transitions, {} messages)",
            qso_id,
            reason,
            state_history.len(),
            messages.len()
        );
        Ok(())
    }

    async fn cleanup_old_backups(&self) -> Result<(), AsyncLoggerError> {
        let mut entries = tokio::fs::read_dir(self.backup_directory()).await?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qso_manager::QsoManagerConfig;
    use std::collections::HashMap;
    use uuid::Uuid;

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
    async fn test_relative_backup_directory_resolves_next_to_the_database() {
        // The default `backups` used to resolve against the process cwd, so a
        // `cargo run` from a checkout wrote backups into the repo.
        let config = test_logger_config();
        let db_dir = config.database_path.parent().unwrap().to_path_buf();
        assert!(config.backup.backup_directory.is_relative());
        let logger = QsoLogger::new(config, QsoManager::new(QsoManagerConfig::default()))
            .await
            .unwrap();

        let backup = logger.create_backup().await.unwrap();
        assert_eq!(backup.parent().unwrap(), db_dir.join("backups"));
        assert!(backup.exists());
    }

    #[test]
    fn test_resolve_backup_directory() {
        // A platform-native absolute root: on Windows `/home/...` has no
        // drive prefix, so it is not absolute.
        let root = if cfg!(windows) {
            Path::new(r"C:\")
        } else {
            Path::new("/")
        };
        let db = root.join("home/op/.pancetta/qso.db");
        assert_eq!(
            resolve_backup_directory(Path::new("backups"), &db),
            root.join("home/op/.pancetta/backups")
        );
        let abs_dir = root.join("srv/bk");
        assert_eq!(resolve_backup_directory(&abs_dir, &db), abs_dir);
        // No absolute database directory (`:memory:`, or a relative database
        // path): anchor at the fixed state directory, never the cwd (PAN-126).
        let state_backups = dirs::home_dir().unwrap().join(".pancetta").join("backups");
        assert_eq!(
            resolve_backup_directory(Path::new("backups"), Path::new(":memory:")),
            state_backups.clone()
        );
        assert_eq!(
            resolve_backup_directory(Path::new("backups"), Path::new("qso.db")),
            state_backups
        );
    }

    #[tokio::test]
    async fn test_backup_loop_does_not_back_up_at_startup() {
        // sophon 2026-10-07 / 2026-10-15: the first `interval()` tick fired
        // immediately and raced the startup ADIF->index rebuild of the same
        // qso.db, failing with "file is not a database" / "malformed".
        let mut config = test_logger_config();
        let backup_dir = config.database_path.parent().unwrap().join("bk");
        config.backup.backup_directory = backup_dir.clone();
        let logger = QsoLogger::new(config, QsoManager::new(QsoManagerConfig::default()))
            .await
            .unwrap();
        logger.start().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let count = std::fs::read_dir(&backup_dir)
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(
            count, 0,
            "no backup may run in the first moments after start"
        );
    }

    #[test]
    fn test_first_backup_delay_is_bounded_by_the_interval() {
        assert_eq!(
            first_backup_delay(Duration::from_secs(24 * 3600)),
            STARTUP_BACKUP_DELAY
        );
        assert_eq!(
            first_backup_delay(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert!(STARTUP_BACKUP_DELAY >= Duration::from_secs(60));
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

    // --- Layer 2 timeline persistence (docs/observability-diagnostics-plan.md
    // §"Persist the timeline") -------------------------------------------
    //
    // These exercise the actual discard sites this task fixed:
    // `handle_qso_completed` used to hard-code `state_history: vec![],
    // messages: vec![]` regardless of what the QsoCompleted event carried,
    // and failed QSOs were never persisted anywhere at all. Both are now
    // gated by `LoggerConfig::persist_qso_timeline` (default false, which
    // must reproduce the old discard-everything behavior exactly).

    fn test_metadata(qso_id: QsoId, their_callsign: &str) -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id,
            our_callsign: "W1ABC".to_string(),
            their_callsign: Some(their_callsign.to_string()),
            frequency: 14074000.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: Some(now),
            reports: SignalReports {
                sent: Some(-9),
                received: Some(-12),
            },
            grids: GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
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

    fn non_trivial_timeline(now: DateTime<Utc>) -> (Vec<StateTransition>, Vec<QsoMessage>) {
        let state_history = vec![StateTransition {
            from_state: QsoState::Idle,
            to_state: QsoState::CallingCq {
                frequency: 1500.0,
                started_at: now,
                call_count: 1,
            },
            timestamp: now,
            reason: TransitionReason::UserAction,
        }];
        let messages = vec![QsoMessage {
            timestamp: now,
            direction: MessageDirection::Sent,
            message_type: MessageType::Cq {
                callsign: "W1ABC".to_string(),
                grid: Some("FN42".to_string()),
            },
            raw_text: "CQ W1ABC FN42".to_string(),
            signal_strength: None,
            frequency: 1500.0,
        }];
        (state_history, messages)
    }

    #[tokio::test]
    async fn completed_qso_timeline_persisted_only_when_flag_enabled() {
        let (state_history, messages) = non_trivial_timeline(Utc::now());

        // Flag off (default): the qsos row must carry an EMPTY timeline,
        // exactly like the pre-fix hard-coded `vec![]`, and no qso_events
        // row should exist.
        let off_config = test_logger_config();
        let qso_manager = QsoManager::new(QsoManagerConfig::default());
        let off_logger = QsoLogger::new(off_config, qso_manager.clone())
            .await
            .unwrap();
        let qso_id = Uuid::new_v4();
        off_logger
            .handle_qso_completed(
                qso_id,
                test_metadata(qso_id, "K1DEF"),
                state_history.clone(),
                messages.clone(),
            )
            .await
            .unwrap();
        let stored = off_logger.database.get_qso(qso_id).await.unwrap();
        assert!(
            stored.state_history.is_empty() && stored.messages.is_empty(),
            "persist_qso_timeline=false must reproduce the old empty-vec behavior"
        );
        assert!(
            off_logger
                .database
                .get_qso_timeline(qso_id)
                .await
                .unwrap()
                .is_none(),
            "persist_qso_timeline=false must not write a qso_events row either"
        );

        // Flag on: the qsos row AND the qso_events row both carry the real
        // timeline, byte-for-byte.
        let mut on_config = test_logger_config();
        on_config.persist_qso_timeline = true;
        let qso_manager2 = QsoManager::new(QsoManagerConfig::default());
        let on_logger = QsoLogger::new(on_config, qso_manager2).await.unwrap();
        let qso_id2 = Uuid::new_v4();
        on_logger
            .handle_qso_completed(
                qso_id2,
                test_metadata(qso_id2, "K1DEF"),
                state_history.clone(),
                messages.clone(),
            )
            .await
            .unwrap();
        let stored2 = on_logger.database.get_qso(qso_id2).await.unwrap();
        assert_eq!(stored2.state_history, state_history);
        assert_eq!(stored2.messages, messages);

        let timeline = on_logger
            .database
            .get_qso_timeline(qso_id2)
            .await
            .unwrap()
            .expect("persist_qso_timeline=true must write a qso_events row too");
        assert_eq!(timeline.outcome, "completed");
        assert_eq!(timeline.state_history, state_history);
        assert_eq!(timeline.messages, messages);
    }

    #[tokio::test]
    async fn failed_qso_timeline_persisted_only_when_flag_enabled() {
        let (state_history, messages) = non_trivial_timeline(Utc::now());

        // Flag off (default): failed QSOs get NO persistent record at all —
        // matches today's behavior (QsoFailed was never logged anywhere).
        let off_config = test_logger_config();
        let qso_manager = QsoManager::new(QsoManagerConfig::default());
        let off_logger = QsoLogger::new(off_config, qso_manager.clone())
            .await
            .unwrap();
        let qso_id = Uuid::new_v4();
        off_logger
            .handle_qso_failed(
                qso_id,
                QsoFailureReason::Timeout,
                test_metadata(qso_id, "K1DEF"),
                state_history.clone(),
                messages.clone(),
            )
            .await
            .unwrap();
        assert!(
            off_logger
                .database
                .get_qso_timeline(qso_id)
                .await
                .unwrap()
                .is_none(),
            "persist_qso_timeline=false must not persist failed-QSO timelines"
        );
        // And it must never leak into the confirmed-contact log either.
        assert!(off_logger.database.get_qso(qso_id).await.is_err());

        // Flag on: the failed QSO's full timeline round-trips via
        // qso_events, keyed by qso_id, and the contact log is still
        // untouched (a Failed QSO is never a logged contact).
        let mut on_config = test_logger_config();
        on_config.persist_qso_timeline = true;
        let qso_manager2 = QsoManager::new(QsoManagerConfig::default());
        let on_logger = QsoLogger::new(on_config, qso_manager2).await.unwrap();
        let qso_id2 = Uuid::new_v4();
        on_logger
            .handle_qso_failed(
                qso_id2,
                QsoFailureReason::Timeout,
                test_metadata(qso_id2, "K1DEF"),
                state_history.clone(),
                messages.clone(),
            )
            .await
            .unwrap();

        let timeline = on_logger
            .database
            .get_qso_timeline(qso_id2)
            .await
            .unwrap()
            .expect("persist_qso_timeline=true must persist a failed QSO's timeline");
        assert_eq!(timeline.outcome, "failed");
        assert_eq!(timeline.reason.as_deref(), Some("Timeout"));
        assert_eq!(timeline.callsign.as_deref(), Some("K1DEF"));
        assert_eq!(timeline.state_history, state_history);
        assert_eq!(timeline.messages, messages);
        assert!(
            on_logger.database.get_qso(qso_id2).await.is_err(),
            "a failed QSO must never appear in the confirmed-contact `qsos` table"
        );
    }
}
