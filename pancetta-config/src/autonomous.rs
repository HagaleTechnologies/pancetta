//! Autonomous operator configuration module
//!
//! Settings for the autonomous QSO operator: slot parity, listen-cycle policy,
//! CQ behaviour, DX scoring thresholds, and band hopping.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// How the operator picks its TX slot parity.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotParitySetting {
    Even,
    Odd,
    #[default]
    Auto,
}

/// Adaptive listen-cycle configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenCycleConfig {
    /// TX cycles between forced collision-listen slots (initial value).
    pub initial_interval: u32,
    /// Backed-off interval after enough clean listens.
    pub backoff_interval: u32,
    /// Interval used when a collision was recently detected.
    pub collision_interval: u32,
    /// Clean listens required before back-off kicks in.
    pub backoff_threshold: u32,
}

impl Default for ListenCycleConfig {
    fn default() -> Self {
        Self {
            initial_interval: 3,
            backoff_interval: 5,
            collision_interval: 2,
            backoff_threshold: 5,
        }
    }
}

/// A band entry for band-hopping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandHopEntry {
    /// Dial frequency in Hz (e.g. 14074000).
    pub dial_frequency: u64,
    /// Human-readable band name (e.g. "20m").
    pub band_name: String,
    /// Priority order (lower = higher priority).
    pub priority: u32,
}

/// Band-hopping configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandHoppingConfig {
    /// Enable automatic band hopping.
    pub enabled: bool,
    /// Low-activity cycles before hopping to the next band.
    pub hop_threshold: u32,
    /// Ordered list of bands to hop between.
    pub bands: Vec<BandHopEntry>,
}

impl Default for BandHoppingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hop_threshold: 20,
            bands: vec![
                BandHopEntry {
                    dial_frequency: 14_074_000,
                    band_name: "20m".into(),
                    priority: 1,
                },
                BandHopEntry {
                    dial_frequency: 7_074_000,
                    band_name: "40m".into(),
                    priority: 2,
                },
            ],
        }
    }
}

/// Configurable weights for autonomous priority scoring.
///
/// Each decoded CQ is scored using these weights. Positive weights increase
/// desirability; negative weights penalize. Final score is clamped to 0.0–1.0.
///
/// Corresponds to `[autonomous.priorities]` in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityWeightsConfig {
    /// Weight for DXCC entities not yet worked.
    pub needed_dxcc: f64,
    /// Weight for needed grid/state/zone for award tracking.
    pub needed_grid: f64,
    /// Weight for POTA/SOTA activator detection.
    pub pota_sota: f64,
    /// Weight for callsign rarity (how rarely this prefix is seen).
    pub rarity: f64,
    /// Weight for signal strength (SNR). Stronger = more likely to complete.
    pub signal_strength: f64,
    /// Penalty for stations already worked on this band (should be negative).
    pub duplicate_penalty: f64,
    /// Penalty for stations recently called but QSO didn't complete (should be negative).
    pub recent_failure_penalty: f64,
}

impl Default for PriorityWeightsConfig {
    fn default() -> Self {
        Self {
            needed_dxcc: 0.35,
            needed_grid: 0.20,
            pota_sota: 0.15,
            rarity: 0.10,
            signal_strength: 0.05,
            duplicate_penalty: -0.40,
            recent_failure_penalty: -0.15,
        }
    }
}

impl PriorityWeightsConfig {
    /// Validate that weights are within reasonable bounds.
    pub fn validate(&self) -> ConfigResult<()> {
        for (name, val) in [
            ("needed_dxcc", self.needed_dxcc),
            ("needed_grid", self.needed_grid),
            ("pota_sota", self.pota_sota),
            ("rarity", self.rarity),
            ("signal_strength", self.signal_strength),
            ("duplicate_penalty", self.duplicate_penalty),
            ("recent_failure_penalty", self.recent_failure_penalty),
        ] {
            if val < -1.0 || val > 1.0 {
                return Err(ConfigError::InvalidValue {
                    field: format!("autonomous.priorities.{}", name),
                    value: val.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Frequency allocator configuration for multi-QSO support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyAllocatorConfig {
    /// How many recent decode cycles to consider for occupancy.
    pub decode_history_cycles: usize,
    /// Center of passband preference in Hz.
    pub center_bias_hz: f64,
    /// Minimum preferred offset from DX station in Hz.
    pub dx_proximity_min_hz: f64,
    /// Maximum preferred offset from DX station in Hz.
    pub dx_proximity_max_hz: f64,
    /// Minimum separation between own QSO frequencies in Hz.
    pub min_separation_hz: f64,
    /// Avoid strong signals within this range in Hz.
    pub neighbor_guard_hz: f64,
}

impl Default for FrequencyAllocatorConfig {
    fn default() -> Self {
        Self {
            decode_history_cycles: 4,
            center_bias_hz: 1500.0,
            dx_proximity_min_hz: 50.0,
            dx_proximity_max_hz: 200.0,
            min_separation_hz: 75.0,
            neighbor_guard_hz: 100.0,
        }
    }
}

/// Top-level autonomous operator configuration.
///
/// Corresponds to the `[autonomous]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousConfig {
    /// Enable autonomous mode.
    pub enabled: bool,
    /// TX slot parity selection.
    pub slot_parity: SlotParitySetting,
    /// Number of idle TX cycles before calling CQ (~150 s at default 10).
    pub cq_after_idle_cycles: u32,
    /// Maximum concurrent QSOs (default 1, capability for 2).
    pub max_concurrent_qsos: u32,
    /// Our TX audio offset in Hz within the FT8 sub-band.
    pub tx_offset_hz: f64,
    /// Minimum DX score (0.0–1.0) required to respond to a CQ.
    pub min_dx_score: f64,
    /// Minimum DX score required to open an additional QSO slot (0.0–1.0).
    /// Only applies to second+ concurrent QSOs. First QSO uses min_dx_score.
    pub min_multi_slot_score: f64,
    /// Frequency allocator settings for smart TX offset selection.
    pub frequency: FrequencyAllocatorConfig,
    /// Directed CQ text (e.g. "DX", "NA", or empty for general CQ).
    pub cq_direction: String,
    /// Listen-cycle adaptive policy configuration.
    pub listen_cycle: ListenCycleConfig,
    /// Band-hopping configuration.
    pub band_hopping: BandHoppingConfig,
    /// Priority scoring weights for autonomous operator decisions.
    pub priorities: PriorityWeightsConfig,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slot_parity: SlotParitySetting::Auto,
            cq_after_idle_cycles: 10,
            max_concurrent_qsos: 1,
            tx_offset_hz: 1500.0,
            min_dx_score: 0.3,
            min_multi_slot_score: 0.7,
            frequency: FrequencyAllocatorConfig::default(),
            cq_direction: String::new(),
            listen_cycle: ListenCycleConfig::default(),
            band_hopping: BandHoppingConfig::default(),
            priorities: PriorityWeightsConfig::default(),
        }
    }
}

impl ConfigSection for AutonomousConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.cq_after_idle_cycles == 0 {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.cq_after_idle_cycles".into(),
                value: "0".into(),
            });
        }
        if self.max_concurrent_qsos == 0 {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.max_concurrent_qsos".into(),
                value: "0".into(),
            });
        }
        if !(0.0..=1.0).contains(&self.min_dx_score) {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.min_dx_score".into(),
                value: self.min_dx_score.to_string(),
            });
        }
        if !(0.0..=1.0).contains(&self.min_multi_slot_score) {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.min_multi_slot_score".into(),
                value: self.min_multi_slot_score.to_string(),
            });
        }
        if self.tx_offset_hz < 100.0 || self.tx_offset_hz > 3000.0 {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.tx_offset_hz".into(),
                value: self.tx_offset_hz.to_string(),
            });
        }
        self.priorities.validate()?;
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.enabled = other.enabled;
        self.slot_parity = other.slot_parity;
        self.cq_after_idle_cycles = other.cq_after_idle_cycles;
        self.max_concurrent_qsos = other.max_concurrent_qsos;
        self.tx_offset_hz = other.tx_offset_hz;
        self.min_dx_score = other.min_dx_score;
        self.min_multi_slot_score = other.min_multi_slot_score;
        self.frequency = other.frequency;
        self.cq_direction = other.cq_direction;
        self.listen_cycle = other.listen_cycle;
        self.band_hopping = other.band_hopping;
        self.priorities = other.priorities;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_autonomous_config() {
        let config = AutonomousConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.cq_after_idle_cycles, 10);
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_invalid_dx_score() {
        let mut config = AutonomousConfig::default();
        config.min_dx_score = 1.5;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config = AutonomousConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: AutonomousConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.enabled, deserialized.enabled);
        assert_eq!(config.tx_offset_hz, deserialized.tx_offset_hz);
    }

    #[test]
    fn test_default_priority_weights() {
        let weights = PriorityWeightsConfig::default();
        assert!(weights.validate().is_ok());
        assert!(weights.needed_dxcc > 0.0);
        assert!(weights.duplicate_penalty < 0.0);
    }

    #[test]
    fn test_priority_weights_serialization() {
        let weights = PriorityWeightsConfig::default();
        let toml_str = toml::to_string(&weights).unwrap();
        let deserialized: PriorityWeightsConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(weights.needed_dxcc, deserialized.needed_dxcc);
        assert_eq!(weights.duplicate_penalty, deserialized.duplicate_penalty);
    }

    #[test]
    fn test_autonomous_config_with_priorities() {
        let config = AutonomousConfig::default();
        assert!(config.validate_section().is_ok());
        assert!(config.priorities.needed_dxcc > 0.0);
    }

    #[test]
    fn test_multi_slot_score_validation() {
        let mut config = AutonomousConfig::default();
        config.min_multi_slot_score = 0.7;
        assert!(config.validate_section().is_ok());

        config.min_multi_slot_score = 1.5;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_frequency_config_serialization() {
        let config = AutonomousConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: AutonomousConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.min_multi_slot_score, deserialized.min_multi_slot_score);
        assert_eq!(
            config.frequency.center_bias_hz,
            deserialized.frequency.center_bias_hz
        );
    }
}
