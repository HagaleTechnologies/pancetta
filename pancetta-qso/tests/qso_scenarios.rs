//! Permanent real-world QSO scenario catalog, built on the durable
//! [`pancetta_qso::sim`] harness (virtual band + virtual clock).
//!
//! Each test is a focused, named on-air situation — including the weird ones we
//! actually hit (RR73 repeats, RRR closes, re-call/double-Space, fade-out) plus
//! weak/crowded/collision band conditions and the security invariants
//! (sender-mismatch rejection, no duplicate QSOs). The harness drives the REAL
//! `QsoManager` (and the real `SmartFrequencyAllocator`); these tests assert the
//! resulting `Timeline`.
//!
//! Run: `cargo test -p pancetta-qso --test qso_scenarios`.
//!
//! Authoring notes for future scenarios:
//! - `Sim::call_station` / `Sim::cq` / `Sim::respond_to_caller` are the operator
//!   side; `Sim::inject_decode` is the DX side; `Sim::tick` advances one slot.
//! - The engine auto-sequences replies only for **manual** QSOs (the operator
//!   path the TUI uses), so all "we reply automatically" scenarios open via the
//!   manual entry points — exactly matching production operator behavior.
//! - Two scenarios (DX-busy and duplicate-suppression) live in the autonomous
//!   layer, which tracks its busy/worked sets on `std::time::Instant` rather
//!   than the QSO engine's injectable clock; those are exercised directly
//!   against `AutonomousOperator` and noted as autonomous-only.

use pancetta_core::ResponseStep;
use pancetta_qso::sim::{assert_tx_offset_clear_of, Sim};
use pancetta_qso::{QsoFailureReason, QsoManagerConfig, TimeoutConfig};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

/// Build a default harness for our station.
async fn sim() -> Sim {
    Sim::new(US, Some(GRID)).await
}

// =====================================================================
// 1. Clean answer — we call a CQer; full CQ→report→R→RR73→73 to Completed.
// =====================================================================
#[tokio::test]
async fn scenario_01_clean_answer_we_call_a_cqer() {
    let mut sim = sim().await;

    // Slot 0: DX is calling CQ; operator clicks to call them (manual).
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // our grid goes out: "VB7F K5ARH EM10"

    // Slot 1: DX sends us a report → engine auto-sends our R-report.
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slot 2: DX rogers with RR73 → engine completes and auto-sends our 73.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("VB7F K5ARH EM10");
    tl.assert_transmitted_contains("VB7F K5ARH 73");
    tl.assert_completed_with("VB7F");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 2. Clean CQ — we CQ (manual); a station answers; through to Completed.
// =====================================================================
#[tokio::test]
async fn scenario_02_clean_cq_we_are_the_cqer() {
    let mut sim = sim().await;

    // Slot 0: operator presses `c` → manual CQ.
    sim.cq(FREQ).await;
    sim.tick().await; // "CQ K5ARH EM10"

    // Slot 1: a station answers our CQ → we auto-send our report.
    sim.inject_decode("K5ARH W5XO EM12", FREQ, -10.0, 0.1);
    sim.tick().await;

    // Slot 2: they roger our report (R-report) → we auto-send RR73.
    sim.inject_decode("K5ARH W5XO R-09", FREQ, -10.0, 0.1);
    sim.tick().await;

    // Slot 3: they close with 73 → we complete + log.
    sim.inject_decode("K5ARH W5XO 73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("CQ K5ARH EM10");
    tl.assert_completed_with("W5XO");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 3. RR73 repeat [VB7F] — after completion the DX keeps sending RR73;
//    bounded auto-73 re-sends fire (engine handles within the QSO).
// =====================================================================
#[tokio::test]
async fn scenario_03_rr73_repeat_bounded_resend() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // DX closes; then keeps repeating RR73 for several more slots.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    for _ in 0..4 {
        sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
        sim.tick().await;
    }

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    // We do send our closing 73, but it must be BOUNDED — a stuck DX can't make
    // us transmit unboundedly. (The coordinator caps post-completion re-sends at
    // 3; the engine's own per-QSO close sends our 73 once on completion. Either
    // way the count stays small.)
    let seventy_threes = tl.count_transmitted_containing("VB7F K5ARH 73");
    assert!(
        (1..=6).contains(&seventy_threes),
        "expected a bounded number of closing 73s (1..=6), got {seventy_threes}\n{tl}"
    );
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 4. RRR close [K9HJZ] — DX sends RRR (not RR73); recognized as close.
// =====================================================================
#[tokio::test]
async fn scenario_04_rrr_close() {
    let mut sim = sim().await;

    sim.inject_decode("CQ K9HJZ EN52", FREQ, -7.0, 0.1);
    sim.call_station("K9HJZ", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH K9HJZ -05", FREQ, -7.0, 0.1);
    sim.tick().await;

    // RRR (plain roger) must close the QSO exactly like RR73.
    sim.inject_decode("K5ARH K9HJZ RRR", FREQ, -7.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("K9HJZ");
    tl.assert_transmitted_contains("K9HJZ K5ARH 73");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 5. Bare 73 close — DX closes with plain "73".
// =====================================================================
#[tokio::test]
async fn scenario_05_bare_73_close() {
    let mut sim = sim().await;

    sim.inject_decode("CQ N5XYZ EM00", FREQ, -9.0, 0.1);
    sim.call_station("N5XYZ", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH N5XYZ -15", FREQ, -9.0, 0.1);
    sim.tick().await;

    // DX skips RR73 and closes directly with a bare 73.
    sim.inject_decode("K5ARH N5XYZ 73", FREQ, -9.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("N5XYZ");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 6. State regression — mid-QSO the DX re-sends an EARLIER-stage message;
//    we back up and re-send (manual only).
// =====================================================================
#[tokio::test]
async fn scenario_06_state_regression_dx_resends_earlier() {
    let mut sim = sim().await;

    sim.inject_decode("CQ W7ABC CN87", FREQ, -6.0, 0.1);
    sim.call_station("W7ABC", FREQ).await;
    sim.tick().await; // we sent grid

    // DX sends report → we go to SendingReport, auto-send R-report.
    sim.inject_decode("K5ARH W7ABC -10", FREQ, -6.0, 0.1);
    sim.tick().await;

    // DX rogers → we go to WaitingForConfirmation, send RR73.
    sim.inject_decode("K5ARH W7ABC R-10", FREQ, -6.0, 0.1);
    sim.tick().await;

    // REGRESSION: DX never copied our RR73 and re-sends their SignalReport.
    // The manual state machine backs us up to SendingReport and re-sends R.
    sim.inject_decode("K5ARH W7ABC -10", FREQ, -6.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must have re-sent our R-report after the regression (≥2 R-reports total:
    // the original ack and the post-regression re-send).
    let r_reports = tl.count_transmitted_containing("W7ABC K5ARH R");
    assert!(
        r_reports >= 2,
        "expected ≥2 R-report transmissions across the regression, got {r_reports}\n{tl}"
    );
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 7. Re-call / double-Space [W5XO] — calling an already-active station
//    again CONTINUES the same QSO (one id, no Superseded).
// =====================================================================
#[tokio::test]
async fn scenario_07_recall_continues_same_qso() {
    let mut sim = sim().await;

    sim.inject_decode("CQ W5XO EM12", FREQ, -8.0, 0.1);
    let id1 = sim.call_station("W5XO", FREQ).await;
    sim.tick().await;

    // Operator mashes call again on the same station mid-QSO.
    let id2 = sim.call_station("W5XO", FREQ).await;
    sim.tick().await;
    let id3 = sim.call_station("W5XO", FREQ).await;
    sim.tick().await;

    assert_eq!(id1, id2, "re-call must return the same QSO id");
    assert_eq!(id1, id3, "re-call must return the same QSO id");

    let tl = sim.into_timeline();
    tl.assert_no_duplicate_qsos();
    tl.assert_no_superseded();
}

// =====================================================================
// 8. DX busy with third party — autonomous operator recognizes busy
//    (no auto-pounce). AUTONOMOUS-ONLY: the busy set lives in
//    AutonomousOperator and is keyed on std::time::Instant.
// =====================================================================
#[tokio::test]
async fn scenario_08_dx_busy_with_third_party_autonomous() {
    use pancetta_qso::{AutonomousConfig, AutonomousOperator, DecodedMessageInfo, NullDxEvaluator};

    let mut op = AutonomousOperator::new(
        AutonomousConfig::default(),
        US.to_string(),
        Some(GRID.to_string()),
    );

    // Feed a third-party exchange NOT directed at us: DX is mid-QSO with W9ZZZ.
    // (report from VB7F to W9ZZZ → VB7F is "busy".)
    let busy_now = std::time::Instant::now();
    op.feed_decoded_messages(
        &[DecodedMessageInfo {
            message_text: "W9ZZZ VB7F -12".to_string(),
            callsign: Some("VB7F".to_string()),
            snr: -12,
            frequency_hz: FREQ,
            slot_parity: None,
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }],
        &NullDxEvaluator,
    );

    assert!(
        op.is_dx_busy("VB7F", busy_now),
        "VB7F mid-exchange with a third party must be observable as busy"
    );
    assert!(
        !op.is_dx_busy("K9HJZ", busy_now),
        "an unrelated station must not be marked busy"
    );
}

// =====================================================================
// 9. Context-aware reply — DX directs a report at us → reply picks R;
//    DX directs RR73 → reply picks 73.
// =====================================================================
#[tokio::test]
async fn scenario_09_context_aware_reply_report_then_rr73() {
    // 9a: a station calls US with a report → we open at ReportAck → send R.
    let mut sim_a = sim().await;
    sim_a.inject_decode("K5ARH KA1BCD -07", FREQ, -7.0, 0.1);
    sim_a
        .respond_to_caller(
            "KA1BCD",
            FREQ,
            ResponseStep::ReportAck,
            Some(-7.0),
            Some(-7),
        )
        .await;
    sim_a.tick().await;
    let tl_a = sim_a.into_timeline();
    tl_a.assert_transmitted_contains("KA1BCD K5ARH R");

    // 9b: a station calls US with RR73 → we open at SeventyThree → send 73,
    //     completing immediately.
    let mut sim_b = sim().await;
    sim_b.inject_decode("K5ARH KA1BCD RR73", FREQ, -7.0, 0.1);
    sim_b
        .respond_to_caller(
            "KA1BCD",
            FREQ,
            ResponseStep::SeventyThree,
            Some(-7.0),
            Some(-7),
        )
        .await;
    sim_b.tick().await;
    let tl_b = sim_b.into_timeline();
    tl_b.assert_transmitted_contains("KA1BCD K5ARH 73");
    tl_b.assert_completed_with("KA1BCD");
}

// =====================================================================
// 10. Weak / intermittent — DX decodes only every other slot; QSO still
//     completes over more slots via keep-call.
// =====================================================================
#[tokio::test]
async fn scenario_10_weak_intermittent_still_completes() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -20.0, 0.4);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // we send grid

    // DX drops this slot (fade) — we keep-call our grid.
    sim.tick().await;

    // DX reappears, weak, with a report → we send R.
    sim.inject_decode("K5ARH VB7F -22", FREQ, -22.0, 0.5);
    sim.tick().await;

    // DX drops again — we keep-call our R-report.
    sim.tick().await;

    // DX reappears and rogers with RR73 → we complete.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -21.0, 0.5);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    // Keep-calling means our current outbound is re-sent across the silent
    // (faded) slots. Once we're waiting on the DX's R-ack in SendingReport, the
    // re-arm re-sends our R-report each idle slot — that is the keep-call that
    // bridges the fade to completion.
    let r_reports = tl.count_transmitted_containing("VB7F K5ARH R");
    assert!(
        r_reports >= 2,
        "expected keep-call to re-send our R-report across fades (≥2), got {r_reports}\n{tl}"
    );
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 11. Fade-out mid-QSO — DX vanishes after its report; keep-call retries
//     until the watchdog retires it as Failed{Timeout}.
// =====================================================================
#[tokio::test]
async fn scenario_11_fade_out_retired_by_watchdog() {
    // Tight watchdog so the test runs in a few fast slots: 4 calls max.
    let mut sim = Sim::with_config(QsoManagerConfig {
        our_callsign: US.to_string(),
        our_grid: Some(GRID.to_string()),
        timeouts: TimeoutConfig {
            manual_call_max_calls: 4,
            manual_call_watchdog_minutes: 60, // make the call-count bound bind first
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await; // we move to SendingReport, send R

    // DX vanishes forever. Keep-call re-sends our R each slot until the
    // 4-call cap retires the QSO.
    sim.tick_n(8).await;

    let tl = sim.into_timeline();
    tl.assert_failed_with(QsoFailureReason::Timeout);
    tl.assert_not_completed_with("VB7F");
}

// =====================================================================
// 12. Watchdog cap — an unanswered manual call stops after the call cap
//     and goes terminal.
// =====================================================================
#[tokio::test]
async fn scenario_12_watchdog_cap_unanswered_call() {
    let mut sim = Sim::with_config(QsoManagerConfig {
        our_callsign: US.to_string(),
        our_grid: Some(GRID.to_string()),
        timeouts: TimeoutConfig {
            manual_call_max_calls: 3,
            manual_call_watchdog_minutes: 60,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;

    // Call a station that never answers at all.
    sim.inject_decode("CQ DX0NONE AA00", FREQ, -8.0, 0.1);
    sim.call_station("DX0NONE", FREQ).await;
    sim.tick().await;

    // Tick out the cap with no DX activity.
    sim.tick_n(8).await;

    let tl = sim.into_timeline();
    tl.assert_failed_with(QsoFailureReason::Timeout);
    // We never completed and our re-calls were bounded by the cap (3): the
    // opening call + at most (cap-1) re-arms ⇒ a small number of grid sends.
    let calls = tl.count_transmitted_containing("DX0NONE K5ARH EM10");
    assert!(
        calls <= 3,
        "expected at most the call cap (3) grid transmissions, got {calls}\n{tl}"
    );
}

// =====================================================================
// 13. Crowded band TX-freq selection — dense activity across the
//     passband → allocator picks a clear offset.
// =====================================================================
#[tokio::test]
async fn scenario_13_crowded_band_picks_clear_offset() {
    let mut sim = sim().await;

    // Crowd the low and mid passband heavily; leave the high end clear.
    let crowd: Vec<(&str, f64)> = vec![
        ("CQ A1AA AA00", 700.0),
        ("CQ B2BB BB11", 900.0),
        ("CQ C3CC CC22", 1100.0),
        ("CQ D4DD DD33", 1300.0),
        ("CQ E5EE EE44", 1500.0),
        ("CQ F6FF FF55", 1700.0),
    ];
    sim.inject_crowd(&crowd);
    sim.tick().await;

    // Choose a TX offset; it must avoid the crowded mid (e.g. 1500 Hz).
    let chosen = sim.choose_tx_offset(None).await;
    assert_tx_offset_clear_of(chosen, 1500.0, 50.0);
    assert_tx_offset_clear_of(chosen, 1100.0, 50.0);
}

// =====================================================================
// 14. TX-freq shift — our chosen spot becomes occupied next cycle → the
//     allocator moves us to a still-clear offset.
// =====================================================================
#[tokio::test]
async fn scenario_14_tx_freq_shift_when_spot_occupied() {
    let mut sim = sim().await;

    // Cycle 1: clean band → pick something near center.
    let first = sim.choose_tx_offset(None).await;

    // Cycle 2: a strong station lands right on our chosen offset.
    sim.band_mut().occupy(first, 40.0, 0.9);
    let second = sim.choose_tx_offset(None).await;

    // The allocator must move us off the now-occupied spot.
    assert!(
        (second - first).abs() >= 50.0,
        "expected allocator to shift TX ≥50 Hz off the newly occupied {first:.0} Hz, \
         got {second:.0} Hz"
    );
    assert_tx_offset_clear_of(second, first, 50.0);
}

// =====================================================================
// 15. Two simultaneous QSOs on different offsets proceed independently.
// =====================================================================
#[tokio::test]
async fn scenario_15_two_simultaneous_qsos_independent() {
    let mut sim = sim().await;
    let f1 = 800.0;
    let f2 = 1900.0;

    // Open two manual QSOs on well-separated offsets.
    sim.inject_decode("CQ VB7F DO33", f1, -8.0, 0.1);
    sim.inject_decode("CQ W5XO EM12", f2, -8.0, 0.1);
    sim.call_station("VB7F", f1).await;
    sim.call_station("W5XO", f2).await;
    sim.tick().await;

    // Both get reports in the same slot, on their own offsets.
    sim.inject_decode("K5ARH VB7F -12", f1, -8.0, 0.1);
    sim.inject_decode("K5ARH W5XO -09", f2, -8.0, 0.1);
    sim.tick().await;

    // Both close.
    sim.inject_decode("K5ARH VB7F RR73", f1, -8.0, 0.1);
    sim.inject_decode("K5ARH W5XO RR73", f2, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    tl.assert_completed_with("W5XO");
    // Exactly two distinct QSOs.
    tl.assert_at_most_qsos(2);
    assert_eq!(
        tl.distinct_qso_ids().len(),
        2,
        "expected exactly two distinct QSOs\n{tl}"
    );
    // Our TX for each rode the correct offset.
    assert!(
        tl.transmissions
            .iter()
            .any(|t| t.text.contains("VB7F K5ARH") && (t.freq_hz - f1).abs() < 1.0),
        "VB7F TX must ride {f1} Hz\n{tl}"
    );
    assert!(
        tl.transmissions
            .iter()
            .any(|t| t.text.contains("W5XO K5ARH") && (t.freq_hz - f2).abs() < 1.0),
        "W5XO TX must ride {f2} Hz\n{tl}"
    );
}

// =====================================================================
// 16. Sender mismatch — an exchange message with the wrong from-callsign
//     is rejected; QSO state does not advance.
// =====================================================================
#[tokio::test]
async fn scenario_16_sender_mismatch_rejected() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // RespondingToCq, sent grid

    // An IMPOSTER sends us a report claiming to be from a different station,
    // but addressed to us on the same frequency. The relevance filter / sender
    // verification must NOT advance our VB7F QSO.
    sim.inject_decode("K5ARH PIRATE -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_not_completed_with("PIRATE");
    tl.assert_not_completed_with("VB7F");
    // We never sent an R-report to anybody (state never advanced to SendingReport).
    tl.assert_not_transmitted_contains("K5ARH R");
    // Still exactly one QSO (the VB7F one); the imposter spawned nothing.
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 17. Duplicate suppression — an AUTO call won't re-work an already-worked
//     station (the self-duplicate gate). Documented autonomous/auto-only:
//     manual calls intentionally bypass this gate (operator re-work).
// =====================================================================
#[tokio::test]
async fn scenario_17_duplicate_suppression_auto_path() {
    let sim = sim().await;
    let mgr = sim.manager();

    // First AUTO QSO with VB7F.
    let _id = mgr
        .respond_to_cq("VB7F".to_string(), FREQ, None)
        .await
        .expect("first auto respond_to_cq should succeed");

    // A second AUTO call to the SAME station on the SAME band must be refused by
    // the self-duplicate gate.
    let second = mgr.respond_to_cq("VB7F".to_string(), FREQ, None).await;
    assert!(
        second.is_err(),
        "an auto re-call of an already-worked station must be refused (duplicate gate)"
    );

    // Contrast: a MANUAL re-call is allowed (operator explicitly re-works) —
    // and continues the SAME active QSO rather than spawning a duplicate.
    let manual = mgr
        .respond_to_cq_manual("VB7F".to_string(), FREQ, None)
        .await;
    assert!(
        manual.is_ok(),
        "a manual re-call must be allowed (operator override of the duplicate gate)"
    );
}

// =====================================================================
// 18. Collision — two decodes at ~the same offset in one slot; the engine
//     handles it without spawning spurious QSOs or crashing.
// =====================================================================
#[tokio::test]
async fn scenario_18_collision_two_decodes_same_offset() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    // Collision: our DX's report AND an unrelated station's report land at the
    // same offset in the same slot. Only the correctly-addressed-and-sourced one
    // (from VB7F, to us) may advance our QSO.
    sim.inject_collision("K5ARH VB7F -12", "K5ARH OTHER -20", FREQ);
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    tl.assert_not_completed_with("OTHER");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// 19. Abort — operator aborts an in-progress manual QSO; it goes terminal
//     (UserCancelled) and we stop transmitting to that station.
// =====================================================================
#[tokio::test]
async fn scenario_19_operator_abort() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    // Operator aborts.
    sim.abort("VB7F").await;
    sim.tick().await;

    // Several more slots: no further keep-calls should go out for VB7F.
    let before = sim
        .timeline()
        .count_transmitted_containing("VB7F K5ARH EM10");
    sim.tick_n(3).await;
    let after = sim
        .timeline()
        .count_transmitted_containing("VB7F K5ARH EM10");

    let tl = sim.into_timeline();
    assert_eq!(
        before, after,
        "after abort, no further grid transmissions should be emitted\n{tl}"
    );
    tl.assert_failed_with(QsoFailureReason::UserCancelled);
    tl.assert_not_completed_with("VB7F");
}
