//! Fox (DXpedition operator) configuration module.
//!
//! Defines the operating parameters used when pancetta runs AS the DXpedition
//! (Fox) station, simultaneously answering up to `max_streams` Hound callers
//! per 15-second slot.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Maximum simultaneous TX streams supported by the multi-stream TX pipeline.
/// Validated by [`FoxConfig::validate_section`] — `max_streams` must not
/// exceed this ceiling.
pub const MAX_RETAINED_TX_STREAMS: usize = 8;

fn default_max_streams() -> usize {
    5
}

/// Configuration for FT8 Fox (DXpedition operator) mode.  When active,
/// pancetta simultaneously transmits up to `max_streams` FT8 signals in a
/// single 15-second slot, each answering a different Hound caller.
///
/// Corresponds to the `[fox]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoxConfig {
    /// Maximum simultaneous TX streams per slot.  Must be ≥ 1 and ≤
    /// [`MAX_RETAINED_TX_STREAMS`] (8).  Default 5.
    #[serde(default = "default_max_streams")]
    pub max_streams: usize,
}

impl Default for FoxConfig {
    fn default() -> Self {
        Self {
            max_streams: default_max_streams(),
        }
    }
}

impl ConfigSection for FoxConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.max_streams == 0 || self.max_streams > MAX_RETAINED_TX_STREAMS {
            return Err(ConfigError::InvalidValue {
                field: "fox.max_streams".into(),
                value: self.max_streams.to_string(),
            });
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.max_streams = other.max_streams;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn default_max_streams_is_5() {
        let cfg = FoxConfig::default();
        assert_eq!(cfg.max_streams, 5);
    }

    #[test]
    fn default_is_valid() {
        let cfg = FoxConfig::default();
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn config_without_fox_section_uses_defaults() {
        // A TOML with NO [fox] section must deserialize to defaults.
        let toml = "[station]\ncallsign = \"K5ARH\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        assert_eq!(parsed.fox.max_streams, 5);
    }

    #[test]
    fn validate_rejects_zero() {
        let cfg = FoxConfig { max_streams: 0 };
        assert!(
            cfg.validate_section().is_err(),
            "max_streams = 0 must be invalid"
        );
    }

    #[test]
    fn validate_rejects_above_ceiling() {
        let cfg = FoxConfig { max_streams: 9 };
        assert!(
            cfg.validate_section().is_err(),
            "max_streams = 9 must be invalid (> 8)"
        );

        let cfg8 = FoxConfig { max_streams: 8 };
        assert!(
            cfg8.validate_section().is_ok(),
            "max_streams = 8 must be ok"
        );
    }

    #[test]
    fn validate_accepts_5() {
        let cfg = FoxConfig { max_streams: 5 };
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn serialization_roundtrip() {
        let cfg = FoxConfig::default();
        let toml_str = toml::to_string(&cfg).unwrap();
        let deserialized: FoxConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.max_streams, deserialized.max_streams);
    }
}
