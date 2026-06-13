//! hb-103 v1 — Message-content trust score (Batch 31).
//!
//! Sibling API to [`crate::callsign_continuity::CallsignContinuityFilter`]
//! that returns a continuous score instead of a binary accept/reject.
//! Consumers (autonomous operator decision logic, TUI display ordering,
//! priority scorer) can pick their own threshold.
//!
//! ## Empirical justification
//!
//! Batch 31 Diagnostic R+S extracted per-decode features on full
//! hard-200 + 50 noise windows (6969 samples, 4937 TPs, 2032 FPs).
//! Per-feature Mann-Whitney AUC for TP vs FP discrimination:
//!
//! | Feature                | AUC   | Note     |
//! |------------------------|-------|----------|
//! | in_trust_set (both)    | 0.837 | strong   |
//! | in_trust_set (any)     | 0.755 | strong (hb-062's binary criterion) |
//! | confidence             | 0.706 | strong   |
//! | snr_db                 | 0.578 | weak     |
//! | time_offset (inverted) | 0.576 | weak     |
//! | has_report             | 0.565 | weak     |
//! | error_corrections      | 0.500 | dead (unused field in eval) |
//! | ap_level               | 0.500 | dead (AP not in eval default) |
//! | text_len, has_grid     | ~0.50 | uncorrelated |
//!
//! Batch 31 Diagnostic T: weighted-sum fused score (the formula in
//! [`content_score_from_features`]) achieves AUC **0.886** on the same
//! corpus. At a +2.977 threshold, 98% TP recall is preserved while FP
//! reduction reaches 72.9%.
//!
//! ## Operating curve (from Diagnostic T)
//!
//! | Threshold | TP recall | FP reduction |
//! |-----------|----------:|-------------:|
//! | -0.07     | 100.0%    | 17.2%        |
//! | +0.35     | 100.0%    | 34.3%        |
//! | +2.98     | 98.0%     | 72.9%        |
//! | +3.16     | 95.7%     | 75.4%        |
//! | +3.63     | 64.6%     | 85.6%        |
//! | +3.84     | 32.6%     | 93.3%        |
//!
//! ## Recommendation
//!
//! - **For autonomous TX decisions** (`OperatorAction::Transmit`):
//!   require [`MessageContentScore::SHIP_PRECISE`] (+2.98) or higher.
//!   2% recall cost is acceptable; 73% FP reduction prevents bad TX.
//! - **For TUI display ordering**: rank by score for operator-facing
//!   priority.
//! - **For PSKReporter spotting**: no filter; recall preserved.
//! - **For storage/logging**: no filter; recall preserved.
//!
//! Production `CallsignContinuityFilter::accept()` is NOT changed —
//! continues to apply only the binary trust-set + high-risk-pattern
//! gates. This score is purely additive and consumer-driven.

use crate::callsign_continuity::CallsignContinuityFilter;

/// A continuous trust score derivable from a `DecodedMessage`-shaped
/// input. Consumers can fuse this with their own thresholds.
///
/// Score components and weights are documented in module-level docs
/// and were tuned on Batch 31's hard-200 + noise corpus.
pub struct MessageContentScore;

impl MessageContentScore {
    /// Threshold from Diagnostic T's 98%-recall operating point. Suitable
    /// for high-precision decision paths (autonomous TX).
    pub const SHIP_PRECISE: f64 = 2.977;

    /// Threshold from Diagnostic T's 95.7%-recall operating point.
    /// Trades 4.3% recall for 75.4% FP reduction.
    pub const SHIP_AGGRESSIVE: f64 = 3.160;

    /// Threshold from Diagnostic T's 100%-recall, max-precision-at-recall
    /// point (+0.35). Acceptable for log-only filtering.
    pub const SHIP_CONSERVATIVE: f64 = 0.352;
}

/// Per-decode features needed to compute the content score. The decoder
/// emits these fields on `DecodedMessage`; we accept them as primitives
/// to avoid making `pancetta-qso` depend on `pancetta-ft8`.
///
/// Batch 64 added the four optional FDR confidence-telemetry fields
/// (`bp_iterations_used`, `osd_depth_used`, `nharderrs`,
/// `min_llr_magnitude`), populated by the decoder's FDR Sessions 1-3
/// pipeline. Pre-FDR call sites can leave them `None`; the v1 score
/// formula ignores them so existing consumers are byte-identical.
#[derive(Debug, Clone, Copy)]
pub struct ContentFeatures<'a> {
    /// Full decoded message text (e.g., `"CQ K1ABC FN42"`). Used to
    /// extract callsigns for trust-set membership.
    pub text: &'a str,
    /// Decoder confidence (typically in `[0, 1]`).
    pub confidence: f32,
    /// Estimated SNR in dB.
    pub snr_db: f32,
    /// Time offset (seconds) of the decode within its slot.
    pub time_offset: f64,
    /// BP iterations to convergence (FDR Session 2). Lower = more
    /// confident. `None` when telemetry was absent.
    pub bp_iterations_used: Option<u8>,
    /// OSD depth at codeword acceptance (FDR Session 3). `None` when
    /// BP converged direct, or when telemetry was absent.
    pub osd_depth_used: Option<u8>,
    /// Hard-decision bit errors corrected by OSD (FDR Session 3).
    pub nharderrs: Option<u8>,
    /// Smallest `|LLR|` across the converged codeword (FDR Session 2).
    pub min_llr_magnitude: Option<f32>,
    /// hb-103 v3: recovery-lateness fraction in `[0, 1]`. Late /
    /// aggressive-recovery decodes are markedly more FP-prone.
    /// Preferred source (Batch 81, deterministic): `decode_origin / 6`
    /// from `ConfidenceFeatures::decode_origin` (hb-247) — held-out
    /// ΔAUC +0.040/+0.047 over v2 and byte-identical across runs.
    /// The divisor stays 6 by design: origin 7 (hb-252 BICM-ID rescue,
    /// Batch 98) yields 7/6 which the v3 clamp saturates at 1.0 —
    /// rescued decodes intentionally take the maximum lateness
    /// penalty.
    /// Legacy source (Batch 79, wall-clock proxy): decode time into
    /// window normalized by the slot max — load-sensitive, kept for
    /// research comparison only. `None` contributes nothing — v3
    /// reduces to v2.
    pub lateness_frac: Option<f64>,
}

/// Compute the fused content score for a decode given the active trust
/// filter.
///
/// Formula (Batch 31):
/// ```text
/// score = 2 * (both_callsigns_in_trust)
///       + 1 * (any_callsign_in_trust)
///       + 1 * confidence
///       - 0.1 * time_offset
///       + 0.05 * snr_db
/// ```
///
/// See module docs for the empirical AUC and operating curve.
pub fn content_score_from_features(
    feat: ContentFeatures<'_>,
    filter: &CallsignContinuityFilter,
) -> f64 {
    let calls = crate::callsign_continuity::callsigns_in(feat.text);
    let in_trust_any = calls.iter().any(|c| filter.would_accept_callsign(c));
    let in_trust_both = calls.len() >= 2 && calls.iter().all(|c| filter.would_accept_callsign(c));

    let mut score = 0.0_f64;
    if in_trust_both {
        score += 2.0;
    }
    if in_trust_any {
        score += 1.0;
    }
    score += feat.confidence as f64;
    score -= 0.1 * feat.time_offset;
    score += 0.05 * feat.snr_db as f64;
    score
}

/// hb-103 v2 — content score extended with FDR confidence telemetry
/// (`bp_iterations_used`, `osd_depth_used`, `nharderrs`,
/// `min_llr_magnitude`). The v1 components are unchanged; v2 adds:
///
/// ```text
/// v2_score = v1_score
///          + W_LLR * min_llr_magnitude
///          - W_BP * bp_iterations_used
///          - W_OSD * osd_depth_used
///          - W_NHE * nharderrs
/// ```
///
/// Missing telemetry fields default to neutral values (no contribution).
/// This means v2 reduces to v1 on decodes that don't carry FDR
/// features — the FFI path, AP/cross-sequence decodes, pre-Batch-58
/// constructors.
///
/// Weights are calibration-driven; Batch 64 ships placeholder values
/// and a probe (`batch64_content_score_v2_auc`) that computes AUC on
/// hard-200 vs v1 to drive ship-decision.
///
/// Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Inputs".
pub fn content_score_v2_from_features(
    feat: ContentFeatures<'_>,
    filter: &CallsignContinuityFilter,
) -> f64 {
    let v1 = content_score_from_features(feat, filter);

    // v2 weights — placeholders. Batch 64 probe calibrates.
    const W_LLR: f64 = 0.20;
    const W_BP: f64 = 0.01;
    const W_OSD: f64 = 0.10;
    const W_NHE: f64 = 0.05;

    let llr_term = match feat.min_llr_magnitude {
        Some(v) => W_LLR * v as f64,
        None => 0.0,
    };
    let bp_term = match feat.bp_iterations_used {
        Some(v) => -W_BP * v as f64,
        None => 0.0,
    };
    let osd_term = match feat.osd_depth_used {
        Some(v) => -W_OSD * v as f64,
        None => 0.0,
    };
    let nhe_term = match feat.nharderrs {
        Some(v) => -W_NHE * v as f64,
        None => 0.0,
    };

    v1 + llr_term + bp_term + osd_term + nhe_term
}

/// hb-103 v3 — v2 plus the decode-lateness term (Batch 79):
///
/// ```text
/// v3_score = v2_score + W_TIME * lateness_frac
/// ```
///
/// `lateness_frac` is the decode's wall-time into the decode window
/// normalized by the slot's max (see [`ContentFeatures::lateness_frac`]).
/// `W_TIME = -1.0` was selected by split-half grid search on hard_200 and
/// raw_530 (Batch 79: mean held-out ΔAUC +0.032 / +0.012 over v2 with
/// slot-max normalization; the weight was the held-out optimum on every
/// fold of both corpora). `None` contributes nothing, so v3 reduces to v2
/// (and to v1 when FDR telemetry is also absent) on decodes without the
/// feature — pre-existing consumers are byte-identical.
pub fn content_score_v3_from_features(
    feat: ContentFeatures<'_>,
    filter: &CallsignContinuityFilter,
) -> f64 {
    /// Batch 79 split-half optimum; negative because later = more FP-ish.
    const W_TIME: f64 = -1.0;
    let time_term = match feat.lateness_frac {
        Some(f) => W_TIME * f.clamp(0.0, 1.0),
        None => 0.0,
    };
    content_score_v2_from_features(feat, filter) + time_term
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn feat(text: &str, confidence: f32, snr_db: f32, dt: f64) -> ContentFeatures<'_> {
        ContentFeatures {
            text,
            confidence,
            snr_db,
            time_offset: dt,
            // v1 tests use the no-telemetry path (legacy behavior).
            bp_iterations_used: None,
            osd_depth_used: None,
            nharderrs: None,
            min_llr_magnitude: None,
            lateness_frac: None,
        }
    }

    #[test]
    fn both_in_trust_scores_higher_than_any() {
        // Two-callsign message: both K1ABC and W9XYZ in trust → both bonus fires.
        let mut filter_both = CallsignContinuityFilter::new(100);
        filter_both.extend_from_iter(["K1ABC", "W9XYZ"]);
        let both_score =
            content_score_from_features(feat("K1ABC W9XYZ FN42", 0.95, 0.0, 1.0), &filter_both);
        // Two-callsign message: only K1ABC in trust → only `any` bonus fires.
        let mut filter_any = CallsignContinuityFilter::new(100);
        filter_any.extend_from_iter(["K1ABC"]);
        let any_score =
            content_score_from_features(feat("K1ABC W9XYZ FN42", 0.95, 0.0, 1.0), &filter_any);
        // both: 2 + 1 + 0.95 + 0 + 0 = 3.95
        // any:  0 + 1 + 0.95 + 0 + 0 = 1.95
        assert!(both_score > any_score);
    }

    #[test]
    fn untrusted_scores_below_trusted() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC"]);
        let trusted = content_score_from_features(feat("CQ K1ABC FN42", 0.95, 0.0, 1.0), &filter);
        let untrusted = content_score_from_features(feat("CQ K9XYZ EM10", 0.95, 0.0, 1.0), &filter);
        assert!(trusted > untrusted);
    }

    #[test]
    fn low_confidence_lowers_score() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC", "W9XYZ"]);
        let high = content_score_from_features(feat("K1ABC W9XYZ FN42", 0.99, 0.0, 1.0), &filter);
        let low = content_score_from_features(feat("K1ABC W9XYZ FN42", 0.50, 0.0, 1.0), &filter);
        assert!(high > low);
    }

    #[test]
    fn late_dt_lowers_score() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC", "W9XYZ"]);
        let early = content_score_from_features(feat("K1ABC W9XYZ FN42", 0.95, 0.0, 1.0), &filter);
        let late = content_score_from_features(feat("K1ABC W9XYZ FN42", 0.95, 0.0, 10.0), &filter);
        assert!(early > late);
    }

    #[test]
    fn ship_thresholds_are_ordered() {
        assert!(MessageContentScore::SHIP_CONSERVATIVE < MessageContentScore::SHIP_PRECISE);
        assert!(MessageContentScore::SHIP_PRECISE < MessageContentScore::SHIP_AGGRESSIVE);
    }

    #[test]
    fn unknown_callsign_scores_low_enough_to_reject_at_precise() {
        let filter = CallsignContinuityFilter::new(100);
        let _ = filter; // empty trust set
        let _trust: HashSet<String> = HashSet::new();
        // No trust → no in_trust bonuses; confidence + small SNR + dt
        // penalty should sit well below SHIP_PRECISE (2.977).
        let score = content_score_from_features(feat("CQ K7ZZX EM10", 0.95, -5.0, 1.0), &filter);
        assert!(score < MessageContentScore::SHIP_PRECISE);
    }

    // hb-103 v2 — ConfidenceFeatures consumer tests (Batch 64).

    fn v2_feat(
        text: &str,
        bp: Option<u8>,
        osd: Option<u8>,
        nhe: Option<u8>,
        llr: Option<f32>,
    ) -> ContentFeatures<'_> {
        ContentFeatures {
            text,
            confidence: 0.9,
            snr_db: 0.0,
            time_offset: 1.0,
            bp_iterations_used: bp,
            osd_depth_used: osd,
            nharderrs: nhe,
            min_llr_magnitude: llr,
            lateness_frac: None,
        }
    }

    // hb-103 v3 — decode-lateness term tests (Batch 80).

    #[test]
    fn v3_with_no_time_feature_equals_v2() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC"]);
        let f = v2_feat("CQ K1ABC FN42", Some(20), None, None, Some(1.5));
        let v2 = content_score_v2_from_features(f, &filter);
        let v3 = content_score_v3_from_features(f, &filter);
        assert!(
            (v2 - v3).abs() < 1e-9,
            "v3 with lateness_frac=None must equal v2 (v2={v2}, v3={v3})"
        );
    }

    #[test]
    fn v3_late_decode_scores_below_early() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC"]);
        let mut early = v2_feat("CQ K1ABC FN42", None, None, None, None);
        early.lateness_frac = Some(0.05);
        let mut late = early;
        late.lateness_frac = Some(1.0);
        let s_early = content_score_v3_from_features(early, &filter);
        let s_late = content_score_v3_from_features(late, &filter);
        // W_TIME = -1.0 → a full-window-late decode loses 0.95 vs the early one.
        assert!((s_early - s_late - 0.95).abs() < 1e-9);
    }

    #[test]
    fn v3_time_frac_is_clamped() {
        let filter = CallsignContinuityFilter::new(100);
        let mut f = v2_feat("CQ K1ABC FN42", None, None, None, None);
        f.lateness_frac = Some(7.5); // out-of-range input
        let clamped = content_score_v3_from_features(f, &filter);
        f.lateness_frac = Some(1.0);
        let unit = content_score_v3_from_features(f, &filter);
        assert!((clamped - unit).abs() < 1e-9, "frac must clamp to [0,1]");
    }

    #[test]
    fn v2_with_all_none_features_equals_v1() {
        // ConfidenceFeatures absent → v2 reduces to v1.
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC"]);
        let feat = v2_feat("CQ K1ABC FN42", None, None, None, None);
        let v1 = content_score_from_features(feat, &filter);
        let v2 = content_score_v2_from_features(feat, &filter);
        assert!(
            (v1 - v2).abs() < 1e-9,
            "v2 with all-None features must equal v1 (v1={v1}, v2={v2})"
        );
    }

    #[test]
    fn v2_high_confidence_features_score_higher_than_low() {
        let mut filter = CallsignContinuityFilter::new(100);
        filter.extend_from_iter(["K1ABC"]);
        // High confidence: low BP iters, no OSD, no hard errors, high min LLR.
        let good = v2_feat("CQ K1ABC FN42", Some(5), None, None, Some(3.0));
        // Low confidence: high BP iters, deep OSD, many hard errors, low min LLR.
        let poor = v2_feat("CQ K1ABC FN42", Some(50), Some(3), Some(3), Some(0.05));
        let s_good = content_score_v2_from_features(good, &filter);
        let s_poor = content_score_v2_from_features(poor, &filter);
        assert!(
            s_good > s_poor,
            "good-features decode should score higher than poor (good={s_good}, poor={s_poor})"
        );
    }

    #[test]
    fn v2_partial_features_contribute_their_signal() {
        // Only min_llr_magnitude set → adds +W_LLR * llr without
        // penalties from missing fields.
        let filter = CallsignContinuityFilter::new(100);
        let no_llr = v2_feat("CQ K7ZZX EM10", None, None, None, None);
        let with_llr = v2_feat("CQ K7ZZX EM10", None, None, None, Some(2.0));
        let a = content_score_v2_from_features(no_llr, &filter);
        let b = content_score_v2_from_features(with_llr, &filter);
        assert!(b > a, "min_llr_magnitude must contribute additively");
    }
}
