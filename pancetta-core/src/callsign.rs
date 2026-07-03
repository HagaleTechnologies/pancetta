//! Compound-callsign equivalence (catalog C18) — pure string helpers shared
//! by the QSO engine and the TUI.

/// Extract the *base callsign* from a possibly-compound callsign string.
///
/// FT8 stations transmit compound callsigns in two shapes (and rarely both, as
/// in `VK9/W1XYZ/MM`):
///   - **prefix portable**: `EA8/G8BCG`, `VK9/W1XYZ`, `K1ABC/4`
///   - **suffix portable**:  `G8BCG/P`, `K1ABC/R`, `W1XYZ/MM`
///
/// The *base* is the operator's home callsign — the component that looks like a
/// full callsign: it contains at least one digit AND at least one letter and is
/// ≥3 characters long. Pure-prefix tokens (`EA8`, `VK9`) and short portable
/// suffixes (`P`, `M`, `MM`, `R`, `QRP`, a bare digit) are NOT bases.
///
/// Selection rule (catalog C18): among the `/`-separated components, choose the
/// longest one that is callsign-shaped. "Longest callsign-shaped" disambiguates
/// the two compound shapes without a country-prefix table: in `EA8/G8BCG` the
/// base `G8BCG` (5) beats the prefix `EA8` (3, and not callsign-shaped anyway);
/// in `G8BCG/P` the base `G8BCG` beats the suffix `P`. When no component is
/// callsign-shaped (e.g. the input is itself only a fragment), we fall back to
/// the longest component so the comparison stays conservative.
///
/// Returns the uppercased base. Empty input yields an empty string.
pub fn base_callsign(callsign: &str) -> String {
    let upper = callsign.trim().to_uppercase();
    if upper.is_empty() {
        return String::new();
    }

    // A component is "callsign-shaped" if it has ≥3 chars, contains a digit,
    // and contains a letter. This rejects bare digits ("4"), pure-letter
    // suffixes ("P", "MM", "QRP"), and most bare prefixes ("EA8" is 3 chars
    // with a digit+letters so it IS shaped — but a real base call alongside it
    // is always longer, so the longest-shaped rule still picks the base).
    fn is_callsign_shaped(c: &str) -> bool {
        c.len() >= 3
            && c.bytes().any(|b| b.is_ascii_digit())
            && c.bytes().any(|b| b.is_ascii_alphabetic())
    }

    let components: Vec<&str> = upper.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return upper;
    }

    // Prefer the longest callsign-shaped component (the home call). The task's
    // "if ambiguous, require the non-suffix part" reduces here to "the longer
    // component wins" — a country prefix (EA8, VK9) is shorter than the home
    // call it modifies, and a portable suffix (P/MM/R) is shorter still and not
    // callsign-shaped at all.
    if let Some(best) = components
        .iter()
        .filter(|c| is_callsign_shaped(c))
        .max_by_key(|c| c.len())
    {
        return (*best).to_string();
    }

    // No callsign-shaped component: fall back to the longest component so we
    // still compare *something* deterministic rather than the raw string.
    components
        .iter()
        .max_by_key(|c| c.len())
        .map(|c| c.to_string())
        .unwrap_or(upper)
}

/// Are two (possibly compound) callsigns the **same station**?
///
/// Catalog C18 / peer D4: a station may appear as a compound callsign
/// (`EA8/G8BCG`, `G8BCG/P`) and later in the *same* QSO as the bare base call
/// (`G8BCG`), or vice versa — it is the same operator. WSJT-X/JTDX stall in
/// this case because their sender-verification compares the displayed call
/// against the latched partner. pancetta does not: this helper treats a
/// compound call and its base as equal for the purpose of matching an
/// established QSO's partner.
///
/// Two calls match iff:
///   1. they are byte-identical after uppercasing (the common case), OR
///   2. their extracted [`base_callsign`]s are equal.
///
/// It is deliberately **conservative**: it never merges two genuinely different
/// calls. `K5ARH` vs `K5ARG`, `G8BCG` vs `G8BCH` extract distinct bases and so
/// do NOT match. The relaxation is strictly "ignore a portable prefix/suffix",
/// nothing more.
pub fn callsigns_match(a: &str, b: &str) -> bool {
    let au = a.trim().to_uppercase();
    let bu = b.trim().to_uppercase();
    if au.is_empty() || bu.is_empty() {
        // An empty call matches only another empty call; never relax an empty
        // side into matching a real station.
        return au == bu;
    }
    if au == bu {
        return true;
    }
    base_callsign(&au) == base_callsign(&bu)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- base_callsign extraction ---------------------------------------

    #[test]
    fn base_callsign_extraction_table() {
        assert_eq!(base_callsign("G8BCG"), "G8BCG");
        assert_eq!(base_callsign("G8BCG/P"), "G8BCG");
        assert_eq!(base_callsign("EA8/G8BCG"), "G8BCG");
        assert_eq!(base_callsign("VK9/W1XYZ/MM"), "W1XYZ");
        assert_eq!(base_callsign("K1ABC/R"), "K1ABC");
        assert_eq!(base_callsign("K1ABC/4"), "K1ABC"); // bare-digit reassignment suffix
        assert_eq!(base_callsign("g8bcg/p"), "G8BCG"); // case-insensitive
        assert_eq!(base_callsign("  G8BCG/P  "), "G8BCG"); // trimmed
    }

    #[test]
    fn base_callsign_empty_input_yields_empty() {
        assert_eq!(base_callsign(""), "");
        assert_eq!(base_callsign("   "), "");
    }

    // --- callsigns_match ---------------------------------------------------

    #[test]
    fn callsigns_match_identical_and_compound_equivalence() {
        let matching_pairs = [
            ("G8BCG", "G8BCG"),
            ("g8bcg", "G8BCG"),
            ("G8BCG", "EA8/G8BCG"),
            ("G8BCG", "G8BCG/P"),
            ("EA8/G8BCG", "G8BCG/P"),
            ("K1ABC", "K1ABC/4"),
            ("W1XYZ", "VK9/W1XYZ/MM"),
        ];
        for (a, b) in matching_pairs {
            assert!(
                callsigns_match(a, b),
                "expected {a} and {b} to match as same station"
            );
            assert!(callsigns_match(b, a), "callsigns_match must be symmetric");
        }
    }

    #[test]
    fn callsigns_match_rejects_genuinely_different_stations() {
        let non_matching_pairs = [
            ("K5ARH", "K5ARG"),
            ("G8BCG", "G8BCH"),
            ("EA8/G8BCG", "EA8/G8BCH"),
            ("W1ABC", "W1ABD"),
        ];
        for (a, b) in non_matching_pairs {
            assert!(
                !callsigns_match(a, b),
                "expected {a} and {b} to NOT match as same station"
            );
        }
    }

    #[test]
    fn callsigns_match_empty_string_handling() {
        assert!(!callsigns_match("", "G8BCG"));
        assert!(!callsigns_match("G8BCG", ""));
        assert!(callsigns_match("", ""));
    }
}
