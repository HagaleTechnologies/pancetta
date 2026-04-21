//! Coordinator-level priority evaluator wiring.
//!
//! Bridges `pancetta_qso::PriorityScorer` with the QSO database for
//! duplicate checking and DXCC need lookups.

use pancetta_qso::priority::WorkedStationLookup;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Cached station lookup that holds a snapshot of worked stations.
///
/// Refreshed periodically by the coordinator. The `PriorityScorer` calls
/// this synchronously via the `WorkedStationLookup` trait.
#[derive(Debug, Clone)]
pub struct CachedStationLookup {
    /// Callsigns worked per band.  Key = uppercase band name (e.g. "20M"),
    /// value = set of uppercased callsigns worked on that band.
    worked_on_band: Arc<RwLock<HashMap<String, HashSet<String>>>>,
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
            worked_on_band: Arc::new(RwLock::new(HashMap::new())),
            recent_failures: Arc::new(RwLock::new(HashSet::new())),
            needed_dxcc: Arc::new(RwLock::new(HashSet::new())),
            needed_grids: Arc::new(RwLock::new(HashSet::new())),
            rarity_scores: Arc::new(RwLock::new(HashMap::new())),
            notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
            network_snr: Arc::new(RwLock::new(HashMap::new())),
            network_last_seen: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Seed `worked_on_band` for `band` from a list of callsigns loaded out-of-band
    /// (e.g. from the QSO database at startup).  Both the band key and callsigns
    /// are uppercased for consistent comparison.
    pub fn seed_worked_from_list(&self, band: &str, callsigns: Vec<String>) {
        let mut map = self.worked_on_band.write();
        let set = map.entry(band.to_uppercase()).or_default();
        for call in callsigns {
            set.insert(call.to_uppercase());
        }
        tracing::info!(
            "CachedStationLookup: seeded {} worked station(s) on {} from QSO database",
            set.len(),
            band
        );
    }

    pub fn update_recent_failures(&self, callsigns: HashSet<String>) {
        *self.recent_failures.write() = callsigns;
    }

    pub fn update_needed_dxcc(&self, patterns: HashSet<String>) {
        *self.needed_dxcc.write() = patterns;
    }

    pub fn update_needed_grids(&self, grids: HashSet<String>) {
        *self.needed_grids.write() = grids;
    }

    pub fn update_rarity_scores(&self, scores: HashMap<String, f64>) {
        *self.rarity_scores.write() = scores;
    }

    pub fn update_notable_callsigns(&self, callsigns: HashSet<String>) {
        *self.notable_callsigns.write() = callsigns;
    }

    pub fn update_network_snr(&self, data: HashMap<String, (u32, i32)>) {
        *self.network_snr.write() = data;
    }

    pub fn update_network_last_seen(&self, data: HashMap<String, i64>) {
        *self.network_last_seen.write() = data;
    }

    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    pub fn record_failure(&self, callsign: &str) {
        self.recent_failures.write().insert(callsign.to_uppercase());
    }

    pub fn record_worked(&self, callsign: &str, band: &str) {
        self.worked_on_band
            .write()
            .entry(band.to_uppercase())
            .or_default()
            .insert(callsign.to_uppercase());
    }
}

impl WorkedStationLookup for CachedStationLookup {
    fn is_duplicate(&self, callsign: &str, freq_hz: f64) -> bool {
        let band = pancetta_qso::utils::frequency_to_band(freq_hz).to_uppercase();
        let worked = self.worked_on_band.read();
        worked
            .get(&band)
            .map_or(false, |set| set.contains(&callsign.to_uppercase()))
    }

    fn is_recent_failure(&self, callsign: &str) -> bool {
        self.recent_failures
            .read()
            .contains(&callsign.to_uppercase())
    }

    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        let needed = self.needed_dxcc.read();
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
        let needed = self.needed_grids.read();
        // When the needed set is empty (no grid data available from cqdx.io),
        // return false — "unknown" means "no bonus" rather than inflating all
        // scores with the needed_grid weight.
        if needed.is_empty() {
            return false;
        }
        needed.contains(&grid.to_uppercase())
    }

    fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .read()
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    fn is_notable(&self, callsign: &str) -> bool {
        self.notable_callsigns
            .read()
            .contains(&callsign.to_uppercase())
    }

    fn network_snr(&self, callsign: &str) -> Option<(u32, i32)> {
        self.network_snr
            .read()
            .get(&callsign.to_uppercase())
            .copied()
    }

    fn network_last_seen(&self, callsign: &str) -> Option<i64> {
        self.network_last_seen
            .read()
            .get(&callsign.to_uppercase())
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_band_aware_duplicate() {
        let lookup = CachedStationLookup::new();

        // Work K9ZZ on 20m
        lookup.record_worked("K9ZZ", "20m");

        // Should be duplicate on 20m
        assert!(lookup.is_duplicate("K9ZZ", 14_074_000.0));

        // Should NOT be duplicate on 40m
        assert!(!lookup.is_duplicate("K9ZZ", 7_074_000.0));

        // Should NOT be duplicate on 15m
        assert!(!lookup.is_duplicate("K9ZZ", 21_074_000.0));

        // Work K9ZZ on 40m too
        lookup.record_worked("K9ZZ", "40m");

        // Now duplicate on both bands
        assert!(lookup.is_duplicate("K9ZZ", 14_074_000.0));
        assert!(lookup.is_duplicate("K9ZZ", 7_074_000.0));

        // Still not on 15m
        assert!(!lookup.is_duplicate("K9ZZ", 21_074_000.0));
    }

    #[test]
    fn test_unknown_frequency_not_duplicate() {
        let lookup = CachedStationLookup::new();
        lookup.record_worked("K9ZZ", "20M");
        // freq_hz=0.0 (uninitialized) should not match any band
        assert!(!lookup.is_duplicate("K9ZZ", 0.0));
    }

    #[test]
    fn test_seed_worked_from_list() {
        let lookup = CachedStationLookup::new();
        lookup.seed_worked_from_list("20m", vec!["W1ABC".into(), "K2DEF".into()]);

        assert!(lookup.is_duplicate("W1ABC", 14_074_000.0));
        assert!(lookup.is_duplicate("K2DEF", 14_074_000.0));
        assert!(!lookup.is_duplicate("W1ABC", 7_074_000.0)); // not on 40m
    }
}
