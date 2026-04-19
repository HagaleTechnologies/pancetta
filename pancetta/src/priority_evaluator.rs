//! Coordinator-level priority evaluator wiring.
//!
//! Bridges `pancetta_qso::PriorityScorer` with the QSO database for
//! duplicate checking and DXCC need lookups.

use pancetta_qso::priority::WorkedStationLookup;
use std::collections::{HashMap, HashSet};
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
    /// Rarity scores from cqdx.io, keyed by uppercase callsign.
    rarity_scores: Arc<RwLock<HashMap<String, f64>>>,
    /// Notable callsigns from cqdx.io spot groups.
    notable_callsigns: Arc<RwLock<HashSet<String>>>,
    /// Network SNR data: callsign -> (reporter_count, best_snr).
    network_snr: Arc<RwLock<HashMap<String, (u32, i32)>>>,
    /// Network last-seen timestamps: callsign -> unix timestamp.
    network_last_seen: Arc<RwLock<HashMap<String, i64>>>,
}

impl CachedStationLookup {
    pub fn new() -> Self {
        Self {
            worked_on_band: Arc::new(RwLock::new(HashSet::new())),
            recent_failures: Arc::new(RwLock::new(HashSet::new())),
            needed_dxcc: Arc::new(RwLock::new(HashSet::new())),
            needed_grids: Arc::new(RwLock::new(HashSet::new())),
            rarity_scores: Arc::new(RwLock::new(HashMap::new())),
            notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
            network_snr: Arc::new(RwLock::new(HashMap::new())),
            network_last_seen: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Seed `worked_on_band` from a list of callsigns loaded out-of-band
    /// (e.g. from the QSO database at startup).  Callsigns are uppercased
    /// for consistent comparison.
    pub fn seed_worked_from_list(&self, callsigns: Vec<String>) {
        let mut set = self.worked_on_band.write().unwrap();
        for call in callsigns {
            set.insert(call.to_uppercase());
        }
        tracing::info!(
            "CachedStationLookup: seeded {} worked station(s) from QSO database",
            set.len()
        );
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

    pub fn update_rarity_scores(&self, scores: HashMap<String, f64>) {
        *self.rarity_scores.write().unwrap() = scores;
    }

    pub fn update_notable_callsigns(&self, callsigns: HashSet<String>) {
        *self.notable_callsigns.write().unwrap() = callsigns;
    }

    pub fn update_network_snr(&self, data: HashMap<String, (u32, i32)>) {
        *self.network_snr.write().unwrap() = data;
    }

    pub fn update_network_last_seen(&self, data: HashMap<String, i64>) {
        *self.network_last_seen.write().unwrap() = data;
    }

    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    pub fn record_failure(&self, callsign: &str) {
        self.recent_failures
            .write()
            .unwrap()
            .insert(callsign.to_uppercase());
    }

    pub fn record_worked(&self, callsign: &str) {
        self.worked_on_band
            .write()
            .unwrap()
            .insert(callsign.to_uppercase());
    }
}

impl WorkedStationLookup for CachedStationLookup {
    fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.worked_on_band
            .read()
            .unwrap()
            .contains(&callsign.to_uppercase())
    }

    fn is_recent_failure(&self, callsign: &str) -> bool {
        self.recent_failures
            .read()
            .unwrap()
            .contains(&callsign.to_uppercase())
    }

    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        let needed = self.needed_dxcc.read().unwrap();
        // Conservative policy: when no DXCC filter is configured (empty set),
        // treat every entity as needed.
        if needed.is_empty() {
            return true;
        }
        // The set contains DXCC prefixes (e.g., "3Y/B"), not full callsigns.
        // Use prefix matching: callsign "3Y/B1234" matches prefix "3Y/B".
        let upper = callsign.to_uppercase();
        needed
            .iter()
            .any(|prefix| upper.starts_with(prefix.as_str()))
    }

    fn is_needed_grid(&self, grid: &str) -> bool {
        let needed = self.needed_grids.read().unwrap();
        // Same conservative policy as is_needed_dxcc — see comment above.
        needed.is_empty() || needed.contains(&grid.to_uppercase())
    }

    fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    fn is_notable(&self, callsign: &str) -> bool {
        self.notable_callsigns
            .read()
            .unwrap()
            .contains(&callsign.to_uppercase())
    }

    fn network_snr(&self, callsign: &str) -> Option<(u32, i32)> {
        self.network_snr
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
    }

    fn network_last_seen(&self, callsign: &str) -> Option<i64> {
        self.network_last_seen
            .read()
            .unwrap()
            .get(&callsign.to_uppercase())
            .copied()
    }
}
