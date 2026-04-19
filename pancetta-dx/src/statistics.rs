//! DX Statistics and Achievements
//!
//! This module provides comprehensive statistics calculation and achievement
//! tracking for amateur radio DX activities and awards.

use crate::{dxcc::DxccDatabase, tracker::DxTracker, Band, DxError, Mode, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info};

/// Overall DX statistics summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxStatistics {
    /// Total QSOs logged
    pub total_qsos: u32,
    /// Total unique callsigns worked
    pub unique_callsigns: u32,
    /// Total DXCC entities worked
    pub dxcc_entities_worked: u32,
    /// Total DXCC entities confirmed
    pub dxcc_entities_confirmed: u32,
    /// QSOs by band
    pub qsos_by_band: HashMap<Band, u32>,
    /// QSOs by mode
    pub qsos_by_mode: HashMap<Mode, u32>,
    /// QSOs by year
    pub qsos_by_year: HashMap<u32, u32>,
    /// QSOs by month (last 12 months)
    pub qsos_by_month: HashMap<String, u32>,
    /// Average QSOs per day (last 30 days)
    pub avg_qsos_per_day: f64,
    /// Most active band
    pub most_active_band: Option<Band>,
    /// Most active mode
    pub most_active_mode: Option<Mode>,
    /// First QSO date
    pub first_qso_date: Option<DateTime<Utc>>,
    /// Last QSO date
    pub last_qso_date: Option<DateTime<Utc>>,
    /// Longest QSO distance
    pub longest_distance_km: Option<f64>,
    /// Most worked entity
    pub most_worked_entity: Option<(u16, u32)>, // (entity_code, count)
    /// Countries per continent
    pub countries_per_continent: HashMap<String, u32>,
    /// Confirmation rate percentage
    pub confirmation_rate: f64,
}

/// Band-specific statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandStatistics {
    /// Band
    pub band: Band,
    /// Total QSOs on band
    pub total_qsos: u32,
    /// Unique callsigns on band
    pub unique_callsigns: u32,
    /// DXCC entities worked on band
    pub entities_worked: u32,
    /// DXCC entities confirmed on band
    pub entities_confirmed: u32,
    /// QSOs by mode on this band
    pub qsos_by_mode: HashMap<Mode, u32>,
    /// QSOs by continent
    pub qsos_by_continent: HashMap<String, u32>,
    /// Average signal reports sent/received
    pub avg_rst_sent: f64,
    /// Average signal reports received
    pub avg_rst_received: f64,
    /// Most active hour (UTC)
    pub most_active_hour: Option<u8>,
    /// Activity by hour
    pub activity_by_hour: HashMap<u8, u32>,
    /// Longest distance worked
    pub longest_distance_km: Option<f64>,
    /// Confirmation rate for this band
    pub confirmation_rate: f64,
}

/// Mode-specific statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeStatistics {
    /// Mode
    pub mode: Mode,
    /// Total QSOs in mode
    pub total_qsos: u32,
    /// Unique callsigns in mode
    pub unique_callsigns: u32,
    /// DXCC entities worked in mode
    pub entities_worked: u32,
    /// DXCC entities confirmed in mode
    pub entities_confirmed: u32,
    /// QSOs by band in this mode
    pub qsos_by_band: HashMap<Band, u32>,
    /// Most active band for this mode
    pub most_active_band: Option<Band>,
    /// Average signal reports
    pub avg_rst_sent: f64,
    /// Average signal reports received
    pub avg_rst_received: f64,
    /// Confirmation rate for this mode
    pub confirmation_rate: f64,
}

/// Achievement tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Achievement {
    /// Achievement ID
    pub id: String,
    /// Achievement name
    pub name: String,
    /// Achievement description
    pub description: String,
    /// Achievement category
    pub category: String,
    /// Current progress
    pub current: u32,
    /// Target value
    pub target: u32,
    /// Progress percentage
    pub progress_percent: f64,
    /// Date achieved (if completed)
    pub achieved_date: Option<DateTime<Utc>>,
    /// Date first attempted
    pub first_attempt_date: Option<DateTime<Utc>>,
    /// Is this achievement completed?
    pub completed: bool,
}

/// DX contest statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContestStatistics {
    /// Contest ID
    pub contest_id: String,
    /// Contest name
    pub contest_name: String,
    /// Participation date
    pub date: DateTime<Utc>,
    /// Total QSOs in contest
    pub total_qsos: u32,
    /// Unique callsigns worked
    pub unique_callsigns: u32,
    /// Multipliers worked
    pub multipliers: u32,
    /// Score (if calculated)
    pub score: Option<u32>,
    /// QSOs by band
    pub qsos_by_band: HashMap<Band, u32>,
    /// QSOs by mode
    pub qsos_by_mode: HashMap<Mode, u32>,
    /// Operating time (hours)
    pub operating_hours: f64,
    /// QSOs per hour
    pub qsos_per_hour: f64,
}

/// Activity timeline entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityTimelineEntry {
    /// Date
    pub date: DateTime<Utc>,
    /// Event type
    pub event_type: String,
    /// Description
    pub description: String,
    /// Associated values
    pub values: HashMap<String, String>,
}

/// Statistics engine
pub struct StatisticsEngine {
    /// Reference to tracker
    tracker: std::sync::Arc<DxTracker>,
    /// Reference to DXCC database
    dxcc: std::sync::Arc<DxccDatabase>,
    /// Cached statistics
    cached_stats: Option<DxStatistics>,
    /// Cache timestamp
    cache_time: Option<DateTime<Utc>>,
    /// Cache timeout in minutes
    cache_timeout_minutes: i64,
}

impl StatisticsEngine {
    /// Create new statistics engine from an Arc-wrapped tracker
    pub async fn new(tracker: std::sync::Arc<DxTracker>) -> Result<Self> {
        Ok(Self {
            tracker,
            dxcc: std::sync::Arc::new(crate::dxcc::DxccDatabase::new().await?),
            cached_stats: None,
            cache_time: None,
            cache_timeout_minutes: 15,
        })
    }

    /// Set cache timeout
    pub fn set_cache_timeout(&mut self, minutes: i64) {
        self.cache_timeout_minutes = minutes;
    }

    /// Update statistics (refresh cache)
    pub async fn update_statistics(&mut self) -> Result<()> {
        info!("Updating DX statistics");

        let stats = self.calculate_overall_statistics().await?;
        self.cached_stats = Some(stats);
        self.cache_time = Some(Utc::now());

        Ok(())
    }

    /// Get overall statistics
    pub async fn get_statistics(&mut self) -> Result<DxStatistics> {
        // Check cache
        if let (Some(stats), Some(cache_time)) = (&self.cached_stats, self.cache_time) {
            let cache_age = Utc::now().signed_duration_since(cache_time).num_minutes();
            if cache_age < self.cache_timeout_minutes {
                debug!("Using cached statistics (age: {} minutes)", cache_age);
                return Ok(stats.clone());
            }
        }

        // Calculate fresh statistics
        self.update_statistics().await?;
        Ok(self.cached_stats.clone().unwrap())
    }

    /// Calculate overall statistics
    async fn calculate_overall_statistics(&self) -> Result<DxStatistics> {
        // Get basic QSO counts
        let entity_stats = self.tracker.get_qso_statistics_by_entity().await?;
        let total_qsos = entity_stats.values().sum();
        let dxcc_entities_worked = entity_stats.len() as u32;

        // Count confirmed entities from award tracking table
        let dxcc_entities_confirmed = self.count_confirmed_entities().await.unwrap_or(0);

        // Get band/mode breakdowns
        let qsos_by_band = self.calculate_band_breakdown().await?;
        let qsos_by_mode = self.calculate_mode_breakdown().await?;

        // Calculate yearly breakdown
        let qsos_by_year = self.calculate_yearly_breakdown().await?;

        // Calculate monthly breakdown (last 12 months)
        let qsos_by_month = self.calculate_monthly_breakdown().await?;

        // Calculate average QSOs per day (last 30 days)
        let avg_qsos_per_day = self.calculate_avg_qsos_per_day().await?;

        // Find most active band and mode
        let most_active_band = qsos_by_band
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(&band, _)| band);

        let most_active_mode = qsos_by_mode
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(mode, _)| mode.clone());

        // Get date range
        let (first_qso_date, last_qso_date) = self.get_qso_date_range().await?;

        // Calculate other metrics
        let unique_callsigns = self.calculate_unique_callsigns().await?;
        let longest_distance_km = self.calculate_longest_distance().await?;
        let most_worked_entity = self.find_most_worked_entity(&entity_stats);
        let countries_per_continent = self.calculate_countries_per_continent().await?;
        let confirmation_rate = self.calculate_confirmation_rate().await?;

        Ok(DxStatistics {
            total_qsos,
            unique_callsigns,
            dxcc_entities_worked,
            dxcc_entities_confirmed,
            qsos_by_band,
            qsos_by_mode,
            qsos_by_year,
            qsos_by_month,
            avg_qsos_per_day,
            most_active_band,
            most_active_mode,
            first_qso_date,
            last_qso_date,
            longest_distance_km,
            most_worked_entity,
            countries_per_continent,
            confirmation_rate,
        })
    }

    /// Get statistics for specific band
    pub async fn get_band_statistics(&self, band: Band) -> Result<BandStatistics> {
        let band_str = band.to_string();
        let conn = self.tracker.connection.lock().unwrap();

        // Basic counts
        let total_qsos: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracked_contacts WHERE band = ?1",
                [&band_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let unique_callsigns: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT callsign) FROM tracked_contacts WHERE band = ?1",
                [&band_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let entities_worked: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT dxcc_entity) FROM tracked_contacts WHERE band = ?1",
                [&band_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let entities_confirmed: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT entity_code) FROM award_tracking WHERE band = ?1 AND status = '\"Confirmed\"'",
                [&band_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        // QSOs by mode on this band
        let mut qsos_by_mode = HashMap::new();
        if let Ok(mut stmt) = conn
            .prepare("SELECT mode, COUNT(*) FROM tracked_contacts WHERE band = ?1 GROUP BY mode")
        {
            if let Ok(rows) = stmt.query_map([&band_str], |row| {
                let mode_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((mode_str, count as u32))
            }) {
                for row in rows.flatten() {
                    if let Ok(mode) = serde_json::from_str::<Mode>(&format!("\"{}\"", row.0)) {
                        qsos_by_mode.insert(mode, row.1);
                    }
                }
            }
        }

        // QSOs by continent (via dxcc_entity lookup)
        let mut qsos_by_continent = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT dxcc_entity, COUNT(*) FROM tracked_contacts WHERE band = ?1 GROUP BY dxcc_entity",
        ) {
            if let Ok(rows) = stmt.query_map([&band_str], |row| {
                let entity: i64 = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((entity as u16, count as u32))
            }) {
                for row in rows.flatten() {
                    if let Some(entity) = self.dxcc.get_entity(row.0) {
                        *qsos_by_continent.entry(entity.continent.clone()).or_insert(0) += row.1;
                    }
                }
            }
        }

        // Average RST sent/received
        let (avg_rst_sent, avg_rst_received) = conn
            .query_row(
                "SELECT AVG(CAST(rst_sent AS REAL)), AVG(CAST(rst_received AS REAL)) FROM tracked_contacts WHERE band = ?1",
                [&band_str],
                |row| {
                    let sent: f64 = row.get::<_, Option<f64>>(0)?.unwrap_or(0.0);
                    let recv: f64 = row.get::<_, Option<f64>>(1)?.unwrap_or(0.0);
                    Ok((sent, recv))
                },
            )
            .unwrap_or((0.0, 0.0));

        // Activity by hour
        let mut activity_by_hour: HashMap<u8, u32> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT CAST(strftime('%H', datetime) AS INTEGER) as hr, COUNT(*) FROM tracked_contacts WHERE band = ?1 GROUP BY hr",
        ) {
            if let Ok(rows) = stmt.query_map([&band_str], |row| {
                let hour: i64 = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((hour as u8, count as u32))
            }) {
                for row in rows.flatten() {
                    activity_by_hour.insert(row.0, row.1);
                }
            }
        }

        let most_active_hour = activity_by_hour
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(&hour, _)| hour);

        // Confirmation rate
        let confirmation_rate = if total_qsos > 0 {
            let confirmed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tracked_contacts WHERE band = ?1 AND confirmation_status != '\"None\"'",
                    [&band_str],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            (confirmed as f64 / total_qsos as f64) * 100.0
        } else {
            0.0
        };

        Ok(BandStatistics {
            band,
            total_qsos,
            unique_callsigns,
            entities_worked,
            entities_confirmed,
            qsos_by_mode,
            qsos_by_continent,
            avg_rst_sent,
            avg_rst_received,
            most_active_hour,
            activity_by_hour,
            longest_distance_km: None, // no distance column in table
            confirmation_rate,
        })
    }

    /// Get statistics for specific mode
    pub async fn get_mode_statistics(&self, mode: &Mode) -> Result<ModeStatistics> {
        let mode_str = mode.to_string();
        let conn = self.tracker.connection.lock().unwrap();

        // Basic counts
        let total_qsos: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM tracked_contacts WHERE mode = ?1",
                [&mode_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let unique_callsigns: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT callsign) FROM tracked_contacts WHERE mode = ?1",
                [&mode_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let entities_worked: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT dxcc_entity) FROM tracked_contacts WHERE mode = ?1",
                [&mode_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        let entities_confirmed: u32 = conn
            .query_row(
                "SELECT COUNT(DISTINCT entity_code) FROM award_tracking WHERE mode = ?1 AND status = '\"Confirmed\"'",
                [&mode_str],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u32;

        // QSOs by band in this mode
        let mut qsos_by_band = HashMap::new();
        if let Ok(mut stmt) = conn
            .prepare("SELECT band, COUNT(*) FROM tracked_contacts WHERE mode = ?1 GROUP BY band")
        {
            if let Ok(rows) = stmt.query_map([&mode_str], |row| {
                let band_str: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((band_str, count as u32))
            }) {
                for row in rows.flatten() {
                    if let Ok(band) = row.0.parse::<Band>() {
                        qsos_by_band.insert(band, row.1);
                    }
                }
            }
        }

        let most_active_band = qsos_by_band
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(&band, _)| band);

        // Average RST sent/received
        let (avg_rst_sent, avg_rst_received) = conn
            .query_row(
                "SELECT AVG(CAST(rst_sent AS REAL)), AVG(CAST(rst_received AS REAL)) FROM tracked_contacts WHERE mode = ?1",
                [&mode_str],
                |row| {
                    let sent: f64 = row.get::<_, Option<f64>>(0)?.unwrap_or(0.0);
                    let recv: f64 = row.get::<_, Option<f64>>(1)?.unwrap_or(0.0);
                    Ok((sent, recv))
                },
            )
            .unwrap_or((0.0, 0.0));

        // Confirmation rate
        let confirmation_rate = if total_qsos > 0 {
            let confirmed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tracked_contacts WHERE mode = ?1 AND confirmation_status != '\"None\"'",
                    [&mode_str],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            (confirmed as f64 / total_qsos as f64) * 100.0
        } else {
            0.0
        };

        Ok(ModeStatistics {
            mode: mode.clone(),
            total_qsos,
            unique_callsigns,
            entities_worked,
            entities_confirmed,
            qsos_by_band,
            most_active_band,
            avg_rst_sent,
            avg_rst_received,
            confirmation_rate,
        })
    }

    /// Get all achievements
    pub async fn get_achievements(&self) -> Result<Vec<Achievement>> {
        let mut achievements = Vec::new();

        // Get overall stats for calculating achievements
        let stats = self
            .cached_stats
            .as_ref()
            .ok_or_else(|| DxError::Configuration("Statistics not calculated".to_string()))?;

        // DXCC achievements
        achievements.push(Achievement {
            id: "dxcc_mixed".to_string(),
            name: "DXCC Mixed".to_string(),
            description: "Work 100 DXCC entities on any band/mode combination".to_string(),
            category: "DXCC".to_string(),
            current: stats.dxcc_entities_worked,
            target: 100,
            progress_percent: (stats.dxcc_entities_worked as f64 / 100.0 * 100.0).min(100.0),
            achieved_date: if stats.dxcc_entities_worked >= 100 {
                Some(Utc::now())
            } else {
                None
            },
            first_attempt_date: stats.first_qso_date,
            completed: stats.dxcc_entities_worked >= 100,
        });

        achievements.push(Achievement {
            id: "dxcc_honor_roll".to_string(),
            name: "DXCC Honor Roll".to_string(),
            description: "Work 331+ current DXCC entities".to_string(),
            category: "DXCC".to_string(),
            current: stats.dxcc_entities_worked,
            target: 331,
            progress_percent: (stats.dxcc_entities_worked as f64 / 331.0 * 100.0).min(100.0),
            achieved_date: if stats.dxcc_entities_worked >= 331 {
                Some(Utc::now())
            } else {
                None
            },
            first_attempt_date: stats.first_qso_date,
            completed: stats.dxcc_entities_worked >= 331,
        });

        // QSO count achievements
        achievements.push(Achievement {
            id: "qso_1000".to_string(),
            name: "1,000 QSOs".to_string(),
            description: "Log 1,000 QSOs".to_string(),
            category: "QSO Count".to_string(),
            current: stats.total_qsos,
            target: 1000,
            progress_percent: (stats.total_qsos as f64 / 1000.0 * 100.0).min(100.0),
            achieved_date: if stats.total_qsos >= 1000 {
                Some(Utc::now())
            } else {
                None
            },
            first_attempt_date: stats.first_qso_date,
            completed: stats.total_qsos >= 1000,
        });

        achievements.push(Achievement {
            id: "qso_10000".to_string(),
            name: "10,000 QSOs".to_string(),
            description: "Log 10,000 QSOs".to_string(),
            category: "QSO Count".to_string(),
            current: stats.total_qsos,
            target: 10000,
            progress_percent: (stats.total_qsos as f64 / 10000.0 * 100.0).min(100.0),
            achieved_date: if stats.total_qsos >= 10000 {
                Some(Utc::now())
            } else {
                None
            },
            first_attempt_date: stats.first_qso_date,
            completed: stats.total_qsos >= 10000,
        });

        // Band-specific achievements
        for &band in Band::all() {
            if let Some(&qso_count) = stats.qsos_by_band.get(&band) {
                achievements.push(Achievement {
                    id: format!("band_{}_{}", band.to_string().to_lowercase(), 100),
                    name: format!("{} - 100 QSOs", band),
                    description: format!("Log 100 QSOs on {}", band),
                    category: "Band Achievements".to_string(),
                    current: qso_count,
                    target: 100,
                    progress_percent: (qso_count as f64 / 100.0 * 100.0).min(100.0),
                    achieved_date: if qso_count >= 100 {
                        Some(Utc::now())
                    } else {
                        None
                    },
                    first_attempt_date: stats.first_qso_date,
                    completed: qso_count >= 100,
                });
            }
        }

        // Confirmation achievements
        achievements.push(Achievement {
            id: "confirmation_rate_90".to_string(),
            name: "90% Confirmation Rate".to_string(),
            description: "Achieve 90% QSL confirmation rate".to_string(),
            category: "Confirmation".to_string(),
            current: (stats.confirmation_rate * 100.0) as u32,
            target: 90,
            progress_percent: stats.confirmation_rate,
            achieved_date: if stats.confirmation_rate >= 90.0 {
                Some(Utc::now())
            } else {
                None
            },
            first_attempt_date: stats.first_qso_date,
            completed: stats.confirmation_rate >= 90.0,
        });

        Ok(achievements)
    }

    /// Get contest statistics
    pub async fn get_contest_statistics(&self) -> Result<Vec<ContestStatistics>> {
        // This would analyze QSOs with contest_id field
        // For now, return empty list
        Ok(Vec::new())
    }

    /// Get activity timeline
    pub async fn get_activity_timeline(&self, _days: i64) -> Result<Vec<ActivityTimelineEntry>> {
        // Return empty vec rather than fake hardcoded data
        Ok(Vec::new())
    }

    /// Get top worked entities
    pub async fn get_top_worked_entities(&self, limit: usize) -> Result<Vec<(u16, String, u32)>> {
        let entity_stats = self.tracker.get_qso_statistics_by_entity().await?;

        let mut entities: Vec<(u16, u32)> = entity_stats.into_iter().collect();
        entities.sort_by(|a, b| b.1.cmp(&a.1));
        entities.truncate(limit);

        let mut result = Vec::new();
        for (entity_code, count) in entities {
            if let Some(entity) = self.dxcc.get_entity(entity_code) {
                result.push((entity_code, entity.name.clone(), count));
            } else {
                result.push((
                    entity_code,
                    format!("Unknown Entity {}", entity_code),
                    count,
                ));
            }
        }

        Ok(result)
    }

    /// Calculate QSO rate trends
    pub async fn calculate_qso_rate_trends(&self, _days: i64) -> Result<Vec<(DateTime<Utc>, u32)>> {
        // Not yet implemented — requires date-based aggregate query
        Ok(Vec::new())
    }

    // Helper methods for statistics calculation

    async fn calculate_band_breakdown(&self) -> Result<HashMap<Band, u32>> {
        let conn = self.tracker.connection.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT band, COUNT(*) as cnt FROM tracked_contacts GROUP BY band")?;
        let rows = stmt.query_map([], |row| {
            let band_str: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((band_str, count as u32))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (band_str, count) = row?;
            if let Ok(band) = band_str.parse::<Band>() {
                result.insert(band, count);
            }
        }
        Ok(result)
    }

    async fn calculate_mode_breakdown(&self) -> Result<HashMap<Mode, u32>> {
        let conn = self.tracker.connection.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT mode, COUNT(*) as cnt FROM tracked_contacts GROUP BY mode")?;
        let rows = stmt.query_map([], |row| {
            let mode_str: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((mode_str, count as u32))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (mode_str, count) = row?;
            if let Ok(mode) = serde_json::from_str::<Mode>(&format!("\"{}\"", mode_str)) {
                result.insert(mode, count);
            }
        }
        Ok(result)
    }

    async fn calculate_yearly_breakdown(&self) -> Result<HashMap<u32, u32>> {
        let conn = self.tracker.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT strftime('%Y', datetime) as yr, COUNT(*) as cnt FROM tracked_contacts GROUP BY yr",
        )?;
        let rows = stmt.query_map([], |row| {
            let year_str: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((year_str, count as u32))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (year_str, count) = row?;
            if let Ok(year) = year_str.parse::<u32>() {
                result.insert(year, count);
            }
        }
        Ok(result)
    }

    async fn calculate_monthly_breakdown(&self) -> Result<HashMap<String, u32>> {
        let conn = self.tracker.connection.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT strftime('%Y-%m', datetime) as ym, COUNT(*) as cnt
             FROM tracked_contacts
             WHERE datetime >= datetime('now', '-12 months')
             GROUP BY ym
             ORDER BY ym",
        )?;
        let rows = stmt.query_map([], |row| {
            let ym: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((ym, count as u32))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (ym, count) = row?;
            result.insert(ym, count);
        }
        Ok(result)
    }

    async fn calculate_avg_qsos_per_day(&self) -> Result<f64> {
        let (first, last) = self.get_qso_date_range().await?;
        match (first, last) {
            (Some(first_date), Some(last_date)) => {
                let days = last_date
                    .signed_duration_since(first_date)
                    .num_days()
                    .max(1);
                let conn = self.tracker.connection.lock().unwrap();
                let total: i64 =
                    conn.query_row("SELECT COUNT(*) FROM tracked_contacts", [], |row| {
                        row.get(0)
                    })?;
                Ok(total as f64 / days as f64)
            }
            _ => Ok(0.0),
        }
    }

    async fn get_qso_date_range(&self) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>)> {
        let conn = self.tracker.connection.lock().unwrap();
        let result = conn.query_row(
            "SELECT MIN(datetime), MAX(datetime) FROM tracked_contacts",
            [],
            |row| {
                let min_str: Option<String> = row.get(0)?;
                let max_str: Option<String> = row.get(1)?;
                Ok((min_str, max_str))
            },
        )?;
        let first = result
            .0
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let last = result
            .1
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        Ok((first, last))
    }

    async fn calculate_unique_callsigns(&self) -> Result<u32> {
        let conn = self.tracker.connection.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT callsign) FROM tracked_contacts",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    async fn calculate_longest_distance(&self) -> Result<Option<f64>> {
        // The tracked_contacts table does not store distance_km,
        // so we cannot compute this from the database alone.
        Ok(None)
    }

    fn find_most_worked_entity(&self, entity_stats: &HashMap<u16, u32>) -> Option<(u16, u32)> {
        entity_stats
            .iter()
            .max_by_key(|(_, &count)| count)
            .map(|(&entity, &count)| (entity, count))
    }

    async fn calculate_countries_per_continent(&self) -> Result<HashMap<String, u32>> {
        let conn = self.tracker.connection.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT dxcc_entity FROM tracked_contacts GROUP BY dxcc_entity")?;
        let rows = stmt.query_map([], |row| {
            let entity_code: i64 = row.get(0)?;
            Ok(entity_code as u16)
        })?;
        let mut continent_counts: HashMap<String, std::collections::HashSet<u16>> = HashMap::new();
        for row in rows {
            let entity_code = row?;
            if let Some(entity) = self.dxcc.get_entity(entity_code) {
                continent_counts
                    .entry(entity.continent.clone())
                    .or_default()
                    .insert(entity_code);
            }
        }
        Ok(continent_counts
            .into_iter()
            .map(|(continent, entities)| (continent, entities.len() as u32))
            .collect())
    }

    async fn calculate_confirmation_rate(&self) -> Result<f64> {
        let conn = self.tracker.connection.lock().unwrap();
        let (total, confirmed): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), SUM(CASE WHEN confirmation_status != '\"None\"' THEN 1 ELSE 0 END)
             FROM tracked_contacts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if total == 0 {
            Ok(0.0)
        } else {
            Ok((confirmed as f64 / total as f64) * 100.0)
        }
    }

    async fn count_confirmed_entities(&self) -> Result<u32> {
        let conn = self.tracker.connection.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT entity_code) FROM award_tracking WHERE status = '\"Confirmed\"'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn create_test_tracker() -> (DxTracker, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let tracker = crate::tracker::DxTracker::new(temp_file.path().to_str().unwrap())
            .await
            .unwrap();
        (tracker, temp_file)
    }

    #[tokio::test]
    async fn test_statistics_engine_creation() {
        let (tracker, _temp_file) = create_test_tracker().await;
        let tracker_arc = std::sync::Arc::new(tracker);
        let stats_engine = StatisticsEngine::new(tracker_arc).await.unwrap();
        assert_eq!(stats_engine.cache_timeout_minutes, 15);
    }

    #[tokio::test]
    async fn test_achievement_creation() {
        let achievement = Achievement {
            id: "test".to_string(),
            name: "Test Achievement".to_string(),
            description: "Test description".to_string(),
            category: "Test".to_string(),
            current: 50,
            target: 100,
            progress_percent: 50.0,
            achieved_date: None,
            first_attempt_date: Some(Utc::now()),
            completed: false,
        };

        assert_eq!(achievement.progress_percent, 50.0);
        assert!(!achievement.completed);
    }

    #[test]
    fn test_band_statistics() {
        let stats = BandStatistics {
            band: Band::Band20m,
            total_qsos: 100,
            unique_callsigns: 85,
            entities_worked: 45,
            entities_confirmed: 32,
            qsos_by_mode: HashMap::new(),
            qsos_by_continent: HashMap::new(),
            avg_rst_sent: 590.5,
            avg_rst_received: 588.3,
            most_active_hour: Some(14),
            activity_by_hour: HashMap::new(),
            longest_distance_km: Some(15000.0),
            confirmation_rate: 71.1,
        };

        assert_eq!(stats.band, Band::Band20m);
        assert_eq!(stats.total_qsos, 100);
        assert_eq!(stats.confirmation_rate, 71.1);
    }

    #[test]
    fn test_mode_statistics() {
        let stats = ModeStatistics {
            mode: Mode::FT8,
            total_qsos: 150,
            unique_callsigns: 120,
            entities_worked: 55,
            entities_confirmed: 40,
            qsos_by_band: HashMap::new(),
            most_active_band: Some(Band::Band20m),
            avg_rst_sent: 599.0,
            avg_rst_received: 597.8,
            confirmation_rate: 72.7,
        };

        assert_eq!(stats.mode, Mode::FT8);
        assert_eq!(stats.total_qsos, 150);
        assert_eq!(stats.most_active_band, Some(Band::Band20m));
    }

    #[test]
    fn test_contest_statistics() {
        let stats = ContestStatistics {
            contest_id: "CQ-WW-DX".to_string(),
            contest_name: "CQ World Wide DX Contest".to_string(),
            date: Utc::now(),
            total_qsos: 200,
            unique_callsigns: 180,
            multipliers: 50,
            score: Some(25000),
            qsos_by_band: HashMap::new(),
            qsos_by_mode: HashMap::new(),
            operating_hours: 24.0,
            qsos_per_hour: 8.33,
        };

        assert_eq!(stats.contest_id, "CQ-WW-DX");
        assert_eq!(stats.total_qsos, 200);
        assert_eq!(stats.score, Some(25000));
    }

    #[test]
    fn test_activity_timeline_entry() {
        let entry = ActivityTimelineEntry {
            date: Utc::now(),
            event_type: "milestone".to_string(),
            description: "Reached 1,000 QSOs".to_string(),
            values: [("qso_count".to_string(), "1000".to_string())].into(),
        };

        assert_eq!(entry.event_type, "milestone");
        assert_eq!(entry.values.get("qso_count"), Some(&"1000".to_string()));
    }
}
