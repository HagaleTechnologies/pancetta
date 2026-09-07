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
    let trimmed = call.trim().to_uppercase();
    if trimmed.is_empty() {
        return None;
    }
    // PAN-58: a decode heard via FT8's i3=4 hash-render comes through as
    // "<N7RLK>" (see `pancetta_core::callsign::resolve_hash_render`'s doc).
    // Resolve to the plain callsign it represents before prefix-matching —
    // otherwise the leading '<' never matches any PREFIX_TABLE entry and
    // every hash-rendered decode's Entity column reads blank ("---"). The
    // unresolved hash-miss placeholder "<...>" carries no identity at all
    // and correctly still resolves to `None`.
    let c = pancetta_core::callsign::resolve_hash_render(&trimmed)?;

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

/// The exact DXCC entity-name strings in `dxcc_table::PREFIX_TABLE` that are
/// US states/territories. Matching is EXACT, never substring: the same table
/// also contains "British Virgin Islands", independent "Samoa" (5W) and
/// "Botswana", each of which contains a substring of a name below.
/// PAN-85. Pinned against the generated table by a unit test.
pub const US_RELATED_ENTITIES: &[&str] = &[
    "Alaska",
    "American Samoa",
    "Baker & Howland Islands",
    "Desecheo Island",
    "Guam",
    "Guantanamo Bay",
    "Hawaii",
    "Johnston Island",
    "Mariana Islands",
    "Midway Island",
    "Navassa Island",
    "Palmyra & Jarvis Islands",
    "Puerto Rico",
    "US Virgin Islands",
    "United States",
    "Wake Island",
];

/// Valid US primary administrative subdivisions (ADIF `STATE`), plus the
/// territory codes for the US-related DXCC entities above. Validation only —
/// this maps nothing and resolves nothing; PAN-85 forbids inferring a state.
const US_STATE_CODES: &[&str] = &[
    "AK", "AL", "AR", "AS", "AZ", "CA", "CO", "CT", "DC", "DE", "FL", "GA", "GU", "HI", "IA", "ID",
    "IL", "IN", "KS", "KY", "LA", "MA", "MD", "ME", "MI", "MN", "MO", "MP", "MS", "MT", "NC", "ND",
    "NE", "NH", "NJ", "NM", "NV", "NY", "OH", "OK", "OR", "PA", "PR", "RI", "SC", "SD", "TN", "TX",
    "UT", "VA", "VI", "VT", "WA", "WI", "WV", "WY",
];

/// `true` when `entity` is one of the US states/territories DXCC splits out.
pub fn is_us_related_entity(entity: &str) -> bool {
    US_RELATED_ENTITIES.contains(&entity)
}

/// Trim + uppercase `raw` and return the canonical code iff it is a known US
/// subdivision. `None` for anything else — an unrecognized value renders as
/// the entity alone, never as a placeholder (PAN-85 scenario 2).
pub fn normalize_us_state(raw: &str) -> Option<&'static str> {
    let t = raw.trim();
    if t.len() != 2 || !t.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let upper = t.to_ascii_uppercase();
    US_STATE_CODES.iter().copied().find(|c| *c == upper)
}

/// Render an entity for display, appending `" - ST"` iff the entity is
/// US-related AND `state` validates. Every other case returns the entity
/// unchanged — no dash, no placeholder.
pub fn format_entity_with_state(entity: &str, state: Option<&str>) -> String {
    match state
        .filter(|_| is_us_related_entity(entity))
        .and_then(normalize_us_state)
    {
        Some(code) => format!("{entity} - {code}"),
        None => entity.to_string(),
    }
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

    /// PAN-58: a resolved i3=4 hash-render (`"<N7RLK>"`) is a real,
    /// identifiable callsign (see `pancetta_core::callsign::resolve_hash_render`)
    /// and must resolve to the same entity as its plain form — the leading
    /// `'<'` previously defeated the prefix-table match entirely, leaving
    /// the DX Hunter's Entity column blank ("---") for any station heard
    /// via a hash-rendered decode.
    #[test]
    fn resolved_hash_render_resolves_same_entity_as_plain_callsign() {
        assert_eq!(entity_for_callsign("<N7RLK>"), entity_for_callsign("N7RLK"));
        assert_eq!(entity_for_callsign("<N7RLK>"), Some("United States"));
        assert_eq!(entity_for_callsign("<JA1XYZ>"), Some("Japan"));
    }

    /// The unresolved hash-miss placeholder carries no identity at all —
    /// must never resolve to an entity.
    #[test]
    fn unresolved_hash_placeholder_has_no_entity() {
        assert_eq!(entity_for_callsign("<...>"), None);
    }

    #[test]
    fn us_related_entities_match_exactly() {
        for e in [
            "United States",
            "Alaska",
            "Hawaii",
            "Puerto Rico",
            "US Virgin Islands",
            "Guantanamo Bay",
            "Navassa Island",
            "Desecheo Island",
            "Baker & Howland Islands",
            "American Samoa",
            "Wake Island",
            "Guam",
            "Midway Island",
            "Johnston Island",
            "Palmyra & Jarvis Islands",
            "Mariana Islands",
        ] {
            assert!(is_us_related_entity(e), "{e} should be US-related");
        }
    }

    #[test]
    fn lookalike_entities_are_not_us_related() {
        // Substring traps present in the real generated table.
        for e in [
            "British Virgin Islands",
            "Samoa",
            "Botswana",
            "Japan",
            "United Kingdom",
            "",
        ] {
            assert!(!is_us_related_entity(e), "{e} must not be US-related");
        }
    }

    #[test]
    fn every_whitelisted_name_exists_in_the_generated_table() {
        // Guard: a cty.dat regeneration that renames an entity must fail here,
        // not silently stop suffixing states.
        for name in US_RELATED_ENTITIES {
            assert!(
                crate::dxcc_table::PREFIX_TABLE
                    .iter()
                    .any(|(_, n)| n == name),
                "{name} is no longer in the generated PREFIX_TABLE"
            );
        }
    }

    #[test]
    fn state_codes_normalize_and_validate() {
        assert_eq!(normalize_us_state("ar"), Some("AR"));
        assert_eq!(normalize_us_state("  AR "), Some("AR"));
        assert_eq!(normalize_us_state("PR"), Some("PR"));
        assert_eq!(normalize_us_state("Arkansas"), None); // not a 2-letter code
        assert_eq!(normalize_us_state("ZZ"), None); // not a real subdivision
        assert_eq!(normalize_us_state("A"), None);
        assert_eq!(normalize_us_state(""), None);
        assert_eq!(normalize_us_state("A1"), None);
    }

    // Gherkin scenario 1
    #[test]
    fn us_entity_with_known_state_gets_suffix() {
        assert_eq!(
            format_entity_with_state("United States", Some("AR")),
            "United States - AR"
        );
        assert_eq!(
            format_entity_with_state("United States", Some("ar")),
            "United States - AR"
        );
        assert_eq!(
            format_entity_with_state("Alaska", Some("AK")),
            "Alaska - AK"
        );
        assert_eq!(
            format_entity_with_state("Puerto Rico", Some("PR")),
            "Puerto Rico - PR"
        );
    }

    // Gherkin scenario 2
    #[test]
    fn us_entity_without_state_is_entity_alone() {
        assert_eq!(
            format_entity_with_state("United States", None),
            "United States"
        );
        assert_eq!(
            format_entity_with_state("United States", Some("")),
            "United States"
        );
        assert_eq!(
            format_entity_with_state("United States", Some("   ")),
            "United States"
        );
        assert_eq!(
            format_entity_with_state("United States", Some("unknown")),
            "United States"
        );
    }

    #[test]
    fn non_us_entity_never_gets_a_suffix() {
        assert_eq!(format_entity_with_state("Japan", Some("AR")), "Japan");
        assert_eq!(
            format_entity_with_state("British Virgin Islands", Some("VI")),
            "British Virgin Islands"
        );
        assert_eq!(format_entity_with_state("Samoa", Some("AS")), "Samoa");
    }
}
