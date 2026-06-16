//! Autonomous-operator QSO scenario catalog — the **autonomous** counterpart
//! to `tests/qso_scenarios.rs`.
//!
//! Where `qso_scenarios.rs` drives the operator side imperatively
//! (`Sim::call_station` / `Sim::cq` / `Sim::respond_to_caller`), these tests
//! install the **real** [`AutonomousOperator`] as the decision-maker
//! ([`Sim::with_autonomous`]) and let *it* decide, slot by slot, what to do
//! with the injected band traffic. The decisions are executed against the same
//! real [`QsoManager`]; the resulting [`Timeline`] is asserted exactly as in
//! the operator-driven suite.
//!
//! Run: `cargo test -p pancetta-qso --test autonomous_scenarios`.
//!
//! # Phase-5 autonomous auto-completion (ENABLED)
//!
//! The QSO engine now auto-sequences replies for **both** manual and autonomous
//! (`CallInitiation::Auto`) QSOs. An autonomous-opened QSO
//! (`QsoManager::respond_to_cq` / `start_cq`) emits its **opening** call AND
//! auto-advances through report → R-report → RR73 → completion, and an unanswered
//! Auto pounce is RETIRED by the per-state timeout (it is intentionally NOT
//! kept-alive — an Auto call is one-shot, never a keep-call storm). Two engine
//! changes drive this (both gated to Auto QSOs by construction):
//!   1. the forward auto-reply emitter in `process_message_for_qso` fires on any
//!      forward state advance regardless of `CallInitiation` (regression handling
//!      stays Manual-only);
//!   2. `check_timeouts_at`'s per-state timeout covers `RespondingToCq` /
//!      `SendingReport` (Manual QSOs in those states use the keep-call watchdog).
//!
//! The scenarios split into two groups:
//!
//! - **Guard scenarios (G*)** validate the *decision* gates inside
//!   [`AutonomousOperator::decide_at`]: yield-to-busy-DX, duplicate suppression,
//!   pile-up pick-one, and the auto-call "no keep-call storm" property.
//!
//! - **Phase-5 scenarios (P*)** validate end-to-end autonomous *completion*:
//!   full exchange to completion, context-aware reply to a skipped rung, RR73/RRR
//!   closes, and retirement of an unanswered pounce. All pass now.

use pancetta_qso::sim::Sim;
use pancetta_qso::{
    AutonomousConfig, AutonomousOperator, DxEvaluator, ListenCycleConfig, SlotParityConfig,
};

const US: &str = "K5ARH";
const GRID: &str = "EM10";
const FREQ: f64 = 1500.0;

/// A deterministic autonomous config: fixed Even TX parity (epoch slot 0 is
/// Even, so our virtual slots line up), a very high listen interval so the
/// slot manager always *transmits* (never inserts a collision-listen slot),
/// and an immediate willingness to call CQ disabled (high idle threshold) so a
/// scenario only CQs when it explicitly wants to.
fn auto_config() -> AutonomousConfig {
    AutonomousConfig {
        enabled: true,
        slot_parity: SlotParityConfig::Even,
        cq_after_idle_cycles: 10_000, // effectively never auto-CQ in these tests
        max_concurrent_qsos: 1,
        min_dx_score: 0.3,
        listen_cycle: ListenCycleConfig {
            initial_interval: 100_000,
            ..ListenCycleConfig::default()
        },
        ..AutonomousConfig::default()
    }
}

fn operator() -> AutonomousOperator {
    AutonomousOperator::new(auto_config(), US.to_string(), Some(GRID.to_string()))
}

/// Build an autonomous-driven harness anchored to a fixed Even-parity epoch so
/// every slot's parity and virtual `now` are deterministic.
async fn auto_sim() -> Sim {
    Sim::new(US, Some(GRID)).await.with_autonomous(operator())
}

/// An evaluator that scores one favored callsign high and everything else low,
/// so the pile-up pick-one scenario has a deterministic winner.
struct FavorEvaluator {
    favored: String,
}
impl DxEvaluator for FavorEvaluator {
    fn evaluate_cq(&self, callsign: &str, _grid: Option<&str>, _snr: i8, _freq: f64) -> f64 {
        if callsign.eq_ignore_ascii_case(&self.favored) {
            0.95
        } else {
            0.40
        }
    }
}

// =====================================================================
// GUARD SCENARIOS — pass NOW (decision gates, no completion required).
// =====================================================================

// ---------------------------------------------------------------------
// G1. Auto-answer a CQ: the operator decides to pounce and OPENS the QSO.
//     (The opening grid reply is the part that works pre-Phase-5; the
//     completion is covered by the ignored P1 below.)
// ---------------------------------------------------------------------
#[tokio::test]
async fn auto_g1_answers_a_cq_and_opens_the_qso() {
    let mut sim = auto_sim().await;

    // Slot 0 (Even = our TX slot): a DX is calling CQ. The operator should
    // decide to answer and open an Auto QSO whose opening call goes on air.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Our opening grid reply went out: "VB7F K5ARH EM10".
    tl.assert_transmitted_contains("VB7F K5ARH EM10");
    // Exactly one QSO was opened for the pounce.
    tl.assert_at_most_qsos(1);
}

// ---------------------------------------------------------------------
// G2. Yield to a busy DX: the DX is mid-exchange with a third party, then
//     CQs again — the operator must NOT pounce (dx_busy gate).
// ---------------------------------------------------------------------
#[tokio::test]
async fn auto_g2_yields_to_busy_dx() {
    let mut sim = auto_sim().await;

    // Slot 0: JA1ABC is working a third party (W1XYZ) — a committed exchange
    // (report), NOT directed at us. The operator marks JA1ABC busy.
    sim.inject_decode("JA1ABC W1XYZ -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slot 1 (still Even? epoch parity alternates) — advance to the next of
    // our TX slots and have JA1ABC briefly CQ. Because it was just busy, the
    // operator must suppress the pounce.
    sim.inject_decode("CQ JA1ABC PM95", FREQ, -8.0, 0.1);
    sim.tick().await;
    // One more of our slots to be sure nothing fires late.
    sim.inject_decode("CQ JA1ABC PM95", FREQ, -8.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // We never transmitted toward JA1ABC and never opened a QSO with it.
    tl.assert_not_transmitted_contains("JA1ABC K5ARH");
    tl.assert_not_completed_with("JA1ABC");
    assert_eq!(
        tl.distinct_qso_ids().len(),
        0,
        "must not open a QSO with a busy DX\n{tl}"
    );
}

// ---------------------------------------------------------------------
// G3. Pile-up pick-one: two stations CQ in the same slot; the operator
//     opens exactly ONE QSO (the higher-scored one), deterministically.
// ---------------------------------------------------------------------
#[tokio::test]
async fn auto_g3_pileup_picks_exactly_one() {
    // Favor VB7F over K9ZZ so the winner is deterministic.
    let evaluator = Box::new(FavorEvaluator {
        favored: "VB7F".to_string(),
    });
    let mut sim = Sim::new(US, Some(GRID))
        .await
        .with_autonomous_evaluator(operator(), evaluator);

    // Slot 0: two CQs at different offsets in the SAME slot.
    sim.inject_decode("CQ VB7F DO33", 1500.0, -6.0, 0.1);
    sim.inject_decode("CQ K9ZZ EM48", 1800.0, -10.0, 0.1);
    sim.tick().await;

    let tl = sim.into_timeline();
    // Exactly one pounce opened, and it is the favored station.
    tl.assert_at_most_qsos(1);
    tl.assert_transmitted_contains("VB7F K5ARH EM10");
    tl.assert_not_transmitted_contains("K9ZZ K5ARH");
}

// ---------------------------------------------------------------------
// G4. Duplicate suppression: after answering a DX, an immediate re-CQ from
//     the SAME DX within the recently-responded window must NOT spawn a
//     second pounce / second QSO.
// ---------------------------------------------------------------------
#[tokio::test]
async fn auto_g4_does_not_rework_recently_answered_station() {
    let mut sim = auto_sim().await;

    // Slot 0: DX CQs; we pounce (opens QSO #1).
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slots 1..N: the SAME DX keeps CQing (it never heard us). Within the
    // 60 s recently-responded window the operator must not re-pounce.
    for _ in 0..3 {
        sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
        sim.tick().await;
    }

    let tl = sim.into_timeline();
    // Still exactly one QSO id across the whole run (no duplicate pounce).
    tl.assert_at_most_qsos(1);
    // And we only ever sent ONE opening call to VB7F (Auto path does not
    // keep-call, and the dupe gate blocks re-pounce).
    assert_eq!(
        tl.count_transmitted_containing("VB7F K5ARH EM10"),
        1,
        "expected exactly one opening call to VB7F (no re-pounce, no keep-call)\n{tl}"
    );
}

// ---------------------------------------------------------------------
// G5. Auto-call "no keep-call storm": unlike a MANUAL call (which keep-calls
//     every slot under the watchdog), an unanswered AUTONOMOUS pounce sends
//     its opening call ONCE and then goes quiet — the operator does not
//     hammer the DX every slot. (Validates the Auto-vs-Manual distinction;
//     the *retirement* of the dangling Auto QSO is Phase-5 — see P-watchdog.)
// ---------------------------------------------------------------------
#[tokio::test]
async fn auto_g5_unanswered_auto_call_does_not_keep_calling() {
    let mut sim = auto_sim().await;

    // Slot 0: DX CQs once; we pounce.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Many silent slots: the DX never answers and never CQs again.
    sim.tick_n(8).await;

    let tl = sim.into_timeline();
    // Exactly one transmission toward VB7F across the whole run — no per-slot
    // keep-call storm (that is manual-only behavior).
    assert_eq!(
        tl.count_transmitted_containing("VB7F"),
        1,
        "an unanswered AUTO pounce must not keep-call every slot\n{tl}"
    );
}

// =====================================================================
// PHASE-5 SCENARIOS — autonomous auto-sequencing is ENABLED; these assert
// end-to-end autonomous completion. Each is one piece of the Phase-5 behavior.
// =====================================================================

// ---------------------------------------------------------------------
// P1. Auto-answer a CQ → progress → complete + log.
// ---------------------------------------------------------------------
#[tokio::test]
// PHASE-5 (ENABLED): an autonomous (Auto) QSO opened by a pounce auto-sequences
// its reply ladder — on the DX's report we send our R-report, on RR73 we send our
// 73 and emit QsoCompleted — exactly as a MANUAL QSO does. The forward auto-reply
// emitter in `process_message_for_qso` now fires for Auto QSOs on every forward
// state advance (regression handling stays Manual-only).
async fn auto_p1_full_exchange_to_completion() {
    let mut sim = auto_sim().await;

    // Slot 0: DX CQs; operator pounces → "VB7F K5ARH EM10".
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slot 1: DX sends our report → engine SHOULD auto-send our R-report.
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slot 2: DX rogers with RR73 → engine SHOULD complete + auto-send 73.
    sim.inject_decode("K5ARH VB7F RR73", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("VB7F K5ARH EM10");
    tl.assert_transmitted_contains("VB7F K5ARH 73");
    tl.assert_completed_with("VB7F");
    tl.assert_no_duplicate_qsos();
}

// ---------------------------------------------------------------------
// P2. Context-aware: respond at the correct step to whatever the DX sent.
// ---------------------------------------------------------------------
#[tokio::test]
// PHASE-5 (ENABLED): when an autonomous QSO is open and the DX skips a rung
// (e.g. answers our grid directly with a report, or sends R-report), the engine
// auto-advances to the correct reply for what the DX actually sent (report →
// R-report; R-report → RR73), not the previous rung. The forward auto-sequencer
// now runs for Auto QSOs, so an Auto QSO advances context-aware.
async fn auto_p2_context_aware_reply_to_dx_step() {
    let mut sim = auto_sim().await;

    // Slot 0: pounce on the CQ.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Slot 1: DX skips straight to an R-report (acking a report we "sent").
    // The engine SHOULD respond at the RR73 step, not re-send our grid.
    sim.inject_decode("K5ARH VB7F R-09", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    // The correct context-aware reply to an R-report is our RR73.
    tl.assert_transmitted_contains("VB7F K5ARH RR73");
}

// ---------------------------------------------------------------------
// P3. RR73 / RRR / bare-73 closes handled in the autonomous role.
// ---------------------------------------------------------------------
#[tokio::test]
// PHASE-5 (ENABLED): in the autonomous (Auto) role, receiving the DX's RR73 (or
// RRR, or a bare 73 directed at us) on an open QSO closes it — we emit our 73 and
// complete + log. The forward auto-reply emitter now runs for Auto QSOs, so an
// Auto QSO that reaches the confirmation step auto-finishes.
async fn auto_p3_rr73_closes_the_autonomous_qso() {
    let mut sim = auto_sim().await;

    // Drive the exchange to the point the DX rogers.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.inject_decode("K5ARH VB7F -12", FREQ, -8.0, 0.1);
    sim.tick().await;
    // DX closes with RRR (the "RRR" variant of the roger).
    sim.inject_decode("K5ARH VB7F RRR", FREQ, -8.0, 0.1);
    sim.tick().await;
    sim.tick_n(2).await;

    let tl = sim.into_timeline();
    tl.assert_transmitted_contains("VB7F K5ARH 73");
    tl.assert_completed_with("VB7F");
}

// ---------------------------------------------------------------------
// P-watchdog. An unanswered auto-call eventually retires.
// ---------------------------------------------------------------------
#[tokio::test]
// PHASE-5 (ENABLED): an autonomous pounce that the DX never answers is RETIRED
// (→ Failed{Timeout}) so it doesn't linger as a zombie active QSO blocking
// max_concurrent_qsos. `check_timeouts_at`'s per-state timeout match now covers
// RespondingToCq | SendingReport (report_timeout) for Auto QSOs — Manual QSOs in
// those states are still governed by the keep-call watchdog above.
async fn auto_pwatchdog_unanswered_auto_call_retires() {
    use pancetta_qso::QsoFailureReason;

    let mut sim = auto_sim().await;

    // Slot 0: pounce.
    sim.inject_decode("CQ VB7F DO33", FREQ, -8.0, 0.1);
    sim.tick().await;

    // Wait out well past any reasonable watchdog (each tick = one 15 s slot;
    // 40 slots = 10 minutes of virtual time).
    sim.tick_n(40).await;

    // The dangling Auto QSO should have been retired and cleaned up.
    let active = sim.manager().get_active_qsos().await.len();
    let tl = sim.into_timeline();
    tl.assert_failed_with(QsoFailureReason::Timeout);
    assert_eq!(
        active, 0,
        "a never-answered auto pounce must not linger as an active QSO\n{tl}"
    );
}
