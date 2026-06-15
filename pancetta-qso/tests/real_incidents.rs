//! "Did we actually fix it?" — replay of the EXACT on-air QSO failures the
//! operator (K5ARH) hit over the past few days, asserting that with all the
//! fixes now on `main` each contact **properly progresses** (completes + logs,
//! or is handled correctly).
//!
//! This is the engine-level half (message parse / sequence / state machine),
//! driven through the durable [`pancetta_qso::sim`] harness — a virtual band +
//! virtual clock wrapped around the **real** `QsoManager` and the **real**
//! `MessageExchange` parser. Nothing here reimplements QSO logic: each test
//! injects the DX side exactly as it came off the air and asserts the frames we
//! transmit and the terminal outcome.
//!
//! The PTT / stale-TX / supersede half of these incidents lives in the
//! coordinator-level companion suite `pancetta/tests/real_incidents_coord.rs`.
//!
//! Each test is named after the actual station + the original symptom, with a
//! dated note, so the file reads as a checklist of resolved frustrations.
//!
//! Incidents replayed here (2026-06-12 .. 2026-06-15 on-air session):
//!   * NP4VA  — they sent RR73, we never sent 73 (RR73 mis-parsed as a grid).
//!   * T46FCR — mostly completed but never sent 73.
//!   * PY2GIG — nonsense reply "THEIRCALL MYCALL" (our 6-char grid dropped).
//!   * K9HJZ  — DX closed with RRR (not RR73) and the exchange stalled.
//!   * VB7F   — "couldn't send a 73": acting on a repeated RR73 sent grid.
//!
//! Run: `cargo test -p pancetta-qso --test real_incidents`.

use pancetta_core::ResponseStep;
use pancetta_qso::sim::Sim;

const US: &str = "K5ARH";
/// The classic 4-char grid we transmit in standard messages.
const GRID4: &str = "EM10";
/// The operator's *configured* grid is a 6-char locator. The PY2GIG incident was
/// that this 6-char value was being dropped by the FT8 encoder, degrading our
/// reply to a bare callsign. `outbound_grid_field` now truncates it to 4 chars.
const GRID6: &str = "EM10ch";
const FREQ: f64 = 1500.0;

/// Default harness for our station (4-char grid).
async fn sim() -> Sim {
    Sim::new(US, Some(GRID4)).await
}

// =====================================================================
// Incident 1 — NP4VA
// Symptom (2026-06-12): "they sent RR73, we never sent 73." Root cause:
// "RR73" is a syntactically valid Maidenhead grid (field RR, square 73), so
// the parser swallowed the DX's closing "NP4VA K5ARH RR73" as a CqResponse
// carrying grid "RR73". The FinalConfirmation arm never fired, our 73 was
// never sent, and nothing was logged — the QSO stalled one message short.
// Fix: the close tokens (RR73 / RRR / 73) are now matched BEFORE the grid
// regex in `MessageExchange::parse_qso_message`.
// =====================================================================
#[tokio::test]
async fn np4va_rr73_is_not_swallowed_as_a_grid_we_send_73_and_complete() {
    let mut sim = sim().await;

    // We call NP4VA (manual). Our grid goes out.
    sim.inject_decode("CQ NP4VA FK68", FREQ, -10.0, 0.1);
    sim.call_station("NP4VA", FREQ).await;
    sim.tick().await;

    // They send us a signal report → engine auto-sends our R-report.
    sim.inject_decode("K5ARH NP4VA -13", FREQ, -10.0, 0.1);
    sim.tick().await;

    // They roger with RR73. This is the exact frame that used to be eaten as a
    // grid. It must classify as the close, so we answer 73 and complete.
    sim.inject_decode("K5ARH NP4VA RR73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("NP4VA K5ARH 73");
    tl.assert_completed_with("NP4VA");
    // The bug fingerprint: we must NOT have treated RR73 as a grid and re-sent a
    // CqResponse carrying "RR73".
    tl.assert_not_transmitted_contains("NP4VA K5ARH RR73");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// Incident 2 — T46FCR
// Symptom (2026-06-12): exchange "mostly completed but never sent 73."
// Same root cause as NP4VA (the close token was lost). Replay a full
// exchange ending in their RR73 and assert our 73 goes out, the QSO reaches
// Completed, and a QsoCompleted (the log event) is observed.
// =====================================================================
#[tokio::test]
async fn t46fcr_full_exchange_sends_73_completes_and_logs() {
    let mut sim = sim().await;

    sim.inject_decode("CQ T46FCR FK68", FREQ, -11.0, 0.1);
    sim.call_station("T46FCR", FREQ).await;
    sim.tick().await; // "T46FCR K5ARH EM10"

    sim.inject_decode("K5ARH T46FCR -15", FREQ, -11.0, 0.1);
    sim.tick().await; // our R-report

    sim.inject_decode("K5ARH T46FCR RR73", FREQ, -11.0, 0.1);
    sim.tick().await; // our 73 + complete + log

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("T46FCR K5ARH 73");
    tl.assert_completed_with("T46FCR");
    // A QsoCompleted (what AsyncQsoLogger persists) was emitted for T46FCR.
    assert!(
        tl.completions
            .iter()
            .any(|c| c.their_callsign.as_deref() == Some("T46FCR")),
        "expected a QsoCompleted (log event) for T46FCR\n{tl}"
    );
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// Incident 3 — PY2GIG
// Symptom (2026-06-13): nonsense reply "PY2GIG K5ARH" (bare callsigns, no
// grid). Root cause: the operator's configured grid is a 6-char locator
// (EM10ch). The standard FT8 type-1 message only carries a 4-char grid; the
// encoder silently dropped the invalid 6-char field, degrading our reply to a
// bare callsign. Fix: `outbound_grid_field` truncates+uppercases to the first
// 4 chars at the single message-generation boundary.
// =====================================================================
#[tokio::test]
async fn py2gig_six_char_grid_truncates_to_four_not_bare_callsigns() {
    // Build the harness with the operator's actual 6-char configured grid.
    let mut sim = Sim::new(US, Some(GRID6)).await;

    sim.inject_decode("CQ PY2GIG GG66", FREQ, -9.0, 0.1);
    sim.call_station("PY2GIG", FREQ).await;
    sim.tick().await; // our opening reply

    let tl = sim.into_timeline();
    // The reply must carry a valid 4-char grid…
    tl.assert_transmitted_contains("PY2GIG K5ARH EM10");
    // …and must NOT be the bare-callsign nonsense the operator saw on air.
    assert!(
        !tl.transmissions
            .iter()
            .any(|t| t.text.trim() == "PY2GIG K5ARH"),
        "reply degraded to bare callsigns (the PY2GIG bug); grid was dropped\n{tl}"
    );
    // And we never transmitted the raw 6-char locator (which the encoder drops).
    tl.assert_not_transmitted_contains("EM10CH");
    tl.assert_not_transmitted_contains("EM10ch");
}

// =====================================================================
// Incident 4(a) — K9HJZ
// Symptom (2026-06-13): the DX closed with "RRR" (not RR73) and the exchange
// stalled. Root cause: RRR was not recognized as a close. Fix: RRR is matched
// alongside RR73 as a FinalConfirmation in the parser, so the QSO completes
// and we answer 73. (The duplicate-QSO half of the K9HJZ incident is the
// re-call storm, asserted in the coordinator companion suite.)
// =====================================================================
#[tokio::test]
async fn k9hjz_rrr_close_is_recognized_we_complete() {
    let mut sim = sim().await;

    sim.inject_decode("CQ K9HJZ EN52", FREQ, -7.0, 0.1);
    sim.call_station("K9HJZ", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH K9HJZ -05", FREQ, -7.0, 0.1);
    sim.tick().await;

    // The DX closes with a plain roger (RRR), not RR73. Must close the QSO.
    sim.inject_decode("K5ARH K9HJZ RRR", FREQ, -7.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("K9HJZ");
    tl.assert_transmitted_contains("K9HJZ K5ARH 73");
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// Incident 4(b) — K9HJZ re-call (engine view)
// Symptom (2026-06-13): repeated manual calls to K9HJZ spawned 3 duplicate
// QSO objects. At the engine level a manual re-call of an already-active
// station must CONTINUE the same QSO (same id) — no duplicate, no
// Failed{Superseded}. (The coordinator/PTT view of this storm is in the
// companion suite; here we prove the engine never mints a second QSO id.)
// =====================================================================
#[tokio::test]
async fn k9hjz_repeated_manual_calls_keep_one_qso_no_superseded() {
    let mut sim = sim().await;

    sim.inject_decode("CQ K9HJZ EN52", FREQ, -7.0, 0.1);
    let id1 = sim.call_station("K9HJZ", FREQ).await;
    sim.tick().await;

    // Operator mashes the call again (and again) on the same station.
    let id2 = sim.call_station("K9HJZ", FREQ).await;
    sim.tick().await;
    let id3 = sim.call_station("K9HJZ", FREQ).await;
    sim.tick().await;

    assert_eq!(id1, id2, "2nd manual re-call must return the same QSO id");
    assert_eq!(id1, id3, "3rd manual re-call must return the same QSO id");

    let tl = sim.into_timeline();
    tl.assert_no_duplicate_qsos();
    tl.assert_no_superseded();
}

// =====================================================================
// Incident 5 — VB7F
// Symptom (2026-06-13): "couldn't send a 73 to save our life." The DX kept
// sending RR73 directed at us; when the operator acted on VB7F, the reply
// path sent our GRID instead of 73. Root cause: the context-aware reply did
// not recognize an incoming RR73 as "open at the 73 step." Fix: the smart
// default for a station whose last directed message is RR73/RRR is
// ResponseStep::SeventyThree, and the engine's (SendingReport, RR73) and
// (WaitingForConfirmation, RR73) arms answer 73 → complete.
//
// Here we replay the operator acting on VB7F at the SeventyThree step (what
// the smart default selects when the DX's last frame is RR73) and assert we
// transmit "VB7F K5ARH 73" — NOT a grid — and complete.
// =====================================================================
#[tokio::test]
async fn vb7f_acting_on_repeated_rr73_sends_73_not_grid() {
    let mut sim = sim().await;

    // VB7F is hammering us with RR73 (they didn't copy our previous 73).
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    // Operator acts on VB7F — smart default opens at the 73 step.
    sim.respond_to_caller("VB7F", FREQ, ResponseStep::SeventyThree, Some(-8.0), None)
        .await;
    sim.tick().await;

    let tl = sim.into_timeline();
    // The exact frame the operator could never get out:
    tl.assert_transmitted_contains("VB7F K5ARH 73");
    // The bug was that we sent our grid instead. Assert we did NOT.
    tl.assert_not_transmitted_contains("VB7F K5ARH EM10");
    tl.assert_completed_with("VB7F");
}

// =====================================================================
// Incident 5 (bounded re-send) — VB7F keeps sending RR73
// The DX keeps repeating RR73 after we've completed. Acting/keep-calling must
// still answer 73 and the closing 73s must be BOUNDED (a stuck DX can never
// make us TX forever). The coordinator caps post-completion re-sends at 3; the
// engine's own per-QSO close sends 73 once. Either way the count stays small.
// =====================================================================
#[tokio::test]
async fn vb7f_repeated_rr73_resends_are_bounded() {
    let mut sim = sim().await;

    // Full exchange ending in their first RR73…
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;

    // …then they keep sending RR73 for several more slots.
    for _ in 0..5 {
        sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
        sim.tick().await;
    }

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    let seventy_threes = tl.count_transmitted_containing("VB7F K5ARH 73");
    assert!(
        (1..=6).contains(&seventy_threes),
        "closing 73s to a stuck DX must be bounded (1..=6), got {seventy_threes}\n{tl}"
    );
    tl.assert_no_duplicate_qsos();
}
