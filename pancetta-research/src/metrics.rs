use crate::scorecard::{CompositeInfo, Scorecard, TierResult};
use std::collections::BTreeMap;

/// Default composite-metric weights (spec section "Composite Metric").
pub fn default_weights() -> BTreeMap<String, f64> {
    let mut w = BTreeMap::new();
    w.insert("real_decode_rate_hard_200".to_string(), 0.50);
    w.insert("snr_50pct_synth_clean".to_string(), 0.30);
    w.insert("fixtures_pass_rate".to_string(), 0.15);
    w.insert("snr_50pct_synth_doppler".to_string(), 0.05);
    w
}

/// Map an SNR-at-50%-recovery value (in dB; more negative is better) to a
/// [0, 1] score. clamp((-snr - 10) / 20, 0, 1) — so -30 dB → 1.0, -10 dB → 0.0.
pub fn normalize_snr_db(snr_db: f64) -> f64 {
    let raw = (-snr_db - 10.0) / 20.0;
    raw.clamp(0.0, 1.0)
}

/// Compute the composite score for a scorecard. Missing tiers contribute 0
/// for their term (i.e. the metric degrades gracefully when not all tiers
/// were run; the engineer sees the result but should treat it as partial).
pub fn compute_composite(
    weights: &BTreeMap<String, f64>,
    tiers: &BTreeMap<String, TierResult>,
) -> f64 {
    let real_rate = tiers
        .get("curated-hard-200")
        .and_then(|t| t.decode_rate)
        .unwrap_or(0.0);
    let snr_clean = tiers
        .get("synth-clean")
        .and_then(|t| t.snr_at_50pct_recovery_db)
        .map(normalize_snr_db)
        .unwrap_or(0.0);
    let fixtures = tiers
        .get("fixtures")
        .and_then(|t| t.pass_rate)
        .unwrap_or(0.0);
    let snr_doppler = tiers
        .get("synth-doppler")
        .and_then(|t| t.snr_at_50pct_recovery_db)
        .map(normalize_snr_db)
        .unwrap_or(0.0);

    weights
        .get("real_decode_rate_hard_200")
        .copied()
        .unwrap_or(0.0)
        * real_rate
        + weights.get("snr_50pct_synth_clean").copied().unwrap_or(0.0) * snr_clean
        + weights.get("fixtures_pass_rate").copied().unwrap_or(0.0) * fixtures
        + weights
            .get("snr_50pct_synth_doppler")
            .copied()
            .unwrap_or(0.0)
            * snr_doppler
}

/// Fill in the CompositeInfo on a scorecard from its tiers + the given weights.
pub fn populate_composite(card: &mut Scorecard, weights: BTreeMap<String, f64>) {
    let score = compute_composite(&weights, &card.tiers);
    card.composite = CompositeInfo {
        weights,
        score,
        main_baseline_score: None,
        delta_vs_main: None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scorecard::TierResult;

    #[test]
    fn normalize_snr_boundary_conditions() {
        assert_eq!(normalize_snr_db(-30.0), 1.0);
        assert_eq!(normalize_snr_db(-10.0), 0.0);
        assert!((normalize_snr_db(-20.0) - 0.5).abs() < 1e-9);
        // Out of range clamps:
        assert_eq!(normalize_snr_db(-40.0), 1.0);
        assert_eq!(normalize_snr_db(0.0), 0.0);
    }

    #[test]
    fn composite_fixtures_only() {
        let weights = default_weights();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        let score = compute_composite(&weights, &tiers);
        // Only the fixtures weight (0.15) contributes.
        assert!((score - 0.15).abs() < 1e-9);
    }

    #[test]
    fn composite_all_tiers() {
        let weights = default_weights();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        tiers.insert(
            "curated-hard-200".to_string(),
            TierResult {
                decode_rate: Some(0.5),
                ..Default::default()
            },
        );
        tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-20.0), // → 0.5
                ..Default::default()
            },
        );
        tiers.insert(
            "synth-doppler".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-15.0), // → 0.25
                ..Default::default()
            },
        );
        let score = compute_composite(&weights, &tiers);
        // 0.50*0.5 + 0.30*0.5 + 0.15*1.0 + 0.05*0.25 = 0.25 + 0.15 + 0.15 + 0.0125 = 0.5625
        assert!((score - 0.5625).abs() < 1e-9);
    }
}
