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

    // PAN-17 round 2 (Codex review #248, finding 2): an i3=4 nonstandard-
    // callsign message represents ONE of its two callsigns as a resolved
    // 12-bit hash render, "<K5ARH>" (see `Ft8Message`'s Display impl,
    // pancetta-ft8/src/message.rs) — resolve it and compare like any other
    // callsign. The UNRESOLVED hash-miss placeholder "<...>" carries no
    // identity information at all (any station's hash could have produced
    // it): it must never match anything, including another "<...>" —
    // treating "unknown" as "definitely this station" would let unrelated
    // traffic spoof as whichever partner/our-call is being checked.
    let (Some(au), Some(bu)) = (resolve_hash_render(&au), resolve_hash_render(&bu)) else {
        return false;
    };

    if au == bu {
        return true;
    }
    base_callsign(au) == base_callsign(bu)
}

/// Resolve an i3=4 hash-render token to the plain callsign it represents,
/// for [`callsigns_match`]. Returns the input unchanged for a normal
/// (non-bracketed) callsign, `Some(inner)` for a resolved render
/// (`"<K5ARH>"` -> `"K5ARH"`), and `None` for the unresolved hash-miss
/// placeholder `"<...>"` (or any other bracketed form that isn't a real
/// resolved callsign) — callers must treat `None` as "cannot match".
fn resolve_hash_render(call: &str) -> Option<&str> {
    match call.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        Some(inner) if inner.is_empty() || inner.bytes().all(|b| b == b'.') => None,
        Some(inner) => Some(inner),
        None => Some(call),
    }
}

/// Is `t` shaped like a bare 4-character Maidenhead grid square (two field
/// letters, two square digits — e.g. `FN42`)? Promoted from
/// `pancetta_qso::callsign_continuity`'s identical private check so both
/// crates share one definition (PAN-54).
pub fn is_grid_shape(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    chars.len() == 4
        && chars[0].is_ascii_alphabetic()
        && chars[1].is_ascii_alphabetic()
        && chars[2].is_ascii_digit()
        && chars[3].is_ascii_digit()
}

/// Is `callsign` structurally plausible as a real amateur-radio callsign,
/// as opposed to decoder noise or a placeholder token?
///
/// This is a SHAPE check, not a semantic one — it cannot and does not
/// detect a well-formed decode that happens to be a false positive (that
/// discrimination is `pancetta_qso::content_score`'s job). It exists so a
/// decode that cannot possibly BE a real callsign never outranks a genuine
/// station in `pancetta_qso::priority`'s scoring, regardless of what a
/// coincidental DXCC-prefix/rarity lookup says about it (PAN-54).
///
/// Rejects: empty/whitespace-only input, the unresolved AP-hash placeholder
/// `"<...>"`, anything under 3 or over 11 characters (once hash-resolved —
/// 11 matches `pancetta-ft8`'s i3=4 nonstandard-callsign field limit, see
/// `looks_like_nonstandard_callsign` in `pancetta-ft8/src/message.rs`),
/// anything without at least one digit AND one letter, any character
/// outside the FT8 callsign charset (ASCII alphanumeric plus `/` — PAN-54
/// round 1, Codex #3910471929: `"W1!"`/`"W1---"` previously passed since
/// only digit+letter *presence* was checked, not every character), a bare
/// 4-char Maidenhead grid square mistaken for a callsign, and free-text-
/// shaped garbage like `"ABC1D"` (PAN-54 round 2, Codex #3910544291): a
/// real callsign's digit run always has a plausible prefix (at most 2
/// letters, or none for a digit-led form) before it and at least one
/// suffix letter after it. Applied to [`base_callsign`]'s extraction
/// rather than the raw input, so a compound form (`"K1ABC/P"`,
/// `"VP2E/W5AU/P"`) is validated on its actual home-call component instead
/// of being exempted wholesale — PAN-54 round 3 (Codex #3910624281) found
/// the first cut of this check skipped compound tokens entirely, so
/// appending any portable suffix to implausible garbage (`"ABC1D/P"`)
/// bypassed it outright.
pub fn is_plausible_callsign(callsign: &str) -> bool {
    let upper = callsign.trim().to_uppercase();
    let Some(resolved) = resolve_hash_render(&upper) else {
        return false;
    };
    let len = resolved.len();
    if !(3..=11).contains(&len) {
        return false;
    }
    if is_grid_shape(resolved) {
        return false;
    }
    if !resolved
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'/')
    {
        return false;
    }
    let has_digit = resolved.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = resolved.bytes().any(|b| b.is_ascii_alphabetic());
    if !(has_digit && has_alpha) {
        return false;
    }
    if !has_plausible_callsign_shape(base_callsign(resolved).as_bytes()) {
        return false;
    }
    true
}

/// Digit-run positional shape check for a bare token (called on
/// [`base_callsign`]'s output, so `is_plausible_callsign` gets this for
/// compound forms too): the digit run must be flanked by a plausible
/// prefix (at most 2 letters, or none for a digit-led form) and at least
/// one suffix letter after the last digit. Ported from
/// `pancetta_ft8::message::looks_like_compound_callsign_shape` (see
/// `is_plausible_callsign`'s doc for why this is a deliberate, documented
/// duplication rather than a shared call).
fn has_plausible_callsign_shape(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }
    let Some(last_digit_pos) = bytes.iter().rposition(u8::is_ascii_digit) else {
        return false;
    };
    if last_digit_pos + 1 >= bytes.len()
        || !bytes[last_digit_pos + 1..]
            .iter()
            .all(u8::is_ascii_alphabetic)
    {
        return false;
    }
    let first_digit_pos = bytes.iter().position(u8::is_ascii_digit).unwrap();
    let prefix = &bytes[..first_digit_pos];
    if prefix.is_empty() {
        return first_digit_pos + 1 < bytes.len()
            && bytes[first_digit_pos + 1].is_ascii_alphabetic();
    }
    prefix.len() <= 2 && prefix.iter().all(u8::is_ascii_alphabetic)
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

    // --- is_grid_shape / is_plausible_callsign ------------------------------

    #[test]
    fn is_grid_shape_matches_maidenhead_field_square() {
        assert!(is_grid_shape("FN42"));
        assert!(is_grid_shape("PM95"));
        assert!(!is_grid_shape("FN4")); // too short
        assert!(!is_grid_shape("FN42A")); // too long (6-char grid, not 4)
        assert!(!is_grid_shape("W1ABC")); // not grid-shaped at all
        assert!(!is_grid_shape("44NN")); // digits/letters swapped
    }

    #[test]
    fn is_plausible_callsign_accepts_real_shapes() {
        assert!(is_plausible_callsign("W5AU"));
        assert!(is_plausible_callsign("g8bcg")); // case-insensitive
        assert!(is_plausible_callsign("  K1ABC/P  ")); // trimmed, portable suffix
        assert!(is_plausible_callsign("<W5AU>")); // resolved AP-hash render
    }

    #[test]
    fn is_plausible_callsign_accepts_max_length_nonstandard_compound() {
        assert!(is_plausible_callsign("VP2E/W5AU/P")); // 11 chars, matches pancetta-ft8's 3..=11 nonstandard-callsign limit
    }

    #[test]
    fn is_plausible_callsign_rejects_placeholder_and_noise() {
        assert!(!is_plausible_callsign("")); // empty
        assert!(!is_plausible_callsign("<...>")); // unresolved AP-hash placeholder
        assert!(!is_plausible_callsign("FN42")); // grid square, not a callsign
        assert!(!is_plausible_callsign("K")); // too short / no digit
        assert!(!is_plausible_callsign("12345")); // no letters
    }

    #[test]
    fn is_plausible_callsign_rejects_non_charset_characters() {
        // PAN-54 round 1 (Codex #3910471929): digit+letter presence alone
        // isn't enough — every character must be in the FT8 callsign
        // charset (ASCII alphanumeric plus '/').
        assert!(!is_plausible_callsign("W1!"));
        assert!(!is_plausible_callsign("W1---"));
        assert!(!is_plausible_callsign("W1 ABC"));
        assert!(!is_plausible_callsign("W1@ABC"));
    }

    #[test]
    fn is_plausible_callsign_rejects_free_text_shaped_single_tokens() {
        // PAN-54 round 2 (Codex #3910544291): digit+letter presence alone
        // isn't enough for a single (non-compound) token either — the
        // digit run needs a plausible prefix/suffix shape.
        assert!(!is_plausible_callsign("ABC1D"));
        assert!(!is_plausible_callsign("EFG2H"));
    }

    #[test]
    fn is_plausible_callsign_rejects_free_text_shaped_compound_tokens() {
        // PAN-54 round 3 (Codex #3910624281): appending a portable suffix
        // must not bypass the positional shape check — it must apply to
        // the extracted home-call component, not be skipped wholesale for
        // any '/'-containing token.
        assert!(!is_plausible_callsign("ABC1D/P"));
    }

    #[test]
    fn is_plausible_callsign_accepts_real_single_token_shapes() {
        // Real callsigns of various prefix shapes must still pass the new
        // positional check.
        assert!(is_plausible_callsign("W5AU")); // 1-letter prefix
        assert!(is_plausible_callsign("PA3ABC")); // 2-letter prefix
        assert!(is_plausible_callsign("8G81PA")); // digit-led prefix
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

    // --- PAN-17 round 2: i3=4 hash-render matching -----------------------

    #[test]
    fn callsigns_match_resolved_hash_render() {
        // "<K5ARH>" is how Ft8Message renders a RESOLVED 12-bit hash --
        // exactly as trustworthy as any other decoded callsign.
        assert!(callsigns_match("<K5ARH>", "K5ARH"));
        assert!(callsigns_match("K5ARH", "<K5ARH>"), "must be symmetric");
        assert!(callsigns_match("<k5arh>", "K5ARH"), "case-insensitive");
        // Compound equivalence still applies through the resolved render.
        assert!(callsigns_match("<G8BCG>", "EA8/G8BCG"));
    }

    #[test]
    fn callsigns_match_rejects_unresolved_hash_placeholder() {
        // "<...>" carries no identity information -- must never match
        // anything, including a real callsign, our own callsign, or
        // another "<...>".
        assert!(!callsigns_match("<...>", "K5ARH"));
        assert!(!callsigns_match("K5ARH", "<...>"));
        assert!(!callsigns_match("<...>", "<...>"));
    }

    #[test]
    fn callsigns_match_hash_render_rejects_wrong_station() {
        assert!(!callsigns_match("<K5ARH>", "K5ARG"));
    }
}
