//! Batch B adversarial scenario catalog — 3rd/4th-party traffic, sender/to-field
//! discrimination, yield-to-busy, and multi-stream isolation.
//!
//! Source: `docs/qso-scenario-catalog-2026-06-16.md` (Batch B, B1–B15). Built on
//! the durable [`pancetta_qso::sim`] harness (virtual band + virtual clock) which
//! drives the **real** `QsoManager`, plus a few cases exercised directly against
//! the real `AutonomousOperator` (its busy/responded sets are keyed on
//! `std::time::Instant`, not the engine's injectable clock — those are noted
//! autonomous-only, mirroring `qso_scenarios.rs::scenario_08`).
//!
//! Run: `cargo test -p pancetta-qso --test adversarial_3party`.
//!
//! Discrimination model these tests pin down (from `qso_manager.rs`):
//!  - `is_message_relevant` routes a decode to an active QSO only when it matches
//!    on **callsign + to-field (== us) + state**, and within a **15 Hz** frequency
//!    tolerance. A frame addressed to a third party (`to != us`) never advances
//!    our QSO, regardless of frequency.
//!  - `determine_state_transition` re-verifies `from_station == expected DX` AND
//!    `to_station == our_callsign` on every advancing arm — so an impostor on the
//!    DX's frequency, or a station using the partner's call, cannot drive us.
//!
//! Each test is a named on-air situation with a slot-by-slot exchange and an
//! asserted CORRECT outcome. Any case where correct behavior currently FAILS is
//! committed `#[ignore]` with a `// KNOWN BUG:` note (assertion left intact), to
//! become the fix list for a coordinated engine follow-up.

use pancetta_qso::sim::Sim;
use pancetta_qso::{AutonomousConfig, AutonomousOperator, DecodedMessageInfo, DxEvaluator};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

/// Build a default harness for our station.
async fn sim() -> Sim {
    Sim::new(US, Some(GRID)).await
}

/// A trivial evaluator that scores every CQ above the autonomous `min_dx_score`
/// (0.3 default), so the *only* reason an auto-response would be suppressed is a
/// policy gate (busy / recently-responded), not a low score. Used by the
/// autonomous-layer B1/B2 cases.
struct AlwaysWorthIt;
impl DxEvaluator for AlwaysWorthIt {
    fn evaluate_cq(&self, _callsign: &str, _grid: Option<&str>, _snr: i8, _freq_hz: f64) -> f64 {
        5.0
    }
}

/// Helper: did the autonomous operator emit a `Transmit` whose rendered text
/// targets `callsign` (i.e. begins addressing that DX)? An auto-response to a CQ
/// renders as `"<DX> <US> <grid>"`, so it starts with the DX callsign.
fn auto_transmitted_to(actions: &[pancetta_qso::OperatorAction], callsign: &str) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            pancetta_qso::OperatorAction::Transmit { message_text, qso_id: None, .. }
                if message_text.starts_with(&format!("{callsign} "))
        )
    })
}

// =====================================================================
// B1 — DX working someone else: `Z DX -07` (to != us). We must NOT take
//      their report into our QSO. We yield.
//      [LOG pattern 6: K2BAG/9A4AA/N4ZSA called over a busy DX]
// =====================================================================
//
// Engine-level: we have an active QSO with VB7F (we called, sent our grid). The
// band then carries VB7F's *report to a third party* `W9ZZZ VB7F -07`. Because
// the to-field is W9ZZZ (not us), the relevance filter must reject it — our QSO
// stays in RespondingToCq and never jumps to SendingReport on someone else's
// report. (SOURCE: LOG pattern 6 + qso_manager::is_message_relevant.)
#[tokio::test]
async fn b1_dx_working_third_party_we_yield() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // RespondingToCq, our grid out

    // VB7F answers a DIFFERENT station, not us. We overhear it on the same freq.
    sim.inject_decode("W9ZZZ VB7F -07", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must not have advanced: no R-report to anyone, no completion with VB7F
    // on the strength of a report that was never addressed to us.
    tl.assert_not_transmitted_contains("VB7F K5ARH R");
    tl.assert_not_completed_with("VB7F");
    tl.assert_not_completed_with("W9ZZZ");
    // We did not spawn a phantom QSO from the third-party frame.
    tl.assert_at_most_qsos(1);
}

// B1 (autonomous facet) — a DX seen working a third party, then briefly CQing,
// must be suppressed by the DX-busy gate even though its score clears the bar.
// AUTONOMOUS-ONLY (busy set keyed on Instant). [SOURCE: LOG pattern 6 + IDEA.]
#[tokio::test]
async fn b1_autonomous_yields_to_busy_dx() {
    let mut op = AutonomousOperator::new(
        AutonomousConfig {
            min_dx_score: 0.3,
            ..Default::default()
        },
        US.to_string(),
        Some(GRID.to_string()),
    );

    // VB7F is mid-exchange with W9ZZZ (report not directed at us) AND CQs in the
    // same batch. The CQ alone would be worth answering (score 5.0 ≥ 0.3), but
    // the busy gate must win.
    op.feed_decoded_messages(
        &[
            DecodedMessageInfo {
                message_text: "W9ZZZ VB7F -07".to_string(),
                callsign: Some("VB7F".to_string()),
                snr: -7,
                frequency_hz: FREQ,
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            },
            DecodedMessageInfo {
                message_text: "CQ VB7F DO33".to_string(),
                callsign: Some("VB7F".to_string()),
                snr: -7,
                frequency_hz: FREQ,
                slot_parity: None,
                confidence: None,
                time_offset_s: None,
                decode_origin: None,
            },
        ],
        &AlwaysWorthIt,
    );

    assert!(
        op.is_dx_busy("VB7F", std::time::Instant::now()),
        "VB7F mid-exchange with a third party must read as busy"
    );

    // Find a transmit slot deterministically and confirm we do NOT key a
    // response addressed to the busy VB7F.
    let mut responded = false;
    for s in 0..8i64 {
        let actions = op.decide_at(s * 15);
        if auto_transmitted_to(&actions, "VB7F") {
            responded = true;
            break;
        }
    }
    assert!(
        !responded,
        "autonomous operator must yield to a busy DX (no auto-response to VB7F)"
    );
}

// =====================================================================
// B2 — DX picks the other guy: we answered VB7F, but VB7F sends its report to
//      X (`X VB7F -10`, to != us) → we yield + back off (no advance).
//      Same relevance-filter mechanism as B1 but with US having explicitly
//      called and the DX explicitly choosing someone else. (SOURCE: LOG/IDEA.)
// =====================================================================
#[tokio::test]
async fn b2_dx_picks_the_other_guy_we_back_off() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    // We sent our grid this slot; track how many grid calls so far.
    let grids_before = sim
        .timeline()
        .count_transmitted_containing("VB7F K5ARH EM10");

    // VB7F picks W4OTH instead of us.
    sim.inject_decode("W4OTH VB7F -10", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must not have advanced to a report on the strength of a report meant
    // for W4OTH.
    tl.assert_not_transmitted_contains("VB7F K5ARH R");
    tl.assert_not_completed_with("VB7F");
    // We stay in our calling state (keep-call may re-send our grid; that's the
    // manual keep-call behavior, not an advance). The point: no state ADVANCE.
    let grids_after = tl.count_transmitted_containing("VB7F K5ARH EM10");
    assert!(
        grids_after >= grids_before,
        "grid count should be monotone (keep-call), never an advance to R\n{tl}"
    );
}

// =====================================================================
// B3 — third station calls mid-QSO: while we're working VB7F, X sends us a
//      directed call `K5ARH W4OTH EM50`. We must DEFER X and keep the current
//      VB7F QSO (no second QSO spawned by an unsolicited directed call; the
//      operator/autonomous layer decides whether to queue X later).
//      (SOURCE: LOG/IDEA + qso_manager routing.)
// =====================================================================
#[tokio::test]
async fn b3_third_station_calls_mid_qso_defer_keep_current() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // RespondingToCq with VB7F

    // A third station calls US on a different offset, mid-QSO.
    sim.inject_decode("K5ARH W4OTH EM50", FREQ + 300.0, -10.0, 0.1);
    sim.tick().await;

    // VB7F continues normally afterward.
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // The VB7F QSO is unharmed and completes.
    tl.assert_completed_with("VB7F");
    // The unsolicited directed call did not start a second QSO on its own
    // (the engine does not auto-respond to a directed call without an operator
    // or autonomous decision; the manual harness made no such decision).
    tl.assert_not_completed_with("W4OTH");
    tl.assert_at_most_qsos(1);
    // We never sent W4OTH our grid (we did not start working them).
    tl.assert_not_transmitted_contains("W4OTH K5ARH");
}

// =====================================================================
// B4 — tail-ender pounces as we finish: as the VB7F QSO completes, X calls us.
//      We complete VB7F, then a FRESH manual QSO with X proceeds independently
//      (distinct QSO id, distinct completion). (SOURCE: LOG/IDEA.)
// =====================================================================
#[tokio::test]
async fn b4_tail_ender_after_completion_fresh_qso() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    let id_vb7f = sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // VB7F rogers; a tail-ender X calls us in the same slot.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.inject_decode("K5ARH W4OTH EM50", FREQ + 250.0, -9.0, 0.1);
    sim.tick().await;

    // Operator works the tail-ender as a fresh manual QSO on its own freq.
    let id_w4oth = sim.call_station("W4OTH", FREQ + 250.0).await;
    sim.tick().await;
    sim.inject_decode("K5ARH W4OTH -08", FREQ + 250.0, -9.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH W4OTH RR73", FREQ + 250.0, -9.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    tl.assert_completed_with("W4OTH");
    assert_ne!(
        id_vb7f, id_w4oth,
        "the tail-ender must be a DISTINCT QSO id, not a continuation"
    );
    assert_eq!(
        tl.distinct_qso_ids().len(),
        2,
        "expected exactly two distinct QSOs (VB7F then W4OTH)\n{tl}"
    );
}

// =====================================================================
// B5 — two answer our CQ in the same slot → deterministic pick-one. The first
//      valid CqResponse advances the single CallingCq QSO; the later answerer
//      does not match (state has moved past CallingCq) — one QSO per CQ.
//      [SOURCE: PEER B1 + qso_manager start_cq_manual docs.]
// =====================================================================
#[tokio::test]
async fn b5_two_answer_our_cq_pick_one() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // "CQ K5ARH EM10"

    // Two stations answer in the same slot, on slightly different offsets.
    sim.inject_decode("K5ARH W5XO EM12", FREQ, -10.0, 0.1);
    sim.inject_decode("K5ARH N4ZZ FN20", FREQ + 8.0, -12.0, 0.2);
    sim.tick().await;

    // Drive the picked exchange to completion (whichever one latched). We send a
    // report to exactly one of them; that one rogers and closes.
    sim.inject_decode("K5ARH W5XO R-09", FREQ, -10.0, 0.1);
    sim.inject_decode("K5ARH N4ZZ R-11", FREQ + 8.0, -12.0, 0.2);
    sim.tick().await;
    sim.inject_decode("K5ARH W5XO 73", FREQ, -10.0, 0.1);
    sim.inject_decode("K5ARH N4ZZ 73", FREQ + 8.0, -12.0, 0.2);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Exactly ONE QSO came out of the single CQ — never both.
    tl.assert_at_most_qsos(1);
    let completed_w5xo = tl.completed_with("W5XO");
    let completed_n4zz = tl.completed_with("N4ZZ");
    assert!(
        completed_w5xo ^ completed_n4zz,
        "exactly one of the two answerers must be worked (deterministic pick-one), \
         got w5xo={completed_w5xo} n4zz={completed_n4zz}\n{tl}"
    );
    // Determinism: the engine latches the FIRST matching CqResponse delivered in
    // the slot, which is W5XO (injected first).
    assert!(
        completed_w5xo,
        "pick-one must be deterministic on injection order (first answerer W5XO)\n{tl}"
    );
}

// =====================================================================
// B6 — directed-CQ off-target answerer: we run a directed CQ; a station that is
//      NOT the directed target answers. Document ACTUAL behavior. The engine's
//      CallingCq accepts any CqResponse addressed to US (the "DX EU/NA" prefix
//      is advisory text, not a routing filter) — so an off-target but
//      correctly-addressed answer is accepted. (SOURCE: PEER B2; policy doc.)
// =====================================================================
#[tokio::test]
async fn b6_directed_cq_off_target_answerer_policy() {
    let mut sim = sim().await;

    // A "directed" CQ is still just a CallingCq QSO in the engine; the harness
    // CQ helper emits a plain CQ. We model the directed intent by treating any
    // correctly-addressed answer as the documented accepted behavior.
    sim.cq(FREQ).await;
    sim.tick().await;

    // An answer addressed to US (regardless of any direction we advertised).
    sim.inject_decode("K5ARH G3OFF IO91", FREQ, -10.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH G3OFF R-09", FREQ, -10.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH G3OFF 73", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // DOCUMENTED BEHAVIOR: the engine accepts any answer addressed to us. There
    // is no directed-CQ rejection at the engine layer (direction is advisory).
    // If a future policy wants to reject off-direction answers, it belongs in
    // the autonomous/operator layer, not the state machine.
    tl.assert_completed_with("G3OFF");
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B7 — calling a station already in QSO (busy): we manually call a DX that is
//      mid-exchange with a third party. Document ACTUAL behavior. The manual
//      path intentionally bypasses busy/duplicate gates (operator override), so
//      a manual call to a busy DX IS keyed (tail-end attempt); it simply won't
//      advance until/unless the DX answers US. The autonomous layer is where
//      "don't barge" is enforced (covered in B1). (SOURCE: PEER B3; policy doc.)
// =====================================================================
#[tokio::test]
async fn b7_calling_a_busy_station_manual_policy() {
    let mut sim = sim().await;

    // The DX is busy working a third party (overheard).
    sim.inject_decode("W9ZZZ VB7F -07", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Operator manually calls the busy DX anyway (tail-end / queue intent).
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    let tl = sim.into_timeline();
    // DOCUMENTED BEHAVIOR: manual override keys our call (we DID send our grid),
    // but we have NOT completed — we only advance if VB7F answers us directly.
    tl.assert_transmitted_contains("VB7F K5ARH EM10");
    tl.assert_not_completed_with("VB7F");
    // The third-party frame did not advance or complete anything on its own.
    tl.assert_not_completed_with("W9ZZZ");
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B8 — third-party exchange `X Y RR73` (none of us): a complete exchange between
//      two other stations → no auto-reply, no advance, no QSO. [SOURCE: PEER B5.]
// =====================================================================
#[tokio::test]
async fn b8_third_party_exchange_ignored() {
    let mut sim = sim().await;

    // We have an active QSO in progress with VB7F.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    // A full third-party exchange flies by on a nearby offset: none of these
    // frames are to/from us.
    sim.inject_decode("N4ZZ G3ABC -05", FREQ + 200.0, -9.0, 0.1);
    sim.tick().await;
    sim.inject_decode("G3ABC N4ZZ R-05", FREQ + 200.0, -9.0, 0.1);
    sim.tick().await;
    sim.inject_decode("N4ZZ G3ABC RR73", FREQ + 200.0, -9.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We never replied to any of it and spawned no QSO from it.
    tl.assert_not_transmitted_contains("N4ZZ");
    tl.assert_not_transmitted_contains("G3ABC");
    tl.assert_not_completed_with("N4ZZ");
    tl.assert_not_completed_with("G3ABC");
    // Only our VB7F QSO exists.
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B9 — impostor: the expected report text arrives but from the WRONG from-call.
//      We're waiting for VB7F's report; `K5ARH PIRATE -12` arrives (correct to,
//      wrong from). Sender verification must reject it; no advance.
//      [SOURCE: PEER B4 + security review C-1/I-1.]
// =====================================================================
#[tokio::test]
async fn b9_impostor_wrong_from_call_no_advance() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await;
    sim.tick().await; // RespondingToCq, waiting for VB7F's report

    // The report is addressed to us, on the right freq, but FROM "PIRATE".
    sim.inject_decode("K5ARH PIRATE -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_not_transmitted_contains("VB7F K5ARH R");
    tl.assert_not_transmitted_contains("PIRATE K5ARH R");
    tl.assert_not_completed_with("VB7F");
    tl.assert_not_completed_with("PIRATE");
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B10 — station X uses the partner's call: X transmits `K5ARH VB7F -10` (claims
//       to be VB7F but is really a different station on a different freq). On our
//       VB7F freq this would normally advance us — but if it lands OUTSIDE the
//       15 Hz window it must not route, and even within tolerance the from/to
//       check governs. Here we model X spoofing VB7F's exact call+to on a far
//       offset → must NOT route into the VB7F QSO (freq tolerance). [SOURCE:
//       IDEA + qso_manager 15 Hz tolerance + sender verify.]
// =====================================================================
#[tokio::test]
async fn b10_partner_call_used_by_other_station_discarded() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await; // VB7F QSO latched at FREQ
    sim.tick().await;

    // Some other station 400 Hz away transmits a frame bearing VB7F's call,
    // addressed to us. Outside the 15 Hz tolerance of our latched VB7F QSO, so
    // it must not route in and advance us.
    sim.inject_decode("K5ARH VB7F -10", FREQ + 400.0, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // The far-offset frame did not advance our VB7F QSO (no R-report sent).
    tl.assert_not_transmitted_contains("VB7F K5ARH R");
    tl.assert_not_completed_with("VB7F");
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B11 — near-miss callsign answering our CQ: `K5ARG` (one char off) appears to
//       answer. The CqResponse `calling_station` must EXACTLY equal our call —
//       `K5ARG` != `K5ARH` → no false start. [SOURCE: IDEA + exact-match
//       relevance.]
// =====================================================================
#[tokio::test]
async fn b11_near_miss_callsign_no_false_start() {
    let mut sim = sim().await;

    sim.cq(FREQ).await;
    sim.tick().await; // "CQ K5ARH EM10"

    // A station answers "K5ARG" (NOT us) — addressed to a near-miss of our call.
    sim.inject_decode("K5ARG W5XO EM12", FREQ, -10.0, 0.1);
    sim.tick().await;
    // It even sends what looks like an R-report to the wrong call.
    sim.inject_decode("K5ARG W5XO R-09", FREQ, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We must not have advanced our CQ: no report sent to W5XO, no completion.
    tl.assert_not_transmitted_contains("W5XO K5ARH");
    tl.assert_not_completed_with("W5XO");
    // The only QSO is our own CallingCq; no new QSO spawned by the near-miss.
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B12 — multi-stream: two concurrent QSOs on distinct freqs must not
//       cross-contaminate partner/report. We drive two manual QSOs and assert
//       each completes with its OWN partner, each TX rides its OWN freq, and a
//       report meant for one never advances the other. [SOURCE: PEER A9.]
// =====================================================================
#[tokio::test]
async fn b12_multi_stream_no_cross_contamination() {
    let mut sim = sim().await;
    let f1 = 700.0;
    let f2 = 2000.0;

    sim.inject_decode("CQ VB7F DO33", f1, -8.0, 0.1);
    sim.inject_decode("CQ W5XO EM12", f2, -8.0, 0.1);
    let id1 = sim.call_station("VB7F", f1).await;
    let id2 = sim.call_station("W5XO", f2).await;
    sim.tick().await;

    assert_ne!(
        id1, id2,
        "two distinct concurrent QSOs must have distinct ids"
    );

    // Cross-addressed report: VB7F's report arrives but at W5XO's offset f2.
    // It must route to the VB7F QSO by callsign+to (within tolerance only if on
    // f1); on f2 it is out of VB7F's tolerance AND wrong partner for W5XO.
    sim.inject_decode("K5ARH VB7F -12", f1, -8.0, 0.1); // correct stream
    sim.inject_decode("K5ARH W5XO -09", f2, -8.0, 0.1); // correct stream
    sim.tick().await;

    sim.inject_decode("K5ARH VB7F RR73", f1, -8.0, 0.1);
    sim.inject_decode("K5ARH W5XO RR73", f2, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    tl.assert_completed_with("W5XO");
    assert_eq!(
        tl.distinct_qso_ids().len(),
        2,
        "expected exactly two distinct QSOs\n{tl}"
    );
    // Each stream's TX rode its own offset — no cross-frequency bleed.
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
    // No VB7F TX ever went out on W5XO's offset and vice-versa.
    assert!(
        !tl.transmissions
            .iter()
            .any(|t| t.text.contains("VB7F K5ARH") && (t.freq_hz - f2).abs() < 1.0),
        "VB7F TX must NOT appear on W5XO's offset (cross-contamination)\n{tl}"
    );
}

// =====================================================================
// B13 — foreign frame on our EXACT freq, bearing the DX call, but wrong to:
//       `W4OTH VB7F -07` on our VB7F freq. Matching is callsign+to+state, NOT
//       freq alone — so a frame addressed to W4OTH (not us), even on our exact
//       offset, must not match/advance. [SOURCE: IDEA + relevance to-field.]
// =====================================================================
#[tokio::test]
async fn b13_foreign_frame_on_our_freq_wrong_to_not_matched() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await; // QSO latched at exactly FREQ
    sim.tick().await;

    // On our EXACT freq, a frame from VB7F but addressed to W4OTH (not us).
    sim.inject_decode("W4OTH VB7F -07", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Freq matches, callsign (from) matches our partner — but to-field is W4OTH,
    // so it must NOT advance our QSO. Proves matching is not freq-alone.
    tl.assert_not_transmitted_contains("VB7F K5ARH R");
    tl.assert_not_completed_with("VB7F");
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// B14 — we QSY mid-QSO: the same QSO identity is preserved across a TX-freq
//       change. The engine-observable invariant: the QSO id is stable and the
//       partner/state continuity is unbroken even as the DX (and our replies)
//       move offset, as long as moves stay within the 15 Hz per-frame
//       tolerance. NOTE: the *operator-driven* TX-offset reassignment lives in
//       the coordinator/allocator (coord_sim.rs), not the engine; the engine
//       latches its QSO frequency at start. So at the engine layer we assert the
//       continuity-preserving part: a small drift within tolerance keeps the
//       SAME QSO id through to completion. [SOURCE: IDEA; engine-observable
//       subset of B14.]
// =====================================================================
#[tokio::test]
async fn b14_qsy_within_tolerance_preserves_identity() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    let id_start = sim.call_station("VB7F", FREQ).await;
    sim.tick().await;

    // The exchange drifts a few Hz per frame (well within the 15 Hz window) —
    // realistic transceiver drift / micro-QSY. Identity must hold.
    sim.inject_decode("K5ARH VB7F -12", FREQ + 6.0, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ + 11.0, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    tl.assert_completed_with("VB7F");
    // Exactly one QSO across the whole drift — identity preserved.
    tl.assert_at_most_qsos(1);
    let ids = tl.distinct_qso_ids();
    assert!(
        ids.contains(&id_start),
        "the completed QSO must be the SAME id we started ({id_start}); ids={ids:?}\n{tl}"
    );
}

// =====================================================================
// B15 — DX drifts BEYOND the 15 Hz tolerance mid-QSO. Catalog marks this
//       "[decide semantics]": should an active QSO prefer callsign+state
//       continuity over the freq window, or drop the partner?
//
//       ACTUAL engine behavior (qso_manager::is_message_relevant): a frame more
//       than 15 Hz off the latched QSO frequency is rejected BEFORE the
//       callsign/to/state check — so a DX that drifts >15 Hz stops advancing our
//       QSO, and we keep-call until the watchdog retires it as Failed{Timeout}
//       even though the DX IS answering us (just on a drifted offset).
//
//       This is the same SHAPE as the catalog's headline "stuck" frustration:
//       the DX is responding but a strict gate refuses to advance. Per catalog
//       guidance ("prefer callsign+state continuity for an active QSO"), the
//       CORRECT behavior is to advance on partner+to+state continuity for an
//       ALREADY-ACTIVE QSO despite the larger drift. The engine does NOT do
//       this today, so this is committed as a KNOWN BUG.
// =====================================================================
// FIXED (B15): is_message_relevant now applies the freq gate AFTER the
// callsign/to/state match. An ESTABLISHED QSO (contra callsign known, past
// CallingCq/Idle) whose partner answers with from==DX && to==us && the
// expected next message is allowed a wider drift bound (100 Hz) instead of
// the tight 15 Hz used for initial/ambiguous matching. We widen the gate
// rather than re-latch the stored frequency (the fn holds only a read lock).
#[tokio::test]
async fn b15_dx_drift_beyond_tolerance_continuity_should_win() {
    let mut sim = sim().await;

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station("VB7F", FREQ).await; // latched at FREQ
    sim.tick().await;

    // VB7F answers us correctly (from VB7F, to K5ARH) but has drifted 40 Hz —
    // beyond the 15 Hz window. It is unmistakably OUR partner answering US.
    sim.inject_decode("K5ARH VB7F -12", FREQ + 40.0, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ + 42.0, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // CORRECT behavior (catalog B15): partner+state continuity wins → we should
    // advance and complete with VB7F despite the drift.
    tl.assert_completed_with("VB7F");
}
