//! In-memory session cache for cqdx.io data.
//!
//! Holds DXCC entities, needed status, rarity scores, and live spot groups.
//! Populated on startup from cqdx.io API, refreshed by polling.

use crate::types::{rank_to_rarity, DxccEntity, NeededEntity, SpotGroup};
use std::collections::{HashMap, HashSet};

/// In-memory cache of cqdx.io data for the current session.
#[derive(Debug, Clone)]
pub struct CqdxCache {
    /// All DXCC entities indexed by prefix (longest-prefix-first for matching).
    prefixes: Vec<(String, u32)>,
    /// Entity details by ADIF number.
    entities: HashMap<u32, DxccEntity>,
    /// Entity IDs the user still needs. None = no data loaded (conservative: everything needed).
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
            prefixes: Vec::new(),
            entities: HashMap::new(),
            needed_entity_ids: None,
            rarity_scores: HashMap::new(),
            spot_groups: Vec::new(),
        }
    }

    /// Load DXCC entity table. Sorts prefixes longest-first for matching.
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>) {
        self.entities.clear();
        self.prefixes.clear();
        for entity in &entities {
            self.entities.insert(entity.adif_number, entity.clone());
            self.prefixes
                .push((entity.prefix.to_uppercase(), entity.adif_number));
        }
        // Sort longest prefix first so "3Y/B" matches before "3Y"
        self.prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
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

    /// Resolve a callsign to its DXCC entity ADIF number using longest-prefix matching.
    pub fn resolve_entity(&self, callsign: &str) -> Option<u32> {
        let upper = callsign.to_uppercase();
        for (prefix, id) in &self.prefixes {
            if upper.starts_with(prefix.as_str()) {
                return Some(*id);
            }
        }
        None
    }

    /// Get rarity score for a callsign. Returns 0.5 (default) if unknown.
    pub fn rarity(&self, callsign: &str) -> f64 {
        self.rarity_scores
            .get(&callsign.to_uppercase())
            .copied()
            .unwrap_or(0.5)
    }

    /// Check if a callsign's DXCC entity is still needed.
    /// Returns true if: no needed data loaded (conservative), or entity is in needed set.
    pub fn is_needed_dxcc(&self, callsign: &str) -> bool {
        match &self.needed_entity_ids {
            None => true, // No data loaded = conservative: everything needed
            Some(ids) => {
                match self.resolve_entity(callsign) {
                    Some(entity_id) => ids.contains(&entity_id),
                    None => false, // Can't resolve = can't be needed
                }
            }
        }
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
pub fn frequency_to_band(freq_hz: u64) -> Option<String> {
    match freq_hz / 1_000_000 {
        1..=2 => Some("160m".to_string()),
        3..=4 => Some("80m".to_string()),
        5..=6 => Some("60m".to_string()),
        7..=8 => Some("40m".to_string()),
        10..=11 => Some("30m".to_string()),
        14..=15 => Some("20m".to_string()),
        18..=19 => Some("17m".to_string()),
        21..=22 => Some("15m".to_string()),
        24..=25 => Some("12m".to_string()),
        28..=30 => Some("10m".to_string()),
        50..=54 => Some("6m".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

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
                adif_number: 327,
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
            dx_dxcc: 327,
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
        assert_eq!(cache.resolve_entity("3Y/B1234"), Some(327));
    }

    #[test]
    fn test_resolve_entity_unknown_returns_none() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        assert_eq!(cache.resolve_entity("ZZ9ZZZ"), None);
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
            entity_id: 327,
            name: "Bouvet Island".to_string(),
            prefix: "3Y/B".to_string(),
            atno: true,
        }]);
        assert!(cache.is_needed_dxcc("3Y/B1234")); // Bouvet is needed
        assert!(!cache.is_needed_dxcc("K1ABC")); // US is NOT needed
    }

    #[test]
    fn test_is_needed_dxcc_empty_means_all_needed() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        // No needed data loaded -> conservative: everything is needed
        assert!(cache.is_needed_dxcc("K1ABC"));
        assert!(cache.is_needed_dxcc("JA1XYZ"));
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
