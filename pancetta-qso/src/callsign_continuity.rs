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

/// True if the message contains a high-risk false-positive pattern.
///
/// Used by `accept()` as a pre-callsign-lookup reject. These patterns
/// are FT8-protocol-legal but operationally implausible in the
/// autonomous-personal-station operator profile, and they happen to be
/// attractor patterns for random LDPC convergence on noise.
///
/// Currently checks:
/// * **/R suffix** (Batch 29 hb-058: 436 FPs eliminated on full
///   hard-200 at 0% recall cost; contest rovers are out-of-profile)
/// * **degenerate Maidenhead grid** (Batch 31 Q: 12 emissions, all FP;
///   field letters outside A-R or AA/ZZ extremes are LDPC-random,
///   never real)
/// * **callsign with ≥3 consecutive digits** (Batch 31 Q: 1 emission
///   on hard-200, FP; real callsigns max out at 2 consecutive digits)
///
/// Returns true when the message should be rejected. Matches uppercase.
pub fn has_high_risk_fp_pattern(message: &str) -> bool {
    let upper = message.to_uppercase();

    // 1. /R suffix variants (hb-058 Batch 29).
    for t in upper.split_whitespace() {
        if t.ends_with("/R") || t.ends_with("/R1") || t.ends_with("/R2") {
            return true;
        }
    }

    // 2. Degenerate Maidenhead grid (Batch 31).
    for t in upper.split_whitespace() {
        if t.len() == 4 && is_grid_shape(t) {
            let chars: Vec<char> = t.chars().collect();
            let f0 = chars[0];
            let f1 = chars[1];
            // Field letters outside A-R are not a legal Maidenhead grid.
            if !('A'..='R').contains(&f0) || !('A'..='R').contains(&f1) {
                return true;
            }
            // Same letter twice at the A or Z extreme (e.g., AA00, ZZ99).
            if f0 == f1 && (f0 == 'A' || f0 == 'Z') {
                return true;
            }
        }
    }

    // 3. Callsign with 3+ consecutive digits (Batch 31).
    for t in upper.split_whitespace() {
        // Skip grids and SNR/report tokens.
        if t.len() == 4 && is_grid_shape(t) {
            continue;
        }
        if t.starts_with('-') || t.starts_with('+') || t == "73" || t == "RR73" {
            continue;
        }
        // Strip suffix for the consecutive-digit check.
        let base = t.split('/').next().unwrap_or(t);
        if has_consecutive_digit_run(base, 3) {
            return true;
        }
    }

    false
}

fn is_grid_shape(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_alphabetic()
        && chars[2].is_ascii_digit()
        && chars[3].is_ascii_digit()
}

fn has_consecutive_digit_run(s: &str, min_run: usize) -> bool {
    let mut run = 0usize;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            run += 1;
            if run >= min_run {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

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
        // hb-058 follow-on (Batch 29): reject high-risk FP patterns (/R*)
        // BEFORE the cold-start lenient gate. These are 100% FP on the audit
        // corpus regardless of trust-set state, so even cold-start operators
        // shouldn't accept them. Rover ops in contests would need a separate
        // toggle (out of scope for this iter).
        if has_high_risk_fp_pattern(message) {
            return false;
        }
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

    /// Non-mutating membership check: would `accept(message)` return true
    /// right now? Same source-of-truth as `accept` but does NOT push the
    /// message's callsigns into the rolling window. Use this from a TX
    /// decision path where you only want to consult the filter, not let
    /// the consultation expand the trust set.
    ///
    /// Single-callsign overload: pass an already-extracted uppercase
    /// callsign string. Matches the same union (static + cqdx + rolling)
    /// but skips message parsing.
    pub fn would_accept_callsign(&self, callsign: &str) -> bool {
        if self.cold_start_threshold > 0 && self.reference_size() < self.cold_start_threshold {
            return true;
        }
        let upper = callsign.to_uppercase();
        if self.static_ref.contains(&upper) {
            return true;
        }
        if let Ok(g) = self.cqdx_spotted.read() {
            if g.contains(&upper) {
                return true;
            }
        }
        if let Ok(g) = self.rolling.read() {
            if g.iter().any(|q| q == &upper) {
                return true;
            }
        }
        false
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
/// optional ADIF path + initial cqdx-spotted snapshot + optional seed
/// list + rolling-window capacity + cold-start threshold. The cqdx
/// snapshot can be empty at construction; the coordinator calls
/// `update_cqdx_spotted` periodically via the cqdx bridge.
///
/// `cold_start_threshold = 0` → strict from first decode.
/// `cold_start_threshold > 0` → lenient until reference_size() ≥ threshold.
///
/// Phase-5 hardening: the `seed` parameter supplies operator-curated
/// callsigns (typically loaded from `~/.pancetta/callsign_seed.txt`).
/// Combined with a small `cold_start_threshold` (e.g. 5), a few seed
/// entries are enough to flip the filter into strict mode immediately,
/// preventing OSD-fabricated calls (`R44XYB`, `OR1QRD`, ...) from
/// flooding the pipeline before ADIF / cqdx populate.
pub fn build_filter(
    adif_path: Option<&Path>,
    initial_cqdx_spotted: HashSet<String>,
    rolling_cap: usize,
    cold_start_threshold: usize,
) -> std::io::Result<CallsignContinuityFilter> {
    build_filter_with_seed(
        adif_path,
        initial_cqdx_spotted,
        Vec::new(),
        rolling_cap,
        cold_start_threshold,
    )
}

/// Phase-5 convenience builder accepting an explicit `seed` list of
/// operator-curated callsigns alongside ADIF + cqdx sources. Seed entries
/// are uppercased and inserted into `static_ref`, contributing to
/// `reference_size()` so a small file can flip the filter out of
/// cold-start lenient mode immediately. See [`build_filter`] for the
/// non-seed shorthand.
pub fn build_filter_with_seed(
    adif_path: Option<&Path>,
    initial_cqdx_spotted: HashSet<String>,
    seed: Vec<String>,
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
    if !seed.is_empty() {
        f.extend_from_iter(seed);
    }
    f.update_cqdx_spotted(initial_cqdx_spotted);
    Ok(f)
}

/// Parse a seed file: one uppercase callsign per line, ignoring blank
/// lines and lines starting with `#`. Returns the deduplicated set of
/// uppercase callsigns. Returns an empty vec if the path doesn't exist.
pub fn parse_seed_file(path: &Path) -> std::io::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Tolerate inline comments after the callsign.
        let token = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_uppercase();
        if token.is_empty() {
            continue;
        }
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    Ok(out)
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
    fn high_risk_fp_pattern_detects_slash_r_variants() {
        assert!(has_high_risk_fp_pattern("K1ABC/R W9XYZ FN42"));
        assert!(has_high_risk_fp_pattern("K1ABC/R1 W9XYZ FN42"));
        assert!(has_high_risk_fp_pattern("K1ABC/R2 W9XYZ FN42"));
        assert!(has_high_risk_fp_pattern("CQ K1ABC/R FN42"));
        // Lowercase tolerated
        assert!(has_high_risk_fp_pattern("k1abc/r w9xyz fn42"));
        // Not /R - other suffixes pass through
        assert!(!has_high_risk_fp_pattern("K1ABC/P W9XYZ FN42"));
        assert!(!has_high_risk_fp_pattern("K1ABC/QRP W9XYZ FN42"));
        assert!(!has_high_risk_fp_pattern("CQ K1ABC FN42"));
        // Adjacent characters that aren't /R
        assert!(!has_high_risk_fp_pattern("K1ABC R-12"));
        assert!(!has_high_risk_fp_pattern("RR73"));
    }

    #[test]
    fn slash_r_rejected_even_when_base_callsign_trusted() {
        // hb-058 Batch 29 finding: /R-suffix decodes are 100% FP on the audit
        // corpus regardless of whether the base callsign is in the trust set.
        // Pre-callsign-lookup rejection prevents the /R FP from passing via
        // trust-set inclusion of the base callsign.
        let mut f = CallsignContinuityFilter::new(100);
        f.extend_from_iter(["K1ABC", "W9XYZ"]);
        // Base callsigns are in trust set, but the /R-suffix form is rejected.
        assert!(!f.accept("K1ABC/R W9XYZ FN42"));
        // Same message without /R passes (trust-set match on the base).
        assert!(f.accept("K1ABC W9XYZ FN42"));
    }

    #[test]
    fn degenerate_grid_detected() {
        // Field letters outside A-R are not legal Maidenhead grids.
        assert!(has_high_risk_fp_pattern("K1ABC W9XYZ ZZ12"));
        assert!(has_high_risk_fp_pattern("K1ABC W9XYZ TT99"));
        assert!(has_high_risk_fp_pattern("k1abc w9xyz xz45"));
        // Same letter twice at extremes.
        assert!(has_high_risk_fp_pattern("K1ABC W9XYZ AA00"));
        assert!(has_high_risk_fp_pattern("K1ABC W9XYZ ZZ99"));
        // Legal grids pass through (we reject the message at the trust-set
        // layer if the callsigns don't match).
        assert!(!has_high_risk_fp_pattern("CQ K1ABC FN42"));
        assert!(!has_high_risk_fp_pattern("CQ K1ABC EM10"));
        assert!(!has_high_risk_fp_pattern("CQ K1ABC PM85")); // Japan
        assert!(!has_high_risk_fp_pattern("CQ K1ABC RR73")); // RR73 is exchange, not grid
    }

    #[test]
    fn digit_run_callsign_detected() {
        // 3+ consecutive digits in a callsign-shaped token is not real.
        assert!(has_high_risk_fp_pattern("K123 W9XYZ FN42"));
        assert!(has_high_risk_fp_pattern("K1ABC AB9876 FN42"));
        // 2 consecutive digits is fine (KA1AB, W9XYZ patterns).
        assert!(!has_high_risk_fp_pattern("KA12B W9XYZ FN42"));
        assert!(!has_high_risk_fp_pattern("CQ K1ABC FN42"));
        // SNR/grid tokens with digits are not callsigns.
        assert!(!has_high_risk_fp_pattern("K1ABC W9XYZ -12"));
        assert!(!has_high_risk_fp_pattern("K1ABC W9XYZ FN42"));
    }

    #[test]
    fn slash_r_rejected_even_in_cold_start() {
        // The /R reject runs BEFORE the cold-start lenient gate; even a
        // brand-new operator log doesn't open the floodgates to /R FPs.
        let f = CallsignContinuityFilter::new_lenient(100, 50);
        // In cold-start, normal messages are accepted.
        assert!(f.accept("CQ K1ABC FN42"));
        // But /R is rejected even pre-threshold.
        assert!(!f.accept("CQ K1ABC/R FN42"));
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
        // Real-looking but untrusted → reject (no anchor in static or rolling yet).
        assert!(!f.accept("K7ZZX KC4XYZ EM10"));
    }

    #[test]
    fn lenient_cold_start_passes_until_threshold() {
        let f = CallsignContinuityFilter::new_lenient(100, 5);
        // No static, no cqdx → reference_size()=0 < 5 → lenient → accept
        // real-looking-but-untrusted messages. (Note: degenerate-grid /
        // /R-suffix / digit-run patterns are rejected EVEN in lenient
        // cold-start because they're definitionally noise-emergent FPs.)
        assert!(f.accept("CQ K7ZZX EM10"));
        assert!(f.accept("ANY GARBAGE WITH NO CALLSIGN"));
    }

    #[test]
    fn lenient_activates_when_reference_grows() {
        let mut f = CallsignContinuityFilter::new_lenient(100, 3);
        // Lenient mode initially (real-looking-but-untrusted goes through).
        assert!(f.accept("K7ZZX W9XYZ EM10"));
        // Add 3 static callsigns → threshold met → strict mode.
        f.extend_from_iter(["K1ABC", "W9XYZ", "DL5XYZ"]);
        assert_eq!(f.reference_size(), 3);
        assert!(f.accept("CQ K1ABC FN42"));
        // Use a fresh callsign not in static/cqdx/rolling → reject in strict.
        assert!(!f.accept("CQ KC4QQQ EM10"));
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
        assert!(!f.accept("CQ K7ZZX EM10"));
    }

    #[test]
    fn build_filter_lenient_mode() {
        let f = build_filter(None, HashSet::new(), 100, 5).unwrap();
        // Lenient: reference empty → accept real-looking unknown
        assert!(f.accept("CQ K7ZZX EM10"));
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
    fn seed_flips_filter_out_of_cold_start() {
        // Phase-5 hardening: with a small threshold (5) and a 3-entry
        // seed, an unknown callsign like the OSD-fabricated `R44XYB`
        // observed in the 2026-05-30 live capture must be REJECTED.
        // Threshold met case: bump the seed to 5 so the filter is strict.
        let f = build_filter_with_seed(
            None,
            HashSet::new(),
            vec![
                "K5ARH".to_string(),
                "W1ABC".to_string(),
                "WB9KMW".to_string(),
                "JA1XYZ".to_string(),
                "DL5XYZ".to_string(),
            ],
            100,
            5,
        )
        .unwrap();
        assert_eq!(f.reference_size(), 5);
        // The 5 seed entries hit the threshold → strict mode active.
        assert!(!f.accept("CQ R44XYB FN42"));
        assert!(f.accept("CQ K5ARH EM12"));
    }

    #[test]
    fn would_accept_callsign_does_not_mutate_rolling() {
        let mut f = CallsignContinuityFilter::new(100);
        f.extend_from_iter(["K1ABC"]);
        // Check an unknown callsign: must NOT add it to rolling.
        assert!(!f.would_accept_callsign("ZZ0ZZZ"));
        // Subsequent accept of a message containing ZZ0ZZZ — still
        // rejected, proving the prior would_accept call didn't taint
        // the rolling window.
        assert!(!f.accept("ZZ0ZZZ AA00 -10"));
    }

    #[test]
    fn would_accept_callsign_matches_accept_for_known() {
        let mut f = CallsignContinuityFilter::new(100);
        f.extend_from_iter(["K1ABC", "W9XYZ"]);
        assert!(f.would_accept_callsign("K1ABC"));
        assert!(f.would_accept_callsign("k1abc")); // case-insensitive
        assert!(!f.would_accept_callsign("UNKNOWN"));
    }

    #[test]
    fn parse_seed_file_handles_comments_and_blanks() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "# operator seed list\nK5ARH\n\n  W1ABC  # nearby\nwb9kmw\n# trailing comment"
        )
        .unwrap();
        let calls = parse_seed_file(tmp.path()).unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.contains(&"K5ARH".to_string()));
        assert!(calls.contains(&"W1ABC".to_string()));
        assert!(calls.contains(&"WB9KMW".to_string()));
    }

    #[test]
    fn parse_seed_file_missing_returns_empty() {
        let nonexistent = std::path::PathBuf::from("/tmp/seed-does-not-exist-zzz.txt");
        let calls = parse_seed_file(&nonexistent).unwrap();
        assert!(calls.is_empty());
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
