//! "Did we actually fix it?" — coordinator-level replay of the on-air QSO
//! failures the operator (K5ARH) hit over the past few days, asserting that
//! with all the fixes now on `main` each contact **properly progresses** at the
//! PTT / stale-TX / supersede layer.
//!
//! This is the coordinator half (PTT keying, the drop-stale-TX gate, and the
//! re-call/supersede behavior). The parse/sequence half lives in the engine
//! companion suite `pancetta-qso/tests/real_incidents.rs`.
//!
//! The vehicle is the same set of faithful coordinator replicas used by
//! `tx_ptt_integration.rs` (the harness this incident suite reuses rather than
//! reimplementing): a directly-constructed [`MockRig`] behind a real
//! [`MessageBus`] consumer that mirrors `coordinator/hamlib.rs`'s `SetPtt`
//! handling, the byte-identical `active_tx_qso_key` / `tx_qso_is_live` gate
//! predicates from `coordinator/tx.rs` (Step 4b), and the `active_tx_qsos`
//! populater replay from `coordinator/qso.rs` (insert on StateChanged-into-
//! active, remove on terminal Failed / after-grace on Completed). The QSO side
//! is driven by the **real** `QsoManager` so the StateChanged/MessageToSend
//! ordering and the re-call continuation are production behavior, not a mock.
//!
//! Incidents replayed here (2026-06-13 .. 2026-06-15 on-air session):
//!   * W5XO  — pressed Space twice → superseded + restarted from grid, and the
//!     superseded QSO's queued TX kept transmitting.
//!   * 9A4AA — re-call "supersede storm" + PTT never keyed on QSOs.
//!
//! Run: `cargo test -p pancetta --test real_incidents_coord`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pancetta_hamlib::{MockRig, PttState, RigControl, Vfo};
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig, QsoState};

const US: &str = "K5ARH";
const FREQ: f64 = 1500.0;

// ---------------------------------------------------------------------------
// Faithful coordinator replicas (kept byte-identical to production semantics;
// the production helpers are `pub(crate)` so an integration test must mirror
// them — the same approach `tx_ptt_integration.rs` takes).
// ---------------------------------------------------------------------------

/// Mirror of `coordinator::active_tx_qso_key`.
fn active_tx_qso_key(qso_id: &str) -> String {
    qso_id.trim().to_uppercase()
}

/// Mirror of `coordinator::tx_qso_is_live`. `None` (manual / tune / test-TX) is
/// never gated; a `Some(id)` keys only if its canonical key is present in the
/// active set. This is the Step 4b drop-stale-TX gate.
fn tx_qso_is_live(qso_id: Option<&str>, active: &HashSet<String>) -> bool {
    match qso_id {
        None => true,
        Some(id) => active.contains(&active_tx_qso_key(id)),
    }
}

/// Spawn a consumer on the `Hamlib` channel mirroring `coordinator/hamlib.rs`'s
/// `SetPtt` handling, backed by a shared [`MockRig`].
async fn spawn_hamlib_consumer(bus: &MessageBus, shutdown: Arc<AtomicBool>) -> Arc<MockRig> {
    let (_tx, rx) = bus
        .create_channel(ComponentId::Hamlib)
        .await
        .expect("create Hamlib channel");

    let rig = Arc::new(MockRig::default());
    rig.connect().await.expect("mock rig connect");

    let rig_for_task = Arc::clone(&rig);
    tokio::spawn(async move {
        while !shutdown.load(Ordering::Acquire) {
            match rx.try_recv() {
                Ok(message) => {
                    if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                        message.message_type
                    {
                        let ptt = if state { PttState::On } else { PttState::Off };
                        let _ = rig_for_task.set_ptt(Vfo::Current, ptt).await;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    });

    rig
}

/// Faithful replica of the `TransmitRequest` worker arm's keying sequence
/// (`coordinator/tx.rs`, Step 4b → Step 5 PTT-on → Step 9 PTT-off), timing
/// compressed. Returns `true` if it keyed, `false` if Step 4b dropped it.
async fn process_transmit_request(
    bus: &MessageBus,
    active: &HashSet<String>,
    qso_id: Option<String>,
) -> bool {
    if !tx_qso_is_live(qso_id.as_deref(), active) {
        return false; // Step 4b: drop stale TX — no PTT.
    }
    let ptt_on = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true }),
        Instant::now(),
    );
    bus.send_message(ptt_on).await.expect("send PTT ON");
    tokio::time::sleep(Duration::from_millis(40)).await;
    let ptt_off = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    );
    bus.send_message(ptt_off).await.expect("send PTT OFF");
    true
}

/// Poll the mock rig's PTT for up to `timeout`; return `true` once observed On.
async fn wait_for_ptt_on(rig: &MockRig, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(PttState::On) = rig.get_ptt(Vfo::Current).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// Drain a QsoManager event receiver into a Vec (non-blocking).
fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// Replay the coordinator's `active_tx_qsos` populater over a batch of events:
/// insert on StateChanged-into-active, remove on terminal Failed. (The Completed
/// path removes only after a grace slot in production — irrelevant to these
/// tests, which key during the live window.)
fn apply_populater(events: &[QsoEvent], active: &mut HashSet<String>) {
    for ev in events {
        if let QsoEvent::StateChanged {
            qso_id, new_state, ..
        } = ev
        {
            let key = active_tx_qso_key(&qso_id.to_string());
            if new_state.is_active() {
                active.insert(key);
            } else if matches!(new_state, QsoState::Failed { .. }) {
                active.remove(&key);
            }
        }
    }
}

// =====================================================================
// Incident 6 — W5XO
// Symptom (2026-06-13): operator pressed Space twice → the QSO was superseded
// and restarted from grid, AND the superseded QSO's queued TX kept
// transmitting (keying PTT every cycle). Two fixes converge:
//   (a) a manual re-call of an already-active station CONTINUES the same QSO
//       (same id, no restart-to-grid, no Superseded failure); and
//   (b) the drop-stale-TX gate (Step 4b) drops any TransmitRequest whose
//       qso_id is no longer live — so even a stale/superseded request can't
//       keep keying PTT.
// We assert the live QSO keys PTT and a stale (not-in-active-set) request does
// not — and that the re-call kept exactly one QSO id.
// =====================================================================
#[tokio::test]
async fn w5xo_double_space_continues_one_qso_and_stale_tx_does_not_key_ptt() {
    // --- (a) Re-call mid-exchange CONTINUES the same QSO (real QsoManager). ---
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: US.to_string(),
        ..Default::default()
    });
    let mut rx = manager.subscribe();

    // First Space: start the W5XO QSO.
    let id1 = manager
        .respond_to_cq_with(
            "W5XO".to_string(),
            FREQ,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            None,
        )
        .await
        .expect("first call");

    // Advance into the exchange: their report → we move to SendingReport.
    manager
        .process_message(
            pancetta_qso::MessageType::SignalReport {
                to_station: US.to_string(),
                from_station: "W5XO".to_string(),
                report: -12,
            },
            "K5ARH W5XO -12".to_string(),
            FREQ,
            Some(-12.0),
        )
        .await
        .expect("process report");

    // Second Space on the SAME station mid-exchange — must continue, not restart.
    let id2 = manager
        .respond_to_cq_with(
            "W5XO".to_string(),
            FREQ,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            None,
        )
        .await
        .expect("second call (re-Space)");

    assert_eq!(
        id1, id2,
        "double-Space on W5XO must CONTINUE the same QSO, not restart a new one"
    );

    let events = drain_events(&mut rx);
    // No Superseded failure for W5XO (the storm fingerprint).
    assert!(
        !events.iter().any(|ev| matches!(
            ev,
            QsoEvent::StateChanged { new_state: QsoState::Failed { reason, .. }, .. }
                if matches!(reason, pancetta_qso::QsoFailureReason::Superseded)
        ) || matches!(
            ev,
            QsoEvent::QsoFailed { reason, .. }
                if matches!(reason, pancetta_qso::QsoFailureReason::Superseded)
        )),
        "double-Space on W5XO must NOT produce a Superseded failure: {events:?}"
    );
    // After the re-call we must still see no restart-to-grid for a *second* QSO:
    // every event references the single id1.
    assert!(
        events.iter().all(|ev| match ev {
            QsoEvent::StateChanged { qso_id, .. }
            | QsoEvent::MessageToSend { qso_id, .. }
            | QsoEvent::QsoCompleted { qso_id, .. }
            | QsoEvent::QsoFailed { qso_id, .. } => *qso_id == id1,
            _ => true,
        }),
        "all events must belong to the single W5XO QSO {id1}: {events:?}"
    );

    // --- (b) The drop-stale-TX gate: stale request drops, live request keys. ---
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

    // Build the active set the way the coordinator would, from the real events.
    let mut active: HashSet<String> = HashSet::new();
    apply_populater(&events, &mut active);
    assert!(
        active.contains(&active_tx_qso_key(&id1.to_string())),
        "the live W5XO QSO must be in the active_tx set"
    );

    // A stale/superseded request for an OTHER (ended) qso_id must be dropped:
    // no PTT. This is the "superseded QSO's queued TX kept transmitting" bug.
    let stale_keyed =
        process_transmit_request(&bus, &active, Some("w5xo-superseded-stale".to_string())).await;
    assert!(
        !stale_keyed,
        "a stale/superseded W5XO TransmitRequest must be DROPPED at Step 4b — \
         it must not keep keying PTT"
    );
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        rig.get_ptt(Vfo::Current).await.expect("get_ptt"),
        PttState::Off,
        "PTT must stay OFF for the stale superseded request"
    );

    // The LIVE QSO's request keys PTT.
    let rig_poll = Arc::clone(&rig);
    let poller =
        tokio::spawn(async move { wait_for_ptt_on(&rig_poll, Duration::from_secs(5)).await });
    let live_keyed = process_transmit_request(&bus, &active, Some(id1.to_string())).await;
    assert!(live_keyed, "the live W5XO QSO must key PTT");
    assert!(
        poller.await.expect("poller join"),
        "mock rig PTT must be observed ON for the live W5XO QSO"
    );

    shutdown.store(true, Ordering::Release);
}

// =====================================================================
// Incident 7 — 9A4AA
// Symptom (2026-06-15): re-call "supersede storm" + PTT never keyed on QSOs.
// Root cause for the PTT half: QSO start did not emit a StateChanged into an
// active state before the first MessageToSend, so the coordinator never
// inserted the qso_id into active_tx_qsos and the first (and every) scheduled
// TransmitRequest was dropped at Step 4b — PTT never keyed. Fix: QSO start now
// emits StateChanged(active) BEFORE the first MessageToSend, so the qso_id is
// live by the time its TransmitRequest reaches the gate. Re-call half: an
// answered re-call continues the same QSO (no storm/duplicate).
// =====================================================================
#[tokio::test]
async fn nine_a4aa_qso_start_keys_ptt_and_recall_continues_no_storm() {
    let manager = QsoManager::new(QsoManagerConfig {
        our_callsign: US.to_string(),
        ..Default::default()
    });
    let mut rx = manager.subscribe();

    // Operator calls 9A4AA.
    let id1 = manager
        .respond_to_cq_with(
            "9A4AA".to_string(),
            FREQ,
            Some(pancetta_core::slot::SlotParity::Odd),
            CallInitiation::Manual,
            None,
        )
        .await
        .expect("call 9A4AA");

    let events = drain_events(&mut rx);

    // The PTT fix: StateChanged(active) must precede the first MessageToSend so
    // the active-set insert is ordered ahead of the TransmitRequest.
    let active_idx = events
        .iter()
        .position(|ev| {
            matches!(ev, QsoEvent::StateChanged { qso_id, new_state, .. }
                if *qso_id == id1 && new_state.is_active())
        })
        .expect("QSO start MUST emit a StateChanged into an active state (PTT-keying fix)");
    let first_msg_idx = events
        .iter()
        .position(|ev| matches!(ev, QsoEvent::MessageToSend { qso_id, .. } if *qso_id == id1))
        .expect("QSO start should emit the opening MessageToSend");
    assert!(
        active_idx < first_msg_idx,
        "StateChanged(active) must precede the first MessageToSend so the \
         9A4AA qso_id is in active_tx_qsos before its TransmitRequest hits the \
         Step 4b gate (got active at {active_idx}, msg at {first_msg_idx})"
    );

    // Replay the populater + assert the gate PASSES the first scheduled TX.
    let mut active: HashSet<String> = HashSet::new();
    apply_populater(&events, &mut active);
    assert!(
        tx_qso_is_live(Some(&id1.to_string()), &active),
        "by its first TransmitRequest, the 9A4AA qso_id must be live so Step 4b \
         PASSES it (otherwise scheduled TX never keys PTT)"
    );

    // End-to-end PTT: the live 9A4AA QSO keys the mock rig.
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;
    let rig_poll = Arc::clone(&rig);
    let poller =
        tokio::spawn(async move { wait_for_ptt_on(&rig_poll, Duration::from_secs(5)).await });
    let keyed = process_transmit_request(&bus, &active, Some(id1.to_string())).await;
    assert!(keyed, "the 9A4AA QSO must pass the gate");
    assert!(
        poller.await.expect("poller join"),
        "mock rig PTT was never observed ON for the 9A4AA QSO (PTT-never-keyed bug)"
    );
    shutdown.store(true, Ordering::Release);

    // Re-call half: 9A4AA answers; operator re-calls → continues the SAME QSO,
    // no supersede storm, no duplicate id.
    manager
        .process_message(
            pancetta_qso::MessageType::SignalReport {
                to_station: US.to_string(),
                from_station: "9A4AA".to_string(),
                report: -10,
            },
            "K5ARH 9A4AA -10".to_string(),
            FREQ,
            Some(-10.0),
        )
        .await
        .expect("9A4AA answers");

    let id2 = manager
        .respond_to_cq_with(
            "9A4AA".to_string(),
            FREQ,
            Some(pancetta_core::slot::SlotParity::Odd),
            CallInitiation::Manual,
            None,
        )
        .await
        .expect("re-call 9A4AA");
    assert_eq!(
        id1, id2,
        "re-calling 9A4AA must CONTINUE the same QSO (no supersede storm)"
    );

    let more = drain_events(&mut rx);
    assert!(
        !more.iter().any(|ev| matches!(
            ev,
            QsoEvent::StateChanged { new_state: QsoState::Failed { reason, .. }, .. }
                if matches!(reason, pancetta_qso::QsoFailureReason::Superseded)
        ) || matches!(
            ev,
            QsoEvent::QsoFailed { reason, .. }
                if matches!(reason, pancetta_qso::QsoFailureReason::Superseded)
        )),
        "re-calling 9A4AA must NOT produce a Superseded failure (storm): {more:?}"
    );
}
