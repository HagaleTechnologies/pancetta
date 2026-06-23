//! Batch A — completion / asymmetry / close-token adversarial scenarios.
//!
//! A permanent regression library that PROVES proper QSO progression through
//! the hard parts of an FT8 contact: the moment the DX *answers* us, the
//! close-token semantics (RR73 vs RRR vs 73), report-value corrections,
//! mutual-deadlock, and the guards against logging a phantom QSO.
//!
//! Built on the durable [`pancetta_qso::sim`] harness (virtual band + virtual
//! clock around the REAL `QsoManager` + REAL `MessageExchange` parser). Each
//! test scripts the DX side slot-by-slot exactly as it would come off the air
//! and asserts the frames we transmit and the terminal outcome.
//!
//! Frame convention (matches the on-air / `real_incidents.rs` convention):
//! a frame is `<to> <from> [field]`. So a report the DX (NP4VA) sends *us*
//! (K5ARH) is `"K5ARH NP4VA -13"`, and a bare-call answer is `"K5ARH NP4VA"`.
//!
//! Source tags: LOG = past on-air log analysis; PEER = peer/forum research
//! (WSJT-X/JTDX/MSHV/ft8mon/Hinson FT8 guide); IDEA = operator-grounded
//! ideation. See `docs/qso-scenario-catalog-2026-06-16.md`.
//!
//! Run: `cargo test -p pancetta-qso --test adversarial_completion`.

use pancetta_qso::sim::Sim;
use pancetta_qso::{QsoFailureReason, QsoManagerConfig, TimeoutConfig};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

/// Default harness for our station.
async fn sim() -> Sim {
    Sim::new(US, Some(GRID)).await
}

/// True if `t` is a plain signal report we sent the DX (`"<DX> K5ARH -NN"`
/// or `"<DX> K5ARH +NN"`) — distinct from our grid (`"<DX> K5ARH EM10"`) or
/// an R-report (`"<DX> K5ARH R-NN"`). Inspects the third (report) field so the
/// 'R' inside "K5ARH" never trips the R-report check.
fn is_our_signal_report(text: &str, dx: &str) -> bool {
    let prefix = format!("{dx} {US} ");
    let Some(field) = text.strip_prefix(&prefix) else {
        return false;
    };
    // A plain report starts with a sign and parses as a number; an R-report
    // starts with 'R'; a grid is alphabetic.
    (field.starts_with('-') || field.starts_with('+'))
        && field.trim_start_matches(['-', '+']).parse::<i8>().is_ok()
}

// =====================================================================
// A1 — stuck-at-grid: DX answers with a bare `K5ARH N8ME` (no report).
// Source: LOG (N8ME, F5NNN, N9FME, IQ0VT, KB5YNF, KA0NC, first-K9HJZ —
// the single highest-frequency stall in the corpus). We answered with our
// grid and looped it 10x until the watchdog timed out while the DX kept
// answering with a bare call.
//
// Correct behavior: when the DX returns our call (acknowledging us) we now
// KNOW they heard our grid, so we must STEP FORWARD and send them a signal
// report — not re-loop our grid forever.
// =====================================================================
#[tokio::test]
async fn a1_stuck_at_grid_bare_call_answer_advances_to_report() {
    let mut sim = sim().await;

    // Slot 0: DX is calling CQ; operator calls them (manual). Our grid goes out.
    sim.inject_decode("CQ N8ME EN82", FREQ, -10.0, 0.1);
    sim.call_station("N8ME", FREQ).await;
    sim.tick().await; // "N8ME K5ARH EM10"

    // Slot 1: DX answers with a BARE call back to us — no report. This is the
    // exact frame shape that used to stall the QSO at grid forever.
    sim.inject_decode("K5ARH N8ME", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must have opened with our grid…
    tl.assert_transmitted_contains("N8ME K5ARH EM10");
    // …and then ADVANCED to sending them a signal report (the fix).
    assert!(
        tl.transmissions
            .iter()
            .any(|t| is_our_signal_report(&t.text, "N8ME")),
        "expected to advance grid -> signal report (a 'N8ME K5ARH -NN' frame), \
         but only ever sent grid\n{tl}"
    );
}

// =====================================================================
// A2 — stuck-at-grid-2: DX answers `K5ARH N8ME EN82` (repeats their grid).
// Source: LOG (same incident family). A CqResponse carrying the DX's grid
// directed at us is still just "I'm here, I heard you" — we must step to a
// report, not loop our grid.
// =====================================================================
#[tokio::test]
async fn a2_stuck_at_grid_grid_answer_advances_to_report() {
    let mut sim = sim().await;

    sim.inject_decode("CQ N8ME EN82", FREQ, -10.0, 0.1);
    sim.call_station("N8ME", FREQ).await;
    sim.tick().await; // "N8ME K5ARH EM10"

    // DX answers by repeating their own grid back to us (CqResponse to us).
    sim.inject_decode("K5ARH N8ME EN82", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("N8ME K5ARH EM10");
    assert!(
        tl.transmissions
            .iter()
            .any(|t| is_our_signal_report(&t.text, "N8ME")),
        "expected to advance grid -> report when DX repeated their grid to us\n{tl}"
    );
}

// =====================================================================
// A1+A2 regression: the formerly-stuck case now runs all the way to a
// COMPLETE, LOGGED QSO. Source: LOG (stuck-at-grid family) — the proof the
// fix doesn't just emit a report but lets the contact finish.
// =====================================================================
#[tokio::test]
async fn a1_stuck_at_grid_then_full_completion() {
    let mut sim = sim().await;

    sim.inject_decode("CQ N8ME EN82", FREQ, -10.0, 0.1);
    sim.call_station("N8ME", FREQ).await;
    sim.tick().await; // grid

    // Bare-call answer → we advance to sending a report.
    sim.inject_decode("K5ARH N8ME", FREQ, -10.0, 0.1);
    sim.tick().await; // our report

    // DX rogers our report with their R-report.
    sim.inject_decode("K5ARH N8ME R-07", FREQ, -10.0, 0.1);
    sim.tick().await; // our RR73

    // DX closes with 73.
    sim.inject_decode("K5ARH N8ME 73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("N8ME");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// A3 — they 73 us while we're at the report step (DX skips the R-report).
// Source: PEER (WSJT-X auto-sequencer accepts an early 73 as the close) +
// LOG. We've sent our R-report; the DX jumps straight to 73. Accept it,
// complete + log, and stop re-sending our report.
// =====================================================================
#[tokio::test]
async fn a3_they_73_us_early_we_accept_and_complete() {
    let mut sim = sim().await;

    sim.inject_decode("CQ K9XYZ EM29", FREQ, -9.0, 0.1);
    sim.call_station("K9XYZ", FREQ).await;
    sim.tick().await; // grid

    // DX sends us a report → we send our R-report (SendingReport).
    sim.inject_decode("K5ARH K9XYZ -14", FREQ, -9.0, 0.1);
    sim.tick().await; // "K9XYZ K5ARH R-NN"

    // DX skips the R-report and closes straight with 73.
    sim.inject_decode("K5ARH K9XYZ 73", FREQ, -9.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("K9XYZ");
    // We must NOT keep sending our R-report after they've closed.
    let reports_after = tl
        .transmissions
        .iter()
        .filter(|t| t.slot >= 3 && t.text.contains("R-"))
        .count();
    assert_eq!(
        reports_after, 0,
        "kept sending R-report after DX 73'd us\n{tl}"
    );
}

// =====================================================================
// A4 — DX skips grid: answers our CQ directly with a plain report.
// Source: PEER (an answering station that already has our copy can open with
// a signal report instead of a grid). We CQ; a station answers with
// "K5ARH W5XO -10". We should accept it, send our R-report/close, and log.
//
// KNOWN BUG (CQer-flow asymmetry, NOT the stuck-at-grid Caller bug fixed in
// this batch): the state machine has NO `(CallingCq, SignalReport)` arm — it
// only advances CallingCq on a `CqResponse`. When a caller skips the grid and
// answers our CQ with a bare report, we never advance out of CallingCq and
// keep re-CQing (see timeline: slots 2-3 still emit "CQ K5ARH EM10"). The
// symmetric fix belongs with the CQer-flow gaps (A4/A5) in a follow-up; the
// Caller-side stuck-at-grid fix in this batch does not touch the CQer ladder.
// FIXED (A4): `(CallingCq, SignalReport[to==us])` arm added to
// determine_state_transition (→ WaitingForReport), routed in
// is_message_relevant, and generate_response sends our report — so a CQ
// answered with a bare report now advances and completes.
#[tokio::test]
async fn a4_dx_skips_grid_answers_cq_with_report() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // "CQ K5ARH EM10"

    // A station answers our CQ with a plain report (skipping the grid step).
    sim.inject_decode("K5ARH W5XO -10", FREQ, -10.0, 0.1);
    sim.tick().await;

    // They roger our report.
    sim.inject_decode("K5ARH W5XO R-08", FREQ, -10.0, 0.1);
    sim.tick().await;

    // They close.
    sim.inject_decode("K5ARH W5XO 73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("W5XO");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// A5 — out-of-order RR73 (CQer flow): after we (the CQer) send our report,
// the caller fires RR73 instead of an R-report. We should accept the
// completion + log.
// Source: PEER/LOG.
//
// KNOWN BUG (CQer-flow asymmetry, NOT the stuck-at-grid Caller bug fixed in
// this batch): in the CQer flow we sit in `WaitingForReport` after sending our
// report, expecting the caller's `ReportAck` (R-report). There is NO
// `(WaitingForReport, FinalConfirmation|SeventyThree)` transition arm, so a
// caller that closes early with RR73/73 (skipping the R-report) is ignored and
// the QSO never completes (see timeline: slot 2 RR73 produces no transition).
// The Caller flow already accepts an early close from `SendingReport` (the
// FIX-2 arm); the symmetric CQer arm is the follow-up.
// FIXED (A5): symmetric early-close arm added —
// `(WaitingForReport, FinalConfirmation|SeventyThree[from==DX,to==us])` →
// Completed in determine_state_transition, routed in is_message_relevant, and
// generate_response answers our 73. The CQer now accepts a caller's early RR73.
#[tokio::test]
async fn a5_out_of_order_rr73_after_our_report_completes() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // CQ

    // Caller answers our CQ with their grid → we send their report.
    sim.inject_decode("K5ARH W5XO EM12", FREQ, -10.0, 0.1);
    sim.tick().await; // "W5XO K5ARH -NN" (we are now WaitingForReport)

    // Instead of an R-report, the caller fires RR73 (out-of-order close).
    sim.inject_decode("K5ARH W5XO RR73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("W5XO");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// A6 — wrong/repeated report value, no RR73 yet (Caller flow).
// Source: LOG. After we send our R-report, the DX RE-sends their report
// (they didn't copy our R). We must STAY at the report step and re-send our
// R — never auto-complete on a repeat.
// =====================================================================
#[tokio::test]
async fn a6_repeated_report_no_rr73_we_resend_r_and_hold() {
    let mut sim = sim().await;

    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.call_station("NP4VA", FREQ).await;
    sim.tick().await; // grid

    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await; // our R-report (SendingReport)

    // DX repeats their report (didn't copy our R).
    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must NOT have completed (no RR73 ever arrived).
    tl.assert_not_completed_with("NP4VA");
    // We must have (re-)sent our R-report and kept holding at the report step.
    tl.assert_transmitted_contains("NP4VA K5ARH R");
}

// =====================================================================
// A7 — DX corrects the report value (different value second time).
// Source: PEER (operator may re-key a corrected report). We latch the
// NEWEST received report and re-send our R; we do not advance on the change.
// The completion below carries the freshest exchange.
// =====================================================================
#[tokio::test]
async fn a7_dx_corrects_report_we_latch_newest_and_complete() {
    let mut sim = sim().await;

    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.call_station("NP4VA", FREQ).await;
    sim.tick().await; // grid

    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await; // our R-report

    // DX corrects to a different value (re-keyed report).
    sim.inject_decode("K5ARH NP4VA -09", FREQ, -10.0, 0.1);
    sim.tick().await;

    // Finish the QSO.
    sim.inject_decode("K5ARH NP4VA RR73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must not have prematurely completed on the corrected report, and we
    // must still be able to complete on the close.
    tl.assert_completed_with("NP4VA");
    // We re-sent our R-report at least twice (once per received report).
    assert!(
        tl.count_transmitted_containing("NP4VA K5ARH R") >= 2,
        "expected to re-send R on the corrected report\n{tl}"
    );
}

// =====================================================================
// A8 — stuck loop: neither side copies the R-frame.
// Source: LOG/IDEA. We answer; DX sends a report; we send R; DX never
// rogers (keeps repeating their report). We keep-call to the watchdog and
// NEVER auto-complete a contact that wasn't actually closed.
// =====================================================================
#[tokio::test]
async fn a8_stuck_r_loop_runs_to_watchdog_never_completes() {
    // Short TIME watchdog so termination is reachable within the tick budget.
    // (The call-count cap no longer retires an ENGAGED SendingReport exchange —
    // that abandoned QSOs one slot before the DX's RR73; see the 9K2MP on-air
    // fix in qso_manager. A genuinely stuck R-loop now terminates via the time
    // watchdog instead.) The key invariant is unchanged: we NEVER auto-complete
    // a contact the DX didn't actually close.
    let config = QsoManagerConfig {
        our_callsign: US.to_string(),
        our_grid: Some(GRID.to_string()),
        timeouts: TimeoutConfig {
            manual_call_max_calls: 4,
            manual_call_watchdog_minutes: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut sim = Sim::with_config(config).await;

    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.call_station("NP4VA", FREQ).await;
    sim.tick().await; // grid

    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await; // our R-report

    // DX keeps repeating their report — never rogers our R.
    for _ in 0..12 {
        sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
        sim.tick().await;
    }

    let tl = sim.into_timeline();
    tl.assert_not_completed_with("NP4VA");
    // The QSO must terminate via the watchdog, not hang forever.
    tl.assert_failed_with(QsoFailureReason::Timeout);
}

// =====================================================================
// A9 — mutual deadlock: after WE (the CQer) receive the caller's R, WE owe
// RR73. We must SEND it, not idle.
// Source: PEER (deadlock pattern). CQer flow: CQ -> their grid -> our report
// -> their R-report -> OUR RR73.
// =====================================================================
#[tokio::test]
async fn a9_cqer_owes_rr73_after_receiving_r_and_sends_it() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // CQ

    sim.inject_decode("K5ARH W5XO EM12", FREQ, -10.0, 0.1);
    sim.tick().await; // our report (WaitingForReport)

    // Caller rogers our report with their R-report → WE owe RR73.
    sim.inject_decode("K5ARH W5XO R-09", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must have transmitted RR73 (not gone idle).
    tl.assert_transmitted_contains("W5XO K5ARH RR73");
}

// =====================================================================
// A10 — premature self-completion guard: we send our grid, get NO replies,
// and must NEVER log a phantom QSO; the watchdog must Fail it.
// Source: LOG (16 full-10-call timeouts) / IDEA.
// =====================================================================
#[tokio::test]
async fn a10_no_replies_never_logs_phantom_watchdog_fails() {
    let config = QsoManagerConfig {
        our_callsign: US.to_string(),
        our_grid: Some(GRID.to_string()),
        timeouts: TimeoutConfig {
            manual_call_max_calls: 3,
            manual_call_watchdog_minutes: 60,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut sim = Sim::with_config(config).await;

    sim.inject_decode("CQ N9FME EN50", FREQ, -10.0, 0.1);
    sim.call_station("N9FME", FREQ).await;
    sim.tick().await; // grid

    // Never any reply. Keep ticking past the call cap.
    sim.tick_n(8).await;

    let tl = sim.into_timeline();
    // NEVER a phantom completion.
    tl.assert_not_completed_with("N9FME");
    tl.assert_failed_with(QsoFailureReason::Timeout);
}

// =====================================================================
// A11 — RR73 vs RRR vs 73 distinct close paths.
// Source: PEER [D1] (RR73 = no reply expected by the sender, folds 73;
// RRR = a roger; 73 = sign-off). In OUR caller flow, the DX closing from
// our R-report with any of these completes the contact. Here we assert all
// three close tokens drive a completion.
// =====================================================================
#[tokio::test]
async fn a11_rr73_rrr_73_all_close_the_contact() {
    for (idx, close) in ["RR73", "RRR", "73"].into_iter().enumerate() {
        let mut sim = sim().await;
        let dx = format!("W{}ABC", idx + 1);

        sim.inject_decode(&format!("CQ {dx} EM00"), FREQ, -10.0, 0.1);
        sim.call_station(&dx, FREQ).await;
        sim.tick().await; // grid

        sim.inject_decode(&format!("K5ARH {dx} -12"), FREQ, -10.0, 0.1);
        sim.tick().await; // our R-report

        sim.inject_decode(&format!("K5ARH {dx} {close}"), FREQ, -10.0, 0.1);
        sim.tick().await;

        let tl = sim.into_timeline();
        tl.assert_completed_with(&dx);
    }
}

// =====================================================================
// A12 — log on OUR state, not a "73" string match. After we send our
// R-report and the DX rogers with RR73 (we complete), the DX then CQs
// again. We must already have LOGGED on reaching Completed and must not
// un-log or re-complete from the DX's subsequent CQ.
// Source: PEER [A6/D7] / LOG.
// =====================================================================
#[tokio::test]
async fn a12_log_on_state_partner_cqs_after_close() {
    let mut sim = sim().await;

    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.call_station("NP4VA", FREQ).await;
    sim.tick().await; // grid
    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await; // our R-report
    sim.inject_decode("K5ARH NP4VA RR73", FREQ, -10.0, 0.1);
    sim.tick().await; // complete + log

    // Partner now CQs again (worked the band, moved on).
    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.tick().await;
    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("NP4VA");
    // Exactly one completion — the DX's later CQ must not re-complete/duplicate.
    let np4va_completions = tl
        .completions
        .iter()
        .filter(|c| c.their_callsign.as_deref() == Some("NP4VA"))
        .count();
    assert_eq!(np4va_completions, 1, "logged NP4VA more than once\n{tl}");
}

// =====================================================================
// A13 — bounded auto-73 STOPS after the cap on repeated RR73.
// Source: PEER/LOG (a stuck DX hammering RR73 must not make us TX forever).
// We complete, the DX keeps sending RR73, and our closing 73s stay bounded.
// =====================================================================
#[tokio::test]
async fn a13_bounded_auto_73_stops_under_rr73_storm() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // grid
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await; // our R-report
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await; // complete + first 73

    // DX hammers RR73 for many more slots.
    for _ in 0..12 {
        sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
        sim.tick().await;
    }

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    let seventy_threes = tl.count_transmitted_containing("VB7F K5ARH 73");
    // The engine completes the QSO on the first RR73; once Completed it is
    // terminal, so subsequent RR73s route to no active QSO and produce no more
    // 73s. The count must stay small regardless of how long the DX hammers.
    assert!(
        (1..=6).contains(&seventy_threes),
        "closing 73s to a stuck DX must be bounded (1..=6), got {seventy_threes}\n{tl}"
    );
}
