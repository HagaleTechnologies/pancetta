// PAN-223. Formerly the inline module `pan223_frame_scheduled_report_tests`.
use super::*;
use pancetta_core::slot::{SlotParity, SLOT_NS};
use pancetta_qso::TxAttemptId;

/// 2026-01-01 00:00:00 UTC is an Even FT8 slot start (as in
/// `schedule_tx_tests::at`).
fn at(seconds: f64) -> UtcTimestamp {
    let base = UtcTimestamp::from_ymd_hms(2026, 1, 1, 0, 0, 0).unwrap();
    base + SignedDuration::from_nanos((seconds * 1_000_000_000.0) as i64)
}

/// 2.3 s into own slot 0: past FT8's 1.7 s acceptance cap, so deferred
/// to the next own slot.
fn deferred_schedule() -> TxSchedule {
    schedule_tx(
        at(2.3),
        SlotParity::Even,
        current_slot_acceptance_cap_ms(pancetta_ft8::Protocol::Ft8, 3000),
        12_000,
        SLOT_NS,
    )
}

#[tokio::test]
async fn report_frame_scheduled_carries_the_schedule_target_slot() {
    let bus = MessageBus::new(16).unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let schedule = deferred_schedule();
    assert_eq!(schedule.target_slot, at(30.0));

    report_frame_scheduled(&bus, Some("qa"), Some(TxAttemptId(7)), &schedule).await;

    let msg = qso.try_recv().expect("the report reaches Qso");
    assert_eq!(msg.source, ComponentId::Ft8Transmitter);
    match msg.message_type {
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameScheduled {
            qso_id,
            attempt_id,
            target_slot,
        }) => {
            assert_eq!(qso_id, "qa");
            assert_eq!(attempt_id, TxAttemptId(7));
            assert_eq!(target_slot, schedule.target_slot);
        }
        other => panic!("expected TxFrameScheduled, got {other:?}"),
    }
    assert!(qso.try_recv().is_err());
}

#[tokio::test]
async fn report_frame_scheduled_skips_frames_without_a_qso_or_attempt() {
    let bus = MessageBus::new(16).unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let schedule = deferred_schedule();

    report_frame_scheduled(&bus, None, Some(TxAttemptId(7)), &schedule).await;
    report_frame_scheduled(&bus, Some("qa"), None, &schedule).await;

    assert!(
        qso.try_recv().is_err(),
        "a frame with no QSO or no attempt has no ledger entry to re-key"
    );
}

/// PAN-185: a full Qso channel parks the report instead of dropping it.
#[tokio::test]
async fn pan223_full_qso_channel_still_delivers_tx_frame_scheduled() {
    let bus = MessageBus::new(1).unwrap();
    let qso = bus.durable_inbox(ComponentId::Qso).await.unwrap();
    let (qso_tx, _) = bus.get_or_create_channel(ComponentId::Qso).await.unwrap();
    qso_tx
        .try_send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                qso_id: "filler".into(),
                message_text: "FILLER".into(),
                replaced: None,
                attempt_id: None,
            }),
            Instant::now(),
        ))
        .unwrap();
    let before = bus.get_statistics();
    let schedule = deferred_schedule();

    report_frame_scheduled(&bus, Some("qa"), Some(TxAttemptId(7)), &schedule).await;

    let after = bus.get_statistics();
    assert_eq!(after.parked_messages, before.parked_messages + 1);
    assert_eq!(after.dropped_messages, before.dropped_messages);
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, .. })
            if qso_id == "filler"
    ));
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameScheduled {
            ref qso_id,
            attempt_id: TxAttemptId(7),
            target_slot,
        }) if qso_id == "qa" && target_slot == at(30.0)
    ));
    assert!(qso.try_recv().is_err());
}
