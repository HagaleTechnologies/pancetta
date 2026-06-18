//! # pancetta-dx
//!
//! Network integrations for amateur radio data sources that don't fit
//! the cqdx.io HTTP client (`pancetta-cqdx`) — typically because they
//! speak a non-cqdx protocol (DX cluster telnet/WebSocket) or because
//! the call requires per-operator credentials we keep local on the
//! pancetta host (LoTW upload, future eQSL/Clublog/QRZ).
//!
//! ## Live integrations
//!
//! - [`cluster`] — DX cluster client (telnet + WebSocket transports),
//!   used by the `dx_cluster` coordinator component to receive spots
//!   from human operators worldwide.
//! - [`pskreporter`] — uploads locally-decoded FT8 messages to the
//!   global PSKReporter database for reciprocal spot visibility.
//!
//! ## Per-QSO log upload (opt-in)
//!
//! - [`qso_upload`] — per-QSO upload clients wired into the coordinator's
//!   `start_qso_upload_subscriber`, all opt-in (default disabled):
//!   [`ClubLogClient`] / [`QrzLogbookClient`] (raw ADIF POST), [`EqslClient`]
//!   (ADIF POST to eQSL.cc's `importADIF.cfm`), and [`LotwUploadClient`]
//!   (shells out to the operator's `tqsl` CLI to sign + upload, since LoTW
//!   requires a TQSL digital signature). Credentials stay local and are never
//!   logged.
//!
//! ## Scaffolding
//!
//! - [`lotw`] — ARRL LoTW *web-scrape* client (login + bulk ADIF upload + QSL
//!   download) wired but with no coordinator caller. The per-QSO signed-upload
//!   path lives in [`qso_upload`] instead (it shells out to `tqsl`). This
//!   module remains for future bulk-confirmation download and is covered by the
//!   HTTPS scheme guard tests.
//!
//! ## What used to live here
//!
//! Several modules were deleted in 2026-04 because cqdx.io now serves
//! the same data through `pancetta-cqdx`:
//!
//! | Removed module           | Replacement                                      |
//! |--------------------------|--------------------------------------------------|
//! | `dxcc.rs`                | `pancetta_cqdx::CqdxCache::resolve_entity`       |
//! | `priorities.rs`          | `pancetta_qso::priority::PriorityScorer`         |
//! | `propagation.rs` / `_enhanced` | (deferred — future cqdx.io feature)        |
//! | `statistics.rs`          | `pancetta_cqdx::CqdxCache` + per-band rolling    |
//! | `tracker.rs`             | `pancetta_qso::async_logger::QsoLogger`     |
//! | `gridsquare.rs`          | `pancetta_core::gridsquare`                      |
//! | `geography.rs`           | `geographiclib_rs::Geodesic` directly            |

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md

pub mod cluster;
pub mod lotw;
pub mod pskreporter;
pub mod qrz_xml;
pub mod qso_upload;

pub use qrz_xml::{QrzLookup, QrzXmlClient};
pub use qso_upload::{
    ClubLogClient, EqslClient, LotwClient as LotwUploadClient, QrzInsertOutcome, QrzLogbookClient,
    QsoUploadOutcome,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
