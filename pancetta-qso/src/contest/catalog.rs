//! Built-in contest catalog — see docs/superpowers/specs/2026-08-30-contest-mode-design.md §1.

use super::profile::{ContestProfile, ExchangeShape};

/// Contest profiles shipped with pancetta. Operators can add their own via
/// `contest.custom_profiles` in pancetta-config (a later plan).
pub fn builtin_catalog() -> Vec<ContestProfile> {
    vec![ContestProfile {
        id: "us-state-qso-party".to_string(),
        display_name: "US State QSO Party".to_string(),
        cq_tag_patterns: vec!["KSQP".to_string(), "SCQP".to_string()],
        exchange_shape: ExchangeShape::GridWithRAck,
        verified: true,
        source_notes: "Live-confirmed 2026-08-29/30: 285 KSQP (Kansas QSO \
            Party) \"R\"+grid exchanges across dozens of unrelated \
            callsign pairs (~/.pancetta/logs/pancetta.log.2026-08-30), plus \
            the SC QSO Party's own published FT8/FT4 digital-mode \
            instructions. Tag list intentionally partial — most US state \
            QSO parties use their own state abbreviation as the CQ tag; \
            add others via contest.custom_profiles as confirmed."
            .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_nonempty_and_every_entry_is_verified() {
        let catalog = builtin_catalog();
        assert!(!catalog.is_empty());
        for profile in &catalog {
            assert!(
                profile.verified,
                "profile {} must be verified to ship in the built-in catalog",
                profile.id
            );
            assert!(!profile.id.is_empty());
            assert!(!profile.cq_tag_patterns.is_empty());
        }
    }

    #[test]
    fn us_state_qso_party_profile_uses_grid_with_r_ack() {
        let catalog = builtin_catalog();
        let profile = catalog
            .iter()
            .find(|p| p.id == "us-state-qso-party")
            .expect("us-state-qso-party must be in the built-in catalog");
        assert_eq!(profile.exchange_shape, ExchangeShape::GridWithRAck);
        assert!(profile.cq_tag_patterns.contains(&"KSQP".to_string()));
    }
}
