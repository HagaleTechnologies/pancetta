//! PAN-494 / PAN-495: new-on-band and ATNO from cqdx's full-history lists.

use super::test_support::{adif, seeded_lookup};
use super::*;
use pancetta_qso::priority::{PriorityScorer, PriorityTier, PriorityWeights};

fn scorer(l: &CachedStationLookup) -> PriorityScorer {
    PriorityScorer::new(PriorityWeights::default(), Box::new(l.clone()))
}

fn needed_row(entity_id: u32, prefix: &str, atno: bool) -> pancetta_cqdx::NeededEntity {
    pancetta_cqdx::NeededEntity {
        entity_id,
        prefix: prefix.into(),
        name: prefix.into(),
        atno,
    }
}

// PAN-495: cqdx's per-band `atno` flag means "never confirmed"; only
// the all-bands list (never worked anywhere) may make an ATNO.
#[test]
fn worked_but_unconfirmed_entity_is_not_atno_on_a_per_band_list() {
    let l = CachedStationLookup::new();
    // All-bands list: Bouvet only (Fiji was worked somewhere).
    l.update_needed_entities(&[needed_row(24, "3Y/B", true)], None, UtcTimestamp::now());
    // 15m list flags both as never confirmed.
    l.update_needed_entities(
        &[needed_row(176, "3D2", true), needed_row(24, "3Y/B", true)],
        Some("15M".into()),
        UtcTimestamp::now(),
    );
    assert_eq!(l.needed_dxcc_status("3D2YA"), NeedStatus::Needed);
    assert!(!l.is_atno("3D2YA"));
    assert_eq!(l.display_needed_atno("3D2YA"), (true, false));
    assert_eq!(l.needed_atno_shown("3D2YA"), (true, false));
    let fiji = scorer(&l).score_tiered_explained("3D2YA", None, -10, 21_074_000.0);
    assert_ne!(fiji.score.tier, PriorityTier::Atno);
    assert!(l.is_atno("3Y0J"));
    assert_eq!(l.display_needed_atno("3Y0J"), (true, true));
    let bouvet = scorer(&l).score_tiered_explained("3Y0J", None, -10, 21_074_000.0);
    assert_eq!(bouvet.score.tier, PriorityTier::Atno);
}

#[test]
fn per_band_atno_flag_alone_never_claims_atno() {
    let l = CachedStationLookup::new();
    l.update_needed_entities(
        &[needed_row(24, "3Y/B", true)],
        Some("20M".into()),
        UtcTimestamp::now(),
    );
    assert_eq!(l.needed_dxcc_status("3Y0J"), NeedStatus::Needed);
    assert!(!l.is_atno("3Y0J"));
    assert_eq!(l.display_needed_atno("3Y0J"), (true, false));
}

#[tokio::test]
async fn entity_logged_this_session_stops_being_atno_before_cqdx_refreshes() {
    let (_tmp, l) = seeded_lookup(&adif(&[("3Y0J", "FT8", 14.074, "")]), "K5ARH").await;
    l.update_needed_entities(&[needed_row(24, "3Y/B", true)], None, UtcTimestamp::now());
    assert!(!l.is_atno("3Y0J"));
    assert_eq!(l.needed_atno_shown("3Y0J"), (true, false));
}

// PAN-494: "new on this band" follows cqdx's list for the operating band.
#[tokio::test]
async fn cqdx_band_list_without_entity_is_not_new_on_band() {
    // Canada logged locally only on 20m; cqdx's 15m list omits Canada.
    let (_tmp, l) = seeded_lookup(&adif(&[("VE3ABC", "FT8", 14.074, "")]), "K5ARH").await;
    l.update_needed_entities(
        &[needed_row(24, "3Y/B", true)],
        Some("15M".into()),
        UtcTimestamp::now(),
    );
    assert!(!l.is_dxcc_needed_on_band("VE3XYZ", 21_074_000.0));
    let ve = scorer(&l).score_tiered_explained("VE3XYZ", None, -10, 21_074_000.0);
    assert!(!ve.factors.dxcc_new_on_band);
    assert_eq!(ve.score.tier, PriorityTier::Standard);
}

#[tokio::test]
async fn cqdx_band_list_with_entity_is_new_on_band() {
    let (_tmp, l) = seeded_lookup(&adif(&[("VE3ABC", "FT8", 14.074, "")]), "K5ARH").await;
    l.update_needed_entities(
        &[needed_row(1, "VE", false)],
        Some("160M".into()),
        UtcTimestamp::now(),
    );
    assert!(l.is_dxcc_needed_on_band("VE3XYZ", 1_840_000.0));
}

#[tokio::test]
async fn local_log_decides_new_on_band_when_cqdx_list_is_for_another_band() {
    let (_tmp, l) = seeded_lookup(&adif(&[("VE3ABC", "FT8", 14.074, "")]), "K5ARH").await;
    // Only a 20m list loaded; operating on 15m.
    l.update_needed_entities(&[], Some("20M".into()), UtcTimestamp::now());
    assert!(l.is_dxcc_needed_on_band("VE3XYZ", 21_074_000.0));
    assert!(!l.is_dxcc_needed_on_band("VE3XYZ", 14_074_000.0));
}

#[tokio::test]
async fn local_qso_clears_new_on_band_while_cqdx_band_list_still_lists_entity() {
    let (_tmp, l) = seeded_lookup(&adif(&[("JA1ABC", "FT8", 21.074, "")]), "K5ARH").await;
    l.update_needed_entities(
        &[needed_row(339, "JA", false)],
        Some("15M".into()),
        UtcTimestamp::now(),
    );
    assert!(!l.is_dxcc_needed_on_band("JA1XYZ", 21_074_000.0));
}
