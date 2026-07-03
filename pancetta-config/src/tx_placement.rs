//! TX-placement instrument configuration module.
//!
//! Defines the opt-in **auto-repark** feature: when the operator's parked
//! idle TX offset degrades to busy-both (openness code 0) in the
//! TX-placement instrument and a meaningfully better slice is available,
//! the coordinator may move the parked offset automatically.
//!
//! Auto-repark adjusts the IDLE parked offset ONLY — it never touches a
//! live transmit stream. It is gated on the shared `active_tx_qsos` set
//! being empty at write-time (see `pancetta/src/coordinator/autonomous.rs`
//! `should_repark` + its call site); this config section only controls
//! whether the feature is engaged at all and how large a score gain is
//! required before it fires (hysteresis).

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

fn default_auto_repark() -> bool {
    false
}
fn default_repark_min_score_gain() -> f64 {
    20.0
}

/// TX-placement instrument configuration. Corresponds to the
/// `[tx_placement]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPlacementConfig {
    /// Enable opt-in auto-repark of the parked idle TX offset when it
    /// degrades to busy-both and a meaningfully better slice exists.
    /// Default `false` (OFF) — a fresh install/upgrade never reparks
    /// unless the operator explicitly opts in.
    #[serde(default = "default_auto_repark")]
    pub auto_repark: bool,
    /// Minimum score gain (best slice score minus the parked slice's
    /// current score) required before auto-repark fires. Hysteresis
    /// against reparking for a marginal improvement. Default 20.0.
    #[serde(default = "default_repark_min_score_gain")]
    pub repark_min_score_gain: f64,
}

impl Default for TxPlacementConfig {
    fn default() -> Self {
        Self {
            auto_repark: default_auto_repark(),
            repark_min_score_gain: default_repark_min_score_gain(),
        }
    }
}

impl ConfigSection for TxPlacementConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // auto_repark is a bool — nothing to validate.
        if !(self.repark_min_score_gain.is_finite()) || self.repark_min_score_gain < 0.0 {
            return Err(ConfigError::InvalidValue {
                field: "tx_placement.repark_min_score_gain".into(),
                value: self.repark_min_score_gain.to_string(),
            });
        }

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.auto_repark = other.auto_repark;
        self.repark_min_score_gain = other.repark_min_score_gain;
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_are_off_and_20() {
        let cfg = TxPlacementConfig::default();
        assert!(!cfg.auto_repark);
        assert_eq!(cfg.repark_min_score_gain, 20.0);
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = TxPlacementConfig::default();
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn config_without_tx_placement_section_uses_defaults() {
        // A TOML with NO [tx_placement] section must deserialize to
        // defaults — a fresh install/upgrade sees auto_repark == false.
        let toml = "[station]\ncallsign = \"K5ARH\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        assert!(!parsed.tx_placement.auto_repark);
        assert_eq!(parsed.tx_placement.repark_min_score_gain, 20.0);
    }

    #[test]
    fn validate_rejects_negative_min_gain() {
        let mut cfg = TxPlacementConfig::default();
        cfg.repark_min_score_gain = -1.0;
        assert!(cfg.validate_section().is_err());
    }

    #[test]
    fn validate_rejects_non_finite_min_gain() {
        let mut cfg = TxPlacementConfig::default();
        cfg.repark_min_score_gain = f64::NAN;
        assert!(cfg.validate_section().is_err());

        cfg.repark_min_score_gain = f64::INFINITY;
        assert!(cfg.validate_section().is_err());
    }

    #[test]
    fn validate_accepts_zero_min_gain() {
        let mut cfg = TxPlacementConfig::default();
        cfg.repark_min_score_gain = 0.0;
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn serialization_roundtrip() {
        let cfg = TxPlacementConfig::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        let deserialized: TxPlacementConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.auto_repark, deserialized.auto_repark);
        assert_eq!(
            cfg.repark_min_score_gain,
            deserialized.repark_min_score_gain
        );
    }

    #[test]
    fn custom_values_parse_correctly() {
        let toml = r#"
auto_repark = true
repark_min_score_gain = 35.0
"#;
        let cfg: TxPlacementConfig = toml::from_str(toml).unwrap();
        assert!(cfg.auto_repark);
        assert_eq!(cfg.repark_min_score_gain, 35.0);
        assert!(cfg.validate_section().is_ok());
    }
}
