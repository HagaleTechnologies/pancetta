//! Duplicate-QSO checking configuration.
//!
//! Corresponds to the `[duplicate_checking]` section in the TOML config file.
//! Threaded by the coordinator into `pancetta_qso::QsoManagerConfig` — the
//! defaults here MUST match `pancetta_qso::DuplicateCheckConfig::default()`
//! (guarded by `config_duplicate_defaults_match_qso_manager_defaults` in the
//! `pancetta` crate) so an absent section changes nothing.

use crate::{ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

/// Duplicate-QSO checking: refuse to call a station already worked recently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateCheckingConfig {
    /// Enable duplicate checking. When false, pancetta will happily call the
    /// same station again immediately.
    pub enabled: bool,
    /// A prior QSO only counts as a duplicate if it started within this many
    /// hours.
    pub time_window_hours: u32,
    /// When true, a prior QSO only blocks a re-call if it was within 50 Hz of
    /// the same RF frequency (so the same station on another band — or after a
    /// substantial QSY — can be worked again). When false, any QSO with that
    /// callsign inside the window blocks.
    pub check_frequency: bool,
}

impl Default for DuplicateCheckingConfig {
    fn default() -> Self {
        // MUST mirror pancetta-qso qso_manager.rs DuplicateCheckConfig::default().
        Self {
            enabled: true,
            time_window_hours: 24,
            check_frequency: true,
        }
    }
}

impl ConfigSection for DuplicateCheckingConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // All fields are bools or an inherently bounded u32; a 0-hour window
        // is coherent (nothing is ever inside it → checking effectively off).
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.enabled = other.enabled;
        self.time_window_hours = other.time_window_hours;
        self.check_frequency = other.check_frequency;
    }
}
