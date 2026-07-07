use crate::scorecard::{CompositeInfo, RegressionFlags, Scorecard, TierResult};
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
// **Naming note (Phase C 2026-06-02):** the name "saturation-aware" is
// pancetta-internal shorthand. It does NOT mean a statistical-saturation
// correction (i.e. nonlinear ceiling behavior); it is a corpus-refresh
// offset accumulator that corrects for known corpus-shift jumps so the
// cumulative graduation log stays comparable across refresh events.
// A clearer (longer) name would be **composite-with-corpus-offset** or
// **corpus-shift-corrected composite**. The "saturation" framing comes
// from the operational story: corpora rotate when the decoder
// *saturates* the previous corpus. The math is just an additive offset.
// See `docs/engineering/2026-06-02-engineering-substance-audit.md`
// (claim 31).
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

// ---------------------------------------------------------------------------
// Task W0.3 (2026-07-06) — real `RegressionFlags` computation.
// ---------------------------------------------------------------------------
//
// Previously written as `RegressionFlags::default()` at every scorecard-build
// call site (`bin/eval.rs`) — always `false`/`false`/`0.0`, computed nowhere
// (design spec §2). This is a pure function so it's independently testable
// and so both `eval` (self-diffing the run it's about to write against the
// checked-in `research/scorecards/main.json` baseline) and any other tool
// can call it the same way.

/// Compute `RegressionFlags` for `current` relative to `baseline` (typically
/// the checked-in `research/scorecards/main.json`). Pure — no I/O, no
/// mutation. Tiers present in only one side are treated as "nothing to
/// compare" for that tier's contribution (never fabricates a regression from
/// a tier that simply wasn't run on one side).
///
/// - `fixture_regression`: `true` iff both sides ran the `fixtures` tier and
///   `current`'s `pass_rate` is strictly lower than `baseline`'s.
/// - `false_positive_introduced`: `true` iff both sides ran the `noise_1000`
///   tier (Task W0.1) and `current`'s `false_positives_total` is strictly
///   higher than `baseline`'s. Any nonzero increase counts — there is no
///   threshold, matching the noise-tier's zero-tolerance hard gate in
///   `bin/compare.rs`.
/// - `snr_curve_regression_db`: `current - baseline` on the `synth-clean`
///   tier's `snr_at_50pct_recovery_db` (Task W0.2's 2500 Hz-calibrated
///   curve). Positive means `current` needs MORE SNR to hit 50% recovery
///   (worse sensitivity); negative means it improved. `0.0` when either
///   side is missing the tier or the field.
pub fn compute_regression_flags(baseline: &Scorecard, current: &Scorecard) -> RegressionFlags {
    let fixture_regression = match (
        baseline.tiers.get("fixtures"),
        current.tiers.get("fixtures"),
    ) {
        (Some(b), Some(c)) => match (b.pass_rate, c.pass_rate) {
            (Some(bv), Some(cv)) => cv < bv,
            _ => false,
        },
        _ => false,
    };

    let false_positive_introduced = match (
        baseline.tiers.get("noise_1000"),
        current.tiers.get("noise_1000"),
    ) {
        (Some(b), Some(c)) => {
            let bv = b.false_positives_total.unwrap_or(0);
            let cv = c.false_positives_total.unwrap_or(0);
            cv > bv
        }
        _ => false,
    };

    let snr_curve_regression_db = match (
        baseline.tiers.get("synth-clean"),
        current.tiers.get("synth-clean"),
    ) {
        (Some(b), Some(c)) => match (b.snr_at_50pct_recovery_db, c.snr_at_50pct_recovery_db) {
            (Some(bv), Some(cv)) => cv - bv,
            _ => 0.0,
        },
        _ => 0.0,
    };

    RegressionFlags {
        fixture_regression,
        false_positive_introduced,
        snr_curve_regression_db,
    }
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

    // -----------------------------------------------------------------
    // Task W0.3 — real `RegressionFlags` computation (was
    // `RegressionFlags::default()`, computed nowhere; design spec §2).
    // -----------------------------------------------------------------

    /// Minimal scorecard builder for the regression-flags tests — only
    /// the `tiers` map matters for `compute_regression_flags`; every
    /// other field is a fixed placeholder.
    fn make_card(tiers: BTreeMap<String, TierResult>) -> Scorecard {
        Scorecard {
            schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
            generated_at: chrono::Utc::now(),
            mode: crate::Mode::Ft8,
            git: crate::scorecard::GitInfo {
                branch: "test".into(),
                head_sha: "0000000".into(),
                main_merge_base: "0000000".into(),
                dirty: false,
            },
            build: crate::scorecard::BuildInfo {
                rustc_version: "1.85.0".into(),
                release: true,
                features: vec![],
            },
            harness: crate::scorecard::HarnessInfo {
                harness_version: "test".into(),
                host: "test/test".into(),
                cores_used: 1,
                elapsed_seconds: 0.0,
            },
            config: crate::scorecard::ConfigInfo {
                decoder: serde_json::json!({}),
                seed: 0,
                tiers_run: vec![],
                fp_filter_active: false,
            },
            tiers,
            composite: CompositeInfo {
                weights: BTreeMap::new(),
                score: 0.0,
                main_baseline_score: None,
                delta_vs_main: None,
            },
            regressions: RegressionFlags::default(),
            notes: String::new(),
        }
    }

    #[test]
    fn regression_flags_default_when_no_shared_tiers() {
        let baseline = make_card(BTreeMap::new());
        let current = make_card(BTreeMap::new());
        let flags = compute_regression_flags(&baseline, &current);
        assert!(!flags.fixture_regression);
        assert!(!flags.false_positive_introduced);
        assert_eq!(flags.snr_curve_regression_db, 0.0);
    }

    #[test]
    fn regression_flags_detects_fixture_pass_rate_drop() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(0.9),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        assert!(
            flags.fixture_regression,
            "pass_rate dropped 1.0 -> 0.9, must flag fixture_regression"
        );
    }

    #[test]
    fn regression_flags_no_fixture_regression_when_pass_rate_holds_or_improves() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(0.9),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "fixtures".to_string(),
            TierResult {
                pass_rate: Some(1.0),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        assert!(!flags.fixture_regression);
    }

    #[test]
    fn regression_flags_detects_false_positive_introduced() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "noise_1000".to_string(),
            TierResult {
                false_positives_total: Some(0),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "noise_1000".to_string(),
            TierResult {
                false_positives_total: Some(3),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        assert!(
            flags.false_positive_introduced,
            "false_positives_total rose 0 -> 3, must flag false_positive_introduced"
        );
    }

    #[test]
    fn regression_flags_no_false_positive_when_noise_tier_unchanged() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "noise_1000".to_string(),
            TierResult {
                false_positives_total: Some(0),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "noise_1000".to_string(),
            TierResult {
                false_positives_total: Some(0),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        assert!(!flags.false_positive_introduced);
    }

    #[test]
    fn regression_flags_computes_snr_curve_regression_db() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-20.0),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                // Worse sensitivity: needs 1.5 dB MORE SNR to hit 50%.
                snr_at_50pct_recovery_db: Some(-18.5),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        // current - baseline = -18.5 - (-20.0) = +1.5 (positive = worse).
        assert!(
            (flags.snr_curve_regression_db - 1.5).abs() < 1e-9,
            "expected +1.5 dB regression, got {}",
            flags.snr_curve_regression_db
        );
    }

    #[test]
    fn regression_flags_snr_curve_negative_when_sensitivity_improves() {
        let mut baseline_tiers = BTreeMap::new();
        baseline_tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-20.0),
                ..Default::default()
            },
        );
        let mut current_tiers = BTreeMap::new();
        current_tiers.insert(
            "synth-clean".to_string(),
            TierResult {
                snr_at_50pct_recovery_db: Some(-21.0),
                ..Default::default()
            },
        );
        let baseline = make_card(baseline_tiers);
        let current = make_card(current_tiers);
        let flags = compute_regression_flags(&baseline, &current);
        assert!(
            (flags.snr_curve_regression_db - (-1.0)).abs() < 1e-9,
            "expected -1.0 dB (improvement), got {}",
            flags.snr_curve_regression_db
        );
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

/// hb-248 (Batch 87): normalize hashed-callsign display tokens so that
/// pancetta and ft8_lib renderings of the SAME decode compare equal.
///
/// ft8_lib renders an unresolved hashed callsign as `<...>`; pancetta
/// renders the 12-bit hash value as `<...NNNN>` (message.rs
/// `CallsignField::Hash`). Exact-text truth intersection therefore
/// counts every pancetta decode of a hashed message as BOTH a phantom
/// miss and a phantom FP (Batch 86 audit: 43.4% of nominal misses on
/// the 5/30 corpus were such aliases).
///
/// Rule (conservative): any whitespace-delimited token matching
/// `<...` + digits + `>` (including bare `<...>`) is canonicalized to
/// `<...>`. RESOLVED hash tokens (`<K1ABC>`) are left untouched — a
/// pancetta-resolved hash vs ft8_lib's `<...>` still mismatches; the
/// consuming probe should count that residual separately.
pub fn hash_normalize_message(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            let inner = tok.strip_prefix("<...").and_then(|r| r.strip_suffix('>'));
            match inner {
                Some(digits) if digits.chars().all(|c| c.is_ascii_digit()) => "<...>",
                _ => tok,
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod hash_normalize_tests {
    use super::hash_normalize_message;

    #[test]
    fn pancetta_hash_value_token_normalizes() {
        assert_eq!(
            hash_normalize_message("<...3631> WZ8DX EM79"),
            "<...> WZ8DX EM79"
        );
    }

    #[test]
    fn bare_ellipsis_token_unchanged() {
        assert_eq!(
            hash_normalize_message("<...> WZ8DX EM79"),
            "<...> WZ8DX EM79"
        );
    }

    #[test]
    fn resolved_hash_token_left_alone() {
        assert_eq!(
            hash_normalize_message("<K1ABC> W9XYZ 73"),
            "<K1ABC> W9XYZ 73"
        );
    }

    #[test]
    fn non_digit_suffix_left_alone() {
        assert_eq!(hash_normalize_message("<...X1> CQ"), "<...X1> CQ");
    }

    #[test]
    fn plain_messages_untouched() {
        assert_eq!(hash_normalize_message("CQ K5ARH EM10"), "CQ K5ARH EM10");
    }
}
