//! # pancetta-qso
//!
//! QSO management: autonomous operator, priority scorer, frequency allocator, state machine, ADIF logging.
//!
//! A comprehensive QSO (contact) management and logging library for FT8 amateur radio communications.
//! This library provides state machine-based QSO tracking, ADIF import/export,
//! SQLite-based storage, and comprehensive statistics and analytics.
//!
//! ## Data Flow
//! `pancetta` coordinator (decoded FT8 messages) -> **pancetta-qso** -> `pancetta` coordinator (TX decisions)
//!
//! ## Key Types
//! - [`AutonomousOperator`] -- decision engine: hunt rare DX, answer CQ callers, hybrid mode
//! - [`PriorityScorer`] -- weighted scorer (needed DXCC > needed grid > POTA/SOTA > rarity)
//! - [`SmartFrequencyAllocator`] -- selects TX audio frequency for parallel QSOs
//! - [`QsoManager`] -- QSO lifecycle state machine (calling -> exchanging -> logging)
//! - [`QsoManagerConfig`] -- operator callsign, grid, autonomous mode settings
//!
//! ## Crate Relationships
//! - Receives from: `pancetta` coordinator (decoded messages, band state)
//! - Sends to: `pancetta` coordinator (TX decisions, logged QSOs)
//!
//! ## Features
//!
//! - **QSO State Machine**: Complete FT8 QSO flow management with automatic state transitions
//! - **ADIF 3.0 Support**: Full ADIF import/export with validation and conversion
//! - **SQLite Database**: Efficient storage with advanced querying and indexing
//! - **Comprehensive Logging**: Automatic logging with duplicate detection and validation
//! - **Statistics & Analytics**: Detailed QSO statistics, trends, and achievement tracking
//! - **Contest Support**: Contest-specific QSO handling and tracking
//! - **Message Exchange**: FT8 message parsing and generation with validation
//!
//! ## Quick Start
//!
//! ```rust
//! use pancetta_qso::*;
//! use chrono::Utc;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create QSO manager
//!     let config = QsoManagerConfig {
//!         our_callsign: "W1ABC".to_string(),
//!         our_grid: Some("FN42".to_string()),
//!         ..Default::default()
//!     };
//!     
//!     let qso_manager = QsoManager::new(config);
//!     qso_manager.start().await?;
//!     
//!     // Start a CQ call
//!     let qso_id = qso_manager.start_cq(14074000.0, None).await?;
//!     println!("Started CQ: {}", qso_id);
//!     
//!     // Get QSO status
//!     let progress = qso_manager.get_qso(qso_id).await?;
//!     println!("QSO state: {:?}", progress.state);
//!     
//!     Ok(())
//! }
//! ```
//!
//! ## Architecture
//!
//! The library is organized into several key modules:
//!
//! - [`states`]: Core QSO state definitions and transitions
//! - [`qso_manager`]: QSO lifecycle management and state machine
//! - [`exchange`]: FT8 message parsing and generation
//! - [`adif`]: ADIF 3.0 format support for import/export
//! - [`async_database`]: SQLite-based persistent storage
//! - [`async_logger`]: QSO logging with automatic features
//! - [`statistics`]: Comprehensive statistics and analytics
//!
//! ## Usage Examples
//!
//! ### Basic QSO Management
//!
//! ```rust,ignore
//! use pancetta_qso::*;
//!
//! // Create and configure QSO manager
//! let config = QsoManagerConfig::default();
//! let manager = QsoManager::new(config);
//!
//! // Start CQ and handle responses
//! let qso_id = manager.start_cq(14074000.0, None).await?;
//!
//! // Process incoming messages
//! manager.process_message(
//!     MessageType::CqResponse {
//!         calling_station: "W1ABC".to_string(),
//!         responding_station: "K1DEF".to_string(),
//!         grid: Some("FN31".to_string()),
//!     },
//!     "W1ABC K1DEF FN31".to_string(),
//!     14074000.0,
//!     Some(-12.0),
//! ).await?;
//! ```
//!
//! ### ADIF Import/Export
//!
//! Use [`AsyncQsoLogger`] to export logged QSOs to ADIF or import from an existing log file.
//! See [`async_logger`] for the full API, including `export_adif` and `import_adif`.
//!
//! ### Statistics and Analytics
//!
//! Use [`AsyncQsoDatabase`] to query the SQLite log and [`StatisticsCalculator`] to derive
//! per-band, per-DXCC, and time-series metrics. See [`statistics`] for details.

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![deny(unsafe_code)]
#![allow(dead_code, unused_imports)]

// Re-export all public types and functions for easy access
pub use crate::adif::*;
pub use crate::adif_log_writer::{AdifLogError, AdifLogResult, AdifLogWriter};
pub use crate::async_database::{
    AsyncQsoDatabase, DatabaseStats, DateRange, FrequencyRange, QslStatus, QsoDatabaseRecord,
    QsoFilter, QueryOptions, SortField, SortOrder,
};
pub use crate::async_logger::{
    AsyncQsoLogger, AutoLoggingConfig, BackupConfig, ExportFormat, ExportImportConfig,
    ExportResult, ImportResult, IntegrationConfig, LoggerConfig, ValidationConfig,
};
pub use crate::autonomous::*;
pub use crate::exchange::*;
pub use crate::frequency::*;
pub use crate::priority::*;
pub use crate::qso_manager::*;
pub use crate::states::*;
pub use crate::statistics::*;

// Module declarations
pub mod adif;
pub mod adif_log_writer;
pub mod async_database;
pub mod async_logger;
pub mod autonomous;
pub mod exchange;
pub mod frequency;
pub mod priority;
pub mod qso_manager;
pub mod states;
pub mod statistics;

pub mod callsign_continuity;
pub use callsign_continuity::{build_filter, CallsignContinuityFilter};

pub mod content_score;
pub use content_score::{content_score_from_features, ContentFeatures, MessageContentScore};

pub mod cross_time_state;
// Note: `QsoState`/`QsoPhase` live inside the `cross_time_state` module
// rather than being re-exported, to avoid clashing with the existing
// `states::QsoState` (the QSO-lifecycle state machine).
pub use cross_time_state::{
    A7ExpectedCall, A7RecentCallTable, CallsignDtHistory, CrossTimeState, DecodeRecord, DtPrior,
    DtSighting, QsoKey, WithinQsoContext,
};

pub mod cross_sequence;
pub use cross_sequence::{
    A7SeedEntry, CrossSequenceCallCache, DEFAULT_CAPACITY as CROSS_SEQUENCE_DEFAULT_CAPACITY,
    DEFAULT_MAX_AGE_SLOTS as CROSS_SEQUENCE_DEFAULT_MAX_AGE_SLOTS,
    SLOT_DURATION_SECS as CROSS_SEQUENCE_SLOT_DURATION_SECS,
};

pub mod fdr;
pub use fdr::{should_reject as fdr_should_reject, FdrFeatures, FdrLevel, MessageCategory};

pub mod sim;

// Common error type for the entire library
use crate::async_database::AsyncDatabaseError;
use crate::async_logger::AsyncLoggerError;
use thiserror::Error;

/// Common error types for the QSO library
#[derive(Debug, Error)]
pub enum QsoError {
    /// QSO manager error
    #[error("QSO manager error: {source}")]
    Manager {
        #[from]
        source: QsoManagerError,
    },

    /// Message exchange error
    #[error("Message exchange error: {source}")]
    Exchange {
        #[from]
        source: ExchangeError,
    },

    /// ADIF processing error
    #[error("ADIF error: {source}")]
    Adif {
        #[from]
        source: AdifError,
    },

    /// Database error
    #[error("Database error: {source}")]
    Database {
        #[from]
        source: AsyncDatabaseError,
    },

    /// Logging error
    #[error("Logging error: {source}")]
    Logger {
        #[from]
        source: AsyncLoggerError,
    },

    /// Statistics error
    #[error("Statistics error: {source}")]
    Statistics {
        #[from]
        source: StatisticsError,
    },

    /// Autonomous operator error
    #[error("Autonomous operator error: {source}")]
    Autonomous {
        #[from]
        source: AutonomousError,
    },
}

/// Result type for QSO operations
pub type QsoResult<T> = Result<T, QsoError>;

/// Library version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Library name
pub const NAME: &str = env!("CARGO_PKG_NAME");

/// Library description
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// High-level QSO system builder for easy setup
pub struct QsoSystemBuilder {
    qso_config: Option<QsoManagerConfig>,
    logger_config: Option<LoggerConfig>,
    enable_logger: bool,
}

impl QsoSystemBuilder {
    /// Create a new QSO system builder
    pub fn new() -> Self {
        Self {
            qso_config: None,
            logger_config: None,
            enable_logger: false,
        }
    }

    /// Set QSO manager configuration
    pub fn with_qso_config(mut self, config: QsoManagerConfig) -> Self {
        self.qso_config = Some(config);
        self
    }

    /// Set logger configuration and enable it
    pub fn with_logger(mut self, config: LoggerConfig) -> Self {
        self.logger_config = Some(config);
        self.enable_logger = true;
        self
    }

    /// Enable logger with default configuration
    pub fn enable_logger(mut self) -> Self {
        self.enable_logger = true;
        self
    }

    /// Build the complete QSO system
    pub async fn build(self) -> QsoResult<QsoSystem> {
        let qso_config = self.qso_config.unwrap_or_default();
        let qso_manager = QsoManager::new(qso_config);
        qso_manager.start().await?;

        let logger = if self.enable_logger {
            let logger_config = self.logger_config.unwrap_or_default();
            let qso_logger =
                std::sync::Arc::new(AsyncQsoLogger::new(logger_config, qso_manager.clone()).await?);
            qso_logger.start().await?;
            Some(qso_logger)
        } else {
            None
        };

        Ok(QsoSystem {
            qso_manager,
            logger,
        })
    }
}

impl Default for QsoSystemBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete QSO system with all components
pub struct QsoSystem {
    /// QSO manager instance
    pub qso_manager: QsoManager,

    /// Logger instance (if enabled)
    pub logger: Option<std::sync::Arc<AsyncQsoLogger>>,
}

impl QsoSystem {
    /// Create a new QSO system with default configuration
    pub async fn new(our_callsign: String, our_grid: Option<String>) -> QsoResult<Self> {
        let qso_config = QsoManagerConfig {
            our_callsign,
            our_grid,
            ..Default::default()
        };

        QsoSystemBuilder::new()
            .with_qso_config(qso_config)
            .build()
            .await
    }

    /// Create a new QSO system with logging enabled
    pub async fn with_logging(
        our_callsign: String,
        our_grid: Option<String>,
        db_path: Option<std::path::PathBuf>,
    ) -> QsoResult<Self> {
        let qso_config = QsoManagerConfig {
            our_callsign: our_callsign.clone(),
            our_grid,
            ..Default::default()
        };

        let logger_config = LoggerConfig {
            database_path: db_path.unwrap_or_else(|| "qso.db".into()),
            ..Default::default()
        };

        QsoSystemBuilder::new()
            .with_qso_config(qso_config)
            .with_logger(logger_config)
            .build()
            .await
    }

    /// Create a fully featured QSO system
    pub async fn full_featured(
        our_callsign: String,
        our_grid: Option<String>,
        db_path: Option<std::path::PathBuf>,
    ) -> QsoResult<Self> {
        let qso_config = QsoManagerConfig {
            our_callsign: our_callsign.clone(),
            our_grid,
            ..Default::default()
        };

        let logger_config = LoggerConfig {
            database_path: db_path.unwrap_or_else(|| "qso.db".into()),
            ..Default::default()
        };

        QsoSystemBuilder::new()
            .with_qso_config(qso_config)
            .with_logger(logger_config)
            .build()
            .await
    }

    /// Start a CQ call
    pub async fn start_cq(&self, frequency: f64) -> QsoResult<QsoId> {
        Ok(self.qso_manager.start_cq(frequency, None).await?)
    }

    /// Respond to a CQ call
    pub async fn respond_to_cq(&self, callsign: String, frequency: f64) -> QsoResult<QsoId> {
        Ok(self
            .qso_manager
            .respond_to_cq(callsign, frequency, None)
            .await?)
    }

    /// Process an incoming message
    pub async fn process_message(
        &self,
        message_type: MessageType,
        raw_text: String,
        frequency: f64,
        signal_strength: Option<f32>,
    ) -> QsoResult<()> {
        Ok(self
            .qso_manager
            .process_message(message_type, raw_text, frequency, signal_strength)
            .await?)
    }

    /// Get QSO status
    pub async fn get_qso(&self, qso_id: QsoId) -> QsoResult<QsoProgress> {
        Ok(self.qso_manager.get_qso(qso_id).await?)
    }

    /// Get all active QSOs
    pub async fn get_active_qsos(&self) -> Vec<(QsoId, QsoProgress)> {
        self.qso_manager.get_active_qsos().await
    }

    /// Cancel a QSO
    pub async fn cancel_qso(&self, qso_id: QsoId) -> QsoResult<()> {
        Ok(self.qso_manager.cancel_qso(qso_id).await?)
    }

    /// Get comprehensive statistics (requires logger)
    pub async fn get_statistics(&self) -> QsoResult<Option<crate::statistics::QsoStatistics>> {
        if let Some(ref logger) = self.logger {
            let _db_stats = logger.get_statistics().await?;
            // Convert database stats to comprehensive statistics would require access to the database
            // For now, return None if logger is not available with statistics calculator
            Ok(None)
        } else {
            Ok(None)
        }
    }

    /// Export QSOs to ADIF (requires logger)
    pub async fn export_adif<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        filter: Option<&QsoFilter>,
    ) -> QsoResult<Option<ExportResult>> {
        if let Some(ref logger) = self.logger {
            Ok(Some(logger.export_adif(path, filter).await?))
        } else {
            Ok(None)
        }
    }

    /// Import QSOs from ADIF (requires logger)
    pub async fn import_adif<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> QsoResult<Option<ImportResult>> {
        if let Some(ref logger) = self.logger {
            Ok(Some(logger.import_adif(path).await?))
        } else {
            Ok(None)
        }
    }
}

/// Utility functions for common operations
pub mod utils {
    use super::*;

    /// Parse an FT8 message string into a MessageType
    pub fn parse_ft8_message(
        message: &str,
        our_callsign: &str,
    ) -> Result<MessageType, ExchangeError> {
        let exchange = MessageExchange::new(our_callsign.to_string());
        exchange.parse_message(message)
    }

    /// Generate an FT8 message string from a MessageType
    pub fn generate_ft8_message(
        message_type: &MessageType,
        our_callsign: &str,
    ) -> Result<String, ExchangeError> {
        let exchange = MessageExchange::new(our_callsign.to_string());
        exchange.generate_message(message_type)
    }

    /// Calculate signal report from signal strength and noise floor
    pub fn calculate_signal_report(signal_strength: f32, noise_floor: f32) -> SignalReport {
        let exchange = MessageExchange::new("".to_string());
        exchange.calculate_signal_report(signal_strength, noise_floor)
    }

    /// Validate a callsign format
    pub fn validate_callsign(callsign: &str) -> bool {
        let exchange = MessageExchange::new("".to_string());
        exchange.validate_callsign(callsign).is_ok()
    }

    /// Validate a grid square format
    pub fn validate_grid_square(grid: &str) -> bool {
        let exchange = MessageExchange::new("".to_string());
        exchange.validate_grid(grid).is_ok()
    }

    /// Convert frequency in Hz to band designation
    pub fn frequency_to_band(frequency_hz: f64) -> String {
        let processor = AdifProcessor::new();
        processor.frequency_to_band(frequency_hz)
    }

    /// Get library version information
    pub fn version_info() -> VersionInfo {
        VersionInfo {
            version: VERSION.to_string(),
            name: NAME.to_string(),
            description: DESCRIPTION.to_string(),
        }
    }
}

/// Version information structure
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Library version
    pub version: String,

    /// Library name
    pub name: String,

    /// Library description
    pub description: String,
}

/// Re-export commonly used external types for convenience
pub use chrono::{DateTime, Utc};
pub use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_qso_system_creation() {
        let system = QsoSystem::new("W1ABC".to_string(), Some("FN42".to_string()))
            .await
            .unwrap();

        assert_eq!(system.qso_manager.config().our_callsign, "W1ABC");
        assert_eq!(
            system.qso_manager.config().our_grid,
            Some("FN42".to_string())
        );
        assert!(system.logger.is_none());
    }

    #[tokio::test]
    async fn test_qso_system_builder() {
        let qso_config = QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            ..Default::default()
        };

        let system = QsoSystemBuilder::new()
            .with_qso_config(qso_config)
            .build()
            .await
            .unwrap();

        assert_eq!(system.qso_manager.config().our_callsign, "W1ABC");
        assert!(system.logger.is_none());
    }

    #[tokio::test]
    async fn test_basic_qso_operations() {
        let system = QsoSystem::new("W1ABC".to_string(), Some("FN42".to_string()))
            .await
            .unwrap();

        // Start a CQ call
        let qso_id = system.start_cq(14074000.0).await.unwrap();

        // Get QSO status
        let progress = system.get_qso(qso_id).await.unwrap();
        assert!(matches!(progress.state, QsoState::CallingCq { .. }));

        // Get active QSOs
        let active = system.get_active_qsos().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, qso_id);

        // Cancel QSO
        system.cancel_qso(qso_id).await.unwrap();
    }

    #[test]
    fn test_utility_functions() {
        // Test callsign validation
        assert!(utils::validate_callsign("W1ABC"));
        assert!(utils::validate_callsign("K1DEF"));
        assert!(!utils::validate_callsign("123"));
        assert!(!utils::validate_callsign(""));

        // Test grid square validation
        assert!(utils::validate_grid_square("FN42"));
        assert!(utils::validate_grid_square("FN42AB"));
        assert!(!utils::validate_grid_square("ZZ99"));
        assert!(!utils::validate_grid_square(""));

        // Test frequency to band conversion
        assert_eq!(utils::frequency_to_band(14074000.0), "20M");
        assert_eq!(utils::frequency_to_band(7074000.0), "40M");

        // Test signal report calculation
        let report = utils::calculate_signal_report(-10.0, -25.0);
        assert_eq!(report, 15); // 15 dB SNR

        // Test version info
        let version = utils::version_info();
        assert!(!version.version.is_empty());
        assert!(!version.name.is_empty());
    }

    #[test]
    fn test_message_parsing() {
        let message_type = utils::parse_ft8_message("CQ W1ABC FN42", "W1ABC").unwrap();

        if let MessageType::Cq { callsign, grid } = message_type {
            assert_eq!(callsign, "W1ABC");
            assert_eq!(grid, Some("FN42".to_string()));
        } else {
            panic!("Expected CQ message");
        }
    }

    #[test]
    fn test_message_generation() {
        let message_type = MessageType::Cq {
            callsign: "W1ABC".to_string(),
            grid: Some("FN42".to_string()),
        };

        let generated = utils::generate_ft8_message(&message_type, "W1ABC").unwrap();
        assert_eq!(generated, "CQ W1ABC FN42");
    }

    #[test]
    fn test_version_constants() {
        assert!(!VERSION.is_empty());
        assert!(!NAME.is_empty());
        assert!(!DESCRIPTION.is_empty());
        assert_eq!(NAME, "pancetta-qso");
    }
}
