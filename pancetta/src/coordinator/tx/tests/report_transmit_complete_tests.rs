use super::*;
use pancetta_core::time::UtcTimestamp;

async fn check(success: bool, qso_id: Option<&str>) {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    report_transmit_complete(
        &bus,
        success,
        "K1ABC N0CALL FN20".into(),
        0,
        qso_id.map(str::to_string),
        None,
    )
    .await
    .unwrap();
    assert!(
        matches!(autonomous.try_recv().unwrap().message_type, MessageType::TransmitComplete { success: got, message_text, duration_ms: 0, qso_id: id, attempt_id: None } if got == success && id.as_deref() == qso_id && message_text == "K1ABC N0CALL FN20")
    );
    assert!(autonomous.try_recv().is_err());
    if !success && qso_id.is_some() {
        assert!(
            matches!(qso.try_recv().unwrap().message_type, MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { attempt_id: _, qso_id: id, message_text, replaced: None }) if Some(id.as_str()) == qso_id && message_text == "K1ABC N0CALL FN20")
        );
    }
    assert!(qso.try_recv().is_err());
}

#[tokio::test]
async fn successful_frame_only_notifies_autonomous() {
    check(true, Some("q1")).await;
}
#[tokio::test]
async fn unkeyed_frame_only_notifies_autonomous() {
    check(false, None).await;
}

fn qso_filler() -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Qso,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
            qso_id: "filler".into(),
            message_text: "FILLER".into(),
            replaced: None,
            attempt_id: None,
        }),
        Instant::now(),
    )
}
fn autonomous_complete(success: bool, qso_id: &str) -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Autonomous,
        MessageType::TransmitComplete {
            success,
            message_text: "K1ABC N0CALL FN20".into(),
            duration_ms: 0,
            qso_id: Some(qso_id.into()),
            attempt_id: None,
        },
        Instant::now(),
    )
}

/// PAN-185 scenario 1: a manual QSO's keep-call frame the TX worker
/// dropped before it aired, reported while the QSO component's channel
/// is full. The rollback parks on the Qso inbox's durable lane and comes
/// out right after the message that filled the channel.
#[tokio::test]
async fn pan185_full_qso_channel_still_delivers_tx_frame_not_sent() {
    let bus = MessageBus::new(1).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let qso = bus.durable_inbox(ComponentId::Qso).await.unwrap();
    let (qso_tx, _) = bus.get_or_create_channel(ComponentId::Qso).await.unwrap();
    qso_tx.try_send(qso_filler()).unwrap();
    let before = bus.get_statistics();

    report_transmit_complete(
        &bus,
        false,
        "K1ABC N0CALL FN20".into(),
        0,
        Some("q1".into()),
        Some(pancetta_qso::TxAttemptId(7)),
    )
    .await
    .unwrap();

    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::TransmitComplete { success: false, qso_id: Some(ref id), .. }
            if id == "q1"
    ));
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, .. })
            if qso_id == "filler"
    ));
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, ref message_text, attempt_id: Some(pancetta_qso::TxAttemptId(7)), replaced: None })
            if qso_id == "q1" && message_text == "K1ABC N0CALL FN20"
    ));
    assert!(qso.try_recv().is_err());
    let after = bus.get_statistics();
    assert_eq!(after.parked_messages, before.parked_messages + 1);
    assert_eq!(after.dropped_messages, before.dropped_messages);
}

/// PAN-185: a played frame's `TxFrameSent` (PAN-182) parks the same way.
#[tokio::test]
async fn pan185_full_qso_channel_still_delivers_tx_frame_sent() {
    let bus = MessageBus::new(1).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let qso = bus.durable_inbox(ComponentId::Qso).await.unwrap();
    let (qso_tx, _) = bus.get_or_create_channel(ComponentId::Qso).await.unwrap();
    qso_tx.try_send(qso_filler()).unwrap();
    let aired = aired_payload(1500.0, crate::message_bus::TxOrigin::Local, None);
    report_played_transmit_complete(
        &bus,
        true,
        "K1ABC N0CALL FN20".into(),
        12_640,
        Some("q1".into()),
        aired.clone(),
        None,
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, .. })
            if qso_id == "filler"
    ));
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameSent { ref qso_id, ref message_text, aired: ref got, .. })
            if qso_id == "q1" && message_text == "K1ABC N0CALL FN20" && *got == aired
    ));
    assert!(qso.try_recv().is_err());
    assert_eq!(bus.get_statistics().dropped_messages, 0);
}

/// PAN-185: a full Autonomous channel parks the completion, so a tracked
/// self-CQ still settles.
#[tokio::test]
async fn pan185_full_autonomous_channel_still_delivers_transmit_complete() {
    let bus = MessageBus::new(1).unwrap();
    let autonomous = bus.durable_inbox(ComponentId::Autonomous).await.unwrap();
    let (auto_tx, _) = bus
        .get_or_create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    auto_tx
        .try_send(autonomous_complete(true, "first"))
        .unwrap();
    report_transmit_complete(
        &bus,
        true,
        "K1ABC N0CALL FN20".into(),
        0,
        Some("q1".into()),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::TransmitComplete { qso_id: Some(ref id), .. } if id == "first"
    ));
    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::TransmitComplete { success: true, qso_id: Some(ref id), .. } if id == "q1"
    ));
    assert!(autonomous.try_recv().is_err());
    assert_eq!(bus.get_statistics().dropped_messages, 0);
}

/// PAN-185: parked completions for one QSO come out in send order, so a
/// later frame's completion cannot overtake an earlier one.
#[tokio::test]
async fn pan185_autonomous_completions_keep_their_order_when_parked() {
    let bus = MessageBus::new(1).unwrap();
    let autonomous = bus.durable_inbox(ComponentId::Autonomous).await.unwrap();
    let (auto_tx, _) = bus
        .get_or_create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    auto_tx
        .try_send(autonomous_complete(true, "first"))
        .unwrap();
    report_transmit_complete(&bus, false, "A".into(), 0, Some("q1".into()), None)
        .await
        .unwrap();
    assert_eq!(bus.durable_lane_len(ComponentId::Autonomous), 1);
    report_transmit_complete(&bus, true, "B".into(), 0, Some("q1".into()), None)
        .await
        .unwrap();
    assert_eq!(bus.durable_lane_len(ComponentId::Autonomous), 2);
    let got: Vec<(bool, String)> = std::iter::from_fn(|| autonomous.try_recv().ok())
        .filter_map(|m| match m.message_type {
            MessageType::TransmitComplete {
                success,
                message_text,
                qso_id: Some(id),
                ..
            } if id == "q1" => Some((success, message_text)),
            _ => None,
        })
        .collect();
    assert_eq!(got, vec![(false, "A".to_string()), (true, "B".to_string())]);
}
#[tokio::test]
async fn failed_qso_frame_also_notifies_qso() {
    check(false, Some("q1")).await;
}

/// PAN-182: a QSO frame that played out also tells the QSO engine it
/// aired, with the offset and binding it aired on, after the unchanged
/// Autonomous completion.
#[tokio::test]
async fn successful_transmit_complete_mirrors_tx_frame_sent_to_qso() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let aired = aired_payload(
        1500.0,
        crate::message_bus::TxOrigin::Remote,
        Some("peer".into()),
    );
    assert_eq!(
        aired,
        pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1500.0,
            remote_binding: Some((true, Some("peer".into()))),
        }
    );
    let reported_from = UtcTimestamp::now();
    report_played_transmit_complete(
        &bus,
        true,
        "K1ABC N0CALL FN20".into(),
        12_640,
        Some("q1".into()),
        aired.clone(),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::TransmitComplete { success: true, ref message_text, duration_ms: 12_640, qso_id: Some(ref id), .. }
            if id == "q1" && message_text == "K1ABC N0CALL FN20"
    ));
    assert!(autonomous.try_recv().is_err());
    let msg = qso.try_recv().unwrap();
    assert_eq!(msg.source, ComponentId::Ft8Transmitter);
    assert!(matches!(
        msg.message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameSent { ref qso_id, ref message_text, aired: ref got, aired_at, attempt_id: None })
            if qso_id == "q1" && message_text == "K1ABC N0CALL FN20" && *got == aired
                && aired_at >= reported_from && aired_at <= UtcTimestamp::now()
    ));
    assert!(qso.try_recv().is_err());
}

/// PAN-280: a played frame's `TxFrameSent` carries the frame's own
/// attempt id, so Qso can settle the report against the call that
/// charged it (the auto-73 resend marker).
#[tokio::test]
async fn played_success_reports_its_attempt_id_on_tx_frame_sent() {
    let bus = MessageBus::new(16).unwrap();
    let (_, _autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let aired = aired_payload(1500.0, crate::message_bus::TxOrigin::Local, None);
    report_played_transmit_complete(
        &bus,
        true,
        "K9ZZ K5ARH 73".into(),
        12_640,
        Some("q1".into()),
        aired,
        Some(pancetta_qso::TxAttemptId(7)),
    )
    .await
    .unwrap();
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameSent {
            ref qso_id,
            attempt_id: Some(pancetta_qso::TxAttemptId(7)),
            ..
        }) if qso_id == "q1"
    ));
    assert!(qso.try_recv().is_err());
}

/// PAN-280: the bundle path reports each item with `item.attempt_id`, so
/// two items played in one bundle each carry their own id.
#[tokio::test]
async fn bundle_item_reports_its_own_attempt_id_on_tx_frame_sent() {
    let bus = MessageBus::new(16).unwrap();
    let (_, _autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    for (qso_id, text, id) in [("q1", "K9ZZ K5ARH 73", 7), ("q2", "W1AW K5ARH RR73", 8)] {
        report_played_transmit_complete(
            &bus,
            true,
            text.into(),
            12_640,
            Some(qso_id.into()),
            aired_payload(1500.0, crate::message_bus::TxOrigin::Local, None),
            Some(pancetta_qso::TxAttemptId(id)),
        )
        .await
        .unwrap();
    }
    for (want_qso, want_id) in [("q1", 7), ("q2", 8)] {
        match qso.try_recv().unwrap().message_type {
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameSent {
                qso_id,
                attempt_id,
                ..
            }) => {
                assert_eq!(qso_id, want_qso);
                assert_eq!(attempt_id, Some(pancetta_qso::TxAttemptId(want_id)));
            }
            other => panic!("expected TxFrameSent, got {other:?}"),
        }
    }
    assert!(qso.try_recv().is_err());
}

/// PAN-182: a frame that did not play out reports only the PAN-75
/// not-sent mirror, never `TxFrameSent`; an untracked frame (no QSO)
/// reports nothing to the QSO engine either way.
#[tokio::test]
async fn failed_transmit_complete_sends_no_tx_frame_sent() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let aired = aired_payload(1500.0, crate::message_bus::TxOrigin::Local, None);
    report_played_transmit_complete(
        &bus,
        false,
        "K1ABC N0CALL FN20".into(),
        0,
        Some("q1".into()),
        aired.clone(),
        None,
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, replaced: None, .. })
            if qso_id == "q1"
    ));
    assert!(qso.try_recv().is_err());

    report_played_transmit_complete(&bus, true, "CQ N0CALL FN20".into(), 0, None, aired, None)
        .await
        .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert!(qso.try_recv().is_err());
}

/// A pivot-duplicate's payload already aired via the pivot, so the QSO
/// engine must roll back the frame the pivot replaced, never this one.
/// Autonomous hears nothing: the pivoted frame keyed and already reported
/// (PAN-185 ownership rule).
#[tokio::test]
async fn pivot_duplicate_drop_rolls_back_the_replaced_frame() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let replaced = pancetta_qso::UnsentAttemptPayload {
        frequency_hz: 1500.0,
        remote_binding: Some((false, None)),
    };
    report_pivot_duplicate_dropped(
        &bus,
        Some("q1".into()),
        Some(("K1ABC N0CALL FN20".into(), replaced.clone(), None)),
    )
    .await;
    assert!(autonomous.try_recv().is_err());
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { attempt_id: _, ref qso_id, ref message_text, replaced: Some(ref payload) })
            if qso_id == "q1" && message_text == "K1ABC N0CALL FN20" && *payload == replaced
    ));
    assert!(qso.try_recv().is_err());

    // No recorded pivot source: nothing reaches the QSO engine.
    report_pivot_duplicate_dropped(&bus, Some("q1".into()), None).await;
    assert!(autonomous.try_recv().is_err());
    assert!(qso.try_recv().is_err());
}

/// A refused bundle clears only its own pivots' tombstones and hands back
/// each replaced frame once, keyed by QSO (Codex P2 on PR #418).
#[test]
fn unwind_bundle_pivots_returns_each_replaced_frame_once() {
    let payload = pancetta_qso::UnsentAttemptPayload {
        frequency_hz: 1500.0,
        remote_binding: None,
    };
    let a = super::super::active_tx_qso_key("qa");
    let b = super::super::active_tx_qso_key("qb");
    let other = super::super::active_tx_qso_key("qz");
    let mut once = std::collections::HashMap::new();
    let mut from = PivotReplaced::new();
    for key in [&a, &b, &other] {
        once.insert(key.clone(), ("NEW".to_string(), 1500.0));
    }
    from.insert(a.clone(), ("A OLD".to_string(), payload.clone(), None));
    from.insert(other.clone(), ("Z OLD".to_string(), payload.clone(), None));
    let mut replaced = unwind_bundle_pivots(&[a.clone(), b.clone()], &mut once, &mut from);
    assert_eq!(once.keys().collect::<Vec<_>>(), vec![&other]);
    assert_eq!(from.keys().collect::<Vec<_>>(), vec![&other]);
    assert_eq!(
        take_bundle_replaced(&mut replaced, Some("qa")).map(|(t, _, _)| t),
        Some("A OLD".to_string())
    );
    assert!(take_bundle_replaced(&mut replaced, Some("qa")).is_none());
    assert!(take_bundle_replaced(&mut replaced, Some("qb")).is_none());
    assert!(take_bundle_replaced(&mut replaced, None).is_none());
}

/// PAN-75 (Codex P1 on PR #419): items a re-enqueued superseding bundle
/// carries over unchanged still air, so only the others are abandoned.
#[test]
fn notviable_supersede_skips_items_the_new_bundle_carries_over() {
    let item = |q: &str, text: &str| crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1500.0,
        qso_id: Some(q.to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let in_flight = vec![item("qa", "K1A N0CALL -07"), item("qb", "K2B N0CALL R-10")];
    let reenqueued = MessageType::MultiTransmitRequest {
        items: vec![item("qa", "K1A N0CALL RR73"), item("qb", "K2B N0CALL R-10")],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    let local = (crate::message_bus::TxOrigin::Local, None);
    let abandoned = items_abandoned_by_supersede(&in_flight, local, Some(&reenqueued));
    assert_eq!(abandoned.len(), 1);
    assert!(std::ptr::eq(abandoned[0], &in_flight[0]));
    assert_eq!(
        items_abandoned_by_supersede(&in_flight, local, None).len(),
        2
    );
    // Sub-epsilon offset noise still counts as carried (Codex P2 on PR #419).
    let mut noisy = reenqueued.clone();
    if let MessageType::MultiTransmitRequest { items, .. } = &mut noisy {
        items[1].frequency_offset += 0.1;
    }
    assert_eq!(
        items_abandoned_by_supersede(&in_flight, local, Some(&noisy)).len(),
        1
    );
    // An authorization change carries nothing over (Codex P2 on PR #419).
    let remote = (crate::message_bus::TxOrigin::Remote, Some("client-a"));
    assert_eq!(
        items_abandoned_by_supersede(&in_flight, remote, Some(&reenqueued)).len(),
        2
    );
    // PAN-136 (Codex P2 on PR #456): nor does a per-item re-bind under an
    // unchanged fold — qb's same-text frame now bound to client-a.
    let mut rebound = reenqueued.clone();
    if let MessageType::MultiTransmitRequest { items, .. } = &mut rebound {
        items[1].origin = crate::message_bus::TxOrigin::Remote;
        items[1].remote_client_key_id = Some("client-a".to_string());
    }
    let abandoned = items_abandoned_by_supersede(&in_flight, local, Some(&rebound));
    assert_eq!(abandoned.len(), 2);
    assert!(item_rebound_by_supersede(&in_flight[1], Some(&rebound)));
    assert!(!item_rebound_by_supersede(&in_flight[0], Some(&rebound)));
    assert!(!item_rebound_by_supersede(&in_flight[1], Some(&reenqueued)));
}

/// An abandoned bundle item is rolled back by its own offset, so a newer
/// same-text attempt at another offset is not hit (Codex P1 on PR #419).
#[tokio::test]
async fn abandoned_bundle_item_reports_its_own_offset() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let item = crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "K1A N0CALL -07".to_string(),
        frequency_offset: 1250.0,
        qso_id: Some("qa".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    report_abandoned_bundle_item(
        &bus,
        &item,
        Some((true, Some("client-a".into()))),
        AutonomousOutcome::Report,
    )
    .await;
    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::TransmitComplete { success: false, .. }
    ));
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref message_text, replaced: Some(ref p), .. })
            if message_text == "K1A N0CALL -07"
                && p.frequency_hz == 1250.0
                && p.remote_binding == Some((true, Some("client-a".to_string())))
    ));
}

/// A pivoted frame refused before PTT: its producing request is still
/// queued, so the QSO engine rolls back the replaced frame instead
/// (Codex P2 on PR #418), and Autonomous hears nothing from this frame:
/// the queued request reports its own outcome (PAN-185). Without a pivot
/// it rolls back this frame and reports to Autonomous as usual.
#[tokio::test]
async fn unaired_pivoted_frame_rolls_back_the_replaced_frame() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let replaced = pancetta_qso::UnsentAttemptPayload {
        frequency_hz: 1500.0,
        remote_binding: Some((false, None)),
    };
    report_unaired_frame(
        &bus,
        "K1ABC N0CALL R-07".into(),
        Some("q1".into()),
        None,
        Some(("K1ABC N0CALL FN20".into(), replaced.clone(), None)),
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_err());
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { attempt_id: _, ref qso_id, ref message_text, replaced: Some(ref payload) })
            if qso_id == "q1" && message_text == "K1ABC N0CALL FN20" && *payload == replaced
    ));
    assert!(qso.try_recv().is_err());

    report_unaired_frame(
        &bus,
        "K1ABC N0CALL R-07".into(),
        Some("q1".into()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert!(matches!(
        qso.try_recv().unwrap().message_type,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref message_text, replaced: None, .. })
            if message_text == "K1ABC N0CALL R-07"
    ));
}
