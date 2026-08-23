//! Async-safe SQLite database integration using sqlx
//!
//! This module provides async-safe persistent storage for QSO data using SQLite
//! through the sqlx library, enabling proper Send/Sync support for tokio spawns.

use crate::adif::{AdifProcessor, AdifQso};
use crate::states::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow},
    Row,
};
use std::collections::HashMap;
use std::path::Path;
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

/// Async database operation errors
#[derive(Debug, Error)]
pub enum AsyncDatabaseError {
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

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
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

    /// Gracefully close every connection in the pool and wait for it to
    /// finish, rather than relying on `Drop` (which does not await the
    /// underlying workers' shutdown). Needed before deleting an on-disk
    /// database file out from under it — on Windows, a still-open file
    /// handle can turn that delete into a sharing-violation error instead
    /// of the safe no-op it is on Unix.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Initialize database schema
    async fn initialize_schema(&mut self) -> Result<(), AsyncDatabaseError> {
        // Enable WAL mode and relaxed synchronous for better concurrent performance.
        // WAL mode allows readers and writers to operate concurrently without blocking.
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&self.pool)
            .await?;

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
        .bind(&now)
        .bind(&now)
        .bind(&checksum)
        .execute(&self.pool)
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

        // VACUUM INTO has no bind-parameter form for its target, so the path is
        // interpolated into a single-quoted SQL string literal. Escape embedded
        // single quotes (`'` → `''`) so a path containing `'` cannot break out
        // of the literal and inject SQL.
        let escaped_path = escape_sqlite_string_literal(&backup_path_str);

        // Use VACUUM INTO which atomically creates a complete copy of the database.
        sqlx::query(&format!("VACUUM INTO '{}'", escaped_path))
            .execute(&self.pool)
            .await
            .map_err(AsyncDatabaseError::Sqlx)?;

        info!("Database backup completed (VACUUM INTO)");
        Ok(())
    }

    /// Get database statistics
    pub async fn get_statistics(&self) -> Result<DatabaseStats, AsyncDatabaseError> {
        let total_qsos: u64 = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM qsos")
            .fetch_one(&self.pool)
            .await? as u64;

        let confirmed_qsos: u64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM qsos WHERE json_extract(metadata, '$.confirmed') = 1",
        )
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

    /// Check for duplicate QSOs.
    ///
    /// `check_frequency` mirrors `QsoManager`'s in-memory check: when `true`,
    /// only a re-call within 50 Hz of the same RF frequency counts as a
    /// duplicate; when `false`, any re-call of the same callsign within the
    /// time window counts, regardless of frequency. Both paths must apply
    /// the same semantics — a QSO that ages out of the in-memory working set
    /// (e.g. after `cleanup_completed_qsos`) falls through to this DB-only
    /// check, and previously it ignored `check_frequency` entirely (always
    /// requiring proximity), silently reverting an operator's
    /// `check_frequency = false` setting once a QSO left memory.
    pub async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
        start_time: DateTime<Utc>,
        time_window_hours: u32,
        check_frequency: bool,
    ) -> Result<Option<QsoId>, AsyncDatabaseError> {
        let time_threshold = start_time - chrono::Duration::hours(time_window_hours as i64);

        let duplicate_id: Option<String> = if check_frequency {
            sqlx::query_scalar(
                "SELECT qso_id FROM qsos
                 WHERE json_extract(metadata, '$.their_callsign') = ?
                 AND ABS(json_extract(metadata, '$.frequency') - ?) < 50.0
                 AND datetime(json_extract(metadata, '$.start_time')) > datetime(?)
                 AND datetime(json_extract(metadata, '$.start_time')) < datetime(?)
                 LIMIT 1",
            )
            .bind(callsign)
            .bind(frequency)
            .bind(time_threshold.to_rfc3339())
            .bind(start_time.to_rfc3339())
            .fetch_optional(&self.pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT qso_id FROM qsos
                 WHERE json_extract(metadata, '$.their_callsign') = ?
                 AND datetime(json_extract(metadata, '$.start_time')) > datetime(?)
                 AND datetime(json_extract(metadata, '$.start_time')) < datetime(?)
                 LIMIT 1",
            )
            .bind(callsign)
            .bind(time_threshold.to_rfc3339())
            .bind(start_time.to_rfc3339())
            .fetch_optional(&self.pool)
            .await?
        };

        if let Some(id_str) = duplicate_id {
            if let Ok(qso_id) = Uuid::parse_str(&id_str) {
                return Ok(Some(qso_id));
            }
        }

        Ok(None)
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
    pub async fn get_worked_bands_and_grids(&self) -> Vec<(String, String)> {
        let result: Result<Vec<(String, String)>, sqlx::Error> = sqlx::query_as(
            "SELECT DISTINCT json_extract(adif_data, '$.band'), \
                             json_extract(metadata, '$.grids.theirs') \
             FROM qsos \
             WHERE json_extract(adif_data, '$.band') IS NOT NULL \
               AND json_extract(metadata, '$.grids.theirs') IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await;

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

    /// Build a fresh index at `db_path` by replaying every record in `adif_path`.
    ///
    /// If `db_path` exists, it is deleted first — caller should only invoke this
    /// when the DB is known to be stale or missing. `db_path` may also be the
    /// literal `":memory:"` sentinel (see [`Self::open`]) for a throwaway,
    /// on-disk-file-free rebuild; that case is never `try_exists`/`remove_file`d.
    /// Returns the new database handle, ready for queries.
    pub async fn replay_from_adif(
        db_path: impl AsRef<std::path::Path>,
        adif_path: impl AsRef<std::path::Path>,
    ) -> Result<Self, AsyncDatabaseError> {
        let db_path = db_path.as_ref();
        let adif_path = adif_path.as_ref();

        // `:memory:` is `Self::open`'s special in-memory sentinel, not a real
        // path -- never `try_exists`/`remove_file` it. On Unix that pair would
        // otherwise happily delete an unrelated regular file that literally
        // happens to be named `:memory:` in the process's current directory
        // (`open` below only recognizes the sentinel AFTER this check).
        let is_in_memory = db_path == std::path::Path::new(":memory:");

        // Drop any existing index so the rebuild is from scratch.
        if !is_in_memory && tokio::fs::try_exists(db_path).await.unwrap_or(false) {
            tokio::fs::remove_file(db_path)
                .await
                .map_err(|source| AsyncDatabaseError::Io {
                    path: db_path.to_path_buf(),
                    source,
                })?;
        }

        let db = Self::open(db_path).await?;

        let raw = tokio::fs::read_to_string(adif_path)
            .await
            .map_err(|source| AsyncDatabaseError::Io {
                path: adif_path.to_path_buf(),
                source,
            })?;

        let processor = crate::adif::AdifProcessor::new();
        let adif_file = processor
            .parse_string(&raw)
            .map_err(|e| AsyncDatabaseError::Replay(format!("ADIF parse failed: {e}")))?;

        let mut inserted: u64 = 0;
        let mut skipped: u64 = 0;
        for adif_record in &adif_file.records {
            let adif_qso = processor
                .record_to_qso(adif_record)
                .map_err(|e| AsyncDatabaseError::Replay(format!("record→AdifQso failed: {e}")))?;
            let metadata = processor.adif_to_qso(&adif_qso);

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

            db.insert_qso(&progress).await?;
            inserted += 1;
        }

        if skipped > 0 {
            warn!(
                "Skipped {} ADIF records with no CALL field during replay",
                skipped
            );
        }
        info!(
            "Replayed {} records from {} into {} ({} skipped)",
            inserted,
            adif_path.display(),
            db_path.display(),
            skipped,
        );
        Ok(db)
    }

    /// Export all QSOs in the index to an ADIF file at `path`.
    ///
    /// Iterates every row in `qsos`, converts via `qso_to_adif`, and writes a
    /// complete ADIF file. Intended for the DB→ADIF migration path at startup.
    pub async fn export_to_adif(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<(), AsyncDatabaseError> {
        use crate::adif::{AdifFile, AdifHeader};

        let path = path.as_ref();

        // Fetch all rows from the database as QsoProgress.
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT progress_data FROM qsos ORDER BY created_at ASC")
                .fetch_all(&self.pool)
                .await?;

        let processor = crate::adif::AdifProcessor::new();
        let mut records = Vec::with_capacity(rows.len());
        for (progress_json,) in &rows {
            if let Ok(progress) = serde_json::from_str::<QsoProgress>(progress_json) {
                let adif_qso = processor
                    .qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
                records.push(processor.qso_to_record(&adif_qso));
            }
        }

        let adif_file = AdifFile {
            header: AdifHeader::default(),
            records,
        };

        let content = processor
            .generate_string(&adif_file)
            .map_err(|e| AsyncDatabaseError::Replay(format!("ADIF generate failed: {e}")))?;

        tokio::fs::write(path, content)
            .await
            .map_err(|source| AsyncDatabaseError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        info!("Exported {} QSOs to {}", rows.len(), path.display());
        Ok(())
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
                mode: "FT8".to_string(),
                start_time: Utc::now(),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
                last_rx_text: None,
                dx_repeat_count: 0,
                hound: false,
                partner_freq: None,
                pending_freq_drift: None,
                hound_qsyed: false,
                remote_origin: false,
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
            state: QsoState::Idle,
            state_history: vec![],
            messages: vec![],
            metadata: QsoMetadata {
                qso_id: Uuid::new_v4(),
                our_callsign: "W1ABC".to_string(),
                their_callsign: Some(callsign.to_string()),
                frequency,
                mode: "FT8".to_string(),
                // Strictly in the past relative to a check_duplicate call
                // made moments after insert — the query's upper time bound
                // is `datetime(existing_start_time) < datetime(query_now)`,
                // and SQLite's `datetime()` truncates to whole-second
                // granularity, so `Utc::now()` for both would risk landing
                // in the same second and failing the strict `<`.
                start_time: Utc::now() - chrono::Duration::minutes(5),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
                last_rx_text: None,
                dx_repeat_count: 0,
                hound: false,
                partner_freq: None,
                pending_freq_drift: None,
                hound_qsyed: false,
                remote_origin: false,
                tx_parity_provisional: false,
            },
        }
    }

    // Regression tests for #137: the persistent-DB check_duplicate path used
    // to unconditionally require frequency proximity (100 Hz), ignoring
    // check_frequency entirely — so an operator who set check_frequency =
    // false to get strict one-QSO-per-callsign-per-window behavior would
    // silently get frequency-gated behavior back the moment the QSO aged out
    // of the in-memory working set. These three cases mirror QsoManager's
    // in-memory check_duplicate semantics exactly (50 Hz threshold, same
    // check_frequency branching).

    #[tokio::test]
    async fn check_duplicate_with_check_frequency_true_requires_proximity() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let progress = duplicate_check_test_progress("K2DEF", 14074000.0);
        db.insert_qso(&progress).await.unwrap();

        // Same callsign, 200 Hz away, check_frequency=true — not a duplicate.
        let far = db
            .check_duplicate("K2DEF", 14074200.0, Utc::now(), 24, true)
            .await
            .unwrap();
        assert!(
            far.is_none(),
            "200 Hz away must not count as a duplicate when check_frequency=true"
        );

        // Same callsign, 10 Hz away, check_frequency=true — a duplicate.
        let near = db
            .check_duplicate("K2DEF", 14074010.0, Utc::now(), 24, true)
            .await
            .unwrap();
        assert!(
            near.is_some(),
            "10 Hz away must count as a duplicate when check_frequency=true"
        );
    }

    #[tokio::test]
    async fn check_duplicate_with_check_frequency_false_ignores_frequency() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let progress = duplicate_check_test_progress("K2DEF", 14074000.0);
        db.insert_qso(&progress).await.unwrap();

        // Same callsign, 200 Hz away, check_frequency=false — still a
        // duplicate. This is the exact case #137 reported broken: the old
        // unconditional 100 Hz SQL filter would have returned None here.
        let far = db
            .check_duplicate("K2DEF", 14074200.0, Utc::now(), 24, false)
            .await
            .unwrap();
        assert!(
            far.is_some(),
            "200 Hz away must still count as a duplicate when check_frequency=false"
        );
    }

    #[tokio::test]
    async fn check_duplicate_matches_in_memory_50hz_threshold_not_100hz() {
        let db = QsoDatabase::new_in_memory().await.unwrap();
        let progress = duplicate_check_test_progress("K2DEF", 14074000.0);
        db.insert_qso(&progress).await.unwrap();

        // 75 Hz away: outside the in-memory path's 50 Hz threshold, so the
        // DB path must now agree (not a duplicate) rather than using the old
        // 100 Hz threshold (which would have wrongly said "duplicate").
        let mid = db
            .check_duplicate("K2DEF", 14074075.0, Utc::now(), 24, true)
            .await
            .unwrap();
        assert!(
            mid.is_none(),
            "75 Hz away must not count as a duplicate — must match the in-memory 50 Hz threshold, not the old 100 Hz one"
        );
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
                mode: "FT8".to_string(),
                start_time: Utc::now(),
                end_time: None,
                reports: SignalReports::default(),
                grids: GridSquares::default(),
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
                last_rx_text: None,
                dx_repeat_count: 0,
                hound: false,
                partner_freq: None,
                pending_freq_drift: None,
                hound_qsyed: false,
                remote_origin: false,
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
