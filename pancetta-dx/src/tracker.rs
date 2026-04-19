//! Worked/Confirmed Tracking
//!
//! This module manages tracking of worked and confirmed QSOs for various
//! amateur radio awards including DXCC, WAS, WAZ, etc.

use crate::{Band, ConfirmationStatus, DxError, DxQso, Mode, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info};
use uuid::Uuid;

/// Award tracking status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AwardStatus {
    /// Not worked on this band/mode
    NotWorked,
    /// Worked but not confirmed
    Worked,
    /// Confirmed (counts toward award)
    Confirmed,
}

/// Award tracking entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardEntry {
    /// DXCC entity code
    pub entity_code: u16,
    /// Band
    pub band: Band,
    /// Mode (None for mixed mode awards)
    pub mode: Option<Mode>,
    /// Current status
    pub status: AwardStatus,
    /// First worked date
    pub first_worked: Option<DateTime<Utc>>,
    /// First confirmed date
    pub first_confirmed: Option<DateTime<Utc>>,
    /// QSO ID that provided first work
    pub worked_qso_id: Option<Uuid>,
    /// QSO ID that provided first confirmation
    pub confirmed_qso_id: Option<Uuid>,
    /// Confirmation method
    pub confirmation_method: Option<ConfirmationStatus>,
}

/// Award summary statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwardSummary {
    /// Award name
    pub award_name: String,
    /// Band (None for mixed band)
    pub band: Option<Band>,
    /// Mode (None for mixed mode)
    pub mode: Option<Mode>,
    /// Total entities worked
    pub worked_count: u32,
    /// Total entities confirmed
    pub confirmed_count: u32,
    /// Total possible entities
    pub total_entities: u32,
    /// Percentage worked
    pub worked_percentage: f64,
    /// Percentage confirmed
    pub confirmed_percentage: f64,
    /// Entities still needed
    pub needed_entities: Vec<u16>,
}

/// QSO statistics by entity
#[derive(Debug, Clone)]
pub struct EntityStats {
    /// Total QSOs with this entity
    pub total_qsos: u32,
    /// QSOs by band
    pub band_breakdown: HashMap<Band, u32>,
    /// QSOs by mode
    pub mode_breakdown: HashMap<Mode, u32>,
    /// First QSO date
    pub first_qso: Option<DateTime<Utc>>,
    /// Last QSO date
    pub last_qso: Option<DateTime<Utc>>,
    /// Confirmed QSOs
    pub confirmed_qsos: u32,
}

/// DX Tracker database manager
pub struct DxTracker {
    pub(crate) connection: std::sync::Mutex<Connection>,
}

impl DxTracker {
    /// Create new DX tracker with database
    pub async fn new(database_path: &str) -> Result<Self> {
        let connection = Connection::open(database_path)?;

        let mut tracker = Self {
            connection: std::sync::Mutex::new(connection),
        };
        tracker.initialize_database().await?;

        Ok(tracker)
    }

    /// Initialize database schema
    async fn initialize_database(&mut self) -> Result<()> {
        info!("Initializing DX tracker database schema");

        // QSOs table
        self.connection.lock().unwrap().execute(
            "CREATE TABLE IF NOT EXISTS tracked_contacts (
                id TEXT PRIMARY KEY,
                callsign TEXT NOT NULL,
                datetime TEXT NOT NULL,
                frequency INTEGER NOT NULL,
                band TEXT NOT NULL,
                mode TEXT NOT NULL,
                rst_sent TEXT NOT NULL,
                rst_received TEXT NOT NULL,
                grid_square TEXT,
                qth TEXT,
                name TEXT,
                qsl_route TEXT,
                confirmation_status TEXT NOT NULL,
                confirmation_date TEXT,
                dxcc_entity INTEGER NOT NULL,
                contest_id TEXT,
                notes TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        // Award tracking table
        self.connection.lock().unwrap().execute(
            "CREATE TABLE IF NOT EXISTS award_tracking (
                entity_code INTEGER NOT NULL,
                band TEXT NOT NULL,
                mode TEXT,
                status TEXT NOT NULL,
                first_worked TEXT,
                first_confirmed TEXT,
                worked_qso_id TEXT,
                confirmed_qso_id TEXT,
                confirmation_method TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (entity_code, band, mode)
            )",
            [],
        )?;

        // Indexes for performance
        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_tracked_contacts_callsign ON tracked_contacts(callsign)",
            [],
        )?;

        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_tracked_contacts_dxcc_entity ON tracked_contacts(dxcc_entity)",
            [],
        )?;

        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_tracked_contacts_datetime ON tracked_contacts(datetime)",
            [],
        )?;

        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_tracked_contacts_band ON tracked_contacts(band)",
            [],
        )?;

        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_tracked_contacts_mode ON tracked_contacts(mode)",
            [],
        )?;

        self.connection.lock().unwrap().execute(
            "CREATE INDEX IF NOT EXISTS idx_award_entity_band ON award_tracking(entity_code, band)",
            [],
        )?;

        info!("Database schema initialized successfully");
        Ok(())
    }

    /// Add a new QSO
    pub async fn add_qso(&self, mut qso: DxQso) -> Result<Uuid> {
        if qso.id.is_none() {
            qso.id = Some(Uuid::new_v4());
        }

        let qso_id = qso.id.unwrap();

        debug!("Adding QSO: {} on {} {}", qso.callsign, qso.band, qso.mode);

        self.connection.lock().unwrap().execute(
            "INSERT INTO tracked_contacts (
                id, callsign, datetime, frequency, band, mode,
                rst_sent, rst_received, grid_square, qth, name,
                qsl_route, confirmation_status, confirmation_date,
                dxcc_entity, contest_id, notes
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                qso_id.to_string(),
                qso.callsign,
                qso.datetime.to_rfc3339(),
                qso.frequency as i64,
                qso.band.to_string(),
                qso.mode.to_string(),
                qso.rst_sent,
                qso.rst_received,
                qso.grid_square,
                qso.qth,
                qso.name,
                qso.qsl_route,
                serde_json::to_string(&qso.confirmation_status)?,
                qso.confirmation_date.map(|d| d.to_rfc3339()),
                qso.dxcc_entity as i64,
                qso.contest_id,
                qso.notes,
            ],
        )?;

        // Update award tracking
        self.update_award_tracking(&qso).await?;

        info!("Added QSO {} with entity {}", qso_id, qso.dxcc_entity);
        Ok(qso_id)
    }

    /// Update QSO record
    pub async fn update_qso(&self, qso: &DxQso) -> Result<()> {
        let Some(qso_id) = qso.id else {
            return Err(DxError::Configuration(
                "QSO ID is required for update".to_string(),
            ));
        };

        debug!("Updating QSO: {}", qso_id);

        let rows_affected = self.connection.lock().unwrap().execute(
            "UPDATE tracked_contacts SET
                callsign = ?1, datetime = ?2, frequency = ?3, band = ?4, mode = ?5,
                rst_sent = ?6, rst_received = ?7, grid_square = ?8, qth = ?9, name = ?10,
                qsl_route = ?11, confirmation_status = ?12, confirmation_date = ?13,
                dxcc_entity = ?14, contest_id = ?15, notes = ?16, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?17",
            params![
                qso.callsign,
                qso.datetime.to_rfc3339(),
                qso.frequency as i64,
                qso.band.to_string(),
                qso.mode.to_string(),
                qso.rst_sent,
                qso.rst_received,
                qso.grid_square,
                qso.qth,
                qso.name,
                qso.qsl_route,
                serde_json::to_string(&qso.confirmation_status)?,
                qso.confirmation_date.map(|d| d.to_rfc3339()),
                qso.dxcc_entity as i64,
                qso.contest_id,
                qso.notes,
                qso_id.to_string(),
            ],
        )?;

        if rows_affected == 0 {
            return Err(DxError::Configuration(format!("QSO {} not found", qso_id)));
        }

        // Update award tracking
        self.update_award_tracking(qso).await?;

        Ok(())
    }

    /// Update award tracking for a QSO
    async fn update_award_tracking(&self, qso: &DxQso) -> Result<()> {
        // Check current award status
        let current_entry = self
            .get_award_entry(qso.dxcc_entity, qso.band, Some(&qso.mode))
            .await?;

        let is_confirmed = matches!(
            qso.confirmation_status,
            ConfirmationStatus::QslCard
                | ConfirmationStatus::EQsl
                | ConfirmationStatus::Lotw
                | ConfirmationStatus::ClubLog
                | ConfirmationStatus::Qrz
                | ConfirmationStatus::Other(_)
        );

        let new_status = if is_confirmed {
            AwardStatus::Confirmed
        } else {
            AwardStatus::Worked
        };

        match current_entry {
            Some(entry) => {
                // Update existing entry if this is an improvement
                let should_update = match (&entry.status, &new_status) {
                    (AwardStatus::NotWorked, _) => true,
                    (AwardStatus::Worked, AwardStatus::Confirmed) => true,
                    (AwardStatus::Confirmed, AwardStatus::Confirmed) => {
                        // Update if this is an earlier confirmation
                        entry.first_confirmed.is_none()
                            || entry.first_confirmed > Some(qso.datetime)
                    }
                    _ => false,
                };

                if should_update {
                    self.update_award_entry(qso, &entry, new_status).await?;
                }
            }
            None => {
                // Create new entry
                self.create_award_entry(qso, new_status).await?;
            }
        }

        Ok(())
    }

    /// Create new award tracking entry
    async fn create_award_entry(&self, qso: &DxQso, status: AwardStatus) -> Result<()> {
        let is_confirmed = status == AwardStatus::Confirmed;

        self.connection.lock().unwrap().execute(
            "INSERT INTO award_tracking (
                entity_code, band, mode, status, first_worked, first_confirmed,
                worked_qso_id, confirmed_qso_id, confirmation_method
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                qso.dxcc_entity as i64,
                qso.band.to_string(),
                qso.mode.to_string(),
                serde_json::to_string(&status)?,
                qso.datetime.to_rfc3339(),
                if is_confirmed {
                    Some(qso.datetime.to_rfc3339())
                } else {
                    None
                },
                qso.id.map(|id| id.to_string()),
                if is_confirmed {
                    qso.id.map(|id| id.to_string())
                } else {
                    None
                },
                if is_confirmed {
                    Some(serde_json::to_string(&qso.confirmation_status)?)
                } else {
                    None
                },
            ],
        )?;

        debug!(
            "Created award entry for entity {} on {} {}",
            qso.dxcc_entity, qso.band, qso.mode
        );

        Ok(())
    }

    /// Update existing award tracking entry
    async fn update_award_entry(
        &self,
        qso: &DxQso,
        entry: &AwardEntry,
        new_status: AwardStatus,
    ) -> Result<()> {
        let is_confirmed = new_status == AwardStatus::Confirmed;

        self.connection.lock().unwrap().execute(
            "UPDATE award_tracking SET
                status = ?1,
                first_confirmed = ?2,
                confirmed_qso_id = ?3,
                confirmation_method = ?4,
                updated_at = CURRENT_TIMESTAMP
            WHERE entity_code = ?5 AND band = ?6 AND mode = ?7",
            params![
                serde_json::to_string(&new_status)?,
                if is_confirmed {
                    Some(qso.datetime.to_rfc3339())
                } else {
                    entry.first_confirmed.map(|d| d.to_rfc3339())
                },
                if is_confirmed {
                    qso.id.map(|id| id.to_string())
                } else {
                    entry.confirmed_qso_id.map(|id| id.to_string())
                },
                if is_confirmed {
                    Some(serde_json::to_string(&qso.confirmation_status)?)
                } else {
                    entry
                        .confirmation_method
                        .as_ref()
                        .map(|c| serde_json::to_string(c))
                        .transpose()?
                },
                qso.dxcc_entity as i64,
                qso.band.to_string(),
                qso.mode.to_string(),
            ],
        )?;

        debug!(
            "Updated award entry for entity {} on {} {}",
            qso.dxcc_entity, qso.band, qso.mode
        );

        Ok(())
    }

    /// Get award tracking entry
    pub async fn get_award_entry(
        &self,
        entity_code: u16,
        band: Band,
        mode: Option<&Mode>,
    ) -> Result<Option<AwardEntry>> {
        let mode_str = mode.map(|m| m.to_string());

        let conn = self.connection.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT entity_code, band, mode, status, first_worked, first_confirmed,
                    worked_qso_id, confirmed_qso_id, confirmation_method
             FROM award_tracking
             WHERE entity_code = ?1 AND band = ?2 AND mode = ?3",
                params![entity_code as i64, band.to_string(), mode_str],
                |row| self.award_entry_from_row(row),
            )
            .optional()?;

        Ok(row)
    }

    /// Convert database row to AwardEntry
    fn award_entry_from_row(&self, row: &Row) -> rusqlite::Result<AwardEntry> {
        let status_json: String = row.get("status")?;
        let status: AwardStatus = serde_json::from_str(&status_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0, // column index
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

        let first_worked = row
            .get::<_, Option<String>>("first_worked")?
            .map(|s| DateTime::parse_from_rfc3339(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .map(|dt| dt.with_timezone(&Utc));

        let first_confirmed = row
            .get::<_, Option<String>>("first_confirmed")?
            .map(|s| DateTime::parse_from_rfc3339(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .map(|dt| dt.with_timezone(&Utc));

        let worked_qso_id = row
            .get::<_, Option<String>>("worked_qso_id")?
            .map(|s| Uuid::parse_str(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

        let confirmed_qso_id = row
            .get::<_, Option<String>>("confirmed_qso_id")?
            .map(|s| Uuid::parse_str(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

        let confirmation_method = row
            .get::<_, Option<String>>("confirmation_method")?
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

        // Parse band and mode
        let band_str: String = row.get("band")?;
        let band = band_str.parse::<Band>().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                6, // column index
                rusqlite::types::Type::Text,
                Box::new(DxError::Parse(format!("Invalid band: {}", band_str))),
            )
        })?;

        let mode = row
            .get::<_, Option<String>>("mode")?
            .map(|s| s.parse::<Mode>())
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    7, // column index
                    rusqlite::types::Type::Text,
                    Box::new(DxError::Parse(e)),
                )
            })?;

        Ok(AwardEntry {
            entity_code: row.get::<_, i64>("entity_code")? as u16,
            band,
            mode,
            status,
            first_worked,
            first_confirmed,
            worked_qso_id,
            confirmed_qso_id,
            confirmation_method,
        })
    }

    /// Check if entity/band/mode combination is needed
    pub async fn is_needed(&self, _callsign: &str, _band: Band, _mode: &Mode) -> Result<bool> {
        // This would need DXCC lookup to get entity code from callsign
        // For now, return true as placeholder
        Ok(true)
    }

    /// Get QSO statistics by entity
    pub async fn get_qso_statistics_by_entity(&self) -> Result<HashMap<u16, u32>> {
        let conn = self.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT dxcc_entity, COUNT(*) as qso_count
             FROM tracked_contacts
             GROUP BY dxcc_entity",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>("dxcc_entity")? as u16,
                row.get::<_, i64>("qso_count")? as u32,
            ))
        })?;

        let mut stats = HashMap::new();
        for row in rows {
            let (entity_code, count) = row?;
            stats.insert(entity_code, count);
        }

        Ok(stats)
    }

    /// Get QSO statistics by entity since a date
    pub async fn get_qso_statistics_by_entity_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<HashMap<u16, u32>> {
        let conn = self.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT dxcc_entity, COUNT(*) as qso_count
             FROM tracked_contacts
             WHERE datetime >= ?1
             GROUP BY dxcc_entity",
        )?;

        let rows = stmt.query_map([since.to_rfc3339()], |row| {
            Ok((
                row.get::<_, i64>("dxcc_entity")? as u16,
                row.get::<_, i64>("qso_count")? as u32,
            ))
        })?;

        let mut stats = HashMap::new();
        for row in rows {
            let (entity_code, count) = row?;
            stats.insert(entity_code, count);
        }

        Ok(stats)
    }

    /// Get QSO statistics by band for an entity
    pub async fn get_qso_statistics_by_band(&self, entity_code: u16) -> Result<HashMap<Band, u32>> {
        let conn = self.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT band, COUNT(*) as qso_count
             FROM tracked_contacts
             WHERE dxcc_entity = ?1
             GROUP BY band",
        )?;

        let rows = stmt.query_map([entity_code as i64], |row| {
            let band_str: String = row.get("band")?;
            let band = band_str.parse::<Band>().map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    1, // column index
                    rusqlite::types::Type::Text,
                    Box::new(DxError::Parse(format!("Invalid band: {}", band_str))),
                )
            })?;

            Ok((band, row.get::<_, i64>("qso_count")? as u32))
        })?;

        let mut stats = HashMap::new();
        for row in rows {
            let (band, count) = row?;
            stats.insert(band, count);
        }

        Ok(stats)
    }

    /// Get QSO statistics by mode for an entity
    pub async fn get_qso_statistics_by_mode(&self, entity_code: u16) -> Result<HashMap<Mode, u32>> {
        let conn = self.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT mode, COUNT(*) as qso_count
             FROM tracked_contacts
             WHERE dxcc_entity = ?1
             GROUP BY mode",
        )?;

        let rows = stmt.query_map([entity_code as i64], |row| {
            let mode_str: String = row.get("mode")?;
            let mode: Mode = serde_json::from_str(&format!("\"{}\"", mode_str)).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2, // column index
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

            Ok((mode, row.get::<_, i64>("qso_count")? as u32))
        })?;

        let mut stats = HashMap::new();
        for row in rows {
            let (mode, count) = row?;
            stats.insert(mode, count);
        }

        Ok(stats)
    }

    /// Get last QSO date for an entity
    pub async fn get_last_qso_date(&self, entity_code: u16) -> Result<Option<DateTime<Utc>>> {
        let conn = self.connection.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT MAX(datetime) as last_qso
             FROM tracked_contacts
             WHERE dxcc_entity = ?1",
                [entity_code as i64],
                |row| {
                    let date_str: Option<String> = row.get("last_qso")?;
                    Ok(date_str)
                },
            )
            .optional()?;

        match result {
            Some(Some(date_str)) => {
                let parsed = DateTime::parse_from_rfc3339(&date_str)
                    .map_err(|e| DxError::Parse(format!("Invalid date format: {}", e)))?;
                Ok(Some(parsed.with_timezone(&Utc)))
            }
            _ => Ok(None),
        }
    }

    /// Get award summary for DXCC
    pub async fn get_dxcc_summary(
        &self,
        band: Option<Band>,
        mode: Option<Mode>,
    ) -> Result<AwardSummary> {
        let mut where_conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(band) = band {
            where_conditions.push("band = ?");
            params.push(Box::new(band.to_string()));
        }

        if let Some(mode) = mode {
            where_conditions.push("mode = ?");
            params.push(Box::new(mode.to_string()));
        }

        let where_clause = if where_conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_conditions.join(" AND "))
        };

        let query = format!(
            "SELECT 
                COUNT(CASE WHEN status = '\"Worked\"' OR status = '\"Confirmed\"' THEN 1 END) as worked_count,
                COUNT(CASE WHEN status = '\"Confirmed\"' THEN 1 END) as confirmed_count
             FROM award_tracking{}",
            where_clause
        );

        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let conn = self.connection.lock().unwrap();
        let (worked_count, confirmed_count) =
            conn.query_row(&query, param_refs.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>("worked_count")? as u32,
                    row.get::<_, i64>("confirmed_count")? as u32,
                ))
            })?;

        // For DXCC, there are currently 340 entities
        let total_entities = 340u32;

        let award_name = match (band, mode) {
            (Some(b), Some(m)) => format!("DXCC {} {}", b, m),
            (Some(b), None) => format!("DXCC {}", b),
            (None, Some(m)) => format!("DXCC {}", m),
            (None, None) => "DXCC Mixed".to_string(),
        };

        Ok(AwardSummary {
            award_name,
            band,
            mode,
            worked_count,
            confirmed_count,
            total_entities,
            worked_percentage: (worked_count as f64 / total_entities as f64) * 100.0,
            confirmed_percentage: (confirmed_count as f64 / total_entities as f64) * 100.0,
            needed_entities: Vec::new(), // Would be populated with actual needed entities
        })
    }
}

// FromStr for Band is now implemented in pancetta-core

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn create_test_tracker() -> (DxTracker, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let tracker = DxTracker::new(temp_file.path().to_str().unwrap())
            .await
            .unwrap();
        (tracker, temp_file)
    }

    #[tokio::test]
    async fn test_tracker_creation() {
        let (tracker, _temp_file) = create_test_tracker().await;
        let result: i64 = tracker
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, 1);
    }

    #[tokio::test]
    async fn test_add_qso() {
        let (mut tracker, _temp_file) = create_test_tracker().await;

        let qso = DxQso {
            id: None,
            callsign: "W1ABC".to_string(),
            datetime: Utc::now(),
            frequency: 14_200_000,
            band: Band::Band20m,
            mode: Mode::CW,
            rst_sent: "599".to_string(),
            rst_received: "599".to_string(),
            grid_square: Some("FN42".to_string()),
            qth: Some("Boston, MA".to_string()),
            name: Some("John".to_string()),
            qsl_route: None,
            confirmation_status: ConfirmationStatus::None,
            confirmation_date: None,
            dxcc_entity: 6, // United States
            contest_id: None,
            notes: None,
        };

        let qso_id = tracker.add_qso(qso).await.unwrap();
        assert!(qso_id != Uuid::nil());
    }

    #[tokio::test]
    async fn test_award_tracking() {
        let (mut tracker, _temp_file) = create_test_tracker().await;

        let qso = DxQso {
            id: None,
            callsign: "JA1ABC".to_string(),
            datetime: Utc::now(),
            frequency: 14_200_000,
            band: Band::Band20m,
            mode: Mode::CW,
            rst_sent: "599".to_string(),
            rst_received: "599".to_string(),
            grid_square: Some("PM95".to_string()),
            qth: Some("Tokyo".to_string()),
            name: Some("Taro".to_string()),
            qsl_route: None,
            confirmation_status: ConfirmationStatus::Lotw,
            confirmation_date: Some(Utc::now()),
            dxcc_entity: 61, // Japan
            contest_id: None,
            notes: None,
        };

        tracker.add_qso(qso).await.unwrap();

        let entry = tracker
            .get_award_entry(61, Band::Band20m, Some(&Mode::CW))
            .await
            .unwrap();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.status, AwardStatus::Confirmed);
        assert_eq!(entry.entity_code, 61);
    }

    #[tokio::test]
    async fn test_qso_statistics() {
        let (mut tracker, _temp_file) = create_test_tracker().await;

        // Add test QSOs
        for i in 0..5 {
            let qso = DxQso {
                id: None,
                callsign: format!("TEST{}", i),
                datetime: Utc::now(),
                frequency: 14_200_000,
                band: Band::Band20m,
                mode: Mode::CW,
                rst_sent: "599".to_string(),
                rst_received: "599".to_string(),
                grid_square: None,
                qth: None,
                name: None,
                qsl_route: None,
                confirmation_status: ConfirmationStatus::None,
                confirmation_date: None,
                dxcc_entity: 6, // United States
                contest_id: None,
                notes: None,
            };
            tracker.add_qso(qso).await.unwrap();
        }

        let stats = tracker.get_qso_statistics_by_entity().await.unwrap();
        assert_eq!(stats.get(&6), Some(&5));
    }
}
