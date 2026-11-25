//! PAN-489: the TX DX-parity gate reads the shared `SlotClock`, and for one
//! freshness window after a resync it holds a frame to a resolved partner
//! that was scheduled after the step, stating the step. A frame scheduled
//! before the step is left to the PAN-483 fence.

use super::slot_clock_resync_tests::{await_drop_outcome, count_prefixed, ticking_harness_at};
use super::worker_gate_tests::{await_completion, Harness};
use super::*;
use crate::message_bus::TxOrigin;
use pancetta_core::slot::{SlotParity, SLOT_NS};
use pancetta_core::slot_clock::{ManualClock, WallClockJump};
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

const DIAL_20M: u64 = 14_074_000;
const TEXT: &str = "5Z4VJ W5AU EM10";

fn even_sighting(
    callsign: &str,
    decoded_at: std::time::SystemTime,
) -> pancetta_qso::A7ExpectedCall {
    pancetta_qso::A7ExpectedCall {
        callsign: callsign.to_string(),
        freq_hz: 1500.0,
        slot_parity: 0, // Even
        decoded_at,
        captured_dial_hz: DIAL_20M,
        slot_ns: SLOT_NS,
    }
}

/// A `ManualClock` slot clock that has taken its first read, and a table
/// holding a 20 m / FT8-grid Even sighting of 5Z4VJ stamped at that time.
fn sighted() -> (
    ManualClock,
    pancetta_core::SlotClock,
    pancetta_qso::CrossTimeState,
) {
    let manual = ManualClock::new(UtcTimestamp::now());
    let clock = manual.slot_clock(Arc::new(AtomicI64::new(SLOT_NS)));
    let _ = clock.read();
    let cts = pancetta_qso::CrossTimeState::empty();
    cts.a7_recent_calls.write().unwrap().record(even_sighting(
        "5Z4VJ",
        std::time::SystemTime::from(manual.now()),
    ));
    (manual, clock, cts)
}

/// The gate's context for a frame scheduled under the reading's own epoch.
fn context_at(reading: pancetta_core::slot_clock::SlotReading) -> DxParityContext {
    DxParityContext {
        dial_hz: DIAL_20M,
        slot_ns: SLOT_NS,
        clock: reading,
        scheduled_resync_epoch: reading.resync_epoch,
    }
}

/// [`sighted`], then 2 s later the wall clock steps back 10 s.
fn after_backward_step() -> (
    ManualClock,
    pancetta_core::SlotClock,
    pancetta_qso::CrossTimeState,
) {
    let (manual, clock, cts) = sighted();
    manual.advance(Duration::from_secs(2));
    manual.step_wall(-SignedDuration::from_secs(10));
    (manual, clock, cts)
}

fn even_conflict(
    context: DxParityContext,
    cts: &pancetta_qso::CrossTimeState,
) -> Option<DxParityHold> {
    dx_parity_conflict("5Z4VJ", SlotParity::Even, context, cts)
}

#[test]
fn backward_resync_holds_with_the_step() {
    let (_manual, clock, cts) = after_backward_step();
    let reading = clock.read();
    assert_eq!(reading.resync_epoch, 1);
    assert_eq!(
        even_conflict(context_at(reading), &cts),
        Some(DxParityHold::ClockStepped(WallClockJump::Backward(
            SignedDuration::from_secs(10)
        )))
    );
}

#[test]
fn forward_resync_holds_after_eviction() {
    let (manual, clock, cts) = sighted();
    manual.advance(Duration::from_secs(2));
    manual.step_wall(SignedDuration::from_secs(60));
    let reading = clock.read();
    // A post-step decode of another station evicts the pre-step sighting.
    cts.a7_recent_calls.write().unwrap().record(even_sighting(
        "JA1XYZ",
        std::time::SystemTime::from(manual.now()),
    ));
    {
        let a7 = cts.a7_recent_calls.read().unwrap();
        assert!(
            !a7.entries_with_parity(0)
                .chain(a7.entries_with_parity(1))
                .any(|entry| entry.callsign == "5Z4VJ"),
            "the forward step must let the table evict the pre-step sighting"
        );
    }
    assert_eq!(
        even_conflict(context_at(reading), &cts),
        Some(DxParityHold::ClockStepped(WallClockJump::Forward(
            SignedDuration::from_secs(60)
        )))
    );
}

#[test]
fn sub_resync_backward_step_keeps_the_sighting() {
    let (manual, clock, cts) = sighted();
    manual.advance(Duration::from_millis(200));
    manual.step_wall(-SignedDuration::from_millis(500));
    let reading = clock.read();
    assert_eq!(reading.resync_epoch, 0, "0.5 s is not a resync");
    assert_eq!(
        even_conflict(context_at(reading), &cts),
        Some(DxParityHold::CollidingParity)
    );
}

#[test]
fn step_hold_ends_after_one_freshness_window() {
    let (manual, clock, cts) = after_backward_step();
    let _ = clock.read();
    manual.advance(Duration::from_secs(31));
    // 31 s after the step: the hold has ended, and the sighting's wall age
    // is 23 s (2 s before the step, -10 s step, 31 s after).
    assert_eq!(
        even_conflict(context_at(clock.read()), &cts),
        Some(DxParityHold::CollidingParity)
    );
    manual.advance(Duration::from_secs(10));
    assert_eq!(even_conflict(context_at(clock.read()), &cts), None);
}

#[test]
fn frame_scheduled_before_the_resync_is_left_to_the_fence() {
    let (_manual, clock, cts) = after_backward_step();
    let context = DxParityContext {
        scheduled_resync_epoch: 0,
        ..context_at(clock.read())
    };
    assert_eq!(even_conflict(context, &cts), None);
}

#[test]
fn final_gate_names_the_clock_step() {
    use std::collections::HashSet;
    use std::sync::RwLock;
    let (_manual, clock, cts) = after_backward_step();
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    active
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-kenya"));
    let denial = final_single_tx_gate_denial(
        Some("qso-kenya"),
        SlotParity::Even,
        context_at(clock.read()),
        &active,
        Some("5Z4VJ"),
        &cts,
    );
    let jump = WallClockJump::Backward(SignedDuration::from_secs(10));
    assert_eq!(denial, Some(FinalGateDenial::DxParityClockStep(jump)));
    assert_eq!(
        final_gate_denial_text(
            FinalGateDenial::DxParityClockStep(jump),
            Some("qso-kenya"),
            SlotParity::Even,
            TEXT,
        )
        .1,
        "holding TX at final pre-PTT check: system clock stepped -10.0 s within the DX-parity window, so the DX's earlier sightings cannot be aged: '5Z4VJ W5AU EM10'"
    );
}

#[test]
fn bundle_holds_only_the_partner_item_after_a_step() {
    use std::collections::HashSet;
    use std::sync::RwLock;
    let (_manual, clock, cts) = after_backward_step();
    let active: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
    for id in ["qso-kenya", "qso-cq"] {
        active
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(id));
    }
    let ids = vec![Some("qso-kenya".to_string()), Some("qso-cq".to_string())];
    let callsigns = vec![Some("5Z4VJ".to_string()), None];
    let (live, holds) = bundle_live_mask(
        &ids,
        &callsigns,
        SlotParity::Even,
        context_at(clock.read()),
        &active,
        &cts,
    );
    assert_eq!(
        holds,
        vec![
            Some(DxParityHold::ClockStepped(WallClockJump::Backward(
                SignedDuration::from_secs(10)
            ))),
            None
        ]
    );
    assert_eq!(live, vec![false, true]);
}

// --- Worker level ---

/// A worker over a `ManualClock` anchored 1.2 s before the next Even FT8
/// boundary after real time, ticking at real speed.
async fn even_ticking_harness() -> (Harness, ManualClock, tokio::task::JoinHandle<()>) {
    let boundary =
        pancetta_core::slot::next_slot_with_parity(UtcTimestamp::now(), SlotParity::Even);
    ticking_harness_at(boundary - SignedDuration::from_millis(1_200)).await
}

/// Opens a manual call to 5Z4VJ answering its Odd CQ, so the QSO transmits
/// Even, and publishes the manager to the worker. Returns the QSO id.
async fn open_even_call(h: &Harness) -> String {
    let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W5AU".into(), // QsoManager refuses the N0CALL placeholder
        ..Default::default()
    });
    let id = manager
        .respond_to_cq_manual("5Z4VJ".into(), 1500.0, Some(SlotParity::Odd))
        .await
        .unwrap()
        .to_string();
    h.coordinator.qso_manager_watch.send_replace(Some(manager));
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&id));
    id
}

fn record_even_sighting(h: &Harness, manual: &ManualClock) {
    h.coordinator
        .cross_time_state
        .a7_recent_calls
        .write()
        .unwrap()
        .record(even_sighting(
            "5Z4VJ",
            std::time::SystemTime::from(manual.now()),
        ));
}

async fn request_even(h: &Harness, qso_id: &str) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: TEXT.into(),
                frequency_offset: 1500.0,
                qso_id: Some(qso_id.to_string()),
                tx_parity: Some(SlotParity::Even),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

/// AC1: the step lands before the frame is scheduled, so the PAN-483 fence
/// stays silent; the DX-parity gate holds the frame and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn even_sighting_holds_an_even_frame_scheduled_after_a_backward_step() {
    use std::sync::atomic::Ordering;
    let (h, manual, ticker) = even_ticking_harness().await;
    let qso_id = open_even_call(&h).await;
    record_even_sighting(&h, &manual);
    manual.step_wall(-SignedDuration::from_secs(10));
    // Read the worker's own clock (the harness's `utc_clock` reads it) so
    // the step registers before the frame is dequeued and scheduled.
    let _ = (h.utc_clock)();
    request_even(&h, &qso_id).await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, TEXT, Duration::from_secs(40)).await;
    ticker.abort();
    assert_eq!(completed, Some(false), "{diagnostics:?}");
    assert!(!ptt_on, "never keyed: {diagnostics:?}");
    assert!(
        diagnostics
            .iter()
            .any(|d| d
                .contains("holding TX: system clock stepped -10.0 s within the DX-parity window")),
        "{diagnostics:?}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// AC2: the frame is scheduled before the step, which lands between Step 4
/// and the Step 4d final gate. The PAN-483 fence keeps owning this case:
/// the frame is dropped with the step's real size and never keyed. The
/// sighting is recorded while the worker waits at its PTT gate, so Step
/// 4b-parity (before that gate) has not already held the frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn even_frame_scheduled_before_a_backward_step_is_dropped_with_the_step() {
    use std::sync::atomic::Ordering;
    let (h, manual, ticker) = even_ticking_harness().await;
    let qso_id = open_even_call(&h).await;
    let held = h.coordinator.ptt_sync_gate.lock().await;
    request_even(&h, &qso_id).await;
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("worker reaches its PTT gate after building the waveform");
    record_even_sighting(&h, &manual);
    manual.step_wall(-SignedDuration::from_secs(10));
    drop(held);
    let out = await_drop_outcome(&h, &[TEXT]).await;
    ticker.abort();
    assert_eq!(out.failed, vec![TEXT.to_string()], "{out:?}");
    assert!(!out.ptt.contains(&true), "never keyed: {out:?}");
    assert_eq!(
        count_prefixed(
            &out.diagnostics,
            "dropping stale TX after a -10.0 s clock jump"
        ),
        1,
        "{out:?}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}
