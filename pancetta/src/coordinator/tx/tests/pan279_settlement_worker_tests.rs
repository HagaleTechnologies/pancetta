use super::pan115_worker_gate_tests::{
    await_ptt_on, worker_harness, worker_harness_with_config, Harness,
};
use super::*;
use crate::message_bus::{QsoMessage, RigControlMessage, TxOrigin};
use pancetta_qso::TxAttemptId;
use std::time::Duration;

fn live(h: &Harness, qso_id: &str) {
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(qso_id));
}

fn request(
    text: &str,
    hz: f64,
    qso_id: Option<&str>,
    attempt: u64,
    tx_parity: Option<pancetta_core::slot::SlotParity>,
) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: Some(TxAttemptId(attempt)),
        message_text: text.into(),
        frequency_offset: hz,
        qso_id: qso_id.map(str::to_string),
        tx_parity,
        origin: TxOrigin::Local,
        remote_client_key_id: None,
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

fn parity_now() -> pancetta_core::slot::SlotParity {
    let now = chrono::Utc::now();
    pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    )
}

#[derive(Default, Debug)]
struct Observed {
    /// Autonomous `(qso_id, success, attempt_id)` completions.
    completions: Vec<(Option<String>, bool, Option<TxAttemptId>)>,
    /// Qso-bound reports, as short strings in arrival order.
    qso: Vec<String>,
    ptt_on: usize,
}

/// Drain every receiver for `within`, or until `done` holds.
async fn observe(h: &Harness, within: Duration, done: impl Fn(&Observed) -> bool) -> Observed {
    let mut o = Observed::default();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline && !done(&o) {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if matches!(
                msg.message_type,
                MessageType::RigControl(RigControlMessage::SetPtt { state: true })
            ) {
                o.ptt_on += 1;
            }
        }
        while h.tui_rx.try_recv().is_ok() {}
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                qso_id,
                attempt_id,
                ..
            } = msg.message_type
            {
                o.completions.push((qso_id, success, attempt_id));
            }
        }
        while let Ok(msg) = h.qso_rx.try_recv() {
            match msg.message_type {
                MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                    message_text,
                    attempt_id,
                    ..
                }) => o.qso.push(format!("NotSent {message_text} {attempt_id:?}")),
                MessageType::QsoMessage(QsoMessage::TxFrameScheduled { attempt_id, .. }) => {
                    o.qso.push(format!("Scheduled {attempt_id:?}"))
                }
                MessageType::QsoMessage(QsoMessage::TxFrameSent { message_text, .. }) => {
                    o.qso.push(format!("Sent {message_text}"))
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    o
}

fn for_qso(o: &Observed, qso_id: &str) -> Vec<(bool, Option<TxAttemptId>)> {
    o.completions
        .iter()
        .filter(|(q, ..)| q.as_deref() == Some(qso_id))
        .map(|(_, ok, a)| (*ok, *a))
        .collect()
}

/// Key `first`, then 600 ms after PTT-on (inside the keyed slot) send
/// the superseder `second` built with the current slot's parity.
async fn key_then_supersede(
    h: &Harness,
    first: MessageType,
    second: impl FnOnce(pancetta_core::slot::SlotParity) -> MessageType,
) {
    send(h, first).await;
    assert!(
        await_ptt_on(h, Duration::from_secs(20)).await,
        "worker did not key the first frame"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    send(h, second(parity_now())).await;
}

/// Scenario 2: an autonomous self-CQ re-keyed by a newer frame for the
/// same QSO settles once, as sent, under the replacement's attempt; the
/// abandoned frame only rolls back its own ledger attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan279_same_qso_replace_settles_the_self_cq_once_as_sent() {
    let h = worker_harness().await;
    live(&h, "qso-cq");
    key_then_supersede(
        &h,
        request("CQ N0CALL FN20", 1500.0, Some("qso-cq"), 21, None),
        |p| request("CQ N0CALL FN20", 1700.0, Some("qso-cq"), 22, Some(p)),
    )
    .await;
    let o = observe(&h, Duration::from_secs(20), |o| {
        o.qso.iter().any(|q| q.starts_with("Sent "))
    })
    .await;
    assert_eq!(
        for_qso(&o, "qso-cq"),
        vec![(true, Some(TxAttemptId(22)))],
        "{o:?}"
    );
    assert!(
        o.qso
            .contains(&"NotSent CQ N0CALL FN20 Some(TxAttemptId(21))".to_string()),
        "{o:?}"
    );
    assert!(o.qso.contains(&"Sent CQ N0CALL FN20".to_string()), "{o:?}");
}

/// PAN-38 round 5 kept: a manual (`qso_id: None`) superseder does not
/// own the QSO's outcome, so the abandoned frame still fails to
/// Autonomous under its own attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan279_manual_replace_still_fails_the_abandoned_frame() {
    let h = worker_harness().await;
    live(&h, "qso-cq");
    key_then_supersede(
        &h,
        request("CQ N0CALL FN20", 1500.0, Some("qso-cq"), 21, None),
        |p| MessageType::TransmitRequest {
            attempt_id: None,
            message_text: "CQ TEST N0CALL FN20".into(),
            frequency_offset: 1700.0,
            qso_id: None,
            tx_parity: Some(p),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        },
    )
    .await;
    let o = observe(&h, Duration::from_secs(10), |o| {
        !for_qso(o, "qso-cq").is_empty()
    })
    .await;
    assert_eq!(
        for_qso(&o, "qso-cq"),
        vec![(false, Some(TxAttemptId(21)))],
        "{o:?}"
    );
}

/// Scenario 1 through the worker: a superseder too late for the keyed
/// slot (no explicit parity, so it resolves to the other parity) is
/// requeued and airs in its own next slot. The abandoned frame rolls
/// back its attempt; Autonomous hears only the superseder's success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan279_notviable_superseder_airs_in_its_own_next_slot() {
    let h = worker_harness().await;
    live(&h, "qso-sv");
    key_then_supersede(
        &h,
        request("K1ABC W9XYZ FN20", 1500.0, Some("qso-sv"), 11, None),
        |_| request("K1ABC W9XYZ -10", 1500.0, Some("qso-sv"), 12, None),
    )
    .await;
    let o = observe(&h, Duration::from_secs(45), |o| {
        o.qso.iter().any(|q| q == "Sent K1ABC W9XYZ -10")
    })
    .await;
    let not_sent = o
        .qso
        .iter()
        .position(|q| q == "NotSent K1ABC W9XYZ FN20 Some(TxAttemptId(11))");
    let scheduled = o.qso.iter().position(|q| q == "Scheduled TxAttemptId(12)");
    let sent = o.qso.iter().position(|q| q == "Sent K1ABC W9XYZ -10");
    assert!(
        not_sent.is_some() && scheduled.is_some() && sent.is_some(),
        "{o:?}"
    );
    assert!(not_sent < scheduled && scheduled < sent, "{o:?}");
    assert_eq!(
        for_qso(&o, "qso-sv"),
        vec![(true, Some(TxAttemptId(12)))],
        "{o:?}"
    );
    assert_eq!(
        o.ptt_on, 1,
        "one more key-down after the first, no re-supersede loop: {o:?}"
    );
}

/// Single-arm Bundle success: with room for two streams, a same-QSO
/// superseder at a new offset is requeued as a bundle that no longer
/// carries the in-flight frame. The replaced frame (attempt 1), dropped
/// silently before PAN-279, rolls back; Autonomous hears no failure for
/// it, and later the superseder's success under attempt 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan279_single_arm_bundle_success_rolls_back_the_replaced_frame() {
    let mut cfg = pancetta_config::Config::default();
    cfg.autonomous.max_concurrent_qsos = 2;
    let h = worker_harness_with_config(cfg).await;
    live(&h, "qso-1");
    key_then_supersede(
        &h,
        request("K1ABC W9XYZ R-10", 1000.0, Some("qso-1"), 1, None),
        |p| request("K1ABC W9XYZ R-10", 1600.0, Some("qso-1"), 2, Some(p)),
    )
    .await;
    let o = observe(&h, Duration::from_secs(45), |o| {
        !for_qso(o, "qso-1").is_empty()
    })
    .await;
    assert!(
        o.qso
            .contains(&"NotSent K1ABC W9XYZ R-10 Some(TxAttemptId(1))".to_string()),
        "{o:?}"
    );
    assert_eq!(
        for_qso(&o, "qso-1"),
        vec![(true, Some(TxAttemptId(2)))],
        "{o:?}"
    );
}
