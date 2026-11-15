//! Database / QSO-log persistence configuration.
//!
//! Corresponds to the `[database]` section in the TOML config file. Threaded
//! by the coordinator into `pancetta_qso::async_logger::LoggerConfig`.

use crate::{ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Database / QSO-log persistence settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Make a changed ADIF snapshot on live startup.
    pub adif_backup_enabled: bool,
    /// Absolute directory, home-relative directory, or path beside qsos.adi.
    pub adif_backup_directory: String,
    /// Maximum number of owned ADIF snapshots to keep.
    pub adif_backup_retain_count: usize,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            persist_qso_timeline: false,
            adif_backup_enabled: true,
            adif_backup_directory: "backups".into(),
            adif_backup_retain_count: 7,
        }
    }
}

impl ConfigSection for DatabaseConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.adif_backup_directory.trim().is_empty() {
            return Err(crate::ConfigError::Validation(
                "database.adif_backup_directory must not be blank".into(),
            ));
        }
        if self.adif_backup_retain_count == 0 {
            return Err(crate::ConfigError::Validation(
                "database.adif_backup_retain_count must be positive".into(),
            ));
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.persist_qso_timeline = other.persist_qso_timeline;
        self.adif_backup_enabled = other.adif_backup_enabled;
        self.adif_backup_directory = other.adif_backup_directory;
        self.adif_backup_retain_count = other.adif_backup_retain_count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adif_backup_defaults() {
        let c: DatabaseConfig = toml::from_str("").unwrap();
        assert!(c.adif_backup_enabled);
        assert_eq!(c.adif_backup_directory, "backups");
        assert_eq!(c.adif_backup_retain_count, 7);
    }

    #[test]
    fn adif_backup_validation() {
        for (directory, count, field) in [(" ", 7, "directory"), ("backups", 0, "retain_count")] {
            let c = DatabaseConfig {
                adif_backup_directory: directory.into(),
                adif_backup_retain_count: count,
                ..Default::default()
            };
            assert!(c
                .validate_section()
                .unwrap_err()
                .to_string()
                .contains(&format!("database.adif_backup_{field}")));
        }
    }

    #[test]
    fn merge_with_carries_adif_backup_settings() {
        let other: DatabaseConfig = toml::from_str(
            r#"adif_backup_enabled = false
adif_backup_directory = "/archive"
adif_backup_retain_count = 3"#,
        )
        .unwrap();
        other.validate_section().unwrap();
        let mut base = DatabaseConfig::default();
        base.merge_with(other);
        assert!(!base.adif_backup_enabled);
        assert_eq!(base.adif_backup_directory, "/archive");
        assert_eq!(base.adif_backup_retain_count, 3);
    }

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
            ..Default::default()
        };
        let override_cfg = DatabaseConfig {
            persist_qso_timeline: true,
            ..Default::default()
        };

        base.merge_with(override_cfg);

        assert!(
            base.persist_qso_timeline,
            "merge_with must carry persist_qso_timeline from the overriding config"
        );
    }
}
