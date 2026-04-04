//! In-memory session cache for cqdx.io data.
//!
//! Holds DXCC entities, needed status, rarity scores, and priority spots.
//! Populated on startup from cqdx.io API, refreshed by polling.

use crate::types::{DxccEntity, NeededEntity, PrioritySpot};
use std::collections::{HashMap, HashSet};

/// In-memory cache of cqdx.io data for the current session.
#[derive(Debug, Clone)]
pub struct CqdxCache {
    /// All DXCC entities indexed by prefix (longest-prefix-first for matching).
    prefixes: Vec<(String, u32)>,
    /// Entity details by ID.
    entities: HashMap<u32, DxccEntity>,
    /// Entity IDs the user still needs. None = no data loaded (conservative: everything needed).
    needed_entity_ids: Option<HashSet<u32>>,
    /// Rarity scores from priority spots, keyed by uppercase callsign.
    rarity_scores: HashMap<String, f64>,
    /// Latest priority spot poll results.
    priorities: Vec<PrioritySpot>,
}

impl CqdxCache {
    pub fn new() -> Self {
        Self {
            prefixes: Vec::new(),
            entities: HashMap::new(),
            needed_entity_ids: None,
            rarity_scores: HashMap::new(),
            priorities: Vec::new(),
        }
    }

    /// Load DXCC entity table. Sorts prefixes longest-first for matching.
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>) {
        self.entities.clear();
        self.prefixes.clear();
        for entity in &entities {
            self.entities.insert(entity.id, entity.clone());
            self.prefixes.push((entity.prefix.to_uppercase(), entity.id));
        }
        // Sort longest prefix first so "3Y/B" matches before "3Y"
        self.prefixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// Load needed entity IDs. Calling this with an empty vec means nothing is needed.
    pub fn load_needed(&mut self, needed: Vec<NeededEntity>) {
        let ids: HashSet<u32> = needed.iter().map(|n| n.entity_id).collect();
        self.needed_entity_ids = Some(ids);
    }

    /// Update priority spots from latest poll. Also updates rarity cache.
    /// Uses upsert (not clear) so callsigns that drop out of the top-N
    /// retain their last-known rarity until replaced by a new poll result.
    pub fn update_priorities(&mut self, spots: Vec<PrioritySpot>) {
        for spot in &spots {
            self.rarity_scores.insert(spot.callsign.to_uppercase(), spot.rarity);
        }
        self.priorities = spots;
    }

    /// Resolve a callsign to its DXCC entity ID using longest-prefix matching.
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

    /// Get current priority spots for frequency nudge decisions.
    pub fn priority_spots(&self) -> &[PrioritySpot] {
        &self.priorities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn sample_entities() -> Vec<DxccEntity> {
        vec![
            DxccEntity {
                id: 291, name: "United States".to_string(), prefix: "K".to_string(),
                continent: "NA".to_string(), cq_zone: 5, itu_zone: 8,
            },
            DxccEntity {
                id: 339, name: "Japan".to_string(), prefix: "JA".to_string(),
                continent: "AS".to_string(), cq_zone: 25, itu_zone: 45,
            },
            DxccEntity {
                id: 327, name: "Bouvet Island".to_string(), prefix: "3Y/B".to_string(),
                continent: "AF".to_string(), cq_zone: 38, itu_zone: 67,
            },
        ]
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
    fn test_rarity_from_priorities() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.update_priorities(vec![PrioritySpot {
            callsign: "3Y0J".to_string(),
            grid: Some("JD15".to_string()),
            frequency: 14074000,
            mode: "FT8".to_string(),
            snr: Some(-12),
            entity: Some("Bouvet Island".to_string()),
            rarity: 0.98,
            needed: true,
            last_spotted: chrono::Utc::now(),
            spot_count: 5,
        }]);
        assert!((cache.rarity("3Y0J") - 0.98).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rarity_unknown_callsign_returns_default() {
        let cache = CqdxCache::new();
        assert!((cache.rarity("W1ABC") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_is_needed_dxcc_with_data() {
        let mut cache = CqdxCache::new();
        cache.load_entities(sample_entities());
        cache.load_needed(vec![NeededEntity {
            entity_id: 327,
            name: "Bouvet Island".to_string(),
            prefix: "3Y/B".to_string(),
        }]);
        assert!(cache.is_needed_dxcc("3Y/B1234")); // Bouvet is needed
        assert!(!cache.is_needed_dxcc("K1ABC"));    // US is NOT needed
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
    fn test_priority_spots_accessor() {
        let mut cache = CqdxCache::new();
        assert!(cache.priority_spots().is_empty());
        cache.update_priorities(vec![PrioritySpot {
            callsign: "3Y0J".to_string(),
            grid: None,
            frequency: 14074000,
            mode: "FT8".to_string(),
            snr: None,
            entity: None,
            rarity: 0.9,
            needed: true,
            last_spotted: chrono::Utc::now(),
            spot_count: 1,
        }]);
        assert_eq!(cache.priority_spots().len(), 1);
    }
}
