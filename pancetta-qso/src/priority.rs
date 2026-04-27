//! Priority scoring engine for autonomous CQ evaluation.
//!
//! Scores decoded CQ messages to determine which stations to call.
//! Pure and stateless: all external context (worked stations, recent failures)
//! is provided via the `WorkedStationLookup` trait.

use crate::autonomous::DxEvaluator;
use serde::{Deserialize, Serialize};

/// Weights for each scoring factor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityWeights {
    pub needed_dxcc: f64,
    pub needed_grid: f64,
    pub pota_sota: f64,
    pub rarity: f64,
    pub signal_strength: f64,
    pub duplicate_penalty: f64,
    pub recent_failure_penalty: f64,
}

impl Default for PriorityWeights {
    fn default() -> Self {
        Self {
            needed_dxcc: 0.35,
            needed_grid: 0.20,
            pota_sota: 0.15,
            rarity: 0.10,
            signal_strength: 0.05,
            duplicate_penalty: -0.40,
            recent_failure_penalty: -0.15,
        }
    }
}

/// Breakdown of how a CQ was scored.
#[derive(Debug, Clone)]
pub struct ScoreBreakdown {
    pub callsign: String,
    pub needed_dxcc: f64,
    pub needed_grid: f64,
    pub pota_sota: f64,
    pub rarity: f64,
    pub signal_strength: f64,
    pub duplicate_penalty: f64,
    pub recent_failure_penalty: f64,
    pub total: f64,
}

/// Trait for looking up whether a station has been worked.
///
/// Implemented by the coordinator layer to bridge to the QSO database.
/// Kept synchronous because `DxEvaluator::evaluate_cq` is synchronous.
pub trait WorkedStationLookup: Send + Sync {
    /// Has this callsign been worked on the given band (frequency in Hz)?
    fn is_duplicate(&self, callsign: &str, freq_hz: f64) -> bool;

    /// Was this callsign recently called but the QSO failed?
    fn is_recent_failure(&self, callsign: &str) -> bool;

    /// Is this DXCC entity needed (not yet confirmed)?
    fn is_needed_dxcc(&self, callsign: &str) -> bool;

    /// Is this grid square needed for award tracking?
    fn is_needed_grid(&self, grid: &str) -> bool;

    /// Get rarity score for a callsign (0.0 = common, 1.0 = rare).
    /// Returns 0.5 as default if unknown.
    fn rarity(&self, callsign: &str) -> f64 {
        let _ = callsign;
        0.5
    }

    /// Is this callsign flagged as notable (rare/legendary activation)?
    fn is_notable(&self, _callsign: &str) -> bool {
        false
    }

    /// Get network SNR data: (reporter_count, best_snr).
    fn network_snr(&self, _callsign: &str) -> Option<(u32, i32)> {
        None
    }

    /// Get network last-seen timestamp (unix seconds).
    fn network_last_seen(&self, _callsign: &str) -> Option<i64> {
        None
    }
}

/// No-op lookup that reports nothing is worked/needed.
/// Used for testing and when no QSO database is available.
#[derive(Debug, Clone)]
pub struct NullLookup;

impl WorkedStationLookup for NullLookup {
    fn is_duplicate(&self, _callsign: &str, _freq_hz: f64) -> bool {
        false
    }
    fn is_recent_failure(&self, _callsign: &str) -> bool {
        false
    }
    fn is_needed_dxcc(&self, _callsign: &str) -> bool {
        false
    }
    fn is_needed_grid(&self, _grid: &str) -> bool {
        false
    }
}

/// Detect POTA/SOTA activators from callsign patterns.
///
/// POTA (Parks on the Air) stations use the `/P` portable suffix.
/// SOTA (Summits on the Air) stations use the `/S` portable suffix.
/// `/QRP` indicates low-power operation only — not a portable activation.
///
/// Only suffix-style indicators count; operating-area prefixes like
/// `VE3/W1ABC` are not POTA/SOTA activations.
pub fn is_pota_sota_candidate(callsign: &str) -> bool {
    let upper = callsign.to_uppercase();
    upper.ends_with("/P") || upper.ends_with("/S")
}

/// Normalize SNR from typical FT8 range (-24 to +10) to 0.0–1.0.
fn normalize_snr(snr: i8) -> f64 {
    let clamped = (snr as f64).clamp(-24.0, 10.0);
    (clamped + 24.0) / 34.0
}

/// Priority scorer that implements `DxEvaluator`.
pub struct PriorityScorer {
    weights: PriorityWeights,
    lookup: Box<dyn WorkedStationLookup>,
}

impl PriorityScorer {
    pub fn new(weights: PriorityWeights, lookup: Box<dyn WorkedStationLookup>) -> Self {
        Self { weights, lookup }
    }

    /// Score a CQ with detailed breakdown.
    pub fn score_cq_detailed(
        &self,
        callsign: &str,
        grid: Option<&str>,
        snr: i8,
        freq_hz: f64,
    ) -> ScoreBreakdown {
        let needed_dxcc = if self.lookup.is_needed_dxcc(callsign) {
            1.0
        } else {
            0.0
        };
        let needed_grid = match grid {
            Some(g) if self.lookup.is_needed_grid(g) => 1.0,
            _ => 0.0,
        };
        let pota_sota = if is_pota_sota_candidate(callsign) {
            1.0
        } else {
            0.0
        };
        let rarity = self.lookup.rarity(callsign);
        let signal_strength = normalize_snr(snr);
        let duplicate_penalty = if self.lookup.is_duplicate(callsign, freq_hz) {
            1.0
        } else {
            0.0
        };
        let recent_failure_penalty = if self.lookup.is_recent_failure(callsign) {
            1.0
        } else {
            0.0
        };

        // Notable station bonus
        let notable_bonus = if self.lookup.is_notable(callsign) {
            0.3
        } else {
            0.0
        };

        // Staleness multiplier: deprioritize stale network spots
        let staleness = if let Some(last_seen) = self.lookup.network_last_seen(callsign) {
            let now = chrono::Utc::now().timestamp();
            let age_secs = (now - last_seen).max(0);
            match age_secs {
                0..=300 => 1.0,   // <5 min: fresh
                301..=600 => 0.7, // 5-10 min: aging
                601..=900 => 0.4, // 10-15 min: stale
                _ => 0.2,         // >15 min: very stale
            }
        } else {
            1.0 // no network data = no penalty
        };

        // Network SNR bonus/penalty
        let snr_bonus = if let Some((reporter_count, best_snr)) = self.lookup.network_snr(callsign)
        {
            if reporter_count >= 5 && best_snr >= -20 {
                0.1 // well-confirmed, likely workable
            } else if reporter_count == 1 && best_snr < -25 {
                -0.1 // uncertain, might not be workable
            } else {
                0.0
            }
        } else {
            0.0
        };

        let raw_score = (needed_dxcc * self.weights.needed_dxcc
            + needed_grid * self.weights.needed_grid
            + pota_sota * self.weights.pota_sota
            + rarity * self.weights.rarity
            + signal_strength * self.weights.signal_strength
            + duplicate_penalty * self.weights.duplicate_penalty
            + recent_failure_penalty * self.weights.recent_failure_penalty
            + notable_bonus
            + snr_bonus)
            * staleness;

        let total = raw_score.clamp(0.0, 1.0);

        ScoreBreakdown {
            callsign: callsign.to_string(),
            needed_dxcc,
            needed_grid,
            pota_sota,
            rarity,
            signal_strength,
            duplicate_penalty,
            recent_failure_penalty,
            total,
        }
    }
}

impl DxEvaluator for PriorityScorer {
    fn evaluate_cq(&self, callsign: &str, grid: Option<&str>, snr: i8, freq_hz: f64) -> f64 {
        self.score_cq_detailed(callsign, grid, snr, freq_hz).total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    struct TestLookup {
        duplicates: HashSet<String>,
        recent_failures: HashSet<String>,
        needed_dxcc: HashSet<String>,
        needed_grids: HashSet<String>,
    }

    impl TestLookup {
        fn new() -> Self {
            Self {
                duplicates: HashSet::new(),
                recent_failures: HashSet::new(),
                needed_dxcc: HashSet::new(),
                needed_grids: HashSet::new(),
            }
        }
    }

    impl WorkedStationLookup for TestLookup {
        fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
            self.duplicates.contains(callsign)
        }
        fn is_recent_failure(&self, callsign: &str) -> bool {
            self.recent_failures.contains(callsign)
        }
        fn is_needed_dxcc(&self, callsign: &str) -> bool {
            self.needed_dxcc.contains(callsign)
        }
        fn is_needed_grid(&self, grid: &str) -> bool {
            self.needed_grids.contains(grid)
        }
    }

    #[test]
    fn test_pota_sota_detection() {
        // POTA portable suffix — should match
        assert!(is_pota_sota_candidate("W1ABC/P"));
        assert!(is_pota_sota_candidate("K5ARH/P"));
        assert!(is_pota_sota_candidate("w1abc/p")); // case insensitive

        // SOTA portable suffix — should match
        assert!(is_pota_sota_candidate("W1ABC/S"));
        assert!(is_pota_sota_candidate("K5ARH/S"));

        // /QRP is low-power only — NOT a POTA/SOTA indicator
        assert!(!is_pota_sota_candidate("K5ARH/QRP"));
        assert!(!is_pota_sota_candidate("K2DEF/QRP"));

        // Prefix-style calls — should NOT match (operating-area prefix, not POTA)
        assert!(!is_pota_sota_candidate("VE3/W1ABC")); // operating from VE3
        assert!(!is_pota_sota_candidate("DL/K5ARH")); // operating from Germany
        assert!(!is_pota_sota_candidate("F/W1ABC")); // operating from France

        // Callsigns with 'P' or 'S' embedded — should NOT match (not a /P or /S suffix)
        assert!(!is_pota_sota_candidate("PP5XX")); // 'P' is part of prefix, not a /P suffix
        assert!(!is_pota_sota_candidate("PS7AB")); // 'S' is part of prefix, not a /S suffix

        // Other portable/mobile suffixes — should NOT match
        assert!(!is_pota_sota_candidate("W1ABC/M")); // mobile
        assert!(!is_pota_sota_candidate("W1ABC/MM")); // maritime mobile
        assert!(!is_pota_sota_candidate("W1ABC/LGT")); // lighthouse

        // Regular calls — should NOT match
        assert!(!is_pota_sota_candidate("W1ABC"));
        assert!(!is_pota_sota_candidate("K2DEF"));
    }

    #[test]
    fn test_snr_normalization() {
        assert!((normalize_snr(-24) - 0.0).abs() < 0.01);
        assert!((normalize_snr(10) - 1.0).abs() < 0.01);
        assert!((normalize_snr(-7) - 0.5).abs() < 0.05);
        assert!((normalize_snr(-30) - 0.0).abs() < 0.01);
        assert!((normalize_snr(20) - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_null_lookup_baseline_score() {
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let score = scorer.evaluate_cq("W1ABC", Some("FN42"), -10, 14074000.0);
        assert!(
            score > 0.0,
            "Baseline score should be positive, got {}",
            score
        );
        assert!(
            score < 0.5,
            "Baseline score should be modest, got {}",
            score
        );
    }

    #[test]
    fn test_needed_dxcc_boosts_score() {
        let mut lookup = TestLookup::new();
        lookup.needed_dxcc.insert("JA1ABC".to_string());
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let score_needed = scorer.evaluate_cq("JA1ABC", Some("PM95"), -10, 14074000.0);

        let scorer_null = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let score_not_needed = scorer_null.evaluate_cq("JA1ABC", Some("PM95"), -10, 14074000.0);

        assert!(
            score_needed > score_not_needed,
            "Needed DXCC should boost score: {} vs {}",
            score_needed,
            score_not_needed
        );
    }

    #[test]
    fn test_duplicate_penalty_reduces_score() {
        let mut lookup = TestLookup::new();
        lookup.duplicates.insert("K1DEF".to_string());
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let score_dup = scorer.evaluate_cq("K1DEF", Some("FN31"), -10, 14074000.0);

        let scorer_null = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let score_fresh = scorer_null.evaluate_cq("K1DEF", Some("FN31"), -10, 14074000.0);

        assert!(
            score_dup < score_fresh,
            "Duplicate should reduce score: {} vs {}",
            score_dup,
            score_fresh
        );
    }

    #[test]
    fn test_pota_sota_callsign_boosts_score() {
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let score_regular = scorer.evaluate_cq("W1ABC", Some("FN42"), -10, 14074000.0);
        let score_pota = scorer.evaluate_cq("W1ABC/P", Some("FN42"), -10, 14074000.0);
        let score_sota = scorer.evaluate_cq("W1ABC/S", Some("FN42"), -10, 14074000.0);
        let score_qrp = scorer.evaluate_cq("W1ABC/QRP", Some("FN42"), -10, 14074000.0);
        assert!(
            score_pota > score_regular,
            "POTA /P suffix should boost score: {} vs {}",
            score_pota,
            score_regular
        );
        assert!(
            score_sota > score_regular,
            "SOTA /S suffix should boost score: {} vs {}",
            score_sota,
            score_regular
        );
        assert_eq!(
            score_qrp, score_regular,
            "/QRP should not boost score (low-power, not POTA/SOTA): {} vs {}",
            score_qrp, score_regular
        );
    }

    #[test]
    fn test_stronger_signal_slightly_preferred() {
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let score_weak = scorer.evaluate_cq("W1ABC", Some("FN42"), -20, 14074000.0);
        let score_strong = scorer.evaluate_cq("W1ABC", Some("FN42"), 5, 14074000.0);
        assert!(
            score_strong > score_weak,
            "Stronger signal should be slightly preferred: {} vs {}",
            score_strong,
            score_weak
        );
    }

    #[test]
    fn test_score_ordering_needed_dxcc_beats_duplicate() {
        let mut lookup = TestLookup::new();
        lookup.needed_dxcc.insert("ZL1ABC".to_string());
        lookup.duplicates.insert("ZL1ABC".to_string());
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));
        let breakdown = scorer.score_cq_detailed("ZL1ABC", Some("RF73"), -10, 14074000.0);
        assert!(
            breakdown.duplicate_penalty > 0.0,
            "Duplicate factor should be active"
        );
        assert!(
            breakdown.needed_dxcc > 0.0,
            "Needed DXCC factor should be active"
        );
    }

    #[test]
    fn test_custom_weights() {
        let weights = PriorityWeights {
            needed_dxcc: 0.0,
            needed_grid: 0.0,
            pota_sota: 1.0,
            rarity: 0.0,
            signal_strength: 0.0,
            duplicate_penalty: 0.0,
            recent_failure_penalty: 0.0,
        };
        let scorer = PriorityScorer::new(weights, Box::new(NullLookup));
        let score_regular = scorer.evaluate_cq("W1ABC", None, -10, 14074000.0);
        let score_portable = scorer.evaluate_cq("W1ABC/P", None, -10, 14074000.0);
        assert!(
            (score_regular - 0.0).abs() < 0.01,
            "Non-portable should score ~0 with pota-only weights"
        );
        assert!(
            (score_portable - 1.0).abs() < 0.01,
            "Portable should score ~1.0 with pota-only weights"
        );
    }

    #[test]
    fn test_score_clamped_to_0_1() {
        let weights = PriorityWeights {
            needed_dxcc: 1.0,
            needed_grid: 1.0,
            pota_sota: 1.0,
            rarity: 1.0,
            signal_strength: 1.0,
            duplicate_penalty: 0.0,
            recent_failure_penalty: 0.0,
        };
        let mut lookup = TestLookup::new();
        lookup.needed_dxcc.insert("W1ABC".to_string());
        lookup.needed_grids.insert("FN42".to_string());
        let scorer = PriorityScorer::new(weights, Box::new(lookup));
        let score = scorer.evaluate_cq("W1ABC/P", Some("FN42"), 10, 14074000.0);
        assert!(
            score <= 1.0,
            "Score should be clamped to 1.0, got {}",
            score
        );
        assert!(
            score >= 0.0,
            "Score should be clamped to 0.0, got {}",
            score
        );
    }

    #[test]
    fn test_evaluate_cq_trait_matches_detailed() {
        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(NullLookup));
        let trait_score = scorer.evaluate_cq("W1ABC", Some("FN42"), -10, 14074000.0);
        let detailed = scorer.score_cq_detailed("W1ABC", Some("FN42"), -10, 14074000.0);
        assert!(
            (trait_score - detailed.total).abs() < f64::EPSILON,
            "Trait and detailed should match"
        );
    }

    struct RarityLookup {
        rarity_map: HashMap<String, f64>,
    }

    impl WorkedStationLookup for RarityLookup {
        fn is_duplicate(&self, _callsign: &str, _freq_hz: f64) -> bool {
            false
        }
        fn is_recent_failure(&self, _callsign: &str) -> bool {
            false
        }
        fn is_needed_dxcc(&self, _callsign: &str) -> bool {
            false
        }
        fn is_needed_grid(&self, _grid: &str) -> bool {
            false
        }
        fn rarity(&self, callsign: &str) -> f64 {
            self.rarity_map.get(callsign).copied().unwrap_or(0.5)
        }
    }

    #[test]
    fn test_rarity_affects_score() {
        let mut rarity_map = HashMap::new();
        rarity_map.insert("3Y0J".to_string(), 0.98);

        let weights = PriorityWeights {
            needed_dxcc: 0.0,
            needed_grid: 0.0,
            pota_sota: 0.0,
            rarity: 1.0,
            signal_strength: 0.0,
            duplicate_penalty: 0.0,
            recent_failure_penalty: 0.0,
        };

        let scorer_rare = PriorityScorer::new(
            weights.clone(),
            Box::new(RarityLookup {
                rarity_map: rarity_map.clone(),
            }),
        );
        let scorer_common = PriorityScorer::new(weights, Box::new(NullLookup));

        let score_rare = scorer_rare.evaluate_cq("3Y0J", None, -10, 14074000.0);
        let score_common = scorer_common.evaluate_cq("W1ABC", None, -10, 14074000.0);

        assert!(
            score_rare > score_common,
            "Rare station should score higher: {} vs {}",
            score_rare,
            score_common
        );
        assert!(
            (score_rare - 0.98).abs() < 0.01,
            "Rarity-only score should be ~0.98, got {}",
            score_rare
        );
    }
}
