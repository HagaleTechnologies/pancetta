//! False-positive filter — callsign-continuity based.
//!
//! hb-052 production FP filter. Holds a set of "known-good" callsigns
//! (the reference set) and accepts decodes whose extracted callsigns
//! intersect that set. Decodes with no callsigns at all are dropped
//! (likely garbage).
//!
//! Reference set sources (combine via `extend_*` calls; the filter
//! unions them):
//! 1. **Corpus baselines** — for eval-harness validation (batch-4 MVP).
//!    See `extend_from_baselines`.
//! 2. **ADIF logs** — operator's logged QSO callsigns. Real-world
//!    "I've talked to these stations" set. See `extend_from_adif`
//!    (hb-052 iter 2).
//! 3. **Rolling window** — callsigns from recent decodes this session.
//!    Live decoder feedback loop. See `with_rolling_window`.
//! 4. **cqdx.io spots cache** — recent network-wide spotted callsigns.
//!    Future iter.
//!
//! The MVP found -21.7% novels at -0.02% recall on hard-200 with
//! source (1). This module lifts that logic into a library function
//! that production can call.

use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;

/// Extract callsigns from an FT8 decoded message. Strips CQ/CQ-modifier
/// prefixes and returns up to 2 callsign-shaped tokens. Suffixes like
/// `/R`, `/P`, `/QRP` are stripped — the base callsign is what's used
/// for reference-set membership.
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

/// Parse ADIF text and return all CALL field values. ADIF format:
/// each field is `<NAME:LENGTH>VALUE` where LENGTH is the byte count
/// of VALUE. Case-insensitive tag matching. Tolerates extra whitespace.
///
/// Example input:
///   `<CALL:5>K1ABC <BAND:3>20m <EOR>`
/// Returns: `["K1ABC"]`
pub fn parse_adif_calls(text: &str) -> Vec<String> {
    let upper = text.to_uppercase();
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = upper[search_from..].find("<CALL:") {
        let start = search_from + rel + "<CALL:".len();
        // Read decimal length up to '>'
        let rest = &upper[start..];
        let len_end = match rest.find('>') {
            Some(i) => i,
            None => break,
        };
        // The length spec may include a `:type` suffix like `<CALL:5:S>`;
        // strip on the first ':' if present.
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
        // Extract from the ORIGINAL (case-preserved) string for the value.
        let value = text[value_start..value_end].trim().to_string();
        if !value.is_empty() {
            out.push(value);
        }
        search_from = value_end;
    }
    out
}

/// FP filter state. Holds the "known-good callsigns" reference set
/// plus optional rolling-window state.
pub struct FpFilter {
    /// Static reference set (from baselines, ADIF, etc.). Built up
    /// before decoding starts and not modified.
    reference: HashSet<String>,
    /// Optional rolling-window of recently-decoded callsigns. Modified
    /// as decodes flow through `accept`. None = rolling window disabled.
    rolling: Option<Mutex<VecDeque<String>>>,
    /// Capacity of the rolling window (if enabled).
    rolling_cap: usize,
}

impl FpFilter {
    /// New empty filter. Add reference sources before using.
    pub fn new() -> Self {
        Self {
            reference: HashSet::new(),
            rolling: None,
            rolling_cap: 0,
        }
    }

    /// Add all callsigns found across jt9 baseline JSON files in a
    /// directory (one JSON per WAV, keyed by SHA, with a `decodes`
    /// array). This is the eval-harness corpus source per the
    /// batch-4 MVP. Returns the number of files loaded.
    pub fn extend_from_baselines(&mut self, baselines_dir: &Path) -> anyhow::Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir(baselines_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let s = std::fs::read_to_string(&path)?;
            let v: Value = serde_json::from_str(&s)?;
            if let Some(decodes) = v.get("decodes").and_then(|d| d.as_array()) {
                for d in decodes {
                    if let Some(m) = d.get("message").and_then(|m| m.as_str()) {
                        for cs in callsigns_in(m) {
                            self.reference.insert(cs);
                        }
                    }
                }
            }
            count += 1;
        }
        Ok(count)
    }

    /// Add callsigns from an explicit list. Useful for testing.
    pub fn extend_from_iter<I, S>(&mut self, calls: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for c in calls {
            let s = c.as_ref().to_uppercase();
            if !s.is_empty() {
                self.reference.insert(s);
            }
        }
    }

    /// Parse an ADIF file and extract all CALL (or `<CALL:N>`) values
    /// as reference callsigns. Operator's logged QSOs become the
    /// "I've talked to these stations" set. Returns the number of
    /// callsigns added.
    pub fn extend_from_adif(&mut self, path: &Path) -> anyhow::Result<usize> {
        let text = std::fs::read_to_string(path)?;
        let added_before = self.reference.len();
        for call in parse_adif_calls(&text) {
            // Strip /R, /P suffixes for matching consistency.
            let bare = call.split('/').next().unwrap_or(&call).to_uppercase();
            if !bare.is_empty() {
                self.reference.insert(bare);
            }
        }
        Ok(self.reference.len() - added_before)
    }

    /// Enable a rolling-window of size N. Each call to `accept` that
    /// returns true also pushes the decode's callsigns into the window
    /// (evicting oldest above capacity). N=0 disables.
    pub fn with_rolling_window(mut self, n: usize) -> Self {
        if n > 0 {
            self.rolling = Some(Mutex::new(VecDeque::new()));
            self.rolling_cap = n;
        }
        self
    }

    /// Size of the static reference set. Useful for diagnostics.
    pub fn reference_size(&self) -> usize {
        self.reference.len()
    }

    /// True if any of the message's extracted callsigns appear in the
    /// reference set OR the current rolling window. A message with no
    /// callsigns at all is rejected.
    ///
    /// If `update_rolling` is true and the result is true, the
    /// callsigns are also pushed into the rolling window (if enabled).
    pub fn accept(&self, message: &str, update_rolling: bool) -> bool {
        let calls = callsigns_in(message);
        if calls.is_empty() {
            return false;
        }
        // Membership check: static reference OR rolling window snapshot.
        let in_reference = calls.iter().any(|c| self.reference.contains(c));
        let in_rolling = if let Some(ref rw) = self.rolling {
            if let Ok(guard) = rw.lock() {
                calls.iter().any(|c| guard.iter().any(|q| q == c))
            } else {
                false
            }
        } else {
            false
        };
        if !(in_reference || in_rolling) {
            return false;
        }
        if update_rolling {
            if let Some(ref rw) = self.rolling {
                if let Ok(mut guard) = rw.lock() {
                    for c in &calls {
                        if !guard.iter().any(|q| q == c) {
                            guard.push_back(c.clone());
                            while guard.len() > self.rolling_cap {
                                guard.pop_front();
                            }
                        }
                    }
                }
            }
        }
        true
    }
}

impl Default for FpFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callsigns_in_cq() {
        // Note: the extractor takes up to 2 callsign-shaped tokens after
        // CQ/modifier. Grid squares like FN42 fit the loose shape filter
        // (3-10 chars, has digit, has alpha) so they're returned too.
        // This is consistent with the batch-4 MVP behavior and the same
        // grid-as-callsign rule used in cross_validate_novels.rs.
        let calls = callsigns_in("CQ K1ABC FN42");
        assert_eq!(calls, vec!["K1ABC", "FN42"]);
    }

    #[test]
    fn callsigns_in_cq_dx() {
        let calls = callsigns_in("CQ DX K1ABC FN42");
        assert_eq!(calls, vec!["K1ABC", "FN42"]);
    }

    #[test]
    fn callsigns_in_qso() {
        // Takes only the first 2 callsign-shaped tokens; EM48 is third.
        let calls = callsigns_in("K1ABC W9XYZ EM48");
        assert_eq!(calls, vec!["K1ABC", "W9XYZ"]);
    }

    #[test]
    fn callsigns_strips_slash_r() {
        let calls = callsigns_in("K1ABC W9XYZ/R EM48");
        assert_eq!(calls, vec!["K1ABC", "W9XYZ"]);
    }

    #[test]
    fn filter_no_reference_rejects_all() {
        let f = FpFilter::new();
        assert!(!f.accept("CQ K1ABC FN42", false));
    }

    #[test]
    fn filter_reference_accepts() {
        let mut f = FpFilter::new();
        f.extend_from_iter(["K1ABC"]);
        assert!(f.accept("CQ K1ABC FN42", false));
        assert!(!f.accept("CQ ZZ0ZZZ AA00", false));
    }

    #[test]
    fn filter_no_callsign_rejects() {
        let mut f = FpFilter::new();
        f.extend_from_iter(["K1ABC"]);
        // "73" + bare letters aren't callsign-shaped → dropped
        assert!(!f.accept("BLAH 73 OOPS", false));
    }

    #[test]
    fn adif_parser_extracts_calls() {
        let text = "ADIF Export by pancetta\n\
            <ADIF_VER:5>3.1.0 <PROGRAMID:8>pancetta <EOH>\n\
            <CALL:5>K1ABC <BAND:3>20M <MODE:3>FT8 <EOR>\n\
            <CALL:5>W9XYZ <BAND:3>40M <MODE:3>FT8 <EOR>\n\
            <CALL:7>DL5XYZ <BAND:3>15M <EOR>\n";
        let calls = parse_adif_calls(text);
        assert_eq!(calls, vec!["K1ABC", "W9XYZ", "DL5XYZ"]);
    }

    #[test]
    fn adif_parser_handles_typed_field() {
        // ADIF allows `<CALL:5:S>` where S is the data-type indicator.
        let text = "<CALL:5:S>K1ABC <EOR>";
        let calls = parse_adif_calls(text);
        assert_eq!(calls, vec!["K1ABC"]);
    }

    #[test]
    fn adif_parser_empty_file_returns_empty() {
        let text = "ADIF Export by pancetta\n<ADIF_VER:5>3.1.0 <EOH>\n";
        let calls = parse_adif_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn filter_extends_from_adif_text() {
        let mut f = FpFilter::new();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "<CALL:5>K1ABC <EOR>\n<CALL:5>W9XYZ <EOR>\n").unwrap();
        let n = f.extend_from_adif(tmp.path()).unwrap();
        assert_eq!(n, 2);
        assert!(f.accept("CQ K1ABC FN42", false));
        assert!(f.accept("K1ABC W9XYZ EM48", false));
        assert!(!f.accept("CQ ZZ0ZZZ AA00", false));
    }

    #[test]
    fn filter_rolling_empty_rejects() {
        // Empty reference + empty rolling → always reject.
        let f = FpFilter::new().with_rolling_window(2);
        assert!(!f.accept("CQ K1ABC FN42", true));
    }

    #[test]
    fn filter_rolling_grows_via_reference_match() {
        let mut f = FpFilter::new().with_rolling_window(4);
        f.extend_from_iter(["K1ABC"]);
        // Reference match → accept; pushes K1ABC + FN42 to rolling.
        assert!(f.accept("CQ K1ABC FN42", true));
        // W9XYZ not in reference, but K1ABC is → accept; W9XYZ added.
        assert!(f.accept("K1ABC W9XYZ EM48", true));
        // A new callsign with no other anchors should reject.
        assert!(!f.accept("ZZ0ZZZ AA0AA AA00", false));
    }
}
