//! Station configuration module
//!
//! This module handles amateur radio station settings including callsign,
//! grid square locator, power levels, and station identification.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Station configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
// Container-level serde default: any field omitted from a partial config file
// falls back to this struct's `Default` impl rather than failing to
// deserialize (which previously reverted the WHOLE config to defaults).
#[serde(default)]
pub struct StationConfig {
    /// Amateur radio callsign (e.g., "N1ABC", "VK2DEF/P"); 3 to 20 characters
    pub callsign: String,

    /// Maidenhead grid square locator (e.g., "FN31pr", "JO65cx"); 4 to 8 characters
    pub grid_square: String,

    /// Transmitter power in watts (1 to 1500)
    pub power_watts: u32,

    /// QTH (location) description
    pub qth: String,

    /// DXCC entity code (ITU country code, 1 to 999)
    pub dxcc_entity: u16,

    /// ITU zone number (1 to 90)
    pub itu_zone: u8,

    /// CQ zone number (1 to 40)
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
    /// next opposite-parity slot. Default 3000ms — keeps both the middle
    /// and end Costas sync arrays and 49 of 58 data symbols (147 coded
    /// bits, vs the 91 information bits LDPC(174,91) needs) on the air;
    /// anything past ~5940ms elapsed is undecodable in principle.
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
    3000
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
            tx_late_max_ms: 3000,
            tx_self_parity: TxSelfParity::Auto,
            ptt_lead_ms: 80,
        }
    }
}

impl ConfigSection for StationConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // Bounds first, all reported together, then the semantic checks.
        let bound_errors = self.bound_errors();
        if !bound_errors.is_empty() {
            return Err(ConfigError::Validation(format!(
                "Station validation failed: {}",
                bound_errors.join("\n")
            )));
        }

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

        if other.tx_late_max_ms != 0 {
            self.tx_late_max_ms = other.tx_late_max_ms;
        }

        self.tx_self_parity = other.tx_self_parity;

        if other.ptt_lead_ms != 0 {
            self.ptt_lead_ms = other.ptt_lead_ms;
        }

        // Merge custom fields
        self.custom_fields.extend(other.custom_fields);
    }
}

impl StationConfig {
    /// Inclusive length and range bounds, one diagnostic per failing field.
    ///
    /// PAN-315: these replace the validator 0.21 derive attributes and keep
    /// its `field: Validation error: code [params]` text, with `min`, `max`
    /// and `value` rendered as debug-formatted JSON values. Lengths count
    /// Unicode characters, as validator did.
    fn bound_errors(&self) -> Vec<String> {
        use serde_json::Value;
        let mut errors = Vec::new();
        let mut check =
            |field: &str, code: &str, measured: u64, min: u64, max: u64, value: Value| {
                if measured < min || measured > max {
                    let params = HashMap::from([
                        ("min", Value::from(min)),
                        ("max", Value::from(max)),
                        ("value", value),
                    ]);
                    errors.push(format!("{field}: Validation error: {code} [{params:?}]"));
                }
            };
        let callsign = self.callsign.as_str();
        check(
            "callsign",
            "length",
            callsign.chars().count() as u64,
            3,
            20,
            Value::from(callsign),
        );
        let grid = self.grid_square.as_str();
        check(
            "grid_square",
            "length",
            grid.chars().count() as u64,
            4,
            8,
            Value::from(grid),
        );
        let power = self.power_watts;
        check(
            "power_watts",
            "range",
            power.into(),
            1,
            1500,
            Value::from(power),
        );
        let dxcc = self.dxcc_entity;
        check(
            "dxcc_entity",
            "range",
            dxcc.into(),
            1,
            999,
            Value::from(dxcc),
        );
        let itu = self.itu_zone;
        check("itu_zone", "range", itu.into(), 1, 90, Value::from(itu));
        let cq = self.cq_zone;
        check("cq_zone", "range", cq.into(), 1, 40, Value::from(cq));
        errors
    }

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
    ///
    /// PAN-330: semantic validity (field letters A–R, square digits,
    /// subsquare A–X, extended digits) is owned by
    /// `pancetta_core::gridsquare::validate_grid`; this adapter pins the
    /// station context's 4/6/8-character precision and conventional
    /// presentation case (uppercase field/square, lowercase subsquare).
    fn validate_grid_square(&self) -> ConfigResult<()> {
        let grid = &self.grid_square;
        let invalid = || ConfigError::InvalidValue {
            field: "grid_square".to_string(),
            value: grid.clone(),
        };

        // Grid square must be 4, 6, or 8 characters
        if ![4, 6, 8].contains(&grid.len()) {
            return Err(invalid());
        }

        // Core semantic validation (pair ranges and structure).
        if pancetta_core::gridsquare::validate_grid(grid).is_err() {
            return Err(invalid());
        }

        // Presentation case: LLNN (uppercase), then lowercase subsquare pair
        // when 6 or 8 characters, then digits when 8.
        let chars: Vec<char> = grid.chars().collect();
        if !chars[0].is_ascii_uppercase() || !chars[1].is_ascii_uppercase() {
            return Err(invalid());
        }
        if chars.len() >= 6 && (!chars[4].is_ascii_lowercase() || !chars[5].is_ascii_lowercase()) {
            return Err(invalid());
        }
        if chars.len() == 8 && (!chars[6].is_ascii_digit() || !chars[7].is_ascii_digit()) {
            return Err(invalid());
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
// rationale: test-only builder structs assigned field-by-field after
// default(); sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    /// PAN-330 AC6: station grid validation delegates to core Maidenhead
    /// semantics, with this caller's 4/6/8 precision and conventional
    /// uppercase-field / lowercase-subsquare presentation pinned.
    #[test]
    fn station_grid_policy_table() {
        let mut cfg = StationConfig::default();
        for ok in ["FN42", "FN31pr", "AA00aa", "FN42ab12", "EM00"] {
            cfg.grid_square = ok.to_string();
            assert!(cfg.validate_section().is_ok(), "expected valid: {ok:?}");
        }
        for bad in [
            "fn42",       // presentation case: field must be uppercase
            "FN42PR",     // subsquare must be lowercase here
            "FN4",        // wrong length
            "FN42X",      // odd length
            "FN4212",     // wrong character classes
            "ZZ99",       // field letters outside A–R
            "FN42yy",     // subsquare outside a–x
            "FN42AB12CD", // 10 chars: beyond 4/6/8 precision
            "",
            "   ",
        ] {
            cfg.grid_square = bad.to_string();
            assert!(cfg.validate_section().is_err(), "expected invalid: {bad:?}");
        }
    }

    #[test]
    fn test_default_station_config() {
        let config = StationConfig::default();
        assert_eq!(config.callsign, "N0CALL");
        assert_eq!(config.grid_square, "AA00aa");
        assert_eq!(config.power_watts, 100);
        assert!(config.validate_section().is_ok());
    }

    /// Regression: same bug class as NetworkConfig's merge_with (see
    /// network.rs's `merge_with_carries_over_every_opt_in_integration`) —
    /// tx_late_max_ms/tx_self_parity/ptt_lead_ms drive the live TX-slot
    /// scheduler (coordinator/tx.rs) and were silently dropped during
    /// multi-source config merging, always reverting to compiled-in defaults
    /// regardless of what the operator's file said.
    #[test]
    fn merge_with_carries_over_tx_timing_fields() {
        let mut base = StationConfig::default();

        let mut file_cfg = StationConfig::default();
        file_cfg.tx_late_max_ms = 12_000;
        file_cfg.tx_self_parity = TxSelfParity::Even;
        file_cfg.ptt_lead_ms = 150;

        base.merge_with(file_cfg);

        assert_eq!(base.tx_late_max_ms, 12_000);
        assert_eq!(base.tx_self_parity, TxSelfParity::Even);
        assert_eq!(base.ptt_lead_ms, 150);
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
        assert_eq!(cfg.tx_late_max_ms, 3000);
        assert_eq!(cfg.tx_self_parity, TxSelfParity::Auto);
        assert_eq!(cfg.ptt_lead_ms, 80);
    }

    /// PAN-315 AC5: the six bounds validator 0.21 enforced through derive
    /// attributes keep their inclusive limits. Each row varies one field
    /// against the valid default. Callsign length counts Unicode characters.
    #[test]
    fn station_derived_constraints_boundary_table() {
        type Edit = fn(&mut StationConfig);
        let rows: &[(&str, Edit, bool)] = &[
            ("callsign 3 chars", |c| c.callsign = "K1A".into(), true),
            ("callsign 2 chars", |c| c.callsign = "K1".into(), false),
            ("callsign empty", |c| c.callsign = String::new(), false),
            (
                "callsign 20 chars",
                |c| c.callsign = format!("K1{}", "A".repeat(18)),
                true,
            ),
            (
                "callsign 21 chars",
                |c| c.callsign = format!("K1{}", "A".repeat(19)),
                false,
            ),
            (
                "callsign 64 chars",
                |c| c.callsign = format!("K1{}", "A".repeat(62)),
                false,
            ),
            // 20 characters but 38 bytes: the limit counts characters.
            (
                "callsign 20 unicode chars",
                |c| c.callsign = format!("K1{}", "Ä".repeat(18)),
                true,
            ),
            (
                "callsign 21 unicode chars",
                |c| c.callsign = format!("K1{}", "Ä".repeat(19)),
                false,
            ),
            ("grid 4 chars", |c| c.grid_square = "FN42".into(), true),
            ("grid 8 chars", |c| c.grid_square = "FN42ab12".into(), true),
            ("grid 3 chars", |c| c.grid_square = "FN4".into(), false),
            (
                "grid 9 chars",
                |c| c.grid_square = "FN42ab12x".into(),
                false,
            ),
            ("power 1 W", |c| c.power_watts = 1, true),
            ("power 1500 W", |c| c.power_watts = 1500, true),
            ("power 0 W", |c| c.power_watts = 0, false),
            ("power 1501 W", |c| c.power_watts = 1501, false),
            ("dxcc 1", |c| c.dxcc_entity = 1, true),
            ("dxcc 999", |c| c.dxcc_entity = 999, true),
            ("dxcc 0", |c| c.dxcc_entity = 0, false),
            ("dxcc 1000", |c| c.dxcc_entity = 1000, false),
            ("itu 1", |c| c.itu_zone = 1, true),
            ("itu 90", |c| c.itu_zone = 90, true),
            ("itu 0", |c| c.itu_zone = 0, false),
            ("itu 91", |c| c.itu_zone = 91, false),
            ("cq 1", |c| c.cq_zone = 1, true),
            ("cq 40", |c| c.cq_zone = 40, true),
            ("cq 0", |c| c.cq_zone = 0, false),
            ("cq 41", |c| c.cq_zone = 41, false),
            // Existing semantic checks still run after the bounds pass.
            (
                "callsign without digit",
                |c| c.callsign = "KAAA".into(),
                false,
            ),
            ("grid outside A-R", |c| c.grid_square = "ZZ99".into(), false),
            (
                "latitude out of range",
                |c| {
                    c.coordinates = Some(Coordinates {
                        latitude: 91.0,
                        longitude: 0.0,
                        elevation_meters: None,
                    })
                },
                false,
            ),
            (
                "antenna azimuth 360",
                |c| c.antennas[0].azimuth_degrees = Some(360),
                false,
            ),
        ];
        for (label, edit, valid) in rows {
            let mut cfg = StationConfig::default();
            edit(&mut cfg);
            assert_eq!(cfg.validate_section().is_ok(), *valid, "{label}");
        }
    }

    /// Split a station diagnostic into per-field `(field, code, params)`
    /// rows. validator stored fields and parameters in hash maps, so their
    /// order was never stable; compare them as sets.
    fn normalized_station_errors(message: &str) -> Vec<(String, String, Vec<String>)> {
        let body = message
            .strip_prefix("Validation error: Station validation failed: ")
            .unwrap_or_else(|| panic!("missing station prefix: {message}"));
        let mut rows: Vec<_> = body
            .lines()
            .map(|line| {
                let (field, rest) = line.split_once(": Validation error: ").unwrap();
                let (code, params) = rest.split_once(" [{").unwrap();
                let mut params: Vec<String> = params
                    .strip_suffix("}]")
                    .unwrap()
                    .split(", ")
                    .map(str::to_string)
                    .collect();
                params.sort();
                (field.to_string(), code.to_string(), params)
            })
            .collect();
        rows.sort();
        rows
    }

    fn row(field: &str, code: &str, params: &[&str]) -> (String, String, Vec<String>) {
        let mut params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
        params.sort();
        (field.to_string(), code.to_string(), params)
    }

    /// PAN-315 AC5: diagnostics keep validator 0.21's grammar, the
    /// `Station validation failed:` prefix and its debug-rendered
    /// min/max/value parameters.
    #[test]
    fn station_derived_diagnostics_compatible() {
        let mut cfg = StationConfig::default();
        cfg.callsign = "K1".into();
        let message = cfg.validate_section().unwrap_err().to_string();
        assert_eq!(
            normalized_station_errors(&message),
            vec![row(
                "callsign",
                "length",
                &[
                    r#""min": Number(3)"#,
                    r#""max": Number(20)"#,
                    r#""value": String("K1")"#
                ],
            )]
        );

        let mut cfg = StationConfig::default();
        cfg.grid_square = "FN4".into();
        cfg.power_watts = 1501;
        cfg.dxcc_entity = 0;
        cfg.itu_zone = 91;
        cfg.cq_zone = 0;
        let message = cfg.validate_section().unwrap_err().to_string();
        assert_eq!(
            normalized_station_errors(&message),
            vec![
                row(
                    "cq_zone",
                    "range",
                    &[
                        r#""min": Number(1)"#,
                        r#""max": Number(40)"#,
                        r#""value": Number(0)"#
                    ]
                ),
                row(
                    "dxcc_entity",
                    "range",
                    &[
                        r#""min": Number(1)"#,
                        r#""max": Number(999)"#,
                        r#""value": Number(0)"#
                    ]
                ),
                row(
                    "grid_square",
                    "length",
                    &[
                        r#""min": Number(4)"#,
                        r#""max": Number(8)"#,
                        r#""value": String("FN4")"#
                    ]
                ),
                row(
                    "itu_zone",
                    "range",
                    &[
                        r#""min": Number(1)"#,
                        r#""max": Number(90)"#,
                        r#""value": Number(91)"#
                    ]
                ),
                row(
                    "power_watts",
                    "range",
                    &[
                        r#""min": Number(1)"#,
                        r#""max": Number(1500)"#,
                        r#""value": Number(1501)"#
                    ]
                ),
            ]
        );
    }
}
