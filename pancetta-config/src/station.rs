//! Station configuration module
//!
//! This module handles amateur radio station settings including callsign,
//! grid square locator, power levels, and station identification.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use validator::Validate;

/// Station configuration settings
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
pub struct StationConfig {
    /// Amateur radio callsign (e.g., "N1ABC", "VK2DEF/P")
    #[validate(length(min = 3, max = 20))]
    pub callsign: String,

    /// Maidenhead grid square locator (e.g., "FN31pr", "JO65cx")
    #[validate(length(min = 4, max = 8))]
    pub grid_square: String,

    /// Transmitter power in watts
    #[validate(range(min = 1, max = 1500))]
    pub power_watts: u32,

    /// QTH (location) description
    pub qth: String,

    /// DXCC entity code (ITU country code)
    #[validate(range(min = 1, max = 999))]
    pub dxcc_entity: u16,

    /// ITU zone number
    #[validate(range(min = 1, max = 90))]
    pub itu_zone: u8,

    /// CQ zone number
    #[validate(range(min = 1, max = 40))]
    pub cq_zone: u8,

    /// Station coordinates for accurate distance calculations
    pub coordinates: Option<Coordinates>,

    /// Antenna information
    pub antennas: Vec<AntennaConfig>,

    /// Additional station information
    pub operator_name: Option<String>,

    /// Contest station category (for contest logging)
    pub contest_category: Option<String>,

    /// Custom fields for extensibility
    #[serde(default)]
    pub custom_fields: HashMap<String, String>,

    /// Maximum latency past the slot boundary at which we still attempt
    /// late-start TX via audio skip-ahead. Beyond this, defer to the
    /// next opposite-parity slot. Default 8000ms — leaves ~5s of audio
    /// on the air with two of three Costas sync arrays still in window.
    #[serde(default = "default_tx_late_max_ms")]
    pub tx_late_max_ms: u64,

    /// When calling CQ (no DX context), prefer this parity. `Auto`
    /// (default) lets the scheduler pick whichever next slot is closer.
    #[serde(default)]
    pub tx_self_parity: TxSelfParity,

    /// PTT engage lead time before slot boundary, in milliseconds.
    /// Default 80ms — enough for solid-state keying. Bump up for slow
    /// mechanical relays.
    #[serde(default = "default_ptt_lead_ms")]
    pub ptt_lead_ms: u64,
}

fn default_tx_late_max_ms() -> u64 {
    8000
}

fn default_ptt_lead_ms() -> u64 {
    80
}

/// Self-parity preference when calling CQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TxSelfParity {
    /// Pick whichever next slot is closer, regardless of parity.
    #[default]
    Auto,
    /// Lock CQ to even slots (`:00`, `:30`).
    Even,
    /// Lock CQ to odd slots (`:15`, `:45`).
    Odd,
}

/// Geographic coordinates for the station
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coordinates {
    /// Latitude in decimal degrees (-90.0 to 90.0)
    pub latitude: f64,

    /// Longitude in decimal degrees (-180.0 to 180.0)
    pub longitude: f64,

    /// Elevation above sea level in meters
    pub elevation_meters: Option<f64>,
}

/// Antenna configuration for the station
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntennaConfig {
    /// Unique identifier for this antenna
    pub id: String,

    /// Human-readable name/description
    pub name: String,

    /// Antenna type (e.g., "dipole", "yagi", "vertical")
    pub antenna_type: String,

    /// Frequency bands this antenna covers
    pub bands: Vec<String>,

    /// Antenna gain in dBi (optional)
    pub gain_dbi: Option<f64>,

    /// Antenna pattern (omnidirectional, directional, etc.)
    pub pattern: AntennaPattern,

    /// Height above ground in meters
    pub height_meters: Option<f64>,

    /// Azimuth direction for directional antennas (degrees)
    pub azimuth_degrees: Option<u16>,

    /// Whether this antenna is currently active
    pub active: bool,
}

/// Antenna radiation pattern types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntennaPattern {
    /// Radiates equally in all directions
    Omnidirectional,

    /// Highly directional (e.g., yagi, dish)
    Directional,

    /// Somewhat directional (e.g., loop, quad)
    SemiDirectional,

    /// Bidirectional pattern (e.g., dipole broadside)
    Bidirectional,
}

impl Default for StationConfig {
    fn default() -> Self {
        Self {
            callsign: "N0CALL".to_string(),
            grid_square: "AA00aa".to_string(),
            power_watts: 100,
            qth: "Unknown".to_string(),
            dxcc_entity: 291, // United States
            itu_zone: 8,      // ITU Region 2, Zone 8 (US East Coast)
            cq_zone: 5,       // CQ Zone 5 (US East Coast)
            coordinates: None,
            antennas: vec![AntennaConfig {
                id: "default".to_string(),
                name: "Default Antenna".to_string(),
                antenna_type: "dipole".to_string(),
                bands: vec!["40m".to_string(), "20m".to_string()],
                gain_dbi: Some(2.15), // Typical dipole gain
                pattern: AntennaPattern::Bidirectional,
                height_meters: Some(10.0),
                azimuth_degrees: None,
                active: true,
            }],
            operator_name: None,
            contest_category: None,
            custom_fields: HashMap::new(),
            tx_late_max_ms: 8000,
            tx_self_parity: TxSelfParity::Auto,
            ptt_lead_ms: 80,
        }
    }
}

impl ConfigSection for StationConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // Perform validation using the validator crate
        use validator::Validate;
        Validate::validate(self)
            .map_err(|e| ConfigError::Validation(format!("Station validation failed: {}", e)))?;

        // Additional custom validation
        self.validate_callsign()?;
        self.validate_grid_square()?;
        self.validate_coordinates()?;
        self.validate_antennas()?;

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // Always take the other value — the layered config system ensures
        // higher-priority sources override lower ones. We only skip truly
        // empty/unset values (empty strings, zero numerics, None).
        if !other.callsign.is_empty() {
            self.callsign = other.callsign;
        }

        if !other.grid_square.is_empty() {
            self.grid_square = other.grid_square;
        }

        if other.power_watts != 0 {
            self.power_watts = other.power_watts;
        }

        if !other.qth.is_empty() {
            self.qth = other.qth;
        }

        if other.dxcc_entity != 0 {
            self.dxcc_entity = other.dxcc_entity;
        }

        if other.itu_zone != 0 {
            self.itu_zone = other.itu_zone;
        }

        if other.cq_zone != 0 {
            self.cq_zone = other.cq_zone;
        }

        if other.coordinates.is_some() {
            self.coordinates = other.coordinates;
        }

        if !other.antennas.is_empty() {
            self.antennas = other.antennas;
        }

        if other.operator_name.is_some() {
            self.operator_name = other.operator_name;
        }

        if other.contest_category.is_some() {
            self.contest_category = other.contest_category;
        }

        // Merge custom fields
        self.custom_fields.extend(other.custom_fields);
    }
}

impl StationConfig {
    /// Validate the callsign format and characters
    fn validate_callsign(&self) -> ConfigResult<()> {
        let callsign = &self.callsign;

        // Check for valid characters (alphanumeric and slash)
        if !callsign.chars().all(|c| c.is_alphanumeric() || c == '/') {
            return Err(ConfigError::InvalidValue {
                field: "callsign".to_string(),
                value: callsign.clone(),
            });
        }

        // Must contain at least one letter and one number
        let has_letter = callsign.chars().any(|c| c.is_alphabetic());
        let has_number = callsign.chars().any(|c| c.is_numeric());

        if !has_letter || !has_number {
            return Err(ConfigError::InvalidValue {
                field: "callsign".to_string(),
                value: callsign.clone(),
            });
        }

        Ok(())
    }

    /// Validate the Maidenhead grid square format
    fn validate_grid_square(&self) -> ConfigResult<()> {
        let grid = &self.grid_square;

        // Grid square must be 4, 6, or 8 characters
        if ![4, 6, 8].contains(&grid.len()) {
            return Err(ConfigError::InvalidValue {
                field: "grid_square".to_string(),
                value: grid.clone(),
            });
        }

        // Validate format: LLNNLLNN (letters, numbers, letters, numbers)
        let chars: Vec<char> = grid.chars().collect();

        // First two characters must be letters (A-R)
        if chars.len() >= 2 {
            if !chars[0].is_ascii_uppercase()
                || !chars[1].is_ascii_uppercase()
                || chars[0] < 'A'
                || chars[0] > 'R'
                || chars[1] < 'A'
                || chars[1] > 'R'
            {
                return Err(ConfigError::InvalidValue {
                    field: "grid_square".to_string(),
                    value: grid.clone(),
                });
            }
        }

        // Next two characters must be digits (0-9)
        if chars.len() >= 4 {
            if !chars[2].is_ascii_digit() || !chars[3].is_ascii_digit() {
                return Err(ConfigError::InvalidValue {
                    field: "grid_square".to_string(),
                    value: grid.clone(),
                });
            }
        }

        // If 6 or 8 characters, next two must be lowercase letters (a-x)
        if chars.len() >= 6 {
            if !chars[4].is_ascii_lowercase()
                || !chars[5].is_ascii_lowercase()
                || chars[4] < 'a'
                || chars[4] > 'x'
                || chars[5] < 'a'
                || chars[5] > 'x'
            {
                return Err(ConfigError::InvalidValue {
                    field: "grid_square".to_string(),
                    value: grid.clone(),
                });
            }
        }

        // If 8 characters, last two must be digits (0-9)
        if chars.len() == 8 {
            if !chars[6].is_ascii_digit() || !chars[7].is_ascii_digit() {
                return Err(ConfigError::InvalidValue {
                    field: "grid_square".to_string(),
                    value: grid.clone(),
                });
            }
        }

        Ok(())
    }

    /// Validate coordinates if present
    fn validate_coordinates(&self) -> ConfigResult<()> {
        if let Some(coords) = &self.coordinates {
            if coords.latitude < -90.0 || coords.latitude > 90.0 {
                return Err(ConfigError::InvalidValue {
                    field: "coordinates.latitude".to_string(),
                    value: coords.latitude.to_string(),
                });
            }

            if coords.longitude < -180.0 || coords.longitude > 180.0 {
                return Err(ConfigError::InvalidValue {
                    field: "coordinates.longitude".to_string(),
                    value: coords.longitude.to_string(),
                });
            }
        }

        Ok(())
    }

    /// Validate antenna configurations
    fn validate_antennas(&self) -> ConfigResult<()> {
        // Check for unique antenna IDs
        let mut ids = std::collections::HashSet::new();
        for antenna in &self.antennas {
            if !ids.insert(&antenna.id) {
                return Err(ConfigError::InvalidValue {
                    field: "antennas.id".to_string(),
                    value: antenna.id.clone(),
                });
            }

            // Validate azimuth range
            if let Some(azimuth) = antenna.azimuth_degrees {
                if azimuth >= 360 {
                    return Err(ConfigError::InvalidValue {
                        field: format!("antennas.{}.azimuth_degrees", antenna.id),
                        value: azimuth.to_string(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Get the active antenna for a specific band
    pub fn get_active_antenna_for_band(&self, band: &str) -> Option<&AntennaConfig> {
        self.antennas
            .iter()
            .find(|antenna| antenna.active && antenna.bands.contains(&band.to_string()))
    }

    /// Get all active antennas
    pub fn get_active_antennas(&self) -> Vec<&AntennaConfig> {
        self.antennas
            .iter()
            .filter(|antenna| antenna.active)
            .collect()
    }
}

impl Coordinates {
    /// Calculate distance to another set of coordinates using the Haversine formula
    pub fn distance_to(&self, other: &Coordinates) -> f64 {
        const EARTH_RADIUS_KM: f64 = 6371.0;

        let lat1_rad = self.latitude.to_radians();
        let lat2_rad = other.latitude.to_radians();
        let delta_lat = (other.latitude - self.latitude).to_radians();
        let delta_lon = (other.longitude - self.longitude).to_radians();

        let a = (delta_lat / 2.0).sin().powi(2)
            + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);

        let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

        EARTH_RADIUS_KM * c
    }

    /// Calculate bearing to another set of coordinates
    pub fn bearing_to(&self, other: &Coordinates) -> f64 {
        let lat1_rad = self.latitude.to_radians();
        let lat2_rad = other.latitude.to_radians();
        let delta_lon = (other.longitude - self.longitude).to_radians();

        let y = delta_lon.sin() * lat2_rad.cos();
        let x = lat1_rad.cos() * lat2_rad.sin() - lat1_rad.sin() * lat2_rad.cos() * delta_lon.cos();

        let bearing_rad = y.atan2(x);
        (bearing_rad.to_degrees() + 360.0) % 360.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_station_config() {
        let config = StationConfig::default();
        assert_eq!(config.callsign, "N0CALL");
        assert_eq!(config.grid_square, "AA00aa");
        assert_eq!(config.power_watts, 100);
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_callsign_validation() {
        let mut config = StationConfig::default();

        // Valid callsigns
        config.callsign = "K1ABC".to_string();
        assert!(config.validate_callsign().is_ok());

        config.callsign = "VK2DEF/P".to_string();
        assert!(config.validate_callsign().is_ok());

        // Invalid callsigns
        config.callsign = "123".to_string(); // No letters
        assert!(config.validate_callsign().is_err());

        config.callsign = "ABC".to_string(); // No numbers
        assert!(config.validate_callsign().is_err());

        config.callsign = "K1@BC".to_string(); // Invalid character
        assert!(config.validate_callsign().is_err());
    }

    #[test]
    fn test_grid_square_validation() {
        let mut config = StationConfig::default();

        // Valid grid squares
        config.grid_square = "FN31".to_string();
        assert!(config.validate_grid_square().is_ok());

        config.grid_square = "FN31pr".to_string();
        assert!(config.validate_grid_square().is_ok());

        config.grid_square = "FN31pr59".to_string();
        assert!(config.validate_grid_square().is_ok());

        // Invalid grid squares
        config.grid_square = "fn31".to_string(); // Lowercase letters in wrong position
        assert!(config.validate_grid_square().is_err());

        config.grid_square = "F131".to_string(); // Number in letter position
        assert!(config.validate_grid_square().is_err());

        config.grid_square = "FNAB".to_string(); // Letter in number position
        assert!(config.validate_grid_square().is_err());
    }

    #[test]
    fn test_coordinates_distance() {
        let coord1 = Coordinates {
            latitude: 40.7128,
            longitude: -74.0060,
            elevation_meters: None,
        };

        let coord2 = Coordinates {
            latitude: 34.0522,
            longitude: -118.2437,
            elevation_meters: None,
        };

        let distance = coord1.distance_to(&coord2);
        assert!((distance - 3944.0).abs() < 50.0); // Approximately 3944 km between NYC and LA
    }

    #[test]
    fn test_antenna_management() {
        let config = StationConfig::default();

        // Test getting active antennas
        let active_antennas = config.get_active_antennas();
        assert_eq!(active_antennas.len(), 1);

        // Test getting antenna for specific band
        let antenna_20m = config.get_active_antenna_for_band("20m");
        assert!(antenna_20m.is_some());

        let antenna_80m = config.get_active_antenna_for_band("80m");
        assert!(antenna_80m.is_none());
    }

    #[test]
    fn station_config_parses_new_tx_fields() {
        let toml = r#"
            callsign = "K5ARH"
            grid_square = "EM10"
            power_watts = 100
            qth = "Test"
            dxcc_entity = 291
            itu_zone = 8
            cq_zone = 4
            antennas = []
            tx_late_max_ms = 6000
            tx_self_parity = "even"
            ptt_lead_ms = 120
        "#;
        let cfg: StationConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.tx_late_max_ms, 6000);
        assert_eq!(cfg.tx_self_parity, TxSelfParity::Even);
        assert_eq!(cfg.ptt_lead_ms, 120);
    }

    #[test]
    fn station_config_defaults_when_new_tx_fields_absent() {
        let toml = r#"
            callsign = "K5ARH"
            grid_square = "EM10"
            power_watts = 100
            qth = "Test"
            dxcc_entity = 291
            itu_zone = 8
            cq_zone = 4
            antennas = []
        "#;
        let cfg: StationConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.tx_late_max_ms, 8000);
        assert_eq!(cfg.tx_self_parity, TxSelfParity::Auto);
        assert_eq!(cfg.ptt_lead_ms, 80);
    }
}
