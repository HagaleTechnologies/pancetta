//! In-memory session cache for cqdx.io data.
//!
//! Holds DXCC entities, needed status, rarity scores, and live spot groups.
//! Populated on startup from cqdx.io API, refreshed by polling.

use crate::types::{rank_to_rarity, DxccEntity, NeededEntity, SpotGroup};
use std::collections::{HashMap, HashSet};

/// In-memory cache of cqdx.io data for the current session.
#[derive(Debug, Clone)]
pub struct CqdxCache {
    /// Entity details by ADIF number.
    entities: HashMap<u32, DxccEntity>,
    /// Entity IDs the user still needs. None = no data loaded (needed status
    /// unknown); `Some` of an empty set = cqdx answered "nothing needed".
    needed_entity_ids: Option<HashSet<u32>>,
    /// Rarity scores from live spot groups, keyed by uppercase callsign.
    rarity_scores: HashMap<String, f64>,
    /// Latest live spot group poll results.
    spot_groups: Vec<SpotGroup>,
}

impl Default for CqdxCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CqdxCache {
    pub fn new() -> Self {
        Self {
            entities: HashMap::new(),
            needed_entity_ids: None,
            rarity_scores: HashMap::new(),
            spot_groups: Vec::new(),
        }
    }

    /// Load entity metadata independently of offline callsign resolution.
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>) {
        self.entities = entities.into_iter().map(|e| (e.adif_number, e)).collect();
    }

    /// Load needed entity IDs. Calling this with an empty vec means nothing is needed.
    pub fn load_needed(&mut self, needed: Vec<NeededEntity>) {
        let ids: HashSet<u32> = needed.iter().map(|n| n.entity_id).collect();
        self.needed_entity_ids = Some(ids);
    }

    /// Update live spot groups from latest poll. Also updates rarity cache.
    /// Uses upsert (not clear) so callsigns that drop out of the current window
    /// retain their last-known rarity until replaced by a new poll result.
    pub fn update_spot_groups(&mut self, groups: Vec<SpotGroup>) {
        for group in &groups {
            let rarity = rank_to_rarity(group.rarity_rank);
            self.rarity_scores
                .insert(group.dx_call.to_uppercase(), rarity);
        }
        self.spot_groups = groups;
    }

    /// Resolve through the shared offline DXCC identity rules.
    pub fn resolve_entity(&self, callsign: &str) -> Option<u32> {
        pancetta_core::dxcc::entity_id_for_callsign(callsign).map(u32::from)
    }

    /// Get rarity score for a callsign. Returns 0.5 (default) if unknown.
    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    /// Check if a callsign's DXCC entity is still needed.
    ///
    /// PAN-93: `None` while no needed data is loaded (status unknown, never
    /// "everything needed"); otherwise `Some(true)` when the entity is in the
    /// needed set and `Some(false)` when it is not, cannot be resolved, or
    /// the loaded set is empty.
    pub fn is_needed_dxcc(&self, callsign: &str) -> Option<bool> {
        let ids = self.needed_entity_ids.as_ref()?;
        Some(
            self.resolve_entity(callsign)
                .is_some_and(|entity_id| ids.contains(&entity_id)),
        )
    }

    /// Get current live spot groups for frequency nudge decisions.
    pub fn spot_groups(&self) -> &[SpotGroup] {
        &self.spot_groups
    }

    /// hb-062: return the set of uppercase callsigns currently in the cache
    /// (from spot_groups + rarity_scores). Used by CallsignContinuityFilter
    /// as the cqdx source for the production FP filter.
    pub fn spotted_callsigns(&self) -> std::collections::HashSet<String> {
        let mut out: std::collections::HashSet<String> =
            self.rarity_scores.keys().cloned().collect();
        for g in &self.spot_groups {
            out.insert(g.dx_call.to_uppercase());
        }
        out
    }
}

/// Derive ham radio band name from frequency in Hz.
///
/// PAN-330: core band membership plus the core lowercase name — the old
/// integer-MHz buckets (e.g. all of 5–6 MHz → `60m`) are gone, so
/// 5,410,000 Hz now classifies as no band instead of `60m`, and 2m/70cm
/// resolve where core supports them.
pub fn frequency_to_band(freq_hz: u64) -> Option<String> {
    pancetta_core::Band::from_frequency(freq_hz).map(|b| b.name().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    /// PAN-330 AC3: cqdx frequency classification is core band membership
    /// with the core lowercase label — including the old broad-MHz-bucket
    /// corrections at the 60 m edge.
    #[test]
    fn band_vocabulary_table() {
        for (hz, expected) in [
            (1_800_000u64, Some("160m")),
            (2_000_000, Some("160m")),
            (3_500_000, Some("80m")),
            (5_330_000, Some("60m")),
            (5_357_500, Some("60m")),
            (5_405_000, Some("60m")),
            (7_074_000, Some("40m")),
            (14_074_000, Some("20m")),
            (50_313_000, Some("6m")),
            (54_000_000, Some("6m")),
            (144_174_000, Some("2m")),
            (432_100_000, Some("70cm")),
        ] {
            assert_eq!(frequency_to_band(hz).as_deref(), expected, "at {hz} Hz");
        }
        // The old integer-MHz buckets are gone: adjacent MHz inside an old
        // bucket and the 60 m spillover are no band, and edge ±1 Hz holds.
        for hz in [
            5_410_000u64, // old bucket said 60m; core says none
            5_329_999,
            5_405_001,
            6_000_000,
            8_000_000,
            11_000_000,
            15_000_000,
            30_000_000,
            54_000_001,
            0,
        ] {
            assert_eq!(frequency_to_band(hz), None, "expected no band at {hz} Hz");
        }
    }

    fn sample_entities() -> Vec<DxccEntity> {
        vec![
            DxccEntity {
                adif_number: 291,
                entity_name: "United States".to_string(),
                prefix: "K".to_string(),
                continent: "NA".to_string(),
                cq_zone: 5,
                itu_zone: 8,
                rarity_rank: Some(340),
                rarity_tier: "common".to_string(),
                is_deleted: false,
            },
            DxccEntity {
                adif_number: 339,
                entity_name: "Japan".to_string(),
                prefix: "JA".to_string(),
                continent: "AS".to_string(),
                cq_zone: 25,
                itu_zone: 45,
                rarity_rank: Some(300),
                rarity_tier: "common".to_string(),
                is_deleted: false,
            },
            DxccEntity {
                adif_number: 24,
                entity_name: "Bouvet Island".to_string(),
                prefix: "3Y/B".to_string(),
                continent: "AF".to_string(),
                cq_zone: 38,
                itu_zone: 67,
                rarity_rank: Some(1),
                rarity_tier: "legendary".to_string(),
                is_deleted: false,
            },
        ]
    }

    fn sample_spot_group(dx_call: &str, rarity_rank: Option<u32>) -> SpotGroup {
        SpotGroup {
            dx_call: dx_call.to_string(),
            band: "20m".to_string(),
            mode: "FT8".to_string(),
            dx_dxcc: 24,
            dx_entity_name: "Bouvet Island".to_string(),
            dx_continent: "AF".to_string(),
            dx_cq_zone: 38,
            dx_grid: Some("JD15".to_string()),
            rarity_rank,
            rarity_tier: "legendary".to_string(),
            frequency: 14074000,
            best_snr: Some(-12),
            reporter_count: 5,
            sources: vec!["pskreporter".to_string()],
            first_seen: 1743688920,
            last_seen: 1743689040,
            confidence: 4.2,
            is_notable: false,
            notable_type: None,
        }
    }

    #[test]
    fn cache_uses_core_entity_ids_without_entity_load() {
        let mut cache = CqdxCache::new();
        assert_eq!(cache.is_needed_dxcc("7K1ABC"), None);
        cache.load_needed(vec![NeededEntity {
            entity_id: 339,
            name: "Japan".into(),
            prefix: "JA".into(),
            atno: true,
        }]);
        for call in ["JA1ABC", "7K1ABC", "JR1ABC", "JH1ABC", "8N1ABC", "JE1ABC"] {
            assert_eq!(cache.resolve_entity(call), Some(339), "{call}");
            assert_eq!(cache.is_needed_dxcc(call), Some(true), "{call}");
        }
        assert_eq!(cache.resolve_entity("3Y0J"), Some(24));
        assert_eq!(cache.resolve_entity("KG4OJT"), Some(291));
        assert_eq!(cache.resolve_entity("KG4AC"), Some(105));
        cache.load_needed(vec![]);
        assert_eq!(cache.is_needed_dxcc("7K1ABC"), Some(false));
    }

    #[test]
    fn test_resolve_entity_by_prefix() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("K1ABC"), Some(291));
        assert_eq!(cache.resolve_entity("JA1XYZ"), Some(339));
    }

    #[test]
    fn test_resolve_entity_longest_prefix_wins() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("3Y0J"), Some(24));
    }

    #[test]
    fn test_resolve_entity_unknown_returns_none() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("QQ9XYZ"), None);
    }

    #[test]
    fn test_rarity_from_spot_groups() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.update_spot_groups(vec![sample_spot_group("3Y0J", Some(1))]);
        // rank 1 → rarity 1.0
        assert!((cache.rarity("3Y0J") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rarity_common_station() {
        let mut cache = CqdxCache::new();
        cache.update_spot_groups(vec![sample_spot_group("K1ABC", Some(340))]);
        // rank 340 → rarity ~0.0
        assert!(cache.rarity("K1ABC") < 0.01);
    }

    #[test]
    fn test_rarity_unknown_callsign_returns_default() {
        let cache = CqdxCache::new();
        assert!((cache.rarity("W1ABC") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rarity_null_rank_returns_default() {
        let mut cache = CqdxCache::new();
        cache.update_spot_groups(vec![sample_spot_group("XX9XX", None)]);
        assert!((cache.rarity("XX9XX") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_is_needed_dxcc_with_data() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.load_needed(vec![NeededEntity {
            entity_id: 24,
            name: "Bouvet Island".to_string(),
            prefix: "3Y/B".to_string(),
            atno: true,
        }]);
        assert_eq!(cache.is_needed_dxcc("3Y0J"), Some(true)); // Bouvet is needed
        assert_eq!(cache.is_needed_dxcc("K1ABC"), Some(false)); // US is NOT needed
    }

    #[test]
    fn test_is_needed_dxcc_unloaded_is_unknown() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        // PAN-93: no needed data loaded -> unknown, not "everything needed".
        assert_eq!(cache.is_needed_dxcc("K1ABC"), None);
        assert_eq!(cache.is_needed_dxcc("JA1XYZ"), None);
    }

    #[test]
    fn test_is_needed_dxcc_loaded_empty_is_not_needed() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.load_needed(vec![]);
        // PAN-93: a successful empty answer means nothing is needed.
        assert_eq!(cache.is_needed_dxcc("K1ABC"), Some(false));
        assert_eq!(cache.is_needed_dxcc("3Y0J"), Some(false));
    }

    #[test]
    fn test_spot_groups_accessor() {
        let mut cache = CqdxCache::new();
        assert!(cache.spot_groups().is_empty());
        cache.update_spot_groups(vec![sample_spot_group("3Y0J", Some(1))]);
        assert_eq!(cache.spot_groups().len(), 1);
    }

    #[test]
    fn test_spotted_callsigns_returns_uppercase_callsigns() {
        let mut cache = CqdxCache::new();
        assert!(cache.spotted_callsigns().is_empty());
        cache.update_spot_groups(vec![
            sample_spot_group("3y0j", Some(1)),
            sample_spot_group("k1abc", Some(500)),
        ]);
        let calls = cache.spotted_callsigns();
        assert!(calls.contains("3Y0J"));
        assert!(calls.contains("K1ABC"));
        assert_eq!(calls.len(), 2);
    }
}
