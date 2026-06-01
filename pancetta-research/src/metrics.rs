use crate::scorecard::{CompositeInfo, Scorecard, TierResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

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
///
/// Populates `score` (raw composite) only; saturation-aware adjustments are
/// applied by `saturation_aware_composite` at read-time so historical
/// scorecards stay byte-stable on disk.
pub fn populate_composite(card: &mut Scorecard, weights: BTreeMap<String, f64>) {
    let score = compute_composite(&weights, &card.tiers);
    card.composite = CompositeInfo {
        weights,
        score,
        main_baseline_score: None,
        delta_vs_main: None,
    };
}

// ---------------------------------------------------------------------------
// hb-133 — Saturation-aware composite (corpus-shift-robust)
// ---------------------------------------------------------------------------
//
// When the evaluation corpus is rotated (e.g. hard-200 mix refresh on
// 2026-05-30), the raw composite jumps by an amount that reflects corpus
// shift, NOT decoder improvement. To keep multi-week graduation tracking
// comparable across refresh events, we record a one-time additive offset
// per refresh in `research/scorecards/refresh_offsets.json` and subtract
// the cumulative sum from the raw composite when reporting "saturation-
// aware" numbers.
//
// Concretely, for a current score `s_raw`:
//
//     s_sat = s_raw - Σ_{offsets} offset_to_subtract
//
// The offset is computed as
//
//     offset = score(prev_main, new_corpus) - score(prev_main, old_corpus)
//
// — same decoder, two corpora — so the difference is corpus-shift by
// construction. See research/ideation/2026-06-01-metric.md (M5).

/// A single corpus-refresh event recorded in `refresh_offsets.json`.
/// Each entry is a one-time fixup: the additive correction applied to
/// every composite computed against the post-refresh corpus so that the
/// pre-refresh baseline remains comparable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefreshOffset {
    /// ISO date the corpus refresh landed (informational; used by tooling
    /// for display only).
    pub refresh_date: String,
    /// Main-branch SHA of the commit that introduced the post-refresh
    /// corpus. Informational; current `saturation_aware_composite` applies
    /// every offset unconditionally (the assumption being that any score
    /// being adjusted was measured against the latest corpus). Future
    /// work could gate offsets by the scorecard's `git.head_sha` history.
    pub applies_from_sha: String,
    /// The additive correction in composite units (typically positive when
    /// the new corpus is "easier"; negative if it gets harder). Subtracted
    /// from raw composite by `saturation_aware_composite`.
    pub offset_to_subtract: f64,
    /// Human-readable note explaining how the offset was measured.
    #[serde(default)]
    pub note: String,
}

/// On-disk envelope for `research/scorecards/refresh_offsets.json`.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RefreshOffsetRegistry {
    /// Optional schema version; defaults to 1 when absent (round-trip safe).
    #[serde(default = "default_registry_schema_version")]
    pub schema_version: u32,
    /// Ordered list of refresh events. Append-only; never edit historicals.
    #[serde(default)]
    pub offsets: Vec<RefreshOffset>,
}

fn default_registry_schema_version() -> u32 {
    1
}

impl RefreshOffsetRegistry {
    /// Current registry schema version on disk.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Load the registry from a JSON file. The file may contain an
    /// underscore-prefixed `_doc` key; serde will ignore unknown fields
    /// by default since we don't `deny_unknown_fields`.
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path.as_ref())?;
        let reg: RefreshOffsetRegistry = serde_json::from_str(&s)?;
        Ok(reg)
    }

    /// Best-effort load: returns an empty registry if the file is absent.
    /// Other errors (malformed JSON, permission, etc.) still propagate so
    /// the operator sees them. Use this for harness binaries that want
    /// to degrade gracefully when the file hasn't been created yet.
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        if !path.as_ref().exists() {
            return Ok(Self::default());
        }
        Self::load(path)
    }

    /// Sum of all `offset_to_subtract` values. This is the amount that
    /// `saturation_aware_composite` subtracts from a raw composite.
    pub fn total_offset(&self) -> f64 {
        self.offsets.iter().map(|o| o.offset_to_subtract).sum()
    }
}

/// Subtract the cumulative corpus-refresh offset from a raw composite to
/// produce the "saturation-aware" composite. This is the headline metric
/// you should compare across corpus rotations.
///
/// `raw_composite` is the output of `compute_composite` (or
/// `Scorecard::composite::score`); `registry` is loaded from
/// `research/scorecards/refresh_offsets.json`. With no offsets recorded,
/// this is the identity function.
pub fn saturation_aware_composite(raw_composite: f64, registry: &RefreshOffsetRegistry) -> f64 {
    raw_composite - registry.total_offset()
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

    // -----------------------------------------------------------------
    // hb-133 — saturation-aware composite tests
    // -----------------------------------------------------------------

    #[test]
    fn saturation_aware_identity_with_empty_registry() {
        let reg = RefreshOffsetRegistry::default();
        // No offsets recorded → saturation_aware == raw.
        assert!((saturation_aware_composite(0.5791144, &reg) - 0.5791144).abs() < 1e-12);
    }

    #[test]
    fn saturation_aware_subtracts_single_offset() {
        // Anchor to the 2026-05-30 hard-200 refresh: the offset between
        // the pre-refresh main.json (0.5694146) and post-refresh main.json
        // (0.5791144) is +0.0096998 with the same production decoder on
        // both sides. The saturation-aware score reconstructs the
        // pre-refresh number from the post-refresh raw.
        let reg = RefreshOffsetRegistry {
            schema_version: 1,
            offsets: vec![RefreshOffset {
                refresh_date: "2026-05-30".into(),
                applies_from_sha: "e6a1594e158e1db5d201b980d211cf18efa0fa37".into(),
                offset_to_subtract: 0.009699,
                note: "hard-200 refresh".into(),
            }],
        };
        let post_refresh_raw = 0.579114;
        let pre_refresh_baseline = 0.569415;
        let sat = saturation_aware_composite(post_refresh_raw, &reg);
        // Allow 1e-6 rounding tolerance — offsets stored to 6 decimals.
        assert!(
            (sat - pre_refresh_baseline).abs() < 1e-6,
            "expected {pre_refresh_baseline}, got {sat}"
        );
    }

    #[test]
    fn saturation_aware_sums_multiple_offsets() {
        let reg = RefreshOffsetRegistry {
            schema_version: 1,
            offsets: vec![
                RefreshOffset {
                    refresh_date: "2026-05-30".into(),
                    applies_from_sha: "sha-1".into(),
                    offset_to_subtract: 0.009699,
                    note: String::new(),
                },
                RefreshOffset {
                    refresh_date: "2026-07-15".into(),
                    applies_from_sha: "sha-2".into(),
                    offset_to_subtract: 0.005000,
                    note: String::new(),
                },
            ],
        };
        assert!((reg.total_offset() - 0.014699).abs() < 1e-9);
        let raw = 0.600000;
        let sat = saturation_aware_composite(raw, &reg);
        assert!((sat - (raw - 0.014699)).abs() < 1e-9);
    }

    #[test]
    fn registry_loads_json_with_doc_key() {
        // The on-disk registry file carries an underscore-prefixed `_doc`
        // explanatory key. serde must ignore it (we don't deny_unknown).
        let json = r#"{
            "_doc": "ignore me",
            "schema_version": 1,
            "offsets": [
                {
                    "refresh_date": "2026-05-30",
                    "applies_from_sha": "abc123",
                    "offset_to_subtract": 0.009699,
                    "note": "test"
                }
            ]
        }"#;
        let reg: RefreshOffsetRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(reg.schema_version, 1);
        assert_eq!(reg.offsets.len(), 1);
        assert!((reg.total_offset() - 0.009699).abs() < 1e-12);
    }

    #[test]
    fn registry_load_or_default_handles_missing_file() {
        let path = std::path::PathBuf::from("/tmp/pancetta-research-hb133-nonexistent.json");
        // Ensure the path really doesn't exist.
        let _ = std::fs::remove_file(&path);
        let reg = RefreshOffsetRegistry::load_or_default(&path).unwrap();
        assert_eq!(reg.offsets.len(), 0);
        assert_eq!(reg.total_offset(), 0.0);
    }

    #[test]
    fn registry_roundtrips_through_json() {
        let reg = RefreshOffsetRegistry {
            schema_version: 1,
            offsets: vec![RefreshOffset {
                refresh_date: "2026-05-30".into(),
                applies_from_sha: "deadbeef".into(),
                offset_to_subtract: 0.0123,
                note: "rt".into(),
            }],
        };
        let s = serde_json::to_string(&reg).unwrap();
        let back: RefreshOffsetRegistry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.offsets.len(), 1);
        assert_eq!(back.offsets[0].refresh_date, "2026-05-30");
        assert!((back.total_offset() - 0.0123).abs() < 1e-12);
    }
}
