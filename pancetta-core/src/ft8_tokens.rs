//! Shared FT8 token vocabulary (PAN-330).
//!
//! One owner for the token-level policies that multiple crates previously
//! copied: whether a payload token is a *committed exchange* (report /
//! RR73 / RRR / 73 / RR), and the two *named* CQ-modifier policies —
//! what the encoder can pack versus what a decoded free-text CQ line may
//! carry. Recognition is deliberately separate from encodability: `12` is a
//! decoded CQ-zone modifier but is not directly packable, while `ABCD` packs
//! but is not a decoded whitelist token. Both facts are pinned by tests
//! below.

/// Is `tok` the payload of a committed exchange — the trailing token of a
/// standard FT8 exchange that commits or acknowledges a contact?
///
/// Recognizes (case-insensitively) `RR73`, `RRR`, `73`, `RR`, and signed
/// digit reports (`-12`, `+05`) with an optional leading `R` (`R-12`).
/// Grids, unsigned numbers, empty signs (`-`), and trailing junk are
/// rejected.
///
/// Moved from `pancetta_qso::autonomous::is_exchange_payload` (which
/// re-exports it) so the TUI's busy detector and the QSO engine share one
/// definition instead of verbatim copies (PAN-330).
pub fn is_exchange_payload(tok: &str) -> bool {
    let u = tok.to_uppercase();
    if u == "RR73" || u == "RRR" || u == "73" || u == "RR" {
        return true;
    }
    // Signal reports: "-12", "+05", "R-12", "R+05".
    let body = u.strip_prefix('R').unwrap_or(&u);
    if let Some(rest) = body.strip_prefix(['-', '+']) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Can the FT8 *encoder* pack `token` as a CQ modifier?
///
/// The packable policy (unchanged from `pancetta_ft8::encoder`'s private
/// `is_cq_modifier`): a nonempty token that is either one to four ASCII
/// uppercase letters (`DX`, `QRP`, `ABCD`) or exactly three ASCII digits
/// (`123`). Packing constraints are a property of the FT8 message format —
/// this says nothing about what a *decoded* line may carry; see
/// [`is_decoded_cq_modifier`].
pub fn is_packable_cq_modifier(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes.len() <= 4 && bytes.iter().all(|b| b.is_ascii_uppercase()) {
        return true;
    }
    bytes.len() == 3 && bytes.iter().all(|b| b.is_ascii_digit())
}

/// Is `token` a CQ modifier as it may appear in *decoded* free text?
///
/// The decoded policy (unchanged from the FT8 message validator's private
/// `is_valid_cq_modifier`, minus absence): a named whitelist seen in real
/// on-air traffic (`DX`, `NA`, `SA`, `EU`, `AS`, `AF`, `OC`, `QRP`, `POTA`,
/// `SOTA`, `FD`, `RU`, `TEST` — compared after ASCII case normalization),
/// or a short generic token of one to three ASCII letters or one to three
/// ASCII digits. An *empty* input is not a recognized token: modifier
/// absence stays a caller decision.
pub fn is_decoded_cq_modifier(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let upper: String = token.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "DX" | "NA"
            | "SA"
            | "EU"
            | "AS"
            | "AF"
            | "OC"
            | "QRP"
            | "POTA"
            | "SOTA"
            | "FD"
            | "RU"
            | "TEST"
    ) {
        return true;
    }
    let len = token.chars().count();
    if !(1..=4).contains(&len) {
        return false;
    }
    if len <= 3 && token.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    len <= 3 && token.chars().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_payload_table() {
        // Committed payloads (case-insensitive tokens and reports, optional R).
        for ok in [
            "RR73", "rr73", "RRR", "73", "RR", "-12", "+05", "R-12", "R+05", "R+5", "-0", "+00",
        ] {
            assert!(is_exchange_payload(ok), "expected committed: {ok:?}");
        }
        // Grids, unsigned numbers, empty signs, trailing junk, empty.
        for bad in [
            "FN42", "fn42", "RR7", "R", "", " ", "-", "+", "R+", "-12 ", "-1a", "12", "05",
            "R--12", "73x", "RR73RR73",
        ] {
            assert!(!is_exchange_payload(bad), "expected rejected: {bad:?}");
        }
    }

    #[test]
    fn cq_modifier_policy_table() {
        // Packable policy: 1-4 uppercase letters or exactly 3 digits.
        for ok in ["DX", "QRP", "NA", "ABCD", "A", "123"] {
            assert!(is_packable_cq_modifier(ok), "expected packable: {ok:?}");
        }
        for bad in [
            "", "dx", "Dx", "ABCDE", "12", "1234", "1", "AB1", "D-X", "DX ", "123 ", "Δ",
        ] {
            assert!(
                !is_packable_cq_modifier(bad),
                "expected unpackable: {bad:?}"
            );
        }
        // Decoded policy: named whitelist + 1-3 letters/digits, case-folded.
        for ok in [
            "DX", "dx", "Dx", "NA", "QRP", "POTA", "SOTA", "FD", "RU", "TEST", "AB", "A", "12",
            "123", "eu",
        ] {
            assert!(is_decoded_cq_modifier(ok), "expected decoded: {ok:?}");
        }
        for bad in ["", "ABCDE", "1234", "AB1", "AB CD", "POTAA", "Δ", "D-X"] {
            assert!(!is_decoded_cq_modifier(bad), "expected rejected: {bad:?}");
        }
        // The two policies deliberately diverge (PAN-330 decision 1):
        // `12` decodes but is not packable; `ABCD` packs but is not a
        // decoded whitelist/generic token.
        assert!(is_decoded_cq_modifier("12") && !is_packable_cq_modifier("12"));
        assert!(is_packable_cq_modifier("ABCD") && !is_decoded_cq_modifier("ABCD"));
    }

    #[test]
    fn callsign_token_table() {
        use crate::callsign::is_callsign_token;
        for ok in [
            "K5ARH",
            "DL1ABC",
            "EA8/K5ARH",
            "K5ARH/P",
            "VP2E/W5AU/P",
            "4X1RF",
            "k5arh",
            "FN42ab", // 6-char grid-shaped token: not a bare 4-char grid; the
                      // caller's grid/report disambiguation owns rejecting it
        ] {
            assert!(is_callsign_token(ok), "expected token: {ok:?}");
        }
        for bad in [
            "",
            "K5",           // too short (< 3)
            "123456789012", // too long (> 11)
            "FN42",         // bare 4-char grid shape is not a callsign
            "-12",          // report: no letter
            "R-12",
            "DX", // no digit
            "QRP",
            "73",
            "W1!",      // charset outside ASCII alphanumeric + '/'
            "W1 ",      // interior space
            "/K1ABC",   // empty leading slash component
            "K1ABC/",   // empty trailing slash component
            "K1ABC//P", // empty interior slash component
            "Δ5ARH",    // non-ASCII
        ] {
            assert!(!is_callsign_token(bad), "expected rejected: {bad:?}");
        }
    }
}
