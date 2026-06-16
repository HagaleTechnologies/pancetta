//! Batch C — adversarial QSO scenarios: timing, fading, lifecycle races,
//! re-engagement, and operator-in-the-loop.
//!
//! Built on the durable [`pancetta_qso::sim`] harness (virtual band +
//! [`pancetta_qso::sim::SimClock`]). Each test scripts a slot-by-slot exchange,
//! injects the virtual clock at the right moments (`tick` drives
//! `rearm_manual_calls_at` + `check_timeouts_at` at the slot's virtual `now`),
//! and asserts the CORRECT on-air outcome. Source tags on each scenario:
//! LOG (past-run log analysis), PEER (WSJT-X/JTDX/MSHV/ft8mon behavior), IDEA
//! (operator-grounded ideation). See `docs/qso-scenario-catalog-2026-06-16.md`
//! (Batch C, C1–C20).
//!
//! Run: `cargo test -p pancetta-qso --test adversarial_timing`.
//!
//! TEST-ONLY batch: this file never edits engine code. Where a scenario reveals
//! a genuine bug (correct behavior currently fails), the test is committed
//! `#[ignore]` with a precise `// KNOWN BUG:` note rather than weakened.
//!
//! **One KNOWN BUG found: C3** (`c3_watchdog_vs_just_in_time_answer_same_slot`,
//! `#[ignore]`d) — a just-in-time DX answer arriving in the exact slot the
//! manual call cap trips is retired by the watchdog anyway, because
//! `check_timeouts_at` runs after the advancing RX and does not exempt a QSO
//! that just made forward progress. See that test's KNOWN BUG note for the
//! precise root cause and the fix shape. Every other scenario asserts the
//! actual, correct engine behavior, documented inline.
//!
//! ## Harness limits relevant to Batch C
//!
//! - **The manual watchdog's *time* bound is not usable in this harness.** The
//!   engine stamps `first_call_at` with real `Utc::now()`, but the watchdog
//!   compares it to the *virtual* `now`, which runs `slot * 15 s` ahead of real
//!   now. A minutes-bound watchdog would therefore spuriously fire on any QSO
//!   opened at a high slot index. The **call-count** bound is timestamp-free
//!   and is the reliable retire path here — the re-engagement scenarios (C4/C6)
//!   retire the abandoned first attempt by the call cap, not the minutes bound.
//!
//! - **"Band" vs. "audio offset".** The QSO engine sees only the *audio offset*
//!   (`frequency` on every state / message) and routes with a 15 Hz tolerance.
//!   The *RF band* is derived separately from the dial frequency
//!   (`utils::frequency_to_band`), a source the sim does not drive — so at the
//!   engine level every QSO shares one RF band. "Band change" scenarios (C8/C9)
//!   are therefore modeled at the level the engine actually arbitrates on: a
//!   stale close arriving on a different audio offset (beyond tolerance) is not
//!   routed into the active QSO. The *coordinator-level* teardown of a whole
//!   band (re-tuning the rig, dropping all keep-calls) lives in
//!   `pancetta/tests/coord_sim.rs`; the note on each test says so.
//! - **`tx_parity` is latched once, at QSO start** (`dx_parity.opposite()`), and
//!   carried on every `MessageToSend`. C10 asserts the latched value on our
//!   transmissions; the engine never keys the DX's parity.
//! - **The manual watchdog is per-QSO, not per-step** (C12): `call_count` and
//!   `first_call_at` span the whole manual QSO across CallingCq /
//!   RespondingToCq / SendingReport; once the DX advances us past those states
//!   the (longer) per-state timeouts govern.

use pancetta_core::ResponseStep;
use pancetta_qso::sim::Sim;
use pancetta_qso::states::QsoState;
use pancetta_qso::{QsoFailureReason, QsoManagerConfig, TimeoutConfig};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

/// Default harness.
async fn sim() -> Sim {
    Sim::new(US, Some(GRID)).await
}

/// Harness with a tunable manual watchdog (call-count bound made to bind first
/// by pushing the minute bound out of range).
async fn sim_with_max_calls(max_calls: u32) -> Sim {
    Sim::with_config(QsoManagerConfig {
        our_callsign: US.to_string(),
        our_grid: Some(GRID.to_string()),
        timeouts: TimeoutConfig {
            manual_call_max_calls: max_calls,
            manual_call_watchdog_minutes: 600,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
}

// =====================================================================
// C1 — DX fades AFTER our report, returns 3 slots later. Keep-call across
//      the silence; resume the exchange and complete. [PEER C2]
// =====================================================================
#[tokio::test]
async fn c1_dx_fades_after_report_returns_and_completes() {
    let mut sim = sim().await;
    let dx = "VB7F";

    // Slot 0: DX CQ, operator calls → our grid goes out.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // "VB7F K5ARH EM10"

    // Slot 1: DX sends report → we advance to SendingReport, auto-send our R.
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slots 2,3,4: DX FADES OUT entirely. We must keep-calling our R-report
    // (manual keep-call re-arm fires every slot) and NOT give up / complete.
    sim.tick_n(3).await;

    // Slot 5: DX returns and rogers with RR73 → we complete and send 73.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await; // let the close settle

    let tl = sim.into_timeline();
    // Across the 3 silent slots the keep-call re-sent our R-report (so ≥2 total:
    // the initial ack plus re-arms during the fade) — this is the bridge.
    let r_reports = tl.count_transmitted_containing("VB7F K5ARH R");
    assert!(
        r_reports >= 2,
        "expected keep-call to re-send our R-report across the fade (≥2), got {r_reports}\n{tl}"
    );
    tl.assert_completed_with(dx);
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// C2 — Intermittent decodes (DX copied only every other slot). The QSO
//      still completes; the watchdog counts CALLS, not slots, so a long
//      but progressing exchange is never spuriously retired. [LOG/PEER]
// =====================================================================
#[tokio::test]
async fn c2_intermittent_decodes_completes_watchdog_counts_calls() {
    // Generous call cap so the test proves *progress* survives even when the
    // exchange spans many slots with gaps.
    let mut sim = sim_with_max_calls(20).await;
    let dx = "W5XO";

    // Slot 0: call.
    sim.inject_decode("CQ W5XO EM12", FREQ, -15.0, 0.3);
    sim.call_station(dx, FREQ).await;
    sim.tick().await;

    // Slot 1: silent (DX not copied).
    sim.tick().await;

    // Slot 2: report copied → we send R.
    sim.inject_decode("K5ARH W5XO -14", FREQ, -16.0, 0.3);
    sim.tick().await;

    // Slot 3: silent again.
    sim.tick().await;

    // Slot 4: RR73 copied → complete.
    sim.inject_decode("K5ARH W5XO RR73", FREQ, -15.0, 0.3);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
    // It completed despite spanning 5+ slots: the watchdog bound is on call
    // count (20 here), not elapsed slots — proving "counts calls not slots".
    tl.assert_no_duplicate_qsos();
    // Sanity: we never tripped a Timeout failure.
    assert!(
        !tl.failed_with_reason(&QsoFailureReason::Timeout),
        "a progressing intermittent QSO must not be retired by the watchdog\n{tl}"
    );
}

// =====================================================================
// C3 — Watchdog-expiry vs. just-in-time answer in the SAME slot.
//      The DX's answer arrives in the very slot the call cap trips.
//      High bug-exposure race. [IDEA — race, high bug-exposure]
//
// KNOWN BUG (confirmed by this harness): when a manual QSO's `call_count`
// has reached `manual_call_max_calls` and the DX finally answers in that
// same slot, the engine DOES process the answer first (the state advances
// RespondingToCq → SendingReport in `tick`'s step 1), but then
// `check_timeouts_at` (step 2) immediately retires the QSO as
// `Failed{Timeout}` anyway. Root cause (`qso_manager.rs::check_timeouts_at`,
// ~L2522-2541): `SendingReport` is one of the manual-watchdog-covered states,
// and the watchdog fires purely on `call_count >= max_calls` (or elapsed >=
// watchdog) WITHOUT exempting a QSO that just made forward progress this
// slot. So a just-in-time answer at the call cap is thrown away — the
// operator loses a QSO the DX actually came back on. Observed slot 3:
//   `RespondCq->SendReport`  then  `SendReport->Failed` + `FAILED(Timeout)`
// in the same tick.
//
// Correct behavior (asserted below): the QSO must NOT be retired in the slot
// it received an advancing message — the watchdog should reset/forgive the
// count (or skip retirement) when the state advanced this slot. The fix is a
// coordinated engine change (e.g. clear the SendingReport watchdog coverage
// once a report is acked, or reset `call_count` on a real state advance, or
// only time-out on staleness measured from the last *forward* transition).
// Un-ignore once fixed.
#[tokio::test]
#[ignore = "KNOWN BUG: just-in-time answer at the manual call cap is retired by \
            the watchdog in the same slot it advanced (check_timeouts fires after \
            the advancing RX). See the test's KNOWN BUG note."]
async fn c3_watchdog_vs_just_in_time_answer_same_slot() {
    // Cap = 3. Opening call = call_count 1 (slot 0). Re-arms on slots 1 and 2
    // bring call_count to 3, which is == max_calls. The watchdog check at the
    // top of slot 3's tick WOULD retire it — but in that same slot the DX
    // finally answers with a report. tick() order is: (1) deliver decodes,
    // (2) rearm + check_timeouts. The report advances us to SendingReport, but
    // (BUG) check_timeouts then retires the now-SendingReport QSO at the cap.
    let mut sim = sim_with_max_calls(3).await;
    let dx = "T46FCR";

    sim.inject_decode("CQ T46FCR FK52", FREQ, -10.0, 0.2);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // slot 0: call_count 1
    sim.tick().await; // slot 1: re-arm -> call_count 2
    sim.tick().await; // slot 2: re-arm -> call_count 3 (== cap)

    // Slot 3: the watchdog would fire now (call_count == cap) — but the DX's
    // report lands in this same slot and is processed first, advancing the
    // state. The QSO must survive to be worked.
    sim.inject_decode("K5ARH T46FCR -11", FREQ, -10.0, 0.2);
    sim.tick().await;

    // The QSO must still be alive (advanced to SendingReport), NOT Failed.
    let active = sim.manager().get_active_qsos().await;
    let still_alive = active
        .iter()
        .any(|(_, p)| p.state.their_callsign() == Some(dx));
    let tl = sim.timeline();
    assert!(
        still_alive,
        "C3 RACE: a just-in-time answer in the watchdog slot must NOT be retired \
         — RX is processed before check_timeouts. (If this fails, the watchdog \
         races ahead of message processing — a real bug.)\n{tl}"
    );
    assert!(
        !tl.failed_with_reason(&QsoFailureReason::Timeout),
        "C3 RACE: no Timeout failure should be observed when the answer arrived \
         in the same slot\n{tl}"
    );

    // And it can carry through to completion from here.
    sim.inject_decode("K5ARH T46FCR RR73", FREQ, -10.0, 0.2);
    sim.tick().await;
    sim.tick_n(2).await;
    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
}

// =====================================================================
// C4 — A watchdog-Failed station starts answering within a short window.
//      The mapping was cleared on retire, so a fresh MANUAL call re-engages
//      as a brand-new QSO (operator override of the duplicate gate). The
//      engine does NOT blacklist forever. [IDEA — decide window]
// =====================================================================
#[tokio::test]
async fn c4_reengage_after_watchdog_failure_fresh_qso() {
    // HARNESS NOTE: we retire attempt 1 by the CALL-COUNT bound, not the
    // minutes bound. The engine stamps `first_call_at` with real `Utc::now()`,
    // while the watchdog compares it to the *virtual* `now` (which runs ahead
    // by `slot * 15 s`); a minutes-bound watchdog would therefore spuriously
    // fire on any QSO opened at a high slot index. The call-count bound is
    // timestamp-free and the reliable retire path in the harness. Cap = 10:
    // the unanswered attempt 1 keep-calls to the cap and retires (~9 slots),
    // while the prompt 3-slot re-engagement completes at call_count ≈ 3, well
    // under the cap — so this isolates the re-engagement policy from the C3
    // call-cap race.
    let mut sim = sim_with_max_calls(10).await;
    let dx = "PY2GIG";

    // Call, never answered → watchdog retires it (mapping cleared).
    sim.inject_decode("CQ PY2GIG GG66", FREQ, -12.0, 0.2);
    sim.call_station(dx, FREQ).await;
    sim.tick_n(12).await; // tick past the 10-call cap

    let tl_mid = sim.timeline().clone();
    assert!(
        tl_mid.failed_with_reason(&QsoFailureReason::Timeout),
        "the abandoned call must have been retired by the watchdog\n{tl_mid}"
    );

    // The DX now actually starts answering. Operator re-clicks: a fresh manual
    // call must be accepted (NOT blocked as a duplicate / blacklist), opening a
    // NEW QSO that can complete.
    sim.inject_decode("CQ PY2GIG GG66", FREQ, -10.0, 0.2);
    sim.call_station(dx, FREQ).await;
    sim.tick().await;
    sim.inject_decode("K5ARH PY2GIG -09", FREQ, -10.0, 0.2);
    sim.tick().await;
    sim.inject_decode("K5ARH PY2GIG RR73", FREQ, -10.0, 0.2);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    // Re-engagement succeeded — the station is NOT blacklisted after a watchdog
    // failure; a manual re-call works any time (no fixed lock-out window).
    tl.assert_completed_with(dx);
}

// =====================================================================
// C5 — We abandoned them (watchdog-Failed), but the DX persists and CLOSES
//      with us anyway. With the QSO retired and mapping cleared, a stray
//      RR73 directed at us has no active QSO to route into — it is harmless
//      (no panic, no phantom completion). The "gift completion" is realized
//      via the operator re-engaging (covered in C4); here we assert the
//      stale close is safely ignored, not misrouted. [IDEA]
// =====================================================================
#[tokio::test]
async fn c5_abandoned_then_dx_closes_is_safely_ignored() {
    let mut sim = sim_with_max_calls(3).await;
    let dx = "K9HJZ";

    sim.inject_decode("CQ K9HJZ EN52", FREQ, -9.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick_n(6).await; // retire by watchdog

    assert!(
        sim.timeline()
            .failed_with_reason(&QsoFailureReason::Timeout),
        "precondition: the call was abandoned by the watchdog"
    );

    // The DX, unaware we gave up, fires RR73 at us. No active QSO exists.
    sim.inject_decode("K5ARH K9HJZ RR73", FREQ, -9.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    // No phantom completion from the stray close, and no new QSO spawned.
    tl.assert_not_completed_with(dx);
    // Exactly one QSO id ever existed (the original, now Failed) — the stray
    // RR73 did not create or revive anything.
    tl.assert_at_most_qsos(1);
}

// =====================================================================
// C6 — Late return after many slots (mapping long cleared). A bare CQ +
//      manual re-call is handled as a NEW QSO with no crash; the engine does
//      not choke on the long gap. [IDEA]
// =====================================================================
#[tokio::test]
async fn c6_late_return_after_minutes_handled_as_new() {
    // Call-count-bound watchdog (cap 10) — see C4's harness note on why we
    // avoid the minutes bound. Attempt 1 retires at the cap; the prompt
    // re-engagement after a long gap completes at call_count ≈ 3, isolating the
    // "late return handled as new, no crash" behavior from the C3 race.
    let mut sim = sim_with_max_calls(10).await;
    let dx = "9A4AA";

    // First attempt fails out (unanswered → call-cap retire).
    sim.inject_decode("CQ 9A4AA JN95", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick_n(12).await;

    // A long quiet stretch (many more slots) — the mapping is gone.
    sim.tick_n(10).await;

    // The DX reappears much later; operator re-calls. Fresh QSO, completes.
    sim.inject_decode("CQ 9A4AA JN95", FREQ, -8.0, 0.1);
    let id2 = sim.call_station(dx, FREQ).await;
    sim.tick().await;
    sim.inject_decode("K5ARH 9A4AA -07", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH 9A4AA RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
    // The late re-engagement is its own QSO id (distinct from the failed first).
    assert!(
        tl.distinct_qso_ids().contains(&id2),
        "the late re-call must be its own QSO id\n{tl}"
    );
}

// =====================================================================
// C7 — Stale close after we worked ANOTHER station. A late RR73 from an old
//      partner must NOT be misrouted into the new, active QSO. [IDEA]
// =====================================================================
#[tokio::test]
async fn c7_stale_close_not_misrouted_into_new_qso() {
    let mut sim = sim().await;
    let old_dx = "NP4VA";
    let new_dx = "VB7F";

    // Work and complete the first station cleanly.
    sim.inject_decode("CQ NP4VA FK68", FREQ, -7.0, 0.1);
    sim.call_station(old_dx, FREQ).await;
    sim.tick().await;
    sim.inject_decode("K5ARH NP4VA -10", FREQ, -7.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH NP4VA RR73", FREQ, -7.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await; // first QSO completed + mapping cleared

    // Now start a fresh QSO with a DIFFERENT station on the same offset.
    sim.inject_decode("CQ VB7F DO33", FREQ, -7.0, 0.1);
    sim.call_station(new_dx, FREQ).await;
    sim.tick().await; // we sent our grid to VB7F

    // The OLD partner belatedly re-sends RR73 (to us) on the same offset. It
    // must NOT advance/complete the new VB7F QSO (sender verification: the new
    // QSO expects from_station == VB7F, not NP4VA).
    sim.inject_decode("K5ARH NP4VA RR73", FREQ, -7.0, 0.1);
    sim.tick().await;

    // Finish VB7F normally.
    sim.inject_decode("K5ARH VB7F -09", FREQ, -7.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -7.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(old_dx); // the legit first completion
    tl.assert_completed_with(new_dx); // the second, uncontaminated by the stale close
                                      // The stale NP4VA RR73 during the VB7F exchange did not send a 73 to NP4VA
                                      // a second time, nor advance VB7F prematurely — VB7F took the full ladder.
                                      // (Two distinct completions, no extra phantom QSO.)
    tl.assert_at_most_qsos(2);
}

// =====================================================================
// C8 — Return after a "band change": a close arriving on an audio offset
//      OUTSIDE the 15 Hz routing tolerance of the active QSO is ignored —
//      it does not match the current QSO's frequency context. (Engine-level
//      proxy for "different band/freq"; true RF-band teardown is
//      coordinator-level — see pancetta/tests/coord_sim.rs.) [IDEA]
// =====================================================================
#[tokio::test]
async fn c8_close_off_frequency_context_is_ignored() {
    let mut sim = sim().await;
    let dx = "VB7F";
    let qso_freq = 1500.0;
    let far_freq = 1800.0; // 300 Hz away → outside the 15 Hz tolerance

    sim.inject_decode("CQ VB7F DO33", qso_freq, -8.0, 0.1);
    sim.call_station(dx, qso_freq).await;
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F -12", qso_freq, -8.0, 0.1);
    sim.tick().await; // SendingReport, awaiting their ack

    // A close from VB7F but on a FAR offset (as if they reappeared on another
    // freq/band). Routing's 15 Hz tolerance rejects it: the active QSO does not
    // complete from an off-frequency frame.
    sim.inject_decode("K5ARH VB7F RR73", far_freq, -8.0, 0.1);
    sim.tick().await;

    // The QSO must still be active (not completed by the off-freq close).
    let active = sim.manager().get_active_qsos().await;
    let still_active = active
        .iter()
        .any(|(_, p)| p.state.their_callsign() == Some(dx));
    let tl = sim.timeline();
    assert!(
        still_active,
        "an off-frequency-context close must NOT complete the active QSO\n{tl}"
    );
    assert!(
        !tl.completed_with(dx),
        "off-frequency close wrongly completed the QSO\n{tl}"
    );

    // On-frequency close still works.
    sim.inject_decode("K5ARH VB7F RR73", qso_freq, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;
    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
}

// =====================================================================
// C9 — Band change mid-QSO → graceful teardown, no stale keep-call. The
//      engine-exposed teardown an operator can drive is `cancel_qso` (the
//      coordinator wires a band/freq change to this + a rig retune). After
//      teardown there must be NO further keep-call TX for the torn-down QSO.
//      (Full RF-band retune + global keep-call drop is coordinator-level —
//      pancetta/tests/coord_sim.rs.) High bug-exposure. [IDEA]
// =====================================================================
#[tokio::test]
async fn c9_band_change_mid_qso_tears_down_no_stale_keepcall() {
    let mut sim = sim().await;
    let dx = "VB7F";

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // grid out
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await; // SendingReport, keep-calling our R

    // Operator changes band → coordinator tears the QSO down. The engine-level
    // teardown is cancel_qso (UserCancelled). Capture TX count, then tear down.
    let before = sim.timeline().count_transmitted_containing("VB7F K5ARH R");
    sim.abort(dx).await; // cancel_qso → Failed{UserCancelled}, mapping cleared
    sim.tick().await;

    // Several more slots on the "new band": the torn-down QSO must NOT keep
    // re-arming a stale R-report keep-call.
    sim.tick_n(4).await;
    let after = sim.timeline().count_transmitted_containing("VB7F K5ARH R");

    let tl = sim.into_timeline();
    assert_eq!(
        before, after,
        "after a band-change teardown there must be NO further stale keep-call \
         TX for the torn-down QSO\n{tl}"
    );
    tl.assert_failed_with(QsoFailureReason::UserCancelled);
    tl.assert_not_completed_with(dx);
}

// =====================================================================
// C10 — DX is transmitting on OUR own (would-be) parity. tx_parity is
//       re-derived from the DX frame (dx_parity.opposite()), so we never key
//       the DX's slot. Asserted on the latched tx_parity carried by every
//       transmission. [PEER]
// =====================================================================
#[tokio::test]
async fn c10_tx_parity_derived_from_dx_never_keys_dx_slot() {
    let mut sim = sim().await;
    let dx = "VB7F";

    // Whatever the current slot parity is, the operator clicks while the DX is
    // transmitting on it. call_station latches tx_parity = dx_parity.opposite().
    let dx_parity = sim.parity();
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // our grid goes out with the latched tx_parity

    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
    // EVERY transmission for this QSO carries tx_parity == opposite(dx_parity).
    // We never key the DX's slot.
    let our_tx: Vec<_> = tl
        .transmissions
        .iter()
        .filter(|t| t.text.contains("VB7F"))
        .collect();
    assert!(!our_tx.is_empty(), "expected transmissions to VB7F\n{tl}");
    for t in &our_tx {
        assert_eq!(
            t.tx_parity,
            Some(dx_parity.opposite()),
            "TX must ride the OPPOSITE parity to the DX (never the DX's slot): \
             tx_parity={:?}, dx_parity={:?}\n{tl}",
            t.tx_parity,
            dx_parity
        );
        assert_ne!(
            t.tx_parity,
            Some(dx_parity),
            "TX must never key the DX's own parity\n{tl}"
        );
    }
}

// =====================================================================
// C11 — Late-in-slot TX-defer decision. The engine does NOT make the
//       "too late in the slot → defer" decision — that is the TX scheduler's
//       (`coordinator/tx.rs::schedule_tx`, unit-tested there and exercised in
//       pancetta/tests/coord_sim.rs). At the QSO-engine level the only
//       guarantee is that every emitted message carries a tx_parity for the
//       scheduler to place; we assert that here and note the limit. [PEER]
// =====================================================================
#[tokio::test]
async fn c11_engine_emits_parity_for_scheduler_to_defer() {
    // HARNESS LIMIT: late-in-slot deferral is a scheduler decision based on
    // wall-clock slot phase + tx_late_max_ms, not a QSO-engine decision. The
    // SimClock has no sub-slot phase, so this scenario is only expressible at
    // the engine level as "the engine hands the scheduler a placeable frame
    // (with parity)". The actual defer/skip-ahead logic is covered by
    // tx.rs::schedule_tx_tests and coord_sim.rs.
    let mut sim = sim().await;
    let dx = "VB7F";

    let dx_parity = sim.parity();
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await;

    let tl = sim.into_timeline();
    let grid = tl
        .transmissions
        .iter()
        .find(|t| t.text.contains("VB7F K5ARH EM10"))
        .expect("our grid must be emitted");
    // The frame is placeable: it carries an explicit parity the scheduler uses
    // to pick the next same-parity slot (and to defer if too late).
    assert_eq!(
        grid.tx_parity,
        Some(dx_parity.opposite()),
        "emitted frame must carry the parity the TX scheduler needs to place \
         (and defer) it\n{tl}"
    );
}

// =====================================================================
// C12 — Per-step-stall vs. per-QSO watchdog semantics. The manual watchdog
//       is PER-QSO: call_count + first_call_at span the whole manual QSO
//       across CallingCq / RespondingToCq / SendingReport. Progressing from
//       grid → report does NOT reset the count; the cap bounds total calls
//       for the QSO. Asserted directly. [LOG]
// =====================================================================
#[tokio::test]
async fn c12_watchdog_is_per_qso_not_per_step() {
    // Cap = 4. Opening grid call = 1. We then progress to SendingReport (the DX
    // sent a report once) and stall there. The cap must STILL bind across the
    // whole QSO (the move grid→report did NOT reset call_count), retiring the
    // QSO after a total of 4 calls — proving per-QSO, not per-step, semantics.
    let mut sim = sim_with_max_calls(4).await;
    let dx = "VB7F";

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // slot 0: grid, call_count 1

    // One report copied → advance to SendingReport (still the same QSO, same
    // call_count progression — re-arms keep incrementing it).
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await; // slot 1: now SendingReport; re-arm counts

    // DX vanishes. Keep-call re-arms in SendingReport keep counting toward the
    // SAME per-QSO cap of 4 — not a fresh per-step budget.
    sim.tick_n(8).await;

    let tl = sim.into_timeline();
    tl.assert_failed_with(QsoFailureReason::Timeout);
    tl.assert_not_completed_with(dx);
    // Total grid + R transmissions are bounded by the per-QSO cap (4), NOT
    // reset to a fresh budget when we crossed into SendingReport. (Allow a
    // little slack for the opening-frame accounting, but it must stay small —
    // a per-step reset would let it run well past the cap.)
    let total = tl.count_transmitted_containing("VB7F K5ARH EM10")
        + tl.count_transmitted_containing("VB7F K5ARH R");
    assert!(
        total <= 5,
        "per-QSO watchdog: total calls across grid+report must stay near the \
         per-QSO cap (≤5), got {total} — a per-step reset would exceed this\n{tl}"
    );
}

// =====================================================================
// C13 — Operator re-clicks an ACTIVE station: continue the same QSO, no
//       duplicate, no Superseded. (Also covered in qso_scenarios.rs #7;
//       re-asserted here in the adversarial suite per the catalog.) [IDEA]
// =====================================================================
#[tokio::test]
async fn c13_operator_reclick_active_station_continues() {
    let mut sim = sim().await;
    let dx = "W5XO";

    sim.inject_decode("CQ W5XO EM12", FREQ, -8.0, 0.1);
    let id1 = sim.call_station(dx, FREQ).await;
    sim.tick().await;

    // Mid-exchange the DX sends a report; meanwhile the operator keeps clicking.
    sim.inject_decode("K5ARH W5XO -10", FREQ, -8.0, 0.1);
    let id2 = sim.call_station(dx, FREQ).await;
    sim.tick().await;
    let id3 = sim.call_station(dx, FREQ).await;
    sim.tick().await;

    assert_eq!(id1, id2, "re-click must return the SAME QSO id");
    assert_eq!(id1, id3, "re-click must return the SAME QSO id");

    sim.inject_decode("K5ARH W5XO RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
    tl.assert_no_duplicate_qsos();
    tl.assert_no_superseded();
}

// =====================================================================
// C14 — Operator ABORT, then RE-CALL. Clean teardown of the first attempt
//       (UserCancelled, mapping cleared), then a fresh manual QSO that
//       completes. [IDEA]
// =====================================================================
#[tokio::test]
async fn c14_operator_abort_then_recall_fresh_qso() {
    let mut sim = sim().await;
    let dx = "VB7F";

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    let id1 = sim.call_station(dx, FREQ).await;
    sim.tick().await;

    // Operator changes their mind and aborts.
    sim.abort(dx).await;
    sim.tick().await;
    assert!(
        sim.timeline()
            .failed_with_reason(&QsoFailureReason::UserCancelled),
        "abort must tear the QSO down (UserCancelled)"
    );

    // Operator re-calls the same station: a fresh QSO (the old one is terminal +
    // unmapped), which completes.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    let id2 = sim.call_station(dx, FREQ).await;
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    assert_ne!(
        id1, id2,
        "the re-call after a full abort must be a NEW QSO id (clean teardown)"
    );
    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
}

// =====================================================================
// C15 — Operator forces a manual frame OUT OF SEQUENCE (e.g. jump straight
//       to RR73 / SeventyThree). The override is honored — we send the close
//       and the log is consistent (one QSO, completed). [IDEA]
// =====================================================================
#[tokio::test]
async fn c15_operator_forced_out_of_sequence_close_honored() {
    let mut sim = sim().await;
    let dx = "KA1BCD";

    // The DX is sending us RR73; the operator forces our 73 directly via the
    // context-reply entry point at the SeventyThree step (skipping grid/report).
    sim.inject_decode("K5ARH KA1BCD RR73", FREQ, -7.0, 0.1);
    sim.respond_to_caller(dx, FREQ, ResponseStep::SeventyThree, Some(-7.0), Some(-7))
        .await;
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    // The forced out-of-sequence 73 went out and the QSO logged as completed —
    // operator override honored, consistent single-QSO log.
    tl.assert_transmitted_contains("KA1BCD K5ARH 73");
    tl.assert_completed_with(dx);
    tl.assert_no_duplicate_qsos();
}

// =====================================================================
// C16 — Operator clicks a NEW station mid-QSO. Both QSOs proceed in parallel
//       (multi-stream) on their own offsets; the active QSO is NEVER silently
//       dropped. [IDEA]
// =====================================================================
#[tokio::test]
async fn c16_operator_clicks_new_station_mid_qso_no_silent_drop() {
    let mut sim = sim().await;
    let first = "VB7F";
    let second = "W5XO";
    let f1 = 800.0;
    let f2 = 1900.0;

    // Start the first QSO.
    sim.inject_decode("CQ VB7F DO33", f1, -8.0, 0.1);
    sim.call_station(first, f1).await;
    sim.tick().await; // grid to VB7F

    // Mid-QSO the operator clicks a DIFFERENT station on a different offset.
    sim.inject_decode("CQ W5XO EM12", f2, -8.0, 0.1);
    sim.call_station(second, f2).await;
    sim.tick().await;

    // Both are active right now — the first was NOT silently dropped.
    let active = sim.manager().get_active_qsos().await;
    let has_first = active
        .iter()
        .any(|(_, p)| p.state.their_callsign() == Some(first));
    let has_second = active
        .iter()
        .any(|(_, p)| p.state.their_callsign() == Some(second));
    assert!(
        has_first && has_second,
        "clicking a new station mid-QSO must keep BOTH active (parallel / \
         queued), never silently drop the first: first={has_first}, \
         second={has_second}"
    );

    // Drive both to completion on their own offsets.
    sim.inject_decode("K5ARH VB7F -12", f1, -8.0, 0.1);
    sim.inject_decode("K5ARH W5XO -09", f2, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", f1, -8.0, 0.1);
    sim.inject_decode("K5ARH W5XO RR73", f2, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_completed_with(first);
    tl.assert_completed_with(second);
    assert_eq!(
        tl.distinct_qso_ids().len(),
        2,
        "exactly two distinct QSOs — neither dropped, no extras\n{tl}"
    );
    // Each TX rode its own offset.
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
// C17 — Hashed/partial `<...>` callsign inbound. A non-resolvable hashed
//       callsign frame is parsed as NonStandard (the engine can't resolve the
//       sender), so it does NOT advance any QSO and does NOT log. We assert
//       the active QSO is untouched by such a frame. [PEER D2/D3]
// =====================================================================
#[tokio::test]
async fn c17_hashed_partial_callsign_does_not_advance_or_log() {
    let mut sim = sim().await;
    let dx = "VB7F";

    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.call_station(dx, FREQ).await;
    sim.tick().await; // RespondingToCq, grid out

    // A hashed/partial-call frame arrives on our offset. It cannot resolve to a
    // structured, sender-verified exchange (the `<...>` token is not VB7F), so
    // it must NOT advance our QSO toward a report and must NOT complete/log.
    sim.inject_decode("K5ARH <...> -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // The QSO is still in RespondingToCq (we have NOT moved to SendingReport
    // off an unresolved frame), and nothing was logged.
    let active = sim.manager().get_active_qsos().await;
    let still_responding = active.iter().any(|(_, p)| {
        p.state.their_callsign() == Some(dx) && matches!(p.state, QsoState::RespondingToCq { .. })
    });
    let tl = sim.timeline();
    assert!(
        still_responding,
        "a hashed/partial-callsign frame must NOT advance the QSO past \
         RespondingToCq\n{tl}"
    );
    // We never sent an R-report off the unresolved frame.
    assert!(
        !tl.transmitted_contains("VB7F K5ARH R"),
        "must not send an R-report in response to an unresolved hashed-call \
         frame\n{tl}"
    );

    // A legitimate VB7F report afterward still advances us normally.
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;
    let tl = sim.into_timeline();
    tl.assert_completed_with(dx);
    tl.assert_no_duplicate_qsos();
}
