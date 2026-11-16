use super::pan115_worker_gate_tests::{
    await_completion, await_ptt_on, request, worker_harness, Harness,
};
use crate::message_bus::{
    ComponentId, ComponentMessage, MessageType, TransmitRequestItem, TxOrigin,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Below every mode's effective key-time cap (FT2 ≈ 640 ms, FT4 1500 ms,
/// FT8 3000 ms at the default `tx_late_max_ms`).
const KEY_DOWN_BOUND: Duration = Duration::from_millis(500);

/// 20m: in-band under the default region-2 plan.
const IN_BAND_DIAL_HZ: u64 = 14_074_000;

/// The worker has passed every final gate and is parked at key-down by
/// the armed probe. Hold the bus table the way an in-flight
/// `create_channel` does, let the worker key, and report whether PTT-on
/// reached Hamlib within the bound while the table was still held.
async fn keys_down_while_table_held(h: &Harness) -> bool {
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.key_down_probe.reached.notified(),
    )
    .await
    .expect("worker must pass every final gate and reach key-down");
    let hold = h.coordinator.message_bus.hold_channel_table().await;
    h.coordinator.key_down_probe.release.notify_one();
    let keyed = await_ptt_on(h, KEY_DOWN_BOUND).await; // try_recv only: no bus lock
    drop(hold);
    keyed
}

/// One two-item local bundle, built like
/// `pan118_worker_band_plan_tests::send_bundle`.
async fn send_bundle(h: &Harness, a_offset: f64, b_offset: f64) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![
                    TransmitRequestItem {
                        attempt_id: None,
                        message_text: "CQ N0CALL FN20".to_string(),
                        frequency_offset: a_offset,
                        qso_id: None,
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                    TransmitRequestItem {
                        attempt_id: None,
                        message_text: "CQ N0CALL EN27".to_string(),
                        frequency_offset: b_offset,
                        qso_id: None,
                        origin: TxOrigin::Local,
                        remote_client_key_id: None,
                    },
                ],
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_tx_keys_down_while_a_registration_holds_the_bus_table() {
    let h = worker_harness().await;
    h.coordinator
        .key_down_probe
        .armed
        .store(true, Ordering::Release);
    request(&h, "CQ N0CALL FN20").await;
    assert!(
        keys_down_while_table_held(&h).await,
        "single-TX key-down waited on the table"
    );
    let (completed, _, _) = await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(20)).await;
    assert_eq!(completed, Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_keys_down_while_a_registration_holds_the_bus_table() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    h.coordinator
        .key_down_probe
        .armed
        .store(true, Ordering::Release);
    send_bundle(&h, 500.0, 2_500.0).await;
    assert!(
        keys_down_while_table_held(&h).await,
        "bundle key-down waited on the table"
    );
    let (completed, _, _) = await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(50)).await;
    assert_eq!(completed, Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tune_keys_down_while_a_registration_holds_the_bus_table() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
    h.coordinator
        .key_down_probe
        .armed
        .store(true, Ordering::Release);
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs: 1,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        keys_down_while_table_held(&h).await,
        "tune key-down waited on the table"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn manual_ptt_command_enqueues_both_states_while_the_table_is_held() {
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let _hold = bus.hold_channel_table().await;
    for state in [true, false] {
        assert_eq!(
            super::send_ptt_command(&bus, ComponentId::Tui, state, || true),
            crate::message_bus::SendIfOutcome::Enqueued
        );
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.source, ComponentId::Tui);
        assert!(matches!(
            msg.message_type,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: s })
                if s == state
        ));
    }
}
