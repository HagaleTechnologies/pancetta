//! Hound (DXpedition chaser) configuration module.
//!
//! Defines the audio-offset regions used when pancetta operates as a Hound
//! chasing a Fox station. The Hound transmits in the low "call" region and
//! QSYs up to the "response" region when the Fox answers.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

fn default_call_min() -> f64 {
    300.0
}
fn default_call_max() -> f64 {
    900.0
}
fn default_response_min() -> f64 {
    1000.0
}
fn default_response_max() -> f64 {
    2700.0
}

/// Audio-offset regions for FT8 Hound (DXpedition chaser) mode. The Hound calls
/// the Fox low (call region) and QSYs up to the response region when answered.
///
/// Corresponds to the `[hound]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoundConfig {
    /// Low calling-region min audio offset (Hz). Default 300.
    #[serde(default = "default_call_min")]
    pub call_min_hz: f64,
    /// Low calling-region max (Hz). Default 900.
    #[serde(default = "default_call_max")]
    pub call_max_hz: f64,
    /// Post-QSY response-region min (Hz). Default 1000.
    #[serde(default = "default_response_min")]
    pub response_min_hz: f64,
    /// Post-QSY response-region max (Hz). Default 2700.
    #[serde(default = "default_response_max")]
    pub response_max_hz: f64,
}

impl Default for HoundConfig {
    fn default() -> Self {
        Self {
            call_min_hz: default_call_min(),
            call_max_hz: default_call_max(),
            response_min_hz: default_response_min(),
            response_max_hz: default_response_max(),
        }
    }
}

impl ConfigSection for HoundConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // PAN-72 (Codex round 6 on PR #350, finding 3): these bounds are the
        // station's real transmittable audio envelope, not a round number.
        // They mirror `pancetta_qso::qso_manager::ACTIVE_QSO_TX_OFFSET_MIN_HZ`
        // / `..._MAX_HZ`, which every committed TX offset is clamped to —
        // duplicated as literals only because `pancetta-config` sits BELOW
        // `pancetta-qso` in the dependency graph and cannot import them.
        //
        // The old ceiling of 3000 admitted a Hound region (2900, 3000] that no
        // relocation could ever land in: the allocator would happily rank an
        // offset there, `apply_tx_offset_switch` would clamp it down to 2900,
        // and the commit-time Hound-region guard would then refuse its own
        // clamped value — a silent, permanent no-op. `hound_offset_for`'s QSY
        // is worse still: it writes the region offset into `metadata.frequency`
        // with no clamp at all, putting our R+report outside the modulator's
        // range. Rejecting the config at load is the honest failure.
        const AUDIO_MIN: f64 = 200.0;
        const AUDIO_MAX: f64 = 2900.0;

        for (name, val) in [
            ("hound.call_min_hz", self.call_min_hz),
            ("hound.call_max_hz", self.call_max_hz),
            ("hound.response_min_hz", self.response_min_hz),
            ("hound.response_max_hz", self.response_max_hz),
        ] {
            if !(AUDIO_MIN..=AUDIO_MAX).contains(&val) {
                return Err(ConfigError::InvalidValue {
                    field: name.into(),
                    value: val.to_string(),
                });
            }
        }

        if self.call_min_hz >= self.call_max_hz {
            return Err(ConfigError::InvalidValue {
                field: "hound.call_min_hz".into(),
                value: format!("{} >= call_max_hz ({})", self.call_min_hz, self.call_max_hz),
            });
        }

        if self.response_min_hz >= self.response_max_hz {
            return Err(ConfigError::InvalidValue {
                field: "hound.response_min_hz".into(),
                value: format!(
                    "{} >= response_max_hz ({})",
                    self.response_min_hz, self.response_max_hz
                ),
            });
        }

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.call_min_hz = other.call_min_hz;
        self.call_max_hz = other.call_max_hz;
        self.response_min_hz = other.response_min_hz;
        self.response_max_hz = other.response_max_hz;
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_are_300_900_1000_2700() {
        let cfg = HoundConfig::default();
        assert_eq!(cfg.call_min_hz, 300.0);
        assert_eq!(cfg.call_max_hz, 900.0);
        assert_eq!(cfg.response_min_hz, 1000.0);
        assert_eq!(cfg.response_max_hz, 2700.0);
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = HoundConfig::default();
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn config_without_hound_section_uses_defaults() {
        // A TOML with NO [hound] section must deserialize to defaults.
        let toml = "[station]\ncallsign = \"K5ARH\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        assert_eq!(parsed.hound.call_min_hz, 300.0);
        assert_eq!(parsed.hound.call_max_hz, 900.0);
        assert_eq!(parsed.hound.response_min_hz, 1000.0);
        assert_eq!(parsed.hound.response_max_hz, 2700.0);
    }

    #[test]
    fn validate_rejects_call_min_ge_call_max() {
        let mut cfg = HoundConfig::default();
        cfg.call_min_hz = 900.0;
        cfg.call_max_hz = 900.0;
        assert!(
            cfg.validate_section().is_err(),
            "min >= max must be invalid"
        );

        cfg.call_min_hz = 950.0;
        cfg.call_max_hz = 900.0;
        assert!(cfg.validate_section().is_err(), "min > max must be invalid");
    }

    #[test]
    fn validate_rejects_response_min_ge_response_max() {
        let mut cfg = HoundConfig::default();
        cfg.response_min_hz = 2700.0;
        cfg.response_max_hz = 2700.0;
        assert!(cfg.validate_section().is_err());

        cfg.response_min_hz = 2800.0; // also out-of-range — hits the bounds check first
        cfg.response_max_hz = 2700.0;
        assert!(cfg.validate_section().is_err());
    }

    #[test]
    fn validate_rejects_value_outside_the_transmittable_envelope() {
        let mut cfg = HoundConfig::default();
        cfg.call_min_hz = 100.0; // below 200
        assert!(cfg.validate_section().is_err());

        cfg = HoundConfig::default();
        cfg.response_max_hz = 3100.0; // above the transmittable ceiling
        assert!(cfg.validate_section().is_err());
    }

    /// PAN-72 (Codex round 6 on PR #350, finding 3): the ceiling is the real
    /// TX envelope (`ACTIVE_QSO_TX_OFFSET_MAX_HZ` = 2900), not a round 3000. A
    /// region above it can never be reached: `apply_tx_offset_switch` clamps
    /// every commit to 2900 and its Hound-region guard then refuses the clamped
    /// value, so the Hound would silently never relocate at all.
    #[test]
    fn validate_rejects_a_response_region_above_the_tx_ceiling() {
        let mut cfg = HoundConfig::default();
        cfg.response_min_hz = 2910.0;
        cfg.response_max_hz = 2990.0;
        assert!(
            cfg.validate_section().is_err(),
            "a Hound response region entirely above the 2900 Hz TX ceiling is \
             unreachable and must be rejected at load, not silently no-op"
        );

        // ...and the top of the envelope itself stays valid.
        cfg = HoundConfig::default();
        cfg.response_min_hz = 2850.0;
        cfg.response_max_hz = 2900.0;
        assert!(
            cfg.validate_section().is_ok(),
            "2850-2900 Hz is inside the transmittable envelope and must remain \
             a legal Hound response region"
        );
    }

    #[test]
    fn serialization_roundtrip() {
        let cfg = HoundConfig::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        let deserialized: HoundConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.call_min_hz, deserialized.call_min_hz);
        assert_eq!(cfg.call_max_hz, deserialized.call_max_hz);
        assert_eq!(cfg.response_min_hz, deserialized.response_min_hz);
        assert_eq!(cfg.response_max_hz, deserialized.response_max_hz);
    }

    #[test]
    fn custom_values_parse_correctly() {
        let toml = r#"
call_min_hz = 400.0
call_max_hz = 800.0
response_min_hz = 1200.0
response_max_hz = 2500.0
"#;
        let cfg: HoundConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.call_min_hz, 400.0);
        assert_eq!(cfg.call_max_hz, 800.0);
        assert_eq!(cfg.response_min_hz, 1200.0);
        assert_eq!(cfg.response_max_hz, 2500.0);
        assert!(cfg.validate_section().is_ok());
    }
}
