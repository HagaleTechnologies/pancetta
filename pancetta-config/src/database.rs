//! Database / QSO-log persistence configuration.
//!
//! Corresponds to the `[database]` section in the TOML config file. Threaded
//! by the coordinator into `pancetta_qso::async_logger::LoggerConfig`.

use crate::{ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Database / QSO-log persistence settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Layer 2 timeline persistence
    /// (docs/observability-diagnostics-plan.md §"Persist the timeline").
    ///
    /// When `true`, a completed or failed QSO's full state-transition +
    /// message timeline (`QsoProgress::state_history` / `.messages`) is
    /// persisted so the whole "what we sent / what we heard / why we
    /// advanced" is reconstructable offline, keyed by QSO id
    /// (`QsoDatabase::get_qso_timeline`).
    ///
    /// Defaults to `false`. Every completed/failed QSO already carries its
    /// timeline through a broadcast event; writing it durably is a
    /// per-QSO, unbounded-length JSON blob that accumulates for the life
    /// of the SQLite index. That's a real, if modest, storage/IO cost an
    /// operator should opt into deliberately rather than pay by default —
    /// especially since today's default behavior (no timeline persisted)
    /// is exactly what an absent `[database]` section preserves.
    pub persist_qso_timeline: bool,
}

impl ConfigSection for DatabaseConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // A single bool: nothing to validate.
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.persist_qso_timeline = other.persist_qso_timeline;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_persistence_off() {
        // The whole point of a config-gated feature: an absent [database]
        // section must reproduce today's discard-everything behavior.
        assert!(!DatabaseConfig::default().persist_qso_timeline);
    }

    /// Regression test for the 2026-07-05 `merge_with` bug class
    /// (CLAUDE.md): a field present on the struct but missing its
    /// `self.x = other.x` line in `merge_with` silently reverts to default
    /// on merge, even though parsing and validation both succeed.
    #[test]
    fn merge_with_carries_over_persist_qso_timeline() {
        let mut base = DatabaseConfig {
            persist_qso_timeline: false,
        };
        let override_cfg = DatabaseConfig {
            persist_qso_timeline: true,
        };

        base.merge_with(override_cfg);

        assert!(
            base.persist_qso_timeline,
            "merge_with must carry persist_qso_timeline from the overriding config"
        );
    }
}
