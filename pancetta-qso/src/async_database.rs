//! Async-safe SQLite database integration using sqlx
//!
//! This module provides async-safe persistent storage for QSO data using SQLite
//! through the sqlx library, enabling proper Send/Sync support for tokio spawns.

use crate::adif::AdifProcessor;
use crate::states::*;
use chrono::{DateTime, Utc};
use sqlx::{
    sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow},
    Row,
};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;
use tracing::{debug, info};
use uuid::Uuid;

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
}

/// Async QSO database using sqlx
#[derive(Clone)]
pub struct AsyncQsoDatabase {
    /// Database connection pool
    pool: SqlitePool,

    /// ADIF processor for conversions
    adif_processor: AdifProcessor,

    /// Database schema version
    schema_version: u32,
}

impl AsyncQsoDatabase {
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
        filter: &crate::database::QsoFilter,
        options: &crate::database::QueryOptions,
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

        // Add limit
        if let Some(limit) = options.limit {
            query.push_str(&format!(" LIMIT {}", limit));
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
        let pool = self.pool.clone();

        // Use VACUUM INTO which atomically creates a complete copy of the database.
        // This runs in a blocking task since it may take time for large databases.
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                sqlx::query(&format!("VACUUM INTO '{}'", backup_path_str))
                    .execute(&pool)
                    .await
            })
        })
        .await
        .map_err(|e| AsyncDatabaseError::Sqlx(sqlx::Error::Protocol(e.to_string())))?
        .map_err(AsyncDatabaseError::Sqlx)?;

        info!("Database backup completed (VACUUM INTO)");
        Ok(())
    }

    /// Get database statistics
    pub async fn get_statistics(
        &self,
    ) -> Result<crate::database::DatabaseStats, AsyncDatabaseError> {
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

        Ok(crate::database::DatabaseStats {
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

    /// Check for duplicate QSOs
    pub async fn check_duplicate(
        &self,
        callsign: &str,
        frequency: f64,
        start_time: DateTime<Utc>,
        time_window_hours: u32,
    ) -> Result<Option<QsoId>, AsyncDatabaseError> {
        let time_threshold = start_time - chrono::Duration::hours(time_window_hours as i64);

        let duplicate_id: Option<String> = sqlx::query_scalar(
            "SELECT qso_id FROM qsos 
             WHERE json_extract(metadata, '$.their_callsign') = ?
             AND ABS(json_extract(metadata, '$.frequency') - ?) < 100.0
             AND datetime(json_extract(metadata, '$.start_time')) > datetime(?)
             AND datetime(json_extract(metadata, '$.start_time')) < datetime(?)
             LIMIT 1",
        )
        .bind(callsign)
        .bind(frequency)
        .bind(time_threshold.to_rfc3339())
        .bind(start_time.to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;

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
        filter: &crate::database::QsoFilter,
        options: &crate::database::QueryOptions,
    ) -> Result<Vec<crate::database::QsoDatabaseRecord>, AsyncDatabaseError> {
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

        // Add limit
        if let Some(limit) = options.limit {
            query.push_str(&format!(" LIMIT {}", limit));
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
                records.push(crate::database::QsoDatabaseRecord {
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
}

// AsyncQsoDatabase is automatically Send + Sync thanks to SqlitePool

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_async_database_creation() {
        let db = AsyncQsoDatabase::new_in_memory().await;
        assert!(db.is_ok());
    }

    #[tokio::test]
    async fn test_insert_and_get_qso() {
        let db = AsyncQsoDatabase::new_in_memory().await.unwrap();

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
            },
        };

        // Insert QSO
        let id = db.insert_qso(&progress).await.unwrap();
        assert!(id > 0);

        // Get QSO back
        let retrieved = db.get_qso(progress.metadata.qso_id).await.unwrap();
        assert_eq!(retrieved.metadata.qso_id, progress.metadata.qso_id);
    }

    #[tokio::test]
    async fn test_update_qso() {
        let db = AsyncQsoDatabase::new_in_memory().await.unwrap();

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
}
