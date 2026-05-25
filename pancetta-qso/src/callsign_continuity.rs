//! Callsign-continuity FP filter for production decode pipeline.
//!
//! hb-052 production version. The eval-harness MVP
//! (pancetta-research/src/fp_filter.rs) showed -21.7% novels at -0.02%
//! recall on hard-200 with a corpus-baseline reference set. Production
//! deployment uses three combined sources:
//!
//! 1. **ADIF log** — operator's logged QSO callsigns (~/.pancetta/qsos.adi)
//! 2. **Rolling window** — callsigns from recent decodes this session
//! 3. **cqdx.io spots** — live network-wide spotted callsigns
//!
//! The filter accepts a decode if any of its extracted callsigns appear
//! in the union of those three sources. Decodes with no extractable
//! callsigns are rejected.
//!
//! **Cold-start handling:** at session start, the rolling window is
//! empty and cqdx may not have polled yet. The filter has a "lenient"
//! mode (constructor `new_lenient`) that accepts all decodes until the
//! reference set reaches a configurable threshold size. Once the
//! threshold is crossed, the filter activates.
//!
//! Threading: `accept` takes `&self` and uses interior mutability for
//! the rolling window — safe to share across the coordinator's decode
//! pipeline.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::RwLock;

/// Extract bare callsign tokens from an FT8 decoded message.
/// Returns the first 2 callsign-shaped tokens after stripping
/// `CQ`/CQ-modifier prefixes and `/R`,`/P`,`/QRP` suffixes.
pub fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    if idx < tokens.len() && tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
    }
    for t in tokens.iter().skip(idx).take(2) {
        if looks_like_callsign(t) {
            let bare = t.split('/').next().unwrap_or(t);
            out.push(bare.to_string());
        }
    }
    out
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}

/// Parse ADIF text and return all CALL field values. Format:
/// `<NAME:LENGTH>VALUE`. Case-insensitive tag matching. Tolerates
/// `<CALL:5:S>` typed fields.
pub fn parse_adif_calls(text: &str) -> Vec<String> {
    let upper = text.to_uppercase();
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = upper[search_from..].find("<CALL:") {
        let start = search_from + rel + "<CALL:".len();
        let rest = &upper[start..];
        let len_end = match rest.find('>') {
            Some(i) => i,
            None => break,
        };
        let len_spec = &rest[..len_end];
        let len_only = len_spec.split(':').next().unwrap_or(len_spec);
        let value_len: usize = match len_only.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                search_from = start + len_end + 1;
                continue;
            }
        };
        let value_start = start + len_end + 1;
        let value_end = value_start.saturating_add(value_len).min(upper.len());
        let value = text[value_start..value_end].trim().to_string();
        if !value.is_empty() {
            out.push(value);
        }
        search_from = value_end;
    }
    out
}

/// Production callsign-continuity filter. Reference set built from:
/// - Static ADIF log (loaded once on construction or via extend_from_adif)
/// - Rolling window of recent decodes (interior-mutable)
/// - cqdx.io spots (refreshed by the caller via update_spotted)
///
/// Thread-safe via RwLock on rolling window + cqdx set.
pub struct CallsignContinuityFilter {
    /// Static reference: operator's ADIF log (and any explicit additions).
    /// Built up before/during startup; not modified per-decode.
    static_ref: HashSet<String>,
    /// Rolling window from this session's recent decodes.
    rolling: RwLock<VecDeque<String>>,
    /// Capacity of the rolling window.
    rolling_cap: usize,
    /// cqdx.io spotted callsigns; refreshed periodically by the bridge.
    cqdx_spotted: RwLock<HashSet<String>>,
    /// When `static_ref + cqdx_spotted` is below this threshold, the
    /// filter passes everything (lenient cold-start). Once the threshold
    /// is crossed, the filter actively rejects.
    cold_start_threshold: usize,
}

impl CallsignContinuityFilter {
    /// Strict filter: rejects from the first decode.
    pub fn new(rolling_cap: usize) -> Self {
        Self {
            static_ref: HashSet::new(),
            rolling: RwLock::new(VecDeque::new()),
            rolling_cap,
            cqdx_spotted: RwLock::new(HashSet::new()),
            cold_start_threshold: 0,
        }
    }

    /// Lenient filter: passes everything until reference set ≥ threshold.
    /// Recommended for production — avoids dropping legitimate first-of-session
    /// decodes before ADIF/cqdx populate.
    pub fn new_lenient(rolling_cap: usize, cold_start_threshold: usize) -> Self {
        Self {
            static_ref: HashSet::new(),
            rolling: RwLock::new(VecDeque::new()),
            rolling_cap,
            cqdx_spotted: RwLock::new(HashSet::new()),
            cold_start_threshold,
        }
    }

    /// Add callsigns from an ADIF log file. Strips suffixes; uppercases.
    /// Returns count added.
    pub fn extend_from_adif(&mut self, path: &Path) -> std::io::Result<usize> {
        let text = std::fs::read_to_string(path)?;
        let before = self.static_ref.len();
        for c in parse_adif_calls(&text) {
            let bare = c.split('/').next().unwrap_or(&c).to_uppercase();
            if !bare.is_empty() {
                self.static_ref.insert(bare);
            }
        }
        Ok(self.static_ref.len() - before)
    }

    /// Add explicit callsigns to the static reference (test/admin use).
    pub fn extend_from_iter<I, S>(&mut self, calls: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for c in calls {
            let s = c.as_ref().to_uppercase();
            if !s.is_empty() {
                self.static_ref.insert(s);
            }
        }
    }

    /// Update the cqdx-spotted set. Called periodically by the coordinator's
    /// cqdx bridge after each spot poll.
    pub fn update_cqdx_spotted(&self, spotted: HashSet<String>) {
        if let Ok(mut g) = self.cqdx_spotted.write() {
            *g = spotted;
        }
    }

    /// Current effective reference size (static + cqdx). Used by the
    /// cold-start gate. Excludes the rolling window because that's a
    /// derivative of accepted decodes.
    pub fn reference_size(&self) -> usize {
        let cqdx = self.cqdx_spotted.read().map(|g| g.len()).unwrap_or(0);
        self.static_ref.len() + cqdx
    }

    /// True if any of the message's extracted callsigns appear in any
    /// source. In lenient mode, returns true when the reference set is
    /// below threshold (passing everything through to populate the
    /// rolling window).
    ///
    /// Always pushes the decode's callsigns into the rolling window on
    /// acceptance (or in lenient cold-start), so the window keeps growing.
    pub fn accept(&self, message: &str) -> bool {
        let calls = callsigns_in(message);
        // Cold-start lenient mode: accept everything until reference is big enough.
        if self.cold_start_threshold > 0 && self.reference_size() < self.cold_start_threshold {
            // Still update rolling so it pre-populates for when strict mode kicks in.
            if !calls.is_empty() {
                self.push_rolling(&calls);
            }
            return true;
        }
        if calls.is_empty() {
            return false;
        }
        let in_static = calls.iter().any(|c| self.static_ref.contains(c));
        let in_cqdx = self
            .cqdx_spotted
            .read()
            .map(|g| calls.iter().any(|c| g.contains(c)))
            .unwrap_or(false);
        let in_rolling = self
            .rolling
            .read()
            .map(|g| calls.iter().any(|c| g.iter().any(|q| q == c)))
            .unwrap_or(false);
        if !(in_static || in_cqdx || in_rolling) {
            return false;
        }
        self.push_rolling(&calls);
        true
    }

    fn push_rolling(&self, calls: &[String]) {
        if let Ok(mut g) = self.rolling.write() {
            for c in calls {
                if !g.iter().any(|q| q == c) {
                    g.push_back(c.clone());
                    while g.len() > self.rolling_cap {
                        g.pop_front();
                    }
                }
            }
        }
    }
}

/// hb-062: convenience builder. Construct a production filter from an
/// optional ADIF path + initial cqdx-spotted snapshot + rolling-window
/// capacity + cold-start threshold. The cqdx snapshot can be empty at
/// construction; the coordinator calls `update_cqdx_spotted` periodically
/// via the cqdx bridge.
///
/// `cold_start_threshold = 0` → strict from first decode.
/// `cold_start_threshold > 0` → lenient until reference_size() ≥ threshold.
pub fn build_filter(
    adif_path: Option<&Path>,
    initial_cqdx_spotted: HashSet<String>,
    rolling_cap: usize,
    cold_start_threshold: usize,
) -> std::io::Result<CallsignContinuityFilter> {
    let mut f = if cold_start_threshold > 0 {
        CallsignContinuityFilter::new_lenient(rolling_cap, cold_start_threshold)
    } else {
        CallsignContinuityFilter::new(rolling_cap)
    };
    if let Some(p) = adif_path {
        if p.exists() {
            f.extend_from_adif(p)?;
        }
    }
    f.update_cqdx_spotted(initial_cqdx_spotted);
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn strict_filter_rejects_unknown_callsigns() {
        let f = CallsignContinuityFilter::new(100);
        assert!(!f.accept("CQ K1ABC FN42"));
    }

    #[test]
    fn strict_filter_accepts_known_callsign() {
        let mut f = CallsignContinuityFilter::new(100);
        f.extend_from_iter(["K1ABC"]);
        assert!(f.accept("CQ K1ABC FN42"));
        assert!(!f.accept("CQ ZZ0ZZZ AA00"));
    }

    #[test]
    fn cqdx_source_accepts_via_spots() {
        let f = CallsignContinuityFilter::new(100);
        let mut spotted = HashSet::new();
        spotted.insert("K1ABC".to_string());
        f.update_cqdx_spotted(spotted);
        assert!(f.accept("CQ K1ABC FN42"));
    }

    #[test]
    fn rolling_window_grows_via_static_match() {
        let mut f = CallsignContinuityFilter::new(10);
        f.extend_from_iter(["K1ABC"]);
        // K1ABC in static → accept; pushes K1ABC + FN42 to rolling.
        assert!(f.accept("CQ K1ABC FN42"));
        // W9XYZ not in static, but K1ABC is → accept; W9XYZ now in rolling.
        assert!(f.accept("K1ABC W9XYZ EM48"));
        // ZZ0ZZZ has no anchor → reject.
        assert!(!f.accept("ZZ0ZZZ AA0AA AA00"));
    }

    #[test]
    fn lenient_cold_start_passes_until_threshold() {
        let f = CallsignContinuityFilter::new_lenient(100, 5);
        // No static, no cqdx → reference_size()=0 < 5 → lenient → accept all.
        assert!(f.accept("CQ ZZ0ZZZ AA00"));
        assert!(f.accept("ANY GARBAGE WITH NO CALLSIGN"));
    }

    #[test]
    fn lenient_activates_when_reference_grows() {
        let mut f = CallsignContinuityFilter::new_lenient(100, 3);
        // Lenient mode initially.
        assert!(f.accept("ZZ0ZZZ AA00"));
        // Add 3 static callsigns → threshold met → strict mode.
        f.extend_from_iter(["K1ABC", "W9XYZ", "DL5XYZ"]);
        assert_eq!(f.reference_size(), 3);
        assert!(f.accept("CQ K1ABC FN42"));
        assert!(!f.accept("CQ AA0AA BB11"));
    }

    #[test]
    fn build_filter_combines_adif_and_cqdx() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "<CALL:5>K1ABC <EOR>\n<CALL:5>W9XYZ <EOR>").unwrap();
        let mut spotted = HashSet::new();
        spotted.insert("DL5XYZ".to_string());
        let f = build_filter(Some(tmp.path()), spotted, 100, 0).unwrap();
        // Static from ADIF
        assert!(f.accept("CQ K1ABC FN42"));
        assert!(f.accept("CQ W9XYZ FN42"));
        // cqdx-spotted
        assert!(f.accept("CQ DL5XYZ FN42"));
        // Unknown
        assert!(!f.accept("CQ ZZ0ZZZ AA00"));
    }

    #[test]
    fn build_filter_lenient_mode() {
        let f = build_filter(None, HashSet::new(), 100, 5).unwrap();
        // Lenient: reference empty → accept all
        assert!(f.accept("CQ ZZ0ZZZ AA00"));
    }

    #[test]
    fn build_filter_missing_adif_path_is_ok() {
        // Non-existent ADIF path doesn't error — just no callsigns added.
        let nonexistent = std::path::PathBuf::from("/tmp/this-path-does-not-exist-12345.adi");
        let f = build_filter(Some(&nonexistent), HashSet::new(), 100, 0).unwrap();
        // Strict + empty reference → reject
        assert!(!f.accept("CQ K1ABC FN42"));
    }

    #[test]
    fn extend_from_adif_loads_callsigns() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "<CALL:5>K1ABC <EOR>\n<CALL:5>W9XYZ <EOR>\n<CALL:7>DL5XYZ/P <EOR>"
        )
        .unwrap();
        let mut f = CallsignContinuityFilter::new(100);
        let n = f.extend_from_adif(tmp.path()).unwrap();
        assert_eq!(n, 3); // suffix /P stripped from DL5XYZ
        assert!(f.accept("CQ K1ABC FN42"));
        assert!(f.accept("CQ W9XYZ FN42"));
        assert!(f.accept("DL5XYZ K1ABC -10"));
    }
}
