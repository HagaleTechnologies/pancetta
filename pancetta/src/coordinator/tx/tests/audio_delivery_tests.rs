// PAN-182. Formerly the inline module `pan182_audio_delivery_tests`.
use super::tx_log_worker_tests::fill_channel;
use super::worker_gate_tests::worker_harness;
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage, TxOrigin};
use std::time::{Duration, Instant};

/// PR #507 review (P1): a Step-7 audio send that comes back `Dropped`
/// (here: the Audio channel is full) enqueued no samples, so the frame
/// never aired even though PTT keyed. Step 10 must report it as not
/// sent: the QSO engine gets the PAN-75 `TxFrameNotSent` mirror (which
/// rolls the attempt back) and never `TxFrameSent`, whose false
/// confirmation would release a held PAN-182 stall switch on a frame
/// that never keyed any audio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_audio_delivery_reports_the_frame_as_not_sent() {
    let h = worker_harness().await;
    assert!(fill_channel(&h, ComponentId::Audio).await > 0);
    // The defer-time liveness gate needs the QSO id to be TX-live (the
    // QSO component publishes this set; here it is seeded directly).
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert("Q1".into());

    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "K1ABC N0CALL FN20".to_string(),
                frequency_offset: 1500.0,
                qso_id: Some("q1".into()),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut completion = None;
    let mut not_sent_mirror = false;
    let mut sent_mirror = false;
    fn capture(
        completion: &mut Option<bool>,
        not_sent_mirror: &mut bool,
        sent_mirror: &mut bool,
        msg: ComponentMessage,
    ) {
        match msg.message_type {
            MessageType::TransmitComplete {
                success,
                ref message_text,
                ..
            } if message_text == "K1ABC N0CALL FN20" => {
                *completion = Some(success);
            }
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent { ref qso_id, .. })
                if qso_id == "q1" =>
            {
                *not_sent_mirror = true;
            }
            MessageType::QsoMessage(QsoMessage::TxFrameSent { ref qso_id, .. })
                if qso_id == "q1" =>
            {
                *sent_mirror = true;
            }
            _ => {}
        }
    }
    while completion.is_none() && Instant::now() < deadline {
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
        }
        while let Ok(msg) = h.qso_rx.try_recv() {
            capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Anything still in flight after the completion.
    while let Ok(msg) = h.qso_rx.try_recv() {
        capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
    }

    assert_eq!(
        completion,
        Some(false),
        "a frame whose audio never enqueued must complete as failed"
    );
    assert!(
        not_sent_mirror,
        "the QSO engine must get the PAN-75 not-sent mirror"
    );
    assert!(
        !sent_mirror,
        "no aired mirror may be sent for a frame that never keyed audio"
    );
}
