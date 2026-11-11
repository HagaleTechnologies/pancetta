//! Callsign → DXCC entity-name resolver (offline, cty.dat-derived).
//!
//! The DX Hunter shows a DXCC "Entity" column. cqdx live spots already carry an
//! `entity_name`, but LOCAL decodes (the bulk of what the operator sees) have
//! none. This module provides a self-contained, offline resolver over the 317
//! distinct entity names in the generated prefix table, so a local decode still
//! shows its country. It lives in `pancetta-core` so the priority evaluator,
//! alerts, QSO logging and the TUI share one domain lookup (PAN-284).
//!
//! The prefix table (`dxcc_table::PREFIX_TABLE`) is auto-generated from the
//! AD1C BigCTY `cty.dat` file (see `pancetta-core/scripts/gen_dxcc_table.py`).
//! Lookups probe a prefix index built on first use, from the longest table
//! prefix down to one character, so the first hit is the longest
//! leading-prefix match. That handles portable-PREFIX compounds (e.g.
//! "EA8/G8BCG" → "EA8" → Canary Islands) and ignores trailing "/P", "/MM"
//! suffixes. A resolved FT8 hash-render (`"<N7RLK>"`, PAN-58) resolves as its
//! plain callsign, and a small US safety net answers K/W/N and AA–AL calls the
//! table does not cover. Unmatched prefixes return `None` (the column shows
//! "---"): we never emit a GUESS that could mislabel a station. Where the TUI
//! has cqdx's authoritative `entity_name`, that still takes precedence.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::dxcc_table::PREFIX_TABLE;

/// `PREFIX_TABLE` keyed by prefix, plus the longest prefix length.
struct PrefixIndex {
    by_prefix: HashMap<&'static str, &'static str>,
    max_len: usize,
}

static INDEX: LazyLock<PrefixIndex> = LazyLock::new(|| PrefixIndex {
    by_prefix: PREFIX_TABLE.iter().copied().collect(),
    max_len: PREFIX_TABLE.iter().map(|(p, _)| p.len()).max().unwrap_or(0),
});

/// Resolve a callsign to a DXCC entity name, best-effort. Returns `None` for an
/// unrecognized prefix (caller renders "---" rather than a guess).
///
/// Full normalized exact calls win, followed by the KG4 length rule and a
/// longest-leading-prefix match against `PREFIX_TABLE`. Both tables use
/// indexed lookups. The prefix-only layer remains equivalent to the
/// pre-PAN-284 linear scan (pinned by the full equivalence corpus).
/// The "K"/"W"/"N" and "AA"–"AL" US prefixes are covered by the table; the
/// US safety net below answers the US patterns cty.dat does not list.
pub fn entity_for_callsign(call: &str) -> Option<&'static str> {
    let trimmed = call.trim().to_uppercase();
    if trimmed.is_empty() {
        return None;
    }
    // PAN-58: a decode heard via FT8's i3=4 hash-render comes through as
    // "<N7RLK>" (see `crate::callsign::resolve_hash_render`'s doc).
    // Resolve to the plain callsign it represents before prefix-matching —
    // otherwise the leading '<' never matches any PREFIX_TABLE entry and
    // every hash-rendered decode's Entity column reads blank ("---"). The
    // unresolved hash-miss placeholder "<...>" carries no identity at all
    // and correctly still resolves to `None`.
    let c = crate::callsign::resolve_hash_render(&trimmed)?;

    layered_lookup(c, |c| EXACT_INDEX.get(c).copied())
}

static EXACT_INDEX: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| crate::dxcc_exact::EXACT_TABLE.iter().copied().collect());

struct EntityIndex {
    by_name: HashMap<&'static str, (u16, &'static str)>,
    by_primary: HashMap<&'static str, u16>,
}

static ENTITIES: LazyLock<EntityIndex> = LazyLock::new(|| {
    let mut by_name = HashMap::new();
    let mut by_primary = HashMap::new();
    for &(primary, name, id, canonical) in crate::dxcc_registry::REGISTRY {
        by_name.insert(name, (id, canonical));
        by_primary.insert(primary, id);
    }
    for &(alias, name) in crate::dxcc_registry::NAME_ALIASES {
        let identity = by_name[name];
        by_name.insert(alias, identity);
    }
    EntityIndex {
        by_name,
        by_primary,
    }
});

/// ADIF DXCC identity resolved with the same precedence as the displayed entity.
pub fn entity_id_for_callsign(call: &str) -> Option<u16> {
    entity_for_callsign(call).and_then(entity_id_of_name)
}

/// ADIF identity of a known cty entity name, including contest subdivisions.
pub fn entity_id_of_name(name: &str) -> Option<u16> {
    ENTITIES.by_name.get(name).map(|&(id, _)| id)
}

/// Look up a cty primary prefix as a registry key, never as a callsign.
pub fn entity_by_primary_prefix(primary: &str) -> Option<u16> {
    ENTITIES
        .by_primary
        .get(primary.trim().to_uppercase().as_str())
        .copied()
}

/// Whether this identity is represented in the offline entity registry.
pub fn is_known_entity_id(id: u16) -> bool {
    ENTITIES.by_primary.values().any(|&known| known == id)
}

/// Canonical parent entity name for history keys; preserve unknown names.
pub fn canonical_entity(name: &str) -> &str {
    ENTITIES
        .by_name
        .get(name)
        .map_or(name, |&(_, canonical)| canonical)
}

struct SubEntityRule {
    prefix: &'static str,
    short_entity: &'static str,
    other_entity: &'static str,
}

const SUB_ENTITY_RULES: &[SubEntityRule] = &[SubEntityRule {
    prefix: "KG4",
    short_entity: "Guantanamo Bay",
    other_entity: "United States",
}];

fn sub_entity_rule(c: &str) -> Option<&'static str> {
    let head = c.split('/').next()?;
    for rule in SUB_ENTITY_RULES {
        if let Some(suffix) = head.strip_prefix(rule.prefix) {
            return Some(
                if suffix.is_empty()
                    || (suffix.len() == 2 && suffix.bytes().all(|b| b.is_ascii_alphabetic()))
                {
                    rule.short_entity
                } else {
                    rule.other_entity
                },
            );
        }
    }
    None
}

fn layered_lookup(
    c: &str,
    exact: impl FnOnce(&str) -> Option<&'static str>,
) -> Option<&'static str> {
    exact(c)
        .or_else(|| sub_entity_rule(c))
        .or_else(|| prefix_lookup(c))
}

fn prefix_lookup(c: &str) -> Option<&'static str> {
    // Longest leading-prefix match: probe from the longest table prefix down.
    // The leading prefix also correctly handles portable-PREFIX compounds
    // (e.g. "EA8/G8BCG" → "EA8" → Canary Islands) and ignores trailing "/P",
    // "/MM" suffixes. `get` is `None` off a char boundary; every key is
    // ASCII, so such a slice could not have matched the old scan either.
    let index = &*INDEX;
    for len in (1..=c.len().min(index.max_len)).rev() {
        if let Some(&name) = c.get(..len).and_then(|p| index.by_prefix.get(p)) {
            return Some(name);
        }
    }

    us_safety_net(c)
}
/// US safety-net fallback: K/W/N anything, or A followed by A–L (the AA–AL US
/// block). Reachable: the table has no bare `AH` key (cty.dat lists AH0–AH9),
/// so `AH` plus a non-digit (e.g. "AHA") lands here and answers
/// "United States".
fn us_safety_net(c: &str) -> Option<&'static str> {
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
    "Kure Island",
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
    fn entity_registry_is_complete_and_ids_are_valid() {
        let valid: serde_json::Value =
            serde_json::from_str(include_str!("../data/adif_dxcc_ids.json")).unwrap();
        for &(_, name) in PREFIX_TABLE.iter().chain(crate::dxcc_exact::EXACT_TABLE) {
            let id = entity_id_of_name(name).unwrap_or_else(|| panic!("unmapped {name}"));
            assert!(
                valid["entities"].get(id.to_string()).is_some(),
                "{name}: {id}"
            );
        }
        for (call, id) in [
            ("JA1ABC", 339),
            ("W1AW", 291),
            ("DL1ABC", 230),
            ("VK2ABC", 150),
            ("3Y0J", 24),
            ("KG4AC", 105),
        ] {
            assert_eq!(entity_id_for_callsign(call), Some(id), "{call}");
        }
    }

    #[test]
    fn primary_prefix_lookup_handles_slash_entities() {
        assert_eq!(entity_by_primary_prefix("3Y/B"), Some(24));
        assert_eq!(entity_by_primary_prefix("3y/b"), Some(24));
        assert_eq!(entity_by_primary_prefix("QQ"), None);
    }

    #[test]
    fn cty_subdivisions_share_parent_ids() {
        for (child, parent) in [
            ("European Turkey", "Asiatic Turkey"),
            ("African Italy", "Italy"),
            ("Sicily", "Italy"),
            ("Shetland Islands", "Scotland"),
            ("Bear Island", "Svalbard"),
            ("Vienna Intl Ctr", "Austria"),
        ] {
            assert_eq!(entity_id_of_name(child), entity_id_of_name(parent));
            assert_eq!(canonical_entity(child), parent);
        }
        assert_eq!(canonical_entity("Unknown name"), "Unknown name");
        assert_eq!(entity_id_of_name("Cabo Verde"), Some(409));
    }

    fn fixture_resolve(call: &str, exact: &[(&str, &'static str)]) -> Option<&'static str> {
        let normalized = call.trim().to_uppercase();
        let c = crate::callsign::resolve_hash_render(&normalized)?;
        layered_lookup(c, |c| {
            exact
                .iter()
                .find_map(|(key, name)| (*key == c).then_some(*name))
        })
    }

    #[test]
    fn exact_call_override_beats_subentity_and_prefix() {
        assert_eq!(
            fixture_resolve("KG4AB", &[("KG4AB", "United States")]),
            Some("United States")
        );
        assert_eq!(fixture_resolve("KG4AB", &[]), Some("Guantanamo Bay"));
        assert_eq!(entity_for_callsign("KG4BKW"), Some("Guam"));
        assert_eq!(entity_for_callsign("W1AW/KG4"), Some("Guantanamo Bay"));
        assert_eq!(entity_for_callsign("3Y0J"), Some("Bouvet"));
    }

    #[test]
    fn exact_call_normalization_and_full_match() {
        let fixture = [("KG4AB", "United States"), ("EA8/KG4AB", "Japan")];
        for call in [" kg4ab ", "<KG4AB>"] {
            assert_eq!(fixture_resolve(call, &fixture), Some("United States"));
        }
        assert_eq!(fixture_resolve("KG4ABC", &fixture), Some("United States"));
        assert_eq!(fixture_resolve("EA8/KG4AB", &fixture), Some("Japan"));
        assert_eq!(
            fixture_resolve("EA8/KG4ABC", &fixture),
            Some("Canary Islands")
        );
        assert_eq!(fixture_resolve("KG4AB/P", &fixture), Some("Guantanamo Bay"));
        assert_eq!(fixture_resolve("<...>", &fixture), None);
    }

    #[test]
    fn every_generated_exact_call_resolves() {
        for &(call, name) in crate::dxcc_exact::EXACT_TABLE {
            assert_eq!(entity_for_callsign(call), Some(name), "{call}");
        }
    }

    #[test]
    fn kg4_length_rule_and_portable_suffix() {
        for call in ["KG4", "KG4AB", "KG4AC", "KG4AB/P"] {
            assert_eq!(entity_for_callsign(call), Some("Guantanamo Bay"), "{call}");
        }
        for call in [
            "KG4OJT", "KG4ABC", "KG4DFX", "KG4OJT/P", "KG4A", "KG4ÉÉ", "KG412",
        ] {
            assert_eq!(entity_for_callsign(call), Some("United States"), "{call}");
        }
    }

    /// The pre-PAN-284 resolver, kept verbatim as the oracle for the index.
    fn linear_entity_for_callsign(call: &str) -> Option<&'static str> {
        let trimmed = call.trim().to_uppercase();
        if trimmed.is_empty() {
            return None;
        }
        let c = crate::callsign::resolve_hash_render(&trimmed)?;
        let mut best: Option<(&str, &str)> = None;
        for &(pfx, name) in PREFIX_TABLE {
            if c.starts_with(pfx) && best.is_none_or(|(b, _)| pfx.len() > b.len()) {
                best = Some((pfx, name));
            }
        }
        best.map(|(_, name)| name).or_else(|| us_safety_net(c))
    }

    /// Every [A-Z0-9] string of length 1 to 3 (47,988), each table prefix in
    /// eight shapes (57,384), and odd inputs: 105,385 in total.
    fn equivalence_corpus() -> Vec<String> {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut out = Vec::with_capacity(105_385);
        for &a in CHARS {
            out.push(char::from(a).to_string());
            for &b in CHARS {
                out.push(format!("{}{}", char::from(a), char::from(b)));
                for &c in CHARS {
                    out.push(format!(
                        "{}{}{}",
                        char::from(a),
                        char::from(b),
                        char::from(c)
                    ));
                }
            }
        }
        for (prefix, _) in PREFIX_TABLE {
            for suffix in ["", "1ABC", "A", "/P", "0"] {
                out.push(format!("{prefix}{suffix}"));
            }
            out.push(format!("{prefix}/G8BCG"));
            out.push(format!("<{prefix}1AB>"));
            out.push(prefix.to_lowercase());
        }
        out.extend(
            [
                "",
                " ",
                "<...>",
                "<>",
                "<<K1ABC>>",
                "ß",
                "ﬁ1AB",
                "É1ABC",
                " k5arh ",
                "K5ARH/MM",
                "AH",
                "AHA",
                "AHAB",
            ]
            .map(String::from),
        );
        out
    }

    #[test]
    fn indexed_lookup_matches_the_linear_scan_over_the_corpus() {
        let corpus = equivalence_corpus();
        assert_eq!(corpus.len(), 105_385);
        let mismatches: Vec<_> = corpus
            .iter()
            .filter(|c| {
                let upper = c.trim().to_uppercase();
                let indexed = crate::callsign::resolve_hash_render(&upper).and_then(prefix_lookup);
                indexed != linear_entity_for_callsign(c)
            })
            .take(20)
            .collect();
        assert!(mismatches.is_empty(), "first mismatches: {mismatches:?}");
    }

    #[test]
    fn full_layered_precedence_over_the_corpus() {
        let exact: HashMap<_, _> = crate::dxcc_exact::EXACT_TABLE.iter().copied().collect();
        for call in equivalence_corpus() {
            let upper = call.trim().to_uppercase();
            let expected = crate::callsign::resolve_hash_render(&upper).and_then(|c| {
                exact
                    .get(c)
                    .copied()
                    .or_else(|| {
                        let head = c.split('/').next().unwrap();
                        head.strip_prefix("KG4").map(|suffix| {
                            if suffix.is_empty()
                                || (suffix.len() == 2
                                    && suffix.bytes().all(|b| b.is_ascii_alphabetic()))
                            {
                                "Guantanamo Bay"
                            } else {
                                "United States"
                            }
                        })
                    })
                    .or_else(|| linear_entity_for_callsign(&call))
            });
            assert_eq!(entity_for_callsign(&call), expected, "{call}");
        }
    }

    #[test]
    fn index_has_one_key_per_table_row() {
        assert_eq!(PREFIX_TABLE.len(), 7_173);
        assert_eq!(
            INDEX.by_prefix.len(),
            PREFIX_TABLE.len(),
            "duplicate prefix in PREFIX_TABLE"
        );
        let longest = PREFIX_TABLE.iter().map(|(p, _)| p.len()).max().unwrap();
        assert_eq!(INDEX.max_len, longest);
    }

    #[test]
    fn us_safety_net_still_answers_ah_without_a_digit() {
        // cty.dat has AH0..AH9 but no bare AH, so these reach the safety net.
        for call in ["AH", "AHA", "AHAB"] {
            assert_eq!(entity_for_callsign(call), Some("United States"), "{call}");
        }
        assert_eq!(entity_for_callsign("AH6ABC"), Some("Hawaii"));
    }

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
        assert_eq!(entity_for_callsign("KP4ABC"), Some("Puerto Rico"));
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
    /// identifiable callsign (see `crate::callsign::resolve_hash_render`)
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
            "Kure Island",
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
        // KH7K/AH7K/NH7K resolve to Kure Island, logged with STATE=HI.
        assert_eq!(
            format_entity_with_state("Kure Island", Some("HI")),
            "Kure Island - HI"
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
