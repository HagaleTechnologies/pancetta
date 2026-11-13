//! Real-worker gate tests and QSO-manager event ordering.
mod common;
use common::CoordSim;
use pancetta_lib::coordinator::active_tx_qso_key;
use std::collections::HashSet;

#[tokio::test]
async fn gate_active_qso_keys_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;
    let mut request = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    request.qso_id = Some("qso-active-1".to_string());
    if let Some(id) = &request.qso_id {
        sim.active_tx_qsos
            .write()
            .unwrap()
            .insert(active_tx_qso_key(id));
    }
    sim.drive_slot(vec![request]).await;
    assert!(sim.observations.lock().unwrap().ptt.contains(&true));
    assert!(sim.observations.lock().unwrap().audio > 0);
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

#[tokio::test]
async fn gate_active_qso_ptt_on_edge_observed() {
    let mut sim = CoordSim::new("K5ARH").await;
    let mut request = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    request.qso_id = Some("qso-edge".to_string());
    if let Some(id) = &request.qso_id {
        sim.active_tx_qsos
            .write()
            .unwrap()
            .insert(active_tx_qso_key(id));
    }
    sim.drive_slot(vec![request]).await;
    assert!(sim.observations.lock().unwrap().ptt.contains(&true));
    assert!(sim.observations.lock().unwrap().audio > 0);
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}

#[tokio::test]
async fn gate_inactive_qso_drops_and_ptt_stays_off() {
    let mut sim = CoordSim::new("K5ARH").await;
    let mut request = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    request.qso_id = Some("qso-ended".to_string());
    sim.drive_slot(vec![request]).await;
    sim.timeline.assert_silent();
    assert_eq!(sim.timeline.dropped.len(), 1);
    assert_eq!(sim.observations.lock().unwrap().audio, 0);
    assert!(!sim.observations.lock().unwrap().ptt.contains(&true));
    sim.shutdown().await;
}

#[tokio::test]
async fn gate_manual_none_qso_keys_ptt() {
    let mut sim = CoordSim::new("K5ARH").await;
    let mut request = sim.manual_tx("CQ K5ARH EM10", 1500.0);
    request.qso_id = None;
    if let Some(id) = &request.qso_id {
        sim.active_tx_qsos
            .write()
            .unwrap()
            .insert(active_tx_qso_key(id));
    }
    sim.drive_slot(vec![request]).await;
    assert!(sim.observations.lock().unwrap().ptt.contains(&true));
    assert!(sim.observations.lock().unwrap().audio > 0);
    sim.timeline.assert_all_released();
    sim.shutdown().await;
}
// ---------------------------------------------------------------------------
// Manager-level root-cause / regression tests
// ---------------------------------------------------------------------------

use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig, QsoState};

/// Drain a QsoManager event receiver into a Vec (non-blocking; the events
/// are already buffered in the broadcast channel by the time we drain).
fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// ROOT CAUSE + REGRESSION GUARD.
///
/// The coordinator's `active_tx_qsos` populater inserts a qso_id ONLY when it
/// sees a `QsoEvent::StateChanged { new_state, .. }` whose `new_state` is
/// active. The scheduled-QSO start path (`respond_to_cq_with`) must therefore
/// emit such a `StateChanged` AT START — and emit it BEFORE the first
/// `MessageToSend` (both flow through the same serial event consumer, so the
/// insert must be ordered first). Otherwise the first scheduled
/// `TransmitRequest` reaches the Step 4b gate with the qso_id absent and PTT
/// is silently dropped — exactly the operator's bug.
#[tokio::test]
async fn qso_start_emits_statechanged_before_first_message() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_with(
            "W1AW".to_string(),
            1500.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Auto,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let events = drain_events(&mut rx);

    // Find the index of the first StateChanged into an active state for this
    // QSO, and the first MessageToSend for this QSO.
    let active_state_idx = events.iter().position(|ev| {
        matches!(
            ev,
            QsoEvent::StateChanged { qso_id: id, new_state, .. }
                if *id == qso_id && new_state.is_active()
        )
    });
    let first_msg_idx = events
        .iter()
        .position(|ev| matches!(ev, QsoEvent::MessageToSend { qso_id: id, .. } if *id == qso_id));

    let active_state_idx = active_state_idx.expect(
        "QSO start MUST emit a StateChanged into an active state so the \
         coordinator inserts the qso_id into active_tx_qsos before the first \
         TransmitRequest reaches the Step 4b PTT gate (bug: scheduled TX \
         never keys PTT)",
    );
    let first_msg_idx =
        first_msg_idx.expect("QSO start should emit a MessageToSend (the first call)");

    assert!(
        active_state_idx < first_msg_idx,
        "StateChanged(active) must be emitted BEFORE the first MessageToSend \
         so the active-set insert is ordered ahead of the TransmitRequest \
         (got StateChanged at {}, MessageToSend at {})",
        active_state_idx,
        first_msg_idx
    );
}

/// End-to-end at the manager level: simulate the coordinator's
/// `active_tx_qsos` populater consuming the QSO event stream in order, then
/// assert that by the time the first `MessageToSend` is processed the qso_id
/// is already live in the set — i.e. the Step 4b gate would PASS it.
#[tokio::test]
async fn qso_start_populates_active_set_before_transmit_request() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();

    // Manual caller-response (the "Callers"/Space path); the Grid step routes
    // through respond_to_cq_with, the same scheduled-QSO start path.
    let qso_id = manager
        .respond_to_caller(
            "VK3ABC".to_string(),
            1200.0,
            Some(pancetta_core::slot::SlotParity::Odd),
            pancetta_core::ResponseStep::Grid,
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .expect("respond_to_caller");

    let events = drain_events(&mut rx);

    // Replay the coordinator's populater: insert on StateChanged-into-active,
    // remove on terminal Failed — exactly as coordinator/qso.rs does.
    let mut active: HashSet<String> = HashSet::new();
    let mut gate_result_at_first_tx: Option<bool> = None;
    for ev in &events {
        match ev {
            QsoEvent::StateChanged {
                qso_id: id,
                new_state,
                ..
            } => {
                let key = active_tx_qso_key(&id.to_string());
                if new_state.is_active() {
                    active.insert(key);
                } else if matches!(new_state, QsoState::Failed { .. }) {
                    active.remove(&key);
                }
            }
            QsoEvent::MessageToSend { qso_id: id, .. }
                if *id == qso_id && gate_result_at_first_tx.is_none() =>
            {
                // This is the moment the coordinator forwards the first
                // TransmitRequest. Evaluate the Step 4b gate now.
                gate_result_at_first_tx =
                    Some(active.contains(&active_tx_qso_key(&id.to_string())));
            }
            _ => {}
        }
    }

    assert_eq!(
        gate_result_at_first_tx,
        Some(true),
        "by the first MessageToSend, the qso_id must already be in \
         active_tx_qsos so the Step 4b gate PASSES it (otherwise scheduled \
         TX never keys PTT)"
    );
}
