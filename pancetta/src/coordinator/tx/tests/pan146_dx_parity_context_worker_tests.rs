use super::pan115_worker_gate_tests::{await_completion, worker_harness, Harness};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, TxOrigin};
use pancetta_core::slot::{SlotParity, SLOT_NS};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DIAL_20M: u64 = 14_074_000;
const DIAL_15M: u64 = 21_074_000;
const TEXT: &str = "5Z4VJ W5AU EM10";

/// Opens a manual call to 5Z4VJ (latched `tx_parity = Odd`), publishes
/// the manager to the worker, keeps a 20 m / FT8-grid Odd sighting of
/// 5Z4VJ fresh, and sends one Odd frame for the QSO with the dial at
/// `live_dial_hz`. Returns `(completed, diagnostics, ptt_on)`.
async fn call_with_dx_heard_on_20m(
    h: &Harness,
    live_dial_hz: u64,
) -> (Option<bool>, Vec<String>, bool) {
    h.coordinator
        .operating_frequency_hz
        .store(live_dial_hz, Ordering::Relaxed);
    let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W5AU".into(), // QsoManager refuses the N0CALL placeholder
        ..Default::default()
    });
    let id = manager
        .respond_to_cq_manual("5Z4VJ".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    h.coordinator.qso_manager_watch.send_replace(Some(manager));
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&id.to_string()));

    let stop = Arc::new(AtomicBool::new(false));
    let refresher = {
        let cts = h.coordinator.cross_time_state.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                cts.a7_recent_calls
                    .write()
                    .unwrap()
                    .record(pancetta_qso::A7ExpectedCall {
                        callsign: "5Z4VJ".into(),
                        freq_hz: 1500.0,
                        slot_parity: 1,
                        decoded_at: std::time::SystemTime::now(),
                        captured_dial_hz: DIAL_20M,
                        slot_ns: SLOT_NS,
                    });
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };

    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: TEXT.into(),
                frequency_offset: 1500.0,
                qso_id: Some(id.to_string()),
                tx_parity: Some(SlotParity::Odd),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    let result = await_completion(h, TEXT, Duration::from_secs(50)).await;
    stop.store(true, Ordering::Release);
    let _ = refresher.await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_band_sighting_still_holds_the_call() {
    let h = worker_harness().await;
    let (completed, diagnostics, ptt_on) = call_with_dx_heard_on_20m(&h, DIAL_20M).await;
    assert_eq!(completed, Some(false));
    assert!(!ptt_on, "a same-band collision must not key");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("DX observed transmitting on the colliding parity")),
        "{diagnostics:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_band_sighting_does_not_hold_the_call() {
    let h = worker_harness().await;
    let (completed, diagnostics, ptt_on) = call_with_dx_heard_on_20m(&h, DIAL_15M).await;
    assert_eq!(completed, Some(true), "{diagnostics:?}");
    assert!(ptt_on, "the 15 m call must key");
    assert!(!diagnostics.iter().any(|d| d.contains("colliding parity")));
}
