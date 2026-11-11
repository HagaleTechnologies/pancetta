//! Async-safe SQLite database integration using sqlx
//!
//! This module provides async-safe persistent storage for QSO data using SQLite
//! through the sqlx library, enabling proper Send/Sync support for tokio spawns.

use crate::adif::{AdifProcessor, AdifQso, AdifRecord};
use crate::states::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteConnectOptions, Connection};
use sqlx::{
    sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow,
        SqliteSynchronous,
    },
    Row,
};
use std::collections::HashMap;
<<<<<<< HEAD
use std::path::{Path, PathBuf};
=======
use std::path::Path;
use std::str::FromStr;
>>>>>>> e5a553cfab7c125c74bd606b208826e27e18aa63
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared query types (formerly in database.rs, now the canonical location)
// ---------------------------------------------------------------------------

/// Database query filters
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct QsoFilter {
    /// Filter by callsign pattern
    pub callsign_pattern: Option<String>,

    /// Filter by date range
    pub date_range: Option<DateRange>,

    /// Filter by frequency range (Hz)
    pub frequency_range: Option<FrequencyRange>,

    /// Filter by band
    pub band: Option<String>,

    /// Filter by mode
    pub mode: Option<String>,

    /// Filter by grid square pattern
    pub grid_pattern: Option<String>,

    /// Filter by contest
    pub contest_id: Option<String>,

    /// Filter by QSL status
    pub qsl_status: Option<QslStatus>,

    /// Filter by confirmation status
    pub confirmed: Option<bool>,

    /// Include only QSOs with minimum signal strength
    pub min_signal_strength: Option<i8>,

    /// Custom SQL WHERE clause
    pub custom_where: Option<String>,
}

/// Date range filter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateRange {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// Frequency range filter (Hz)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyRange {
    pub min: f64,
    pub max: f64,
}

/// QSL status filter
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QslStatus {
    Sent,
    Received,
    Confirmed,
    Requested,
    NotSent,
    NotReceived,
}

/// Database query options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryOptions {
    /// Sort order
    pub sort_by: Option<SortField>,

    /// Sort direction
    pub sort_order: SortOrder,

    /// Limit number of results
    pub limit: Option<u32>,

    /// Skip number of results (pagination)
    pub offset: Option<u32>,

    /// Include related data
    pub include_metadata: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            sort_by: Some(SortField::QsoDate),
            sort_order: SortOrder::Descending,
            limit: None,
            offset: None,
            include_metadata: true,
        }
    }
}

/// Sort fields
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SortField {
    QsoDate,
    Callsign,
    Frequency,
    Mode,
    Band,
    SignalReport,
    CreatedAt,
}

/// Sort order
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SortOrder {
    Ascending,
    Descending,
}

/// PAN-347: one indexed contact, as the local needs lookup reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkedContact {
    /// Uppercase band ("20M"). The record's logged BAND when the index band
    /// is a `…MHZ` fallback bucket (a record imported without FREQ).
    pub band: String,
    /// Uppercase, trimmed callsign as logged.
    pub callsign: String,
    /// Uppercase logged mode (SUBMODE preferred on import); empty when the
    /// row has none.
    pub mode: String,
    /// QSL_RCVD or LOTW_QSL_RCVD is Y or V. eQSL does not count.
    pub confirmed: bool,
    pub start_time: DateTime<Utc>,
}

/// PAN-347: an ADIF QSL_RCVD / LOTW_QSL_RCVD value that confirms the contact.
pub fn is_confirmed_qsl_value(v: &str) -> bool {
    matches!(v.trim().to_ascii_uppercase().as_str(), "Y" | "V")
}

/// PAN-347: SQL twin of `is_confirmed_qsl_value`, over the two fields it reads.
const CONFIRMED_QSO_SQL: &str =
    "upper(trim(coalesce(json_extract(metadata, '$.tags.QSL_RCVD'), ''))) IN ('Y','V') \
     OR upper(trim(coalesce(json_extract(metadata, '$.tags.LOTW_QSL_RCVD'), ''))) IN ('Y','V')";

/// PAN-347: the index band, or the logged BAND tag when the index band is a
/// `…MHZ` fallback bucket.
fn logged_band(adif_band: &str, band_tag: Option<&str>) -> String {
    let b = adif_band.trim().to_uppercase();
    if b.ends_with("MHZ") {
        if let Some(tag) = band_tag.map(str::trim).filter(|t| !t.is_empty()) {
            return tag.to_uppercase();
        }
    }
    b
}

/// QSO database record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QsoDatabaseRecord {
    /// Primary key
    pub id: i64,

    /// QSO unique identifier
    pub qso_id: QsoId,

    /// QSO metadata
    pub metadata: QsoMetadata,

    /// Final QSO state
    pub final_state: QsoState,

    /// QSO progress data (JSON)
    pub progress_data: Option<String>,

    /// ADIF data
    pub adif_data: AdifQso,

    /// Created timestamp
    pub created_at: DateTime<Utc>,

    /// Updated timestamp
    pub updated_at: DateTime<Utc>,

    /// Checksum for integrity verification
    pub checksum: String,
}

/// Database statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseStats {
    /// Total number of QSOs
    pub total_qsos: u64,

    /// Number of confirmed QSOs
    pub confirmed_qsos: u64,

    /// Number of unique callsigns worked
    pub unique_callsigns: u64,

    /// Number of countries worked
    pub countries_worked: u64,

    /// Number of grid squares worked
    pub grid_squares_worked: u64,

    /// QSOs by mode
    pub qsos_by_mode: HashMap<String, u64>,

    /// QSOs by band
    pub qsos_by_band: HashMap<String, u64>,

    /// QSOs by year
    pub qsos_by_year: HashMap<u32, u64>,

    /// First QSO date
    pub first_qso: Option<DateTime<Utc>>,

    /// Last QSO date
    pub last_qso: Option<DateTime<Utc>>,

    /// Database size in bytes
    pub database_size: u64,
}

/// A persisted per-QSO timeline record (Layer 2 timeline persistence).
///
/// Reconstructs "what we sent / what we heard / why we advanced" for a
/// completed or failed QSO, offline, keyed by `qso_id`. See
/// `QsoDatabase::insert_qso_timeline` / `get_qso_timeline`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QsoTimelineRecord {
    /// The QSO's stable identity (`QsoMetadata::qso_id`).
    pub qso_id: QsoId,
    /// The contra-station's callsign, if known at the time of persistence.
    pub callsign: Option<String>,
    /// `"completed"` or `"failed"`.
    pub outcome: String,
    /// The `QsoFailureReason` (as its `Debug` text), present only when
    /// `outcome == "failed"`.
    pub reason: Option<String>,
    /// The full sequence of state transitions this QSO went through.
    pub state_history: Vec<StateTransition>,
    /// Every message sent/received over the life of the QSO.
    pub messages: Vec<QsoMessage>,
    /// When this timeline record was written.
    pub created_at: DateTime<Utc>,
}

/// An ADIF log converted exactly as the index replay converts it.
pub struct AdifSnapshot {
    path: PathBuf,
    records: Vec<QsoMetadata>,
}
impl AdifSnapshot {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, AsyncDatabaseError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| io_error(path, e))?;
        let processor = AdifProcessor::new();
        let file = processor
            .parse_string(&raw)
            .map_err(|e| AsyncDatabaseError::Replay(format!("ADIF parse failed: {e}")))?;
        Ok(Self {
            path: path.to_owned(),
            records: QsoDatabase::convert_adif_records(&processor, &file.records)?,
        })
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousIndex {
    Replace,
    KeepCopy,
}
pub struct Rebuilt {
    pub db: QsoDatabase,
    pub inserted: u64,
    pub skipped: u64,
    pub kept: Option<PathBuf>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexDrift {
    pub missing_from_index: usize,
    pub missing_from_adif: usize,
}
impl IndexDrift {
    pub fn is_current(&self) -> bool {
        self.missing_from_index == 0 && self.missing_from_adif == 0
    }
}
pub struct LegacyMigration {
    pub backup: PathBuf,
    pub backup_existed: bool,
    pub exported: usize,
}
#[derive(Debug)]
pub struct LegacyMigrationError {
    pub db: PathBuf,
    pub adif: PathBuf,
    pub backup: Option<PathBuf>,
    pub source: AsyncDatabaseError,
}
impl std::fmt::Display for LegacyMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "legacy index {} was not migrated to {}: {}; ",
            self.db.display(),
            self.adif.display(),
            self.source
        )?;
        match &self.backup {
            Some(p) => write!(f, "a copy of it is at {}", p.display()),
            None => write!(f, "no copy of it could be made, so it was not opened"),
        }
    }
}
impl std::error::Error for LegacyMigrationError {}
#[derive(Debug, Clone, Copy)]
enum RebuildStep {
    TempReady,
    RowsCommitted,
    TempClosed,
    OldCheckpointed,
    OldKept,
    Swapped,
    Done,
}
fn io_error(path: &Path, source: std::io::Error) -> AsyncDatabaseError {
    AsyncDatabaseError::Io {
        path: path.to_owned(),
        source,
    }
}
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}
fn remove_if_exists(path: &Path) -> Result<(), AsyncDatabaseError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(path, e)),
    }
}
fn sync_file(path: &Path) -> Result<(), AsyncDatabaseError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error(path, e))
}
fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
}
fn checkpoint_allows_swap(busy: i64, log: i64, checkpointed: i64) -> bool {
    busy == 0 && log == checkpointed
}
fn copy_private(from: &Path, to: &Path) -> Result<(), AsyncDatabaseError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(to).map_err(|e| io_error(to, e))?;
    let bytes = std::fs::read(from).map_err(|e| io_error(from, e))?;
    out.write_all(&bytes)
        .and_then(|_| out.sync_all())
        .map_err(|e| io_error(to, e))
}

/// Async database operation errors
#[derive(Debug, Error)]
pub enum AsyncDatabaseError {
    #[error("index {path} is in use by another connection; the rebuild left it in place")]
    IndexBusy { path: PathBuf },
    #[error("{reason}")]
    IndexUnavailable { reason: String },
    #[error("{unreadable} of {total} rows cannot be read; the first is row {id} (qso_id {qso_id}): {reason}")]
    UnreadableRows {
        unreadable: usize,
        total: usize,
        id: i64,
        qso_id: String,
        reason: String,
    },
    #[error("SQLx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("QSO not found: {qso_id}")]
    QsoNotFound { qso_id: QsoId },

    #[error("Duplicate QSO: {qso_id}")]
    DuplicateQso { qso_id: QsoId },

    #[error("Invalid query parameters: {message}")]
    InvalidQuery { message: String },

    #[error("Database migration failed: {version}")]
    MigrationFailed { version: u32 },

    #[error("Schema validation failed: {message}")]
    SchemaValidation { message: String },

    #[error("I/O at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ADIF replay failed: {0}")]
    Replay(String),
}

/// Maximum number of rows any dynamic query is allowed to request.
///
/// `QueryOptions::limit` is a `u32` interpolated directly into a `LIMIT`
/// clause. While that interpolation is injection-safe (it is a number), an
/// unbounded value such as `u32::MAX` would ask SQLite to materialize every
/// row and could exhaust memory. We clamp to a sane ceiling.
const MAX_QUERY_LIMIT: u32 = 10_000;

/// Clamp a caller-supplied row limit to [`MAX_QUERY_LIMIT`] so a hostile or
/// buggy `limit = u32::MAX` cannot force SQLite to return the whole table
/// (potential OOM). Normal small limits pass through unchanged.
fn clamp_query_limit(limit: u32) -> u32 {
    limit.min(MAX_QUERY_LIMIT)
}

/// Escape a string for safe interpolation inside a single-quoted SQLite
/// string literal by doubling embedded single quotes (`'` → `''`).
///
/// This is needed for `VACUUM INTO '<path>'`, which has no bind-parameter
/// form — without escaping, a path containing `'` would terminate the literal
/// early and allow SQL injection via the operator-config backup path.
fn escape_sqlite_string_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Async QSO database using sqlx
#[derive(Clone)]
pub struct QsoDatabase {
    /// Database connection pool
    pool: SqlitePool,

    /// ADIF processor for conversions
    adif_processor: AdifProcessor,

    /// Database schema version
    schema_version: u32,
}

impl QsoDatabase {
    /// Open or create a database at the specified path
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self, AsyncDatabaseError> {
        let database_url = if path.as_ref() == Path::new(":memory:") {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite:{}?mode=rwc", path.as_ref().display())
        };

        // `synchronous` is a per-connection setting, so it goes in the connect
        // options, which every pooled connection runs (PAN-281). `from_str`
        // parses the URL exactly as `connect(&database_url)` did.
        let options = SqliteConnectOptions::from_str(&database_url)?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        let mut db = Self {
            pool,
            adif_processor: AdifProcessor::new(),
            schema_version: 1,
        };

        db.initialize_schema().await?;
        Ok(db)
    }

    /// Create an in-memory database for testing
    pub async fn new_in_memory() -> Result<Self, AsyncDatabaseError> {
        Self::open(":memory:").await
    }

    /// Initialize database schema
    async fn initialize_schema(&mut self) -> Result<(), AsyncDatabaseError> {
        // WAL and `synchronous = FULL` are connect options (see `open`), not
        // PRAGMAs here: `synchronous` is per connection, and a PRAGMA through
        // the pool reaches only one of its connections. WAL lets readers and
        // writers run concurrently; FULL syncs the WAL at every commit, so a
        // completed QSO's row survives a power cut (PAN-281).

        let schema = r#"
            CREATE TABLE IF NOT EXISTS qsos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                qso_id TEXT NOT NULL UNIQUE,
                metadata TEXT NOT NULL,
                final_state TEXT NOT NULL,
                progress_data TEXT NOT NULL,
                adif_data TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                checksum TEXT NOT NULL
            );
            
            CREATE INDEX IF NOT EXISTS idx_qsos_qso_id ON qsos(qso_id);
            CREATE INDEX IF NOT EXISTS idx_qsos_created_at ON qsos(created_at);
            CREATE INDEX IF NOT EXISTS idx_qsos_updated_at ON qsos(updated_at);
            
            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );

            -- Layer 2 timeline persistence (docs/observability-diagnostics-plan.md):
            -- a completed/failed QSO's full state-transition + message history,
            -- gated behind `[database].persist_qso_timeline` (default off) so it
            -- never grows unless the operator opts in. Deliberately a SEPARATE
            -- table from `qsos` (the confirmed-contact log): a Failed QSO is not
            -- a logged contact and must never appear in `qsos` (duplicate
            -- checks / ADIF export / worked-station seeding all read that
            -- table), but its timeline is still valuable for diagnosing why it
            -- failed.
            CREATE TABLE IF NOT EXISTS qso_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                qso_id TEXT NOT NULL,
                callsign TEXT,
                outcome TEXT NOT NULL,
                reason TEXT,
                state_history TEXT NOT NULL,
                messages TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_qso_events_qso_id ON qso_events(qso_id);
            CREATE INDEX IF NOT EXISTS idx_qso_events_created_at ON qso_events(created_at);
        "#;

        sqlx::query(schema).execute(&self.pool).await?;

        // Record schema version
        sqlx::query("INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?, ?)")
            .bind(self.schema_version as i64)
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;

        info!(
            "Database schema initialized (version {})",
            self.schema_version
        );
        Ok(())
    }

    /// Insert a new QSO record
    pub async fn insert_qso(&self, progress: &QsoProgress) -> Result<i64, AsyncDatabaseError> {
        self.insert_qso_with(&self.pool, progress, Utc::now()).await
    }

    /// [`Self::insert_qso`] on any executor, so a batch can share one
    /// transaction. `created_at` is the row's searchable timestamp (the
    /// date-range filters and ordering use it): the moment of logging for a
    /// live QSO, the QSO's own start time for one read from ADIF.
    async fn insert_qso_with<'e, E>(
        &self,
        executor: E,
        progress: &QsoProgress,
        created_at: DateTime<Utc>,
    ) -> Result<i64, AsyncDatabaseError>
    where
        E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
    {
        let qso_id = progress.metadata.qso_id.to_string();
        let metadata_json = serde_json::to_string(&progress.metadata)?;
        let state_json = serde_json::to_string(&progress.state)?;
        let progress_json = serde_json::to_string(progress)?;

        let adif_qso = self
            .adif_processor
            .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
        let adif_json = serde_json::to_string(&adif_qso)?;

        let now = Utc::now().to_rfc3339();
        let created_at = created_at.to_rfc3339();
        let checksum = Self::calculate_checksum(&metadata_json, &state_json, &adif_json);

        let result = sqlx::query(
            "INSERT INTO qsos (qso_id, metadata, final_state, progress_data, adif_data, 
                              created_at, updated_at, checksum) 
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&qso_id)
        .bind(&metadata_json)
        .bind(&state_json)
        .bind(&progress_json)
        .bind(&adif_json)
        .bind(&created_at)
        .bind(&now)
        .bind(&checksum)
        .execute(executor)
        .await?;

        let id = result.last_insert_rowid();
        debug!("Inserted QSO {} with database ID {}", qso_id, id);
        Ok(id)
    }

    /// Update an existing QSO record
    pub async fn update_qso(&self, progress: &QsoProgress) -> Result<(), AsyncDatabaseError> {
        let qso_id = progress.metadata.qso_id.to_string();
        let metadata_json = serde_json::to_string(&progress.metadata)?;
        let state_json = serde_json::to_string(&progress.state)?;
        let progress_json = serde_json::to_string(progress)?;

        let adif_qso = self
            .adif_processor
            .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
        let adif_json = serde_json::to_string(&adif_qso)?;

        let now = Utc::now().to_rfc3339();
        let checksum = Self::calculate_checksum(&metadata_json, &state_json, &adif_json);

        let rows_affected = sqlx::query(
            "UPDATE qsos SET metadata = ?, final_state = ?, progress_data = ?, 
                           adif_data = ?, updated_at = ?, checksum = ? 
             WHERE qso_id = ?",
        )
        .bind(&metadata_json)
        .bind(&state_json)
        .bind(&progress_json)
        .bind(&adif_json)
        .bind(&now)
        .bind(&checksum)
        .bind(&qso_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows_affected == 0 {
            return Err(AsyncDatabaseError::QsoNotFound {
                qso_id: progress.metadata.qso_id,
            });
        }

        debug!("Updated QSO {}", qso_id);
        Ok(())
    }

    /// Get a QSO by ID
    pub async fn get_qso(&self, qso_id: QsoId) -> Result<QsoProgress, AsyncDatabaseError> {
        let qso_id_str = qso_id.to_string();

        let row = sqlx::query_as::<_, (String,)>("SELECT progress_data FROM qsos WHERE qso_id = ?")
            .bind(&qso_id_str)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some((progress_json,)) => {
                let progress: QsoProgress = serde_json::from_str(&progress_json)?;
                Ok(progress)
            }
            None => Err(AsyncDatabaseError::QsoNotFound { qso_id }),
        }
    }

    /// Delete a QSO by ID
    pub async fn delete_qso(&self, qso_id: QsoId) -> Result<(), AsyncDatabaseError> {
        let qso_id_str = qso_id.to_string();

        let rows_affected = sqlx::query("DELETE FROM qsos WHERE qso_id = ?")
            .bind(&qso_id_str)
            .execute(&self.pool)
            .await?
            .rows_affected();

        if rows_affected == 0 {
            return Err(AsyncDatabaseError::QsoNotFound { qso_id });
        }

        debug!("Deleted QSO {}", qso_id);
        Ok(())
    }

    /// Search QSOs with filters
    pub async fn search_qsos(
        &self,
        filter: &QsoFilter,
        options: &QueryOptions,
    ) -> Result<Vec<QsoProgress>, AsyncDatabaseError> {
        // Build dynamic query based on filters
        let mut query = String::from("SELECT progress_data FROM qsos WHERE 1=1");
        let mut bindings = vec![];

        // Add filter conditions
        if let Some(pattern) = &filter.callsign_pattern {
            query.push_str(" AND metadata LIKE ?");
            bindings.push(format!("%{}%", pattern));
        }

        if let Some(date_range) = &filter.date_range {
            query.push_str(" AND created_at >= ?");
            bindings.push(date_range.start.to_rfc3339());
            query.push_str(" AND created_at <= ?");
            bindings.push(date_range.end.to_rfc3339());
        }

        // Add ordering
        query.push_str(" ORDER BY created_at DESC");

        // Add limit (clamped so a hostile u32::MAX can't force a full-table OOM)
        if let Some(limit) = options.limit {
            query.push_str(&format!(" LIMIT {}", clamp_query_limit(limit)));
        }

        // Execute query
        let mut result = sqlx::query(&query);
        for binding in bindings {
            result = result.bind(binding);
        }

        let rows = result
            .map(|row: SqliteRow| row.get::<String, _>(0))
            .fetch_all(&self.pool)
            .await?;

        // Parse results
        let mut qsos = Vec::new();
        for progress_json in rows {
            if let Ok(progress) = serde_json::from_str::<QsoProgress>(&progress_json) {
                qsos.push(progress);
            }
        }

        Ok(qsos)
    }

    /// Get total QSO count
    pub async fn get_qso_count(&self) -> Result<i64, AsyncDatabaseError> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM qsos")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Calculate checksum for data integrity
    fn calculate_checksum(metadata: &str, state: &str, adif: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        metadata.hash(&mut hasher);
        state.hash(&mut hasher);
        adif.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Create a backup of the database using VACUUM INTO for atomic backup.
    ///
    /// This replaces the old export-reimport approach which was non-atomic and
    /// could corrupt the backup on crash.
    pub async fn backup<P: AsRef<Path>>(&self, backup_path: P) -> Result<(), AsyncDatabaseError> {
        let backup_path_str = backup_path.as_ref().to_string_lossy().to_string();

        // Defensive: a NUL byte can't appear in a valid filesystem path and a
        // newline has no legitimate place in a backup target — refuse rather
        // than risk a malformed statement.
        if backup_path_str.contains('\0') || backup_path_str.contains('\n') {
            return Err(AsyncDatabaseError::InvalidQuery {
                message: "backup path contains a NUL byte or newline".to_string(),
            });
        }

        // VACUUM INTO writes a uniquely named sibling first; the backup only
        // appears under its real name via a no-clobber hard link once it is
        // complete. A failure (or a same-named backup made concurrently by
        // another logger/process) therefore never leaves a partial file
        // under the real name and never deletes anyone else's backup.
        let target = backup_path.as_ref();
        static PARTIAL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let file_name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "backup".to_string());
        let partial = target.with_file_name(format!(
            ".{file_name}.partial-{}-{}",
            std::process::id(),
            PARTIAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let remove_partial = || async {
            match tokio::fs::remove_file(&partial).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(
                    "Could not remove partial backup {}: {}",
                    partial.display(),
                    e
                ),
            }
        };

        // VACUUM INTO has no bind-parameter form for its target, so the path is
        // interpolated into a single-quoted SQL string literal. Escape embedded
        // single quotes (`'` → `''`) so a path containing `'` cannot break out
        // of the literal and inject SQL.
        let escaped_path = escape_sqlite_string_literal(&partial.to_string_lossy());

        // Use VACUUM INTO which atomically creates a complete copy of the database.
        if let Err(e) = sqlx::query(&format!("VACUUM INTO '{}'", escaped_path))
            .execute(&self.pool)
            .await
        {
            remove_partial().await;
            return Err(AsyncDatabaseError::Sqlx(e));
        }

        // Fails with AlreadyExists rather than replacing an existing file.
        let linked = tokio::fs::hard_link(&partial, target).await;
        remove_partial().await;
        linked.map_err(|source| AsyncDatabaseError::Io {
            path: target.to_path_buf(),
            source,
        })?;

        info!("Database backup completed (VACUUM INTO)");
        Ok(())
    }

    /// Get database statistics
    pub async fn get_statistics(&self) -> Result<DatabaseStats, AsyncDatabaseError> {
        let total_qsos: u64 = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM qsos")
            .fetch_one(&self.pool)
            .await? as u64;

        // PAN-347: count real confirmations (QSL_RCVD / LOTW_QSL_RCVD of Y or V).
        let confirmed_qsos: u64 = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM qsos WHERE {CONFIRMED_QSO_SQL}"
        ))
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0) as u64;

        let unique_callsigns: u64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(DISTINCT json_extract(metadata, '$.their_callsign')) FROM qsos",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(0) as u64;

        // For now, simplified stats - can be enhanced later
        let countries_worked = 0;
        let grid_squares_worked = 0;
        let qsos_by_mode = HashMap::new();
        let qsos_by_band = HashMap::new();
        let qsos_by_year = HashMap::new();

        let first_qso: Option<DateTime<Utc>> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT created_at FROM qsos ORDER BY created_at ASC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

        let last_qso: Option<DateTime<Utc>> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT created_at FROM qsos ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc));

        Ok(DatabaseStats {
            total_qsos,
            confirmed_qsos,
            unique_callsigns,
            countries_worked,
            grid_squares_worked,
            qsos_by_mode,
            qsos_by_band,
            qsos_by_year,
            first_qso,
            last_qso,
            database_size: 0, // Would need file system access to calculate
        })
    }

    /// Check a rolling window by callsign and optional band, independent of audio offset.
    /// Metadata is serialized from DateTime<Utc> as UTC RFC3339. Pad its optional
    /// fractional seconds to nine digits for exact comparisons without SQLite's
    /// millisecond rounding. The startup history read validates stored dates.
    pub async fn check_duplicate(
        &self,
        callsign: &str,
        band: Option<&str>,
        now: DateTime<Utc>,
        time_window_hours: u32,
    ) -> Result<Option<QsoId>, AsyncDatabaseError> {
        let cutoff = now - chrono::Duration::hours(time_window_hours as i64);
        let id: Option<String> = sqlx::query_scalar(
            "WITH contacts AS (
                 SELECT qso_id, json_extract(metadata, '$.start_time') AS started FROM qsos
                 WHERE UPPER(TRIM(json_extract(metadata, '$.their_callsign'))) = ?
                 AND (? IS NULL OR UPPER(TRIM(json_extract(adif_data, '$.band'))) = ?)
             ), dated AS (
                 SELECT qso_id, substr(started, 1, 19) || '.' ||
                     substr((CASE WHEN substr(started, 20, 1) = '.'
                         THEN rtrim(substr(started, 21), 'Z') ELSE '' END)
                         || '000000000', 1, 9) || 'Z' AS stamp FROM contacts
             ) SELECT qso_id FROM dated WHERE stamp > ? AND stamp <= ? LIMIT 1",
        )
        .bind(callsign.trim().to_uppercase())
        .bind(band.map(|b| b.trim().to_uppercase()))
        .bind(band.map(|b| b.trim().to_uppercase()))
        .bind(cutoff.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .bind(now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .fetch_optional(&self.pool)
        .await?;
        id.map(|id| {
            Uuid::parse_str(&id).map_err(|e| AsyncDatabaseError::InvalidQuery {
                message: e.to_string(),
            })
        })
        .transpose()
    }

    /// PAN-347: every indexed contact with its band, callsign, mode and
    /// confirmation, for the local needs lookup. Not reduced per band or
    /// callsign; the cache keeps the latest date. Validate every date before
    /// returning anything, so a failed refresh cannot erase history.
    pub async fn worked_history(&self) -> Result<Vec<WorkedContact>, AsyncDatabaseError> {
        type Row = (
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT json_extract(adif_data, '$.band'), json_extract(metadata, '$.tags.BAND'),
                    json_extract(metadata, '$.their_callsign'),
                    json_extract(metadata, '$.start_time'), json_extract(metadata, '$.mode'),
                    json_extract(metadata, '$.tags.QSL_RCVD'),
                    json_extract(metadata, '$.tags.LOTW_QSL_RCVD') FROM qsos",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (band, band_tag, call, date, mode, qsl, lotw) in rows {
            let start_time = DateTime::parse_from_rfc3339(&date)
                .map_err(|e| AsyncDatabaseError::InvalidQuery {
                    message: format!("Invalid QSO start date {date}: {e}"),
                })?
                .with_timezone(&Utc);
            let confirmed = qsl.as_deref().is_some_and(is_confirmed_qsl_value)
                || lotw.as_deref().is_some_and(is_confirmed_qsl_value);
            out.push(WorkedContact {
                band: logged_band(&band, band_tag.as_deref()),
                callsign: call.trim().to_uppercase(),
                mode: mode.unwrap_or_default().trim().to_uppercase(),
                confirmed,
                start_time,
            });
        }
        Ok(out)
    }

    /// Complete, unpaginated contest ledger. Malformed rows fail closed.
    /// Identity is checked before time filtering so reusing an instance id
    /// with different rules or periods cannot silently hide older evidence.
    pub async fn contest_ledger(
        &self,
        context: &crate::contest::scoring::ContestScoringContext,
        operator_callsign: &str,
    ) -> Result<Vec<crate::contest::scoring::ContestContactFact>, AsyncDatabaseError> {
        use crate::contest::ledger;
        let rows = sqlx::query("SELECT metadata, final_state, adif_data FROM qsos")
            .fetch_all(&self.pool)
            .await?;
        let error = |message: String| AsyncDatabaseError::InvalidQuery { message };
        let mut facts = Vec::new();
        for row in rows {
            // Do not use json_extract to select rows: malformed JSON must
            // not disappear from a supposedly complete history.
            let metadata: QsoMetadata = serde_json::from_str(row.get("metadata"))?;
            if metadata.tags.get(ledger::INSTANCE).map(String::as_str)
                != Some(context.instance_id.as_str())
            {
                continue;
            }
            // Unreadable `adif_data` must fail the supposedly complete
            // history closed, same as unreadable metadata.
            serde_json::from_str::<AdifQso>(row.get("adif_data"))?;
            let fact = ledger::fact(&metadata, context, operator_callsign).map_err(error)?;
            let state: QsoState = serde_json::from_str(row.get("final_state"))?;
            if matches!(state, QsoState::Completed { .. }) && context.in_period(fact.start) {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    /// Search QSOs returning QsoDatabaseRecord format for compatibility
    pub async fn search_qsos_records(
        &self,
        filter: &QsoFilter,
        options: &QueryOptions,
    ) -> Result<Vec<QsoDatabaseRecord>, AsyncDatabaseError> {
        // Build dynamic query based on filters
        let mut query = String::from(
            "SELECT id, qso_id, metadata, final_state, progress_data, adif_data, 
                    created_at, updated_at, checksum 
             FROM qsos WHERE 1=1",
        );
        let mut bindings = vec![];

        // Add filter conditions
        if let Some(pattern) = &filter.callsign_pattern {
            query.push_str(" AND metadata LIKE ?");
            bindings.push(format!("%{}%", pattern));
        }

        if let Some(date_range) = &filter.date_range {
            query.push_str(" AND created_at >= ?");
            bindings.push(date_range.start.to_rfc3339());
            query.push_str(" AND created_at <= ?");
            bindings.push(date_range.end.to_rfc3339());
        }

        // Add ordering
        query.push_str(" ORDER BY created_at DESC");

        // Add limit (clamped so a hostile u32::MAX can't force a full-table OOM)
        if let Some(limit) = options.limit {
            query.push_str(&format!(" LIMIT {}", clamp_query_limit(limit)));
        }

        // Execute query
        let mut result = sqlx::query(&query);
        for binding in bindings {
            result = result.bind(binding);
        }

        let rows = result.fetch_all(&self.pool).await?;

        // Parse results
        let mut records = Vec::new();
        for row in rows {
            let id: i64 = row.get("id");
            let qso_id_str: String = row.get("qso_id");
            let metadata_json: String = row.get("metadata");
            let state_json: String = row.get("final_state");
            let progress_data: Option<String> = row.get("progress_data");
            let adif_json: String = row.get("adif_data");
            let created_at_str: String = row.get("created_at");
            let updated_at_str: String = row.get("updated_at");
            let checksum: String = row.get("checksum");

            // Parse fields
            if let (
                Ok(qso_id),
                Ok(metadata),
                Ok(final_state),
                Ok(adif_data),
                Ok(created_at),
                Ok(updated_at),
            ) = (
                Uuid::parse_str(&qso_id_str),
                serde_json::from_str::<QsoMetadata>(&metadata_json),
                serde_json::from_str::<QsoState>(&state_json),
                serde_json::from_str::<crate::adif::AdifQso>(&adif_json),
                DateTime::parse_from_rfc3339(&created_at_str),
                DateTime::parse_from_rfc3339(&updated_at_str),
            ) {
                records.push(QsoDatabaseRecord {
                    id,
                    qso_id,
                    metadata,
                    final_state,
                    progress_data,
                    adif_data,
                    created_at: created_at.with_timezone(&Utc),
                    updated_at: updated_at.with_timezone(&Utc),
                    checksum,
                });
            }
        }

        Ok(records)
    }

    /// Get distinct callsigns worked on a specific band.
    ///
    /// This is the async equivalent of `QsoDatabase::get_worked_callsigns`,
    /// used at startup to seed the worked-station duplicate filter.
    pub async fn get_worked_callsigns(&self, band: &str) -> Vec<String> {
        let result: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
            "SELECT DISTINCT json_extract(metadata, '$.their_callsign') \
             FROM qsos \
             WHERE json_extract(adif_data, '$.band') = ? \
               AND json_extract(metadata, '$.their_callsign') IS NOT NULL",
        )
        .bind(band)
        .fetch_all(&self.pool)
        .await;

        match result {
            Ok(callsigns) => callsigns,
            Err(e) => {
                tracing::warn!(
                    "get_worked_callsigns: query failed (band={}): {} — treating as empty",
                    band,
                    e
                );
                Vec::new()
            }
        }
    }

    /// Get every distinct (band, callsign) pair ever worked, across ALL
    /// bands in one query.
    ///
    /// Unlike [`Self::get_worked_callsigns`] (one band at a time, used to
    /// seed the duplicate filter for whichever band the rig happens to be
    /// tuned to at startup), the DX Hunter needs to evaluate rows on bands
    /// OTHER than the current dial — so this pulls the whole log at once to
    /// seed a per-band-DXCC-entity worked-set (2026-07-18, DX Hunter
    /// per-band-needed gap).
    pub async fn get_worked_bands_and_callsigns(&self) -> Vec<(String, String)> {
        let result: Result<Vec<(String, String)>, sqlx::Error> = sqlx::query_as(
            "SELECT DISTINCT json_extract(adif_data, '$.band'), \
                             json_extract(metadata, '$.their_callsign') \
             FROM qsos \
             WHERE json_extract(adif_data, '$.band') IS NOT NULL \
               AND json_extract(metadata, '$.their_callsign') IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await;

        match result {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(
                    "get_worked_bands_and_callsigns: query failed: {} — treating as empty",
                    e
                );
                Vec::new()
            }
        }
    }

    /// Mirrors `get_worked_bands_and_callsigns` but for the DX's grid square
    /// instead of callsign — feeds `CachedStationLookup::seed_worked_grids_from_list`
    /// for #164's per-band-grid-new tier. Rows with no grid on the QSO are
    /// simply absent (`json_extract` on a missing/null field filters them via
    /// the `IS NOT NULL` clause), not an error.
    /// Fallible history read: a successful empty log differs from unavailable history.
    pub async fn try_get_worked_bands_and_grids(
        &self,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT DISTINCT json_extract(adif_data, '$.band'), \
                             json_extract(metadata, '$.grids.theirs') \
             FROM qsos \
             WHERE json_extract(adif_data, '$.band') IS NOT NULL \
               AND json_extract(metadata, '$.grids.theirs') IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_worked_bands_and_grids(&self) -> Vec<(String, String)> {
        let result = self.try_get_worked_bands_and_grids().await;

        match result {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(
                    "get_worked_bands_and_grids: query failed: {} — treating as empty",
                    e
                );
                Vec::new()
            }
        }
    }

    /// Every (callsign, state) pair in the log that carries an ADIF `STATE`,
    /// oldest first so the caller's last-write-wins seed ends on the most
    /// recent value. Mirrors `get_worked_bands_and_grids` one tier over
    /// (PAN-85). Rows with no state are filtered by the `IS NOT NULL` clause.
    ///
    /// Deliberately NOT `SELECT DISTINCT`, unlike its two siblings: the
    /// last-write-wins seed depends on the `ORDER BY start_time` this query
    /// carries, and de-duplicating (callsign, state) pairs would keep the
    /// stale rows of a station that later moved. The cost is one redundant
    /// `record_state` per repeat contact at startup — a map insert, paid once.
    /// Fallible history read: a successful empty log differs from unavailable history.
    pub async fn try_get_worked_callsigns_and_states(
        &self,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT json_extract(metadata, '$.their_callsign'), \
                    json_extract(metadata, '$.their_state') \
             FROM qsos \
             WHERE json_extract(metadata, '$.their_callsign') IS NOT NULL \
               AND json_extract(metadata, '$.their_state') IS NOT NULL \
             ORDER BY json_extract(metadata, '$.start_time') ASC",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_worked_callsigns_and_states(&self) -> Vec<(String, String)> {
        let result = self.try_get_worked_callsigns_and_states().await;

        match result {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::warn!(
                    "get_worked_callsigns_and_states: query failed: {} — treating as empty",
                    e
                );
                Vec::new()
            }
        }
    }

    /// Build a fresh index at `db_path` by replaying every record in `adif_path`.
    ///
    /// If `db_path` exists, it is replaced atomically once the ADIF has been read and
    /// converted (an unreadable/malformed ADIF leaves it untouched) — caller
    /// should only invoke this when the DB is known to be stale or missing. `db_path` may also be the
    /// literal `":memory:"` sentinel (see [`Self::open`]) for a throwaway,
    /// on-disk-file-free rebuild; that case is never `try_exists`/`remove_file`d.
    /// Returns the new database handle, ready for queries.
    pub async fn replay_from_adif(
        db_path: impl AsRef<std::path::Path>,
        adif_path: impl AsRef<std::path::Path>,
    ) -> Result<Self, AsyncDatabaseError> {
        let snapshot = AdifSnapshot::load(adif_path).await?;
        Ok(
            Self::rebuild_from(db_path, snapshot, PreviousIndex::Replace)
                .await?
                .db,
        )
    }

    /// Probe startup history without losing an unreadable index's WAL during
    /// SQLite's failed open. Busy/locked failures never restore over live data.
    pub async fn open_for_reconciliation(
        path: impl AsRef<Path>,
    ) -> Result<Self, AsyncDatabaseError> {
        let path = path.as_ref();
        let wal = suffixed(path, "-wal");
        let original = match std::fs::read(&wal) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io_error(&wal, e)),
        };
        let probe = async {
            let mut conn = sqlx::SqliteConnection::connect_with(
                &SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(false),
            )
            .await?;
            let result = sqlx::query("SELECT name FROM sqlite_schema LIMIT 1")
                .fetch_optional(&mut conn)
                .await;
            conn.close().await?;
            result.map(|_| ())
        }
        .await;
        if let Err(error) = probe {
            let code = error
                .as_database_error()
                .and_then(|d| d.code())
                .and_then(|c| c.parse::<i32>().ok())
                .map(|c| c & 255);
            if matches!(code, Some(11 | 26)) && !original.is_empty() {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&wal)
                    .map_err(|e| io_error(&wal, e))?;
                file.write_all(&original)
                    .and_then(|_| file.sync_all())
                    .map_err(|e| io_error(&wal, e))?;
            }
            return Err(error.into());
        }
        Self::open(path).await
    }

    pub fn rebuild_temp_path(db_path: &Path) -> PathBuf {
        suffixed(db_path, ".new")
    }
    pub async fn rebuild_from(
        db_path: impl AsRef<Path>,
        snapshot: AdifSnapshot,
        previous: PreviousIndex,
    ) -> Result<Rebuilt, AsyncDatabaseError> {
        let path = db_path.as_ref();
        let result = Self::rebuild_with(path, snapshot, previous, &mut |_| {}).await;
        if result.is_err() && path != Path::new(":memory:") {
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let _ = remove_if_exists(&suffixed(&Self::rebuild_temp_path(path), suffix));
            }
        }
        result
    }

    // Before Swapped the old index remains at its original name. After Swapped
    // the new index is complete, checkpointed and synced. Never unlink the target.
    async fn rebuild_with(
        db_path: &Path,
        snapshot: AdifSnapshot,
        previous: PreviousIndex,
        at: &mut (dyn FnMut(RebuildStep) + Send),
    ) -> Result<Rebuilt, AsyncDatabaseError> {
        debug!(
            records = snapshot.len(),
            empty = snapshot.is_empty(),
            "Preparing index rebuild"
        );
        if db_path == Path::new(":memory:") {
            let db = Self::open(db_path).await?;
            let (inserted, skipped) = db.insert_converted(snapshot.records).await?;
            info!(
                "Replayed {} records from {} into {} ({} skipped)",
                inserted,
                snapshot.path.display(),
                db_path.display(),
                skipped
            );
            return Ok(Rebuilt {
                db,
                inserted,
                skipped,
                kept: None,
            });
        }
        let temp = Self::rebuild_temp_path(db_path);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            remove_if_exists(&suffixed(&temp, suffix))?;
        }
        at(RebuildStep::TempReady);
        let db = Self::open(&temp).await?;
        let result = db.insert_converted(snapshot.records).await;
        at(RebuildStep::RowsCommitted);
        db.close().await;
        let (inserted, skipped) = result?;
        if std::fs::metadata(suffixed(&temp, "-wal")).is_ok_and(|m| m.len() > 0) {
            return Err(AsyncDatabaseError::Replay(
                "temporary index WAL was not checkpointed".into(),
            ));
        }
        sync_file(&temp)?;
        for suffix in ["-wal", "-shm"] {
            remove_if_exists(&suffixed(&temp, suffix))?;
        }
        at(RebuildStep::TempClosed);
        let mut kept = None;
        if db_path.try_exists().map_err(|e| io_error(db_path, e))? {
            // SQLite can discard an invalid WAL even when opening fails. Retain
            // its original bytes before probing so an unreadable copy is complete.
            let old_wal_path = suffixed(db_path, "-wal");
            let old_wal = match std::fs::read(&old_wal_path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(io_error(&old_wal_path, e)),
            };
            let checkpoint = async {
                let mut conn = sqlx::SqliteConnection::connect_with(
                    &SqliteConnectOptions::new()
                        .filename(db_path)
                        .create_if_missing(false),
                )
                .await?;
                let result =
                    sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
                        .fetch_one(&mut conn)
                        .await;
                conn.close().await?;
                result
            }
            .await;
            let unreadable = match checkpoint {
                Ok((busy, log, done)) if checkpoint_allows_swap(busy, log, done) => false,
                Ok(_) => {
                    return Err(AsyncDatabaseError::IndexBusy {
                        path: db_path.to_owned(),
                    })
                }
                Err(e) => {
                    let code = e
                        .as_database_error()
                        .and_then(|d| d.code())
                        .and_then(|c| c.parse::<i32>().ok())
                        .map(|c| c & 255);
                    match code {
                        Some(11 | 26) => true,
                        Some(5 | 6) => {
                            return Err(AsyncDatabaseError::IndexBusy {
                                path: db_path.to_owned(),
                            })
                        }
                        _ => return Err(e.into()),
                    }
                }
            };
            at(RebuildStep::OldCheckpointed);
            if unreadable || previous == PreviousIndex::KeepCopy {
                for n in 0..=999 {
                    let candidate = suffixed(
                        db_path,
                        &if n == 0 {
                            ".pre-rebuild".into()
                        } else {
                            format!(".pre-rebuild.{n}")
                        },
                    );
                    match std::fs::hard_link(db_path, &candidate) {
                        Ok(()) => {
                            kept = Some(candidate);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => return Err(io_error(&candidate, e)),
                    }
                }
                let Some(copy) = &kept else {
                    return Err(AsyncDatabaseError::Replay(
                        "all pre-rebuild copy names are taken".into(),
                    ));
                };
                if unreadable && !old_wal.is_empty() {
                    use std::io::Write;
                    let wal_copy = suffixed(copy, "-wal");
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&wal_copy)
                        .map_err(|e| io_error(&wal_copy, e))?;
                    file.write_all(&old_wal)
                        .and_then(|_| file.sync_all())
                        .map_err(|e| io_error(&wal_copy, e))?;
                    remove_if_exists(&old_wal_path)?;
                }
            }
            at(RebuildStep::OldKept);
        }
        for attempt in 0..=10 {
            match std::fs::rename(&temp, db_path) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 10 => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(e) => return Err(io_error(db_path, e)),
            }
        }
        at(RebuildStep::Swapped);
        for suffix in ["-wal", "-shm"] {
            remove_if_exists(&suffixed(db_path, suffix))?;
        }
        sync_parent(db_path);
        let db = Self::open(db_path).await?;
        info!(
            "Replayed {} records from {} into {} ({} skipped)",
            inserted,
            snapshot.path.display(),
            db_path.display(),
            skipped
        );
        at(RebuildStep::Done);
        Ok(Rebuilt {
            db,
            inserted,
            skipped,
            kept,
        })
    }

    pub async fn drift_from(
        &self,
        snapshot: &AdifSnapshot,
    ) -> Result<IndexDrift, AsyncDatabaseError> {
        #[derive(Deserialize)]
        struct ContactFields {
            their_callsign: Option<String>,
            start_time: DateTime<Utc>,
        }
        fn key(call: Option<&str>, time: DateTime<Utc>) -> Option<(String, i64)> {
            call.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|c| (c.to_uppercase(), time.timestamp()))
        }
        let mut counts = HashMap::<_, i64>::new();
        for record in &snapshot.records {
            if let Some(k) = key(record.their_callsign.as_deref(), record.start_time) {
                *counts.entry(k).or_default() += 1;
            }
        }
        let rows: Vec<(String,)> = sqlx::query_as("SELECT metadata FROM qsos")
            .fetch_all(&self.pool)
            .await?;
        let mut invalid = 0;
        for (raw,) in rows {
            match serde_json::from_str::<ContactFields>(&raw) {
                Ok(row) => {
                    if let Some(k) = key(row.their_callsign.as_deref(), row.start_time) {
                        *counts.entry(k).or_default() -= 1;
                    }
                }
                Err(_) => invalid += 1,
            }
        }
        Ok(IndexDrift {
            missing_from_index: counts
                .values()
                .filter(|n| **n > 0)
                .map(|n| *n as usize)
                .sum(),
            missing_from_adif: invalid
                + counts
                    .values()
                    .filter(|n| **n < 0)
                    .map(|n| (-*n) as usize)
                    .sum::<usize>(),
        })
    }

    pub async fn migrate_legacy_to_adif(
        db_path: impl AsRef<Path>,
        adif_path: impl AsRef<Path>,
    ) -> Result<(Self, LegacyMigration), LegacyMigrationError> {
        let db_path = db_path.as_ref();
        let adif_path = adif_path.as_ref();
        let backup = suffixed(db_path, ".pre-migration");
        let mut copied = false;
        let result = async {
            let backup_existed = backup.try_exists().map_err(|e| io_error(&backup, e))?;
            if !backup_existed {
                let partial = suffixed(&backup, ".partial");
                let partial_wal = suffixed(&backup, ".partial-wal");
                for p in [&partial, &partial_wal, &suffixed(&backup, "-wal")] {
                    remove_if_exists(p)?;
                }
                let wal = suffixed(db_path, "-wal");
                if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
                    copy_private(&wal, &partial_wal)?;
                    std::fs::rename(&partial_wal, suffixed(&backup, "-wal"))
                        .map_err(|e| io_error(&backup, e))?;
                }
                copy_private(db_path, &partial)?;
                std::fs::hard_link(&partial, &backup).map_err(|e| io_error(&backup, e))?;
                remove_if_exists(&partial)?;
                sync_parent(&backup);
            }
            copied = true;
            let db = Self::open(db_path).await?;
            match db.export_to_adif(adif_path).await {
                Ok(exported) => Ok((
                    db,
                    LegacyMigration {
                        backup: backup.clone(),
                        backup_existed,
                        exported,
                    },
                )),
                Err(e) => {
                    db.close().await;
                    Err(e)
                }
            }
        }
        .await;
        result.map_err(|source| LegacyMigrationError {
            db: db_path.to_owned(),
            adif: adif_path.to_owned(),
            backup: copied.then_some(backup),
            source,
        })
    }

    /// Index `records` exactly as [`Self::replay_from_adif`] does: the same
    /// record→row conversion, the same −15 dB report default, and the same
    /// skip (with a `warn!`) for a record with no CALL. Returns (inserted,
    /// skipped for having no CALL). Fails, inserting nothing, if
    /// `record_to_qso` rejects any record or any insert fails: the rows
    /// commit in one transaction.
    pub async fn insert_adif_records(
        &self,
        processor: &AdifProcessor,
        records: &[AdifRecord],
    ) -> Result<(u64, u64), AsyncDatabaseError> {
        let converted = Self::convert_adif_records(processor, records)?;
        self.insert_converted(converted).await
    }

    /// `record_to_qso` + `adif_to_qso` for every record, failing on the first
    /// record that cannot be converted.
    fn convert_adif_records(
        processor: &AdifProcessor,
        records: &[AdifRecord],
    ) -> Result<Vec<QsoMetadata>, AsyncDatabaseError> {
        records
            .iter()
            .map(|adif_record| {
                processor
                    .record_to_qso(adif_record)
                    .map(|adif_qso| processor.adif_to_qso(&adif_qso))
                    .map_err(|e| AsyncDatabaseError::Replay(format!("record→AdifQso failed: {e}")))
            })
            .collect()
    }

    /// Insert converted records as completed QSOs, all in one transaction.
    /// Records with no CALL are skipped. Returns (inserted, skipped).
    async fn insert_converted(
        &self,
        converted: Vec<QsoMetadata>,
    ) -> Result<(u64, u64), AsyncDatabaseError> {
        let mut inserted: u64 = 0;
        let mut skipped: u64 = 0;
        // One transaction: an import stopped part-way (error, kill, power
        // loss) leaves none of its rows, never a partial index that looks
        // current (PAN-127).
        let mut tx = self.pool.begin().await?;
        for metadata in converted {
            // ADIF records with no <CALL:N> field are semantically broken — skip
            // them rather than inserting a record with no callsign.
            let their_callsign = match metadata.their_callsign.clone() {
                Some(c) => c,
                None => {
                    warn!(
                        qso_id = %metadata.qso_id,
                        "Skipping ADIF record with no CALL field"
                    );
                    skipped += 1;
                    continue;
                }
            };

            let completed_at = metadata.end_time.unwrap_or(metadata.start_time);
            let duration_seconds = metadata
                .end_time
                .map(|end| {
                    end.signed_duration_since(metadata.start_time)
                        .num_seconds()
                        .max(0) as u32
                })
                .unwrap_or(0);

            // Signal reports default to -15 dB (middling FT8) when the source
            // ADIF did not carry an RST field.
            let their_report: SignalReport = metadata.reports.received.unwrap_or(-15);
            let our_report: SignalReport = metadata.reports.sent.unwrap_or(-15);

            let progress = QsoProgress {
                state: QsoState::Completed {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency: metadata.frequency,
                    grid_square: metadata.grids.theirs.clone(),
                    completed_at,
                    duration_seconds,
                },
                state_history: vec![],
                messages: vec![],
                metadata,
            };

            // A historical record lands in its own date range, not the day
            // it was imported or replayed (PR #451 review).
            let created_at = progress.metadata.start_time;
            self.insert_qso_with(&mut *tx, &progress, created_at)
                .await?;
            inserted += 1;
        }
        tx.commit().await?;
        Ok((inserted, skipped))
    }

    /// The file beside the index at `db_path` that marks it as missing rows
    /// the ADIF log holds. `pancetta import` creates it before writing and
    /// removes it once the new rows are indexed; startup rebuilds an index it
    /// finds marked. The new rows commit together, but an import stopped
    /// between appending to the ADIF log and committing them (Ctrl-C, power
    /// loss) may leave the index incomplete. The marker also catches changed
    /// fields outside the contact key used by startup comparison (PAN-127).
    pub fn rebuild_marker(db_path: &Path) -> std::path::PathBuf {
        let mut name = db_path.as_os_str().to_owned();
        name.push(".rebuild");
        name.into()
    }

    /// Checkpoint the WAL into the database file, then close the pool, so a
    /// short-lived process (e.g. `pancetta import`) leaves a quiescent
    /// `qso.db` when this returns: every committed frame is in the main file,
    /// and no pooled connection still owes it a write (PAN-127). The pool's
    /// own close is not enough on its own — a connection can outlive the
    /// `Pool::close` future's completion, and its later checkpoint would
    /// change `qso.db`'s bytes and mtime after the caller had already read
    /// them.
    pub async fn close(&self) {
        // TRUNCATE writes every committed frame into the main database file
        // and resets the WAL to zero bytes, so whatever the pooled
        // connections do afterwards cannot touch the file again.
        if let Ok(row) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&self.pool)
            .await
        {
            // (busy, log_frames, checkpointed_frames); busy means a reader
            // blocked the truncation — the frames that did get checkpointed
            // are still durable in the main file.
            let (busy, log, checkpointed): (i64, i64, i64) = (row.get(0), row.get(1), row.get(2));
            if busy != 0 {
                warn!("WAL checkpoint on close was busy: {checkpointed}/{log} frames checkpointed");
            }
        }
        self.pool.close().await
    }

    /// Export all QSOs in the index to an ADIF file at `path`.
    ///
    /// Iterates every row in `qsos`, converts via `qso_to_adif`, and writes a
    /// complete ADIF file. Intended for the DB→ADIF migration path at startup.
    pub async fn export_to_adif(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<usize, AsyncDatabaseError> {
        use crate::adif::{AdifFile, AdifHeader};

        let path = path.as_ref();

        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT id, qso_id, progress_data FROM qsos ORDER BY created_at ASC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let processor = AdifProcessor::new();
        let mut records = Vec::with_capacity(rows.len());
        let mut failures = Vec::new();
        for (id, qso_id, raw) in &rows {
            match serde_json::from_str::<QsoProgress>(raw) {
                Ok(progress) => {
                    let qso = processor
                        .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
                    records.push(processor.qso_to_record(&qso));
                }
                Err(e) => failures.push((*id, qso_id.clone(), e.to_string())),
            }
        }
        if let Some((id, qso_id, reason)) = failures.first() {
            return Err(AsyncDatabaseError::UnreadableRows {
                unreadable: failures.len(),
                total: rows.len(),
                id: *id,
                qso_id: qso_id.clone(),
                reason: reason.clone(),
            });
        }
        let adif_file = AdifFile {
            header: AdifHeader::default(),
            records,
        };

        let content = processor
            .generate_string(&adif_file)
            .map_err(|e| AsyncDatabaseError::Replay(format!("ADIF generate failed: {e}")))?;

        let temp = suffixed(path, ".migrating");
        remove_if_exists(&temp)?;
        let result = async {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| io_error(&temp, e))?;
            file.write_all(content.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|e| io_error(&temp, e))?;
            drop(file);
            let drift = self.drift_from(&AdifSnapshot::load(&temp).await?).await?;
            if !drift.is_current() {
                return Err(AsyncDatabaseError::Replay(format!(
                    "the exported logbook does not read back the same: {drift:?}"
                )));
            }
            std::fs::hard_link(&temp, path).map_err(|e| io_error(path, e))?;
            sync_parent(path);
            Ok(rows.len())
        }
        .await;
        remove_if_exists(&temp)?;
        result
    }

    /// Total number of QSOs in the index.
    pub async fn count_qsos(&self) -> Result<u64, AsyncDatabaseError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM qsos")
            .fetch_one(&self.pool)
            .await?;
        Ok(count as u64)
    }

    /// Persist a QSO's full state-transition + message timeline into the
    /// `qso_events` table (Layer 2 timeline persistence — see
    /// `docs/observability-diagnostics-plan.md`). Callers should gate this
    /// behind `[database].persist_qso_timeline` (default off); this method
    /// itself performs no gating so it stays trivially testable.
    ///
    /// Unlike `insert_qso`/`update_qso`, this always inserts a new row —
    /// a QSO id can legitimately appear more than once here (e.g. superseded
    /// then later cleaned up) and each is a distinct historical record, not
    /// an upsert target.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_qso_timeline(
        &self,
        qso_id: QsoId,
        callsign: Option<&str>,
        outcome: &str,
        reason: Option<&str>,
        state_history: &[StateTransition],
        messages: &[QsoMessage],
    ) -> Result<i64, AsyncDatabaseError> {
        let state_history_json = serde_json::to_string(state_history)?;
        let messages_json = serde_json::to_string(messages)?;
        let now = Utc::now().to_rfc3339();

        let result = sqlx::query(
            "INSERT INTO qso_events (qso_id, callsign, outcome, reason, state_history,
                                      messages, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(qso_id.to_string())
        .bind(callsign)
        .bind(outcome)
        .bind(reason)
        .bind(&state_history_json)
        .bind(&messages_json)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        debug!(
            "Persisted QSO timeline for {} ({}, {} transitions, {} messages)",
            qso_id,
            outcome,
            state_history.len(),
            messages.len()
        );
        Ok(id)
    }

    /// Fetch the most recently persisted timeline for `qso_id`, if any.
    ///
    /// A QSO can have more than one row (see `insert_qso_timeline`); this
    /// returns the latest so a caller reconstructing "what happened" gets
    /// the final, most-complete history.
    pub async fn get_qso_timeline(
        &self,
        qso_id: QsoId,
    ) -> Result<Option<QsoTimelineRecord>, AsyncDatabaseError> {
        // (qso_id, callsign, outcome, reason, state_history_json, messages_json, created_at)
        type QsoEventRow = (
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            String,
        );
        let row: Option<QsoEventRow> = sqlx::query_as(
            "SELECT qso_id, callsign, outcome, reason, state_history, messages, created_at
                 FROM qso_events WHERE qso_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(qso_id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some((
                qso_id_str,
                callsign,
                outcome,
                reason,
                state_history_json,
                messages_json,
                created_at,
            )) => {
                let qso_id =
                    Uuid::parse_str(&qso_id_str).map_err(|e| AsyncDatabaseError::InvalidQuery {
                        message: format!("stored qso_id {qso_id_str} is not a valid UUID: {e}"),
                    })?;
                let state_history: Vec<StateTransition> =
                    serde_json::from_str(&state_history_json)?;
                let messages: Vec<QsoMessage> = serde_json::from_str(&messages_json)?;
                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| AsyncDatabaseError::InvalidQuery {
                        message: format!(
                            "stored created_at {created_at} is not valid RFC3339: {e}"
                        ),
                    })?;
                Ok(Some(QsoTimelineRecord {
                    qso_id,
                    callsign,
                    outcome,
                    reason,
                    state_history,
                    messages,
                    created_at,
                }))
            }
            None => Ok(None),
        }
    }
}

// QsoDatabase is automatically Send + Sync thanks to SqlitePool

#[cfg(test)]
mod tests {
    use super::*;

    /// PAN-281: every connection the pool can hand out commits durably. Five
    /// connections held at once (the pool's max) are five distinct ones; when
    /// one PRAGMA through the pool set `synchronous` to NORMAL, one of them
    /// reported 1 instead of 2.
    #[tokio::test]
    async fn every_pooled_connection_is_wal_and_synchronous_full() {
        let tmp = tempfile::tempdir().unwrap();
        let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();

        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(db.pool.acquire().await.unwrap());
        }
        let mut seen = Vec::new();
        for conn in &mut held {
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            seen.push((synchronous, journal_mode));
        }

        let expected = vec![(2, "wal".to_string()); 5];
        assert_eq!(seen, expected, "(synchronous, journal_mode) per connection");
        drop(held);
        db.close().await;
    }

    // --- Security: I-8 (VACUUM INTO path escaping) -------------------------

    #[test]
    fn test_escape_sqlite_string_literal_doubles_single_quotes() {
        // A path containing a single quote must have it doubled so it cannot
        // terminate the surrounding string literal in `VACUUM INTO '<path>'`.
        assert_eq!(
            escape_sqlite_string_literal("/tmp/back'up.db"),
            "/tmp/back''up.db"
        );
        // An injection attempt: the closing quote is doubled, neutralizing it.
        assert_eq!(
            escape_sqlite_string_literal("x'; DROP TABLE qsos; --"),
            "x''; DROP TABLE qsos; --"
        );
    }

    #[test]
    fn test_escape_sqlite_string_literal_passthrough() {
        // Normal paths are returned unchanged.
        let p = "/home/op/.pancetta/backups/qso-2026.db";
        assert_eq!(escape_sqlite_string_literal(p), p);
    }

    #[tokio::test]
    async fn state_history_is_chronological_not_insertion_order() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let processor = AdifProcessor::new();
        let file = processor.parse_string("<EOH>\n\
            <CALL:5>W5ABC <QSO_DATE:8>20250102 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <STATE:2>CA <EOR>\n\
            <CALL:5>W5ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <STATE:2>AR <EOR>").unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();
        assert_eq!(
            db.try_get_worked_callsigns_and_states().await.unwrap(),
            vec![("W5ABC".into(), "AR".into()), ("W5ABC".into(), "CA".into())]
        );
    }

    #[tokio::test]
    async fn fallible_display_history_distinguishes_empty_from_failure() {
        let db = QsoDatabase::open(":memory:").await.unwrap();
        assert!(db
            .try_get_worked_bands_and_grids()
            .await
            .unwrap()
            .is_empty());
        assert!(db
            .try_get_worked_callsigns_and_states()
            .await
            .unwrap()
            .is_empty());
        db.pool.close().await;
        assert!(db.try_get_worked_bands_and_grids().await.is_err());
        assert!(db.try_get_worked_callsigns_and_states().await.is_err());
        assert!(db.get_worked_bands_and_grids().await.is_empty());
        assert!(db.get_worked_callsigns_and_states().await.is_empty());
    }

    #[tokio::test]
    async fn test_backup_rejects_nul_and_newline_paths() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        assert!(matches!(
            db.backup("/tmp/ev\0il.db").await,
            Err(AsyncDatabaseError::InvalidQuery { .. })
        ));
        assert!(matches!(
            db.backup("/tmp/ev\nil.db").await,
            Err(AsyncDatabaseError::InvalidQuery { .. })
        ));
    }

    /// A real on-disk database whose non-header pages are overwritten, so
    /// opening it still works (page 1 / `sqlite_master` is intact) but a full
    /// read such as `VACUUM INTO` fails with "database disk image is malformed".
    async fn open_corrupted_db(dir: &std::path::Path) -> QsoDatabase {
        let db_path = dir.join("corrupt.db");
        let db = QsoDatabase::open(&db_path).await.unwrap();
        // Fold the WAL into the main file so every page is on disk to corrupt.
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&db.pool)
            .await
            .unwrap();
        // Corrupt the `qsos` table's root page: schema init on reopen never
        // reads it, but VACUUM copies every table row and must.
        let root: i64 =
            sqlx::query_scalar("SELECT rootpage FROM sqlite_master WHERE name = 'qsos'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        db.pool.close().await;
        let mut bytes = std::fs::read(&db_path).unwrap();
        let start = (root as usize - 1) * 4096;
        for b in &mut bytes[start..start + 4096] {
            *b = 0xFF;
        }
        std::fs::write(&db_path, &bytes).unwrap();
        QsoDatabase::open(&db_path).await.unwrap()
    }

    #[tokio::test]
    async fn test_failed_backup_removes_the_partial_target() {
        // sophon 2026-10-07 / 2026-10-15: each failed VACUUM INTO left a
        // 0-byte `qso_backup_*.db` behind, which then counted towards
        // `retain_count` and pushed real backups out.
        let dir = tempfile::tempdir().unwrap();
        let db = open_corrupted_db(dir.path()).await;
        let target = dir.path().join("backup.db");

        assert!(db.backup(&target).await.is_err());
        assert!(
            !target.exists(),
            "a failed backup must not leave a partial file behind"
        );
    }

    #[tokio::test]
    async fn test_concurrent_same_name_backups_keep_one_valid_backup() {
        // PR #453 review (Codex P2): two loggers backing up in the same
        // second derive the same file name; the loser must not delete the
        // winner's backup.
        let dir = tempfile::tempdir().unwrap();
        let db = QsoDatabase::open(dir.path().join("ok.db")).await.unwrap();
        for i in 0..20 {
            let target = dir.path().join(format!("same-{i}.db"));
            let (a, b) = tokio::join!(db.backup(&target), db.backup(&target));
            assert!(
                a.is_ok() ^ b.is_ok(),
                "exactly one backup wins: {a:?} / {b:?}"
            );
            let copy = QsoDatabase::open(&target)
                .await
                .expect("winner is a valid db");
            assert_eq!(copy.count_qsos().await.unwrap(), 0);
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".partial-"))
            .collect();
        assert!(leftovers.is_empty(), "no partial files left behind");
    }

    #[tokio::test]
    async fn test_failed_backup_never_removes_a_preexisting_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = QsoDatabase::open(dir.path().join("ok.db")).await.unwrap();
        let target = dir.path().join("existing.db");
        std::fs::write(&target, b"not ours").unwrap();

        // VACUUM INTO refuses a non-empty existing target.
        assert!(db.backup(&target).await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"not ours");
    }

    // --- Security: I-9 (LIMIT clamping) -----------------------------------

    #[test]
    fn test_clamp_query_limit() {
        assert_eq!(clamp_query_limit(0), 0);
        assert_eq!(clamp_query_limit(10), 10);
        assert_eq!(clamp_query_limit(MAX_QUERY_LIMIT), MAX_QUERY_LIMIT);
        // Above the cap is clamped down.
        assert_eq!(clamp_query_limit(MAX_QUERY_LIMIT + 1), MAX_QUERY_LIMIT);
        assert_eq!(clamp_query_limit(u32::MAX), MAX_QUERY_LIMIT);
    }

    #[tokio::test]
    async fn test_async_database_creation() {
        let db = QsoDatabase::new_in_memory().await;
        assert!(db.is_ok());
    }

    #[tokio::test]
    async fn test_insert_and_get_qso() {
        let db = QsoDatabase::new_in_memory().await.unwrap();

        let progress = QsoProgress {
            state: QsoState::Idle,
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id: Uuid::new_v4(),
                our_callsign: "W1ABC".to_string(),
                their_callsign: Some("K2DEF".to_string()),
                frequency: 14074000.0,
                completed_rf_frequency_hz: None,
                mode: "FT8".to_string(),
                start_time: Utc::now(),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
            },
        };

        // Insert QSO
        let id = db.insert_qso(&progress).await.unwrap();
        assert!(id > 0);

        // Get QSO back
        let retrieved = db.get_qso(progress.metadata.qso_id).await.unwrap();
        assert_eq!(retrieved.metadata.qso_id, progress.metadata.qso_id);
    }

    // --- DX Hunter per-band-needed (2026-07-18): get_worked_bands_and_callsigns ---

    #[tokio::test]
    async fn get_worked_bands_and_callsigns_returns_every_band_in_one_query() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        // 20m (14.074 MHz) and 40m (7.074 MHz) — distinct bands, distinct
        // callsigns. Unlike get_worked_callsigns (one band per call), this
        // must return BOTH pairs from a single query.
        db.insert_qso(&duplicate_check_test_progress("JA1ABC", 14_074_000.0))
            .await
            .unwrap();
        db.insert_qso(&duplicate_check_test_progress("VK2XYZ", 7_074_000.0))
            .await
            .unwrap();

        let mut pairs = db.get_worked_bands_and_callsigns().await;
        pairs.sort();

        assert_eq!(pairs.len(), 2);
        assert!(pairs
            .iter()
            .any(|(band, call)| band.eq_ignore_ascii_case("20m") && call == "JA1ABC"));
        assert!(pairs
            .iter()
            .any(|(band, call)| band.eq_ignore_ascii_case("40m") && call == "VK2XYZ"));
    }

    #[tokio::test]
    async fn get_worked_bands_and_callsigns_empty_db_returns_empty() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        assert!(db.get_worked_bands_and_callsigns().await.is_empty());
    }

    // --- DX Hunter per-band-needed (2026-07-18): get_worked_bands_and_grids ---

    #[tokio::test]
    async fn get_worked_bands_and_grids_returns_every_band_in_one_query() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_qso(&duplicate_check_test_progress("JA1ABC", 14_074_000.0))
            .await
            .unwrap();
        db.insert_qso(&duplicate_check_test_progress("VK2XYZ", 7_074_000.0))
            .await
            .unwrap();

        let mut pairs = db.get_worked_bands_and_grids().await;
        pairs.sort();

        // duplicate_check_test_progress doesn't set a grid, so with the default
        // fixture this should be empty (grids.theirs is None) — this pins the
        // "no grid on the QSO -> no pair emitted" behavior, not a false-positive.
        assert!(pairs.is_empty());
    }

    #[tokio::test]
    async fn get_worked_bands_and_grids_includes_grid_when_present() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let mut progress = duplicate_check_test_progress("JA1ABC", 14_074_000.0);
        progress.metadata.grids.theirs = Some("PM95".to_string());
        db.insert_qso(&progress).await.unwrap();

        let pairs = db.get_worked_bands_and_grids().await;
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].0.eq_ignore_ascii_case("20m"));
        assert_eq!(pairs[0].1, "PM95");
    }

    #[tokio::test]
    async fn get_worked_bands_and_grids_empty_db_returns_empty() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        assert!(db.get_worked_bands_and_grids().await.is_empty());
    }

    fn duplicate_check_test_progress(callsign: &str, frequency: f64) -> QsoProgress {
        QsoProgress {
            state: QsoState::Completed {
                their_callsign: callsign.into(),
                their_report: -10,
                our_report: -12,
                frequency,
                grid_square: None,
                completed_at: Utc::now(),
                duration_seconds: 60,
            },
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id: Uuid::new_v4(),
                our_callsign: "W1ABC".to_string(),
                their_callsign: Some(callsign.to_string()),
                frequency,
                completed_rf_frequency_hz: None,
                mode: "FT8".to_string(),
                start_time: Utc::now() - chrono::Duration::minutes(5),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
            },
        }
    }

    /// PAN-347: the four-record probe log. DL1ABC 20m FT8 no QSL; JA1ABC
    /// QSL_RCVD=N + LOTW_QSL_RCVD=Y; G4ABC MFSK/FT4 QSL_RCVD=Y; F5ABC BAND=40m
    /// with no FREQ and eQSL only.
    async fn four_record_fixture() -> (tempfile::TempDir, QsoDatabase) {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let adif = "PAN-347 fixture\n<ADIF_VER:5>3.1.4 <EOH>\n\
            <CALL:6>DL1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m <EOR>\n\
            <CALL:6>JA1ABC <QSO_DATE:8>20250101 <TIME_ON:6>121500 <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m <QSL_RCVD:1>N <LOTW_QSL_RCVD:1>Y <EOR>\n\
            <CALL:5>G4ABC <QSO_DATE:8>20250101 <TIME_ON:6>123000 <MODE:4>MFSK <SUBMODE:3>FT4 <FREQ:8>7.047500 <BAND:3>40m <QSL_RCVD:1>Y <EOR>\n\
            <CALL:5>F5ABC <QSO_DATE:8>20250101 <TIME_ON:6>124500 <MODE:3>FT4 <BAND:3>40m <EQSL_QSL_RCVD:1>Y <EOR>\n";
        tokio::fs::write(&adif_path, adif).await.unwrap();
        let db = QsoDatabase::replay_from_adif(":memory:", &adif_path)
            .await
            .unwrap();
        (tmp, db)
    }

    #[tokio::test]
    async fn worked_history_reads_mode_confirmation_and_logged_band() {
        let (_tmp, db) = four_record_fixture().await;
        let mut rows = db.worked_history().await.unwrap();
        rows.sort_by(|a, b| a.callsign.cmp(&b.callsign));
        let view: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.callsign.as_str(),
                    r.band.as_str(),
                    r.mode.as_str(),
                    r.confirmed,
                )
            })
            .collect();
        assert_eq!(
            view,
            vec![
                ("DL1ABC", "20M", "FT8", false), // no QSL fields
                ("F5ABC", "40M", "FT4", false),  // no FREQ: logged BAND; eQSL does not confirm
                ("G4ABC", "40M", "FT4", true),   // MFSK/FT4 indexes as FT4; QSL_RCVD=Y
                ("JA1ABC", "20M", "FT8", true),  // QSL_RCVD=N but LOTW_QSL_RCVD=Y
            ]
        );
    }

    #[tokio::test]
    async fn statistics_count_confirmed_contacts_from_qsl_fields() {
        let (_tmp, db) = four_record_fixture().await;
        assert_eq!(db.get_statistics().await.unwrap().confirmed_qsos, 2);
    }

    #[test]
    fn confirmation_values_are_y_or_v_only() {
        for v in ["Y", "y", " V "] {
            assert!(is_confirmed_qsl_value(v), "{v:?}");
        }
        for v in ["N", "R", "I", "", "YES"] {
            assert!(!is_confirmed_qsl_value(v), "{v:?}");
        }
    }

    #[tokio::test]
    async fn pan260_band_window_and_history() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let now = DateTime::parse_from_rfc3339("2026-11-04T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(db.worked_history().await.unwrap().is_empty());
        for (age_seconds, expected) in [
            (1800, true),
            (86400, false),
            (86401, false),
            (0, true),
            (-1, false),
        ] {
            let mut p = duplicate_check_test_progress("k1abc", 14_075_200.0);
            p.metadata.start_time = now - chrono::Duration::seconds(age_seconds);
            db.insert_qso(&p).await.unwrap();
            assert_eq!(
                db.check_duplicate("K1ABC", Some("20M"), now, 24)
                    .await
                    .unwrap()
                    .is_some(),
                expected
            );
            assert!(db
                .check_duplicate("K1ABC", Some("40m"), now, 24)
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                db.check_duplicate("K1ABC", None, now, 24)
                    .await
                    .unwrap()
                    .is_some(),
                expected
            );
            assert!(db
                .check_duplicate("K1ABC", Some("20m"), now, 0)
                .await
                .unwrap()
                .is_none());
            db.delete_qso(p.metadata.qso_id).await.unwrap();
        }
        for (delta, expected) in [(-1, true), (0, false), (1, false)] {
            let mut p = duplicate_check_test_progress("K1ABC", 14_075_200.0);
            p.metadata.start_time =
                now - chrono::Duration::hours(24) - chrono::Duration::nanoseconds(delta);
            db.insert_qso(&p).await.unwrap();
            assert_eq!(
                db.check_duplicate("k1abc", Some("20m"), now, 24)
                    .await
                    .unwrap()
                    .is_some(),
                expected
            );
            db.delete_qso(p.metadata.qso_id).await.unwrap();
        }
        let mut future = duplicate_check_test_progress("K1ABC", 14_075_800.0);
        future.metadata.start_time = now + chrono::Duration::nanoseconds(1);
        db.insert_qso(&future).await.unwrap();
        assert!(db
            .check_duplicate("k1abc", Some("20m"), now, 24)
            .await
            .unwrap()
            .is_none());
        db.delete_qso(future.metadata.qso_id).await.unwrap();
        for (rf, age) in [(14_075_200.0, 60), (14_075_800.0, 30), (7_075_200.0, 10)] {
            let mut p = duplicate_check_test_progress("k1abc", rf);
            p.metadata.start_time = now - chrono::Duration::minutes(age);
            db.insert_qso(&p).await.unwrap();
        }
        sqlx::query("UPDATE qsos SET adif_data = json_set(adif_data, '$.band', lower(json_extract(adif_data, '$.band')))").execute(&db.pool).await.unwrap();
        assert!(db
            .check_duplicate("K1ABC", Some("20M"), now, 24)
            .await
            .unwrap()
            .is_some());
        // PAN-347: one row per contact; the cache keeps the latest date.
        let history = db.worked_history().await.unwrap();
        assert_eq!(history.len(), 3);
        let latest_20m = history
            .iter()
            .filter(|r| r.band == "20M" && r.callsign == "K1ABC")
            .map(|r| r.start_time)
            .max();
        assert_eq!(latest_20m, Some(now - chrono::Duration::minutes(30)));
        assert!(history
            .iter()
            .any(|r| r.band == "40M" && r.callsign == "K1ABC"));
        sqlx::query("UPDATE qsos SET metadata = json_set(metadata, '$.start_time', 'invalid')")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db.worked_history().await.is_err());
        db.pool.close().await;
        assert!(db.worked_history().await.is_err());
        assert!(db
            .check_duplicate("K1ABC", Some("20m"), now, 24)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_update_qso() {
        let db = QsoDatabase::new_in_memory().await.unwrap();

        let mut progress = QsoProgress {
            state: QsoState::Idle,
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id: Uuid::new_v4(),
                our_callsign: "W1ABC".to_string(),
                their_callsign: Some("K2DEF".to_string()),
                frequency: 14074000.0,
                completed_rf_frequency_hz: None,
                mode: "FT8".to_string(),
                start_time: Utc::now(),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
            },
        };

        // Insert QSO
        db.insert_qso(&progress).await.unwrap();

        // Update QSO
        progress.state = QsoState::Completed {
            their_callsign: "K2DEF".to_string(),
            their_report: -10,
            our_report: -15,
            frequency: 14074000.0,
            grid_square: Some("FN42".to_string()),
            completed_at: Utc::now(),
            duration_seconds: 120,
        };

        db.update_qso(&progress).await.unwrap();

        // Verify update
        let retrieved = db.get_qso(progress.metadata.qso_id).await.unwrap();
        assert!(matches!(retrieved.state, QsoState::Completed { .. }));
    }

    #[tokio::test]
    async fn replay_from_adif_round_trips_records() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let db_path = tmp.path().join("qsos.db");

        // Two records, valid ADIF
        let adif_contents = "Pancetta ADIF round-trip test\n\
            <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
            <EOH>\n\
            \n\
            <CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
            <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
            <EOR>\n\
            \n\
            <CALL:5>K9DEF <QSO_DATE:8>20250102 <TIME_ON:6>121500 \
            <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
            <EOR>\n";
        tokio::fs::write(&adif_path, adif_contents).await.unwrap();

        let db = QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        let count = db.count_qsos().await.unwrap();
        assert_eq!(count, 2, "expected 2 records replayed, got {}", count);

        // frequency_to_band returns uppercase ("20M") — coordinator also uppercases.
        let calls = db.get_worked_callsigns("20M").await;
        assert!(
            calls.contains(&"W1ABC".to_string()),
            "missing W1ABC in {:?}",
            calls
        );
        assert!(
            calls.contains(&"K9DEF".to_string()),
            "missing K9DEF in {:?}",
            calls
        );
    }

    /// PAN-127: `close()` must leave a file-backed index quiescent — every
    /// committed frame checkpointed into `qso.db`, the WAL sidecar empty or
    /// gone — before it returns. Before the fix the pool's close left a
    /// connection alive with the whole log still in the WAL, so a caller
    /// that snapshotted `qso.db`'s bytes or mtime right after `close()`
    /// raced the connection's late checkpoint (`tests/import_cli.rs` failed
    /// 24/40 parallel runs).
    #[tokio::test]
    async fn close_leaves_a_file_index_quiescent() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let db_path = tmp.path().join("qso.db");
        tokio::fs::write(&adif_path, INSERT_FIXTURE).await.unwrap();

        QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap()
            .close()
            .await;

        let wal_len = std::fs::metadata(tmp.path().join("qso.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert_eq!(
            wal_len, 0,
            "close() must checkpoint the WAL into qso.db before returning"
        );

        // The checkpointed file must be complete on its own: reopening it
        // (no WAL to recover from) sees every record.
        let reopened = QsoDatabase::open(&db_path).await.unwrap();
        assert_eq!(reopened.count_qsos().await.unwrap(), 3);
        reopened.close().await;
    }

    /// PAN-127: three records exercising band, grid and state seeding.
    const INSERT_FIXTURE: &str = "<ADIF_VER:5>3.1.4 <EOH>\n\
        <CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 \
        <FREQ:9>14.074000 <BAND:3>20m <GRIDSQUARE:4>FN42 <STATE:2>MA <EOR>\n\
        <CALL:5>K9DEF <QSO_DATE:8>20250102 <TIME_ON:4>1215 <MODE:4>MFSK \
        <SUBMODE:3>FT4 <FREQ:8>7.047500 <RST_SENT:3>-07 <RST_RCVD:3>+02 <EOR>\n\
        <CALL:5>N0XYZ <QSO_DATE:8>20250103 <TIME_ON:6>130000 <MODE:3>FT8 \
        <FREQ:9>21.074000 <GRIDSQUARE:6>EM10ab <STATE:2>TX <EOR>\n";

    fn sorted(mut pairs: Vec<(String, String)>) -> Vec<(String, String)> {
        pairs.sort();
        pairs
    }

    #[tokio::test]
    async fn insert_adif_records_indexes_like_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        tokio::fs::write(&adif_path, INSERT_FIXTURE).await.unwrap();
        let replayed = QsoDatabase::replay_from_adif(":memory:", &adif_path)
            .await
            .unwrap();

        let processor = AdifProcessor::new();
        let file = processor.parse_string(INSERT_FIXTURE).unwrap();
        let inserted = QsoDatabase::new_in_memory().await.unwrap();
        let counts = inserted
            .insert_adif_records(&processor, &file.records)
            .await
            .unwrap();

        assert_eq!(counts, (3, 0));
        assert_eq!(inserted.count_qsos().await.unwrap(), 3);
        assert_eq!(
            inserted.count_qsos().await.unwrap(),
            replayed.count_qsos().await.unwrap()
        );
        let bands_and_calls = sorted(inserted.get_worked_bands_and_callsigns().await);
        assert_eq!(bands_and_calls.len(), 3, "{bands_and_calls:?}");
        assert_eq!(
            bands_and_calls,
            sorted(replayed.get_worked_bands_and_callsigns().await)
        );
        assert_eq!(
            sorted(inserted.get_worked_bands_and_grids().await),
            sorted(replayed.get_worked_bands_and_grids().await)
        );
        let states = inserted.get_worked_callsigns_and_states().await;
        assert_eq!(states.len(), 2, "{states:?}");
        assert_eq!(states, replayed.get_worked_callsigns_and_states().await);
        inserted.close().await;
        replayed.close().await;
    }

    /// PR #451 review (Codex P2): an import's rows must commit together. A
    /// failure part-way (here a trigger rejecting the second row; in the
    /// field, a kill or power loss) must leave none of them behind, or the
    /// partial index can look current and never be rebuilt.
    #[tokio::test]
    async fn insert_adif_records_commits_all_rows_or_none() {
        let tmp = tempfile::tempdir().unwrap();
        let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_k9def BEFORE INSERT ON qsos \
             WHEN json_extract(NEW.metadata, '$.their_callsign') = 'K9DEF' \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let processor = AdifProcessor::new();
        let file = processor.parse_string(INSERT_FIXTURE).unwrap();

        let result = db.insert_adif_records(&processor, &file.records).await;

        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            db.count_qsos().await.unwrap(),
            0,
            "a failed import must not leave part of its rows indexed"
        );
        db.close().await;
    }

    #[tokio::test]
    async fn insert_adif_records_skips_a_record_with_no_call() {
        let adif = "<CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 <EOR>\n\
                    <CALL:0> <QSO_DATE:8>20250101 <TIME_ON:6>121000 <MODE:3>FT8 <EOR>\n\
                    <CALL:5>K9DEF <QSO_DATE:8>20250101 <TIME_ON:6>122000 <MODE:3>FT8 <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let db = QsoDatabase::new_in_memory().await.unwrap();

        let counts = db.insert_adif_records(&processor, &file.records).await;

        assert_eq!(counts.unwrap(), (2, 1));
        assert_eq!(db.count_qsos().await.unwrap(), 2);
    }

    /// PR #451 review (Codex P2): an imported LoTW/QRZ record keeps its QSL
    /// state through the index, so `pancetta export` (which regenerates ADIF
    /// from the indexed metadata) does not reset a confirmed contact to
    /// `QSL_RCVD=N`.
    #[tokio::test]
    async fn imported_qsl_state_survives_index_and_export() {
        let adif = "<CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 \
                    <FREQ:9>14.074000 <QSL_SENT:1>Y <QSL_RCVD:1>Y <QSLSDATE:8>20250102 \
                    <QSLRDATE:8>20250110 <QSL_SENT_VIA:1>E <QSL_RCVD_VIA:1>E \
                    <QSLMSG:6>TNX 73 <LOTW_QSL_SENT:1>Y <LOTW_QSL_RCVD:1>Y \
                    <LOTW_QSLSDATE:8>20250102 <LOTW_QSLRDATE:8>20250110 \
                    <EQSL_QSL_RCVD:1>Y <QRZCOM_QSO_UPLOAD_STATUS:1>Y <EOR>\n\
                    <CALL:5>K9DEF <QSO_DATE:8>20250101 <TIME_ON:6>121000 <MODE:3>FT8 \
                    <FREQ:9>14.074000 <QSL_SENT:1>R <QSL_RCVD:1>I <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();

        // The same conversion `pancetta export` applies to each indexed row.
        let progresses = db
            .search_qsos(&QsoFilter::default(), &QueryOptions::default())
            .await
            .unwrap();
        let exported: HashMap<String, HashMap<String, String>> = progresses
            .iter()
            .map(|progress| {
                let adif_qso = processor
                    .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
                let record = processor.qso_to_record(&adif_qso);
                let fields: HashMap<String, String> = record
                    .fields
                    .into_iter()
                    .map(|(name, field)| (name, field.value))
                    .collect();
                (fields["CALL"].clone(), fields)
            })
            .collect();

        let confirmed = &exported["W1ABC"];
        for (name, value) in [
            ("QSL_SENT", "Y"),
            ("QSL_RCVD", "Y"),
            ("QSLSDATE", "20250102"),
            ("QSLRDATE", "20250110"),
            ("QSL_SENT_VIA", "E"),
            ("QSL_RCVD_VIA", "E"),
            ("QSLMSG", "TNX 73"),
            ("LOTW_QSL_SENT", "Y"),
            ("LOTW_QSL_RCVD", "Y"),
            ("LOTW_QSLSDATE", "20250102"),
            ("LOTW_QSLRDATE", "20250110"),
            ("EQSL_QSL_RCVD", "Y"),
            ("QRZCOM_QSO_UPLOAD_STATUS", "Y"),
        ] {
            assert_eq!(
                confirmed.get(name).map(String::as_str),
                Some(value),
                "{name} lost through import → export: {confirmed:?}"
            );
        }
        let requested = &exported["K9DEF"];
        assert_eq!(requested["QSL_SENT"], "R", "{requested:?}");
        assert_eq!(requested["QSL_RCVD"], "I", "{requested:?}");
        db.close().await;
    }

    /// PR #451 review (Codex P2): standard ADIF fields the index has no column
    /// or metadata field for (OPERATOR, TX_PWR, COUNTRY, DXCC, CQZ, ITUZ,
    /// NOTES, the serial strings, a third-party CONTEST_ID with serials, a
    /// TIME_OFF with no QSO_DATE_OFF, a CW report, the BAND of a record with
    /// no FREQ, the MODE behind a SUBMODE) survive import → index → export.
    #[tokio::test]
    async fn imported_standard_fields_survive_index_and_export() {
        let adif = "<CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:4>MFSK \
                    <SUBMODE:3>FT4 <BAND:3>40M <OPERATOR:4>W5AU <TX_PWR:3>100 \
                    <COUNTRY:13>United States <DXCC:3>291 <CQZ:1>5 <ITUZ:1>8 \
                    <NOTES:13>first contact <STX_STRING:3>001 <SRX_STRING:5>123AB \
                    <RST_SENT:3>599 <RST_RCVD:3>5NN <STX:2>17 <SRX:3>342 \
                    <CONTEST_ID:12>KS-QSO-PARTY \
                    <TIME_OFF:6>120130 <COMMENT:15>Worked portable <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();

        // The same conversion `pancetta export` applies to each indexed row.
        let progresses = db
            .search_qsos(&QsoFilter::default(), &QueryOptions::default())
            .await
            .unwrap();
        assert_eq!(progresses.len(), 1);
        let metadata = &progresses[0].metadata;
        let adif_qso = processor.qso_to_adif(metadata, metadata.contest_info.as_ref());
        let exported: HashMap<String, String> = processor
            .qso_to_record(&adif_qso)
            .fields
            .into_iter()
            .map(|(name, field)| (name, field.value))
            .collect();

        for (name, value) in [
            ("MODE", "MFSK"),
            ("SUBMODE", "FT4"),
            ("BAND", "40M"),
            ("OPERATOR", "W5AU"),
            ("TX_PWR", "100"),
            ("COUNTRY", "United States"),
            ("DXCC", "291"),
            ("CQZ", "5"),
            ("ITUZ", "8"),
            ("NOTES", "first contact"),
            ("STX_STRING", "001"),
            ("SRX_STRING", "123AB"),
            ("RST_SENT", "599"),
            ("RST_RCVD", "5NN"),
            ("STX", "17"),
            ("SRX", "342"),
            // PAN-52: a third-party CONTEST_ID (an ADIF-enumeration value
            // here, but any verbatim string must round-trip) re-exports
            // byte-identically through the index.
            ("CONTEST_ID", "KS-QSO-PARTY"),
            ("TIME_OFF", "120130"),
            // A third-party comment exports as read, with no Pancetta
            // attribution appended.
            ("COMMENT", "Worked portable"),
        ] {
            assert_eq!(
                exported.get(name).map(String::as_str),
                Some(value),
                "{name} lost through import → export: {exported:?}"
            );
        }
        db.close().await;
    }

    /// PAN-214: an imported field's ADIF type indicator survives import →
    /// index → export, so `pancetta export` writes `<APP_LOGGER_SCORE:2:N>42`
    /// back as read instead of dropping the `:N`.
    #[tokio::test]
    async fn imported_type_indicators_survive_index_and_export() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
                    <APP_LOGGER_SCORE:2:N>42 <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();

        // The same conversion `pancetta export` applies to each indexed row.
        let progresses = db
            .search_qsos(&QsoFilter::default(), &QueryOptions::default())
            .await
            .unwrap();
        assert_eq!(progresses.len(), 1);
        let metadata = &progresses[0].metadata;
        let adif_qso = processor.qso_to_adif(metadata, metadata.contest_info.as_ref());
        let exported = processor
            .generate_string(&crate::adif::AdifFile {
                header: crate::adif::AdifHeader::default(),
                records: vec![processor.qso_to_record(&adif_qso)],
            })
            .unwrap();

        assert!(
            exported.contains("<APP_LOGGER_SCORE:2:N>42"),
            "the :N type indicator was lost through import → export: {exported}"
        );
        db.close().await;
    }

    /// PAN-214: a row stored before `adif_field_types` existed has no such key
    /// in its persisted `QsoMetadata`. It must still load, with no type
    /// indicators, rather than drop out of every search.
    #[tokio::test]
    async fn a_row_stored_before_type_indicators_still_loads() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
                    <APP_LOGGER_SCORE:2:N>42 <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();
        let key_type = "SELECT json_type(progress_data, '$.metadata.adif_field_types') FROM qsos";
        let stored: Option<String> = sqlx::query_scalar(key_type)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("object"),
            "the import stored no types"
        );
        // Rewrite the row to its pre-PAN-214 shape: no `adif_field_types` key.
        sqlx::query(
            "UPDATE qsos SET \
             progress_data = json_remove(progress_data, '$.metadata.adif_field_types'), \
             metadata = json_remove(metadata, '$.adif_field_types')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let stored: Option<String> = sqlx::query_scalar(key_type)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(stored, None);

        let progresses = db
            .search_qsos(&QsoFilter::default(), &QueryOptions::default())
            .await
            .unwrap();

        assert_eq!(progresses.len(), 1, "the pre-PAN-214 row failed to load");
        assert!(
            progresses[0].metadata.adif_field_types.is_empty(),
            "{:?}",
            progresses[0].metadata.adif_field_types
        );
        db.close().await;
    }

    /// PR #502 review (Codex P1): a row imported before PAN-52 holds its
    /// file's `CONTEST_ID` only in `contest_name` — its persisted
    /// `ContestInfo` has no `adif_contest_id` key. Upgrading must not drop
    /// that `CONTEST_ID` from the startup DB→ADIF migration's export.
    #[tokio::test]
    async fn pre_pan52_imported_contest_id_survives_export() {
        // The second id collides with a built-in profile slug; an imported
        // value is still preserved verbatim (PR #502 review, Codex P2).
        for imported in ["KS-QSO-PARTY", "us-state-qso-party"] {
            let adif = format!(
                "<CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 <MODE:3>FT8 \
                 <BAND:3>40M <CONTEST_ID:{}>{imported} <EOR>\n",
                imported.len()
            );
            let processor = AdifProcessor::new();
            let file = processor.parse_string(&adif).unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let db = QsoDatabase::open(tmp.path().join("qso.db")).await.unwrap();
            db.insert_adif_records(&processor, &file.records)
                .await
                .unwrap();
            // Rewrite the row to its pre-PAN-52 shape: no `adif_contest_id` key.
            sqlx::query(
                "UPDATE qsos SET progress_data = \
                 json_remove(progress_data, '$.metadata.contest_info.adif_contest_id')",
            )
            .execute(&db.pool)
            .await
            .unwrap();
            let (key_type, contest_name): (Option<String>, String) = sqlx::query_as(
                "SELECT json_type(progress_data, '$.metadata.contest_info.adif_contest_id'), \
                 json_extract(progress_data, '$.metadata.contest_info.contest_name') FROM qsos",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!((key_type, contest_name.as_str()), (None, imported));

            let exported_path = tmp.path().join("qsos.adi");
            db.export_to_adif(&exported_path).await.unwrap();
            let exported = std::fs::read_to_string(&exported_path).unwrap();
            let field = format!("<CONTEST_ID:{}>{imported}", imported.len());
            assert!(
                exported.contains(&field),
                "a pre-PAN-52 imported row lost its CONTEST_ID on export: {exported}"
            );
            db.close().await;
        }
    }

    /// PR #451 review (Codex P2): an imported (or replayed) row lands in its
    /// QSO's date range, not the day it was imported; a live-logged row still
    /// lands at the moment it was logged.
    #[tokio::test]
    async fn imported_rows_are_searchable_by_their_qso_date() {
        use chrono::TimeZone;
        let adif = "<CALL:5>W1ABC <QSO_DATE:8>20200315 <TIME_ON:6>120000 <MODE:3>FT8 \
                    <FREQ:9>14.074000 <EOR>\n";
        let processor = AdifProcessor::new();
        let file = processor.parse_string(adif).unwrap();
        let db = QsoDatabase::new_in_memory().await.unwrap();
        db.insert_adif_records(&processor, &file.records)
            .await
            .unwrap();
        let live = duplicate_check_test_progress("K9DEF", 14_074_000.0);
        db.insert_qso(&live).await.unwrap();

        let in_range = |start: DateTime<Utc>, end: DateTime<Utc>| QsoFilter {
            date_range: Some(DateRange { start, end }),
            ..Default::default()
        };
        let calls = |progresses: Vec<QsoProgress>| -> Vec<String> {
            let mut calls: Vec<String> = progresses
                .into_iter()
                .filter_map(|p| p.metadata.their_callsign)
                .collect();
            calls.sort();
            calls
        };
        let year_2020 = in_range(
            Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2020, 12, 31, 23, 59, 59).unwrap(),
        );
        let today = in_range(
            Utc::now() - chrono::Duration::hours(1),
            Utc::now() + chrono::Duration::hours(1),
        );
        let options = QueryOptions::default();

        assert_eq!(
            calls(db.search_qsos(&year_2020, &options).await.unwrap()),
            vec!["W1ABC".to_string()]
        );
        assert_eq!(
            calls(db.search_qsos(&today, &options).await.unwrap()),
            vec!["K9DEF".to_string()]
        );
        let records = db.search_qsos_records(&year_2020, &options).await.unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        db.close().await;
    }

    /// PR #453 review (Codex P2): a newer-but-unreadable `qsos.adi` must not
    /// cost the operator the existing index — the replay may only delete
    /// `qso.db` once the ADIF has been read and converted.
    #[tokio::test]
    async fn replay_from_unreadable_adif_keeps_the_existing_index() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let db_path = tmp.path().join("qsos.db");
        tokio::fs::write(
            &adif_path,
            "keep-index test\n<ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n<EOH>\n\n\
             <CALL:5>W1ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
             <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n<EOR>\n",
        )
        .await
        .unwrap();
        let db = QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        db.pool.close().await;

        tokio::fs::write(&adif_path, [0xFFu8, 0xFE, 0x00, 0x01])
            .await
            .unwrap();
        assert!(QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .is_err());

        let db = QsoDatabase::open(&db_path).await.unwrap();
        assert_eq!(
            db.count_qsos().await.unwrap(),
            1,
            "existing index must survive"
        );
    }

    /// PAN-85: a replayed ADIF record's `STATE` field must survive the
    /// parse -> `adif_to_qso` -> insert chain, so a previously-logged
    /// contact's state is available to seed `CachedStationLookup` at startup.
    #[tokio::test]
    async fn replay_from_adif_preserves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let db_path = tmp.path().join("qsos.db");

        let adif_contents = "Pancetta state round-trip test\n\
            <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
            <EOH>\n\
            \n\
            <CALL:5>W5ABC <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
            <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m <STATE:2>AR\n\
            <EOR>\n";
        tokio::fs::write(&adif_path, adif_contents).await.unwrap();

        let db = QsoDatabase::replay_from_adif(&db_path, &adif_path)
            .await
            .unwrap();
        let pairs = db.get_worked_callsigns_and_states().await;
        assert_eq!(pairs, vec![("W5ABC".to_string(), "AR".to_string())]);
    }

    /// PAN-41 round 3 (Codex on the round-2 `open_read_only` fix): a real
    /// `mode=ro` connection against a WAL-mode database can still create
    /// `-wal`/`-shm` sidecar files if they're missing, and a stale/missing
    /// real index would either read nothing or read stale rows -- both real
    /// gaps in the round-2 fix. The replacement rebuilds a throwaway index
    /// straight from ADIF into `:memory:` (via the same `replay_from_adif`
    /// case 2 above already uses for the write path), which is always fresh
    /// (never stale) and creates no file of any kind (no sidecar risk).
    #[tokio::test]
    async fn replay_from_adif_into_memory_seeds_from_the_real_adif_without_any_file() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        let adif_contents = "Pancetta replay-seed test\n\
            <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
            <EOH>\n\
            \n\
            <CALL:5>K2DEF <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
            <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
            <EOR>\n";
        tokio::fs::write(&adif_path, adif_contents).await.unwrap();

        let db = QsoDatabase::replay_from_adif(":memory:", &adif_path)
            .await
            .unwrap();
        let calls = db.get_worked_callsigns("20M").await;
        assert!(
            calls.contains(&"K2DEF".to_string()),
            "an in-memory rebuild from ADIF must see the real history: {:?}",
            calls
        );

        // Nothing besides the ADIF itself (read-only) may exist in the temp
        // dir -- no `.db`/`-wal`/`-shm` sidecar of any kind was created.
        let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
        let mut names = vec![];
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(
            names,
            vec!["qsos.adi"],
            "an in-memory replay rebuild must never create any file on disk: {:?}",
            names
        );
    }

    /// The missing/stale-index case this fixes: even with NO on-disk index
    /// at all, a fresh in-memory rebuild from ADIF still has full history
    /// (unlike a stale or missing real index, which the round-2 fix left
    /// unhandled).
    #[tokio::test]
    async fn replay_from_adif_into_memory_handles_a_missing_real_index() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        // No qso.db anywhere -- simulates a fresh install or a stale/dropped
        // real index; the in-memory rebuild must not depend on it existing.
        let adif_contents = "Pancetta replay-seed test\n\
            <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
            <EOH>\n\
            \n\
            <CALL:5>W9ZZZ <QSO_DATE:8>20250101 <TIME_ON:6>120000 \
            <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20m\n\
            <EOR>\n";
        tokio::fs::write(&adif_path, adif_contents).await.unwrap();

        let db = QsoDatabase::replay_from_adif(":memory:", &adif_path)
            .await
            .unwrap();
        let calls = db.get_worked_callsigns("20M").await;
        assert!(
            calls.contains(&"W9ZZZ".to_string()),
            "a missing real index must not prevent in-memory seeding from ADIF: {:?}",
            calls
        );
    }

    /// PAN-41 round 4 (P1 Codex finding): `replay_from_adif(":memory:", ..)`
    /// must never reach the `try_exists`/`remove_file` step at all -- on Unix,
    /// those would happily delete a REAL regular file that just happens to be
    /// named `:memory:` relative to the process's current directory, since
    /// `Self::open` only recognizes the sentinel afterward. This can't safely
    /// simulate "a file literally named `:memory:` sits in cwd" (mutating the
    /// process-wide current directory is unsound under `cargo test`'s default
    /// parallel execution), so it instead proves the function has no
    /// filesystem side effects at all: an unrelated real file's mtime is
    /// unchanged, and repeated in-memory calls never leave any file behind.
    #[tokio::test]
    async fn replay_from_adif_into_memory_never_touches_the_filesystem_for_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        let adif_path = tmp.path().join("qsos.adi");
        tokio::fs::write(
            &adif_path,
            "Pancetta replay-seed test\n<ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n<EOH>\n",
        )
        .await
        .unwrap();

        // An unrelated real file, standing in for "a file that happens to be
        // named `:memory:`" -- if the guard regressed and `try_exists`/
        // `remove_file` ran against a real path, a bug in the guard's own
        // comparison logic touching the wrong variable would be far more
        // likely to hit this file (present, in a shared dir) than silently
        // do nothing.
        let sentinel = tmp.path().join("do-not-touch");
        tokio::fs::write(&sentinel, b"precious").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mtime_before = std::fs::metadata(&sentinel).unwrap().modified().unwrap();

        for _ in 0..3 {
            let db = QsoDatabase::replay_from_adif(":memory:", &adif_path)
                .await
                .unwrap();
            drop(db);
        }

        assert!(sentinel.exists(), "an unrelated file must never be deleted");
        let mtime_after = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "an unrelated file must never be modified by an in-memory replay rebuild"
        );
    }

    // --- Layer 2 timeline persistence (docs/observability-diagnostics-plan.md
    // §"Persist the timeline") --------------------------------------------

    /// A non-trivial state_history: Idle -> RespondingToCq -> SendingReport
    /// -> Completed, driven by a mix of received/sent transitions.
    fn sample_state_history(now: DateTime<Utc>) -> Vec<StateTransition> {
        vec![
            StateTransition {
                from_state: QsoState::Idle,
                to_state: QsoState::RespondingToCq {
                    target_callsign: "K1DEF".to_string(),
                    frequency: 1500.0,
                    started_at: now,
                },
                timestamp: now,
                reason: TransitionReason::UserAction,
            },
            StateTransition {
                from_state: QsoState::RespondingToCq {
                    target_callsign: "K1DEF".to_string(),
                    frequency: 1500.0,
                    started_at: now,
                },
                to_state: QsoState::SendingReport {
                    their_callsign: "K1DEF".to_string(),
                    their_report: None,
                    our_report: -12,
                    frequency: 1500.0,
                    started_at: now,
                },
                timestamp: now + chrono::Duration::seconds(15),
                reason: TransitionReason::MessageReceived(MessageType::CqResponse {
                    calling_station: "K1DEF".to_string(),
                    responding_station: "W1ABC".to_string(),
                    grid: Some("FN42".to_string()),
                }),
            },
            StateTransition {
                from_state: QsoState::SendingReport {
                    their_callsign: "K1DEF".to_string(),
                    their_report: None,
                    our_report: -12,
                    frequency: 1500.0,
                    started_at: now,
                },
                to_state: QsoState::Completed {
                    their_callsign: "K1DEF".to_string(),
                    their_report: -9,
                    our_report: -12,
                    frequency: 1500.0,
                    grid_square: Some("FN42".to_string()),
                    completed_at: now + chrono::Duration::seconds(45),
                    duration_seconds: 45,
                },
                timestamp: now + chrono::Duration::seconds(45),
                reason: TransitionReason::MessageReceived(MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                }),
            },
        ]
    }

    fn sample_messages(now: DateTime<Utc>) -> Vec<QsoMessage> {
        vec![
            QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: MessageType::CqResponse {
                    calling_station: "K1DEF".to_string(),
                    responding_station: "W1ABC".to_string(),
                    grid: Some("FN42".to_string()),
                },
                raw_text: "K1DEF W1ABC FN42".to_string(),
                signal_strength: None,
                frequency: 1500.0,
            },
            QsoMessage {
                timestamp: now + chrono::Duration::seconds(15),
                direction: MessageDirection::Received,
                message_type: MessageType::SignalReport {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -9,
                },
                raw_text: "W1ABC K1DEF -09".to_string(),
                signal_strength: Some(-9.0),
                frequency: 1500.0,
            },
            QsoMessage {
                timestamp: now + chrono::Duration::seconds(45),
                direction: MessageDirection::Received,
                message_type: MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                raw_text: "W1ABC K1DEF 73".to_string(),
                signal_strength: Some(-8.0),
                frequency: 1500.0,
            },
        ]
    }

    #[tokio::test]
    async fn qso_timeline_round_trips_state_history_and_messages_for_failed_qso() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let state_history = sample_state_history(now);
        let messages = sample_messages(now);

        db.insert_qso_timeline(
            qso_id,
            Some("K1DEF"),
            "failed",
            Some("Timeout"),
            &state_history,
            &messages,
        )
        .await
        .unwrap();

        let reloaded = db
            .get_qso_timeline(qso_id)
            .await
            .unwrap()
            .expect("timeline must be persisted and reloadable");

        assert_eq!(reloaded.qso_id, qso_id);
        assert_eq!(reloaded.callsign.as_deref(), Some("K1DEF"));
        assert_eq!(reloaded.outcome, "failed");
        assert_eq!(reloaded.reason.as_deref(), Some("Timeout"));
        // The exact round-trip assertion the brief asks for: what comes back
        // out must equal what went in, transition-for-transition and
        // message-for-message — not just "some non-empty vec".
        assert_eq!(reloaded.state_history, state_history);
        assert_eq!(reloaded.messages, messages);
    }

    #[tokio::test]
    async fn qso_timeline_round_trips_for_completed_qso() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let qso_id = Uuid::new_v4();
        let now = Utc::now();
        let state_history = sample_state_history(now);
        let messages = sample_messages(now);

        db.insert_qso_timeline(
            qso_id,
            Some("K1DEF"),
            "completed",
            None,
            &state_history,
            &messages,
        )
        .await
        .unwrap();

        let reloaded = db.get_qso_timeline(qso_id).await.unwrap().unwrap();
        assert_eq!(reloaded.outcome, "completed");
        assert_eq!(reloaded.reason, None);
        assert_eq!(reloaded.state_history, state_history);
        assert_eq!(reloaded.messages, messages);
    }

    #[tokio::test]
    async fn qso_timeline_missing_qso_id_returns_none() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        assert!(db.get_qso_timeline(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn qso_timeline_returns_latest_row_when_qso_id_recurs() {
        // A superseded-then-cleaned-up QSO can legitimately produce more
        // than one row for the same qso_id; the most recent must win.
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let qso_id = Uuid::new_v4();
        let now = Utc::now();

        db.insert_qso_timeline(
            qso_id,
            Some("K1DEF"),
            "failed",
            Some("Superseded"),
            &[],
            &[],
        )
        .await
        .unwrap();
        let second_history = sample_state_history(now);
        db.insert_qso_timeline(
            qso_id,
            Some("K1DEF"),
            "completed",
            None,
            &second_history,
            &[],
        )
        .await
        .unwrap();

        let reloaded = db.get_qso_timeline(qso_id).await.unwrap().unwrap();
        assert_eq!(reloaded.outcome, "completed");
        assert_eq!(reloaded.state_history, second_history);
    }
}

#[cfg(test)]
mod pan282_tests {
    use super::*;
    fn log(calls: &[&str]) -> String {
        let mut text = String::from("<EOH>\n");
        for call in calls {
            text += &format!("<CALL:{}>{call} <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20M <EOR>\n",call.len());
        }
        text
    }
    async fn snapshot(dir: &Path, calls: &[&str]) -> AdifSnapshot {
        let path = dir.join("source.adi");
        std::fs::write(&path, log(calls)).unwrap();
        AdifSnapshot::load(path).await.unwrap()
    }
    async fn seed(path: &Path, calls: &[&str]) -> QsoDatabase {
        let dir = tempfile::tempdir().unwrap();
        QsoDatabase::rebuild_from(
            path,
            snapshot(dir.path(), calls).await,
            PreviousIndex::Replace,
        )
        .await
        .unwrap()
        .db
    }
    fn copy_files(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }
    async fn assert_index(path: &Path, count: u64) {
        let db = QsoDatabase::open(path).await.unwrap();
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        assert_eq!(db.count_qsos().await.unwrap(), count);
        db.close().await;
    }
    #[tokio::test]
    async fn rebuild_leaves_the_old_or_the_complete_new_index_at_every_step() {
        let dir = tempfile::tempdir().unwrap();
        let snaps = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        seed(&path, &["OLD1", "OLD2", "OLD3"]).await.close().await;
        let calls: Vec<_> = (0..50).map(|i| format!("NEW{i}")).collect();
        let refs: Vec<_> = calls.iter().map(String::as_str).collect();
        let mut steps = Vec::new();
        let rebuilt = QsoDatabase::rebuild_with(
            &path,
            snapshot(dir.path(), &refs).await,
            PreviousIndex::Replace,
            &mut |step| {
                let target = snaps.path().join(format!("{step:?}"));
                copy_files(dir.path(), &target);
                steps.push((step, target));
            },
        )
        .await
        .unwrap();
        rebuilt.db.close().await;
        for (step, snap) in steps {
            let count = if matches!(step, RebuildStep::Swapped | RebuildStep::Done) {
                50
            } else {
                3
            };
            assert_index(&snap.join("qso.db"), count).await;
        }
        assert!(!QsoDatabase::rebuild_temp_path(&path).exists());
    }
    #[tokio::test]
    async fn rebuild_over_an_index_with_a_stale_wal_keeps_the_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let killed = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        let db = seed(&path, &["OLD1"]).await;
        db.insert_converted(snapshot(dir.path(), &["OLD2"]).await.records)
            .await
            .unwrap();
        copy_files(dir.path(), killed.path());
        db.close().await;
        let calls: Vec<_> = (0..500).map(|i| format!("NEW{i}")).collect();
        let refs: Vec<_> = calls.iter().map(String::as_str).collect();
        let path = killed.path().join("qso.db");
        let new = QsoDatabase::rebuild_from(
            &path,
            snapshot(dir.path(), &refs).await,
            PreviousIndex::Replace,
        )
        .await
        .unwrap();
        assert_eq!(new.db.count_qsos().await.unwrap(), 500);
        assert!(!new
            .db
            .get_worked_callsigns("20M")
            .await
            .iter()
            .any(|c| c.starts_with("OLD")));
        new.db.close().await;
        assert_index(&path, 500).await;
    }
    #[tokio::test]
    async fn rebuild_keeps_a_copy_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        seed(&path, &["OLD1"]).await.close().await;
        for n in 0..2 {
            let result = QsoDatabase::rebuild_from(
                &path,
                snapshot(dir.path(), &["NEW1", "NEW2"]).await,
                PreviousIndex::KeepCopy,
            )
            .await
            .unwrap();
            let expected = suffixed(
                &path,
                if n == 0 {
                    ".pre-rebuild"
                } else {
                    ".pre-rebuild.1"
                },
            );
            assert_eq!(result.kept, Some(expected.clone()));
            result.db.close().await;
            assert_index(&expected, if n == 0 { 1 } else { 2 }).await;
        }
    }
    #[tokio::test]
    async fn rebuild_keeps_an_unreadable_index_and_swaps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        std::fs::write(&path, b"this is not a database").unwrap();
        std::fs::write(suffixed(&path, "-wal"), [42; 64]).unwrap();
        let result = QsoDatabase::rebuild_from(
            &path,
            snapshot(dir.path(), &["NEW1"]).await,
            PreviousIndex::Replace,
        )
        .await
        .unwrap();
        result.db.close().await;
        assert_eq!(
            std::fs::read(suffixed(&path, ".pre-rebuild")).unwrap(),
            b"this is not a database"
        );
        assert_eq!(
            std::fs::read(suffixed(&path, ".pre-rebuild-wal")).unwrap(),
            [42; 64]
        );
        assert_index(&path, 1).await;
    }
    #[tokio::test]
    async fn rebuild_clears_leftovers_of_a_crashed_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        for suffix in [".new", ".new-wal", ".new-shm", ".new-journal"] {
            std::fs::write(suffixed(&path, suffix), b"junk").unwrap();
        }
        let db = seed(&path, &["NEW1"]).await;
        db.close().await;
        assert_index(&path, 1).await;
        for suffix in [".new", ".new-wal", ".new-shm", ".new-journal"] {
            assert!(!suffixed(&path, suffix).exists());
        }
    }
    #[test]
    fn checkpoint_allows_swap_only_when_complete() {
        for (busy, log, done, expected) in [
            (0, 0, 0, true),
            (0, -1, -1, true),
            (1, 5, 3, false),
            (0, 5, 3, false),
        ] {
            assert_eq!(checkpoint_allows_swap(busy, log, done), expected);
        }
    }
    #[tokio::test]
    async fn drift_counts_both_directions_as_a_multiset() {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(Path::new(":memory:"), &["A", "C"]).await;
        assert_eq!(
            db.drift_from(&snapshot(dir.path(), &["A", "A", "B"]).await)
                .await
                .unwrap(),
            IndexDrift {
                missing_from_index: 2,
                missing_from_adif: 1
            }
        );
    }
    #[tokio::test]
    async fn drift_ignores_sub_second_start_times_and_callsign_case() {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(Path::new(":memory:"), &[]).await;
        let mut records = snapshot(dir.path(), &["w1aw"]).await.records;
        records[0].start_time += chrono::Duration::milliseconds(611);
        db.insert_converted(records).await.unwrap();
        assert!(db
            .drift_from(&snapshot(dir.path(), &["W1AW"]).await)
            .await
            .unwrap()
            .is_current());
    }
    #[tokio::test]
    async fn drift_skips_records_and_rows_without_a_callsign() {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(Path::new(":memory:"), &["A"]).await;
        sqlx::query("UPDATE qsos SET metadata = json_set(metadata, '$.their_callsign', '')")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(db
            .drift_from(&snapshot(dir.path(), &[""]).await)
            .await
            .unwrap()
            .is_current());
    }
    #[tokio::test]
    async fn drift_counts_an_unreadable_row_as_missing_from_the_adif() {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(Path::new(":memory:"), &["A"]).await;
        sqlx::query("UPDATE qsos SET metadata = '{not json'")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            db.drift_from(&snapshot(dir.path(), &[]).await)
                .await
                .unwrap()
                .missing_from_adif,
            1
        );
    }
    #[tokio::test]
    async fn migration_refuses_an_unreadable_row_and_keeps_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        let adif = dir.path().join("qsos.adi");
        let db = seed(&path, &["A", "B"]).await;
        let id: String = sqlx::query_scalar("SELECT qso_id FROM qsos WHERE id=1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE qsos SET progress_data = '{not json' WHERE id=1")
            .execute(&db.pool)
            .await
            .unwrap();
        db.close().await;
        let bytes = std::fs::read(&path).unwrap();
        let err = match QsoDatabase::migrate_legacy_to_adif(&path, &adif).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("migration accepted unreadable row"),
        };
        assert!(
            err.contains("row 1") && err.contains(&id) && err.contains("qso.db.pre-migration"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(suffixed(&path, ".pre-migration")).unwrap(),
            bytes
        );
        assert!(!adif.exists());
        assert!(!suffixed(&adif, ".migrating").exists());
    }
    #[tokio::test]
    async fn migration_copies_a_non_empty_wal_with_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let killed = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        let db = seed(&path, &["A"]).await;
        db.insert_converted(snapshot(dir.path(), &["B"]).await.records)
            .await
            .unwrap();
        copy_files(dir.path(), killed.path());
        db.close().await;
        let path = killed.path().join("qso.db");
        let (db, _) = QsoDatabase::migrate_legacy_to_adif(&path, killed.path().join("qsos.adi"))
            .await
            .unwrap();
        db.close().await;
        assert!(suffixed(&path, ".pre-migration-wal").exists());
        assert_index(&suffixed(&path, ".pre-migration"), 2).await;
    }
    #[tokio::test]
    async fn migration_keeps_an_existing_pre_migration_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qso.db");
        seed(&path, &["A"]).await.close().await;
        std::fs::write(suffixed(&path, ".pre-migration"), b"older copy").unwrap();
        let (db, result) = QsoDatabase::migrate_legacy_to_adif(&path, dir.path().join("qsos.adi"))
            .await
            .unwrap();
        assert!(result.backup_existed);
        db.close().await;
        assert_eq!(std::fs::read(result.backup).unwrap(), b"older copy");
    }
    #[tokio::test]
    async fn export_writes_the_whole_logbook_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qsos.adi");
        let db = seed(Path::new(":memory:"), &["A", "B"]).await;
        assert_eq!(db.export_to_adif(&path).await.unwrap(), 2);
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            matches!(db.export_to_adif(&path).await,Err(AsyncDatabaseError::Io{source,..}) if source.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!suffixed(&path, ".migrating").exists());
    }
}
