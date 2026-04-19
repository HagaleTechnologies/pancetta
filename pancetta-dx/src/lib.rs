//! # pancetta-dx
//!
//! DX hunting, DXCC entity data, rarity scoring, and PSKReporter integration.
//!
//! Advanced DX hunting and amateur radio award tracking system with real-time
//! cluster integration, propagation analysis, and comprehensive DXCC management.
//!
//! This crate provides:
//! - DXCC entity database and lookup functionality
//! - Rarity scoring algorithms for DX prioritization
//! - Worked/confirmed tracking with multiple confirmation methods
//! - Geographic calculations including distance and bearing
//! - Propagation prediction integration
//! - External service integrations (PSKReporter, DX clusters, LoTW)
//! - Comprehensive statistics and progress reporting
//!
//! ## Data Flow
//! external services (PSKReporter, DX clusters) -> **pancetta-dx** -> `pancetta` coordinator (rarity scores, entity lookups)
//!
//! ## Key Types
//! - [`scorer::RarityScorer`] -- computes a 0.0–1.0 rarity score for a DXCC entity
//! - [`dxcc::DxccDatabase`] -- DXCC entity database with callsign-to-entity lookup
//! - [`tracker::DxTracker`] -- tracks worked/confirmed entities per band/mode
//! - [`pskreporter::PskReporter`] -- submits spots to and retrieves spots from PSKReporter
//! - [`DxError`] -- crate-level error type
//!
//! ## Crate Relationships
//! - Receives from: PSKReporter API, DX cluster feeds, LoTW
//! - Sends to: `pancetta` coordinator (rarity scores, needed entity status)

#![allow(dead_code, unused_imports)]

pub mod cluster;
pub mod dxcc;
pub mod geography;
pub mod gridsquare;
pub mod lotw;
pub mod priorities;
pub mod propagation;
pub mod propagation_enhanced;
pub mod pskreporter;
pub mod reports;
pub mod scorer;
pub mod statistics;
pub mod tracker;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

// Re-export unified types from pancetta-core
pub use pancetta_core::{Band, Mode};

/// DX Hunter result type
pub type Result<T> = std::result::Result<T, DxError>;

/// DX hunting specific errors
#[derive(Error, Debug)]
pub enum DxError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Geographic calculation error: {0}")]
    Geography(String),

    #[error("DXCC entity not found: {0}")]
    DxccNotFound(String),

    #[error("Invalid callsign format: {0}")]
    InvalidCallsign(String),

    #[error("Invalid grid square: {0}")]
    InvalidGridSquare(String),

    #[error("Propagation prediction error: {0}")]
    Propagation(String),

    #[error("External service error: {0}")]
    ExternalService(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),

    #[error("XML error: {0}")]
    Xml(#[from] quick_xml::Error),

    #[error("Websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("Join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("Format error: {0}")]
    Format(#[from] std::fmt::Error),
}

// Band and Mode types are now imported from pancetta-core

/// QSO confirmation status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmationStatus {
    /// Not confirmed
    None,
    /// Confirmed via QSL card
    QslCard,
    /// Confirmed via eQSL
    EQsl,
    /// Confirmed via Logbook of the World (LoTW)
    Lotw,
    /// Confirmed via ClubLog
    ClubLog,
    /// Confirmed via QRZ.com
    Qrz,
    /// Confirmed via other means
    Other(String),
}

/// DX spot information from cluster or other sources
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxSpot {
    /// Spotted callsign
    pub callsign: String,
    /// Frequency in Hz
    pub frequency: u64,
    /// Operating mode
    pub mode: Option<Mode>,
    /// Spotter callsign
    pub spotter: String,
    /// Spot time
    pub time: DateTime<Utc>,
    /// Comment/notes
    pub comment: Option<String>,
    /// DXCC entity information
    pub dxcc_entity: Option<u16>,
    /// Grid square if available
    pub grid_square: Option<String>,
    /// Calculated distance from home station
    pub distance_km: Option<f64>,
    /// Calculated bearing from home station
    pub bearing_degrees: Option<f64>,
    /// Rarity score
    pub rarity_score: Option<f64>,
}

/// QSO record for DX tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxQso {
    /// Unique QSO identifier
    pub id: Option<uuid::Uuid>,
    /// Contacted callsign
    pub callsign: String,
    /// QSO date and time
    pub datetime: DateTime<Utc>,
    /// Frequency in Hz
    pub frequency: u64,
    /// Band
    pub band: Band,
    /// Operating mode
    pub mode: Mode,
    /// Signal report sent
    pub rst_sent: String,
    /// Signal report received
    pub rst_received: String,
    /// Grid square of contacted station
    pub grid_square: Option<String>,
    /// QTH/location of contacted station
    pub qth: Option<String>,
    /// Name of operator
    pub name: Option<String>,
    /// QSL route information
    pub qsl_route: Option<String>,
    /// Confirmation status
    pub confirmation_status: ConfirmationStatus,
    /// Confirmation date
    pub confirmation_date: Option<DateTime<Utc>>,
    /// DXCC entity
    pub dxcc_entity: u16,
    /// Contest/activity identifier
    pub contest_id: Option<String>,
    /// Additional notes
    pub notes: Option<String>,
}

/// Station configuration for DX operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationConfig {
    /// Home callsign
    pub callsign: String,
    /// Home grid square
    pub grid_square: String,
    /// Home QTH
    pub qth: String,
    /// Latitude in decimal degrees
    pub latitude: f64,
    /// Longitude in decimal degrees
    pub longitude: f64,
    /// DXCC entity of home station
    pub dxcc_entity: u16,
    /// Preferred QSL route
    pub qsl_route: Option<String>,
    /// LoTW username
    pub lotw_username: Option<String>,
    /// eQSL username
    pub eqsl_username: Option<String>,
    /// ClubLog username
    pub clublog_username: Option<String>,
}

/// DX priority configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxPriorityConfig {
    /// Award tracking preferences
    pub awards: HashMap<String, bool>,
    /// Band priorities (higher = more priority)
    pub band_priorities: HashMap<Band, u8>,
    /// Mode priorities (higher = more priority)
    pub mode_priorities: HashMap<Mode, u8>,
    /// Minimum rarity score to consider
    pub min_rarity_score: f64,
    /// Maximum distance for VHF/UHF DX
    pub max_vhf_distance_km: Option<f64>,
    /// Enable automatic spot filtering
    pub auto_filter: bool,
    /// Blacklisted callsigns/prefixes
    pub blacklist: Vec<String>,
    /// Whitelist for priority callsigns/prefixes
    pub whitelist: Vec<String>,
}

impl Default for DxPriorityConfig {
    fn default() -> Self {
        let mut band_priorities = HashMap::new();
        for band in Band::all() {
            band_priorities.insert(*band, 5); // Default medium priority
        }

        let mut mode_priorities = HashMap::new();
        mode_priorities.insert(Mode::CW, 8);
        mode_priorities.insert(Mode::USB, 7);
        mode_priorities.insert(Mode::FT8, 6);
        mode_priorities.insert(Mode::RTTY, 6);

        Self {
            awards: HashMap::new(),
            band_priorities,
            mode_priorities,
            min_rarity_score: 0.0,
            max_vhf_distance_km: Some(500.0),
            auto_filter: true,
            blacklist: Vec::new(),
            whitelist: Vec::new(),
        }
    }
}

/// Main DX Hunter engine
pub struct DxHunter {
    pub dxcc: std::sync::Arc<dxcc::DxccDatabase>,
    pub tracker: std::sync::Arc<tracker::DxTracker>,
    pub scorer: scorer::RarityScorer,
    pub priorities: priorities::PriorityManager,
    pub geography: geography::GeographyCalculator,
    pub propagation: propagation::PropagationPredictor,
    pub pskreporter: pskreporter::PskReporterClient,
    pub cluster: cluster::DxClusterClient,
    pub lotw: lotw::LotwClient,
    pub statistics: statistics::StatisticsEngine,
    pub reports: reports::ReportGenerator,
    station_config: StationConfig,
}

impl DxHunter {
    /// Create new DX Hunter instance
    pub async fn new(station_config: StationConfig, database_path: &str) -> Result<Self> {
        let dxcc = std::sync::Arc::new(dxcc::DxccDatabase::new().await?);
        let tracker = std::sync::Arc::new(tracker::DxTracker::new(database_path).await?);
        let scorer = scorer::RarityScorer::new(&tracker).await?;
        let priorities = priorities::PriorityManager::new(DxPriorityConfig::default());
        let geography =
            geography::GeographyCalculator::new(station_config.latitude, station_config.longitude);
        let propagation = propagation::PropagationPredictor::new();
        let pskreporter = pskreporter::PskReporterClient::new();
        let cluster = cluster::DxClusterClient::new();
        let lotw = lotw::LotwClient::new(station_config.lotw_username.clone());
        let statistics = statistics::StatisticsEngine::new(std::sync::Arc::clone(&tracker)).await?;
        let reports = reports::ReportGenerator::new(
            std::sync::Arc::clone(&tracker),
            std::sync::Arc::clone(&dxcc),
        )
        .await?;

        Ok(Self {
            dxcc,
            tracker,
            scorer,
            priorities,
            geography,
            propagation,
            pskreporter,
            cluster,
            lotw,
            statistics,
            reports,
            station_config,
        })
    }

    /// Get station configuration
    pub fn station_config(&self) -> &StationConfig {
        &self.station_config
    }

    /// Update station configuration
    pub fn update_station_config(&mut self, config: StationConfig) {
        self.station_config = config;
        self.geography = geography::GeographyCalculator::new(
            self.station_config.latitude,
            self.station_config.longitude,
        );
    }

    /// Process a DX spot and calculate priority
    pub async fn process_spot(&self, mut spot: DxSpot) -> Result<DxSpot> {
        // Look up DXCC entity if not provided
        if spot.dxcc_entity.is_none() {
            if let Ok(entity) = self.dxcc.lookup_callsign(&spot.callsign).await {
                spot.dxcc_entity = Some(entity.entity_code);
            }
        }

        // Calculate geographic information if grid square is available
        if let Some(grid) = &spot.grid_square {
            if let Ok((lat, lon)) = gridsquare::grid_to_coordinates(grid) {
                spot.distance_km = Some(self.geography.calculate_distance(lat, lon));
                spot.bearing_degrees = Some(self.geography.calculate_bearing(lat, lon));
            }
        }

        // Calculate rarity score
        if let Some(entity_code) = spot.dxcc_entity {
            let band = Band::from_frequency(spot.frequency);
            spot.rarity_score = Some(
                self.scorer
                    .calculate_rarity_score(entity_code, band, spot.mode.as_ref())
                    .await?,
            );
        }

        Ok(spot)
    }

    /// Check if a QSO would be needed for awards
    ///
    /// Resolves the callsign to a DXCC entity code via the DXCC database,
    /// then queries the award_tracking table for an existing confirmation.
    /// Returns `true` (needed) if the callsign cannot be resolved — conservative.
    pub async fn is_needed(&self, callsign: &str, band: Band, mode: &Mode) -> Result<bool> {
        match self.dxcc.lookup_callsign(callsign).await {
            Ok(entity) => {
                self.tracker
                    .is_entity_needed(entity.entity_code, band, mode)
                    .await
            }
            Err(_) => {
                // Can't resolve callsign — assume needed (conservative)
                Ok(true)
            }
        }
    }

    /// Record a new QSO
    pub async fn record_qso(&mut self, qso: DxQso) -> Result<uuid::Uuid> {
        let qso_id = self.tracker.add_qso(qso).await?;

        // Update statistics
        self.statistics.update_statistics().await?;

        Ok(qso_id)
    }

    /// Get priority spots based on current configuration
    pub async fn get_priority_spots(&self, _limit: usize) -> Result<Vec<DxSpot>> {
        // This would integrate with various spot sources
        // For now, return empty vector as placeholder
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_band_frequency_detection() {
        assert_eq!(Band::from_frequency(14_200_000), Some(Band::Band20m));
        assert_eq!(Band::from_frequency(7_150_000), Some(Band::Band40m));
        assert_eq!(Band::from_frequency(1_000_000), None);
    }

    #[test]
    fn test_band_contains_frequency() {
        assert!(Band::Band20m.contains_frequency(14_200_000));
        assert!(!Band::Band20m.contains_frequency(7_150_000));
    }

    #[test]
    fn test_mode_display() {
        assert_eq!(Mode::FT8.to_string(), "FT8");
        assert_eq!(Mode::CW.to_string(), "CW");
        // Custom mode variant removed from unified types
    }
}
