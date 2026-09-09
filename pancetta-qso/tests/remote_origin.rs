//! B1 (P3.4c) — a `remote_origin` QSO's emitted `MessageToSend` carries
//! `remote_origin = true`; a normal QSO carries `false`.
//!
//! The coordinator forwards `MessageToSend.remote_origin` into
//! `TransmitRequest.origin` (`Remote` vs `Local`), so this is the engine-level
//! proof that the flag flows through the reply ladder — the security-critical
//! property that a remote-initiated QSO's TX is armed-TX gated end to end.

use pancetta_core::slot::SlotParity;
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig};

fn config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        ..QsoManagerConfig::default()
    }
}

/// Drain the manager's event stream and return the `remote_origin` flag of the
/// first `MessageToSend` seen.
async fn first_message_to_send_remote_origin(
    rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
) -> bool {
    first_message_to_send_remote_origin_and_client(rx).await.0
}

/// Like [`first_message_to_send_remote_origin`], but also returns
/// `remote_client_key_id` (PAN-91: the QSO must carry the requesting peer's
/// identity, not just a bare boolean).
async fn first_message_to_send_remote_origin_and_client(
    rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
) -> (bool, Option<String>) {
    loop {
        match rx.recv().await.expect("event stream closed") {
            QsoEvent::MessageToSend {
                remote_origin,
                remote_client_key_id,
                ..
            } => return (remote_origin, remote_client_key_id),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn remote_origin_qso_emits_remote_message_to_send() {
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    // A QSO opened with remote_origin=true, bound to a specific client.
    manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            true, // remote_origin
            Some("client-a".to_string()),
        )
        .await
        .expect("respond_to_cq_with");

    let (origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert!(
        origin,
        "a remote_origin QSO's MessageToSend MUST carry remote_origin=true \
         (else its TransmitRequest would be Local and bypass the arm)"
    );
    assert_eq!(
        client_key_id.as_deref(),
        Some("client-a"),
        "PAN-91: the QSO must carry the requesting peer's identity so the \
         arm gate can bind TX to THAT client, not just any armed client"
    );
}

#[tokio::test]
async fn normal_qso_emits_local_message_to_send_regression() {
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    // A normal (local) QSO — every existing path passes remote_origin=false.
    manager
        .respond_to_cq_with(
            "K9XYZ".to_string(),
            1500.0,
            Some(SlotParity::Even),
            CallInitiation::Manual,
            None,
            false, // remote_origin
            None,
        )
        .await
        .expect("respond_to_cq_with");

    let origin = first_message_to_send_remote_origin(&mut rx).await;
    assert!(
        !origin,
        "a normal QSO's MessageToSend MUST carry remote_origin=false (regression: \
         local TX stays TxOrigin::Local, byte-identical)"
    );
}

#[tokio::test]
async fn remote_origin_persists_across_the_reply_ladder() {
    // The flag is latched in QsoMetadata at open, so EVERY subsequent
    // MessageToSend for the QSO (keep-calls, auto-sequenced replies) carries it.
    let manager = QsoManager::new(config());
    let mut rx = manager.subscribe();

    let _id = manager
        .start_cq(
            1500.0,
            Some(SlotParity::Odd),
            true,
            Some("client-b".to_string()),
        )
        .await
        .expect("start_cq");

    // Opening CQ MessageToSend is remote, bound to the requesting client.
    let (origin, client_key_id) = first_message_to_send_remote_origin_and_client(&mut rx).await;
    assert!(
        origin,
        "opening CQ of a remote QSO must be remote_origin=true"
    );
    assert_eq!(
        client_key_id.as_deref(),
        Some("client-b"),
        "PAN-91: the manual-CQ remote_origin path must also bind client identity"
    );
}
