#![forbid(unsafe_code)]
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
//! | `dxcc.rs`                | `pancetta_core::dxcc::entity_for_callsign`       |
//! | `priorities.rs`          | `pancetta_qso::priority::PriorityScorer`         |
//! | `propagation.rs` / `_enhanced` | (deferred — future cqdx.io feature)        |
//! | `statistics.rs`          | `pancetta_cqdx::CqdxCache` + per-band rolling    |
//! | `tracker.rs`             | `pancetta_qso::async_logger::QsoLogger`     |
//! | `gridsquare.rs`          | `pancetta_core::gridsquare`                      |
//! | `geography.rs`           | `geographiclib_rs::Geodesic` directly            |

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md

mod body;
pub mod cluster;
pub mod lotw;
mod network_error;
pub mod pskreporter;
pub mod qrz_xml;
pub mod qso_upload;
#[cfg(test)]
mod test_support;
pub mod upload_outbox;

pub use network_error::NetworkError;
pub use qrz_xml::{QrzLookup, QrzXmlClient};
pub use qso_upload::{
    upload_failure_class_for_status, ClubLogClient, EqslClient, LotwClient as LotwUploadClient,
    QrzInsertOutcome, QrzLogbookClient, QsoUploadOutcome, UploadFailureClass,
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
pub enum DxErrorInner {
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),

    /// A response body passed its byte budget; reading stopped there.
    #[error("Response too large: more than {limit} bytes")]
    ResponseTooLarge {
        /// The budget, in bytes.
        limit: usize,
    },

    #[error("External service error: {0}")]
    ExternalService(String),

    /// An online-logbook upload failed; `class` says whether a retry can
    /// help (PAN-57). Displays exactly like `ExternalService`, so log lines
    /// do not change.
    #[error("External service error: {message}")]
    Upload {
        /// Whether a retry can help.
        class: UploadFailureClass,
        /// Operator-facing text (status and body prefix, never a credential).
        message: String,
    },

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

/// A pointer-sized DX failure with a borrowed inspection API.
#[derive(Error, Debug)]
#[error(transparent)]
pub struct DxError(Box<DxErrorInner>);

impl From<DxErrorInner> for DxError {
    fn from(inner: DxErrorInner) -> Self {
        Self(Box::new(inner))
    }
}

impl DxError {
    /// Inspect the failure without moving it out of its box.
    pub fn kind(&self) -> &DxErrorInner {
        &self.0
    }

    /// Whether a retry of the failed upload can help (PAN-57).
    pub fn upload_failure_class(&self) -> UploadFailureClass {
        match self.kind() {
            DxErrorInner::Upload { class, .. } => *class,
            DxErrorInner::ResponseTooLarge { .. } => UploadFailureClass::RecordRejected,
            // Network, Parse, Io, …: retrying never loses data.
            _ => UploadFailureClass::Transient,
        }
    }

    /// The operator-facing failure text: the message alone for `Upload`,
    /// otherwise `Display`.
    pub fn upload_failure_text(&self) -> String {
        match self.kind() {
            DxErrorInner::Upload { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }
}

impl From<NetworkError> for DxError {
    fn from(error: NetworkError) -> Self {
        DxErrorInner::Network(error).into()
    }
}

impl From<std::io::Error> for DxError {
    fn from(error: std::io::Error) -> Self {
        DxErrorInner::Io(error).into()
    }
}

impl From<serde_json::Error> for DxError {
    fn from(error: serde_json::Error) -> Self {
        DxErrorInner::Serialization(error).into()
    }
}

impl From<csv::Error> for DxError {
    fn from(error: csv::Error) -> Self {
        DxErrorInner::Csv(error).into()
    }
}

impl From<quick_xml::Error> for DxError {
    fn from(error: quick_xml::Error) -> Self {
        DxErrorInner::Xml(error).into()
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for DxError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        DxErrorInner::WebSocket(error).into()
    }
}

impl From<tokio::task::JoinError> for DxError {
    fn from(error: tokio::task::JoinError) -> Self {
        DxErrorInner::Join(error).into()
    }
}

impl From<std::fmt::Error> for DxError {
    fn from(error: std::fmt::Error) -> Self {
        DxErrorInner::Format(error).into()
    }
}

/// Every `reqwest` failure enters `DxError` here, so none keeps its request
/// URL (see [`NetworkError`]).
impl From<reqwest::Error> for DxError {
    fn from(e: reqwest::Error) -> Self {
        DxError::from(DxErrorInner::Network(e.into()))
    }
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
    fn dx_error_is_pointer_sized() {
        assert_eq!(std::mem::size_of::<DxError>(), std::mem::size_of::<usize>());
    }

    #[test]
    fn boxed_error_preserves_display_and_classification() {
        for class in [
            UploadFailureClass::Transient,
            UploadFailureClass::LoginRejected,
            UploadFailureClass::RecordRejected,
        ] {
            let err = DxError::from(DxErrorInner::Upload {
                class,
                message: "refused".into(),
            });
            assert_eq!(err.to_string(), "External service error: refused");
            assert_eq!(err.upload_failure_text(), "refused");
            assert_eq!(err.upload_failure_class(), class);
        }
        let err = DxError::from(DxErrorInner::ResponseTooLarge { limit: 65536 });
        assert_eq!(err.to_string(), "Response too large: more than 65536 bytes");
        assert_eq!(
            err.upload_failure_class(),
            UploadFailureClass::RecordRejected
        );
        for (inner, text) in [
            (
                DxErrorInner::Parse("invalid".into()),
                "Parse error: invalid",
            ),
            (
                DxErrorInner::Configuration("missing".into()),
                "Configuration error: missing",
            ),
            (
                DxErrorInner::ExternalService("unavailable".into()),
                "External service error: unavailable",
            ),
        ] {
            let err: DxError = inner.into();
            assert_eq!(err.to_string(), text);
            assert_eq!(err.upload_failure_text(), text);
            assert_eq!(err.upload_failure_class(), UploadFailureClass::Transient);
        }
    }

    #[tokio::test]
    async fn boxed_error_preserves_source_conversions() {
        use std::error::Error;
        let io: DxError = std::io::Error::other("disk unavailable").into();
        assert_eq!(io.source().unwrap().to_string(), "disk unavailable");
        fn check_source(error: impl Error + Into<DxError>, prefix: &str) {
            let text = error.to_string();
            let wrapped: DxError = error.into();
            assert_eq!(wrapped.to_string(), format!("{prefix}: {text}"));
            assert_eq!(wrapped.source().unwrap().to_string(), text);
            assert_eq!(
                wrapped.upload_failure_class(),
                UploadFailureClass::Transient
            );
        }
        check_source(std::fmt::Error, "Format error");
        check_source(
            serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
            "Serialization error",
        );
        check_source(
            csv::Error::from(std::io::Error::other("csv read failed")),
            "CSV error",
        );
        check_source(
            quick_xml::Error::from(std::io::Error::other("xml read failed")),
            "XML error",
        );
        check_source(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed,
            "Websocket error",
        );
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        check_source(task.await.unwrap_err(), "Join error");
        let raw = reqwest::Client::new()
            .get(format!(
                "http://{}/?token=private",
                crate::test_support::closed_port()
            ))
            .send()
            .await
            .unwrap_err();
        let network: DxError = raw.into();
        assert!(network.source().is_some());
        crate::test_support::assert_network_error_omits(&network, &["private"]);
    }

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

    #[tokio::test]
    async fn network_error_drops_the_request_url() {
        let url = format!(
            "http://{}/path?token=tok-secret-123",
            crate::test_support::closed_port()
        );
        let raw = reqwest::Client::new().get(&url).send().await.unwrap_err();
        // reqwest itself renders the URL, so this test sees a regression.
        assert!(raw.to_string().contains("tok-secret-123"));

        let err = DxError::from(raw);
        crate::test_support::assert_network_error_omits(&err, &["tok-secret-123"]);
        assert_eq!(err.to_string(), "Network error: error sending request");
    }
}
