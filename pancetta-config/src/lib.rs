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

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![allow(dead_code, unused_imports)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tracing::{debug, info};

// Re-export all configuration modules
pub mod audio;
pub mod autonomous;
pub mod fox;
pub mod hot_reload;
pub mod hound;
pub mod loader;
pub mod network;
pub mod rig;
pub mod station;
pub mod ui;

pub use audio::*;
pub use autonomous::*;
pub use fox::*;
pub use hound::*;
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

    /// An optional source that is intentionally inactive this run (e.g. the
    /// command-line source when CLI parsing lives in `main.rs`, or the
    /// environment source when no `PANCETTA_*` vars are set). NOT a parse
    /// failure — the loader skips it quietly rather than warning the operator.
    #[error("Configuration source skipped: {0}")]
    SourceSkipped(String),

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
    #[serde(default)]
    pub station: StationConfig,

    /// Audio device and processing settings
    #[serde(default)]
    pub audio: AudioConfig,

    /// Rig control and interface settings
    #[serde(default)]
    pub rig: RigConfig,

    /// User interface preferences
    #[serde(default)]
    pub ui: UiConfig,

    /// Network services configuration
    #[serde(default)]
    pub network: NetworkConfig,

    /// Autonomous operator configuration
    #[serde(default)]
    pub autonomous: AutonomousConfig,

    /// Hound (DXpedition chaser) audio-region configuration
    #[serde(default)]
    pub hound: hound::HoundConfig,

    /// Fox (DXpedition operator) configuration
    #[serde(default)]
    pub fox: fox::FoxConfig,

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
            hound: hound::HoundConfig::default(),
            fox: fox::FoxConfig::default(),
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

    /// Load configuration using the default search paths and hierarchy,
    /// additionally returning any non-fatal load warnings (e.g. a config file
    /// that existed but failed to parse and was skipped, silently reverting
    /// its settings to defaults). The caller should surface these to the
    /// operator so a partial/broken config is never invisible. A clean load
    /// returns an empty warnings vec.
    pub fn load_default_with_warnings() -> ConfigResult<(Self, Vec<String>)> {
        let loader = ConfigLoader::new()?;
        let config = loader.load()?;
        let warnings = loader.load_warnings();
        Ok((config, warnings))
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
        self.hound.validate_section()?;
        self.fox.validate_section()?;

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
        self.hound.merge_with(other.hound);
        self.fox.merge_with(other.fox);

        // Update metadata
        if let Some(ref mut metadata) = self.metadata {
            metadata.last_modified = Some(chrono::Utc::now());
            if let Some(other_metadata) = other.metadata {
                metadata.sources.extend(other_metadata.sources);
            }
        }
    }

    /// Write config text to `path` securely: owner-only permissions and
    /// atomically, so the plaintext credentials this file holds (logbook
    /// passwords, the cqdx PAT) are never group/other-readable and a crash
    /// mid-write can never truncate the operator's config.
    ///
    /// On Unix: the parent dir is forced to `0o700`, the file is created
    /// `0o600` from the first byte (via a `0o600` temp sibling), and the temp
    /// is atomically `rename`d over the target (rename preserves the temp's
    /// mode). This also closes the *silent-reversion* trap where an operator who
    /// hand-`chmod 600`'d the file lost it the next time pancetta rewrote it. On
    /// non-Unix the per-user NTFS ACL on the Windows MiniPC already restricts
    /// access; we still write atomically (temp + rename).
    fn write_secure_atomic(path: &std::path::Path, contents: &str) -> ConfigResult<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // Best-effort: lock the .pancetta dir to the owner. Ignore
                    // failure (e.g. the operator deliberately set a different
                    // mode, or a non-owned ancestor) — the 0600 file is the
                    // real guarantee.
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
        }

        let mut tmp_os = path.as_os_str().to_owned();
        tmp_os.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp_os);

        // Create the temp file owner-only from the outset so the secret is never
        // momentarily world-readable between create and chmod.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(contents.as_bytes())?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, contents)?;
        }

        // Atomic replace. On the rare rename failure, clean up the temp.
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Save configuration to a file in TOML format
    pub fn save_to_file<P: AsRef<std::path::Path>>(&self, path: P) -> ConfigResult<()> {
        let path = path.as_ref();
        debug!("Saving configuration to: {}", path.display());

        // Serialize to TOML
        let toml_string = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize config: {}", e)))?;

        // Write owner-only + atomically (this file holds plaintext credentials).
        Self::write_secure_atomic(path, &toml_string)?;

        info!("Configuration saved to: {}", path.display());
        Ok(())
    }

    /// Targeted persist of the audio input/output device names into an
    /// existing config TOML, without clobbering unrelated keys.
    ///
    /// Used by the TUI device picker: the operator chooses an output (and
    /// optionally input) device and we write just `[audio] output_device`
    /// / `input_device` back to `~/.pancetta/pancetta.toml`. We parse the
    /// file as a generic `toml::Table`, set the two keys under the `audio`
    /// table, and re-serialize — so every other section/value the operator
    /// has set is preserved verbatim. `None` arguments are left untouched.
    /// If the file does not yet exist, a minimal one is created containing
    /// only the `[audio]` section (the loader fills the rest from
    /// defaults).
    pub fn set_audio_devices_in_file<P: AsRef<std::path::Path>>(
        &self,
        path: P,
        input_device: Option<&str>,
        output_device: Option<&str>,
    ) -> ConfigResult<()> {
        let path = path.as_ref();

        // Load the existing document as a generic table, or start fresh.
        let mut root: toml::Table = match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .parse::<toml::Table>()
                .map_err(|e| ConfigError::Validation(format!("Failed to parse config: {}", e)))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e.into()),
        };

        // Ensure an [audio] table exists.
        let audio = root
            .entry("audio".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let audio_table = audio.as_table_mut().ok_or_else(|| {
            ConfigError::Validation("[audio] in config is not a table".to_string())
        })?;

        if let Some(out) = output_device {
            audio_table.insert(
                "output_device".to_string(),
                toml::Value::String(out.to_string()),
            );
        }
        if let Some(inp) = input_device {
            audio_table.insert(
                "input_device".to_string(),
                toml::Value::String(inp.to_string()),
            );
        }

        let serialized = toml::to_string_pretty(&root)
            .map_err(|e| ConfigError::Validation(format!("Failed to serialize config: {}", e)))?;
        // Owner-only + atomic — same guarantee as save_to_file, and critically
        // this path (the TUI device picker) previously re-wrote the file at
        // umask default, silently undoing any `chmod 600` the operator applied.
        Self::write_secure_atomic(path, &serialized)?;
        info!("Audio device selection persisted to: {}", path.display());
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
             - Network: PSKReporter={}, cqdx.io={}",
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
            if self.network.cqdx.enabled {
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

    /// The config file holds plaintext credentials, so save_to_file must write
    /// it owner-only (0600) and atomically, and must re-establish 0600 on a
    /// rewrite (the silent-reversion fix).
    #[cfg(unix)]
    #[test]
    fn save_to_file_is_owner_only_and_atomic() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("pancetta.toml");

        let config = Config::default();
        config.save_to_file(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config file must be owner-only, got {mode:o}");
        // Parent dir locked to the owner.
        let dmode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700, "config dir must be 0700, got {dmode:o}");
        // No leftover temp sibling.
        assert!(!path.with_file_name("pancetta.toml.tmp").exists());

        // Operator hardens further, then a rewrite must NOT loosen it back.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        config
            .set_audio_devices_in_file(&path, None, Some("Rig CODEC"))
            .unwrap();
        let mode2 = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o600, "rewrite must restore 0600, got {mode2:o}");
        // Round-trips and preserved the device write.
        let reloaded = Config::load_from_file(&path).unwrap();
        assert_eq!(reloaded.audio.output_device, "Rig CODEC");
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

    /// Targeted device persist writes output_device under [audio] and does
    /// not clobber unrelated keys the operator already set.
    #[test]
    fn set_audio_devices_in_file_writes_output_and_preserves_other_keys() {
        let temp = NamedTempFile::new().unwrap();
        // Pre-existing config with a custom station callsign + a custom
        // audio sample_rate that must survive the targeted write.
        std::fs::write(
            temp.path(),
            "[station]\ncallsign = \"K5ARH\"\n\n[audio]\nsample_rate = 48000\ninput_device = \"OldMic\"\n",
        )
        .unwrap();

        let config = Config::default();
        config
            .set_audio_devices_in_file(temp.path(), None, Some("USB Codec"))
            .unwrap();

        let written = std::fs::read_to_string(temp.path()).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let audio = parsed["audio"].as_table().unwrap();
        assert_eq!(
            audio["output_device"].as_str(),
            Some("USB Codec"),
            "output_device must be written"
        );
        // Unrelated keys preserved.
        assert_eq!(audio["sample_rate"].as_integer(), Some(48000));
        assert_eq!(audio["input_device"].as_str(), Some("OldMic"));
        assert_eq!(
            parsed["station"].as_table().unwrap()["callsign"].as_str(),
            Some("K5ARH")
        );
    }

    /// A partial config (only [station]) must merge OVER defaults rather than
    /// failing to deserialize and silently reverting EVERYTHING to defaults.
    /// This is the serde(default) fix: the missing sections fill from defaults
    /// while the present one keeps its values.
    #[test]
    fn partial_config_only_station_keeps_callsign_and_defaults_elsewhere() {
        let toml = "[station]\ncallsign = \"K5ARH\"\ngrid_square = \"EM00\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        // The present section is honored.
        assert_eq!(parsed.station.callsign, "K5ARH");
        assert_eq!(parsed.station.grid_square, "EM00");
        // Missing sections fall back to defaults (NOT a deserialize failure).
        let defaults = Config::default();
        assert_eq!(parsed.audio.sample_rate, defaults.audio.sample_rate);
        assert_eq!(parsed.rig.model, defaults.rig.model);
        assert_eq!(parsed.ui.theme, defaults.ui.theme);
    }

    /// An [audio]-only file (the device-picker persistence shape) must load —
    /// previously it failed because the other four sections were required,
    /// which broke launch after the picker wrote it.
    #[test]
    fn audio_only_config_loads_with_defaults_elsewhere() {
        let toml = "[audio]\noutput_device = \"USB Codec\"\ninput_device = \"USB Codec\"\n";
        let parsed: Config = toml::from_str(toml).expect("[audio]-only config must deserialize");
        assert_eq!(parsed.audio.output_device, "USB Codec");
        assert_eq!(parsed.audio.input_device, "USB Codec");
        // Default callsign etc. fill in — no panic, no required-field error.
        assert_eq!(parsed.station.callsign, Config::default().station.callsign);
    }

    /// Round-trip an [audio]-only file through the device-picker persist helper,
    /// then load it back as a full Config (relies on the serde-default fix).
    #[test]
    fn audio_only_file_roundtrips_through_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pancetta.toml");
        // Persist an [audio]-only file (no pre-existing file).
        Config::default()
            .set_audio_devices_in_file(&path, Some("Mic Z"), Some("Spk Z"))
            .unwrap();
        // The written file is [audio]-only...
        let raw = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = raw.parse().unwrap();
        assert!(table.contains_key("audio"));
        assert!(!table.contains_key("station"));
        // ...and it still loads into a full Config thanks to serde(default).
        let loaded = Config::load_from_file(&path).unwrap();
        assert_eq!(loaded.audio.output_device, "Spk Z");
        assert_eq!(loaded.audio.input_device, "Mic Z");
        assert_eq!(loaded.station.callsign, Config::default().station.callsign);
    }

    /// Writing into a non-existent file creates a minimal [audio] section.
    #[test]
    fn set_audio_devices_in_file_creates_minimal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("pancetta.toml");
        let config = Config::default();
        config
            .set_audio_devices_in_file(&path, Some("Mic X"), Some("Spk Y"))
            .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let audio = parsed["audio"].as_table().unwrap();
        assert_eq!(audio["output_device"].as_str(), Some("Spk Y"));
        assert_eq!(audio["input_device"].as_str(), Some("Mic X"));
    }
}
