use super::*;
use pancetta_qso::TxAttemptId;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

fn audit_tmp() -> crate::coordinator::audit_test_util::TempPath {
    crate::coordinator::audit_test_util::TempPath::new("pancetta-tx-pan183-test")
}

fn request(text: &str, qso: &str, hz: f64, id: u64) -> MessageType {
    MessageType::TransmitRequest {
        message_text: text.into(),
        frequency_offset: hz,
        qso_id: Some(qso.into()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
        attempt_id: Some(TxAttemptId(id)),
    }
}

fn item(
    text: &str,
    qso: &str,
    hz: f64,
    origin: crate::message_bus::TxOrigin,
    client: Option<&str>,
    id: u64,
) -> crate::message_bus::TransmitRequestItem {
    crate::message_bus::TransmitRequestItem {
        message_text: text.into(),
        frequency_offset: hz,
        qso_id: Some(qso.into()),
        origin,
        remote_client_key_id: client.map(str::to_string),
        attempt_id: Some(TxAttemptId(id)),
    }
}

/// Coalesce `head` with the queued `backlog`, every QSO live.
async fn coalesce(head: MessageType, backlog: Vec<MessageType>, live: &[&str]) -> MessageType {
    let (tx, rx) = crossbeam_channel::unbounded();
    for m in backlog {
        tx.send(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            m,
            Instant::now(),
        ))
        .unwrap();
    }
    let inbox = super::TxInbox::new(rx, Default::default());
    let bus = MessageBus::new(16).unwrap();
    let audit_tmp = audit_tmp();
    let live: HashSet<String> = live
        .iter()
        .map(|id| super::super::active_tx_qso_key(id))
        .collect();
    coalesce_backlog_into(
        head,
        &inbox,
        &bus,
        &std::sync::Arc::new(std::sync::RwLock::new(live)),
        &std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
        4,
        &std::sync::Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new())),
        &pancetta_agent::audit::AuditLog::new(audit_tmp.path()),
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
    )
    .await
}

fn not_sent(
    msg: ComponentMessage,
) -> (
    String,
    String,
    Option<pancetta_qso::UnsentAttemptPayload>,
    Option<TxAttemptId>,
) {
    match msg.message_type {
        MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
            qso_id,
            message_text,
            replaced,
            attempt_id,
        }) => (qso_id, message_text, replaced, attempt_id),
        other => panic!("expected TxFrameNotSent, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pan183_coalesce_preserves_attempt_id_through_newest_wins() {
    // Two frames for one QSO: the newer one is kept, with its own id.
    let result = coalesce(
        request("K1A N0CALL FN20", "qa", 1000.0, 1),
        vec![request("K1A N0CALL R-07", "qa", 1000.0, 2)],
        &["qa"],
    )
    .await;
    match result {
        MessageType::TransmitRequest {
            message_text,
            attempt_id,
            ..
        } => {
            assert_eq!(message_text, "K1A N0CALL R-07");
            assert_eq!(attempt_id, Some(TxAttemptId(2)));
        }
        other => panic!("expected a single TransmitRequest, got {other:?}"),
    }
    // A lone request passes through with its id.
    match coalesce(request("K1A N0CALL FN20", "qa", 1000.0, 3), vec![], &["qa"]).await {
        MessageType::TransmitRequest { attempt_id, .. } => {
            assert_eq!(attempt_id, Some(TxAttemptId(3)));
        }
        other => panic!("expected a single TransmitRequest, got {other:?}"),
    }
    // The pure coalescer keeps each retained entry's own id.
    let entry = |text: &str, qso: &str, hz: f64, id: u64| CoalesceEntry {
        message_text: text.into(),
        frequency_offset: hz,
        qso_id: Some(qso.into()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
        attempt_id: Some(TxAttemptId(id)),
    };
    let outcome = coalesce_transmit_requests(
        vec![
            entry("A1", "qa", 1000.0, 10),
            entry("B1", "qb", 1600.0, 11),
            entry("A2", "qa", 1000.0, 12),
        ],
        4,
        |_| true,
    );
    let mut kept: Vec<(String, Option<TxAttemptId>)> = outcome
        .retained
        .iter()
        .map(|e| (e.message_text.clone(), e.attempt_id))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kept,
        vec![
            ("A2".to_string(), Some(TxAttemptId(12))),
            ("B1".to_string(), Some(TxAttemptId(11))),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pan183_bundle_fold_preserves_per_item_attempt_ids() {
    let result = coalesce(
        request("K1A N0CALL FN20", "qa", 1000.0, 1),
        vec![request("K2B N0CALL FN20", "qb", 1600.0, 2)],
        &["qa", "qb"],
    )
    .await;
    let MessageType::MultiTransmitRequest { items, .. } = result else {
        panic!("expected a folded bundle, got {result:?}");
    };
    let mut ids: Vec<(Option<String>, Option<TxAttemptId>)> = items
        .iter()
        .map(|it| (it.qso_id.clone(), it.attempt_id))
        .collect();
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        ids,
        vec![
            (Some("qa".to_string()), Some(TxAttemptId(1))),
            (Some("qb".to_string()), Some(TxAttemptId(2))),
        ]
    );

    // A key-time bundle pivot adopts the intent's attempt with its
    // payload; an item with no fresher intent keeps its own.
    let mut latest = std::collections::HashMap::new();
    latest.insert(
        super::super::active_tx_qso_key("qa"),
        super::super::LatestTxIntent {
            message_text: "K1A N0CALL R-07".into(),
            frequency_offset: 1000.0,
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: Some(TxAttemptId(9)),
            request_queued: true,
        },
    );
    let (pivoted, pivots) = super::super::pivot_bundle_items(items, &latest);
    assert_eq!(pivots.len(), 1);
    for it in &pivoted {
        let expected = match it.qso_id.as_deref() {
            Some("qa") => TxAttemptId(9),
            _ => TxAttemptId(2),
        };
        assert_eq!(it.attempt_id, Some(expected));
    }
}

#[tokio::test]
async fn pan183_not_sent_report_carries_request_attempt_id() {
    let bus = MessageBus::new(16).unwrap();
    let (_, autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    report_transmit_complete(
        &bus,
        false,
        "K1A N0CALL FN20".into(),
        0,
        Some("qa".into()),
        Some(TxAttemptId(7)),
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert_eq!(
        not_sent(qso.try_recv().unwrap()),
        (
            "qa".to_string(),
            "K1A N0CALL FN20".to_string(),
            None,
            Some(TxAttemptId(7))
        )
    );
    // An abort before encode reports the dequeued request's own id.
    emit_failed_transmit_complete_for_request(&bus, &request("K1A N0CALL R-07", "qa", 1000.0, 8))
        .await;
    assert!(autonomous.try_recv().is_ok());
    assert_eq!(not_sent(qso.try_recv().unwrap()).3, Some(TxAttemptId(8)));
    // A frame refused before PTT with no pivot reports its own id too.
    report_unaired_frame(
        &bus,
        "K1A N0CALL RR73".into(),
        Some("qa".into()),
        Some(TxAttemptId(5)),
        None,
    )
    .await
    .unwrap();
    assert!(autonomous.try_recv().is_ok());
    assert_eq!(not_sent(qso.try_recv().unwrap()).3, Some(TxAttemptId(5)));
    assert!(qso.try_recv().is_err());
}

/// PR #419 thread 4244657622: a mixed bundle's abandoned local item is
/// reported with the folded Remote binding, which alone would miss its
/// ledger entry; its own id now decides the match.
#[tokio::test]
async fn pan183_abandoned_bundle_item_report_carries_item_id_and_own_binding_shape() {
    let bus = MessageBus::new(16).unwrap();
    let (_, _autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let local = item(
        "K1A N0CALL -07",
        "qa",
        1250.0,
        crate::message_bus::TxOrigin::Local,
        None,
        31,
    );
    report_abandoned_bundle_item(
        &bus,
        &local,
        Some((true, Some("client-a".into()))),
        AutonomousOutcome::Report,
    )
    .await;
    let (qso_id, text, replaced, attempt_id) = not_sent(qso.try_recv().unwrap());
    assert_eq!((qso_id.as_str(), text.as_str()), ("qa", "K1A N0CALL -07"));
    assert_eq!(
        replaced,
        Some(pancetta_qso::UnsentAttemptPayload {
            frequency_hz: 1250.0,
            remote_binding: Some((true, Some("client-a".into()))),
        })
    );
    assert_eq!(attempt_id, Some(TxAttemptId(31)));
}

/// PR #419 thread 4244657620: an identity-conflict supersede abandons the
/// in-flight bundle; each item reports its own attempt, and the new
/// request is re-enqueued with its own.
#[tokio::test(flavor = "current_thread")]
async fn pan183_identity_conflict_abandon_reports_item_ids() {
    let bus = MessageBus::new(16).unwrap();
    let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
    let (_tx_tx, tx_rx) = bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_a_tx, _autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_q_tx, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let mut encoder = super::Ft8Encoder::new();
    let remote = crate::message_bus::TxOrigin::Remote;
    let in_flight = [
        item(
            "KA1ABC K5ARH R-15",
            "qso-1",
            1000.0,
            remote,
            Some("client-a"),
            21,
        ),
        item(
            "OTHER W5AU R-08",
            "qso-other",
            1400.0,
            remote,
            Some("client-a"),
            22,
        ),
    ];
    let now = chrono::Utc::now();
    let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
        pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
        pancetta_core::slot::SLOT_NS,
    );
    let superseding = MessageType::TransmitRequest {
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: Some(cur_parity),
        origin: remote,
        remote_client_key_id: Some("client-b".to_string()),
        attempt_id: Some(TxAttemptId(23)),
    };
    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let mut guard = super::supersede_rekey_tests::keyed_guard(
        &bus,
        &Arc::new(AtomicBool::new(true)),
        &Arc::new(AtomicU64::new(0)),
        &Arc::new(AtomicU64::new(0)),
    )
    .await;
    super::supersede_multi_reenqueue(
        superseding,
        &super::SupersedeInFlight {
            items: &in_flight,
            origin: remote,
            remote_client_key_id: Some("client-a".to_string()),
            tx_parity: Some(cur_parity),
        },
        &super::SlotTimingContext::ft8_for_test(
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
        ),
        2,
        super::SupersedeIo {
            message_bus: &bus,
            inbox: &inbox,
            encoder: &mut encoder,
            ptt_guard: &mut guard,
        },
    )
    .await;
    match inbox
        .try_recv()
        .expect("the new request is re-enqueued")
        .message_type
    {
        MessageType::TransmitRequest { attempt_id, .. } => {
            assert_eq!(attempt_id, Some(TxAttemptId(23)));
        }
        other => panic!("expected the new request, got {other:?}"),
    }
    let mut reported: Vec<(String, Option<TxAttemptId>)> =
        std::iter::from_fn(|| qso.try_recv().ok())
            .map(not_sent)
            .map(|(qso_id, _, _, attempt_id)| (qso_id, attempt_id))
            .collect();
    reported.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        reported,
        vec![
            ("qso-1".to_string(), Some(TxAttemptId(21))),
            ("qso-other".to_string(), Some(TxAttemptId(22))),
        ],
        "each abandoned item reports its own attempt, never the same-text \\
             request keyed in its place"
    );
}

#[tokio::test]
async fn pan183_pivot_replaced_report_carries_replaced_frame_id() {
    let bus = MessageBus::new(16).unwrap();
    let (_, _autonomous) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let (_, qso) = bus.create_channel(ComponentId::Qso).await.unwrap();
    let payload = pancetta_qso::UnsentAttemptPayload {
        frequency_hz: 1500.0,
        remote_binding: Some((false, None)),
    };
    // Refused before PTT after a pivot: the replaced frame's id, not the
    // pivoted-to frame's.
    report_unaired_frame(
        &bus,
        "K1A N0CALL R-07".into(),
        Some("qa".into()),
        Some(TxAttemptId(2)),
        Some((
            "K1A N0CALL FN20".into(),
            payload.clone(),
            Some(TxAttemptId(1)),
        )),
    )
    .await
    .unwrap();
    assert_eq!(
        not_sent(qso.try_recv().unwrap()),
        (
            "qa".to_string(),
            "K1A N0CALL FN20".to_string(),
            Some(payload.clone()),
            Some(TxAttemptId(1))
        )
    );
    // Step 0-dup: the duplicate's own attempt is kept; the replaced one
    // is rolled back.
    report_pivot_duplicate_dropped(
        &bus,
        Some("qa".into()),
        Some((
            "K1A N0CALL FN20".into(),
            payload.clone(),
            Some(TxAttemptId(1)),
        )),
    )
    .await;
    assert_eq!(not_sent(qso.try_recv().unwrap()).3, Some(TxAttemptId(1)));
    // A bundle refused after its pivots hands back each replaced id.
    let key = super::super::active_tx_qso_key("qa");
    let mut once = std::collections::HashMap::new();
    once.insert(key.clone(), ("K1A N0CALL R-07".to_string(), 1500.0));
    let mut from = PivotReplaced::new();
    from.insert(
        key.clone(),
        ("K1A N0CALL FN20".to_string(), payload, Some(TxAttemptId(1))),
    );
    let mut replaced = unwind_bundle_pivots(&[key], &mut once, &mut from);
    assert_eq!(
        take_bundle_replaced(&mut replaced, Some("qa")).and_then(|(_, _, id)| id),
        Some(TxAttemptId(1))
    );
    assert!(qso.try_recv().is_err());
}
