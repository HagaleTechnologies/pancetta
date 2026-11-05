//! Contest scoring configuration (PAN-50).
//!
//! Corresponds to the optional `[contest]` section in the TOML config file.
//! Contest scoring is disabled by default: the section is absent and nothing
//! changes. When present, it identifies ONE entrant in ONE contest edition
//! (the `instance_id`), the built-in family exchange profile, the pinned
//! scoring-rule edition, the entrant's state/province/`DX`, and the contest's
//! UTC operating intervals. Edits require a restart, consistent with the
//! restricted live-apply policy.
//!
//! This crate validates the section structurally (required fields, period
//! ordering/overlap). The rule-dependent checks — unknown `rules_id`,
//! `operator_region` validity against the selected rules, and instance
//! identity against logged history — need the scoring-rule catalog, which
//! lives in `pancetta-qso` (`contest::scoring`); the coordinator performs
//! them at startup conversion so this crate never depends on `pancetta-qso`
//! (serialized config stays independent).
//!
//! Threaded by the coordinator into
//! `pancetta_qso::contest::scoring::ContestScoringContext`.

use crate::{ConfigError, ConfigResult, ConfigSection};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Exact validation message for malformed periods (PAN-50 design).
pub const CONTEST_PERIODS_MESSAGE: &str =
    "contest: periods must be ordered, non-overlapping UTC ranges with start before end";

/// One UTC operating interval. Half-open `[start, end)`: a contact starting
/// exactly at `end` is outside, one starting exactly at `start` is inside.
/// Gaps between intervals are outside the contest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContestPeriodConfig {
    /// Interval start (UTC, inclusive).
    pub start: DateTime<Utc>,
    /// Interval end (UTC, exclusive).
    pub end: DateTime<Utc>,
}

/// Contest scoring: an explicit operator opt-in for manual contest contacts.
/// This does NOT enable autonomous contest operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContestScoringConfig {
    /// Whether contest scoring is active. Defaults to `true` when the
    /// section exists, so `enabled = false` is the explicit opt-out; a
    /// disabled section may contain only this field.
    pub enabled: bool,
    /// Identifies one entrant in one contest edition. Contacts, dupes and
    /// multipliers never mix across instance ids.
    pub instance_id: Option<String>,
    /// Built-in exchange profile id (e.g. `us-state-qso-party`).
    pub profile_id: Option<String>,
    /// Pinned scoring-rule edition (e.g. `ksqp-ft-2026`,
    /// `scqp-digital-fixed-2026`). Validated against the rule catalog at
    /// coordinator startup.
    pub rules_id: Option<String>,
    /// The entrant's state, DC, Canadian province/territory, or `DX`. Never
    /// inferred from a callsign.
    pub operator_region: Option<String>,
    /// UTC operating intervals, ordered and non-overlapping. At least one is
    /// required when enabled.
    pub periods: Vec<ContestPeriodConfig>,
}

impl Default for ContestScoringConfig {
    fn default() -> Self {
        // `enabled` defaults to true when the section exists (serde field
        // default via the container); a section that exists but sets nothing
        // else fails validation with the required-fields message.
        Self {
            enabled: true,
            instance_id: None,
            profile_id: None,
            rules_id: None,
            operator_region: None,
            periods: Vec::new(),
        }
    }
}

impl ContestScoringConfig {
    fn required_string(field: &Option<String>) -> Result<&str, ConfigError> {
        field
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ConfigError::Validation(
                    "contest: instance_id, profile_id, rules_id, operator_region, and at least \
                     one period are required when contest scoring is enabled"
                        .to_string(),
                )
            })
    }
}

impl ConfigSection for ContestScoringConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if !self.enabled {
            // Explicit disable: instance fields are not required.
            return Ok(());
        }
        Self::required_string(&self.instance_id)?;
        Self::required_string(&self.profile_id)?;
        Self::required_string(&self.rules_id)?;
        Self::required_string(&self.operator_region)?;
        if self.periods.is_empty() {
            return Err(ConfigError::Validation(
                "contest: instance_id, profile_id, rules_id, operator_region, and at least one \
                 period are required when contest scoring is enabled"
                    .to_string(),
            ));
        }
        for window in &self.periods {
            if window.start >= window.end {
                return Err(ConfigError::Validation(CONTEST_PERIODS_MESSAGE.to_string()));
            }
        }
        for pair in self.periods.windows(2) {
            // Ordered and non-overlapping; touching intervals are fine
            // (half-open ranges), a later start before an earlier end is not.
            if pair[0].end > pair[1].start {
                return Err(ConfigError::Validation(CONTEST_PERIODS_MESSAGE.to_string()));
            }
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // The section replaces as a unit: an overlay that carries `[contest]`
        // wins wholesale, including its `enabled` flag.
        self.enabled = other.enabled;
        self.instance_id = other.instance_id;
        self.profile_id = other.profile_id;
        self.rules_id = other.rules_id;
        self.operator_region = other.operator_region;
        self.periods = other.periods;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_section() -> ContestScoringConfig {
        ContestScoringConfig {
            enabled: true,
            instance_id: Some("KSQP-2026-K5ARH".to_string()),
            profile_id: Some("us-state-qso-party".to_string()),
            rules_id: Some("ksqp-ft-2026".to_string()),
            operator_region: Some("TX".to_string()),
            periods: vec![ContestPeriodConfig {
                start: DateTime::parse_from_rfc3339("2026-08-29T14:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                end: DateTime::parse_from_rfc3339("2026-08-30T02:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            }],
        }
    }

    #[test]
    fn disabled_default_compatibility_absent_section_changes_nothing() {
        let config = crate::Config::default();
        assert!(config.contest.is_none());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn complete_section_validates() {
        assert!(enabled_section().validate_section().is_ok());
    }

    #[test]
    fn explicit_disable_requires_no_instance_fields() {
        let section = ContestScoringConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(section.validate_section().is_ok());
    }

    #[test]
    fn enabled_section_missing_fields_is_rejected() {
        let mut section = enabled_section();
        section.instance_id = None;
        let err = section.validate_section().unwrap_err().to_string();
        assert!(
            err.contains("required when contest scoring is enabled"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn start_must_precede_end() {
        let mut section = enabled_section();
        let start = section.periods[0].start;
        section.periods[0].end = start;
        let err = section.validate_section().unwrap_err().to_string();
        assert!(err.contains(CONTEST_PERIODS_MESSAGE), "got: {err}");
    }

    #[test]
    fn overlapping_periods_are_rejected_and_gaps_are_allowed() {
        let mut section = enabled_section();
        let p0 = &section.periods[0];
        let (s0, e0) = (p0.start, p0.end);
        // Gap: second interval starts after the first ends — valid.
        section.periods.push(ContestPeriodConfig {
            start: e0 + chrono::Duration::hours(12),
            end: e0 + chrono::Duration::hours(18),
        });
        assert!(section.validate_section().is_ok());
        // Overlap: second interval starts before the first ends — invalid.
        section.periods[1].start = e0 - chrono::Duration::minutes(1);
        let err = section.validate_section().unwrap_err().to_string();
        assert!(err.contains(CONTEST_PERIODS_MESSAGE), "got: {err}");
        // Unordered is equally invalid.
        section.periods[1].start = s0 - chrono::Duration::hours(2);
        section.periods[1].end = s0 - chrono::Duration::hours(1);
        assert!(section.validate_section().is_err());
    }

    #[test]
    fn tomllib_round_trip_of_the_design_example() {
        let toml_text = r#"
[contest]
instance_id = "KSQP-2026-K5ARH"
profile_id = "us-state-qso-party"
rules_id = "ksqp-ft-2026"
operator_region = "TX"
periods = [
  { start = "2026-08-29T14:00:00Z", end = "2026-08-30T02:00:00Z" },
  { start = "2026-08-30T14:00:00Z", end = "2026-08-30T20:00:00Z" },
]
"#;
        let config: crate::Config = toml::from_str(toml_text).unwrap();
        let contest = config.contest.as_ref().expect("section must parse");
        assert!(contest.enabled);
        assert_eq!(contest.rules_id.as_deref(), Some("ksqp-ft-2026"));
        assert_eq!(contest.periods.len(), 2);
        assert!(config.validate().is_ok());

        let serialized = toml::to_string_pretty(&config).unwrap();
        let reparsed: crate::Config = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.contest, config.contest);
    }

    #[test]
    fn explicit_disable_parses_from_toml() {
        let config: crate::Config = toml::from_str("[contest]\nenabled = false\n").unwrap();
        let contest = config
            .contest
            .as_ref()
            .expect("disabled section still parses");
        assert!(!contest.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn config_merge_replaces_the_section_as_a_unit() {
        let base = crate::Config {
            contest: Some(enabled_section()),
            ..Default::default()
        };

        // Overlay without the section keeps the base's contest.
        let merged_keep = {
            let mut merged = base.clone();
            merged.merge_with(crate::Config::default());
            merged
        };
        assert_eq!(merged_keep.contest, base.contest);

        // Overlay carrying an explicit disable replaces the section.
        let overlay = crate::Config {
            contest: Some(ContestScoringConfig {
                enabled: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut merged = base;
        merged.merge_with(overlay);
        assert!(!merged.contest.as_ref().unwrap().enabled);
    }
}
