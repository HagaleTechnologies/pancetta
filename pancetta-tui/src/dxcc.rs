//! Callsign → DXCC entity-name resolver.
//!
//! The DX Hunter shows a DXCC "Entity" column. cqdx live spots already carry an
//! `entity_name`, but LOCAL decodes (the bulk of what the operator sees) have
//! none, so they rendered as "---". This module provides a self-contained,
//! offline resolver covering all ~346 DXCC entities so a local decode still
//! shows its country.
//!
//! The prefix table (`dxcc_table::PREFIX_TABLE`) is auto-generated from the
//! AD1C BigCTY `cty.dat` file (see `pancetta-tui/scripts/gen_dxcc_table.py`).
//! Resolution uses a longest-leading-prefix match, which correctly handles
//! portable-PREFIX compounds (e.g. "EA8/G8BCG" → "EA8" → Canary Islands)
//! and ignores trailing "/P", "/MM" suffixes. Unmatched prefixes return `None`
//! (the column shows "---") — we never emit a GUESS that could mislabel a
//! station. cqdx's authoritative `entity_name` always takes precedence when
//! present.

use crate::dxcc_table::PREFIX_TABLE;

/// Resolve a callsign to a DXCC entity name, best-effort. Returns `None` for an
/// unrecognized prefix (caller renders "---" rather than a guess).
///
/// The lookup is a longest-leading-prefix match over the authoritative
/// `PREFIX_TABLE` (generated from AD1C BigCTY cty.dat). The "K"/"W"/"N" and
/// "AA"–"AL" US prefixes are covered by the table; the explicit fallback below
/// is kept as a safety net for any sparse US callsign patterns not listed in
/// cty.dat (e.g. rarely-allocated K/W/N blocks).
pub fn entity_for_callsign(call: &str) -> Option<&'static str> {
    let c = call.trim().to_uppercase();
    if c.is_empty() {
        return None;
    }

    // Longest leading-prefix match over the authoritative table. The leading
    // prefix also correctly handles portable-PREFIX compounds
    // (e.g. "EA8/G8BCG" → "EA8" → Canary Islands) and ignores trailing "/P",
    // "/MM" suffixes.
    let mut best: Option<(&str, &str)> = None;
    for &(pfx, name) in PREFIX_TABLE {
        if c.starts_with(pfx) && best.is_none_or(|(b, _)| pfx.len() > b.len()) {
            best = Some((pfx, name));
        }
    }
    if let Some((_, name)) = best {
        return Some(name);
    }

    // US safety-net fallback: K/W/N anything, or A followed by A–L (the
    // AA–AL US block). The cty.dat-derived table already contains entries for
    // "K", "W", "N", "AA"–"AL", so this branch is normally unreachable —
    // it exists as a belt-and-suspenders guard for any K/W/N pattern that
    // slipped through.
    let b = c.as_bytes();
    match b.first() {
        Some(b'K') | Some(b'W') | Some(b'N') => return Some("United States"),
        Some(b'A') => {
            if let Some(&second) = b.get(1) {
                if (b'A'..=b'L').contains(&second) {
                    return Some("United States");
                }
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_entities_resolve() {
        assert_eq!(entity_for_callsign("K5ARH"), Some("United States"));
        assert_eq!(entity_for_callsign("W1AW"), Some("United States"));
        assert_eq!(entity_for_callsign("N5GES"), Some("United States"));
        assert_eq!(entity_for_callsign("AA7BQ"), Some("United States"));
        assert_eq!(entity_for_callsign("G8KHF"), Some("England"));
        assert_eq!(entity_for_callsign("DL1ABC"), Some("Fed. Rep. of Germany"));
        assert_eq!(entity_for_callsign("JA1XYZ"), Some("Japan"));
        assert_eq!(entity_for_callsign("VK3ABC"), Some("Australia"));
        assert_eq!(entity_for_callsign("F5ABC"), Some("France"));
        assert_eq!(entity_for_callsign("EA4XYZ"), Some("Spain"));
    }

    #[test]
    fn longest_prefix_wins_over_us_and_broad() {
        // US sub-entities beat the bare K/W/N fallback.
        assert_eq!(entity_for_callsign("KH6XYZ"), Some("Hawaii"));
        assert_eq!(entity_for_callsign("KL7ABC"), Some("Alaska"));
        assert_eq!(entity_for_callsign("KP4XX"), Some("Puerto Rico"));
        // Spain sub-entities beat bare EA.
        assert_eq!(entity_for_callsign("EA8ABC"), Some("Canary Islands"));
        assert_eq!(entity_for_callsign("EA6XYZ"), Some("Balearic Islands"));
        // Scotland (GM) beats England (G).
        assert_eq!(entity_for_callsign("GM4ABC"), Some("Scotland"));
        assert_eq!(entity_for_callsign("MW0ABC"), Some("Wales"));
    }

    #[test]
    fn compound_and_portable_handled() {
        // Portable PREFIX → entity of the prefix.
        assert_eq!(entity_for_callsign("EA8/G8BCG"), Some("Canary Islands"));
        // Portable SUFFIX ignored (leading prefix wins).
        assert_eq!(entity_for_callsign("G8BCG/P"), Some("England"));
    }

    #[test]
    fn a_prefix_disambiguation() {
        // A + letter A–L = US; A + digit = a specific country.
        assert_eq!(entity_for_callsign("AL7XYZ"), Some("Alaska")); // table AL beats US-A
        assert_eq!(entity_for_callsign("A4XYZ"), Some("Oman"));
        assert_eq!(entity_for_callsign("A6ABC"), Some("United Arab Emirates"));
        assert_eq!(entity_for_callsign("AC2XYZ"), Some("United States"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(entity_for_callsign(""), None);
        // A fictional/unassigned-looking prefix not in the table.
        assert_eq!(entity_for_callsign("QZ9ZZ"), None);
    }

    /// Previously-blank DXCC entities that motivated this fix.
    #[test]
    fn previously_missing_entities_now_resolve() {
        // Angola — was blank with the hand-curated table.
        assert_eq!(entity_for_callsign("D2UY"), Some("Angola"));
        // Australia secondary block (VJ) — was blank.
        assert_eq!(entity_for_callsign("VJ6X"), Some("Australia"));
    }

    #[test]
    fn d4_cape_verde_resolves() {
        assert_eq!(entity_for_callsign("D4VHF"), Some("Cape Verde"));
    }
}
