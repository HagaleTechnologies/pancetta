use super::*;
use crate::message_bus::{ComponentId, MessageBus, MessageType};

#[tokio::test]
async fn log_tx_frame_emits_the_given_fields_and_timestamp() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    // A late-but-viable key: the caller passes the ACTUAL audio-start
    // instant, which can be well past the nominal slot boundary — this
    // must round-trip untouched, not get snapped to any slot grid.
    let actual_audio_start = UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 22).unwrap();
    log_tx_frame(
        &bus,
        "K5ARH JA1ABC -12".to_string(),
        1500.0,
        Some("qso-1".to_string()),
        actual_audio_start,
    )
    .await;

    let msg = receiver
        .try_recv()
        .expect("a TxFrameLogged message should have been sent");
    match msg.message_type {
        MessageType::TxFrameLogged {
            text,
            freq_hz,
            qso_id,
            timestamp,
        } => {
            assert_eq!(text, "K5ARH JA1ABC -12");
            assert_eq!(freq_hz, 1500.0);
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
            assert_eq!(timestamp, actual_audio_start);
        }
        other => panic!("expected TxFrameLogged, got {other:?}"),
    }
}

#[tokio::test]
async fn cq_frame_qso_id_is_none() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    log_tx_frame(
        &bus,
        "CQ K5ARH EM10".to_string(),
        1200.0,
        None,
        UtcTimestamp::now(),
    )
    .await;

    let msg = receiver
        .try_recv()
        .expect("TxFrameLogged should send for CQ too");
    match msg.message_type {
        MessageType::TxFrameLogged { qso_id, .. } => assert_eq!(qso_id, None),
        other => panic!("expected TxFrameLogged, got {other:?}"),
    }
}

/// `send_tx_queue_status` must never emit `TxFrameLogged` itself, for
/// either a NOW-SENDING or an idle-clear push — Band Activity's history
/// timestamp comes only from Step 7's `log_tx_frame` call, made once the
/// actual audio-start instant is known (see `log_tx_frame`'s doc
/// comment). Guards against re-coupling the two and reintroducing the
/// stale-timestamp bug this decoupling fixed.
#[tokio::test]
async fn send_tx_queue_status_never_emits_tx_frame_logged() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

    send_tx_queue_status(
        &bus,
        Some(crate::message_bus::TxItem {
            text: "K5ARH JA1ABC -12".to_string(),
            freq_hz: 1500.0,
            qso_id: Some("qso-1".to_string()),
            deferred: false,
        }),
        Vec::new(),
    )
    .await;
    let msg = receiver.try_recv().expect("TxQueueStatus should send");
    assert!(matches!(
        msg.message_type,
        MessageType::TxQueueStatus {
            sending: Some(_),
            ..
        }
    ));
    assert!(
        receiver.try_recv().is_err(),
        "no TxFrameLogged from a NOW-SENDING push"
    );

    send_tx_queue_status(&bus, None, Vec::new()).await;
    let msg2 = receiver
        .try_recv()
        .expect("TxQueueStatus should still send");
    assert!(matches!(
        msg2.message_type,
        MessageType::TxQueueStatus { sending: None, .. }
    ));
    assert!(
        receiver.try_recv().is_err(),
        "no TxFrameLogged from an idle-clear push"
    );
}
