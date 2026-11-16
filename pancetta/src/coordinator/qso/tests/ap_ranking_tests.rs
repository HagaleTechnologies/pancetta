use super::*;
use std::collections::HashSet;

/// Minimal `WorkedStationLookup` fixture — mirrors the one in
/// `pancetta-qso/src/priority.rs`'s own tests and `tui_relay.rs`'s
/// `priority_score_bucket_boundaries_are_exact` test, giving real
/// differentiation between an ATNO/needed-DXCC candidate and a plain
/// worked-before one.
struct TestLookup {
    needed_dxcc: HashSet<String>,
    atno: HashSet<String>,
    duplicates: HashSet<String>,
}

impl pancetta_qso::priority::WorkedStationLookup for TestLookup {
    fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.duplicates.contains(&callsign.to_uppercase())
    }
    fn is_recent_failure(&self, _callsign: &str) -> bool {
        false
    }
    fn is_needed_dxcc(&self, callsign: &str) -> bool {
        self.needed_dxcc.contains(&callsign.to_uppercase())
    }
    fn is_needed_grid(&self, _grid: &str) -> bool {
        false
    }
    fn is_dxcc_needed_on_band(&self, callsign: &str, _freq_hz: f64) -> bool {
        self.needed_dxcc.contains(&callsign.to_uppercase())
    }
    fn is_grid_needed_on_band(&self, _grid: &str, _freq_hz: f64) -> bool {
        false
    }
    fn is_atno(&self, callsign: &str) -> bool {
        self.atno.contains(&callsign.to_uppercase())
    }
    fn is_notable(&self, _callsign: &str) -> bool {
        false
    }
    fn rarity(&self, _callsign: &str) -> f64 {
        0.0
    }
    fn network_last_seen(&self, _callsign: &str) -> Option<i64> {
        None
    }
    fn network_snr(&self, _callsign: &str) -> Option<(u32, i32)> {
        None
    }
}

fn scorer_with(
    needed_dxcc: &[&str],
    atno: &[&str],
    duplicates: &[&str],
) -> pancetta_qso::PriorityScorer {
    let lookup = TestLookup {
        needed_dxcc: needed_dxcc.iter().map(|s| s.to_uppercase()).collect(),
        atno: atno.iter().map(|s| s.to_uppercase()).collect(),
        duplicates: duplicates.iter().map(|s| s.to_uppercase()).collect(),
    };
    pancetta_qso::PriorityScorer::new(
        pancetta_qso::priority::PriorityWeights::default(),
        Box::new(lookup),
    )
}

fn candidate(callsign: &str, snr: i8) -> QsoApCandidate {
    QsoApCandidate {
        callsign: callsign.to_string(),
        grid: None,
        snr,
        freq_hz: 14_074_000.0,
        progress: pancetta_ft8::QsoApProgress::WaitingForReport,
        expected_texts: vec![],
    }
}

// RED (pre-implementation): this test fails against the OLD single-QSO
// `Option<QsoAp>` design because there was no ranking function at all —
// `rank_active_qsos_for_ap` didn't exist. GREEN once implemented above.
#[test]
fn ranks_atno_above_worked_before_regardless_of_snr_order() {
    // K5AAA is a plain, already-worked station with a strong signal;
    // JA1ABC is an ATNO (needed + never-worked-anywhere) with a much
    // weaker signal. Priority ranking must still put the ATNO first —
    // a raw-SNR sort would get this backwards.
    let scorer = scorer_with(&["JA1ABC"], &["JA1ABC"], &["K5AAA"]);
    let candidates = vec![candidate("K5AAA", -5), candidate("JA1ABC", -20)];

    let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 4);

    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].their_call, "JA1ABC", "ATNO must rank first");
    assert_eq!(ranked[1].their_call, "K5AAA");
}

#[test]
fn caps_to_max_qsos_keeping_the_highest_scored() {
    let scorer = scorer_with(&["JA1ABC"], &["JA1ABC"], &[]);
    let candidates = vec![
        candidate("W1AAA", -10),
        candidate("W1BBB", -10),
        candidate("JA1ABC", -25),
        candidate("W1CCC", -10),
    ];

    let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 2);

    assert_eq!(ranked.len(), 2);
    assert_eq!(
        ranked[0].their_call, "JA1ABC",
        "ATNO still wins despite weak SNR"
    );
}

#[test]
fn empty_candidates_yields_empty_ranking() {
    let scorer = scorer_with(&[], &[], &[]);
    assert!(rank_active_qsos_for_ap(&[], &scorer, 4).is_empty());
}

#[test]
fn each_ranked_qso_carries_its_a8_expected_texts() {
    let scorer = scorer_with(&[], &[], &[]);
    let candidates = vec![candidate("W1AW", -10)];

    let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 4);

    assert_eq!(ranked.len(), 1);
    // candidate() defaults to WaitingForReport with no pre-populated
    // expected_texts in the fixture — this test's contract is really
    // about qso_ap_candidate_from_progress populating them from real
    // state, exercised below.
    assert_eq!(
        ranked[0].progress,
        pancetta_ft8::QsoApProgress::WaitingForReport
    );
}

fn waiting_for_report_state(their_callsign: &str) -> pancetta_qso::QsoState {
    pancetta_qso::QsoState::WaitingForReport {
        their_callsign: their_callsign.to_string(),
        frequency: 14_074_000.0,
        started_at: chrono::Utc::now(),
        their_grid: Some("FN31".to_string()),
        our_report: -12,
    }
}

fn empty_metadata(qso_id: pancetta_qso::QsoId, our_callsign: &str) -> pancetta_qso::QsoMetadata {
    let now = chrono::Utc::now();
    pancetta_qso::QsoMetadata {
        qso_id,
        our_callsign: our_callsign.to_string(),
        their_callsign: None,
        frequency: 14_074_000.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".to_string(),
        first_answer_at: None,
        start_time: now,
        end_time: None,
        reports: pancetta_qso::states::SignalReports::default(),
        grids: pancetta_qso::states::GridSquares::default(),
        their_state: None,
        contest_info: None,
        tags: std::collections::HashMap::new(),
        adif_field_types: std::collections::HashMap::new(),
        notes: None,
        tx_parity: None,
        tx_parity_provisional: false,
        initiated_by: pancetta_qso::states::CallInitiation::Auto,
        role: pancetta_qso::states::QsoRole::Caller,
        call_count: 1,
        first_call_at: Some(now),
        last_call_at: Some(now),
        progressed_this_cycle: false,
        stall_cycles: 0,
        last_known_good_offset_hz: None,
        advance_generation: 0,
        pre_switch_offset: None,
        hound: false,
        partner_freq: None,
        pending_freq_drift: None,
        hound_qsyed: false,
        remote_origin: false,
        remote_client_key_id: None,
    }
}

#[test]
fn qso_ap_candidate_from_progress_extracts_grid_snr_freq_from_live_state() {
    let state = waiting_for_report_state("W1AW");
    let metadata = empty_metadata(pancetta_qso::QsoId::new_v4(), "K5ARH");

    let candidate = qso_ap_candidate_from_progress(&state, &metadata, "K5ARH")
        .expect("WaitingForReport must produce a candidate");

    assert_eq!(candidate.callsign, "W1AW");
    assert_eq!(candidate.grid.as_deref(), Some("FN31"));
    assert_eq!(candidate.snr, -12);
    assert_eq!(candidate.freq_hz, 14_074_000.0);
    assert_eq!(
        candidate.progress,
        pancetta_ft8::QsoApProgress::WaitingForReport
    );
    assert!(
        !candidate.expected_texts.is_empty(),
        "a8 expected-texts must be populated, same as the pre-Task-5 single-QSO write site"
    );
}

#[test]
fn qso_ap_candidate_from_progress_returns_none_for_calling_cq_and_idle() {
    let calling_cq = pancetta_qso::QsoState::CallingCq {
        frequency: 14_074_000.0,
        started_at: chrono::Utc::now(),
        call_count: 1,
    };
    let metadata = empty_metadata(pancetta_qso::QsoId::new_v4(), "K5ARH");
    assert!(qso_ap_candidate_from_progress(&calling_cq, &metadata, "K5ARH").is_none());
    assert!(
        qso_ap_candidate_from_progress(&pancetta_qso::QsoState::Idle, &metadata, "K5ARH").is_none()
    );
}
