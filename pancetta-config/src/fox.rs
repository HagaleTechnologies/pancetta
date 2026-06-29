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
///
/// # Stream accounting
///
/// While Fox mode is engaged there is always **one additional CQ stream** (the
/// `CallingCq` QSO).  The validated ceiling is therefore 7, not 8:
/// `max_streams` Hound answers + 1 CQ = at most 8 total streams ≤
/// `MAX_RETAINED_TX_STREAMS`.  A value of 8 would require 9 streams and is
/// rejected by [`FoxConfig::validate_section`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoxConfig {
    /// Maximum simultaneous Hound-answer TX streams per slot.  Must be ≥ 1
    /// and ≤ 7 (one slot is always reserved for the Fox CQ, so 7 answers +
    /// 1 CQ = 8 = [`MAX_RETAINED_TX_STREAMS`]).  Default 5.
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

/// Maximum value for [`FoxConfig::max_streams`].  One stream is always
/// reserved for the Fox's own CQ, so Hound answers are capped at
/// `MAX_RETAINED_TX_STREAMS - 1` (7).
pub const MAX_FOX_ANSWER_STREAMS: usize = MAX_RETAINED_TX_STREAMS - 1;

impl ConfigSection for FoxConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.max_streams == 0 || self.max_streams > MAX_FOX_ANSWER_STREAMS {
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
        // Ceiling is now 7 (answers), not 8 — CQ + 8 answers would exceed
        // MAX_RETAINED_TX_STREAMS=8.
        let cfg9 = FoxConfig { max_streams: 9 };
        assert!(
            cfg9.validate_section().is_err(),
            "max_streams = 9 must be invalid (> 7)"
        );

        let cfg8 = FoxConfig { max_streams: 8 };
        assert!(
            cfg8.validate_section().is_err(),
            "max_streams = 8 must be invalid (CQ + 8 = 9 > MAX_RETAINED_TX_STREAMS=8)"
        );

        let cfg7 = FoxConfig { max_streams: 7 };
        assert!(
            cfg7.validate_section().is_ok(),
            "max_streams = 7 must be ok (CQ + 7 answers = 8 = MAX_RETAINED_TX_STREAMS)"
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
