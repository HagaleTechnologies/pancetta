//! # pancetta-dx
//!
//! Network integrations for amateur radio data sources that don't fit
//! the cqdx.io HTTP client (`pancetta-cqdx`) — typically because they
//! speak a non-cqdx protocol (DX cluster telnet) or because the call
//! requires per-operator credentials we keep local on the pancetta
//! host (LoTW upload, future eQSL/Clublog/QRZ).
//!
//! ## Live integrations
//!
//! - [`cluster`] — traditional DX cluster telnet client, used by the
//!   `dx_cluster` coordinator component to receive spots from human
//!   operators worldwide.
//! - [`pskreporter`] — uploads locally-decoded FT8 messages to the
//!   global PSKReporter database for reciprocal spot visibility.
//!
//! ## Scaffolding
//!
//! - [`lotw`] — ARRL LoTW client with login + ADIF upload + QSL download
//!   wired but no caller yet. The credentialed-integration build-out is
//!   tracked under `docs/superpowers/specs/`. Until then the module is
//!   covered by the HTTPS scheme guard tests but isn't run from the
//!   coordinator.

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![allow(dead_code, unused_imports)]

pub mod cluster;
pub mod lotw;
pub mod pskreporter;

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
