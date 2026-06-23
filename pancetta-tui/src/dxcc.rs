//! Lightweight callsign → DXCC entity-name resolver.
//!
//! The DX Hunter shows a DXCC "Entity" column. cqdx live spots already carry an
//! `entity_name`, but LOCAL decodes (the bulk of what the operator sees) have
//! none, so they rendered as "---". This module provides a self-contained,
//! offline resolver covering the common + notable DXCC entities so a local
//! decode still shows its country.
//!
//! It is deliberately a **curated best-effort** table, not an authoritative
//! 340-entity DXCC database: a longest-leading-prefix match over well-known,
//! unambiguous prefixes, with a US fallback (`K`/`W`/`N`, and `AA`–`AL`).
//! Unmatched prefixes return `None` (the column shows "---") — we never emit a
//! GUESS that could mislabel a station. cqdx's authoritative `entity_name`
//! always takes precedence when present.

/// Curated (leading-prefix, entity-name) pairs. Matching is longest-prefix-wins,
/// so a more-specific prefix (e.g. `KH6`) overrides a broader one. Order here is
/// not significant — the resolver tracks the longest match.
const PREFIX_TABLE: &[(&str, &str)] = &[
    // North America (US sub-entities override the K/W/N fallback below)
    ("KH6", "Hawaii"),
    ("KH2", "Guam"),
    ("KL", "Alaska"),
    ("AL", "Alaska"),
    ("KP4", "Puerto Rico"),
    ("KP2", "US Virgin Is"),
    ("VE", "Canada"),
    ("VA", "Canada"),
    ("VO", "Canada"),
    ("VY", "Canada"),
    ("XE", "Mexico"),
    ("XF", "Mexico"),
    ("CO", "Cuba"),
    ("CM", "Cuba"),
    ("HI", "Dominican Rep"),
    ("HH", "Haiti"),
    ("TI", "Costa Rica"),
    ("TG", "Guatemala"),
    ("YS", "El Salvador"),
    ("HP", "Panama"),
    ("HR", "Honduras"),
    ("YN", "Nicaragua"),
    ("V3", "Belize"),
    ("ZF", "Cayman Is"),
    ("8P", "Barbados"),
    ("J3", "Grenada"),
    ("J6", "St Lucia"),
    ("J7", "Dominica"),
    ("J8", "St Vincent"),
    ("PJ", "Curacao/Bonaire"),
    ("FM", "Martinique"),
    ("FG", "Guadeloupe"),
    ("FS", "St Martin"),
    // South America
    ("PY", "Brazil"),
    ("PP", "Brazil"),
    ("PR", "Brazil"),
    ("PT", "Brazil"),
    ("PU", "Brazil"),
    ("LU", "Argentina"),
    ("CE", "Chile"),
    ("CX", "Uruguay"),
    ("CP", "Bolivia"),
    ("OA", "Peru"),
    ("HC", "Ecuador"),
    ("HK", "Colombia"),
    ("YV", "Venezuela"),
    ("ZP", "Paraguay"),
    ("8R", "Guyana"),
    ("PZ", "Suriname"),
    ("FY", "Fr Guiana"),
    // Europe
    ("G", "England"),
    ("M", "England"),
    ("2E", "England"),
    ("GW", "Wales"),
    ("MW", "Wales"),
    ("2W", "Wales"),
    ("GM", "Scotland"),
    ("MM", "Scotland"),
    ("2M", "Scotland"),
    ("GI", "N Ireland"),
    ("MI", "N Ireland"),
    ("GD", "Isle of Man"),
    ("GJ", "Jersey"),
    ("GU", "Guernsey"),
    ("EI", "Ireland"),
    ("EJ", "Ireland"),
    ("F", "France"),
    ("TM", "France"),
    ("DL", "Germany"),
    ("DA", "Germany"),
    ("DB", "Germany"),
    ("DC", "Germany"),
    ("DD", "Germany"),
    ("DF", "Germany"),
    ("DG", "Germany"),
    ("DH", "Germany"),
    ("DJ", "Germany"),
    ("DK", "Germany"),
    ("DM", "Germany"),
    ("DO", "Germany"),
    ("I", "Italy"),
    ("EA", "Spain"),
    ("EB", "Spain"),
    ("EC", "Spain"),
    ("ED", "Spain"),
    ("EA6", "Balearic Is"),
    ("EA8", "Canary Is"),
    ("EA9", "Ceuta & Melilla"),
    ("CT", "Portugal"),
    ("CR", "Portugal"),
    ("CU", "Azores"),
    ("CT3", "Madeira"),
    ("CQ", "Portugal"),
    ("PA", "Netherlands"),
    ("PB", "Netherlands"),
    ("PC", "Netherlands"),
    ("PD", "Netherlands"),
    ("PE", "Netherlands"),
    ("PH", "Netherlands"),
    ("ON", "Belgium"),
    ("OO", "Belgium"),
    ("OT", "Belgium"),
    ("LX", "Luxembourg"),
    ("OZ", "Denmark"),
    ("OU", "Denmark"),
    ("SM", "Sweden"),
    ("SA", "Sweden"),
    ("SK", "Sweden"),
    ("LA", "Norway"),
    ("LB", "Norway"),
    ("OH", "Finland"),
    ("OF", "Finland"),
    ("OH0", "Aland Is"),
    ("OE", "Austria"),
    ("HB9", "Switzerland"),
    ("HB0", "Liechtenstein"),
    ("OK", "Czech Rep"),
    ("OL", "Czech Rep"),
    ("OM", "Slovakia"),
    ("SP", "Poland"),
    ("SQ", "Poland"),
    ("SN", "Poland"),
    ("HA", "Hungary"),
    ("HG", "Hungary"),
    ("YO", "Romania"),
    ("YP", "Romania"),
    ("LZ", "Bulgaria"),
    ("SV", "Greece"),
    ("SW", "Greece"),
    ("SY", "Greece"),
    ("YU", "Serbia"),
    ("YT", "Serbia"),
    ("E7", "Bosnia-Herz"),
    ("9A", "Croatia"),
    ("S5", "Slovenia"),
    ("Z3", "N Macedonia"),
    ("ZA", "Albania"),
    ("4O", "Montenegro"),
    ("T7", "San Marino"),
    ("9H", "Malta"),
    ("ES", "Estonia"),
    ("YL", "Latvia"),
    ("LY", "Lithuania"),
    ("UR", "Ukraine"),
    ("UT", "Ukraine"),
    ("UU", "Ukraine"),
    ("UX", "Ukraine"),
    ("EW", "Belarus"),
    ("EU", "Belarus"),
    ("ER", "Moldova"),
    ("R", "Russia"),
    ("U", "Russia"),
    // Africa
    ("ZS", "South Africa"),
    ("ZR", "South Africa"),
    ("CN", "Morocco"),
    ("SU", "Egypt"),
    ("3V", "Tunisia"),
    ("7X", "Algeria"),
    ("5A", "Libya"),
    ("ST", "Sudan"),
    ("5Z", "Kenya"),
    ("5H", "Tanzania"),
    ("5X", "Uganda"),
    ("9J", "Zambia"),
    ("Z2", "Zimbabwe"),
    ("A2", "Botswana"),
    ("V5", "Namibia"),
    ("3DA", "Eswatini"),
    ("7P", "Lesotho"),
    ("C9", "Mozambique"),
    ("5R", "Madagascar"),
    ("3B8", "Mauritius"),
    ("3B9", "Rodrigues"),
    ("FR", "Reunion"),
    ("D4", "Cape Verde"),
    ("EL", "Liberia"),
    ("5N", "Nigeria"),
    ("9G", "Ghana"),
    // Asia / Middle East
    ("JA", "Japan"),
    ("JE", "Japan"),
    ("JF", "Japan"),
    ("JG", "Japan"),
    ("JH", "Japan"),
    ("JI", "Japan"),
    ("JJ", "Japan"),
    ("JK", "Japan"),
    ("JL", "Japan"),
    ("JM", "Japan"),
    ("JN", "Japan"),
    ("JO", "Japan"),
    ("JP", "Japan"),
    ("JQ", "Japan"),
    ("JR", "Japan"),
    ("JS", "Japan"),
    ("7J", "Japan"),
    ("8J", "Japan"),
    ("BY", "China"),
    ("BA", "China"),
    ("BD", "China"),
    ("BG", "China"),
    ("BH", "China"),
    ("BV", "Taiwan"),
    ("HL", "South Korea"),
    ("DS", "South Korea"),
    ("6K", "South Korea"),
    ("VU", "India"),
    ("9V", "Singapore"),
    ("9M", "Malaysia"),
    ("HS", "Thailand"),
    ("E2", "Thailand"),
    ("YB", "Indonesia"),
    ("YC", "Indonesia"),
    ("YD", "Indonesia"),
    ("DU", "Philippines"),
    ("DV", "Philippines"),
    ("XV", "Vietnam"),
    ("3W", "Vietnam"),
    ("XU", "Cambodia"),
    ("4X", "Israel"),
    ("4Z", "Israel"),
    ("JY", "Jordan"),
    ("OD", "Lebanon"),
    ("YK", "Syria"),
    ("YI", "Iraq"),
    ("EP", "Iran"),
    ("A4", "Oman"),
    ("A6", "United Arab Em"),
    ("A7", "Qatar"),
    ("A9", "Bahrain"),
    ("HZ", "Saudi Arabia"),
    ("9K", "Kuwait"),
    ("EK", "Armenia"),
    ("4J", "Azerbaijan"),
    ("4L", "Georgia"),
    ("UN", "Kazakhstan"),
    ("EX", "Kyrgyzstan"),
    ("EY", "Tajikistan"),
    ("EZ", "Turkmenistan"),
    ("UK", "Uzbekistan"),
    ("TA", "Turkey"),
    ("5B", "Cyprus"),
    ("AP", "Pakistan"),
    ("S2", "Bangladesh"),
    ("4S", "Sri Lanka"),
    ("XW", "Laos"),
    ("XZ", "Myanmar"),
    // Oceania
    ("VK", "Australia"),
    ("ZL", "New Zealand"),
    ("KH6B", "Hawaii"),
    ("FK", "New Caledonia"),
    ("FO", "Fr Polynesia"),
    ("E5", "Cook Is"),
    ("5W", "Samoa"),
    ("A3", "Tonga"),
    ("3D2", "Fiji"),
    ("YJ", "Vanuatu"),
    ("H4", "Solomon Is"),
    ("P2", "Papua New Guinea"),
    ("T8", "Palau"),
    ("V7", "Marshall Is"),
    ("V6", "Micronesia"),
    ("T3", "Kiribati"),
];

/// Resolve a callsign to a DXCC entity name, best-effort. Returns `None` for an
/// unrecognized prefix (caller renders "---" rather than a guess).
pub fn entity_for_callsign(call: &str) -> Option<&'static str> {
    let c = call.trim().to_uppercase();
    if c.is_empty() {
        return None;
    }

    // Longest leading-prefix match over the curated table. The leading prefix
    // also correctly handles portable-PREFIX compounds (e.g. "EA8/G8BCG" →
    // "EA8" → Canary Is) and ignores trailing "/P", "/MM" suffixes.
    let mut best: Option<(&str, &str)> = None;
    for &(pfx, name) in PREFIX_TABLE {
        if c.starts_with(pfx) && best.is_none_or(|(b, _)| pfx.len() > b.len()) {
            best = Some((pfx, name));
        }
    }
    if let Some((_, name)) = best {
        return Some(name);
    }

    // US fallback: K/W/N anything, or A followed by A–L (the AA–AL US block).
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
        assert_eq!(entity_for_callsign("DL1ABC"), Some("Germany"));
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
        assert_eq!(entity_for_callsign("EA8ABC"), Some("Canary Is"));
        assert_eq!(entity_for_callsign("EA6XYZ"), Some("Balearic Is"));
        // Scotland (GM) beats England (G).
        assert_eq!(entity_for_callsign("GM4ABC"), Some("Scotland"));
        assert_eq!(entity_for_callsign("MW0ABC"), Some("Wales"));
    }

    #[test]
    fn compound_and_portable_handled() {
        // Portable PREFIX → entity of the prefix.
        assert_eq!(entity_for_callsign("EA8/G8BCG"), Some("Canary Is"));
        // Portable SUFFIX ignored (leading prefix wins).
        assert_eq!(entity_for_callsign("G8BCG/P"), Some("England"));
    }

    #[test]
    fn a_prefix_disambiguation() {
        // A + letter A–L = US; A + digit = a specific country.
        assert_eq!(entity_for_callsign("AL7XYZ"), Some("Alaska")); // table AL beats US-A
        assert_eq!(entity_for_callsign("A4XYZ"), Some("Oman"));
        assert_eq!(entity_for_callsign("A6ABC"), Some("United Arab Em"));
        assert_eq!(entity_for_callsign("AC2XYZ"), Some("United States"));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(entity_for_callsign(""), None);
        // A fictional/unassigned-looking prefix not in the table.
        assert_eq!(entity_for_callsign("QZ9ZZ"), None);
    }
}
