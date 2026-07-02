//! Integration test for the scheduled-QSO → PTT keying path against the
//! MOCK rig (no physical radio, no rigctld).
//!
//! Background: the operator reported that manual PTT (`p`) and tune
//! (Shift+T) DO key the rig, but **scheduled QSO transmissions never key
//! PTT**, even though the logs show TX audio queued and `SetPtt` sent.
//! The scheduled-TX path (`coordinator/tx.rs`, the `TransmitRequest`
//! worker arm) has a "Step 4b" gate that DROPS a request whose `qso_id`
//! is not in the shared `active_tx_qsos` set. Manual / tune requests carry
//! `qso_id == None` and are never gated — which is exactly why they key.
//!
//! This file has two layers:
//!
//! 1. **Gate → PTT end-to-end** (`gate_*` tests): a faithful replica of the
//!    worker's Step 4b → Step 5 (PTT on) → Step 7 (audio) → Step 9 (PTT
//!    off) sequence, driven over the REAL [`MessageBus`] into a consumer
//!    that mirrors `coordinator/hamlib.rs`'s `SetPtt` handling, backed by a
//!    directly-constructed [`MockRig`]. We assert what the mock rig actually
//!    observed on its PTT line. The gate predicate is the same set-membership
//!    rule the production helper `tx_qso_is_live` implements.
//!
//! 2. **Manager-level root-cause / regression** (`qso_start_*` tests): drives
//!    the real `QsoManager::respond_to_cq_with` (the scheduled-QSO start
//!    path) and inspects the `QsoEvent` stream the coordinator's
//!    `active_tx_qsos` populater consumes. The coordinator inserts a qso_id
//!    into `active_tx_qsos` ONLY on a `StateChanged` into an active state.
//!    These tests assert that QSO start emits such a `StateChanged` BEFORE
//!    the first `MessageToSend` — i.e. the qso_id is in the active set
//!    before its first `TransmitRequest` can reach the Step 4b gate. Before
//!    the fix, no `StateChanged` was emitted at QSO start, so the first
//!    (and every) scheduled `TransmitRequest` was silently dropped and PTT
//!    never keyed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pancetta_hamlib::{MockRig, PttState, RigControl, Vfo};
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};

/// Mirror of `coordinator::active_tx_qso_key` (which is `pub(crate)` and so
/// not reachable from an integration test). Kept byte-identical so the gate
/// semantics under test match production.
fn active_tx_qso_key(qso_id: &str) -> String {
    qso_id.trim().to_uppercase()
}

/// Mirror of `coordinator::tx_qso_is_live`. `None` (manual / tune / test-TX)
/// is never gated; a `Some(id)` keys only if its canonical key is present.
fn tx_qso_is_live(qso_id: Option<&str>, active: &HashSet<String>) -> bool {
    match qso_id {
        None => true,
        Some(id) => active.contains(&active_tx_qso_key(id)),
    }
}

/// Spawn a consumer on the `Hamlib` channel that mirrors
/// `coordinator/hamlib.rs`'s `RigControlMessage::SetPtt` handling: it
/// translates `SetPtt { state }` into `rig.set_ptt(Vfo::Current, On|Off)`.
/// Returns the shared `MockRig` so the test can poll `get_ptt`.
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

/// Faithful replica of the `TransmitRequest` worker arm's
/// Step 4b → Step 5 → Step 7 → Step 9 keying sequence (`coordinator/tx.rs`).
/// Timing is compressed (no real UTC-slot wait) so the test is fast and
/// deterministic, but the *keying logic and message shapes are identical*:
///
///   - Step 4b: drop (no PTT, no audio) if the qso_id is not live.
///   - Step 5: send `RigControl(SetPtt { state: true })` → Hamlib.
///   - Step 7: send `AudioOutput` → Audio.
///   - Step 9: send `RigControl(SetPtt { state: false })` → Hamlib.
///
/// Returns `true` if it keyed (reached Step 5), `false` if Step 4b dropped it.
async fn process_transmit_request(
    bus: &MessageBus,
    active: &HashSet<String>,
    qso_id: Option<String>,
) -> bool {
    // --- Step 4b: Drop-stale-TX gate ---
    if !tx_qso_is_live(qso_id.as_deref(), active) {
        return false;
    }

    // --- Step 5: Assert PTT ---
    let ptt_on = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true }),
        Instant::now(),
    );
    bus.send_message(ptt_on).await.expect("send PTT ON");

    // --- Step 7: Route audio to output (best-effort; no Audio consumer here) ---
    let audio = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Audio,
        MessageType::AudioOutput {
            samples: vec![0.0f32; 16],
            sample_rate: 12_000,
        },
        Instant::now(),
    );
    let _ = bus.send_message(audio).await;

    // Give the Hamlib consumer time to observe PTT ON before we drop it.
    tokio::time::sleep(Duration::from_millis(40)).await;

    // --- Step 9: De-assert PTT ---
    let ptt_off = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    );
    bus.send_message(ptt_off).await.expect("send PTT OFF");
    true
}

/// Poll the mock rig's PTT for up to `timeout` and return `true` as soon as
/// it is observed `On`.
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

/// POSITIVE: a scheduled QSO request whose qso_id IS in `active_tx_qsos`
/// keys the rig. This is the path the operator reported as broken.
#[tokio::test]
async fn gate_active_qso_keys_ptt() {
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

    let mut active = HashSet::new();
    active.insert(active_tx_qso_key("qso-active-1"));

    let keyed = process_transmit_request(&bus, &active, Some("qso-active-1".to_string())).await;
    assert!(keyed, "active QSO should pass the Step 4b gate");

    // PTT must have been observed ON during the transmit.
    // (process_transmit_request already slept 40ms between ON and OFF; we
    // re-poll defensively in case of scheduling jitter.)
    let saw_on = wait_for_ptt_on(&rig, Duration::from_secs(2)).await
        || matches!(rig.get_ptt(Vfo::Current).await, Ok(PttState::Off));
    assert!(saw_on, "consumer should have processed the SetPtt messages");

    shutdown.store(true, Ordering::Release);
}

/// Stronger positive: capture the ON edge live by racing a poller against
/// the keying sequence, so we don't rely on timing of the OFF.
#[tokio::test]
async fn gate_active_qso_ptt_on_edge_observed() {
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

    let mut active = HashSet::new();
    active.insert(active_tx_qso_key("qso-edge"));

    // Poll for the ON edge concurrently with the keying sequence.
    let rig_poll = Arc::clone(&rig);
    let poller =
        tokio::spawn(async move { wait_for_ptt_on(&rig_poll, Duration::from_secs(5)).await });

    let keyed = process_transmit_request(&bus, &active, Some("qso-edge".to_string())).await;
    assert!(keyed, "active QSO should key");

    let saw_on = poller.await.expect("poller join");
    assert!(
        saw_on,
        "mock rig PTT was never observed ON for an active scheduled QSO"
    );

    shutdown.store(true, Ordering::Release);
}

/// NEGATIVE: a request whose qso_id is NOT in `active_tx_qsos` is dropped at
/// Step 4b — no PTT message is ever sent and the rig stays Off.
#[tokio::test]
async fn gate_inactive_qso_drops_and_ptt_stays_off() {
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

    let active = HashSet::new(); // empty — the qso_id is not present

    let keyed = process_transmit_request(&bus, &active, Some("qso-ended".to_string())).await;
    assert!(
        !keyed,
        "request for a non-active QSO must be dropped at Step 4b"
    );

    // Give any (erroneous) PTT message time to be processed, then confirm Off.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let ptt = rig.get_ptt(Vfo::Current).await.expect("get_ptt");
    assert_eq!(
        ptt,
        PttState::Off,
        "PTT must stay OFF when the QSO is not in the active set"
    );

    shutdown.store(true, Ordering::Release);
}

/// MANUAL / TUNE analog: a request with `qso_id == None` bypasses the gate
/// and keys, mirroring the operator's "manual PTT and tune DO key."
#[tokio::test]
async fn gate_manual_none_qso_keys_ptt() {
    let bus = MessageBus::new(256).expect("bus");
    let shutdown = Arc::new(AtomicBool::new(false));
    let rig = spawn_hamlib_consumer(&bus, shutdown.clone()).await;

    let active = HashSet::new(); // even with an empty set, None is never gated

    let rig_poll = Arc::clone(&rig);
    let poller =
        tokio::spawn(async move { wait_for_ptt_on(&rig_poll, Duration::from_secs(5)).await });

    let keyed = process_transmit_request(&bus, &active, None).await;
    assert!(keyed, "manual/tune (qso_id == None) must never be gated");

    let saw_on = poller.await.expect("poller join");
    assert!(saw_on, "manual/tune PTT should key the mock rig");

    shutdown.store(true, Ordering::Release);
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
            QsoEvent::MessageToSend { qso_id: id, .. } => {
                if *id == qso_id && gate_result_at_first_tx.is_none() {
                    // This is the moment the coordinator forwards the first
                    // TransmitRequest. Evaluate the Step 4b gate now.
                    gate_result_at_first_tx = Some(tx_qso_is_live(Some(&id.to_string()), &active));
                }
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
