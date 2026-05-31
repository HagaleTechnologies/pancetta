//! Callsign-prior aggregator (hb-087 Session 2).
//!
//! Aggregates the prior sources used by the bypass-Costas AP-constrained
//! residual decode mechanism (hb-087). Research-only: production decoder
//! is **not** plumbed against this struct (Session 3 work).
//!
//! ## Sources aggregated
//!
//! 1. **Operator callsign** — single fixed call (e.g. K5ARH); zero
//!    FP-injection risk because the operator always knows their own call.
//! 2. **Recent-this-WAV decodes** — every callsign extracted from any
//!    pancetta decode of the current WAV. Stands in for the rolling
//!    15-30 min window
//!    [`pancetta_qso::callsign_continuity::CallsignContinuityFilter`]
//!    maintains in production. Populated as pass-1 decodes accumulate.
//! 3. **Bundled-common-active** — small static list of frequently-heard
//!    callsigns (DXpedition staples + permanent-active contesters).
//!    Stands in for the cqdx-spotted callsigns prior that production
//!    pulls from [`pancetta_cqdx::Cache::spotted_callsigns`].
//! 4. **cqdx-spotted** — runtime-supplied set of currently-spotted
//!    callsigns. Empty in this Session 2 micro-test (cqdx is not wired
//!    into the eval harness); reserved for Session 3 plumbing.
//!
//! ## Hb-087 design context
//!
//! Session 1's diagnostic (`hb087_callsign_priors_feasibility`) measured
//! 23.6% callsign coverage on missed truths in the top-20 worst hard-200
//! WAVs, above the 20% PROCEED gate. This Session 2 aggregator is the
//! plumbing layer for Session 3's production-side
//! `callsign_prior_residual_pass`; for Session 2 it powers the per-truth
//! AP-decode micro-test
//! (`examples/hb087_session2_ap_decode_microtest.rs`) that gates the
//! Session 2 → Session 3 boundary.
//!
//! Spec:
//! `docs/superpowers/specs/2026-05-31-hb-087-callsign-priors-design.md`

use std::collections::HashSet;

/// Hand-picked list of very-active / DXpedition stations on FT8. Used as
/// a stand-in for the production cqdx-spotted prior. Deliberately small
/// (~75 callsigns) — cqdx-spots in production is broader and band/time
/// filtered, but the bundled list captures the structural mix (NA + EU +
/// AS + AF + OC + DXpedition staples).
///
/// Sourced from common LotW activity + DXpedition history. Identical to
/// the list used by the Session 1 feasibility diagnostic so that
/// `from_session1_pool` reproduces the diagnostic's coverage figures
/// exactly.
pub const BUNDLED_COMMON_ACTIVE: &[&str] = &[
    // North America active
    "K1JT", "K9AN", "W1AW", "WB2FKO", "W2NRA", "K3LR", "N4YDU", "W5KFT", "K6ND", "W7RN", "K8GP",
    "W9RE", "N0NI", "K0PC", "VE3EJ", "VE5SF", "VE9AA", "K2LE", "N3RS", "K4ZW", "K5ZD", "W6YA",
    "N7AT", "K8AZ", "W9KKN", "K0RF", "VE2IM", "VY2ZM", "XE2X", // South America
    "CE3CT", "CW5W", "LU8YE", "PY5EG", "PY2NY", "ZP6CW", "HC8N", // EU active
    "DL1IAO", "DL6FBL", "DJ5IW", "DR1A", "G4PIQ", "G3SXW", "GW3YDX", "EI7M", "F5IN", "F6BEE",
    "I4VEQ", "IK2QEI", "ON4UN", "OZ4UN", "PA0LSK", "S52ZW", "SK3W", "SM5AJV", "9A1A", "OK1RF",
    "OM3RM", "YU1ZZ", "Z32U", "Z37M", // EU/AF border + AF active
    "EA6NB", "CT3MD", "CN2AA", "ZS6Y", "CN8KD", "EA8RM", "S01WS", // Asia active
    "JA1NLX", "JA7QVI", "JE1JKL", "JH4ADV", "BG2AUE", "BY1RX", "HL5IVL", "VR2BG", "9V1YC", "BV9G",
    // OC active
    "VK3ER", "VK6IR", "VK9NI", "ZL3IX", "ZL1ANH", "ZL2IFB", "FK8GM", "T88AT",
    // Recent / current DXpedition staples
    "VP6R", "TX5S", "VK0EK", "FT5ZM", "K9W", "ZL9A", "VP8STI", "VP8SGI", "5W1SA", "TI9A", "3Y0Z",
    "T31EU",
];

/// Aggregated prior sources for hb-087 AP-constrained residual decoding.
///
/// Cheap to construct, cheap to iterate. Production use (Session 3) will
/// hold this behind `Arc` in `Ft8Config`; research use (Session 2 here)
/// constructs one per WAV.
#[derive(Debug, Clone)]
pub struct CallsignPriorSet {
    /// Operator's own callsign, if known. Used for AP1-style "called-
    /// station = operator" injection.
    pub operator: Option<String>,
    /// Callsigns extracted from pancetta decodes of the *current* WAV.
    /// Mirrors the production rolling 15-30 min window (one slot's
    /// worth here, because hard-200 WAVs are disjoint 15-second cuts).
    pub recent_this_wav: Vec<String>,
    /// Static slice of highly-active callsigns. Stands in for
    /// cqdx-spotted under research/eval conditions.
    pub bundled_common: &'static [&'static str],
    /// Runtime-injected cqdx-spotted set. Empty in Session 2; populated
    /// in Session 3 via the production prior-aggregator wire.
    pub cqdx_spotted: HashSet<String>,
}

impl CallsignPriorSet {
    /// Empty prior set — useful as a baseline / default.
    pub fn empty() -> Self {
        Self {
            operator: None,
            recent_this_wav: Vec::new(),
            bundled_common: BUNDLED_COMMON_ACTIVE,
            cqdx_spotted: HashSet::new(),
        }
    }

    /// Construct a prior set the same way Session 1's feasibility
    /// diagnostic did: an operator call, a per-WAV recent-decodes list
    /// extracted from pancetta decodes, the bundled-common-active static
    /// list, and an empty cqdx-spotted set. Reproduces the diagnostic's
    /// 23.6% per-source breakdown when used with the same inputs.
    pub fn from_session1_pool(operator: Option<&str>, recent_this_wav: Vec<String>) -> Self {
        Self {
            operator: operator.map(|s| s.to_uppercase()),
            recent_this_wav,
            bundled_common: BUNDLED_COMMON_ACTIVE,
            cqdx_spotted: HashSet::new(),
        }
    }

    /// Iterate the union of all sources as uppercase, deduped, capped
    /// at `max` entries.
    ///
    /// Ordering favours specificity:
    ///   1. operator (most specific, lowest FP risk),
    ///   2. recent-this-WAV (high coverage — Session 1 measured 23.2%),
    ///   3. cqdx_spotted (broader, ranked-by-rarity in production),
    ///   4. bundled_common (broadest, baseline).
    ///
    /// Session 3 will likely refine the ordering once we have measured
    /// per-source recovery yield; the Session 2 micro-test just needs a
    /// deterministic union for the kill-switch.
    pub fn iter_unique(&self, max: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(max.min(256));
        let mut seen: HashSet<String> = HashSet::with_capacity(max.min(256));
        let push = |s: String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            if seen.insert(s.clone()) {
                out.push(s);
            }
        };
        if let Some(op) = &self.operator {
            push(op.to_uppercase(), &mut out, &mut seen);
            if out.len() >= max {
                return out;
            }
        }
        for c in &self.recent_this_wav {
            push(c.to_uppercase(), &mut out, &mut seen);
            if out.len() >= max {
                return out;
            }
        }
        for c in &self.cqdx_spotted {
            push(c.to_uppercase(), &mut out, &mut seen);
            if out.len() >= max {
                return out;
            }
        }
        for c in self.bundled_common {
            push(c.to_uppercase(), &mut out, &mut seen);
            if out.len() >= max {
                return out;
            }
        }
        out
    }

    /// Whether `callsign` (case-insensitive) appears in any of the
    /// configured sources.
    pub fn contains(&self, callsign: &str) -> bool {
        let up = callsign.to_uppercase();
        if let Some(op) = &self.operator {
            if op.to_uppercase() == up {
                return true;
            }
        }
        if self.recent_this_wav.iter().any(|c| c.to_uppercase() == up) {
            return true;
        }
        if self.cqdx_spotted.iter().any(|c| c.to_uppercase() == up) {
            return true;
        }
        if self.bundled_common.iter().any(|c| c.eq_ignore_ascii_case(&up)) {
            return true;
        }
        false
    }

    /// Which source(s) contain `callsign`. Useful for the Session 2
    /// per-truth report — surfaces which prior source produced each
    /// covered truth so we can see whether recent-window dominates as
    /// the diagnostic predicted.
    pub fn source_of(&self, callsign: &str) -> PriorSourceMask {
        let up = callsign.to_uppercase();
        let mut mask = PriorSourceMask::default();
        if let Some(op) = &self.operator {
            if op.to_uppercase() == up {
                mask.operator = true;
            }
        }
        if self.recent_this_wav.iter().any(|c| c.to_uppercase() == up) {
            mask.recent = true;
        }
        if self.cqdx_spotted.iter().any(|c| c.to_uppercase() == up) {
            mask.cqdx = true;
        }
        if self.bundled_common.iter().any(|c| c.eq_ignore_ascii_case(&up)) {
            mask.bundled = true;
        }
        mask
    }
}

/// Bit-mask of which prior source(s) contain a given callsign. Multiple
/// flags can be set simultaneously (e.g. a recent-window callsign is
/// also in the bundled list).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorSourceMask {
    pub operator: bool,
    pub recent: bool,
    pub cqdx: bool,
    pub bundled: bool,
}

impl PriorSourceMask {
    /// Compact one-letter source label, prioritised by specificity:
    /// O > R > C > B. Useful for the Session 2 micro-test table.
    pub fn label(&self) -> &'static str {
        if self.operator {
            "operator"
        } else if self.recent {
            "recent"
        } else if self.cqdx {
            "cqdx"
        } else if self.bundled {
            "bundled"
        } else {
            "none"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prior_set_iterates_only_bundled() {
        let s = CallsignPriorSet::empty();
        let v = s.iter_unique(10_000);
        // Bundled-common has 75-100 entries; iter_unique returns them.
        assert!(v.len() >= 50, "bundled list too small: {}", v.len());
        // All entries should be present uppercase.
        assert!(v.iter().all(|c| c.chars().all(|x| !x.is_ascii_lowercase())));
        // Should contain known bundled entries.
        assert!(v.contains(&"K1JT".to_string()));
        assert!(v.contains(&"DL1IAO".to_string()));
    }

    #[test]
    fn iter_unique_dedupes_across_sources() {
        let mut s = CallsignPriorSet::empty();
        // Inject K1JT into recent — it's also in bundled. Should appear once.
        s.recent_this_wav = vec!["k1jt".to_string()];
        let v = s.iter_unique(1000);
        let count = v.iter().filter(|c| c.as_str() == "K1JT").count();
        assert_eq!(count, 1, "K1JT should appear exactly once after dedup");
    }

    #[test]
    fn iter_unique_caps_at_max() {
        let s = CallsignPriorSet::from_session1_pool(
            Some("K5ARH"),
            vec!["W1AW".to_string(), "VE3EJ".to_string()],
        );
        let v = s.iter_unique(2);
        assert_eq!(v.len(), 2);
        // Operator is first.
        assert_eq!(v[0], "K5ARH");
    }

    #[test]
    fn iter_unique_operator_first() {
        let s = CallsignPriorSet::from_session1_pool(Some("K5ARH"), vec!["W1AW".to_string()]);
        let v = s.iter_unique(100);
        assert_eq!(v[0], "K5ARH");
        assert!(v.iter().position(|c| c == "K5ARH").unwrap()
            < v.iter().position(|c| c == "W1AW").unwrap());
    }

    #[test]
    fn contains_is_case_insensitive() {
        let s = CallsignPriorSet::from_session1_pool(Some("K5ARH"), vec!["w1aw".to_string()]);
        assert!(s.contains("K5ARH"));
        assert!(s.contains("k5arh"));
        assert!(s.contains("W1AW"));
        assert!(s.contains("w1aw"));
        // bundled also case-insensitive
        assert!(s.contains("k1jt"));
        // Negative
        assert!(!s.contains("ZZ9XYZ"));
    }

    #[test]
    fn source_of_reports_correct_origin() {
        let s = CallsignPriorSet::from_session1_pool(
            Some("K5ARH"),
            vec!["W1AW".to_string()],
        );
        assert_eq!(s.source_of("K5ARH").label(), "operator");
        // W1AW is in BOTH recent_this_wav AND bundled; recent wins by priority
        // because operator beats recent which beats bundled, and W1AW isn't operator.
        let mask = s.source_of("W1AW");
        assert!(mask.recent);
        assert!(mask.bundled);
        assert_eq!(mask.label(), "recent");
        // K1JT is only in bundled
        assert_eq!(s.source_of("K1JT").label(), "bundled");
        // unknown returns none
        assert_eq!(s.source_of("ZZ9XYZ").label(), "none");
    }
}
