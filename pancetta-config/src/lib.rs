//! # pancetta-config
//!
//! TOML-based configuration with hot-reload — used by all crates at startup.
//!
//! A comprehensive configuration management system for Pancetta amateur radio software
//! that provides hierarchical configuration loading, hot-reload capability, and
//! structured settings management.
//!
//! ## Data Flow
//! config file (TOML) -> **pancetta-config** -> every other crate (read at startup / on file change)
//!
//! ## Key Types
//! - [`Config`] -- top-level configuration struct (station, audio, rig, network, UI)
//! - [`ConfigManager`] -- loads configuration and broadcasts hot-reload events
//! - [`ConfigError`] -- configuration-specific error type
//! - [`StationConfig`] -- callsign, grid square, operating preferences
//! - [`AudioConfig`] -- input/output device selection, sample rates
//!
//! ## Crate Relationships
//! - Receives from: filesystem (TOML config files)
//! - Sends to: all crates (`pancetta-audio`, `pancetta-ft8`, `pancetta-dsp`,
//!   `pancetta-qso`, `pancetta-hamlib`, `pancetta-dx`, `pancetta-cqdx`, `pancetta`)
//!
//! ## Features
//!
//! - **Hierarchical Configuration**: Merges configuration from defaults → system → user → CLI
//! - **Hot Reload**: Automatically reloads configuration when files change
//! - **Type Safety**: Strongly typed configuration structures with validation
//! - **TOML Format**: Human-readable configuration files
//! - **Modular Design**: Separate modules for different configuration domains
//!
//! ## Usage
//!
//! ```rust,ignore
//! use pancetta_config::{Config, ConfigManager, ConfigError};
//!
//! // Load configuration with defaults
//! let config = Config::load_default()?;
//!
//! // Create a configuration manager with hot-reload
//! let mut manager = ConfigManager::new()?;
//! manager.start_watching()?;
//!
//! // Access configuration sections
//! println!("Callsign: {}", config.station.callsign);
//! println!("Audio device: {}", config.audio.input_device);
//! ```

#![allow(dead_code, unused_imports)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tracing::{debug, info};

// Re-export all configuration modules
pub mod audio;
pub mod autonomous;
pub mod hot_reload;
pub mod loader;
pub mod network;
pub mod rig;
pub mod station;
pub mod ui;

pub use audio::*;
pub use autonomous::*;
pub use loader::*;
pub use network::*;
pub use rig::*;
pub use station::*;
pub use ui::*;

/// Configuration error types
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parsing error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("File watcher error: {0}")]
    Watcher(#[from] notify::Error),

    #[error("Configuration file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Invalid configuration value: {field} = {value}")]
    InvalidValue { field: String, value: String },

    #[error("Missing required configuration: {0}")]
    MissingRequired(String),
}

/// Result type for configuration operations
pub type ConfigResult<T> = Result<T, ConfigError>;

/// Main configuration structure that combines all settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Station configuration (callsign, grid, power)
    pub station: StationConfig,

    /// Audio device and processing settings
    pub audio: AudioConfig,

    /// Rig control and interface settings
    pub rig: RigConfig,

    /// User interface preferences
    pub ui: UiConfig,

    /// Network services configuration
    pub network: NetworkConfig,

    /// Autonomous operator configuration
    #[serde(default)]
    pub autonomous: AutonomousConfig,

    /// Metadata about the configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ConfigMetadata>,
}

/// Configuration metadata for tracking and debugging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigMetadata {
    /// Configuration schema version
    pub version: String,

    /// When this configuration was last modified
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,

    /// Source files that contributed to this configuration
    pub sources: Vec<PathBuf>,

    /// Unique identifier for this configuration instance
    pub instance_id: uuid::Uuid,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            station: StationConfig::default(),
            audio: AudioConfig::default(),
            rig: RigConfig::default(),
            ui: UiConfig::default(),
            network: NetworkConfig::default(),
            autonomous: AutonomousConfig::default(),
            metadata: Some(ConfigMetadata {
                version: "1.0".to_string(),
                last_modified: Some(chrono::Utc::now()),
                sources: vec![],
                instance_id: uuid::Uuid::new_v4(),
            }),
        }
    }
}

impl Config {
    /// Load configuration using the default search paths and hierarchy
    pub fn load_default() -> ConfigResult<Self> {
        let loader = ConfigLoader::new()?;
        loader.load()
    }

    /// Load configuration from a specific file
    pub fn load_from_file<P: AsRef<std::path::Path>>(path: P) -> ConfigResult<Self> {
        let loader = ConfigLoader::new()?;
        loader.load_from_file(path)
    }

    /// Validate the entire configuration
    pub fn validate(&self) -> ConfigResult<()> {
        debug!("Validating configuration");

        // Validate each section
        self.station.validate_section()?;
        self.audio.validate_section()?;
        self.rig.validate_section()?;
        self.ui.validate_section()?;
        self.network.validate_section()?;
        self.autonomous.validate_section()?;

        info!("Configuration validation successful");
        Ok(())
    }

    /// Merge this configuration with another, with the other taking precedence
    pub fn merge_with(&mut self, other: Config) {
        debug!("Merging configuration");

        self.station.merge_with(other.station);
        self.audio.merge_with(other.audio);
        self.rig.merge_with(other.rig);
        self.ui.merge_with(other.ui);
        self.network.merge_with(other.network);
        self.autonomous.merge_with(other.autonomous);

        // Update metadata
        if let Some(ref mut metadata) = self.metadata {
            metadata.last_modified = Some(chrono::Utc::now());
            if let Some(other_metadata) = other.metadata {
                metadata.sources.extend(other_metadata.sources);
            }
        }
    }

    /// Save configuration to a file in TOML format
    pub fn save_to_file<P: AsRef<std::path::Path>>(&self, path: P) -> ConfigResult<()> {
        let path = path.as_ref();
        debug!("Saving configuration to: {}", path.display());

        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Serialize to TOML
        let toml_string = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize config: {}", e)))?;

        // Write to file
        std::fs::write(path, toml_string)?;

        info!("Configuration saved to: {}", path.display());
        Ok(())
    }

    /// Get a summary of the current configuration
    pub fn summary(&self) -> String {
        format!(
            "Pancetta Configuration Summary:\n\
             - Station: {} ({})\n\
             - Audio: {} → {}\n\
             - Rig: {} @ {}\n\
             - UI: {} theme, {} layout\n\
             - Network: PSKReporter={}, QRZ={}",
            self.station.callsign,
            self.station.grid_square,
            self.audio.input_device,
            self.audio.output_device,
            self.rig.model,
            self.rig.interface.port,
            self.ui.theme,
            self.ui.layout,
            if self.network.psk_reporter.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if self.network.qrz.enabled {
                "enabled"
            } else {
                "disabled"
            }
        )
    }
}

/// Trait for configuration sections that can be merged and validated
pub trait ConfigSection: Default + Clone {
    /// Validate this configuration section
    fn validate_section(&self) -> ConfigResult<()> {
        Ok(())
    }

    /// Merge this section with another, with the other taking precedence
    fn merge_with(&mut self, other: Self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.station.callsign, "N0CALL");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.station.callsign, deserialized.station.callsign);
    }

    #[test]
    fn test_config_save_load() {
        let config = Config::default();
        let temp_file = NamedTempFile::new().unwrap();

        // Save configuration
        config.save_to_file(temp_file.path()).unwrap();

        // Load configuration
        let loaded_config = Config::load_from_file(temp_file.path()).unwrap();
        assert_eq!(config.station.callsign, loaded_config.station.callsign);
    }

    #[test]
    fn test_config_merge() {
        let mut config1 = Config::default();
        let mut config2 = Config::default();

        config2.station.callsign = "K1ABC".to_string();
        config2.station.power_watts = 50;

        config1.merge_with(config2);

        assert_eq!(config1.station.callsign, "K1ABC");
        assert_eq!(config1.station.power_watts, 50);
    }

    #[test]
    fn test_config_summary() {
        let config = Config::default();
        let summary = config.summary();
        assert!(summary.contains("N0CALL"));
        assert!(summary.contains("Pancetta Configuration Summary"));
    }
}
