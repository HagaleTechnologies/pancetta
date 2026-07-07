//! Decoder effort/budget configuration module.
//!
//! Defines the `[decoder]` TOML section that controls the decode-effort
//! preset and an optional explicit per-window time budget. This section is
//! currently parsed/validated/merged only — nothing reads it yet (that is a
//! follow-on task that seeds the `decode_effort_budget_ms` atomic from it).

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Decode-effort preset. `Auto` (the default) maps from the hardware tier
/// probe; the others are explicit operator overrides trading decode
/// thoroughness for latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DecodeEffort {
    /// Derive the effort from the probed hardware tier (Fast/Moderate/Slow).
    #[default]
    Auto,
    /// Minimal effort — fastest decode, lowest recall.
    Eco,
    /// Baseline effort.
    Standard,
    /// Higher effort — more decode passes/candidates for better recall.
    Deep,
    /// Maximum effort — slowest decode, highest recall.
    Max,
}

/// Decoder effort/budget configuration.
///
/// Corresponds to the `[decoder]` section in the TOML config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderConfig {
    /// Effort preset; `auto` maps from the hardware tier probe.
    pub effort: DecodeEffort,
    /// Explicit per-window budget in ms; overrides `effort` when Some.
    pub budget_ms: Option<u64>,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            effort: DecodeEffort::Auto,
            budget_ms: None,
        }
    }
}

impl ConfigSection for DecoderConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.budget_ms == Some(0) {
            return Err(ConfigError::InvalidValue {
                field: "decoder.budget_ms".to_string(),
                value: "0".to_string(),
            });
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.effort = other.effort;
        self.budget_ms = other.budget_ms;
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_are_auto_and_none() {
        let cfg = DecoderConfig::default();
        assert_eq!(cfg.effort, DecodeEffort::Auto);
        assert_eq!(cfg.budget_ms, None);
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = DecoderConfig::default();
        assert!(cfg.validate_section().is_ok());
    }

    #[test]
    fn config_without_decoder_section_uses_defaults() {
        // A TOML with NO [decoder] section must deserialize to defaults.
        let toml = "[station]\ncallsign = \"K5ARH\"\n";
        let parsed: Config = toml::from_str(toml).expect("partial config must deserialize");
        assert_eq!(parsed.decoder.effort, DecodeEffort::Auto);
        assert_eq!(parsed.decoder.budget_ms, None);
    }

    #[test]
    fn parse_round_trip_including_deep_string_form() {
        let toml_str = "[decoder]\neffort = \"deep\"\nbudget_ms = 5000\n";
        let parsed: Config = toml::from_str(toml_str).expect("must parse");
        assert_eq!(parsed.decoder.effort, DecodeEffort::Deep);
        assert_eq!(parsed.decoder.budget_ms, Some(5000));

        // Round-trip: serialize back out and re-parse, values must be stable.
        let rendered = toml::to_string(&parsed).expect("must serialize");
        let reparsed: Config = toml::from_str(&rendered).expect("must reparse");
        assert_eq!(reparsed.decoder.effort, DecodeEffort::Deep);
        assert_eq!(reparsed.decoder.budget_ms, Some(5000));
    }

    #[test]
    fn parse_all_effort_string_forms() {
        for (s, expected) in [
            ("auto", DecodeEffort::Auto),
            ("eco", DecodeEffort::Eco),
            ("standard", DecodeEffort::Standard),
            ("deep", DecodeEffort::Deep),
            ("max", DecodeEffort::Max),
        ] {
            let toml_str = format!("[decoder]\neffort = \"{s}\"\n");
            let parsed: Config = toml::from_str(&toml_str).unwrap_or_else(|e| {
                panic!("effort = \"{s}\" must parse, got error: {e}");
            });
            assert_eq!(parsed.decoder.effort, expected, "effort string {s}");
        }
    }

    #[test]
    fn validate_rejects_budget_ms_zero() {
        let mut cfg = DecoderConfig::default();
        cfg.budget_ms = Some(0);
        assert!(
            cfg.validate_section().is_err(),
            "budget_ms == Some(0) must be invalid"
        );
    }

    #[test]
    fn validate_accepts_budget_ms_none_and_nonzero() {
        let mut cfg = DecoderConfig::default();
        cfg.budget_ms = None;
        assert!(cfg.validate_section().is_ok(), "None must be valid");

        cfg.budget_ms = Some(1);
        assert!(cfg.validate_section().is_ok(), "Some(1) must be valid");

        cfg.budget_ms = Some(12_640);
        assert!(cfg.validate_section().is_ok(), "Some(12640) must be valid");
    }

    /// 2026-07-05 bug-class regression test: a hand-written `merge_with` is a
    /// manually-maintained list of `self.field = other.field` lines, so a
    /// field added after the impl was written is trivially forgotten — and
    /// the bug is invisible because both parsing AND validation still pass;
    /// only the merge silently drops the field back to its compiled-in
    /// default. This test constructs a base config (defaults) and an
    /// override config with NON-DEFAULT values for BOTH `effort` and
    /// `budget_ms`, merges, and asserts BOTH survive — a test that would
    /// fail if either field were accidentally omitted from `merge_with`.
    #[test]
    fn merge_with_carries_over_decoder_section() {
        let mut base = DecoderConfig::default();
        assert_eq!(base.effort, DecodeEffort::Auto);
        assert_eq!(base.budget_ms, None);

        let mut override_cfg = DecoderConfig::default();
        override_cfg.effort = DecodeEffort::Max;
        override_cfg.budget_ms = Some(9_999);

        base.merge_with(override_cfg);

        assert_eq!(
            base.effort,
            DecodeEffort::Max,
            "effort must survive merge_with"
        );
        assert_eq!(
            base.budget_ms,
            Some(9_999),
            "budget_ms must survive merge_with"
        );
    }
}
