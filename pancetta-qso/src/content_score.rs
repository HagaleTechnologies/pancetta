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
}
