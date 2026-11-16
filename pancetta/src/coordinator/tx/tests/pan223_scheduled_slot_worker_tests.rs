use super::pan115_worker_gate_tests::{worker_harness, Harness};
use crate::message_bus::{
    ComponentId, ComponentMessage, MessageType, QsoMessage, TransmitRequestItem, TxOrigin,
};
use pancetta_core::slot::{current_slot_start_with_period, SlotParity, SLOT_NS};
use pancetta_qso::TxAttemptId;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

type Report = (String, TxAttemptId, chrono::DateTime<chrono::Utc>);

/// Dial on 20m and `qsos` live, as the QSO coordinator leaves them.
fn setup(h: &Harness, qsos: &[&str]) {
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut live = h.coordinator.active_tx_qsos.write().unwrap();
    for qso in qsos {
        live.insert(super::super::active_tx_qso_key(qso));
    }
}

async fn send(h: &Harness, message_type: MessageType) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            message_type,
            Instant::now(),
        ))
        .await
        .unwrap();
}

fn request(qso: &str, id: u64, parity: SlotParity) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: Some(TxAttemptId(id)),
        message_text: "K9ZZ K5ARH EM12".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some(qso.to_string()),
        tx_parity: Some(parity),
        origin: TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// Every `TxFrameScheduled` on the Qso channel until `count` arrived or
/// 5 s passed, then 300 ms more so a duplicate would show up.
async fn scheduled_reports(h: &Harness, count: usize) -> Vec<Report> {
    let mut reports = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let drain = |reports: &mut Vec<Report>| {
        while let Ok(msg) = h.qso_rx.try_recv() {
            if let MessageType::QsoMessage(QsoMessage::TxFrameScheduled {
                qso_id,
                attempt_id,
                target_slot,
            }) = msg.message_type
            {
                reports.push((qso_id, attempt_id, target_slot));
            }
        }
    };
    while reports.len() < count && Instant::now() < deadline {
        drain(&mut reports);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain(&mut reports);
    reports
}

/// The start of the slot after the current one, and its parity: a
/// request with that parity always defers to it.
fn next_slot() -> (chrono::DateTime<chrono::Utc>, SlotParity) {
    let next = current_slot_start_with_period(chrono::Utc::now(), SLOT_NS)
        + chrono::Duration::nanoseconds(SLOT_NS);
    (next, SlotParity::of_with_period(next, SLOT_NS))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_request_reports_its_attempt_and_deferred_target() {
    let h = worker_harness().await;
    setup(&h, &["qa"]);
    let (next, parity) = next_slot();

    send(&h, request("qa", 41, parity)).await;

    assert_eq!(
        scheduled_reports(&h, 1).await,
        vec![("qa".to_string(), TxAttemptId(41), next)]
    );
}

/// AC 1, worker half: a request the worker dequeues past own slot S's
/// 1.7 s acceptance cap is reported at S + 30 s, never S.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_dequeued_past_the_acceptance_cap_reports_the_next_own_slot() {
    let h = worker_harness().await;
    setup(&h, &["qa"]);
    // Wait until 2.0-12.0 s into a slot (at most ~5 s).
    let now = chrono::Utc::now();
    let phase = now - current_slot_start_with_period(now, SLOT_NS);
    let wait = if phase < chrono::Duration::milliseconds(2_000) {
        chrono::Duration::milliseconds(2_000) - phase
    } else if phase > chrono::Duration::milliseconds(12_000) {
        chrono::Duration::milliseconds(17_000) - phase
    } else {
        chrono::Duration::zero()
    };
    tokio::time::sleep(wait.to_std().unwrap()).await;
    let s = current_slot_start_with_period(chrono::Utc::now(), SLOT_NS);
    let own = SlotParity::of_with_period(s, SLOT_NS);

    send(&h, request("qa", 42, own)).await;

    let reports = scheduled_reports(&h, 1).await;
    assert_eq!(
        reports,
        vec![(
            "qa".to_string(),
            TxAttemptId(42),
            s + chrono::Duration::seconds(30)
        )],
        "dequeued past the cap of own slot {s}, the frame is scheduled into the next own slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_reports_each_item_at_the_bundle_target() {
    let h = worker_harness().await;
    setup(&h, &["qa", "qb"]);
    let (next, parity) = next_slot();
    let item = |text: &str, offset: f64, qso: &str, id: u64| TransmitRequestItem {
        attempt_id: Some(TxAttemptId(id)),
        message_text: text.to_string(),
        frequency_offset: offset,
        qso_id: Some(qso.to_string()),
        origin: TxOrigin::Local,
        remote_client_key_id: None,
    };

    send(
        &h,
        MessageType::MultiTransmitRequest {
            items: vec![
                item("SM2LIY K5ARH R-14", 1000.0, "qa", 51),
                item("C6AVD K5ARH EM10", 1600.0, "qb", 52),
            ],
            tx_parity: Some(parity),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        },
    )
    .await;

    assert_eq!(
        scheduled_reports(&h, 2).await,
        vec![
            ("qa".to_string(), TxAttemptId(51), next),
            ("qb".to_string(), TxAttemptId(52), next),
        ]
    );
}
