//! Coordinator-level priority evaluator wiring.
//!
//! Bridges `pancetta_qso::PriorityScorer` with the QSO database for
//! duplicate checking and DXCC need lookups.

use pancetta_qso::priority::WorkedStationLookup;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Cached station lookup that holds a snapshot of worked stations.
///
/// Refreshed periodically by the coordinator. The `PriorityScorer` calls
/// this synchronously via the `WorkedStationLookup` trait.
#[derive(Debug, Clone)]
pub struct CachedStationLookup {
    /// Callsigns worked on the current band.
    worked_on_band: Arc<RwLock<HashSet<String>>>,
    /// Callsigns where a recent QSO attempt failed.
    recent_failures: Arc<RwLock<HashSet<String>>>,
    /// DXCC entities still needed.
    needed_dxcc: Arc<RwLock<HashSet<String>>>,
    /// Grid squares still needed for award tracking.
    needed_grids: Arc<RwLock<HashSet<String>>>,
}

impl CachedStationLookup {
    pub fn new() -> Self {
        Self {
            worked_on_band: Arc::new(RwLock::new(HashSet::new())),
            recent_failures: Arc::new(RwLock::new(HashSet::new())),
            needed_dxcc: Arc::new(RwLock::new(HashSet::new())),
            needed_grids: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    pub fn update_worked_on_band(&self, callsigns: HashSet<String>) {
        *self.worked_on_band.write().unwrap() = callsigns;
    }

    pub fn update_recent_failures(&self, callsigns: HashSet<String>) {
        *self.recent_failures.write().unwrap() = callsigns;
    }

    pub fn update_needed_dxcc(&self, patterns: HashSet<String>) {
        *self.needed_dxcc.write().unwrap() = patterns;
    }

    pub fn update_needed_grids(&self, grids: HashSet<String>) {
        *self.needed_grids.write().unwrap() = grids;
    }

    pub fn record_failure(&self, callsign: &str) {
        self.recent_failures.write().unwrap().insert(callsign.to_uppercase());
    }

    pub fn record_worked(&self, callsign: &str) {
        self.worked_on_band.write().unwrap().insert(callsign.to_uppercase());
    }
}

impl WorkedStationLookup for CachedStationLookup {
    fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.worked_on_band.read().unwrap().contains(&callsign.to_uppercase())
    }

    fn is_recent_failure(&self, callsign: &str) -> bool {
        self.recent_failures.read().unwrap().contains(&callsign.to_uppercase())
    }

    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        let needed = self.needed_dxcc.read().unwrap();
        needed.is_empty() || needed.contains(&callsign.to_uppercase())
    }

    fn is_needed_grid(&self, grid: &str) -> bool {
        let needed = self.needed_grids.read().unwrap();
        needed.is_empty() || needed.contains(&grid.to_uppercase())
    }
}
