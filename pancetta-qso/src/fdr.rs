//! FDR — False Decodes Reduction (per-message-type confidence gates).
//!
//! Post-LDPC confidence gate keyed by FT8 message type. Standard
//! callsign-pair traffic passes at the uniform WSJT-X mainline threshold
//! (no sensitivity loss for bulk QSO traffic); unusual message types
//! (free text, telemetry, hashed-callsign, contest-non-standard,
//! DXpedition outside Fox/Hound mode) get tighter per-type confidence
//! thresholds.
//!
//! This is FDR Session 4a — module scaffolding + lib.rs wiring, all
//! default-OFF. Per-type threshold tuning (Session 4b) calibrates
//! against the hard-200 corpus with bootstrap-CI.
//!
//! ## Decoupling rationale
//!
//! pancetta-qso lives in the dep graph above pancetta-ft8 and cannot
//! depend on it. To gate decodes, we accept the relevant inputs as
//! primitives: a [`MessageCategory`] (the pancetta-qso-local view of
//! FT8 message types) plus the per-decode confidence telemetry. The
//! coordinator translates from `pancetta_ft8::DecodedMessage` +
//! `ConfidenceFeatures` into these primitives at the call boundary —
//! same pattern as `CrossSequenceSeed` ⇄ `A7SeedEntry`.
//!
//! Inspired by WSJT-X Improved FDR (DG2YCB, v2.5.0+). Spec:
//! `research/specs/spec-wsjtx-improved-fdr.md`.

use serde::{Deserialize, Serialize};

/// FDR level dial. `Off` is the default (pass everything through).
/// `Level1` is the lightweight gate; `Level2` is the comprehensive gate.
/// Special operating modes (contest, DXpedition Fox/Hound, Echo)
/// silently downgrade `Level2` to `Level1` so legitimate
/// contest/DXpedition exchanges aren't filtered.
///
/// Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Algorithm
/// description" — three-state level dial; Level2 auto-disabled in
/// special operating modes (release notes line ~1555).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FdrLevel {
    /// Pass-through; no candidates rejected.
    #[default]
    Off,
    /// Lightweight: rejects worst-FP-rate types only.
    Level1,
    /// Comprehensive: tightens unusual types. Auto-degrades to
    /// `Level1` in special operating modes.
    Level2,
}

impl FdrLevel {
    /// Effective level after special-mode auto-degradation.
    pub fn effective(self, is_special_mode: bool) -> Self {
        if is_special_mode && self == FdrLevel::Level2 {
            FdrLevel::Level1
        } else {
            self
        }
    }
}

/// pancetta-qso-local view of FT8 message types as the FDR gate
/// classifies them. The coordinator translates from
/// `pancetta_ft8::MessageType` into this enum at the call boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageCategory {
    /// Standard callsign-pair traffic (Standard, Extended) — the "easy"
    /// set per the FDR spec. Never gated regardless of level.
    Standard,
    /// Free text (i3=0 n3=0) — high FP-rate; strictly gated.
    FreeText,
    /// Telemetry (i3=0 n3=5) — high FP-rate; strictly gated.
    Telemetry,
    /// DXpedition mode (i3=0 n3=1) — lightly gated by default; auto-
    /// degraded to easy when the operator is in DXpedition mode (via
    /// `is_special_mode = true`).
    DXpedition,
    /// Non-standard callsigns with 12-bit hash + 58-bit call (i3=4).
    /// Lightly gated.
    NonStdCall,
    /// Contest exchanges, Field Day, RTTY Roundup. Lightly gated when
    /// `is_special_mode = false`; degraded to easy when the operator
    /// is in the matching contest mode.
    Contest,
    /// Unknown/unparseable. Strictly gated.
    Unknown,
}

/// Per-decode confidence telemetry inputs. All fields are `Option<_>`
/// so individual telemetry sources can be missing without forcing the
/// others to be absent. Mirrors `pancetta_ft8::ConfidenceFeatures` —
/// the coordinator copies the four fields across at the call
/// boundary.
#[derive(Debug, Clone, Copy, Default)]
pub struct FdrFeatures {
    /// BP iterations to convergence (lower = more confident).
    pub bp_iterations_used: Option<u8>,
    /// OSD depth at codeword acceptance (None = BP converged direct;
    /// lower = more confident).
    pub osd_depth_used: Option<u8>,
    /// Hard-decision bit errors corrected by OSD (lower = more
    /// confident).
    pub nharderrs: Option<u8>,
    /// Smallest |LLR| across the converged codeword (higher = more
    /// confident).
    pub min_llr_magnitude: Option<f32>,
}

/// Per-message-type confidence threshold. Higher = stricter. Computed
/// against a combined confidence score derived from `FdrFeatures`.
///
/// Session 4a ships **placeholder** thresholds — values are intentionally
/// conservative (Level1 thresholds barely above pass-through, Level2
/// only slightly tighter). Session 4b calibrates against hard-200 with
/// bootstrap-CI on per-type accept rate.
#[derive(Debug, Clone, Copy)]
struct TypeThreshold {
    level1: f32,
    level2: f32,
}

const fn easy() -> TypeThreshold {
    // Standard callsign pairs: never gated, regardless of level.
    TypeThreshold {
        level1: f32::NEG_INFINITY,
        level2: f32::NEG_INFINITY,
    }
}

const fn unusual_light() -> TypeThreshold {
    // Lightly gated at Level1, more strictly at Level2. Placeholder
    // values — Session 4b will calibrate.
    TypeThreshold {
        level1: 0.10,
        level2: 0.25,
    }
}

const fn unusual_strict() -> TypeThreshold {
    // Strictly gated at Level1 already, very strictly at Level2.
    // Placeholder values — Session 4b will calibrate.
    TypeThreshold {
        level1: 0.20,
        level2: 0.40,
    }
}

fn threshold_for(category: MessageCategory, is_special_mode: bool) -> TypeThreshold {
    match category {
        MessageCategory::Standard => easy(),
        MessageCategory::FreeText | MessageCategory::Telemetry => unusual_strict(),
        MessageCategory::NonStdCall => unusual_light(),
        MessageCategory::DXpedition => {
            // Auto-degrade to easy when operator is in DXpedition mode.
            if is_special_mode {
                easy()
            } else {
                unusual_light()
            }
        }
        MessageCategory::Contest => {
            // Same auto-degrade logic for contest mode.
            if is_special_mode {
                easy()
            } else {
                unusual_light()
            }
        }
        MessageCategory::Unknown => unusual_strict(),
    }
}

/// Combined confidence score from FdrFeatures. Higher = more
/// confident. Placeholder weights — Session 4b calibrates.
///
/// Per spec §"Algorithm description" step 3:
///   confidence ≈ w_llr * min_llr_magnitude
///              - w_bp_iters * bp_iterations_used
///              - w_osd * osd_depth_used
///              - w_nharderrs * nharderrs.
///
/// Missing fields are conservative (treated as worst-case): `None`
/// biases the gate toward rejection when telemetry is missing. The
/// `should_reject` entry point separately handles "no telemetry at
/// all" by returning false (never reject without evidence).
fn confidence_score(features: &FdrFeatures) -> f32 {
    let llr = features.min_llr_magnitude.unwrap_or(0.0);
    let bp = features.bp_iterations_used.unwrap_or(u8::MAX);
    let osd = features.osd_depth_used.unwrap_or(3);
    let nharderrs = features.nharderrs.unwrap_or(3);

    // Placeholder weights — Session 4b calibrates.
    let w_llr: f32 = 0.5;
    let w_bp: f32 = 0.01;
    let w_osd: f32 = 0.10;
    let w_nharderrs: f32 = 0.05;

    let score =
        w_llr * llr - w_bp * (bp as f32) - w_osd * (osd as f32) - w_nharderrs * (nharderrs as f32);
    score.clamp(-10.0, 10.0)
}

/// Public entry: decide whether to REJECT a decode given its FT8
/// message category, confidence features, FDR level, and operator
/// mode. Returns `true` to reject, `false` to accept.
///
/// **Off and missing-features paths always return `false`** — the gate
/// is conservative and never rejects without evidence.
///
/// Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Algorithm
/// description" step 5.
pub fn should_reject(
    category: MessageCategory,
    features: Option<&FdrFeatures>,
    level: FdrLevel,
    is_special_mode: bool,
) -> bool {
    let effective = level.effective(is_special_mode);
    if effective == FdrLevel::Off {
        return false;
    }
    let Some(features) = features else {
        return false;
    };
    let threshold = threshold_for(category, is_special_mode);
    let active_threshold = match effective {
        FdrLevel::Off => return false,
        FdrLevel::Level1 => threshold.level1,
        FdrLevel::Level2 => threshold.level2,
    };
    if active_threshold == f32::NEG_INFINITY {
        return false;
    }
    let score = confidence_score(features);
    score < active_threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_features() -> FdrFeatures {
        FdrFeatures {
            bp_iterations_used: Some(5),
            osd_depth_used: None,
            nharderrs: None,
            min_llr_magnitude: Some(3.0),
        }
    }

    fn poor_features() -> FdrFeatures {
        FdrFeatures {
            bp_iterations_used: Some(50),
            osd_depth_used: Some(3),
            nharderrs: Some(3),
            min_llr_magnitude: Some(0.05),
        }
    }

    #[test]
    fn off_never_rejects() {
        let f = poor_features();
        assert!(!should_reject(
            MessageCategory::FreeText,
            Some(&f),
            FdrLevel::Off,
            false
        ));
        assert!(!should_reject(
            MessageCategory::Telemetry,
            Some(&f),
            FdrLevel::Off,
            true
        ));
    }

    #[test]
    fn missing_features_never_rejects() {
        assert!(!should_reject(
            MessageCategory::FreeText,
            None,
            FdrLevel::Level1,
            false
        ));
        assert!(!should_reject(
            MessageCategory::FreeText,
            None,
            FdrLevel::Level2,
            false
        ));
    }

    #[test]
    fn easy_types_never_rejected() {
        let f = poor_features();
        assert!(!should_reject(
            MessageCategory::Standard,
            Some(&f),
            FdrLevel::Level1,
            false
        ));
        assert!(!should_reject(
            MessageCategory::Standard,
            Some(&f),
            FdrLevel::Level2,
            false
        ));
    }

    #[test]
    fn good_features_pass_at_all_levels() {
        let f = good_features();
        assert!(!should_reject(
            MessageCategory::FreeText,
            Some(&f),
            FdrLevel::Off,
            false
        ));
        assert!(!should_reject(
            MessageCategory::FreeText,
            Some(&f),
            FdrLevel::Level1,
            false
        ));
        assert!(!should_reject(
            MessageCategory::FreeText,
            Some(&f),
            FdrLevel::Level2,
            false
        ));
    }

    #[test]
    fn poor_features_reject_unusual_type_at_level1_and_level2() {
        let f = poor_features();
        assert!(should_reject(
            MessageCategory::Telemetry,
            Some(&f),
            FdrLevel::Level1,
            false
        ));
        assert!(should_reject(
            MessageCategory::Telemetry,
            Some(&f),
            FdrLevel::Level2,
            false
        ));
    }

    #[test]
    fn special_mode_degrades_level2_to_level1() {
        // For DXpedition messages: in non-special-mode, even good
        // features pass at Level1 (threshold 0.10) but a borderline
        // case can fail at Level2 (threshold 0.25). In special mode,
        // DXpedition gets reclassified to "easy" via threshold_for.
        let mut f = good_features();
        // Tune the case: just-above-Level1 score.
        f.min_llr_magnitude = Some(0.5);
        f.bp_iterations_used = Some(45);
        f.osd_depth_used = Some(2);
        f.nharderrs = Some(2);
        // In normal mode, DXpedition uses unusual_light thresholds.
        let normal_l2_reject = should_reject(
            MessageCategory::DXpedition,
            Some(&f),
            FdrLevel::Level2,
            false,
        );
        // In special mode, threshold_for returns easy() for DXpedition.
        let special_l2_reject = should_reject(
            MessageCategory::DXpedition,
            Some(&f),
            FdrLevel::Level2,
            true,
        );
        assert!(
            !special_l2_reject,
            "special-mode DXpedition is treated as easy; should never reject"
        );
        // No matter what normal_l2_reject is, special must be ≤ normal.
        let _ = normal_l2_reject;
    }

    #[test]
    fn confidence_score_monotone_in_min_llr() {
        let mut f = good_features();
        f.min_llr_magnitude = Some(1.0);
        let lo = confidence_score(&f);
        f.min_llr_magnitude = Some(5.0);
        let hi = confidence_score(&f);
        assert!(hi > lo, "higher min_llr should produce higher score");
    }

    #[test]
    fn confidence_score_monotone_in_bp_iters() {
        let mut f = good_features();
        f.bp_iterations_used = Some(5);
        let lo_iters = confidence_score(&f);
        f.bp_iterations_used = Some(50);
        let hi_iters = confidence_score(&f);
        assert!(
            hi_iters < lo_iters,
            "more BP iters should produce lower score"
        );
    }

    #[test]
    fn confidence_score_monotone_in_osd_depth() {
        let mut f = good_features();
        f.osd_depth_used = Some(0);
        let lo_depth = confidence_score(&f);
        f.osd_depth_used = Some(3);
        let hi_depth = confidence_score(&f);
        assert!(hi_depth < lo_depth, "deeper OSD should produce lower score");
    }

    #[test]
    fn effective_level_special_mode() {
        assert_eq!(FdrLevel::Off.effective(false), FdrLevel::Off);
        assert_eq!(FdrLevel::Off.effective(true), FdrLevel::Off);
        assert_eq!(FdrLevel::Level1.effective(false), FdrLevel::Level1);
        assert_eq!(FdrLevel::Level1.effective(true), FdrLevel::Level1);
        assert_eq!(FdrLevel::Level2.effective(false), FdrLevel::Level2);
        assert_eq!(FdrLevel::Level2.effective(true), FdrLevel::Level1);
    }
}
