//! Coordinator-level priority evaluator wiring.
//!
//! Bridges `pancetta_qso::PriorityScorer` with the QSO database for
//! duplicate checking and DXCC need lookups.

use pancetta_qso::priority::WorkedStationLookup;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Derive the operator's home DXCC prefix from a callsign by stripping
/// at the first digit. Examples:
///   K5ARH  -> "K"
///   JA1ABC -> "JA"
///   WB9KMW -> "WB"
///   DL5XYZ -> "DL"
/// Returns the uppercase prefix, or `None` if the callsign has no
/// digit (unparseable). Note: this is a heuristic — for the operator's
/// own callsign it's accurate enough. The result is intended for the
/// "all-except-home" exclusion set, not for general DXCC lookup.
pub fn derive_prefix_from_callsign(callsign: &str) -> Option<String> {
    let upper = callsign.to_uppercase();
    let mut prefix = String::new();
    let mut found_digit = false;
    for c in upper.chars() {
        if c.is_ascii_digit() {
            found_digit = true;
            break;
        }
        if c.is_ascii_alphabetic() {
            prefix.push(c);
        } else {
            // Non-alpha, non-digit (e.g. '/') — stop, callsign
            // structure is unusual and we should bail.
            break;
        }
    }
    if !found_digit || prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Phase-5 hardening #2: compute the default "excluded DXCC prefixes"
/// set used when cqdx hasn't supplied a needed-set. The set covers:
///
///   - the operator's home DXCC, derived from their configured callsign
///     (e.g. K5ARH → K). When `dxcc_entity` is 291 (United States), the
///     full US prefix family is added: K, W, N, AA-AK.
///   - prefixes derived from each CALL field in the operator's ADIF
///     (already-worked stations' home DXCCs). Same callsign-prefix
///     heuristic.
///
/// If `adif_path` doesn't exist or isn't readable, ADIF prefixes are
/// silently skipped. Returns an upper-case `HashSet<String>`.
pub fn default_excluded_dxcc_prefixes(
    operator_callsign: &str,
    dxcc_entity: u16,
    adif_path: Option<&Path>,
) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    if let Some(p) = derive_prefix_from_callsign(operator_callsign) {
        out.insert(p);
    }
    if dxcc_entity == 291 {
        // United States: ITU has allocated K, W, N, AA-AK to the US.
        for p in [
            "K", "W", "N", "AA", "AB", "AC", "AD", "AE", "AF", "AG", "AH", "AI", "AJ", "AK",
        ] {
            out.insert(p.to_string());
        }
    }
    if let Some(path) = adif_path {
        if let Ok(text) = std::fs::read_to_string(path) {
            let calls = pancetta_qso::callsign_continuity::parse_adif_calls(&text);
            for call in calls {
                if let Some(p) = derive_prefix_from_callsign(&call) {
                    out.insert(p);
                }
            }
        }
    }
    out
}

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
    /// DXCC entity prefixes that are ATNO (all-time new ones — never worked
    /// on any band). A strict subset of `needed_dxcc`; populated from
    /// cqdx.io's `atno` flag. Empty/inert when cqdx is unconfigured.
    needed_atno: Arc<RwLock<HashSet<String>>>,
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
    /// Phase-5 hardening #2: callsign-prefix exclusions used when
    /// `needed_dxcc` is empty (cqdx unavailable). Populated from:
    ///
    /// - operator's own callsign (home DXCC)
    /// - ADIF CALL field of prior QSOs (already-worked DXCCs)
    ///
    /// When empty, behavior matches the pre-hardening "all needed"
    /// default; when populated, `is_needed_dxcc` returns true for
    /// every callsign whose uppercase prefix does NOT match any entry
    /// (i.e. "all entities except home + already-worked"). This avoids
    /// shipping a full DXCC entity list while still giving the
    /// autonomous operator a defensible signal: non-home calls are
    /// candidates, home calls aren't.
    excluded_dxcc_prefixes: Arc<RwLock<HashSet<String>>>,
}

impl Default for CachedStationLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedStationLookup {
    pub fn new() -> Self {
        Self {
            worked_on_band: Arc::new(RwLock::new(HashMap::new())),
            recent_failures: Arc::new(RwLock::new(HashSet::new())),
            needed_dxcc: Arc::new(RwLock::new(HashSet::new())),
            needed_atno: Arc::new(RwLock::new(HashSet::new())),
            needed_grids: Arc::new(RwLock::new(HashSet::new())),
            rarity_scores: Arc::new(RwLock::new(HashMap::new())),
            notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
            network_snr: Arc::new(RwLock::new(HashMap::new())),
            network_last_seen: Arc::new(RwLock::new(HashMap::new())),
            excluded_dxcc_prefixes: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Phase-5 hardening #2: install the set of callsign-prefix
    /// exclusions used when `needed_dxcc` is empty (cqdx unavailable
    /// or hasn't populated). Operator typically seeds this with:
    ///
    ///   - their own home DXCC prefixes (e.g. K, W, N, AA-AK for US)
    ///   - prefixes derived from worked QSOs in their ADIF
    ///
    /// Subsequent calls fully replace the set. Uppercase enforced.
    pub fn set_excluded_dxcc_prefixes(&self, prefixes: HashSet<String>) {
        let upper: HashSet<String> = prefixes.into_iter().map(|p| p.to_uppercase()).collect();
        *self.excluded_dxcc_prefixes.write() = upper;
    }

    /// Returns the current count of excluded prefixes (for logging).
    pub fn excluded_dxcc_prefix_count(&self) -> usize {
        self.excluded_dxcc_prefixes.read().len()
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

    /// Install the set of ATNO ("all-time new one") DXCC prefixes. Should
    /// be a subset of the `needed_dxcc` set. Uppercase enforced. Replaces
    /// the prior set on each call.
    pub fn update_needed_atno(&self, prefixes: HashSet<String>) {
        let upper: HashSet<String> = prefixes.into_iter().map(|p| p.to_uppercase()).collect();
        *self.needed_atno.write() = upper;
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
            .is_some_and(|set| set.contains(&callsign.to_uppercase()))
    }

    fn is_recent_failure(&self, callsign: &str) -> bool {
        self.recent_failures
            .read()
            .contains(&callsign.to_uppercase())
    }

    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        let needed = self.needed_dxcc.read();
        let upper = callsign.to_uppercase();
        if needed.is_empty() {
            // Phase-5 hardening #2: when cqdx hasn't supplied a needed
            // set, fall back to the "all-except-excluded" default.
            // Excluded = operator's home DXCC + already-worked DXCCs
            // (set by the coordinator at startup). This stops the
            // autonomous operator from scoring every CQ at ~needed
            // (which inflates every callsign to >threshold), while
            // still letting non-home / new-DXCC calls through.
            let excluded = self.excluded_dxcc_prefixes.read();
            if excluded.is_empty() {
                // No exclusions configured either — preserve the
                // historical "everything is needed" behavior so
                // existing tests / dev setups don't regress.
                return true;
            }
            // "Needed" = NOT in excluded prefix set.
            return !excluded
                .iter()
                .any(|prefix| upper.starts_with(prefix.as_str()));
        }
        // cqdx-populated `needed` set: prefix-match as before.
        needed
            .iter()
            .any(|prefix| upper.starts_with(prefix.as_str()))
    }

    fn is_atno(&self, callsign: &str) -> bool {
        let atno = self.needed_atno.read();
        if atno.is_empty() {
            return false;
        }
        let upper = callsign.to_uppercase();
        atno.iter().any(|prefix| upper.starts_with(prefix.as_str()))
    }

    fn is_needed_grid(&self, grid: &str) -> bool {
        let needed = self.needed_grids.read();
        // When the needed set is empty (no grid data available from cqdx.io),
        // return false — "unknown" means "no bonus" rather than inflating all
        // scores with the needed_grid weight.
        if needed.is_empty() {
            return false;
        }
        // Compare on the 4-char Maidenhead field, uppercased. The DX's
        // decoded grid may be 4 or 6 chars; the cqdx-populated set is stored
        // as 4-char fields (see CqdxBridge::startup normalization).
        let trimmed = grid.trim();
        if trimmed.len() < 4 {
            return false;
        }
        needed.contains(&trimmed[..4].to_uppercase())
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

    #[test]
    fn test_empty_needed_no_exclusions_keeps_all_needed_default() {
        // Phase-5 hardening #2: with neither cqdx data nor exclusions,
        // preserve historical "everything is needed" behavior (no
        // regression for dev / tests that depended on this).
        let lookup = CachedStationLookup::new();
        assert!(lookup.is_needed_dxcc("K1ABC"));
        assert!(lookup.is_needed_dxcc("JA1XYZ"));
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
    }

    #[test]
    fn test_empty_needed_with_exclusions_all_except_home() {
        // Phase-5 hardening #2: with US prefixes excluded and no
        // cqdx-populated needed set, "needed" becomes "anything
        // outside US". Mirrors the operator's typical configuration.
        let lookup = CachedStationLookup::new();
        let mut excluded = HashSet::new();
        for p in [
            "K", "W", "N", "AA", "AB", "AC", "AD", "AE", "AF", "AG", "AH", "AI", "AJ", "AK",
        ] {
            excluded.insert(p.to_string());
        }
        lookup.set_excluded_dxcc_prefixes(excluded);

        // US calls — NOT needed
        assert!(!lookup.is_needed_dxcc("K5ARH"));
        assert!(!lookup.is_needed_dxcc("W1ABC"));
        assert!(!lookup.is_needed_dxcc("N9ZZ"));
        assert!(!lookup.is_needed_dxcc("AA1XX"));

        // Non-US calls — needed
        assert!(lookup.is_needed_dxcc("JA1ABC"));
        assert!(lookup.is_needed_dxcc("DL5XYZ"));
        assert!(lookup.is_needed_dxcc("VK2DEF"));
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
    }

    #[test]
    fn test_cqdx_needed_set_wins_over_exclusions() {
        // When cqdx populates needed_dxcc, the exclusion fallback is
        // bypassed entirely — needed-set semantics rule.
        let lookup = CachedStationLookup::new();
        // Configure exclusions (would normally apply)
        let mut excluded = HashSet::new();
        excluded.insert("K".to_string());
        lookup.set_excluded_dxcc_prefixes(excluded);
        // But cqdx says only Bouvet is needed
        let mut needed = HashSet::new();
        needed.insert("3Y/B".to_string());
        lookup.update_needed_dxcc(needed);

        // K5ARH should NOT be needed (not in cqdx-needed set)
        assert!(!lookup.is_needed_dxcc("K5ARH"));
        // JA1ABC — also not in cqdx-needed
        assert!(!lookup.is_needed_dxcc("JA1ABC"));
        // 3Y/B1234 — is in cqdx-needed
        assert!(lookup.is_needed_dxcc("3Y/B1234"));
    }

    #[test]
    fn test_atno_empty_set_is_inert() {
        // No ATNO data loaded: is_atno is false for everything.
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_atno("3Y/B1234"));
        assert!(!lookup.is_atno("K5ARH"));
    }

    #[test]
    fn test_atno_prefix_match() {
        let lookup = CachedStationLookup::new();
        let mut atno = HashSet::new();
        atno.insert("3Y/B".to_string());
        atno.insert("ja".to_string()); // lower-case is normalized on update
        lookup.update_needed_atno(atno);

        assert!(lookup.is_atno("3Y/B1234"));
        assert!(lookup.is_atno("JA1ABC")); // case-insensitive prefix
        assert!(!lookup.is_atno("DL5XYZ")); // not in ATNO set
    }

    #[test]
    fn test_atno_bonus_lifts_score_over_plain_needed() {
        // An ATNO entity should score strictly higher than the same entity
        // when only band-needed (not ATNO), via the atno_bonus weight.
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        use pancetta_qso::DxEvaluator;

        let mut needed = HashSet::new();
        needed.insert("3Y/B".to_string());

        // Lookup A: needed but NOT atno.
        let plain = CachedStationLookup::new();
        plain.update_needed_dxcc(needed.clone());
        let plain_scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(plain));
        let plain_score = plain_scorer.evaluate_cq("3Y/B1234", None, -10, 14_074_000.0);

        // Lookup B: needed AND atno.
        let atno_lookup = CachedStationLookup::new();
        atno_lookup.update_needed_dxcc(needed.clone());
        atno_lookup.update_needed_atno(needed);
        let atno_scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(atno_lookup));
        let atno_score = atno_scorer.evaluate_cq("3Y/B1234", None, -10, 14_074_000.0);

        assert!(
            atno_score > plain_score,
            "ATNO ({atno_score}) should outscore plain-needed ({plain_score})"
        );
    }

    #[test]
    fn test_needed_grids_empty_set_is_inert() {
        // No cqdx grid data loaded: nothing is "needed" — preserves the
        // historical behavior so the needed_grid weight doesn't inflate
        // every score.
        let lookup = CachedStationLookup::new();
        assert!(!lookup.is_needed_grid("FN42"));
        assert!(!lookup.is_needed_grid("JD15"));
    }

    #[test]
    fn test_update_needed_grids_marks_needed() {
        let lookup = CachedStationLookup::new();
        let mut needed = HashSet::new();
        needed.insert("JD15".to_string());
        needed.insert("FN42".to_string());
        lookup.update_needed_grids(needed);

        // In the set — needed.
        assert!(lookup.is_needed_grid("JD15"));
        assert!(lookup.is_needed_grid("FN42"));
        // Case-insensitive match.
        assert!(lookup.is_needed_grid("jd15"));
        // 6-char locator normalizes to its 4-char field before comparison.
        assert!(lookup.is_needed_grid("JD15kl"));
        // Not in the set — not needed.
        assert!(!lookup.is_needed_grid("PM95"));
        // Too short to be a valid field — not needed.
        assert!(!lookup.is_needed_grid("JD"));
    }

    #[test]
    fn test_priority_score_non_us_high_with_default_exclusions() {
        // Phase-5 hardening #2 success criteria: a Japanese CQ
        // (DXCC 339) must score ≥ 0.35 when default exclusions are
        // US-only, and a US CQ must score < 0.30 (won't respond).
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        use pancetta_qso::DxEvaluator;
        let lookup = CachedStationLookup::new();
        let mut excluded = HashSet::new();
        for p in [
            "K", "W", "N", "AA", "AB", "AC", "AD", "AE", "AF", "AG", "AH", "AI", "AJ", "AK",
        ] {
            excluded.insert(p.to_string());
        }
        lookup.set_excluded_dxcc_prefixes(excluded);

        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let ja_score = scorer.evaluate_cq("JA1ABC", Some("PM95"), -10, 14_074_000.0);
        let us_score = scorer.evaluate_cq("K5ARH", Some("EM12"), -10, 14_074_000.0);

        assert!(
            ja_score >= 0.35,
            "non-home (JA) call should score >= 0.35; got {}",
            ja_score
        );
        assert!(
            us_score < 0.30,
            "home (US) call should score < 0.30; got {}",
            us_score
        );
    }

    #[test]
    fn test_derive_prefix_from_callsign() {
        assert_eq!(derive_prefix_from_callsign("K5ARH"), Some("K".into()));
        assert_eq!(derive_prefix_from_callsign("JA1ABC"), Some("JA".into()));
        assert_eq!(derive_prefix_from_callsign("WB9KMW"), Some("WB".into()));
        assert_eq!(derive_prefix_from_callsign("DL5XYZ"), Some("DL".into()));
        assert_eq!(derive_prefix_from_callsign("k5arh"), Some("K".into())); // case-insensitive
        assert_eq!(derive_prefix_from_callsign("NODIGITS"), None);
    }

    #[test]
    fn test_default_exclusions_us() {
        // Operator K5ARH, US (291), no ADIF
        let excluded = default_excluded_dxcc_prefixes("K5ARH", 291, None);
        for p in [
            "K", "W", "N", "AA", "AB", "AC", "AD", "AE", "AF", "AG", "AH", "AI", "AJ", "AK",
        ] {
            assert!(excluded.contains(p), "US prefix '{}' missing", p);
        }
        // Non-US prefix shouldn't be present
        assert!(!excluded.contains("JA"));
        assert!(!excluded.contains("DL"));
    }

    #[test]
    fn test_default_exclusions_non_us_operator() {
        // German operator: DL5XYZ, DXCC 230, no ADIF — only "DL"
        // gets added (no special-case prefix family).
        let excluded = default_excluded_dxcc_prefixes("DL5XYZ", 230, None);
        assert!(excluded.contains("DL"));
        assert!(!excluded.contains("K"));
        assert!(!excluded.contains("JA"));
    }
}
