//! Verifies that QSO metadata latches tx_parity at QSO start and
//! every subsequent MessageToSend for that QSO carries the same
//! latched value.

use pancetta_core::slot::SlotParity;
use pancetta_qso::{QsoEvent, QsoManager, QsoManagerConfig};
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
