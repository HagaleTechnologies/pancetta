//! PAN-50 contest-scoring reducer integration tests: deterministic score,
//! multiplier sets, and dupe classifications for the two built-in rule
//! editions (Kansas FT4/FT8 2026, South Carolina fixed digital 2026).
//!
//! Fixtures use fixed times and hand-computed expected totals, per the plan.

use chrono::{DateTime, TimeZone, Utc};
use pancetta_qso::contest::scoring::*;

fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 29, 14, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
}

fn kansas_tx_entrant() -> ContestScoringContext {
    ContestScoringContext::new(
        "KSQP-2026-K5ARH",
        "us-state-qso-party",
        KSQP_FT_2026,
        "TX",
        vec![
            ContestPeriod {
                start: t(0),
                end: t(720),
            },
            ContestPeriod {
                start: t(1440),
                end: t(1800),
            },
        ],
    )
    .unwrap()
}

fn kansas_ks_entrant() -> ContestScoringContext {
    ContestScoringContext::new(
        "KSQP-2026-K5ARH-KS",
        "us-state-qso-party",
        KSQP_FT_2026,
        "KS",
        vec![ContestPeriod {
            start: t(0),
            end: t(720),
        }],
    )
    .unwrap()
}

fn scqp(operator_region: &str, instance: &str) -> ContestScoringContext {
    ContestScoringContext::new(
        instance,
        "us-state-qso-party",
        SCQP_DIGITAL_FIXED_2026,
        operator_region,
        vec![ContestPeriod {
            start: t(0),
            end: t(720),
        }],
    )
    .unwrap()
}

#[derive(Clone)]
struct FactBuilder {
    instance_id: String,
    callsign: &'static str,
    band: Option<&'static str>,
    mode: &'static str,
    their_region: Option<&'static str>,
    their_county: Option<&'static str>,
    our_county: Option<&'static str>,
    failed: bool,
}

impl FactBuilder {
    fn new(instance: &str, call: &'static str) -> Self {
        FactBuilder {
            instance_id: instance.to_string(),
            callsign: call,
            band: Some("20M"),
            mode: "FT8",
            their_region: None,
            their_county: None,
            our_county: None,
            failed: false,
        }
    }
    fn at(&self, minutes: i64, id: &str) -> ContestContactFact {
        ContestContactFact {
            contact_id: id.to_string(),
            instance_id: self.instance_id.clone(),
            callsign: self.callsign.to_string(),
            band: self.band.map(|b| b.to_string()),
            mode: self.mode.to_string(),
            start: t(minutes),
            failed: self.failed,
            their_region: self.their_region.map(|r| r.to_string()),
            their_county: self.their_county.map(|c| c.to_string()),
            our_county: self.our_county.map(|c| c.to_string()),
        }
    }
}

fn class_of(snapshot: &ContestScoreSnapshot, id: &str) -> ContactClassification {
    snapshot
        .per_contact
        .iter()
        .find(|entry| entry.contact_id == id)
        .map(|entry| entry.classification.clone())
        .unwrap_or_else(|| panic!("contact {id} missing from per_contact"))
}

// --- Kansas: dupes, bands, modes, instances, normalization ---------------

#[test]
fn kansas_same_call_band_mode_repeats_are_dupes() {
    let context = kansas_tx_entrant();
    let w0 = FactBuilder::new(&context.instance_id, "K0KS");
    let facts = vec![
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            ..w0.clone()
        }
        .at(10, "a"),
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            ..w0
        }
        .at(30, "b"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 1);
    assert_eq!(snapshot.dupes, 1);
    assert_eq!(snapshot.score, 1);
    // Kansas has no multipliers: the set is empty and displayed as n/a,
    // never counted as one.
    assert!(!snapshot.multipliers_available);
    assert_eq!(snapshot.multiplier_count, 0);
}

#[test]
fn kansas_case_and_whitespace_normalize_but_editions_and_bands_differ() {
    let context = kansas_tx_entrant();
    let w9 = FactBuilder::new(&context.instance_id, "  k9xyz ");
    let other_instance = FactBuilder::new("KSQP-2027-OTHER", "K9XYZ");
    let facts = vec![
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            ..w9.clone()
        }
        .at(10, "a"),
        // Same station, different case/whitespace, another band: not a dupe.
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            band: Some("40M"),
            ..w9.clone()
        }
        .at(20, "b"),
        // FT4 versus FT8 are distinct in Kansas.
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            mode: "FT4",
            ..w9.clone()
        }
        .at(30, "c"),
        // Same normalized call/band/mode: dupe.
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            callsign: "K9XYZ",
            ..w9
        }
        .at(40, "d"),
        // Another edition/instance never affects this contest.
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            ..other_instance
        }
        .at(50, "e"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 3);
    assert_eq!(snapshot.dupes, 1);
    assert_eq!(snapshot.excluded, 1);
    assert!(matches!(
        class_of(&snapshot, "e"),
        ContactClassification::Excluded(ExclusionReason::WrongInstance)
    ));
}

#[test]
fn kansas_known_county_changes_are_distinct_and_unknown_collapses_the_group() {
    let context = kansas_tx_entrant();
    // Complete evidence: two Kansas counties of the same partner on one
    // band/mode are two contacts (documented county change).
    let mobile = FactBuilder::new(&context.instance_id, "K0MOB");
    let facts = vec![
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("JO"),
            ..mobile.clone()
        }
        .at(10, "a"),
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("WY"),
            ..mobile.clone()
        }
        .at(20, "b"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 2);
    assert_eq!(snapshot.dupes, 0);

    // Mixed known/unknown evidence collapses the whole group to ONE key, in
    // either input order, so an unknown plus a known key can never earn two
    // contacts.
    for (first_county, second_county) in [(None, Some("JO")), (Some("JO"), None)] {
        let facts = vec![
            FactBuilder {
                their_region: Some("KS"),
                their_county: first_county,
                ..mobile.clone()
            }
            .at(10, "a"),
            FactBuilder {
                their_region: Some("KS"),
                their_county: second_county,
                ..mobile.clone()
            }
            .at(20, "b"),
        ];
        let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
        assert_eq!(
            snapshot.scored, 1,
            "collapse must hold for {first_county:?}/{second_county:?}"
        );
        assert_eq!(snapshot.dupes, 1);
    }

    // A grid is never synthesized into a county: no region/county evidence
    // for a non-Kansas entrant leaves the group collapsed, never credited
    // twice.
    let no_evidence = vec![mobile.clone().at(10, "a"), mobile.clone().at(20, "b")];
    let snapshot = reduce_contest_score(&context, &no_evidence, t(60)).unwrap();
    assert_eq!(snapshot.pending, 2);
    assert_eq!(snapshot.scored, 0);
}

#[test]
fn kansas_outside_entrant_needs_explicit_kansas_evidence() {
    let context = kansas_tx_entrant();
    let facts = vec![
        FactBuilder {
            their_region: Some("OK"),
            ..FactBuilder::new(&context.instance_id, "W1OK")
        }
        .at(10, "a"),
        FactBuilder {
            ..FactBuilder::new(&context.instance_id, "W2ZZ")
        }
        .at(20, "b"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    // Known non-Kansas partner: excluded, not pending and not dupe.
    assert!(matches!(
        class_of(&snapshot, "a"),
        ContactClassification::Excluded(ExclusionReason::IneligiblePartnerLocation)
    ));
    // Unresolved partner location: pending.
    assert_eq!(class_of(&snapshot, "b"), ContactClassification::Pending);
    assert_eq!(snapshot.pending, 1);
    assert_eq!(snapshot.excluded, 1);
    assert_eq!(snapshot.scored, 0);
}

#[test]
fn kansas_entrant_works_everyone() {
    let context = kansas_ks_entrant();
    let facts = vec![
        FactBuilder {
            their_region: Some("TX"),
            ..FactBuilder::new(&context.instance_id, "K5TX")
        }
        .at(10, "a"),
        FactBuilder {
            their_region: Some("DX"),
            ..FactBuilder::new(&context.instance_id, "JA1DX")
        }
        .at(20, "b"),
        FactBuilder {
            ..FactBuilder::new(&context.instance_id, "W9???")
        }
        .at(30, "c"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    // A Kansas entrant works everyone, including an unresolved partner
    // location (no county evidence collapses the group at worst).
    assert_eq!(snapshot.scored, 3);
    assert_eq!(snapshot.pending, 0);
}

#[test]
fn kansas_excludes_failed_future_gap_mode_band_and_noncontest_records() {
    let context = kansas_tx_entrant();
    let ks = |call: &'static str| FactBuilder {
        their_region: Some("KS"),
        their_county: Some("SB"),
        ..FactBuilder::new(&context.instance_id, call)
    };
    let facts = vec![
        FactBuilder {
            failed: true,
            ..ks("K0FAI")
        }
        .at(10, "failed"),
        FactBuilder { ..ks("K0FUT") }.at(1000, "future-outside"), // in the gap between the two periods
        FactBuilder {
            mode: "FT8",
            band: Some("17M"),
            ..ks("K0BAD")
        }
        .at(10, "disallowed-band"),
        FactBuilder {
            mode: "SSB",
            ..ks("K0PHN")
        }
        .at(10, "unsupported-mode"),
        FactBuilder {
            ..FactBuilder::new("", "K0NOC")
        }
        .at(10, "noncontest"),
        // Half-open period boundaries: start at the end is outside, at the
        // start is inside.
        FactBuilder { ..ks("K0END") }.at(720, "at-end"),
        FactBuilder { ..ks("K0START") }.at(0, "at-start"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(730)).unwrap();
    for id in [
        "failed",
        "future-outside",
        "disallowed-band",
        "unsupported-mode",
        "noncontest",
    ] {
        assert!(
            matches!(class_of(&snapshot, id), ContactClassification::Excluded(_)),
            "{id} must be excluded"
        );
    }
    assert!(matches!(
        class_of(&snapshot, "at-end"),
        ContactClassification::Excluded(ExclusionReason::OutOfPeriod)
    ));
    assert!(matches!(
        class_of(&snapshot, "at-start"),
        ContactClassification::Scored { .. }
    ));
    assert_eq!(snapshot.scored, 1);
    assert_eq!(snapshot.excluded, 6);
}

// --- South Carolina ------------------------------------------------------

#[test]
fn scqp_digital_modes_share_one_dupe_group_per_band() {
    let context = scqp("SC", "SCQP-2026-K5ARH");
    let partner = |mode: &'static str, id: &'static str, mins: i64| {
        FactBuilder {
            mode,
            their_region: Some("NC"),
            ..FactBuilder::new(&context.instance_id, "W4NC")
        }
        .at(mins, id)
    };
    let facts = vec![
        partner("FT8", "a", 10),
        // Same band, FT4: the digital grouping combines them for dupes.
        partner("FT4", "b", 20),
        // Another band: independent.
        FactBuilder {
            mode: "FT8",
            band: Some("40M"),
            their_region: Some("NC"),
            ..FactBuilder::new(&context.instance_id, "W4NC")
        }
        .at(30, "c"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 2);
    assert_eq!(snapshot.dupes, 1);
}

#[test]
fn scqp_multipliers_are_band_scoped_and_county_changes_permit_another_contact() {
    let context = scqp("SC", "SCQP-2026-K5ARH");
    let sc_partner =
        |county: Option<&'static str>, band: Option<&'static str>, id: &'static str, mins: i64| {
            FactBuilder {
                their_county: county,
                band,
                their_region: Some("SC"),
                ..FactBuilder::new(&context.instance_id, "K1SC")
            }
            .at(mins, id)
        };
    let facts = vec![
        sc_partner(Some("GV"), Some("20M"), "a", 10),
        // Same county again on the same band: dupe, no new multiplier.
        sc_partner(Some("GV"), Some("20M"), "b", 20),
        // A documented new county permits another contact.
        sc_partner(Some("SP"), Some("20M"), "c", 30),
        // The same county on another band is independent and re-credits the
        // multiplier for that band.
        sc_partner(Some("GV"), Some("40M"), "d", 40),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 3);
    assert_eq!(snapshot.dupes, 1);
    assert_eq!(snapshot.multiplier_count, 5); // Three counties plus SC on both bands.
    assert_eq!(snapshot.score, (2 * 3) * 5);
}

#[test]
fn scqp_entrant_vs_outside_entrant_eligibility_and_mult_sets_differ() {
    let sc_entrant = scqp("SC", "SCQP-2026-SC");
    let outside = scqp("NC", "SCQP-2026-NC");
    let facts_for = |instance: &str| {
        vec![
            FactBuilder {
                their_region: Some("SC"),
                their_county: Some("GV"),
                ..FactBuilder::new(instance, "K1SC")
            }
            .at(10, "a"),
            FactBuilder {
                their_region: Some("NC"),
                ..FactBuilder::new(instance, "W4NC")
            }
            .at(20, "b"),
            FactBuilder {
                their_region: Some("DX"),
                ..FactBuilder::new(instance, "JA1DX")
            }
            .at(30, "c"),
        ]
    };
    // SC entrant: SC partner 2 points + county and SC state; NC partner 4 points +
    // state mult; DX 4 points, no mult.
    let sc = reduce_contest_score(&sc_entrant, &facts_for(&sc_entrant.instance_id), t(60)).unwrap();
    assert_eq!(sc.scored, 3);
    assert_eq!(sc.multiplier_count, 3); // County GV, state SC, state NC.
    assert_eq!(sc.score, (2 + 4 + 4) * 3);

    // Outside entrant: only SC partners score; NC and DX partners are
    // excluded and add no multipliers.
    let out = reduce_contest_score(&outside, &facts_for(&outside.instance_id), t(60)).unwrap();
    assert_eq!(out.scored, 1);
    assert_eq!(out.excluded, 2);
    assert_eq!(out.multiplier_count, 1); // 20M:county:GV only
    assert_eq!(out.score, 2 * 1);
}

#[test]
fn scqp_dc_canada_and_dx_partners_and_invalid_combinations() {
    let context = scqp("SC", "SCQP-2026-SC");
    let partner = |call: &'static str, region: Option<&'static str>, id: &'static str| {
        FactBuilder {
            their_region: region,
            ..FactBuilder::new(&context.instance_id, call)
        }
        .at(10, id)
    };
    let facts = vec![
        partner("W1DC", Some("DC"), "dc"),
        partner("VE3ON", Some("ON"), "on"),
        partner("VYTTY", Some("YT"), "yt"),
        partner("JA1DX", Some("DX"), "dx"),
        partner("W9ZZZ", Some("ZZ"), "bad-region"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    // DC, ON, YT all score (4 points + a per-band state/province mult each);
    // DX earns points but no multiplier; an invalid region combination is
    // pending, not excluded.
    assert_eq!(snapshot.scored, 4);
    assert_eq!(snapshot.pending, 1);
    assert_eq!(snapshot.multiplier_count, 3); // DC, ON, YT
    assert_eq!(snapshot.score, (4 + 4 + 4 + 4) * 3);
}

#[test]
fn scqp_the_plan_fixture_scores_twelve_points_partial() {
    // Three scored outside-SC-entrant contacts (SC partners, 2 points
    // each), two distinct counties on one band, no bonus, one dupe, two
    // pending: 6 × 2 + 0 = 12 claimed points, partial.
    let context = scqp("NC", "SCQP-2026-NC");
    let sc = |call: &'static str, county: Option<&'static str>, id: &'static str, mins: i64| {
        FactBuilder {
            their_region: Some("SC"),
            their_county: county,
            ..FactBuilder::new(&context.instance_id, call)
        }
        .at(mins, id)
    };
    let facts = vec![
        sc("W1AAA", Some("GV"), "a", 10),
        sc("W2BBB", Some("SP"), "b", 20),
        sc("W3CCC", Some("GV"), "c", 30),
        // Same partner, same band, same county: the contest dupe.
        sc("W1AAA", Some("GV"), "d", 40),
        // Grid-only SC contact: pending (county never inferred).
        sc("W4DDD", None, "e", 50),
        // Unresolved partner location: pending.
        FactBuilder {
            ..FactBuilder::new(&context.instance_id, "W5EEE")
        }
        .at(60, "f"),
    ];
    let snapshot = reduce_contest_score(&context, &facts, t(90)).unwrap();
    assert_eq!(snapshot.scored, 3);
    assert_eq!(snapshot.dupes, 1);
    assert_eq!(snapshot.pending, 2);
    assert_eq!(snapshot.excluded, 0);
    assert_eq!(snapshot.multiplier_count, 2);
    assert_eq!(snapshot.score, 12);
    assert!(snapshot.partial);
}

// --- Determinism, rate, and the coordinator dupe lookup -------------------

#[test]
fn replay_out_of_order_and_ties_reduce_identically_and_deletion_recomputes() {
    let context = kansas_tx_entrant();
    let w0 = |id: &'static str, mins: i64| {
        FactBuilder {
            their_region: Some("KS"),
            their_county: Some("SB"),
            ..FactBuilder::new(&context.instance_id, "K0AAA")
        }
        .at(mins, id)
    };
    let facts = vec![w0("a", 10), w0("b", 30)];
    let baseline = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(baseline.scored, 1);
    assert_eq!(baseline.dupes, 1);

    // Replay duplication: the same contact id delivered twice counts once.
    let replayed = vec![w0("a", 10), w0("a", 10), w0("b", 30)];
    assert_eq!(
        reduce_contest_score(&context, &replayed, t(60)).unwrap(),
        baseline
    );

    // Out-of-order input with a stable timestamp tie (same start, distinct
    // ids) is ordered by contact id: the same two contacts classify the
    // same way regardless of delivery order.
    let tie_a = FactBuilder {
        their_region: Some("KS"),
        their_county: Some("SB"),
        ..FactBuilder::new(&context.instance_id, "K0TIE")
    }
    .at(45, "x1");
    let mut tie_b = tie_a.clone();
    tie_b.contact_id = "x2".to_string();
    let ordered = vec![
        facts[0].clone(),
        tie_a.clone(),
        tie_b.clone(),
        facts[1].clone(),
    ];
    let shuffled = vec![tie_b, facts[1].clone(), facts[0].clone(), tie_a];
    assert_eq!(
        reduce_contest_score(&context, &ordered, t(60)).unwrap(),
        reduce_contest_score(&context, &shuffled, t(60)).unwrap()
    );

    // Deleted / import-undone contacts recompute from the remaining facts.
    let mut corrected = facts.clone();
    corrected[1].their_county = Some("WY".to_string()); // corrected evidence
    let snapshot = reduce_contest_score(&context, &corrected, t(60)).unwrap();
    assert_eq!(snapshot.scored, 2); // documented county change
    let remaining = vec![facts[0].clone()];
    assert_eq!(
        reduce_contest_score(&context, &remaining, t(60))
            .unwrap()
            .scored,
        1
    );
}

#[test]
fn rate_uses_exact_trailing_hour_decays_and_freezes_at_final_end() {
    let context = kansas_ks_entrant();
    let fact = |call: &'static str, band: &'static str, id: &'static str, mins: i64| {
        FactBuilder {
            band: Some(band),
            their_region: Some("TX"),
            ..FactBuilder::new(&context.instance_id, call)
        }
        .at(mins, id)
    };
    // Distinct calls/bands so every fact is scored; starts at minutes
    // 10, 15, 70, 100, 650.
    let facts = vec![
        fact("K5A", "20M", "a", 10),
        fact("K5B", "40M", "b", 15),
        fact("K5C", "80M", "c", 70),
        fact("K5D", "20M", "d", 100),
        fact("K5E", "40M", "e", 650),
    ];
    // Snapshot at minute 70: window (10, 70]. The contact starting exactly
    // at minute 10 is OUTSIDE the exclusive lower bound; 15 and 70 are in.
    let snapshot = reduce_contest_score(&context, &facts, t(70)).unwrap();
    assert_eq!(snapshot.rate_per_hour, 2);

    // Decay: the same ledger an hour later keeps only minute 100.
    let snapshot = reduce_contest_score(&context, &facts, t(130)).unwrap();
    assert_eq!(snapshot.rate_per_hour, 1);

    // Freeze: long after the final interval end (minute 720) the rate
    // window ends at the final end, so only (660, 720] would count — none
    // are, and the frozen rate is 0.
    let snapshot = reduce_contest_score(&context, &facts, t(10_000)).unwrap();
    assert_eq!(snapshot.rate_per_hour, 0);

    // Before the contest starts the rate is zero without extrapolation.
    let empty = reduce_contest_score(&context, &facts, t(-30)).unwrap();
    assert_eq!(empty.rate_per_hour, 0);
}

#[test]
fn contest_dupe_status_marks_without_refusing() {
    let context = kansas_tx_entrant();
    let existing = FactBuilder {
        their_region: Some("KS"),
        their_county: Some("SB"),
        ..FactBuilder::new(&context.instance_id, "K0AAA")
    }
    .at(10, "a");
    let candidate_dupe = FactBuilder {
        their_region: Some("KS"),
        their_county: Some("SB"),
        ..FactBuilder::new(&context.instance_id, "K0AAA")
    }
    .at(50, "cand-1");
    let candidate_new_county = FactBuilder {
        their_region: Some("KS"),
        their_county: Some("WY"),
        ..FactBuilder::new(&context.instance_id, "K0AAA")
    }
    .at(55, "cand-2");
    let facts = vec![existing];
    assert_eq!(
        contest_dupe_status(&context, &facts, &candidate_dupe),
        ContactClassification::Dupe
    );
    assert!(matches!(
        contest_dupe_status(&context, &facts, &candidate_new_county),
        ContactClassification::Scored { .. }
    ));
}

#[test]
fn checked_arithmetic_path_holds_for_large_ledgers() {
    let context = scqp("SC", "SCQP-2026-SC");
    let mut facts = Vec::new();
    for index in 0..64 {
        // `C00`…`C63`: distinct 3-character county codes so every contact
        // claims a distinct dupe key and multiplier.
        let county: &'static str = Box::leak(format!("C{index:02}").into_boxed_str());
        facts.push(
            FactBuilder {
                their_region: Some("SC"),
                their_county: Some(county),
                ..FactBuilder::new(&context.instance_id, "K1SC")
            }
            .at(10 + index, Box::leak(format!("c{index}").into_boxed_str())),
        );
    }
    // 64 scored SC contacts: 128 points × 65 multiplier keys including SC — the checked
    // u64 path yields the exact formula rather than wrapping.
    let snapshot = reduce_contest_score(&context, &facts, t(600)).unwrap();
    assert_eq!(snapshot.scored, 64);
    assert_eq!(snapshot.multiplier_count, 65);
    assert_eq!(snapshot.score, 128 * 65);
}

#[test]
fn future_contact_cannot_score_or_collapse_current_counties() {
    let context = kansas_tx_entrant();
    let builder = FactBuilder {
        their_region: Some("KS"),
        their_county: Some("SB"),
        ..FactBuilder::new(&context.instance_id, "K0KS")
    };
    let mut second = builder.at(20, "b");
    second.their_county = Some("DG".into());
    let mut future = builder.at(120, "future");
    future.their_county = None;
    let snapshot =
        reduce_contest_score(&context, &[builder.at(10, "a"), second, future], t(60)).unwrap();
    assert_eq!(snapshot.scored, 2);
    assert_eq!(snapshot.dupes, 0);
    assert_eq!(snapshot.excluded, 1);
}

#[test]
fn published_sc_bonus_stations_credit_once_per_band_digital_group() {
    let context = scqp("TX", "SC-test");
    let mut facts = Vec::new();
    for (i, call) in ["W4CAE", "WW4SF", "K4YTZ"].into_iter().enumerate() {
        let b = FactBuilder {
            their_region: Some("SC"),
            their_county: Some("GVIL"),
            ..FactBuilder::new(&context.instance_id, call)
        };
        facts.push(b.at(10, &format!("{i}-a")));
        let mut repeat = b.at(20, &format!("{i}-b"));
        repeat.mode = "FT4".into();
        repeat.their_county = Some("CHAR".into());
        facts.push(repeat);
        let mut other_band = b.at(30, &format!("{i}-c"));
        other_band.band = Some("40M".into());
        facts.push(other_band);
    }
    let snapshot = reduce_contest_score(&context, &facts, t(60)).unwrap();
    assert_eq!(snapshot.scored, 9);
    assert_eq!(snapshot.multiplier_count, 3);
    assert_eq!(snapshot.score, 18 * 3 + 2 * (350 + 250 + 250));
}

#[test]
fn sc_entrant_gets_county_and_sc_state_per_band() {
    for (region, expected) in [("SC", 2), ("TX", 1)] {
        let context = scqp(region, "SC-test");
        let b = FactBuilder {
            their_region: Some("SC"),
            their_county: Some("GVIL"),
            ..FactBuilder::new(&context.instance_id, "K0SC")
        };
        let snapshot = reduce_contest_score(&context, &[b.at(10, "a")], t(60)).unwrap();
        assert_eq!(snapshot.multiplier_count, expected);
        assert_eq!(snapshot.score, 2 * expected);
    }
}
