use super::{
    compute_manual_tx_offset, compute_spot_call_tx_offset, drain_parked_calls_on_band_change,
    partition_pending_calls, PendingManualCall, QueueStamp,
};
use pancetta_core::slot::SlotParity;
use std::collections::VecDeque;

// Build a pending call whose DX is on `dx`, so its desired TX parity is the
// opposite.
fn call(name: &str, dx: SlotParity) -> PendingManualCall {
    PendingManualCall {
        callsign: name.to_string(),
        frequency_hz: 1500.0,
        dx_parity: Some(dx),
        queued_at: QueueStamp::now(),
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 0,
        hold_mode: false,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    }
}

fn names(v: &[PendingManualCall]) -> Vec<String> {
    v.iter().map(|p| p.callsign.clone()).collect()
}

#[test]
fn dx_above_ceiling_clamps_tx_and_sets_partner_freq() {
    let (tx_off, partner) = compute_manual_tx_offset(2931.0, false, 0, &[]);
    assert_eq!(tx_off, pancetta_qso::TX_OFFSET_MAX_HZ);
    assert_eq!(partner, Some(2931.0));
}

#[test]
fn dx_below_floor_clamps_tx_and_sets_partner_freq() {
    let (tx_off, partner) = compute_manual_tx_offset(180.0, false, 0, &[]);
    assert_eq!(tx_off, pancetta_qso::TX_OFFSET_MIN_HZ);
    assert_eq!(partner, Some(180.0));
}

#[test]
fn held_operator_endpoints_survive_manual_placement() {
    for held in [200, 2800] {
        assert_eq!(
            compute_manual_tx_offset(1500.0, true, held, &[]),
            (held as f64, Some(1500.0)),
        );
        assert_eq!(
            compute_manual_tx_offset(held as f64, true, held, &[]),
            (held as f64, None),
        );
    }
}

#[test]
fn held_operator_endpoints_deconflict_inside_operator_range() {
    for held in [200, 2800] {
        let occupied = [held as f64, if held == 200 { 250.0 } else { 2750.0 }];
        let (offset, partner) = compute_manual_tx_offset(1500.0, true, held, &occupied);
        assert!((200.0..=2800.0).contains(&offset));
        assert!(occupied
            .iter()
            .all(|busy| (offset - busy).abs() >= pancetta_qso::MIN_TX_SEPARATION_HZ));
        assert_eq!(partner, Some(1500.0));
    }
}

#[test]
fn held_zero_retains_auto_clamps() {
    for (dx, expected) in [(180.0, 300.0), (2931.0, 2700.0)] {
        assert_eq!(
            compute_manual_tx_offset(dx, true, 0, &[]),
            (expected, Some(dx))
        );
    }
}

// PAN-287: a network-spot call with no DX offset keeps the allocator's
// pick and listens there (placeholder partner_freq = our TX).
#[test]
fn spot_call_offset_keeps_allocator_pick_and_sets_placeholder_partner() {
    assert_eq!(
        compute_spot_call_tx_offset(1000.0, &[]),
        (1000.0, Some(1000.0))
    );
}

#[test]
fn spot_call_offset_is_deconflicted_from_active_qsos() {
    let (tx, partner) = compute_spot_call_tx_offset(1000.0, &[1020.0]);
    assert!((tx - 1020.0).abs() >= pancetta_qso::MIN_TX_SEPARATION_HZ);
    assert_eq!(partner, Some(tx));
}

// PAN-287: a band change drops every parked cross-parity call, in queue
// order, and leaves the queue empty.
#[test]
fn band_change_drains_parked_calls_in_queue_order() {
    let mut q: VecDeque<_> = [
        caller_call("K1AAA", SlotParity::Even, pancetta_core::ResponseStep::Grid),
        caller_call("K2BBB", SlotParity::Odd, pancetta_core::ResponseStep::Grid),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        drain_parked_calls_on_band_change(&mut q),
        vec!["K1AAA".to_string(), "K2BBB".to_string()]
    );
    assert!(q.is_empty());
}

// Build a RespondToCaller-shaped pending call (non-Grid step) whose DX is
// on `dx`, so its desired TX parity is the opposite. Used to verify a
// `RespondToCaller` entry queued by the admission gate participates in
// exactly the SAME cross-parity partition logic as a `StartQso` entry
// (`call` above), not a separate/duplicated path.
fn caller_call(name: &str, dx: SlotParity, step: pancetta_core::ResponseStep) -> PendingManualCall {
    PendingManualCall {
        callsign: name.to_string(),
        frequency_hz: 1500.0,
        dx_parity: Some(dx),
        queued_at: QueueStamp::now(),
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 0,
        hold_mode: false,
        remote_origin: false,
        remote_client_key_id: None,
        step,
        our_snr_of_them: Some(-8.0),
        their_report: None,
    }
}

/// A `RespondToCaller`-shaped entry (step = Report, i.e. NOT the
/// `StartQso`-equivalent `Grid` step) that wants the opposite parity from
/// our committed side stays queued — it is gated by the SAME half-duplex
/// admission logic as a `StartQso` entry, not bypassed.
#[test]
fn respond_to_caller_shaped_entry_stays_queued_on_opposite_side() {
    let q: VecDeque<_> = [caller_call(
        "DX1",
        SlotParity::Even, // wants Odd
        pancetta_core::ResponseStep::Report,
    )]
    .into();
    // Committed to Even already: the Odd-wanting caller-response stays queued.
    let (start, keep) = partition_pending_calls(q, Some(SlotParity::Even), usize::MAX);
    assert!(
        start.is_empty(),
        "opposite-parity caller-response must not start"
    );
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["DX1"]);
}

/// The same `RespondToCaller`-shaped entry starts once the committed side
/// flips to match it (mirrors what promotion after a QSO going terminal
/// looks like from the partition step's point of view).
#[test]
fn respond_to_caller_shaped_entry_starts_when_side_matches() {
    let q: VecDeque<_> = [caller_call(
        "DX1",
        SlotParity::Even, // wants Odd
        pancetta_core::ResponseStep::Report,
    )]
    .into();
    let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd), usize::MAX);
    assert_eq!(names(&start), vec!["DX1"]);
    assert!(keep.is_empty());
}

#[test]
fn idle_adopts_oldest_then_starts_all_same_side() {
    // DX Even ⇒ we TX Odd; DX Odd ⇒ we TX Even.
    let q: VecDeque<_> = [
        call("A", SlotParity::Even), // want Odd
        call("B", SlotParity::Odd),  // want Even
        call("C", SlotParity::Even), // want Odd
    ]
    .into();
    // Idle: adopt oldest (A wants Odd). A & C start, B stays.
    let (start, keep) = partition_pending_calls(q, None, usize::MAX);
    assert_eq!(names(&start), vec!["A", "C"]);
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
}

#[test]
fn committed_side_only_adds_same_side() {
    let q: VecDeque<_> = [
        call("A", SlotParity::Even), // want Odd
        call("B", SlotParity::Odd),  // want Even
    ]
    .into();
    // We're committed to Odd already: only A (wants Odd) joins; B waits.
    let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd), usize::MAX);
    assert_eq!(names(&start), vec!["A"]);
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
}

#[test]
fn committed_side_with_no_match_keeps_all() {
    let q: VecDeque<_> = [call("B", SlotParity::Odd)].into(); // wants Even
                                                              // Committed to Odd, only an Even-wanting call queued → nothing promotes.
    let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd), usize::MAX);
    assert!(start.is_empty());
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
}

#[test]
fn empty_queue_is_noop() {
    let (start, keep) = partition_pending_calls(VecDeque::new(), None, usize::MAX);
    assert!(start.is_empty());
    assert!(keep.is_empty());
}

// ── PAN-78: capacity-bounded partition ───────────────────────────────

#[test]
fn partition_with_one_free_slot_starts_only_the_oldest_matching() {
    let q: VecDeque<_> = [
        call("A", SlotParity::Odd),  // want Even
        call("B", SlotParity::Odd),  // want Even
        call("C", SlotParity::Even), // want Odd
    ]
    .into();
    // Idle: adopt A's side (Even). A and B match, but only one slot is free.
    let (start, keep) = partition_pending_calls(q, None, 1);
    assert_eq!(names(&start), vec!["A"]);
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B", "C"]);
}

#[test]
fn partition_with_zero_free_slots_starts_nothing_and_keeps_order() {
    let q: VecDeque<_> = [
        call("A", SlotParity::Odd),
        call("B", SlotParity::Even),
        call("C", SlotParity::Odd),
    ]
    .into();
    let (start, keep) = partition_pending_calls(q, None, 0);
    assert!(start.is_empty());
    assert_eq!(
        names(&keep.into_iter().collect::<Vec<_>>()),
        vec!["A", "B", "C"]
    );
}

#[test]
fn partition_skips_cross_parity_without_spending_slots() {
    let q: VecDeque<_> = [
        call("X", SlotParity::Odd),  // want Even (cross-parity)
        call("A", SlotParity::Even), // want Odd
        call("B", SlotParity::Even), // want Odd
    ]
    .into();
    // Committed to Odd: X is skipped without consuming the one free slot.
    let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd), 1);
    assert_eq!(names(&start), vec!["A"]);
    assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["X", "B"]);
}

// ── TTL / partition_expired tests ────────────────────────────────────────

use super::partition_expired;

/// Build a call whose `queued_at` is `age` in the past on both clocks.
fn aged_call(name: &str, dx: SlotParity, age: std::time::Duration) -> PendingManualCall {
    let now = QueueStamp::now();
    PendingManualCall {
        callsign: name.to_string(),
        frequency_hz: 1500.0,
        dx_parity: Some(dx),
        queued_at: QueueStamp {
            mono: now
                .mono
                .checked_sub(age)
                .expect("Instant underflow in test"),
            wall: now.wall - chrono::Duration::from_std(age).expect("age fits chrono"),
        },
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 0,
        hold_mode: false,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    }
}

#[test]
fn fresh_calls_are_kept() {
    let ttl = std::time::Duration::from_secs(600);
    let q: VecDeque<_> = [call("A", SlotParity::Even), call("B", SlotParity::Odd)].into();
    let now = QueueStamp::now();
    let (kept, expired) = partition_expired(q, now, ttl);
    assert_eq!(kept.len(), 2);
    assert!(expired.is_empty());
}

#[test]
fn expired_calls_are_removed_and_returned() {
    let ttl = std::time::Duration::from_secs(600);
    let q: VecDeque<_> = [
        // A: 11 min old — expired
        aged_call("A", SlotParity::Even, std::time::Duration::from_secs(660)),
        // B: 5 min old — still fresh
        aged_call("B", SlotParity::Odd, std::time::Duration::from_secs(300)),
        // C: 12 min old — expired
        aged_call("C", SlotParity::Even, std::time::Duration::from_secs(720)),
    ]
    .into();
    let now = QueueStamp::now();
    let (kept, expired) = partition_expired(q, now, ttl);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].callsign, "B");
    assert_eq!(expired, vec!["A", "C"]);
}

#[test]
fn exact_ttl_boundary_expires() {
    // A call that is exactly TTL old should be retired (>=, not >).
    let ttl = std::time::Duration::from_secs(600);
    let q: VecDeque<_> = [aged_call("X", SlotParity::Even, ttl)].into();
    let now = QueueStamp::now();
    let (kept, expired) = partition_expired(q, now, ttl);
    assert!(kept.is_empty());
    assert_eq!(expired, vec!["X"]);
}

#[test]
fn empty_queue_partition_expired_is_noop() {
    let (kept, expired) = partition_expired(
        VecDeque::new(),
        QueueStamp::now(),
        std::time::Duration::from_secs(600),
    );
    assert!(kept.is_empty());
    assert!(expired.is_empty());
}

// ── PAN-289: age counts sleep (wall clock) and survives a backward step ──

/// The stamp `mono_elapsed` (monotonic) and `wall_elapsed` (wall clock,
/// may be negative) after `s`. Only adds to `s`, so a freshly booted CI
/// host cannot underflow an `Instant`.
fn stamp_after(
    s: QueueStamp,
    mono_elapsed: std::time::Duration,
    wall_elapsed: chrono::Duration,
) -> QueueStamp {
    QueueStamp {
        mono: s.mono + mono_elapsed,
        wall: s.wall + wall_elapsed,
    }
}

/// PAN-289, the ticket scenario: a call queued 10 s before a 10-minute
/// sleep, with a 60 s TTL. The monotonic clock saw 10 s, the wall clock
/// 610 s. The call is expired.
#[test]
fn call_queued_before_sleep_expires_on_wall_clock() {
    let ttl = std::time::Duration::from_secs(60);
    let c = call("JA1AAA", SlotParity::Even);
    let s = c.queued_at;
    let now = stamp_after(
        s,
        std::time::Duration::from_secs(10),
        chrono::Duration::seconds(610),
    );
    let (kept, expired) = partition_expired(VecDeque::from([c]), now, ttl);
    assert!(
        kept.is_empty(),
        "a call queued before sleep must not survive wake"
    );
    assert_eq!(expired, vec!["JA1AAA"]);
}

/// PAN-289: a backward wall-clock step (NTP) does not extend a call's life;
/// the monotonic age still retires it.
#[test]
fn backward_wall_step_does_not_extend_ttl() {
    let ttl = std::time::Duration::from_secs(600);
    let c = call("JA2BBB", SlotParity::Odd);
    let s = c.queued_at;
    let now = stamp_after(
        s,
        std::time::Duration::from_secs(610),
        chrono::Duration::seconds(-3600),
    );
    let (kept, expired) = partition_expired(VecDeque::from([c]), now, ttl);
    assert!(kept.is_empty());
    assert_eq!(expired, vec!["JA2BBB"]);
}

/// PAN-289: a fresh call survives a backward wall step; a negative wall
/// elapsed counts as zero, so its age is the monotonic 10 s.
#[test]
fn fresh_call_survives_backward_wall_step() {
    let ttl = std::time::Duration::from_secs(600);
    let c = call("JA3CCC", SlotParity::Even);
    let s = c.queued_at;
    let now = stamp_after(
        s,
        std::time::Duration::from_secs(10),
        chrono::Duration::seconds(-3600),
    );
    assert_eq!(s.age_at(now), std::time::Duration::from_secs(10));
    let (kept, expired) = partition_expired(VecDeque::from([c]), now, ttl);
    assert_eq!(queued_names(&kept), vec!["JA3CCC"]);
    assert!(expired.is_empty());
}

// ── Queued call offset fields + promote logic unit tests ─────────────────
//
// These tests verify that a PendingManualCall correctly carries the
// held_hz/hold_mode snapshot from queue time, and that
// compute_manual_tx_offset produces the expected offset when called with
// those values at promotion time (mirroring what promote_pending_manual_calls
// does in the non-Hound branch).

/// A queued call with held_hz=1500 / hold_mode=true opens at the held
/// offset (1500 Hz) when promoted with no active concurrent QSOs.
#[test]
fn queued_call_with_held_offset_opens_at_held_hz() {
    let p = PendingManualCall {
        callsign: "DX1ABC".to_string(),
        frequency_hz: 900.0, // DX decoded at 900 Hz
        dx_parity: Some(SlotParity::Even),
        queued_at: QueueStamp::now(),
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 1500,
        hold_mode: true,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    };
    let active: Vec<f64> = vec![];
    let (tx_off, partner) =
        compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
    assert_eq!(tx_off, 1500.0, "held offset should be honoured");
    assert_eq!(
        partner,
        Some(900.0),
        "partner_freq must be Some(dx_freq) when tx_off != dx_freq"
    );
}

/// A queued call with held_hz=1500 / hold_mode=true de-conflicts against
/// an active QSO already at 1500 Hz.
#[test]
fn queued_call_deconflicts_held_offset_against_active_at_promotion() {
    let p = PendingManualCall {
        callsign: "DX2XYZ".to_string(),
        frequency_hz: 1200.0, // DX decoded at 1200 Hz
        dx_parity: Some(SlotParity::Odd),
        queued_at: QueueStamp::now(),
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 1500,
        hold_mode: true,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    };
    // An active QSO is already on 1500 Hz at promotion time.
    let active: Vec<f64> = vec![1500.0];
    let (tx_off, partner) =
        compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
    // Should NOT be 1500 (too close to the occupied slot).
    assert_ne!(tx_off, 1500.0, "must not stack on the occupied offset");
    // A held offset deconflicts within the operator range.
    assert!(
        (200.0..=2800.0).contains(&tx_off),
        "tx_off={tx_off} is outside [200, 2800]"
    );
    // partner_freq must be Some so the DX's replies at 1200 Hz are routed.
    assert_eq!(
        partner,
        Some(1200.0),
        "partner_freq must be Some(dx_freq) when tx_off != dx_freq"
    );
}

/// A queued call with hold_mode=false (Auto) and no active QSOs promotes
/// Tx=Rx (partner_freq=None) — regression invariant.
#[test]
fn queued_call_auto_mode_promotes_tx_eq_rx() {
    let p = PendingManualCall {
        callsign: "DX3WWW".to_string(),
        frequency_hz: 1750.0,
        dx_parity: Some(SlotParity::Even),
        queued_at: QueueStamp::now(),
        hound: false,
        fox_freq_hz: None,
        fox_grid: None,
        held_hz: 0,
        hold_mode: false,
        remote_origin: false,
        remote_client_key_id: None,
        step: pancetta_core::ResponseStep::Grid,
        our_snr_of_them: None,
        their_report: None,
    };
    let active: Vec<f64> = vec![];
    let (tx_off, partner) =
        compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
    assert_eq!(tx_off, 1750.0, "Auto + no collision → Tx=Rx");
    assert_eq!(partner, None, "Tx=Rx → partner_freq is None");
}

// ── RespondToCaller admission-gate promotion (Batch 2, part 3) ───────────
//
// These exercise the REAL `promote_pending_manual_calls` (not just the
// pure partition step) against a REAL `QsoManager`, proving a
// `RespondToCaller`-shaped queued entry replays via `respond_to_caller`
// at its OWN step on promotion — not the old hardcoded
// `respond_to_cq_with` (which would always reopen at `RespondingToCq`
// regardless of what step the operator actually chose).

use super::{
    promote_pending_manual_calls, wait_out_completed_tx_grace, PendingManualCalls,
    QsoCapacityLimits,
};
use crate::message_bus::MessageBus;
use pancetta_qso::{QsoManager, QsoManagerConfig, QsoState};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

async fn test_manager(our_callsign: &str) -> QsoManager {
    let m = QsoManager::new(QsoManagerConfig {
        our_callsign: our_callsign.to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    m.start().await.expect("manager start");
    m
}

/// Non-Fox capacity limits with `max_concurrent_qsos = max_concurrent`.
fn limits(max_concurrent: usize) -> QsoCapacityLimits {
    QsoCapacityLimits {
        max_concurrent,
        fox_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        fox_max_streams: Arc::new(std::sync::atomic::AtomicUsize::new(5)),
        fox_admitted_qsos: Arc::new(std::sync::RwLock::new(Default::default())),
        admission: Arc::new(tokio::sync::Mutex::new(())),
    }
}

/// A StartQso-shaped queued call (DX on Even ⇒ wants Odd) at `offset_hz`.
fn even_dx_call_at(name: &str, offset_hz: f64) -> super::PendingManualCall {
    let mut p = call(name, SlotParity::Even);
    p.frequency_hz = offset_hz;
    p
}

fn queued_names(q: &VecDeque<super::PendingManualCall>) -> Vec<String> {
    q.iter().map(|p| p.callsign.clone()).collect()
}

/// PAN-289: promotion drops an expired call instead of starting it. On
/// wake, a QSO going terminal can trigger promotion before the 15 s TTL
/// watchdog runs. `stale` was queued 11 minutes ago by the wall clock but
/// just now by the monotonic clock (a sleep); it is the head and wants
/// Even. `fresh` wants Odd. Without the check the expired head picks Even
/// for the idle station, `stale` starts and `fresh` stays queued.
#[tokio::test]
async fn promotion_discards_expired_call_instead_of_starting_it() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    let mut stale = call("JA1AAA", SlotParity::Odd);
    stale.queued_at = QueueStamp {
        mono: std::time::Instant::now(),
        wall: chrono::Utc::now() - chrono::Duration::minutes(11),
    };
    let fresh = even_dx_call_at("JA2BBB", 1500.0);
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([stale, fresh])));

    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert!(
        !mgr.has_active_qso_with("JA1AAA").await,
        "the expired call must never start"
    );
    assert!(
        mgr.has_active_qso_with("JA2BBB").await,
        "the fresh call starts in the freed window"
    );
    assert!(
        pending.lock().await.is_empty(),
        "the expired call is discarded, not re-queued"
    );
}

/// PAN-274: once shutdown has begun, promotion starts no queued call, so no
/// deferred manual call is keyed while pancetta quits. The call stays queued.
#[tokio::test]
async fn promotion_starts_no_deferred_call_once_shutdown_has_begun() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([even_dx_call_at(
        "JA2BBB", 1500.0,
    )])));
    let mut events = mgr.subscribe();
    let shutdown = std::sync::atomic::AtomicBool::new(true);

    promote_pending_manual_calls(&mgr, &pending, &bus, &limits(1), &shutdown).await;

    assert_eq!(mgr.active_qso_count().await, 0, "no QSO was started");
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|e| matches!(e, pancetta_qso::QsoEvent::MessageToSend { .. })),
        "nothing was queued to transmit"
    );
    assert_eq!(queued_names(&*pending.lock().await), vec!["JA2BBB"]);

    // Control: with the flag clear the same call starts.
    shutdown.store(false, std::sync::atomic::Ordering::Release);
    promote_pending_manual_calls(&mgr, &pending, &bus, &limits(1), &shutdown).await;
    assert!(mgr.has_active_qso_with("JA2BBB").await);
}

/// PAN-274: the completed-TX grace ends as soon as shutdown begins and reports
/// that it did not elapse; without shutdown it elapses normally.
#[tokio::test]
async fn completed_tx_grace_wait_ends_when_shutdown_begins() {
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flip = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        flip.store(true, std::sync::atomic::Ordering::Release);
    });
    let started = std::time::Instant::now();
    assert!(!wait_out_completed_tx_grace(std::time::Duration::from_secs(45), &shutdown).await);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let clear = std::sync::atomic::AtomicBool::new(false);
    assert!(wait_out_completed_tx_grace(std::time::Duration::from_millis(50), &clear).await);
}

/// PAN-78 (Codex P2 on #449): a queued call that fails to start frees
/// its slot for the next matching queued call in the same promotion.
#[tokio::test]
async fn promotion_keeps_filling_after_a_failed_open() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    // "<...>" can never open (PAN-23), so it fails at promotion time.
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([
        even_dx_call_at("<...>", 1000.0),
        even_dx_call_at("JA2BBB", 1500.0),
    ])));

    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert_eq!(mgr.active_qso_count().await, 1, "the slot is still filled");
    assert!(mgr.has_active_qso_with("JA2BBB").await);
    assert_eq!(
        queued_names(&*pending.lock().await),
        vec!["<...>"],
        "the failed call is re-queued at the front"
    );
}

/// PAN-78 (Codex on #449): promotion takes the admission lock, so it
/// cannot take a slot a manual open holding the lock is about to fill.
#[tokio::test]
async fn promotion_waits_for_the_admission_lock() {
    let mgr = Arc::new(test_manager("K5ARH").await);
    let bus = Arc::new(MessageBus::new(1000).expect("bus"));
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([even_dx_call_at(
        "JA1AAA", 1000.0,
    )])));
    let lim = limits(1);
    let held = lim.admission.clone().lock_owned().await;

    let task = {
        let (mgr, bus, pending, lim) = (mgr.clone(), bus.clone(), pending.clone(), lim.clone());
        tokio::spawn(async move {
            promote_pending_manual_calls(
                &mgr,
                &pending,
                &bus,
                &lim,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .await;
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        mgr.active_qso_count().await,
        0,
        "blocked while the lock is held"
    );
    assert!(!task.is_finished());

    drop(held);
    task.await.unwrap();
    assert_eq!(mgr.active_qso_count().await, 1, "promotes once released");
}

/// PAN-78 (Codex on #449): in Fox mode only the Fox's own CQ is a free
/// stream; a CQ started by hand counts against `fox_max_streams`.
#[tokio::test]
async fn fox_capacity_counts_manual_cqs_but_not_the_fox_cq() {
    let mgr = test_manager("K5ARH").await;
    let fox_mode = std::sync::atomic::AtomicBool::new(true);
    let fox_max = std::sync::atomic::AtomicUsize::new(5);
    let fox_set = std::sync::RwLock::new(std::collections::HashSet::new());

    let fox_cq = mgr
        .start_cq_manual(1000.0, None, false, None)
        .await
        .unwrap();
    super::record_fox_admitted(&fox_set, &fox_cq);
    let snap = super::capacity_snapshot(&mgr, 1, &fox_mode, &fox_max, &fox_set).await;
    assert_eq!(snap.active, 0, "the Fox CQ is not counted");

    mgr.start_cq_manual(1500.0, None, false, None)
        .await
        .unwrap();
    let snap = super::capacity_snapshot(&mgr, 1, &fox_mode, &fox_max, &fox_set).await;
    assert_eq!(snap.active, 1, "a manual CQ in Fox mode is counted");
}

/// PAN-78: the scratch reproduction (`pan78_scratch_promotion_ignores_cap`)
/// inverted — three same-side queued calls promoted from idle with the
/// default cap of 1 start exactly one QSO (the oldest); the other two
/// stay queued in FIFO order.
#[tokio::test]
async fn promotion_respects_cap_from_idle() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([
        even_dx_call_at("JA1AAA", 1000.0),
        even_dx_call_at("JA2BBB", 1500.0),
        even_dx_call_at("JA3CCC", 2000.0),
    ])));

    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert_eq!(mgr.active_qso_count().await, 1, "cap 1 ⇒ one QSO starts");
    let active = mgr.get_active_qsos().await;
    assert_eq!(
        active[0].1.metadata.their_callsign.as_deref(),
        Some("JA1AAA"),
        "the oldest queued call starts first"
    );
    assert_eq!(
        queued_names(&*pending.lock().await),
        vec!["JA2BBB", "JA3CCC"],
        "the rest stay queued in FIFO order"
    );
}

#[tokio::test]
async fn promotion_fills_only_free_slots() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    // One same-side QSO already active (DX Even ⇒ we TX Odd).
    mgr.respond_to_cq_manual("W9ACT".to_string(), 700.0, Some(SlotParity::Even))
        .await
        .expect("open active QSO");
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([
        even_dx_call_at("JA1AAA", 1000.0),
        even_dx_call_at("JA2BBB", 1500.0),
        even_dx_call_at("JA3CCC", 2000.0),
    ])));

    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert_eq!(
        mgr.active_qso_count().await,
        2,
        "cap 2, one active ⇒ one starts"
    );
    assert_eq!(
        queued_names(&*pending.lock().await),
        vec!["JA2BBB", "JA3CCC"]
    );
}

#[tokio::test]
async fn promotion_with_no_free_slot_keeps_queue_intact() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    mgr.respond_to_cq_manual("W9ACT".to_string(), 700.0, Some(SlotParity::Even))
        .await
        .expect("open active QSO");
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([
        even_dx_call_at("JA1AAA", 1000.0),
        even_dx_call_at("JA2BBB", 1500.0),
    ])));

    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert_eq!(
        mgr.active_qso_count().await,
        1,
        "no free slot ⇒ nothing starts"
    );
    assert_eq!(
        queued_names(&*pending.lock().await),
        vec!["JA1AAA", "JA2BBB"],
        "queue unchanged"
    );
}

/// A `RespondToCaller` (step = Report) queued because it wanted the
/// opposite parity from our (idle) committed side promotes — via
/// `promote_pending_manual_calls` — into a REAL QSO opened at
/// `SendingReport` (the Report step's initial state), not `RespondingToCq`
/// (the Grid/StartQso shape). This is the admission-gate + promotion
/// round-trip for Part 3: queued-not-admitted-immediately, then promoted
/// once a window is free.
#[tokio::test]
async fn respond_to_caller_shaped_queued_call_promotes_at_its_own_step() {
    let mgr = test_manager("K5ARH").await;
    let bus = MessageBus::new(1000).expect("bus");
    let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([caller_call(
        "JA1ABC",
        SlotParity::Even,
        pancetta_core::ResponseStep::Report,
    )])));

    // Idle (no active QSOs) — `promote_pending_manual_calls` will adopt
    // the queue's own desired parity (Odd, opposite of Even) as the side
    // to commit to, and start it.
    promote_pending_manual_calls(
        &mgr,
        &pending,
        &bus,
        &limits(1),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .await;

    assert!(
        pending.lock().await.is_empty(),
        "the queued caller-response must be promoted (queue drained), not left waiting"
    );

    let active = mgr.get_active_qsos().await;
    assert_eq!(active.len(), 1, "exactly one QSO should have started");
    let (_, progress) = &active[0];
    assert_eq!(progress.metadata.their_callsign.as_deref(), Some("JA1ABC"));
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "step=Report must open at SendingReport (not RespondingToCq/Grid), got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.tx_parity,
        Some(SlotParity::Odd),
        "tx_parity must be opposite of the caller's Even slot"
    );
}
