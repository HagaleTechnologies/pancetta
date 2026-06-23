//! Verifies that QSO metadata latches tx_parity at QSO start and
//! every subsequent MessageToSend for that QSO carries the same
//! latched value.

use pancetta_core::slot::SlotParity;
use pancetta_qso::{MessageType, QsoEvent, QsoManager, QsoManagerConfig};
use tokio::sync::broadcast::error::TryRecvError;

#[tokio::test]
async fn respond_to_cq_latches_opposite_parity() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let mut rx = mgr.subscribe();

    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("respond_to_cq");

    // Drain events; expect a MessageToSend with tx_parity = Odd (opposite of Even).
    loop {
        match rx.try_recv() {
            Ok(QsoEvent::MessageToSend { tx_parity, .. }) => {
                assert_eq!(tx_parity, Some(SlotParity::Odd));
                break;
            }
            Ok(_) => continue,
            Err(TryRecvError::Empty) => {
                tokio::task::yield_now().await;
                continue;
            }
            Err(TryRecvError::Closed) => panic!("broadcast closed"),
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }

    // Verify the latched parity is in metadata.
    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, Some(SlotParity::Odd));
}

#[tokio::test]
async fn respond_to_cq_with_no_dx_parity_latches_none() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, None)
        .await
        .expect("respond_to_cq");

    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, None);
}

/// A multi-message QSO progression must produce MessageToSend events that
/// all carry the same latched tx_parity (opposite of the DX's slot).
///
/// Drives four explicit MessageToSend emissions via `send_message` — the
/// same helper the auto_sequencer uses internally — to verify the latch
/// holds across the full RespondingToCq → SendingReport → WaitingForConfirmation
/// state chain.
#[tokio::test]
async fn four_message_qso_parity_latch_holds() {
    let mgr = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    });
    mgr.start().await.expect("start");

    let mut rx = mgr.subscribe();

    // DX is on Even slots → we must TX on Odd.
    let qso_id = mgr
        .respond_to_cq("K1ABC".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("respond_to_cq");

    // Advance through the state machine so later send_message calls are
    // meaningful (they read parity from metadata, not from current state).
    mgr.process_message(
        MessageType::SignalReport {
            from_station: "K1ABC".to_string(),
            to_station: "K5ARH".to_string(),
            report: -10,
        },
        "K5ARH K1ABC -10".to_string(),
        1500.0,
        Some(-10.0),
    )
    .await
    .expect("process signal report");

    mgr.process_message(
        MessageType::ReportAck {
            from_station: "K1ABC".to_string(),
            to_station: "K5ARH".to_string(),
            report: -10,
        },
        "K5ARH K1ABC R-10".to_string(),
        1500.0,
        Some(-10.0),
    )
    .await
    .expect("process report ack");

    // Emit additional MessageToSend events explicitly (same path the
    // auto_sequencer uses) to exercise the parity latch across messages 2-4.
    mgr.send_message(
        qso_id,
        MessageType::SignalReport {
            from_station: "K5ARH".to_string(),
            to_station: "K1ABC".to_string(),
            report: -12,
        },
        1500.0,
    )
    .await;

    mgr.send_message(
        qso_id,
        MessageType::ReportAck {
            from_station: "K5ARH".to_string(),
            to_station: "K1ABC".to_string(),
            report: -12,
        },
        1500.0,
    )
    .await;

    mgr.send_message(
        qso_id,
        MessageType::FinalConfirmation {
            from_station: "K5ARH".to_string(),
            to_station: "K1ABC".to_string(),
        },
        1500.0,
    )
    .await;

    // Drain events and assert every MessageToSend carries tx_parity = Odd.
    let mut seen_message_to_send: u32 = 0;
    loop {
        match tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Ok(QsoEvent::MessageToSend { tx_parity, .. })) => {
                assert_eq!(
                    tx_parity,
                    Some(SlotParity::Odd),
                    "parity must stay latched on MessageToSend #{seen_message_to_send}"
                );
                seen_message_to_send += 1;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break, // broadcast closed
            Err(_) => break,     // timeout — no more events
        }
    }

    assert!(
        seen_message_to_send >= 4,
        "expected at least 4 MessageToSend events (got {})",
        seen_message_to_send
    );

    // Final sanity check: metadata still carries the latched parity.
    let progress = mgr.get_qso(qso_id).await.expect("get_qso");
    assert_eq!(progress.metadata.tx_parity, Some(SlotParity::Odd));
}
