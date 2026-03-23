//! SQLite database integration for QSO storage and retrieval
//!
//! This module provides persistent storage for QSO data using SQLite,
//! including support for complex queries, statistics, and data integrity.

use crate::adif::{AdifProcessor, AdifQso};
use crate::states::*;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;
use tracing::{debug, error, info};
use uuid::Uuid;

/// Database operation errors
#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("SQLite error: {source}")]
    Sqlite { source: rusqlite::Error },

    #[error("Serialization error: {source}")]
    Serialization { source: serde_json::Error },

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

    #[error("Transaction failed: {message}")]
    Transaction { message: String },
}

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

/// QSO database implementation
pub struct QsoDatabase {
    /// Database connection
    connection: Connection,

    /// ADIF processor for conversions
    adif_processor: AdifProcessor,

    /// Database schema version
    schema_version: u32,
}

impl QsoDatabase {
    /// Create or open a QSO database
    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self, DatabaseError> {
        info!("Opening QSO database: {:?}", db_path.as_ref());

        let connection =
            Connection::open(db_path).map_err(|e| DatabaseError::Sqlite { source: e })?;

        // Enable foreign keys and WAL mode for better performance
        // Note: PRAGMA journal_mode returns a result row, so we must use
        // execute_batch or query_row instead of execute.
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let mut db = Self {
            connection,
            adif_processor: AdifProcessor::new(),
            schema_version: 0,
        };

        db.initialize_schema()?;

        Ok(db)
    }

    /// Create an in-memory database for testing
    pub fn new_in_memory() -> Result<Self, DatabaseError> {
        Self::open(":memory:")
    }

    /// Insert a new QSO record
    pub fn insert_qso(&mut self, progress: &QsoProgress) -> Result<i64, DatabaseError> {
        let tx = self
            .connection
            .transaction()
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let result = Self::insert_qso_tx(&tx, progress, &self.adif_processor);

        match result {
            Ok(id) => {
                tx.commit()
                    .map_err(|e| DatabaseError::Sqlite { source: e })?;
                Ok(id)
            }
            Err(e) => {
                tx.rollback()
                    .map_err(|e| DatabaseError::Sqlite { source: e })?;
                Err(e)
            }
        }
    }

    /// Update an existing QSO record
    pub fn update_qso(&mut self, progress: &QsoProgress) -> Result<(), DatabaseError> {
        let qso_id = progress.metadata.qso_id;

        let tx = self
            .connection
            .transaction()
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let result = Self::update_qso_tx(&tx, progress, &self.adif_processor);

        match result {
            Ok(_) => {
                tx.commit()
                    .map_err(|e| DatabaseError::Sqlite { source: e })?;
                debug!("Updated QSO: {}", qso_id);
                Ok(())
            }
            Err(e) => {
                tx.rollback()
                    .map_err(|e| DatabaseError::Sqlite { source: e })?;
                Err(e)
            }
        }
    }

    /// Get a QSO by ID
    pub fn get_qso(&self, qso_id: QsoId) -> Result<QsoDatabaseRecord, DatabaseError> {
        let mut stmt = self
            .connection
            .prepare(
                "SELECT id, qso_id, metadata, final_state, progress_data, adif_data, 
                    created_at, updated_at, checksum 
             FROM qsos WHERE qso_id = ?1",
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let record = stmt
            .query_row(params![qso_id.to_string()], |row| self.row_to_record(row))
            .optional()
            .map_err(|e| DatabaseError::Sqlite { source: e })?
            .ok_or(DatabaseError::QsoNotFound { qso_id })?;

        Ok(record)
    }

    /// Search QSOs with filters and options
    pub fn search_qsos(
        &self,
        filter: &QsoFilter,
        options: &QueryOptions,
    ) -> Result<Vec<QsoDatabaseRecord>, DatabaseError> {
        let (where_clause, params) = self.build_where_clause(filter)?;
        let order_clause = self.build_order_clause(options);
        let limit_clause = self.build_limit_clause(options);

        let sql = format!(
            "SELECT id, qso_id, metadata, final_state, progress_data, adif_data,
                    created_at, updated_at, checksum
             FROM qsos 
             {} {} {}",
            where_clause, order_clause, limit_clause
        );

        debug!("Executing query: {}", sql);

        let mut stmt = self
            .connection
            .prepare(&sql)
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                self.row_to_record(row)
            })
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let mut records = Vec::new();
        for row in rows {
            let record = row.map_err(|e| DatabaseError::Sqlite { source: e })?;
            records.push(record);
        }

        Ok(records)
    }

    /// Count QSOs matching filter
    pub fn count_qsos(&self, filter: &QsoFilter) -> Result<u64, DatabaseError> {
        let (where_clause, params) = self.build_where_clause(filter)?;

        let sql = format!("SELECT COUNT(*) FROM qsos {}", where_clause);

        let mut stmt = self
            .connection
            .prepare(&sql)
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let count: i64 = stmt
            .query_row(rusqlite::params_from_iter(params.iter()), |row| row.get(0))
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        Ok(count as u64)
    }

    /// Delete a QSO by ID
    pub fn delete_qso(&mut self, qso_id: QsoId) -> Result<bool, DatabaseError> {
        let rows_affected = self
            .connection
            .execute(
                "DELETE FROM qsos WHERE qso_id = ?1",
                params![qso_id.to_string()],
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        if rows_affected > 0 {
            info!("Deleted QSO: {}", qso_id);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get database statistics
    pub fn get_statistics(&self) -> Result<DatabaseStats, DatabaseError> {
        let total_qsos: u64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM qsos", [], |row| row.get::<_, i64>(0))
            .map_err(|e| DatabaseError::Sqlite { source: e })? as u64;

        let confirmed_qsos: u64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM qsos WHERE json_extract(metadata, '$.end_time') IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?
            as u64;

        let unique_callsigns: u64 =
            self.connection
                .query_row(
                    "SELECT COUNT(DISTINCT json_extract(metadata, '$.their_callsign')) FROM qsos 
             WHERE json_extract(metadata, '$.their_callsign') IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| DatabaseError::Sqlite { source: e })? as u64;

        let countries_worked: u64 =
            self.connection
                .query_row(
                    "SELECT COUNT(DISTINCT json_extract(adif_data, '$.country')) FROM qsos 
             WHERE json_extract(adif_data, '$.country') IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| DatabaseError::Sqlite { source: e })? as u64;

        let grid_squares_worked: u64 =
            self.connection
                .query_row(
                    "SELECT COUNT(DISTINCT json_extract(adif_data, '$.gridsquare')) FROM qsos 
             WHERE json_extract(adif_data, '$.gridsquare') IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|e| DatabaseError::Sqlite { source: e })? as u64;

        // Get QSOs by mode
        let mut qsos_by_mode = HashMap::new();
        let mut stmt = self
            .connection
            .prepare(
                "SELECT json_extract(metadata, '$.mode') as mode, COUNT(*) as count 
             FROM qsos GROUP BY mode",
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        for row in rows {
            let (mode, count) = row.map_err(|e| DatabaseError::Sqlite { source: e })?;
            qsos_by_mode.insert(mode, count);
        }

        // Get QSOs by band
        let mut qsos_by_band = HashMap::new();
        let mut stmt = self
            .connection
            .prepare(
                "SELECT json_extract(adif_data, '$.band') as band, COUNT(*) as count 
             FROM qsos GROUP BY band",
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        for row in rows {
            let (band, count) = row.map_err(|e| DatabaseError::Sqlite { source: e })?;
            qsos_by_band.insert(band, count);
        }

        // Get QSOs by year
        let mut qsos_by_year = HashMap::new();
        let mut stmt = self.connection.prepare(
            "SELECT strftime('%Y', json_extract(metadata, '$.start_time')) as year, COUNT(*) as count 
             FROM qsos GROUP BY year"
        ).map_err(|e| DatabaseError::Sqlite { source: e })?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?.parse::<u32>().unwrap_or(0),
                    row.get::<_, i64>(1)? as u64,
                ))
            })
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        for row in rows {
            let (year, count) = row.map_err(|e| DatabaseError::Sqlite { source: e })?;
            qsos_by_year.insert(year, count);
        }

        // Get first and last QSO dates
        let first_qso = self
            .connection
            .query_row(
                "SELECT MIN(json_extract(metadata, '$.start_time')) FROM qsos",
                [],
                |row| {
                    let date_str: Option<String> = row.get(0)?;
                    Ok(date_str.and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                    }))
                },
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        let last_qso = self
            .connection
            .query_row(
                "SELECT MAX(json_extract(metadata, '$.start_time')) FROM qsos",
                [],
                |row| {
                    let date_str: Option<String> = row.get(0)?;
                    Ok(date_str.and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                    }))
                },
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        // Get database size
        let database_size = self
            .connection
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })? as u64;

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
            database_size,
        })
    }

    /// Check for duplicate QSOs
    pub fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
        start_time: DateTime<Utc>,
        time_window_hours: u32,
    ) -> Result<Option<QsoId>, DatabaseError> {
        let time_threshold = start_time - chrono::Duration::hours(time_window_hours as i64);

        let duplicate_id: Option<String> = self
            .connection
            .query_row(
                "SELECT qso_id FROM qsos 
             WHERE json_extract(metadata, '$.their_callsign') = ?1
             AND ABS(json_extract(metadata, '$.frequency') - ?2) < 100.0
             AND datetime(json_extract(metadata, '$.start_time')) > datetime(?3)
             AND datetime(json_extract(metadata, '$.start_time')) < datetime(?4)
             LIMIT 1",
                params![
                    callsign,
                    frequency,
                    time_threshold.to_rfc3339(),
                    start_time.to_rfc3339()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        if let Some(id_str) = duplicate_id {
            if let Ok(qso_id) = Uuid::parse_str(&id_str) {
                return Ok(Some(qso_id));
            }
        }

        Ok(None)
    }

    /// Vacuum database to reclaim space
    pub fn vacuum(&mut self) -> Result<(), DatabaseError> {
        info!("Vacuuming database");
        self.connection
            .execute("VACUUM", [])
            .map_err(|e| DatabaseError::Sqlite { source: e })?;
        Ok(())
    }

    /// Backup database to file
    pub fn backup<P: AsRef<Path>>(&self, backup_path: P) -> Result<(), DatabaseError> {
        info!("Backing up database to: {:?}", backup_path.as_ref());

        let mut backup_conn =
            Connection::open(backup_path).map_err(|e| DatabaseError::Sqlite { source: e })?;

        let backup = rusqlite::backup::Backup::new(&self.connection, &mut backup_conn)
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        backup
            .run_to_completion(5, std::time::Duration::from_millis(250), None)
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        Ok(())
    }

    // Private helper methods

    fn initialize_schema(&mut self) -> Result<(), DatabaseError> {
        // Check current schema version
        self.schema_version = self
            .connection
            .query_row(
                "SELECT COALESCE((SELECT value FROM metadata WHERE key = 'schema_version'), '0')",
                [],
                |row| {
                    row.get::<_, String>(0)?.parse::<u32>().map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            0,
                            "key".to_string(),
                            rusqlite::types::Type::Text,
                        )
                    })
                },
            )
            .unwrap_or(0);

        const CURRENT_SCHEMA_VERSION: u32 = 1;

        if self.schema_version == 0 {
            self.create_initial_schema()?;
            self.schema_version = CURRENT_SCHEMA_VERSION;
        } else if self.schema_version < CURRENT_SCHEMA_VERSION {
            self.migrate_schema(CURRENT_SCHEMA_VERSION)?;
        }

        Ok(())
    }

    fn create_initial_schema(&mut self) -> Result<(), DatabaseError> {
        info!("Creating initial database schema");

        let sql = r#"
            -- Metadata table for schema versioning
            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            
            -- Main QSO table
            CREATE TABLE IF NOT EXISTS qsos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                qso_id TEXT UNIQUE NOT NULL,
                metadata TEXT NOT NULL,  -- JSON
                final_state TEXT NOT NULL,  -- JSON
                progress_data TEXT,  -- JSON (optional)
                adif_data TEXT NOT NULL,  -- JSON
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                checksum TEXT NOT NULL
            );
            
            -- Indexes for performance
            CREATE INDEX IF NOT EXISTS idx_qso_id ON qsos(qso_id);
            CREATE INDEX IF NOT EXISTS idx_callsign ON qsos(json_extract(metadata, '$.their_callsign'));
            CREATE INDEX IF NOT EXISTS idx_start_time ON qsos(json_extract(metadata, '$.start_time'));
            CREATE INDEX IF NOT EXISTS idx_frequency ON qsos(json_extract(metadata, '$.frequency'));
            CREATE INDEX IF NOT EXISTS idx_mode ON qsos(json_extract(metadata, '$.mode'));
            CREATE INDEX IF NOT EXISTS idx_band ON qsos(json_extract(adif_data, '$.band'));
            CREATE INDEX IF NOT EXISTS idx_created_at ON qsos(created_at);
            
            -- Insert schema version
            INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '1');
        "#;

        self.connection
            .execute_batch(sql)
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        self.connection
            .execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('created_at', ?1)",
                params![Utc::now().to_rfc3339()],
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        Ok(())
    }

    fn migrate_schema(&mut self, target_version: u32) -> Result<(), DatabaseError> {
        info!(
            "Migrating database schema from version {} to {}",
            self.schema_version, target_version
        );

        // Future migrations would go here
        match self.schema_version {
            0 => self.create_initial_schema()?,
            _ => {
                return Err(DatabaseError::MigrationFailed {
                    version: target_version,
                })
            }
        }

        // Update schema version
        self.connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = 'schema_version'",
                params![target_version.to_string()],
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        self.schema_version = target_version;

        Ok(())
    }

    fn insert_qso_tx(
        tx: &Transaction,
        progress: &QsoProgress,
        adif_processor: &AdifProcessor,
    ) -> Result<i64, DatabaseError> {
        let qso_id = progress.metadata.qso_id;

        // Check for duplicate
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM qsos WHERE qso_id = ?1",
                params![qso_id.to_string()],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| DatabaseError::Sqlite { source: e })?
            .unwrap_or(false);

        if exists {
            return Err(DatabaseError::DuplicateQso { qso_id });
        }

        let metadata_json = serde_json::to_string(&progress.metadata)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let state_json = serde_json::to_string(&progress.state)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let progress_json = serde_json::to_string(progress)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let adif_qso =
            adif_processor.qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
        let adif_json = serde_json::to_string(&adif_qso)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let now = Utc::now().to_rfc3339();
        let checksum = Self::calculate_checksum(&metadata_json, &state_json, &adif_json);

        let id = tx
            .query_row(
                "INSERT INTO qsos (qso_id, metadata, final_state, progress_data, adif_data, 
                              created_at, updated_at, checksum)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             RETURNING id",
                params![
                    qso_id.to_string(),
                    metadata_json,
                    state_json,
                    progress_json,
                    adif_json,
                    now,
                    now,
                    checksum
                ],
                |row| row.get(0),
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        debug!("Inserted QSO: {} (id: {})", qso_id, id);

        Ok(id)
    }

    fn update_qso_tx(
        tx: &Transaction,
        progress: &QsoProgress,
        adif_processor: &AdifProcessor,
    ) -> Result<(), DatabaseError> {
        let qso_id = progress.metadata.qso_id;

        let metadata_json = serde_json::to_string(&progress.metadata)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let state_json = serde_json::to_string(&progress.state)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let progress_json = serde_json::to_string(progress)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let adif_qso =
            adif_processor.qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
        let adif_json = serde_json::to_string(&adif_qso)
            .map_err(|e| DatabaseError::Serialization { source: e })?;

        let now = Utc::now().to_rfc3339();
        let checksum = Self::calculate_checksum(&metadata_json, &state_json, &adif_json);

        let rows_affected = tx
            .execute(
                "UPDATE qsos SET metadata = ?1, final_state = ?2, progress_data = ?3, 
                            adif_data = ?4, updated_at = ?5, checksum = ?6
             WHERE qso_id = ?7",
                params![
                    metadata_json,
                    state_json,
                    progress_json,
                    adif_json,
                    now,
                    checksum,
                    qso_id.to_string()
                ],
            )
            .map_err(|e| DatabaseError::Sqlite { source: e })?;

        if rows_affected == 0 {
            return Err(DatabaseError::QsoNotFound { qso_id });
        }

        Ok(())
    }

    fn row_to_record(&self, row: &Row) -> rusqlite::Result<QsoDatabaseRecord> {
        let metadata_json: String = row.get("metadata")?;
        let state_json: String = row.get("final_state")?;
        let adif_json: String = row.get("adif_data")?;
        let created_at_str: String = row.get("created_at")?;
        let updated_at_str: String = row.get("updated_at")?;

        let metadata: QsoMetadata = serde_json::from_str(&metadata_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let final_state: QsoState = serde_json::from_str(&state_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let adif_data: AdifQso = serde_json::from_str(&adif_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .with_timezone(&Utc);

        let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .with_timezone(&Utc);

        Ok(QsoDatabaseRecord {
            id: row.get("id")?,
            qso_id: metadata.qso_id,
            metadata,
            final_state,
            progress_data: row.get("progress_data")?,
            adif_data,
            created_at,
            updated_at,
            checksum: row.get("checksum")?,
        })
    }

    fn build_where_clause(
        &self,
        filter: &QsoFilter,
    ) -> Result<(String, Vec<rusqlite::types::Value>), DatabaseError> {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        if let Some(ref pattern) = filter.callsign_pattern {
            conditions.push("json_extract(metadata, '$.their_callsign') LIKE ?".to_string());
            params.push(rusqlite::types::Value::Text(format!("%{}%", pattern)));
        }

        if let Some(ref range) = filter.date_range {
            conditions.push("datetime(json_extract(metadata, '$.start_time')) BETWEEN datetime(?) AND datetime(?)".to_string());
            params.push(rusqlite::types::Value::Text(range.start.to_rfc3339()));
            params.push(rusqlite::types::Value::Text(range.end.to_rfc3339()));
        }

        if let Some(ref range) = filter.frequency_range {
            conditions.push("json_extract(metadata, '$.frequency') BETWEEN ? AND ?".to_string());
            params.push(rusqlite::types::Value::Real(range.min));
            params.push(rusqlite::types::Value::Real(range.max));
        }

        if let Some(ref band) = filter.band {
            conditions.push("json_extract(adif_data, '$.band') = ?".to_string());
            params.push(rusqlite::types::Value::Text(band.clone()));
        }

        if let Some(ref mode) = filter.mode {
            conditions.push("json_extract(metadata, '$.mode') = ?".to_string());
            params.push(rusqlite::types::Value::Text(mode.clone()));
        }

        if let Some(ref pattern) = filter.grid_pattern {
            conditions.push("json_extract(adif_data, '$.gridsquare') LIKE ?".to_string());
            params.push(rusqlite::types::Value::Text(format!("{}%", pattern)));
        }

        if let Some(ref contest) = filter.contest_id {
            conditions.push("json_extract(adif_data, '$.contest_id') = ?".to_string());
            params.push(rusqlite::types::Value::Text(contest.clone()));
        }

        if let Some(confirmed) = filter.confirmed {
            if confirmed {
                conditions.push("json_extract(metadata, '$.end_time') IS NOT NULL".to_string());
            } else {
                conditions.push("json_extract(metadata, '$.end_time') IS NULL".to_string());
            }
        }

        if let Some(min_signal) = filter.min_signal_strength {
            conditions.push(
                "CAST(json_extract(metadata, '$.reports.received') AS INTEGER) >= ?".to_string(),
            );
            params.push(rusqlite::types::Value::Integer(min_signal as i64));
        }

        if let Some(ref custom) = filter.custom_where {
            conditions.push(custom.clone());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        Ok((where_clause, params))
    }

    fn build_order_clause(&self, options: &QueryOptions) -> String {
        if let Some(ref sort_field) = options.sort_by {
            let field_expr = match sort_field {
                SortField::QsoDate => "json_extract(metadata, '$.start_time')",
                SortField::Callsign => "json_extract(metadata, '$.their_callsign')",
                SortField::Frequency => "json_extract(metadata, '$.frequency')",
                SortField::Mode => "json_extract(metadata, '$.mode')",
                SortField::Band => "json_extract(adif_data, '$.band')",
                SortField::SignalReport => "json_extract(metadata, '$.reports.received')",
                SortField::CreatedAt => "created_at",
            };

            let direction = match options.sort_order {
                SortOrder::Ascending => "ASC",
                SortOrder::Descending => "DESC",
            };

            format!("ORDER BY {} {}", field_expr, direction)
        } else {
            "ORDER BY created_at DESC".to_string()
        }
    }

    fn build_limit_clause(&self, options: &QueryOptions) -> String {
        let mut clause = String::new();

        if let Some(limit) = options.limit {
            clause.push_str(&format!("LIMIT {}", limit));

            if let Some(offset) = options.offset {
                clause.push_str(&format!(" OFFSET {}", offset));
            }
        }

        clause
    }

    fn calculate_checksum(metadata: &str, state: &str, adif: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        metadata.hash(&mut hasher);
        state.hash(&mut hasher);
        adif.hash(&mut hasher);

        format!("{:x}", hasher.finish())
    }
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::states::*;

    fn create_test_qso() -> QsoProgress {
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

        let state = QsoState::Completed {
            their_callsign: "K1DEF".to_string(),
            their_report: -12,
            our_report: -15,
            frequency: 14074000.0,
            grid_square: Some("FN31".to_string()),
            completed_at: now,
            duration_seconds: 120,
        };

        QsoProgress {
            state,
            state_history: vec![],
            messages: vec![],
            metadata,
        }
    }

    #[test]
    fn test_database_operations() {
        let mut db = QsoDatabase::new_in_memory().unwrap();
        let qso = create_test_qso();

        // Test insert
        let id = db.insert_qso(&qso).unwrap();
        assert!(id > 0);

        // Test get
        let retrieved = db.get_qso(qso.metadata.qso_id).unwrap();
        assert_eq!(retrieved.qso_id, qso.metadata.qso_id);
        assert_eq!(retrieved.metadata.their_callsign, Some("K1DEF".to_string()));

        // Test search
        let filter = QsoFilter {
            callsign_pattern: Some("K1DEF".to_string()),
            ..Default::default()
        };
        let results = db.search_qsos(&filter, &QueryOptions::default()).unwrap();
        assert_eq!(results.len(), 1);

        // Test count
        let count = db.count_qsos(&QsoFilter::default()).unwrap();
        assert_eq!(count, 1);

        // Test statistics
        let stats = db.get_statistics().unwrap();
        assert_eq!(stats.total_qsos, 1);
        assert_eq!(stats.confirmed_qsos, 1);
        assert_eq!(stats.unique_callsigns, 1);
    }

    #[test]
    fn test_duplicate_detection() {
        let mut db = QsoDatabase::new_in_memory().unwrap();
        let qso = create_test_qso();

        // Insert first QSO
        db.insert_qso(&qso).unwrap();

        // Check for duplicate
        let duplicate = db
            .check_duplicate(
                "K1DEF",
                14074000.0,
                qso.metadata.start_time + chrono::Duration::minutes(30),
                24,
            )
            .unwrap();

        assert_eq!(duplicate, Some(qso.metadata.qso_id));
    }

    #[test]
    fn test_filter_queries() {
        let mut db = QsoDatabase::new_in_memory().unwrap();

        // Insert test QSOs
        let mut qso1 = create_test_qso();
        qso1.metadata.qso_id = Uuid::new_v4();
        qso1.metadata.their_callsign = Some("VE3XYZ".to_string());
        qso1.metadata.frequency = 7074000.0; // 40m

        let mut qso2 = create_test_qso();
        qso2.metadata.qso_id = Uuid::new_v4();
        qso2.metadata.their_callsign = Some("G0ABC".to_string());
        qso2.metadata.frequency = 14074000.0; // 20m

        db.insert_qso(&qso1).unwrap();
        db.insert_qso(&qso2).unwrap();

        // Test frequency range filter
        let filter = QsoFilter {
            frequency_range: Some(FrequencyRange {
                min: 7000000.0,
                max: 8000000.0,
            }),
            ..Default::default()
        };

        let results = db.search_qsos(&filter, &QueryOptions::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].metadata.their_callsign,
            Some("VE3XYZ".to_string())
        );
    }
}
