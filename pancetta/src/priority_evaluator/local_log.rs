//! PAN-347: per-entity worked, confirmed and per-mode sets from the local
//! logbook (`qsos.adi`, through the index). Entities are the offline
//! resolver's `&'static str` names (`pancetta_core::dxcc`).
use pancetta_core::dxcc::canonical_entity;
use pancetta_tui::dx_need::EntityStanding;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(crate) struct LocalLog {
    /// Set once the first full read of the index succeeded; never cleared.
    pub(crate) available: bool,
    worked: HashSet<&'static str>,
    confirmed: HashSet<&'static str>,
    confirmed_on_band: HashMap<String, HashSet<&'static str>>,
    worked_in_mode: HashMap<String, HashSet<&'static str>>,
}

fn normalize_mode(mode: &str) -> Option<String> {
    let m = mode.trim().to_uppercase();
    (!m.is_empty()).then_some(m)
}

impl LocalLog {
    /// One logged contact with `entity`. An empty mode records no mode.
    pub(crate) fn record(&mut self, entity: &'static str, mode: &str, confirmed: bool) {
        let entity = canonical_entity(entity);
        self.worked.insert(entity);
        if confirmed {
            self.confirmed.insert(entity);
        }
        if let Some(m) = normalize_mode(mode) {
            self.worked_in_mode.entry(m).or_default().insert(entity);
        }
    }

    pub(crate) fn record_confirmation_on_band(&mut self, entity: &'static str, band: &str) {
        self.confirmed_on_band
            .entry(band.trim().to_uppercase())
            .or_default()
            .insert(entity);
    }

    pub(crate) fn is_confirmed(&self, entity: &str) -> bool {
        self.confirmed.contains(entity)
    }

    pub(crate) fn is_confirmed_on_band(&self, entity: &str, band: &str) -> bool {
        self.confirmed_on_band
            .get(band)
            .is_some_and(|set| set.contains(entity))
    }

    pub(crate) fn mark_worked(&mut self, entity: &'static str) {
        let entity = canonical_entity(entity);
        self.worked.insert(entity);
    }

    pub(crate) fn is_worked(&self, entity: &str) -> bool {
        self.worked.contains(canonical_entity(entity))
    }

    /// Confirmed beats worked-unconfirmed beats not worked.
    pub(crate) fn standing(&self, entity: &str) -> EntityStanding {
        let entity = canonical_entity(entity);
        if self.confirmed.contains(entity) {
            EntityStanding::Confirmed
        } else if self.worked.contains(entity) {
            EntityStanding::WorkedUnconfirmed
        } else {
            EntityStanding::NotWorked
        }
    }

    /// No contact with `entity` in `mode` on any band. None for an empty mode.
    pub(crate) fn new_in_mode(&self, entity: &str, mode: &str) -> Option<bool> {
        let entity = canonical_entity(entity);
        let m = normalize_mode(mode)?;
        Some(
            !self
                .worked_in_mode
                .get(&m)
                .is_some_and(|set| set.contains(entity)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DL: &str = "Fed. Rep. of Germany";

    #[test]
    fn subdivisions_share_worked_confirmed_and_mode_history() {
        let mut log = LocalLog::default();
        log.record("European Turkey", "FT8", true);
        assert!(log.is_worked("Asiatic Turkey"));
        assert_eq!(log.standing("Asiatic Turkey"), EntityStanding::Confirmed);
        assert_eq!(log.new_in_mode("Asiatic Turkey", "FT8"), Some(false));
        assert_eq!(log.new_in_mode("Asiatic Turkey", "FT4"), Some(true));
        log.record("unmapped entity", "FT8", false);
        assert!(log.is_worked("unmapped entity"));
    }

    #[test]
    fn standing_moves_from_not_worked_to_confirmed_and_never_back() {
        let mut log = LocalLog::default();
        assert_eq!(log.standing(DL), EntityStanding::NotWorked);
        log.record(DL, "FT8", false);
        assert_eq!(log.standing(DL), EntityStanding::WorkedUnconfirmed);
        log.record(DL, "FT8", true);
        assert_eq!(log.standing(DL), EntityStanding::Confirmed);
        log.record(DL, "FT4", false);
        assert_eq!(log.standing(DL), EntityStanding::Confirmed);
    }

    #[test]
    fn new_in_mode_is_per_entity_and_case_insensitive() {
        let mut log = LocalLog::default();
        log.record(DL, "FT8", false);
        assert_eq!(log.new_in_mode(DL, "FT4"), Some(true));
        assert_eq!(log.new_in_mode(DL, "FT8"), Some(false));
        assert_eq!(log.new_in_mode(DL, " ft8 "), Some(false));
        log.record(DL, "ft4", false);
        assert_eq!(log.new_in_mode(DL, "FT4"), Some(false));
        assert_eq!(log.new_in_mode(DL, ""), None);
    }

    #[test]
    fn empty_mode_records_worked_but_no_mode() {
        let mut log = LocalLog::default();
        log.record(DL, "  ", false);
        assert!(log.is_worked(DL));
        assert_eq!(log.new_in_mode(DL, "FT8"), Some(true));
    }
}
