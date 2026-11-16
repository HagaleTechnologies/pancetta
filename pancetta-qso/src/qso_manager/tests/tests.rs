use super::*;

#[test]
fn pan302_qso_late_cap_refreshes_before_slot_acceptance() {
    let mut manager = QsoManager::new(QsoManagerConfig::default());
    let live = Arc::new(std::sync::atomic::AtomicU64::new(8000));
    manager.set_tx_late_max_ms_source(live.clone());
    let timeout_clone = manager.clone();
    let now = chrono::DateTime::from_timestamp(2, 0).unwrap();
    let before = keep_call_tx_slot(
        now,
        15_000,
        timeout_clone
            .tx_late_max_ms
            .load(std::sync::atomic::Ordering::Acquire),
        Some(pancetta_core::slot::SlotParity::Even),
    );
    live.store(1000, std::sync::atomic::Ordering::Release);
    let after = keep_call_tx_slot(
        now,
        15_000,
        timeout_clone
            .tx_late_max_ms
            .load(std::sync::atomic::Ordering::Acquire),
        Some(pancetta_core::slot::SlotParity::Even),
    );
    assert_ne!(
        before, after,
        "the real timeout clone must defer beyond the reduced cap"
    );
    manager.set_tx_late_max_ms(8000);
    assert_eq!(
        timeout_clone
            .tx_late_max_ms
            .load(std::sync::atomic::Ordering::Acquire),
        8000
    );
}
use tokio_test;

#[test]
fn bound_to_other_remote_client_preserves_local_and_unbound_qsos() {
    assert!(bound_to_other_remote_client(true, Some("a"), "b"));
    assert!(!bound_to_other_remote_client(true, Some("b"), "b"));
    assert!(!bound_to_other_remote_client(true, None, "b"));
    assert!(!bound_to_other_remote_client(false, Some("a"), "b"));
}

async fn rollback_fixture() -> (QsoManager, QsoId, DateTime<Utc>, String) {
    let manager = QsoManager::new(test_config());
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(15))
        .await;
    let text = manager
        .get_qso(id)
        .await
        .unwrap()
        .messages
        .last()
        .unwrap()
        .raw_text
        .clone();
    (manager, id, opened, text)
}

#[tokio::test]
async fn rollback_reverses_a_rearm_attempt() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let before = manager.get_qso(id).await.unwrap();
    assert_eq!(before.metadata.call_count, 2);
    assert_eq!(before.metadata.stall_cycles, 1);
    assert_eq!(before.messages.len(), 2);
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 1 }
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.last_call_at, Some(opened));
    assert_eq!(after.metadata.stall_cycles, 0);
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.messages[0].timestamp, before.messages[0].timestamp);
}

#[tokio::test]
async fn rollback_keeps_rearm_cadence() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let mut events = manager.subscribe();
    manager
        .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
        .await;
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(20))
        .await;
    assert!(drain(&mut events).is_empty());
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    assert_eq!(
        drain(&mut events)
            .iter()
            .filter(|e| matches!(e, QsoEvent::MessageToSend { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn rollback_with_unmatched_text_is_noop() {
    let (manager, id, opened, _) = rollback_fixture().await;
    let before = format!("{:?}", manager.get_qso(id).await.unwrap());
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(id, "wrong", opened + Duration::seconds(20))
            .await,
        UnsentAttemptRollback::NoMatchingAttempt
    );
    assert_eq!(before, format!("{:?}", manager.get_qso(id).await.unwrap()));
}

#[tokio::test]
async fn rollback_on_terminal_qso_is_noop() {
    let (manager, id, opened, text) = rollback_fixture().await;
    manager.cancel_qso(id).await.unwrap();
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
            .await,
        UnsentAttemptRollback::QsoNotActive
    );
}

#[tokio::test]
async fn rollback_of_expired_attempt_is_noop() {
    let (manager, id, opened, text) = rollback_fixture().await;
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(
                id,
                &text,
                opened + Duration::seconds(75) + Duration::milliseconds(1)
            )
            .await,
        UnsentAttemptRollback::NoMatchingAttempt
    );
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 2);
}

#[tokio::test]
async fn rollback_matches_newest_same_text_attempt_and_relinks() {
    let (manager, id, opened, text) = rollback_fixture().await;
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(35))
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(
        after.metadata.last_call_at,
        Some(opened + Duration::seconds(15))
    );
    assert_eq!(after.messages.len(), 2);
    assert_eq!(
        after.messages.last().unwrap().timestamp,
        opened + Duration::seconds(15)
    );
    manager
        .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(35))
        .await;
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.call_count, 1);
    assert_eq!(after.metadata.last_call_at, Some(opened));
}

// PAN-183 reproduction, now the regression test: the ledger holds an older
// keep-call rearm attempt (bumped_stall, recorded_sent) and a NEWER
// operator resend of the same text (no stall bump, no Sent entry).
// Reporting the OLDER rearm frame as not sent, by its attempt id, rolls
// the rearm back and leaves the resend's accounting alone. Before the fix
// the newest-same-text match removed the resend's entry instead.
#[tokio::test]
async fn pan183_repro_rollback_of_older_rearm_removes_newer_same_text_resend() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let rearm_id = manager.keep_call_attempt_ids(id)[0].0;
    // The operator explicitly resends the same text while the rearm frame
    // is still waiting before PTT.
    manager.resend_last_tx(id).await.unwrap();
    let before = manager.get_qso(id).await.unwrap();
    assert_eq!(before.metadata.call_count, 3);
    assert_eq!(before.metadata.stall_cycles, 1);
    assert_eq!(before.messages.len(), 2); // resend adds no Sent entry
    assert_eq!(manager.keep_call_ledger_len(id), 2);
    let resend_id = manager.keep_call_attempt_ids(id)[1].0;
    assert_ne!(rearm_id, resend_id);
    // The TX worker reports the OLDER rearm frame as not sent.
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                id,
                &text,
                None,
                Some(rearm_id),
                opened + Duration::seconds(20)
            )
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let after = manager.get_qso(id).await.unwrap();
    // The resend's entry survives; the rearm's stall increment and Sent
    // timeline entry are gone.
    assert_eq!(manager.keep_call_attempt_ids(id), vec![(resend_id, false)]);
    assert_eq!(after.metadata.stall_cycles, 0);
    assert_eq!(after.messages.len(), 1);
    assert_eq!(after.messages[0].timestamp, opened);
    // The resend is still the latest call, so its stamp stays.
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
    // Its own later failure rewinds past the rolled-back rearm.
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                id,
                &text,
                None,
                Some(resend_id),
                opened + Duration::seconds(20)
            )
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 1 }
    );
    assert_eq!(
        manager.get_qso(id).await.unwrap().metadata.last_call_at,
        Some(opened)
    );
}

/// Rebinds `id` to a remote peer, so the next rearm records a same-text,
/// same-offset attempt with a different authorization.
async fn pan183_rebind_remote(manager: &QsoManager, id: QsoId) {
    let mut qsos = manager.qsos.write().await;
    let metadata = &mut qsos.get_mut(&id).unwrap().metadata;
    metadata.remote_origin = true;
    metadata.remote_client_key_id = Some("peer-b".into());
}

#[tokio::test]
async fn pan183_rollback_by_id_ignores_newer_same_text_same_offset_attempt() {
    let (manager, id, opened, text) = rollback_fixture().await;
    pan183_rebind_remote(&manager, id).await;
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    let ids = manager.keep_call_attempt_ids(id);
    assert_eq!(ids.len(), 2);
    let (a, b) = (ids[0].0, ids[1].0);
    let before = manager.get_qso(id).await.unwrap();
    // A supersede report without the frame's binding (PR #419 thread
    // 4244657620): content alone would pick the newer remote rearm.
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(id, &text, None, Some(a), opened + Duration::seconds(35))
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(manager.keep_call_attempt_ids(id), vec![(b, true)]);
    assert_eq!(after.metadata.stall_cycles, 1);
    assert_eq!(after.messages.len(), 2);
    assert_eq!(
        after.messages.last().unwrap().timestamp,
        opened + Duration::seconds(30)
    );
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
}

#[tokio::test]
async fn pan183_rollback_by_id_ignores_binding_mismatch() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let local = manager.keep_call_attempt_ids(id)[0].0;
    pan183_rebind_remote(&manager, id).await;
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    let remote = manager.keep_call_attempt_ids(id)[1].0;
    // A mixed bundle's report carries the folded Remote binding for every
    // item (PR #419 thread 4244657622); the local item's id still wins.
    let folded = UnsentAttemptPayload {
        frequency_hz: 1500.0,
        remote_binding: Some((true, Some("peer-b".into()))),
    };
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                id,
                &text,
                Some(&folded),
                Some(local),
                opened + Duration::seconds(35)
            )
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    assert_eq!(manager.keep_call_attempt_ids(id), vec![(remote, true)]);
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(
        after.messages.last().unwrap().timestamp,
        opened + Duration::seconds(30)
    );
}

#[tokio::test]
async fn pan183_rollback_with_unknown_id_is_noop() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let before = format!("{:?}", manager.get_qso(id).await.unwrap());
    let ledger = manager.keep_call_attempt_ids(id);
    assert_eq!(
        manager
            .roll_back_replaced_attempt_at(
                id,
                &text,
                None,
                Some(TxAttemptId(u64::MAX)),
                opened + Duration::seconds(20)
            )
            .await,
        UnsentAttemptRollback::NoMatchingAttempt
    );
    assert_eq!(before, format!("{:?}", manager.get_qso(id).await.unwrap()));
    assert_eq!(manager.keep_call_attempt_ids(id), ledger);
}

#[tokio::test]
async fn pan183_rollback_without_id_keeps_content_match() {
    let (manager, id, opened, text) = rollback_fixture().await;
    let rearm = manager.keep_call_attempt_ids(id)[0];
    manager.resend_last_tx(id).await.unwrap();
    // An id-less report keeps the newest-same-text match: the resend.
    assert_eq!(
        manager
            .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(manager.keep_call_attempt_ids(id), vec![rearm]);
    assert_eq!(after.metadata.stall_cycles, 1);
    assert_eq!(after.messages.len(), 2);
    assert_eq!(
        after.metadata.last_call_at,
        Some(opened + Duration::seconds(15))
    );
}

fn pan183_emitted_attempt_ids(events: &[QsoEvent]) -> Vec<Option<TxAttemptId>> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend { attempt_id, .. } => Some(*attempt_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn pan183_opening_message_to_send_carries_an_unrecorded_id() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let emitted = pan183_emitted_attempt_ids(&drain(&mut events));
    assert_eq!(emitted.len(), 1);
    assert!(emitted[0].is_some(), "every engine emission has an id");
    assert_eq!(
        manager.keep_call_ledger_len(id),
        0,
        "an opening records none"
    );
}

// PR review on PAN-183: a failed opening call whose text a newer resend
// repeats rolls back nothing; by text alone it took the resend's entry.
#[tokio::test]
async fn pan183_failed_opening_call_never_takes_a_newer_resend_entry() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let opening = pan183_emitted_attempt_ids(&drain(&mut events))[0];
    let text = manager.get_qso(id).await.unwrap().messages[0]
        .raw_text
        .clone();
    manager.resend_last_tx(id).await.unwrap();
    let resend = manager.keep_call_attempt_ids(id);
    assert_eq!(resend.len(), 1);
    let before = format!("{:?}", manager.get_qso(id).await.unwrap());
    assert_eq!(
        manager
            .roll_back_unsent_attempt(id, &text, None, opening)
            .await,
        UnsentAttemptRollback::NoMatchingAttempt
    );
    assert_eq!(before, format!("{:?}", manager.get_qso(id).await.unwrap()));
    assert_eq!(manager.keep_call_attempt_ids(id), resend);
}

#[tokio::test]
async fn pan183_rearm_message_to_send_carries_attempt_id_matching_ledger() {
    let manager = QsoManager::new(test_config());
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let mut events = manager.subscribe();
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(15))
        .await;
    let ledger = manager.keep_call_attempt_ids(id);
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        pan183_emitted_attempt_ids(&drain(&mut events)),
        vec![Some(ledger[0].0)]
    );
}

#[tokio::test]
async fn pan183_resend_message_to_send_carries_distinct_id() {
    let (manager, id, _, _) = rollback_fixture().await;
    let rearm = manager.keep_call_attempt_ids(id)[0].0;
    let mut events = manager.subscribe();
    manager.resend_last_tx(id).await.unwrap();
    let ledger = manager.keep_call_attempt_ids(id);
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger[0].0, rearm);
    let resend = ledger[1].0;
    assert_ne!(resend, rearm);
    assert_eq!(
        pan183_emitted_attempt_ids(&drain(&mut events)),
        vec![Some(resend)]
    );
}

#[tokio::test]
async fn pan183_offset_switch_retransmit_carries_id() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let id = manager
        .respond_to_cq("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .call_count = AUTO_RESEND_MAX_CALLS;
    let mut events = manager.subscribe();
    manager
        .apply_tx_offset_switch(id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    let ledger = manager.keep_call_attempt_ids(id);
    let recorded = ledger.last().unwrap().0;
    assert_eq!(
        pan183_emitted_attempt_ids(&drain(&mut events)),
        vec![Some(recorded)]
    );
}

// PAN-183 gherkin, end to end at the engine's public rollback surface:
// a manual QSO's keep-call rearm frame waits before PTT, the operator
// explicitly resends the same text, and the TX worker reports the OLDER
// rearm frame as not sent with the id its request carried.
#[tokio::test]
async fn pan183_gherkin_rollback_of_older_rearm_with_newer_same_text_resend_end_to_end() {
    let manager = QsoManager::new(test_config());
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let mut events = manager.subscribe();
    // Given a manual QSO whose keep-call rearm frame is waiting before PTT
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(15))
        .await;
    // And the operator explicitly resends the same message text
    manager.resend_last_tx(id).await.unwrap();
    let emitted: Vec<(String, Option<TxAttemptId>)> = drain(&mut events)
        .into_iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend {
                message,
                attempt_id,
                ..
            } => Some((manager.render_sent_text(&message), attempt_id)),
            _ => None,
        })
        .collect();
    assert_eq!(emitted.len(), 2);
    let (rearm_text, rearm_id) = emitted[0].clone();
    let (resend_text, resend_id) = emitted[1].clone();
    assert_eq!(
        rearm_text, resend_text,
        "the resend repeats the rearm's text"
    );
    let before = manager.get_qso(id).await.unwrap();
    // When the TX worker reports the older rearm frame as not sent
    assert_eq!(
        manager
            .roll_back_unsent_attempt(id, &rearm_text, None, rearm_id)
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    // Then the rearm's call, stall increment and Sent entry are rolled back
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.call_count, before.metadata.call_count - 1);
    assert_eq!(
        after.metadata.stall_cycles,
        before.metadata.stall_cycles - 1
    );
    assert_eq!(after.messages.len(), before.messages.len() - 1);
    assert!(after
        .messages
        .iter()
        .all(|m| m.timestamp != opened + Duration::seconds(15)));
    // And the operator resend's accounting is left untouched
    assert_eq!(
        manager.keep_call_attempt_ids(id),
        vec![(resend_id.unwrap(), false)]
    );
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
}

#[tokio::test]
async fn rollback_after_advance_preserves_new_stall_and_stamp() {
    let (manager, id, opened, old_text) = rollback_fixture().await;
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".into(),
                from_station: "K1DEF".into(),
                report: -7,
            },
            "W1ABC K1DEF -07".into(),
            1500.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let advance_stamp = manager.get_qso(id).await.unwrap().metadata.last_call_at;
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    let new_attempt = manager.get_qso(id).await.unwrap();
    let new_text = &new_attempt.messages.last().unwrap().raw_text;
    assert_ne!(new_text, &old_text);
    assert_eq!(new_attempt.metadata.stall_cycles, 1);

    manager
        .roll_back_unsent_attempt_at(id, &old_text, opened + Duration::seconds(35))
        .await;
    let after_old = manager.get_qso(id).await.unwrap();
    assert_eq!(
        after_old.metadata.stall_cycles, 1,
        "old attempt's stall increment was already cleared by the DX advance"
    );
    assert_eq!(
        after_old.metadata.last_call_at,
        new_attempt.metadata.last_call_at
    );
    assert_eq!(
        after_old.metadata.call_count,
        new_attempt.metadata.call_count - 1
    );

    manager
        .roll_back_unsent_attempt_at(id, new_text, opened + Duration::seconds(36))
        .await;
    let after_new = manager.get_qso(id).await.unwrap();
    assert_eq!(after_new.metadata.stall_cycles, 0);
    assert_eq!(
        after_new.metadata.last_call_at, advance_stamp,
        "relink must preserve the intervening DX advance"
    );
}

#[tokio::test]
async fn rollback_after_forward_advance_keeps_the_advance_stamp() {
    let (manager, id, opened, text) = rollback_fixture().await;
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".into(),
                from_station: "K1DEF".into(),
                report: -7,
            },
            "W1ABC K1DEF -07".into(),
            1500.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let before = manager.get_qso(id).await.unwrap();
    assert!(matches!(before.state, QsoState::SendingReport { .. }));
    manager
        .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
        .await;
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.call_count, before.metadata.call_count - 1);
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
    assert_eq!(after.metadata.stall_cycles, 0);
    assert!(!after
        .messages
        .iter()
        .any(|m| m.direction == MessageDirection::Sent
            && m.timestamp == opened + Duration::seconds(15)));
}

#[tokio::test]
async fn rollback_reverses_resend_last_tx() {
    let manager = QsoManager::new(test_config());
    let id = manager
        .respond_to_cq_manual("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let before = manager.get_qso(id).await.unwrap();
    manager.resend_last_tx(id).await.unwrap();
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 2);
    assert_eq!(
        manager
            .roll_back_unsent_attempt(id, &before.messages.last().unwrap().raw_text, None, None)
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 1 }
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
    assert_eq!(after.messages.len(), before.messages.len());
}

#[tokio::test]
async fn rollback_reverses_offset_switch_retransmit() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let id = manager
        .respond_to_cq("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .call_count = AUTO_RESEND_MAX_CALLS;
    let before = manager.get_qso(id).await.unwrap();
    manager
        .apply_tx_offset_switch(id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    let emitted = manager.get_qso(id).await.unwrap();
    assert_eq!(emitted.metadata.call_count, AUTO_RESEND_MAX_CALLS + 1);
    manager
        .roll_back_unsent_attempt(id, &emitted.messages.last().unwrap().raw_text, None, None)
        .await;
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.metadata.call_count, before.metadata.call_count);
    assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
    assert_eq!(after.messages.len(), before.messages.len());
}

#[tokio::test]
async fn ledger_is_shared_across_clones_and_pruned_when_qso_leaves() {
    let (manager, id, opened, text) = rollback_fixture().await;
    assert_eq!(manager.keep_call_ledger_len(id), 1);
    assert_eq!(
        manager
            .clone()
            .roll_back_unsent_attempt_at(id, &text, opened + Duration::seconds(20))
            .await,
        UnsentAttemptRollback::RolledBack { call_count: 1 }
    );
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    assert_eq!(manager.keep_call_ledger_len(id), 1);
    manager.cancel_qso(id).await.unwrap();
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(45))
        .await;
    assert_eq!(manager.keep_call_ledger_len(id), 0);
}

#[tokio::test]
async fn auto_qso_rollback_restores_its_resend() {
    let manager = QsoManager::new(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let id = manager
        .respond_to_cq("K1DEF".into(), 1500.0, None)
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(15))
        .await;
    let emitted = manager.get_qso(id).await.unwrap();
    assert_eq!(emitted.metadata.call_count, 2);
    manager
        .roll_back_unsent_attempt_at(
            id,
            &emitted.messages.last().unwrap().raw_text,
            opened + Duration::seconds(20),
        )
        .await;
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 1);
    let mut events = manager.subscribe();
    manager
        .rearm_manual_calls_at(opened + Duration::seconds(30))
        .await;
    assert_eq!(
        drain(&mut events)
            .iter()
            .filter(|e| matches!(e, QsoEvent::MessageToSend { .. }))
            .count(),
        1
    );
}

fn test_config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: default_active_mode(),
    }
}

/// Drain currently-buffered events into a Vec.
fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// A manager with TX-frequency mode explicitly Auto.
///
/// PAN-72 Fix C (Codex round 10): `apply_tx_offset_switch` now
/// re-validates `tx_freq_mode` inside its own commit (see
/// `OffsetActionHeld`), and `QsoManager::new`'s never-injected default is
/// `Hold` (see the `tx_freq_mode` field's doc comment) — the same
/// convention every other `set_*_source` default follows. Direct
/// `apply_tx_offset_switch` callers in this module that are not
/// exercising Hold itself need Auto explicitly, exactly as production
/// callers get it from the coordinator's injected atomic.
fn auto_manager(config: QsoManagerConfig) -> QsoManager {
    let mut m = QsoManager::new(config);
    m.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    m
}

#[test]
fn offset_action_switch_and_revert_are_distinct() {
    let switch = OffsetAction::Switch { avoid_hz: 1500.0 };
    let revert = OffsetAction::Revert { target_hz: 1200.0 };
    assert_ne!(switch, revert);
    assert_eq!(switch, OffsetAction::Switch { avoid_hz: 1500.0 });
}

#[test]
fn admit_idle_admits_any_desire() {
    use pancetta_core::slot::SlotParity::*;
    assert_eq!(admit_new_qso(None, Some(Even)), TxAdmission::Admit);
    assert_eq!(admit_new_qso(None, Some(Odd)), TxAdmission::Admit);
    assert_eq!(admit_new_qso(None, None), TxAdmission::Admit);
}

#[test]
fn admit_same_side_admits_cross_side_queues() {
    use pancetta_core::slot::SlotParity::*;
    // Same side → concurrent (same window) → admit.
    assert_eq!(admit_new_qso(Some(Even), Some(Even)), TxAdmission::Admit);
    assert_eq!(admit_new_qso(Some(Odd), Some(Odd)), TxAdmission::Admit);
    // Cross side → would TX in the opposite window → queue.
    assert_eq!(admit_new_qso(Some(Even), Some(Odd)), TxAdmission::Queue);
    assert_eq!(admit_new_qso(Some(Odd), Some(Even)), TxAdmission::Queue);
}

#[test]
fn admit_unpinned_desire_rides_live_side() {
    use pancetta_core::slot::SlotParity::*;
    // A request that doesn't pin a parity (e.g. CQ, scheduler picks) never
    // conflicts — it rides whatever side is already live.
    assert_eq!(admit_new_qso(Some(Even), None), TxAdmission::Admit);
    assert_eq!(admit_new_qso(Some(Odd), None), TxAdmission::Admit);
}

// ── PAN-78: capacity primitives ─────────────────────────────────────

#[test]
fn capacity_admits_below_cap() {
    assert_eq!(admit_within_capacity(0, 1), CapacityAdmission::Admit);
    assert_eq!(admit_within_capacity(1, 2), CapacityAdmission::Admit);
}

#[test]
fn capacity_refuses_at_and_above_cap() {
    assert_eq!(
        admit_within_capacity(1, 1),
        CapacityAdmission::AtCapacity { active: 1, cap: 1 }
    );
    assert_eq!(
        admit_within_capacity(3, 2),
        CapacityAdmission::AtCapacity { active: 3, cap: 2 }
    );
}

#[test]
fn capacity_zero_cap_refuses_everything() {
    // Mirrors maybe_answer_caller's `active >= cap` with a Fox
    // fox_max_streams of 0.
    assert_eq!(
        admit_within_capacity(0, 0),
        CapacityAdmission::AtCapacity { active: 0, cap: 0 }
    );
}

const PAN78_DX: &str = "K9XYZ";
const PAN78_OTHER: &str = "K8OTH";
const PAN78_FREQ: f64 = 1500.0;

/// Open a manual QSO the way `StartQso` does (Grid, via
/// `respond_to_cq_with(..Manual..)`).
async fn pan78_open_manual(manager: &QsoManager, call: &str) -> QsoId {
    manager
        .respond_to_cq_with(
            call.to_string(),
            PAN78_FREQ,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap()
}

/// Open a QSO the way `RespondToCaller` does, at `step`.
async fn pan78_reply(manager: &QsoManager, call: &str, step: pancetta_core::ResponseStep) -> QsoId {
    manager
        .respond_to_caller(
            call.to_string(),
            PAN78_FREQ,
            Some(pancetta_core::slot::SlotParity::Even),
            step,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn manual_open_adds_for_a_fresh_callsign() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    assert!(
        manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Grid)
            .await
    );
    assert!(
        manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Report)
            .await
    );
}

#[tokio::test]
async fn manual_open_does_not_add_for_active_manual_recall() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    pan78_open_manual(&manager, PAN78_DX).await;
    assert!(
        !manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Grid)
            .await,
        "re-calling an active manual QSO is FIX 1's idempotent keep-call"
    );
    // Case-insensitive, like the `qsos_by_callsign` keying.
    assert!(
        !manager
            .manual_open_adds_active_qso("k9xyz", PAN78_FREQ, ResponseStep::Report)
            .await
    );
}

#[tokio::test]
async fn manual_open_does_not_add_for_active_auto_supersede() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    manager
        .respond_to_cq(
            PAN78_DX.to_string(),
            PAN78_FREQ,
            Some(pancetta_core::slot::SlotParity::Even),
        )
        .await
        .unwrap();
    assert!(
        !manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Grid)
            .await,
        "a manual open over an active auto QSO is FIX 3's one-for-one supersede"
    );
}

#[tokio::test]
async fn manual_open_seventy_three_never_adds() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    assert!(
        !manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::SeventyThree)
            .await,
        "SeventyThree opens directly at Completed (not active)"
    );
}

#[tokio::test]
async fn manual_open_rr73_rekeys_recently_completed() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    // SeventyThree opens a MANUAL QSO directly at Completed.
    pan78_reply(&manager, PAN78_DX, ResponseStep::SeventyThree).await;
    assert_eq!(manager.active_qso_count().await, 0);
    assert!(
        !manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Rr73)
            .await,
        "Rr73 within the rework grace is FIX B's re-key of the completed QSO"
    );
    assert!(
        manager
            .manual_open_adds_active_qso(PAN78_DX, PAN78_FREQ, ResponseStep::Report)
            .await,
        "Report (ladder rank < 2) is the deliberate rework case: a new QSO"
    );
}

#[tokio::test]
async fn manual_open_other_callsign_still_adds() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    pan78_open_manual(&manager, PAN78_DX).await;
    assert!(
        manager
            .manual_open_adds_active_qso(PAN78_OTHER, PAN78_FREQ, ResponseStep::Grid)
            .await
    );
    assert!(
        manager
            .manual_open_adds_active_qso(PAN78_OTHER, PAN78_FREQ, ResponseStep::Rr73)
            .await
    );
}

/// Mirror test: pins `manual_open_adds_active_qso` to the REAL open
/// functions' effect on `active_qso_count()`, so a future change to the
/// reuse/supersede branches of `respond_to_cq_with` / `respond_to_caller`
/// (or to the predicate) fails here.
#[tokio::test]
async fn manual_open_predicate_matches_real_open_effect() {
    use pancetta_core::ResponseStep;

    #[derive(Debug, Clone, Copy)]
    enum Setup {
        Empty,
        ActiveManual,
        ActiveAuto,
        RecentlyCompletedManual,
    }

    let cases: &[(Setup, &str, ResponseStep)] = &[
        (Setup::Empty, PAN78_DX, ResponseStep::Grid),
        (Setup::Empty, PAN78_DX, ResponseStep::Report),
        (Setup::Empty, PAN78_DX, ResponseStep::ReportAck),
        (Setup::Empty, PAN78_DX, ResponseStep::Rr73),
        (Setup::Empty, PAN78_DX, ResponseStep::SeventyThree),
        (Setup::ActiveManual, PAN78_DX, ResponseStep::Grid),
        (Setup::ActiveManual, PAN78_DX, ResponseStep::Report),
        (Setup::ActiveManual, PAN78_DX, ResponseStep::Rr73),
        (Setup::ActiveManual, PAN78_OTHER, ResponseStep::Grid),
        (Setup::ActiveManual, PAN78_OTHER, ResponseStep::Report),
        (Setup::ActiveAuto, PAN78_DX, ResponseStep::Grid),
        (Setup::ActiveAuto, PAN78_DX, ResponseStep::Report),
        (Setup::ActiveAuto, PAN78_DX, ResponseStep::Rr73),
        (Setup::RecentlyCompletedManual, PAN78_DX, ResponseStep::Grid),
        (
            Setup::RecentlyCompletedManual,
            PAN78_DX,
            ResponseStep::Report,
        ),
        (Setup::RecentlyCompletedManual, PAN78_DX, ResponseStep::Rr73),
        (
            Setup::RecentlyCompletedManual,
            PAN78_DX,
            ResponseStep::SeventyThree,
        ),
    ];

    for &(setup, call, step) in cases {
        let manager = QsoManager::new(test_config());
        manager
            .dial_frequency_hz
            .store(14_074_000, Ordering::Relaxed);
        match setup {
            Setup::Empty => {}
            Setup::ActiveManual => {
                pan78_open_manual(&manager, PAN78_DX).await;
            }
            Setup::ActiveAuto => {
                manager
                    .respond_to_cq(
                        PAN78_DX.to_string(),
                        PAN78_FREQ,
                        Some(pancetta_core::slot::SlotParity::Even),
                    )
                    .await
                    .unwrap();
            }
            Setup::RecentlyCompletedManual => {
                pan78_reply(&manager, PAN78_DX, ResponseStep::SeventyThree).await;
            }
        }

        let before = manager.active_qso_count().await;
        let adds = manager
            .manual_open_adds_active_qso(call, PAN78_FREQ, step)
            .await;
        if step == ResponseStep::Grid {
            pan78_open_manual(&manager, call).await;
        } else {
            pan78_reply(&manager, call, step).await;
        }
        let after = manager.active_qso_count().await;
        assert_eq!(
            after,
            before + usize::from(adds),
            "predicate disagrees with the real open for {setup:?} / {call} / {step:?} \
             (before={before}, adds={adds}, after={after})"
        );
    }
}

/// PAN-78: `engage_hound_adds_active_qso` must match what `engage_hound`
/// really does — a first engage adds a QSO, a repeat engage of the same
/// Fox continues it (same id, no new active QSO), another Fox adds one.
#[tokio::test]
async fn engage_hound_predicate_matches_real_engage_effect() {
    use pancetta_core::slot::SlotParity;
    let manager = QsoManager::new(test_config());
    let engage = |call: &'static str| {
        let manager = &manager;
        async move {
            manager
                .engage_hound(call, 600.0, None, Some(SlotParity::Even))
                .await
                .unwrap()
        }
    };

    assert!(manager.engage_hound_adds_active_qso("K5FOX").await);
    let first = engage("K5FOX").await;
    assert_eq!(manager.active_qso_count().await, 1);

    assert!(!manager.engage_hound_adds_active_qso("K5FOX").await);
    let again = engage("K5FOX").await;
    assert_eq!(again, first, "a repeat engage continues the Hound QSO");
    assert_eq!(manager.active_qso_count().await, 1);

    assert!(manager.engage_hound_adds_active_qso("K6FOX").await);
    engage("K6FOX").await;
    assert_eq!(manager.active_qso_count().await, 2);
}

#[tokio::test]
async fn current_tx_side_none_when_idle_then_pins_after_admit() {
    use pancetta_core::slot::SlotParity;
    let manager = QsoManager::new(test_config());
    assert_eq!(manager.current_tx_side().await, None);

    // A responder latches tx_parity = opposite(dx_parity). DX on Even → we
    // TX Odd, so the side becomes Odd.
    manager
        .respond_to_cq("K9XYZ".to_string(), 14074000.0, Some(SlotParity::Even))
        .await
        .unwrap();
    assert_eq!(manager.current_tx_side().await, Some(SlotParity::Odd));
}

#[tokio::test]
async fn test_start_cq() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::CallingCq { .. }));
    assert_eq!(progress.metadata.frequency, 14074000.0);
}

/// docs/archive/qso-engine-bugs.md BUG 1, autonomous path: `SlotParityConfig::Auto`
/// resolves to `tx_parity: None` for a self-CQ opening (see
/// `classify_autonomous_opening`), so the AUTONOMOUS CQ path is exposed to
/// the identical bug as the manual path — `start_cq` must latch a concrete
/// parity too.
#[tokio::test]
async fn autonomous_cq_with_no_parity_preference_latches_a_concrete_parity() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();
    assert!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .tx_parity
            .is_some(),
        "start_cq(tx_parity: None) must latch a concrete parity, not leave it None"
    );
}

// ── Batch 2: responder-side provisional parity latch + first-decode
//    refinement ──────────────────────────────────────────────────────────

/// `respond_to_cq_with` answering a spot with NO observed dx_parity (e.g.
/// a DX-cluster/DX-Hunter spot never actually decoded live) must latch a
/// CONCRETE parity immediately, marked provisional — not leave
/// `tx_parity` permanently `None` (which used to make the TX scheduler
/// re-resolve "nearest next slot" independently every subsequent slot and
/// alternate parity).
#[tokio::test]
async fn respond_to_cq_with_no_dx_parity_latches_provisional() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            "K1DEF".to_string(),
            14074000.0,
            None,
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.tx_parity.is_some(),
        "must latch a concrete parity, not leave None"
    );
    assert!(
        progress.metadata.tx_parity_provisional,
        "a parity latched with no observed dx_parity must be marked provisional"
    );
}

/// `respond_to_caller` answering with NO observed dx_parity (non-Grid
/// step) latches the same way: concrete + provisional.
#[tokio::test]
async fn respond_to_caller_with_no_dx_parity_latches_provisional() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_caller(
            "K1DEF".to_string(),
            14074000.0,
            None,
            pancetta_core::ResponseStep::Report,
            Some(-8.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.tx_parity.is_some(),
        "must latch a concrete parity, not leave None"
    );
    assert!(
        progress.metadata.tx_parity_provisional,
        "a parity latched with no observed dx_parity must be marked provisional"
    );
}

/// Regression: `respond_to_cq_with` with a REAL observed `dx_parity` sets
/// `tx_parity_provisional: false` — unchanged behavior for the common
/// case (a genuine live decode drove the answer).
#[tokio::test]
async fn respond_to_cq_with_real_dx_parity_is_not_provisional() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            "K1DEF".to_string(),
            14074000.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.tx_parity,
        Some(pancetta_core::slot::SlotParity::Odd)
    );
    assert!(
        !progress.metadata.tx_parity_provisional,
        "a parity latched from a REAL observed dx_parity must not be provisional"
    );
}

/// Regression: `respond_to_caller` with a REAL observed `dx_parity` also
/// sets `tx_parity_provisional: false`.
#[tokio::test]
async fn respond_to_caller_with_real_dx_parity_is_not_provisional() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_caller(
            "K1DEF".to_string(),
            14074000.0,
            Some(pancetta_core::slot::SlotParity::Even),
            pancetta_core::ResponseStep::Report,
            Some(-8.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.tx_parity,
        Some(pancetta_core::slot::SlotParity::Odd)
    );
    assert!(
        !progress.metadata.tx_parity_provisional,
        "a parity latched from a REAL observed dx_parity must not be provisional"
    );
}

/// First-decode refinement: a QSO answered with no observed dx_parity
/// (provisional latch) gets its `tx_parity` corrected to the TRUE
/// opposite-of-DX parity the first time a genuine frame FROM the latched
/// partner arrives carrying a determinable slot parity — and the
/// refinement is one-shot (a second frame with a DIFFERENT parity does
/// NOT change it again).
#[tokio::test]
async fn first_decode_from_partner_refines_provisional_parity_once() {
    use pancetta_core::slot::SlotParity;

    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            "K1DEF".to_string(),
            14074000.0,
            None, // no observed dx_parity — provisional latch
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .tx_parity_provisional
    );

    // First real frame FROM the latched partner (K1DEF), observed on an
    // Even slot. This must refine tx_parity to Odd (opposite) and clear
    // the provisional flag.
    manager
        .process_message_with_parity(
            MessageType::SignalReport {
                from_station: "K1DEF".to_string(),
                to_station: "W1ABC".to_string(),
                report: -10,
            },
            "W1ABC K1DEF -10".to_string(),
            14074000.0,
            Some(-10.0),
            Some(SlotParity::Even),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.tx_parity,
        Some(SlotParity::Odd),
        "tx_parity must refine to the opposite of the observed DX parity"
    );
    assert!(
        !progress.metadata.tx_parity_provisional,
        "the provisional flag must clear after the first refinement"
    );

    // Second frame from the partner, this time observed on Odd (would
    // imply Even if re-refined) — must NOT change the already-refined
    // parity (one-shot).
    manager
        .process_message_with_parity(
            MessageType::ReportAck {
                from_station: "K1DEF".to_string(),
                to_station: "W1ABC".to_string(),
                report: -10,
            },
            "W1ABC K1DEF R-10".to_string(),
            14074000.0,
            Some(-10.0),
            Some(SlotParity::Odd),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.tx_parity,
        Some(SlotParity::Odd),
        "a second frame must NOT re-refine an already-refined parity"
    );
}

/// Security: a frame from a NON-partner (sender mismatch) must NOT
/// trigger the refinement, even though it carries a determinable slot
/// parity — sender verification is never weakened for this purpose.
#[tokio::test]
async fn refinement_does_not_fire_for_non_partner_sender() {
    use pancetta_core::slot::SlotParity;

    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_with(
            "K1DEF".to_string(),
            14074000.0,
            None, // provisional latch
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();

    // A frame from a DIFFERENT station, addressed to someone else
    // entirely, must not be routed to this QSO at all (find_qsos_for_message
    // relevance gate) and must not refine its provisional parity.
    manager
        .process_message_with_parity(
            MessageType::SignalReport {
                from_station: "NF4KE".to_string(),
                to_station: "W1ABC".to_string(),
                report: -12,
            },
            "W1ABC NF4KE -12".to_string(),
            14074000.0,
            Some(-12.0),
            Some(SlotParity::Even),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.tx_parity_provisional,
        "a non-partner frame must not refine the provisional latch"
    );
}

#[tokio::test]
async fn test_respond_to_cq() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
    assert_eq!(progress.metadata.their_callsign, Some("K1DEF".to_string()));
}

// --- active_mode: QsoMetadata.mode follows the station-wide mode -------

/// Regression: default `QsoManagerConfig` stamps `mode == "FT8"` on every
/// QSO metadata it creates — byte-identical to pre-FT4 behavior.
#[tokio::test]
async fn default_config_stamps_mode_ft8() {
    assert_eq!(QsoManagerConfig::default().active_mode, "FT8");
    let manager = QsoManager::new(test_config());
    // CallingCq metadata (start_cq path).
    let cq_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();
    assert_eq!(manager.get_qso(cq_id).await.unwrap().metadata.mode, "FT8");
    // RespondingToCq metadata (respond_to_cq path).
    let rx_id = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    assert_eq!(manager.get_qso(rx_id).await.unwrap().metadata.mode, "FT8");
}

/// FT4 mode: a manager built with `active_mode = "FT4"` stamps `mode ==
/// "FT4"` into the metadata of QSOs it creates (→ ADIF `MODE:FT4`).
#[tokio::test]
async fn active_mode_ft4_stamps_metadata() {
    let config = QsoManagerConfig {
        active_mode: "FT4".to_string(),
        ..test_config()
    };
    let manager = QsoManager::new(config);
    let cq_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();
    assert_eq!(manager.get_qso(cq_id).await.unwrap().metadata.mode, "FT4");
    let rx_id = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    assert_eq!(manager.get_qso(rx_id).await.unwrap().metadata.mode, "FT4");
}

/// `set_active_mode` updates the manager's configured mode, affecting
/// only QSOs opened after the call.
#[tokio::test]
async fn set_active_mode_affects_qsos_opened_afterward() {
    let mut manager = QsoManager::new(test_config()); // starts at "FT8"
    manager.set_active_mode("FT4".to_string());
    assert_eq!(manager.config().active_mode, "FT4");
}

/// An FT4 `QsoMetadata` (built by a manager in FT4 mode) renders ADIF
/// `MODE:FT4`; the default FT8 manager renders `MODE:FT8` — confirming
/// `mode` flows through `qso_to_adif` → `generate_record` unchanged.
#[tokio::test]
async fn adif_mode_follows_active_mode() {
    use crate::adif::AdifProcessor;
    let processor = AdifProcessor::new();
    for mode in ["FT8", "FT4"] {
        let config = QsoManagerConfig {
            active_mode: mode.to_string(),
            ..test_config()
        };
        let manager = QsoManager::new(config);
        let qso_id = manager
            .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
            .await
            .unwrap();
        let meta = manager.get_qso(qso_id).await.unwrap().metadata;
        let record = processor.qso_to_adif(&meta, None);
        let rendered = processor.generate_record(&record).unwrap();
        assert!(
            rendered.contains(&format!("<MODE:{}>{}", mode.len(), mode)),
            "expected MODE:{mode} in ADIF, got: {rendered}"
        );
    }
}

// --- Fix 1 (security): compound-callsign logged-callsign upgrade ------

/// (a) A bare base latched as the partner, later seen under a SINGLE
/// standard compound form (recognized country prefix), upgrades the logged
/// callsign — the legitimate C18 case this feature exists for.
#[tokio::test]
async fn compound_upgrade_bare_base_to_standard_compound() {
    let manager = QsoManager::new(test_config()); // our call = W1ABC
    let freq = 14074000.0;
    // We answer a CQ from bare base G8BCG → latched their_callsign=G8BCG.
    let qso_id = manager
        .respond_to_cq("G8BCG".to_string(), freq, None)
        .await
        .unwrap();
    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .their_callsign
            .as_deref(),
        Some("G8BCG")
    );

    // The DX now signs the standard compound EA8/G8BCG in a frame to us.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "EA8/G8BCG".to_string(),
                report: -12,
            },
            "W1ABC EA8/G8BCG -12".to_string(),
            freq,
            Some(-12.0),
        )
        .await
        .unwrap();

    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .their_callsign
            .as_deref(),
        Some("EA8/G8BCG"),
        "bare base should upgrade to the standard compound form"
    );
}

/// (b) An attacker who knows the partner's on-air base call wraps an
/// ARBITRARY (unrecognized) prefix token around it. Same base → matches,
/// strictly longer → but the bogus token must NOT be allowed to overwrite
/// the logged callsign.
#[tokio::test]
async fn compound_upgrade_rejects_bogus_affix_token() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager
        .respond_to_cq("G8BCG".to_string(), freq, None)
        .await
        .unwrap();

    // BOGUS9 is 6 chars → not a recognized prefix/suffix token.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "BOGUS9/G8BCG/MM".to_string(),
                report: -12,
            },
            "W1ABC BOGUS9/G8BCG/MM -12".to_string(),
            freq,
            Some(-12.0),
        )
        .await
        .unwrap();

    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .their_callsign
            .as_deref(),
        Some("G8BCG"),
        "attacker compound with a bogus token must not overwrite the logged callsign"
    );
}

/// (c) Already-compound latched call; a frame arrives with the SAME base
/// but a DIFFERENT recognized prefix (a substitution, not a completion).
/// It must not silently overwrite the latched call.
#[tokio::test]
async fn compound_upgrade_rejects_different_prefix_substitution() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    // Latch the compound EA8/G8BCG directly.
    let qso_id = manager
        .respond_to_cq("EA8/G8BCG".to_string(), freq, None)
        .await
        .unwrap();
    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .their_callsign
            .as_deref(),
        Some("EA8/G8BCG")
    );

    // A frame swaps the prefix to FR (same base, strictly longer string,
    // recognized token — but a substitution of the latched EA8).
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "FR/G8BCG/P".to_string(),
                report: -12,
            },
            "W1ABC FR/G8BCG/P -12".to_string(),
            freq,
            Some(-12.0),
        )
        .await
        .unwrap();

    assert_eq!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .their_callsign
            .as_deref(),
        Some("EA8/G8BCG"),
        "a different-prefix compound must not replace the latched compound"
    );
}

// --- Manual-vs-auto calling semantics (operator policy) --------------

/// An auto response to a callsign we already have an active QSO with is
/// rejected by the self-duplicate gate (unchanged behavior).
#[tokio::test]
async fn auto_recall_to_same_dx_is_rejected_as_duplicate() {
    let manager = QsoManager::new(test_config());
    let _first = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let second = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await;
    assert!(
        matches!(second, Err(QsoManagerError::DuplicateQso { .. })),
        "auto re-call should be a duplicate, got {:?}",
        second
    );
}

/// A MANUAL call bypasses the self-duplicate gate even when an active
/// QSO with that callsign already exists.
#[tokio::test]
async fn manual_call_bypasses_duplicate_gate() {
    let manager = QsoManager::new(test_config());
    let _first = manager
        .respond_to_cq("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let manual = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await;
    assert!(
        manual.is_ok(),
        "manual call must not be blocked by duplicate gate, got {:?}",
        manual
    );
    let progress = manager.get_qso(manual.unwrap()).await.unwrap();
    assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
    assert_eq!(progress.metadata.call_count, 1);
}

/// Two consecutive manual calls to the same DX are both allowed (the
/// operator hit the duplicate-QSO bug doing exactly this).
#[tokio::test]
async fn manual_recall_to_same_dx_is_allowed() {
    let manager = QsoManager::new(test_config());
    let a = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await;
    let b = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await;
    assert!(a.is_ok() && b.is_ok(), "both manual calls allowed");
}

/// FIX 1: re-calling a station with an ACTIVE manual QSO CONTINUES that QSO
/// — it returns the SAME qso_id, does NOT create a second QSO, and does NOT
/// supersede (the old QSO is NOT marked Failed{Superseded}). This is the
/// core of the "mashing Space spawns duplicate/superseding QSOs" fix.
#[tokio::test]
async fn manual_recall_of_active_qso_continues_same_qso() {
    let manager = QsoManager::new(test_config());
    let first = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let second = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "re-call of an active QSO must continue it (same id), not spawn a new one"
    );

    // Exactly one active QSO for this callsign — the original.
    let active = manager.get_active_qsos().await;
    let active_for_dx: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        active_for_dx,
        vec![first],
        "exactly one active QSO (the original) should remain"
    );

    // It is NOT superseded — still in its original RespondingToCq state.
    let progress = manager.get_qso(first).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "continued QSO must keep its state, got {:?}",
        progress.state
    );

    // The callsign mapping holds only the one QSO.
    let mapping = manager.qsos_by_callsign.read().await;
    assert_eq!(
        mapping.get("K1DEF").map(|v| v.as_slice()),
        Some([first].as_slice()),
        "mapping must point only to the single continued QSO"
    );
}

/// FIX 1 / FIX 3 boundary: a re-call AFTER the prior QSO already went
/// terminal still works — it creates a FRESH QSO (and supersedes any
/// lingering terminal record). This is the genuine "work them again" case.
#[tokio::test]
async fn manual_recall_after_terminal_creates_fresh_qso() {
    let manager = QsoManager::new(test_config());
    let first = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    // Drive the first QSO terminal (operator cancel).
    manager.cancel_qso(first).await.unwrap();

    let second = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    assert_ne!(
        first, second,
        "re-call after the prior QSO went terminal must create a fresh QSO"
    );

    let active = manager.get_active_qsos().await;
    let active_for_dx: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(active_for_dx, vec![second], "only the fresh QSO is active");
}

// --- Manual re-call dedup regression locks (K9HJZ duplicate-QSO incident)
//
// Audit (2026-06-17): the manual re-call entry (`respond_to_cq_with`,
// Manual) is dedup-safe by construction. (1) FIX 1
// (`find_active_manual_qso_for`) continues an ACTIVE manual QSO instead of
// spawning a second one. (2) Every terminal transition clears the
// `qsos_by_callsign` mapping for that QSO — `cancel_qso` and
// `check_timeouts_at` `qsos.remove` it outright, `supersede_active_qsos_for`
// marks it `Failed{Superseded}` and removes its mapping — and
// `find_active_manual_qso_for` / `supersede_active_qsos_for` both filter on
// `state.is_active()`, so a lingering `Failed` record can neither be
// "continued" nor block a fresh call. The tests below pin both guarantees
// against regression. No production change was required.

/// REGRESSION LOCK (scenario a): a manual re-call to a callsign that already
/// has an ACTIVE QSO must leave EXACTLY ONE active QSO for that callsign —
/// never two concurrent ones. Here the prior active QSO is an AUTO QSO (FIX
/// 1's "continue" only matches a prior *manual* QSO), so the manual re-call
/// takes the supersede path: the older QSO is retired to
/// `Failed{Superseded}` and a single fresh manual QSO remains active.
#[tokio::test]
async fn manual_recall_supersedes_active_qso_leaving_exactly_one() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;

    // Prior ACTIVE (auto) QSO with the DX.
    let prior = manager
        .respond_to_cq("K9HJZ".to_string(), freq, None)
        .await
        .unwrap();
    assert!(manager.get_qso(prior).await.unwrap().state.is_active());

    // Operator manually re-calls the same DX on the same band.
    let recall = manager
        .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
        .await
        .unwrap();
    assert_ne!(
        prior, recall,
        "a non-manual prior QSO is superseded, not continued — fresh id"
    );

    // The prior QSO is retired (superseded), not still active.
    let prior_state = manager.get_qso(prior).await.unwrap().state;
    assert!(
        matches!(
            prior_state,
            QsoState::Failed {
                reason: QsoFailureReason::Superseded,
                ..
            }
        ),
        "prior active QSO must be superseded → Failed, got {:?}",
        prior_state
    );

    // EXACTLY ONE active QSO for the callsign — the new manual one.
    let active = manager.get_active_qsos().await;
    let active_for_dx: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K9HJZ"))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        active_for_dx,
        vec![recall],
        "exactly one active QSO (the fresh manual re-call) must remain"
    );

    // The callsign mapping points only to the surviving QSO — the
    // superseded one was unmapped, so no stale entry lingers.
    let mapping = manager.qsos_by_callsign.read().await;
    assert_eq!(
        mapping.get("K9HJZ").map(|v| v.as_slice()),
        Some([recall].as_slice()),
        "mapping must hold only the surviving QSO, no stale superseded id"
    );
}

/// REGRESSION LOCK (scenario b): a manual re-call AFTER the prior QSO with
/// that callsign was retired by the keep-call WATCHDOG (timed out) must
/// start a fresh QSO cleanly — no panic, no stale mapping that misroutes or
/// blocks the new call. This exercises the `check_timeouts_at` terminal
/// path (distinct from the `cancel_qso` path covered above), which is the
/// real "frustrated operator re-call" trigger from the K9HJZ incident.
#[tokio::test]
async fn manual_recall_after_watchdog_timeout_starts_fresh_cleanly() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 2;
    config.timeouts.manual_call_watchdog_minutes = 60; // call-count is the binding bound
    let manager = QsoManager::new(config);
    let freq = 14074000.0;

    let first = manager
        .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
        .await
        .unwrap();

    // Keep-call until the watchdog cap is reached, then let it retire.
    let mut t = Utc::now();
    for _ in 0..4 {
        t += Duration::seconds(15);
        manager.rearm_manual_calls_at(t).await;
    }
    // Past the cap-reaching frame's rollback window (PAN-75).
    manager.check_timeouts_at(t + Duration::seconds(60)).await;

    // The timed-out QSO is gone from the live map AND its callsign mapping
    // is cleared (no stale entry left behind).
    assert!(
        matches!(
            manager.get_qso(first).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "watchdog must have removed the timed-out QSO"
    );
    assert!(
        manager.qsos_by_callsign.read().await.get("K9HJZ").is_none(),
        "timed-out QSO must leave no stale callsign mapping"
    );

    // The operator re-calls the same DX. It must start a fresh, correctly
    // routed QSO — not panic, not be blocked, not be misrouted to the dead id.
    let second = manager
        .respond_to_cq_manual("K9HJZ".to_string(), freq, None)
        .await
        .expect("re-call after watchdog timeout must succeed cleanly");
    assert_ne!(first, second, "must be a brand-new QSO id");

    let progress = manager.get_qso(second).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "fresh QSO must open in RespondingToCq, got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.their_callsign.as_deref(),
        Some("K9HJZ"),
        "fresh QSO must be routed to the re-called DX"
    );
    assert_eq!(
        progress.metadata.call_count, 1,
        "fresh QSO must start its own keep-call count, not inherit the old one"
    );

    // EXACTLY ONE active QSO for the callsign — the fresh one.
    let active = manager.get_active_qsos().await;
    let active_for_dx: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K9HJZ"))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        active_for_dx,
        vec![second],
        "exactly one active QSO (the fresh re-call) after a watchdog timeout"
    );

    // And the mapping points only to it.
    assert_eq!(
        manager
            .qsos_by_callsign
            .read()
            .await
            .get("K9HJZ")
            .map(|v| v.as_slice()),
        Some([second].as_slice()),
        "mapping must point only at the fresh QSO"
    );
}

/// FIX 1: a context-Space reply at a step AHEAD of the existing active
/// QSO's stage ADVANCES the SAME QSO (no new QSO, no supersede). We open a
/// manual QSO (RespondingToCq → step rank 0), then context-reply at Rr73
/// (rank 2): the existing QSO must advance to WaitingForConfirmation and
/// keep its id.
#[tokio::test]
async fn context_reply_ahead_advances_existing_qso() {
    use pancetta_core::ResponseStep;
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let first = manager
        .respond_to_cq_manual("K1DEF".to_string(), freq, None)
        .await
        .unwrap();

    // DX is now ahead (they sent us an R-report); operator context-replies
    // at Rr73. Must advance THIS QSO, not create a new one.
    let advanced = manager
        .respond_to_caller(
            "K1DEF".to_string(),
            freq,
            None,
            ResponseStep::Rr73,
            Some(-10.0),
            Some(-12),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        first, advanced,
        "ahead context reply must continue same QSO"
    );

    let progress = manager.get_qso(first).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "QSO should have advanced to WaitingForConfirmation, got {:?}",
        progress.state
    );

    // Exactly one active QSO for this callsign.
    let active = manager.get_active_qsos().await;
    let n = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
        .count();
    assert_eq!(n, 1, "exactly one active QSO after the advance");
}

/// The manual watchdog retires a RespondingToCq QSO once the call count
/// reaches `manual_call_max_calls`.
#[tokio::test]
async fn manual_watchdog_fires_on_max_calls() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 3;
    config.timeouts.manual_call_watchdog_minutes = 60; // not the binding bound here
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    // Simulate keep-calling: re-arm enough times (one slot apart) to
    // hit the cap. call_count starts at 1; re-arm to 3.
    let mut t = Utc::now();
    for _ in 0..5 {
        t += Duration::seconds(15);
        manager.rearm_manual_calls_at(t).await;
    }
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.call_count >= 3,
        "expected call_count to reach cap, got {}",
        progress.metadata.call_count
    );

    // Watchdog must now retire it, once the cap-reaching frame is past
    // the rollback window (PAN-75).
    manager.check_timeouts_at(t + Duration::seconds(60)).await;
    let after = manager.get_qso(qso_id).await;
    assert!(
        matches!(after, Err(QsoManagerError::QsoNotFound { .. })),
        "watchdog should have removed the QSO, got {:?}",
        after.map(|p| p.state)
    );
}

/// PAN-75 (Codex P2 on PR #418): the cap does not retire a QSO while its
/// cap-reaching frame's TX outcome is still pending. If that frame is
/// reported unsent, the call is given back and keep-calling resumes.
#[tokio::test]
async fn manual_cap_waits_for_cap_reaching_frame_outcome() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 3;
    config.timeouts.manual_call_watchdog_minutes = 60;
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    let t0 = Utc::now();
    let t1 = t0 + Duration::seconds(15);
    let t2 = t0 + Duration::seconds(30);
    manager.rearm_manual_calls_at(t1).await;
    manager.rearm_manual_calls_at(t2).await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.call_count, 3, "cap reached at t2");

    // Same tick as the cap-reaching re-arm: still pending, not retired.
    manager.check_timeouts_at(t2).await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    let text = progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Sent)
        .map(|m| m.raw_text.clone())
        .expect("cap-reaching frame recorded as sent");

    // The TX worker reports the cap-reaching frame as unsent.
    let rollback = manager
        .roll_back_unsent_attempt_at(qso_id, &text, t2 + Duration::seconds(5))
        .await;
    assert_eq!(
        rollback,
        UnsentAttemptRollback::RolledBack { call_count: 2 }
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.call_count, 2,
        "unsent final call given back"
    );

    // Keep-calling resumes and the QSO gets its real last call.
    let t3 = t2 + Duration::seconds(15);
    manager.rearm_manual_calls_at(t3).await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.call_count, 3);
    manager.check_timeouts_at(t3).await;
    assert!(manager.get_qso(qso_id).await.is_ok(), "still pending");

    // Once that frame is past the rollback window, the cap retires it.
    manager.check_timeouts_at(t3 + Duration::seconds(75)).await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));
}

/// The manual watchdog retires a RespondingToCq QSO once the elapsed
/// time exceeds `manual_call_watchdog_minutes`, even below the call cap.
#[tokio::test]
async fn manual_watchdog_fires_on_elapsed_time() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000; // not binding
    config.timeouts.manual_call_watchdog_minutes = 5;
    // Isolate the 5-min manual watchdog from the (tighter) repetitive-TX cap.
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    // Just under 5 minutes: still alive.
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
    manager
        .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
        .await;
    assert!(manager.get_qso(qso_id).await.is_ok());

    // Past 5 minutes: retired.
    manager
        .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
        .await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));
}

/// PAN-17 → PAN-23: a manual call whose DX callsign can never be
/// encoded onto the FT8 wire (here, the decoder's own hash-miss
/// placeholder `<...>` — invalid characters, not just "long") must
/// never even create a QSO in the first place.
///
/// This supersedes PAN-17's original fix, which let `respond_to_cq_manual`
/// create the QSO and relied on the very next watchdog pass to retire it
/// with `MessageUnencodable` (consuming a full TX-worker round trip and
/// briefly showing a doomed QSO in the UI). PAN-23 closes that off at the
/// door: `respond_to_cq_with`'s `InvalidCallsign` guard rejects the
/// literal placeholder synchronously, before any `QsoId` is minted or any
/// `MessageToSend` is emitted — the failure mode observed on-air
/// (production logs 2026-08-20..22) where the operator's selection led
/// straight to a guaranteed-to-fail encode attempt.
///
/// The watchdog's fast-retirement guarantee is still exercised
/// separately by `genuinely_unresolvable_caller_hash_retires_fast_not_a_hang`,
/// which reaches the same `their_callsign == "<...>"` state through
/// inbound decode traffic (latched, not requested) — a path this guard
/// does not and cannot cover, since the placeholder never passes through
/// `respond_to_cq_with`/`respond_to_caller` as a `target_callsign` there.
#[tokio::test]
async fn unencodable_dx_callsign_rejected_immediately_not_via_manual_watchdog() {
    let manager = QsoManager::new(test_config());
    let err = manager
        .respond_to_cq_manual("<...>".to_string(), 14074000.0, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, QsoManagerError::InvalidCallsign { callsign } if callsign == "<...>"),
        "expected InvalidCallsign for the unresolved hash placeholder, got {err:?}"
    );
}

/// A genuinely encodable compound callsign (PAN-17's primary fix covers
/// this on the encoder side) must NOT be caught by the new fast-fail
/// watchdog — only callsigns that can never be represented on the wire
/// at all are retired early.
#[tokio::test]
async fn encodable_compound_callsign_is_not_retired_by_unencodable_watchdog() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "a genuinely encodable compound callsign must not be retired by the \
         unencodable-message watchdog"
    );
}

/// A callsign that's merely long-but-valid up to the 11-char hash-field
/// limit is representable; only exceeding it (or using an invalid
/// character) trips the new watchdog.
#[test]
fn callsign_is_wire_representable_boundary() {
    assert!(callsign_is_wire_representable("K1ABC")); // plain standard
    assert!(callsign_is_wire_representable("YS/WE9G")); // compound, fits
    assert!(callsign_is_wire_representable("PJ4/KA1ABC")); // 10 chars, fits
    assert!(callsign_is_wire_representable("ABCDEFGHIJK")); // exactly 11
    assert!(!callsign_is_wire_representable("ABCDEFGHIJKL")); // 12: too long
    assert!(!callsign_is_wire_representable("<...>")); // invalid chars
}

/// PAN-17 round 2 (Codex review #248, finding 4): the unencodable-
/// message watchdog only checked `their_callsign`. `StationConfig`
/// permits an arbitrarily long compound `our_callsign` with no length
/// cap, so a misconfigured station callsign would leave every QSO --
/// even against a perfectly normal, plain-callsign DX -- stuck
/// re-arming an unencodable TX until the slow generic timeout. Must
/// now retire fast too.
#[tokio::test]
async fn unencodable_our_callsign_retires_immediately() {
    let mut config = test_config();
    config.our_callsign = "VK9/W1XYZ/MM/EXTRA".to_string(); // >11 chars
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);
    let mut events = manager.subscribe();

    // The DX side is perfectly ordinary -- proves this is caught even
    // when `their_callsign` alone would pass every existing check.
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));

    let mut reason = None;
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::QsoFailed {
            qso_id: id,
            reason: r,
            ..
        } = ev
        {
            if id == qso_id {
                reason = Some(r);
            }
        }
    }
    assert!(
        matches!(reason, Some(QsoFailureReason::MessageUnencodable(_))),
        "expected MessageUnencodable, got {:?}",
        reason
    );
}

#[test]
fn callsign_is_plausibly_pack28_standard_boundary() {
    assert!(callsign_is_plausibly_pack28_standard("K1ABC"));
    assert!(callsign_is_plausibly_pack28_standard("W1AW"));
    assert!(callsign_is_plausibly_pack28_standard("K1ABC/P"));
    assert!(callsign_is_plausibly_pack28_standard("K1ABC/R"));
    // Accepted false positive (see the fn's doc): fails pack28's real
    // digit-position rule but this loose check can't detect that.
    assert!(callsign_is_plausibly_pack28_standard("8G81PA"));
    // Unambiguously compound / not pack28-shaped.
    assert!(!callsign_is_plausibly_pack28_standard("YS/WE9G"));
    assert!(!callsign_is_plausibly_pack28_standard("EA8/G8BCG"));
    assert!(!callsign_is_plausibly_pack28_standard("VP2/W1XYZ"));
    assert!(!callsign_is_plausibly_pack28_standard("PJ4/KA1ABC"));
    assert!(!callsign_is_plausibly_pack28_standard("3E40CDW")); // 7 chars
    assert!(!callsign_is_plausibly_pack28_standard("AB")); // 2 chars
}

#[test]
fn report_stage_partner_callsign_extracts_from_report_bearing_states_only() {
    let now = Utc::now();
    assert_eq!(
        report_stage_partner_callsign(&QsoState::SendingReport {
            their_callsign: "YS/WE9G".to_string(),
            their_report: None,
            our_report: -10,
            frequency: 14074000.0,
            started_at: now,
        }),
        Some("YS/WE9G")
    );
    assert_eq!(
        report_stage_partner_callsign(&QsoState::WaitingForReport {
            their_callsign: "YS/WE9G".to_string(),
            frequency: 14074000.0,
            started_at: now,
            their_grid: None,
            our_report: -10,
        }),
        Some("YS/WE9G")
    );
    // PAN-27 finding 3: WaitingForConfirmation is deliberately excluded
    // -- its queued message is always FinalConfirmation (RR73-class),
    // representable via i3=4 regardless of callsign shape. A numeric
    // report only re-enters via a regression to SendingReport, which
    // changes the state variant itself (covered by the arm above).
    assert_eq!(
        report_stage_partner_callsign(&QsoState::WaitingForConfirmation {
            their_callsign: "YS/WE9G".to_string(),
            their_report: -10,
            our_report: -10,
            frequency: 14074000.0,
            grid_square: None,
            started_at: now,
        }),
        None
    );
    // SendingConfirmation only ever sends RR73/73 (both representable
    // via i3=4 regardless of callsign shape) -- deliberately excluded.
    assert_eq!(
        report_stage_partner_callsign(&QsoState::SendingConfirmation {
            their_callsign: "YS/WE9G".to_string(),
            their_report: -10,
            our_report: -10,
            frequency: 14074000.0,
            grid_square: None,
            started_at: now,
        }),
        None
    );
    assert_eq!(report_stage_partner_callsign(&QsoState::Idle), None);
}

/// PAN-17 round 3 (Codex re-review of #248, finding 3): a resolved i3=4
/// hash render ("<K1DEF>") must be normalized to the plain callsign
/// ("K1DEF") before it flows into latched QSO state -- otherwise the
/// still-bracketed literal self-sabotages via the unencodable-message
/// watchdog (`<`/`>` are outside the wire charset). Exercises the full
/// parse -> process_message -> latch path (the normalization itself
/// lives in `exchange.rs::normalize_callsign_token`; this proves it's
/// wired end-to-end into the QSO engine, not just unit-tested in
/// isolation).
#[tokio::test]
async fn resolved_hash_render_reply_latches_plain_callsign_not_bracketed() {
    let mut config = test_config();
    config.our_callsign = "YS/WE9G".to_string(); // compound operator
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    // K1DEF (a standard callsign) replies to our compound CQ; their own
    // callsign lands in the i3=4 hash slot and, having resolved,
    // decodes as "<K1DEF>" -- exactly the wire text
    // `MessageExchange::parse_message` would receive from the decoder.
    let raw_text = "YS/WE9G <K1DEF> EM10";
    let parsed = crate::utils::parse_ft8_message(raw_text, "YS/WE9G").unwrap();
    assert!(
        matches!(&parsed, MessageType::CqResponse { responding_station, .. } if responding_station == "K1DEF"),
        "parse_ft8_message must normalize the resolved hash render, got: {:?}",
        parsed
    );

    manager
        .process_message(parsed, raw_text.to_string(), 14074000.0, Some(-10.0))
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(&progress.state, QsoState::WaitingForReport { their_callsign, .. } if their_callsign == "K1DEF"),
        "expected their_callsign latched as the plain 'K1DEF', got: {:?}",
        progress.state
    );

    // The QSO has landed directly on a report-bearing rung
    // (WaitingForReport) needing a numeric report addressed to OUR
    // OWN compound callsign -- symmetric to finding 1, that
    // genuinely cannot be encoded via real FT8 regardless of which
    // side is compound, so round 3's report-stage check correctly
    // retires it. What this test isolates is WHY: the failure reason
    // must be the report-stage wording (proving K1DEF already passed
    // the plain wire-representability check first) and must NOT
    // mention invalid characters or contain the raw "<" / ">"
    // brackets -- if normalization had NOT happened, the round-2
    // wire-representability check (which runs BEFORE the round-3
    // report-stage check) would have rejected "<K1DEF>" outright on
    // its invalid characters instead.
    let mut events = manager.subscribe();
    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));

    let mut reason = None;
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::QsoFailed {
            qso_id: id,
            reason: r,
            ..
        } = ev
        {
            if id == qso_id {
                reason = Some(r);
            }
        }
    }
    match reason {
        Some(QsoFailureReason::MessageUnencodable(detail)) => {
            assert!(
                detail.contains("numeric report"),
                "expected the report-stage reason, got: {detail}"
            );
            assert!(
                !detail.contains('<') && !detail.contains('>'),
                "failure reason must reference the plain callsign, not a \
                 bracketed hash render: {detail}"
            );
            assert!(
                detail.contains("K1DEF") && !detail.contains("<K1DEF>"),
                "failure reason must name the plain 'K1DEF', not '<K1DEF>': {detail}"
            );
        }
        other => panic!("expected MessageUnencodable, got {:?}", other),
    }
}

/// PAN-17 round 4 (Codex re-review of #248): "resolve first-time
/// callers before filtering type-4 replies."
///
/// Investigated and determined to be an INHERENT i3=4 protocol
/// limitation, not a pancetta bug: a 12-bit hash is a one-way
/// compression of a callsign into 4096 buckets. Reversing it requires
/// having independently learned the plaintext from a standard-format
/// decode -- if a station's very first-ever transmission we've heard
/// (ever, on this band, this session) is itself an i3=4 reply that
/// puts them in the hash slot (structurally forced whenever the OTHER
/// party -- us -- is compound, since the pack28-failing callsign
/// always wins the exact slot), there are no other bits anywhere in
/// that 77-bit payload encoding their plaintext. Round 3's seeding
/// (`Ft8Decoder`'s per-window loop over every decoded
/// `MessageType::Standard`, `pancetta-ft8/src/decoder.rs`) already
/// seeds from ANY standard-format decode this station makes, whenever
/// and wherever heard, not just frames addressed to us -- there is no
/// earlier opportunity pancetta's code is failing to use; WSJT-X has
/// the identical limitation for the identical reason.
///
/// What this test proves instead: the QSO-layer behavior for this
/// case is a clean, BOUNDED failure, not a hang or a silent full-
/// watchdog-window loop. `their_callsign` latches as the literal
/// unresolved placeholder `"<...>"` (`is_message_relevant`'s
/// `CallingCq`/`CqResponse` arm does not itself validate the sender's
/// identity, only that the response is addressed to us -- by design,
/// since at that point we don't yet know who will answer). The
/// EXISTING `callsign_is_wire_representable` watchdog check (PAN-17
/// round 2) then retires it on the very next pass: `"<...>"` contains
/// `<`/`.`/`>`, all outside the wire charset, so it can never be
/// transmitted regardless of report-bearing status.
#[tokio::test]
async fn genuinely_unresolvable_caller_hash_retires_fast_not_a_hang() {
    let mut config = test_config();
    config.our_callsign = "YS/WE9G".to_string(); // compound operator
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);
    let mut events = manager.subscribe();

    let qso_id = manager
        .start_cq(14074000.0, None, false, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    // A standard-callsign station replies to our compound CQ, but we
    // (this decoder, this session) have NEVER heard their plaintext
    // callsign in any standard-format frame before -- their hash
    // genuinely cannot resolve. This is exactly what
    // `MessageExchange::parse_message`/`normalize_callsign_token`
    // hands the QSO engine for that case: the literal placeholder,
    // left untouched (there is no real callsign to normalize it to).
    let parsed = MessageType::CqResponse {
        calling_station: "YS/WE9G".to_string(),
        responding_station: "<...>".to_string(),
        grid: Some("EM10".to_string()),
    };

    manager
        .process_message(
            parsed,
            "YS/WE9G <...> EM10".to_string(),
            14074000.0,
            Some(-10.0),
        )
        .await
        .unwrap();

    // The QSO advances and latches the unresolved placeholder -- relevance
    // routing for a not-yet-partnered CallingCq QSO only verifies the
    // response is addressed to us, not who the sender claims to be.
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(&progress.state, QsoState::WaitingForReport { their_callsign, .. } if their_callsign == "<...>"),
        "expected their_callsign latched as the unresolved placeholder, got: {:?}",
        progress.state
    );

    // The very next watchdog pass retires it -- bounded, fast, not a
    // hang and not a full 5-minute silent loop.
    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));

    let mut reason = None;
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::QsoFailed {
            qso_id: id,
            reason: r,
            ..
        } = ev
        {
            if id == qso_id {
                reason = Some(r);
            }
        }
    }
    assert!(
        matches!(reason, Some(QsoFailureReason::MessageUnencodable(_))),
        "expected a fast, distinct MessageUnencodable retirement, got {:?}",
        reason
    );
}

/// PAN-17 round 3 (Codex re-review of #248, finding 1): a QSO that
/// reaches a report-bearing rung (here, `SendingReport`) against a
/// still-compound partner must retire immediately with
/// `MessageUnencodable`, instead of re-arming an encode that
/// `try_encode_nonstandard` (pancetta-ft8) will always refuse — the
/// original PAN-17 symptom relocated to the report stage. `YS/WE9G`
/// alone passes `callsign_is_wire_representable` (it fits the 58-bit
/// hash field fine), so this specifically exercises the NEW
/// report-stage check, not the pre-existing wire-representability one.
#[tokio::test]
async fn report_stage_watchdog_retires_compound_partner() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        progress.state = QsoState::SendingReport {
            their_callsign: "YS/WE9G".to_string(),
            their_report: Some(-10),
            our_report: -10,
            frequency: 14074000.0,
            started_at: start,
        };
    }

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(
        matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "a report-bearing rung against a compound-callsign partner must retire fast"
    );
}

/// The report-stage watchdog must NOT trip for a perfectly ordinary
/// standard-callsign partner at the same rung -- no false positives.
#[tokio::test]
async fn report_stage_watchdog_does_not_retire_standard_partner() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        progress.state = QsoState::SendingReport {
            their_callsign: "K1DEF".to_string(),
            their_report: Some(-10),
            our_report: -10,
            frequency: 14074000.0,
            started_at: start,
        };
    }

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "a report-bearing rung against a standard-callsign partner must NOT retire early"
    );
}

/// A bare `/P` or `/R` suffix is genuinely pack28-representable (the
/// ONLY two suffixes `pack28` special-cases) -- the report-stage
/// watchdog must not treat it as unencodable. Regression guard for
/// `adversarial_compound_calls.rs::base_first_then_portable_suffix_completes`.
#[tokio::test]
async fn report_stage_watchdog_does_not_retire_portable_suffix_partner() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("G8BCG/P".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        progress.state = QsoState::SendingReport {
            their_callsign: "G8BCG/P".to_string(),
            their_report: Some(-5),
            our_report: -5,
            frequency: 14074000.0,
            started_at: start,
        };
    }

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "a /P-suffix partner is pack28-representable and must NOT retire early"
    );
}

/// PAN-27 finding 3 (round 4 review): the skip-rung path
/// `RespondingToCq + ReportAck → WaitingForConfirmation` (a compound-
/// call DX sends their R-report directly, skipping the plain-report
/// rung) leaves `their_callsign` compound-shaped in a
/// `WaitingForConfirmation` state — but the message that state actually
/// has queued is `FinalConfirmation` (RR73), always representable via
/// i3=4 regardless of callsign shape. The report-stage watchdog must
/// NOT retire this QSO; it was about to complete cleanly.
#[tokio::test]
async fn report_stage_watchdog_does_not_retire_compound_partner_waiting_for_confirmation() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000;
    config.timeouts.manual_call_watchdog_minutes = 60;
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("YS/WE9G".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        progress.state = QsoState::WaitingForConfirmation {
            their_callsign: "YS/WE9G".to_string(),
            their_report: -10,
            our_report: -10,
            frequency: 14074000.0,
            grid_square: None,
            started_at: start,
        };
    }

    manager
        .check_timeouts_at(start + Duration::seconds(1))
        .await;

    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "WaitingForConfirmation against a compound partner has RR73 (not a \
         numeric report) queued and must NOT retire early"
    );
}

#[tokio::test]
async fn engage_contest_profile_stamps_contest_info() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();

    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    let contest_info = progress
        .metadata
        .contest_info
        .expect("contest_info must be set after engaging a profile");
    assert_eq!(contest_info.contest_name, "us-state-qso-party");
}

#[tokio::test]
async fn engage_contest_profile_errors_for_unknown_qso() {
    let config = test_config();
    let manager = QsoManager::new(config);
    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .next()
        .unwrap();
    let bogus_id = QsoId::new_v4();
    let result = manager.engage_contest_profile(bogus_id, profile).await;
    assert!(matches!(
        result,
        Err(QsoManagerError::QsoNotFound { qso_id }) if qso_id == bogus_id
    ));
}

#[tokio::test]
async fn r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged() {
    // `test_config()`'s default `our_callsign` is "W1ABC", but this test's
    // decode text replays the real PAN-49 incident verbatim ("K5ARH K5TD
    // R EM40" — K5ARH is the operator's real callsign). Override so the
    // message is actually addressed to "us"; otherwise
    // `MessageType::is_addressed_to`'s routing check silently drops the
    // frame before it ever reaches a QSO, independent of the transition
    // arm under test.
    let mut config = test_config();
    config.our_callsign = "K5ARH".to_string();
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();

    // No QSO engaged yet — an R+grid ack for an unrelated station must stay
    // NonStandard (today's behavior, unchanged) and route nowhere.
    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    assert!(
        drain(&mut rx).is_empty(),
        "an unengaged QSO must not react to an R+grid ack"
    );

    // Engage a QSO with K5TD, then the same text must reclassify and route.
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    drain(&mut rx); // discard the initial call's MessageToSend
    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "engaged QSO must advance on the R+grid ack, got {:?}",
        progress.state
    );
}

/// PAN-49 regression: replays the real decode from
/// ~/.pancetta/logs/pancetta.log.2026-08-30 (K5TD acking our grid during
/// the 2026-08-29/30 Kansas QSO Party session) that stalled a live manual
/// QSO before this fix. Must now advance to WaitingForConfirmation with
/// the grid latched, instead of silently landing in NonStandard forever.
///
/// `our_callsign` is overridden to the operator's real callsign (K5ARH,
/// not `test_config()`'s default "W1ABC") so the replayed text is
/// genuinely addressed to "us" — see the comment on
/// `r_grid_ack_reclassifies_only_when_a_qso_is_contest_engaged` above for
/// why that matters.
#[tokio::test]
async fn pan_49_k5td_r_grid_ack_advances_the_qso() {
    let mut config = test_config();
    config.our_callsign = "K5ARH".to_string();
    let manager = QsoManager::new(config);
    let qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH K5TD R EM40".to_string(),
            },
            "K5ARH K5TD R EM40".to_string(),
            1203.1,
            Some(-11.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { grid_square: Some(ref g), .. } if g == "EM40"),
        "expected WaitingForConfirmation with grid EM40, got {:?}",
        progress.state
    );
    assert_eq!(progress.metadata.grids.theirs, Some("EM40".to_string()));
    assert_eq!(
        progress
            .metadata
            .contest_info
            .as_ref()
            .map(|c| c.contest_name.as_str()),
        Some("us-state-qso-party")
    );
}

/// Final-review Finding 1/3 regression: a `ContestReply` frame that
/// legitimately reclassifies (because ITS sender — W0D — has its own
/// contest-engaged QSO, satisfying Finding 2's per-QSO gate) is still
/// broadcast-routed to every active QSO addressed to us, including an
/// UNRELATED engaged QSO (K5TD) whose partner is not W0D. That QSO's
/// transition arm must `reject_sender` (state does not advance), and —
/// this is the assertion that would have caught Finding 1 before this
/// fix — the grid-latch block must NOT write W0D's grid into K5TD's
/// `metadata.grids.theirs` just because the frame reached
/// `process_message_for_qso` for that QSO.
#[tokio::test]
async fn spurious_contest_reply_from_unrelated_station_does_not_advance_or_leak_grid() {
    let mut config = test_config();
    config.our_callsign = "K5ARH".to_string();
    let manager = QsoManager::new(config);
    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();

    // QSO under test: engaged with K5TD, RespondingToCq.
    let k5td_qso_id = manager
        .respond_to_cq_manual("K5TD".to_string(), 1203.0, None)
        .await
        .unwrap();
    manager
        .engage_contest_profile(k5td_qso_id, profile.clone())
        .await
        .unwrap();

    // A second, unrelated engaged QSO with W0D at the same frequency —
    // its presence is what legitimately unlocks reclassification (per
    // Finding 2's tightened per-QSO gate) for a frame FROM W0D, even
    // though that frame has nothing to do with the K5TD QSO.
    let w0d_qso_id = manager
        .respond_to_cq_manual("W0D".to_string(), 1203.0, None)
        .await
        .unwrap();
    manager
        .engage_contest_profile(w0d_qso_id, profile)
        .await
        .unwrap();

    // W0D acks OUR grid — legitimate traffic for the W0D QSO, but must
    // not be able to advance or contaminate the unrelated K5TD QSO.
    manager
        .process_message(
            MessageType::NonStandard {
                text: "K5ARH W0D R EM28".to_string(),
            },
            "K5ARH W0D R EM28".to_string(),
            1203.0,
            Some(-11.0),
        )
        .await
        .unwrap();

    let k5td_progress = manager.get_qso(k5td_qso_id).await.unwrap();
    assert!(
        matches!(k5td_progress.state, QsoState::RespondingToCq { .. }),
        "K5TD QSO must not advance on a ContestReply from an unrelated sender, got {:?}",
        k5td_progress.state
    );
    assert_eq!(
        k5td_progress.metadata.grids.theirs, None,
        "W0D's grid must not leak into the unrelated K5TD QSO's metadata"
    );
}

/// PR #344 round-1 Codex P2 regression: PAN-51's `ft8_message_to_qso_type`
/// classifies a native ReplyWithR decode directly as `ContestReply`,
/// never routing through `process_message_with_parity`'s NonStandard
/// reclassification step -- so a QSO that never called
/// `engage_contest_profile` must still decline to advance (and must not
/// latch the grid) when it receives a natively-typed `ContestReply` from
/// its own real partner, exactly matching the pre-PAN-51 (no-contest)
/// behavior for a decode ft8_lib recognized but pancetta never engaged a
/// contest profile for.
#[tokio::test]
async fn native_contest_reply_ignored_when_qso_not_contest_engaged() {
    let mut config = test_config();
    config.our_callsign = "K5ARH".to_string();
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
        .await
        .unwrap();
    // Deliberately never calls engage_contest_profile.

    manager
        .process_message(
            MessageType::ContestReply {
                to_station: "K5ARH".to_string(),
                from_station: "K9ZZ".to_string(),
                grid: "EN37".to_string(),
                is_ack: true,
            },
            "K5ARH K9ZZ R EN37".to_string(),
            1500.0,
            Some(-11.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "non-contest-engaged QSO must not advance on a native ContestReply, got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.grids.theirs, None,
        "grid must not latch for a non-contest-engaged QSO's native ContestReply"
    );
}

/// Companion to the above: the SAME frame DOES advance the QSO once it
/// is contest-engaged -- proving the new gate blocks only the
/// unengaged case, not `ContestReply` handling generally.
#[tokio::test]
async fn native_contest_reply_advances_when_qso_contest_engaged() {
    let mut config = test_config();
    config.our_callsign = "K5ARH".to_string();
    let manager = QsoManager::new(config);
    let profile = crate::contest::catalog::builtin_catalog()
        .into_iter()
        .find(|p| p.id == "us-state-qso-party")
        .unwrap();

    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
        .await
        .unwrap();
    manager
        .engage_contest_profile(qso_id, profile)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::ContestReply {
                to_station: "K5ARH".to_string(),
                from_station: "K9ZZ".to_string(),
                grid: "EN37".to_string(),
                is_ack: true,
            },
            "K5ARH K9ZZ R EN37".to_string(),
            1500.0,
            Some(-11.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "contest-engaged QSO must advance on a native ContestReply, got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.grids.theirs,
        Some("EN37".to_string()),
        "grid must latch for a contest-engaged QSO's native ContestReply"
    );
}

/// Repetitive-TX watchdog: a QSO stuck in the same active TX state (we keep
/// re-sending the same message) is retired at repetitive_tx_timeout_secs,
/// independent of (and tighter than) the manual keep-call watchdog.
#[tokio::test]
async fn repetitive_tx_watchdog_retires_stuck_state() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000; // non-binding
    config.timeouts.manual_call_watchdog_minutes = 60; // non-binding (>>2min)
    config.timeouts.repetitive_tx_timeout_secs = 120;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;

    // Just under 2 min in the same state: still alive.
    manager
        .check_timeouts_at(start + Duration::seconds(119))
        .await;
    assert!(manager.get_qso(qso_id).await.is_ok());

    // Past 2 min stuck re-sending the same message: retired by the cap
    // (manual watchdog bounds are deliberately non-binding here).
    manager
        .check_timeouts_at(start + Duration::seconds(121))
        .await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));
}

/// rearm_manual_calls re-emits a CqResponse MessageToSend and increments
/// the call count — but only once per slot, and not for auto QSOs.
#[tokio::test]
async fn rearm_emits_call_once_per_slot_for_manual_only() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let manual_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let _auto_id = manager
        .respond_to_cq("K9ZZ".to_string(), 14076000.0, None)
        .await
        .unwrap();

    // Drain the two initial MessageToSend events from the responses.
    let mut initial = 0;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, QsoEvent::MessageToSend { .. }) {
            initial += 1;
        }
    }
    assert_eq!(initial, 2, "two initial calls (one manual, one auto)");

    // Re-arm too soon (same instant as start): no new call.
    let start = manager
        .get_qso(manual_id)
        .await
        .unwrap()
        .metadata
        .start_time;
    manager.rearm_manual_calls_at(start).await;
    let mut too_soon = 0;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, QsoEvent::MessageToSend { .. }) {
            too_soon += 1;
        }
    }
    assert_eq!(too_soon, 0, "re-arm within a slot must not re-call");

    // Re-arm a slot later: exactly one new call, for the manual QSO.
    manager
        .rearm_manual_calls_at(start + Duration::seconds(15))
        .await;
    let mut recalls = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { qso_id, .. } = ev {
            recalls.push(qso_id);
        }
    }
    assert_eq!(recalls.len(), 1, "exactly one re-call");
    assert_eq!(recalls[0], manual_id, "only the manual QSO is re-called");
    assert_eq!(
        manager
            .get_qso(manual_id)
            .await
            .unwrap()
            .metadata
            .call_count,
        2
    );
}

/// resend_last_tx on a QSO with a prior Sent message re-emits a
/// MessageToSend carrying that message.
#[tokio::test]
async fn resend_last_tx_reemits_last_sent() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    // Drain the initial call event.
    while events.try_recv().is_ok() {}

    manager.resend_last_tx(qso_id).await.unwrap();

    // Expect exactly one MessageToSend, re-emitting the initial CqResponse.
    let mut resends = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend {
            qso_id, message, ..
        } = ev
        {
            resends.push((qso_id, message));
        }
    }
    assert_eq!(resends.len(), 1, "exactly one resend event");
    assert_eq!(resends[0].0, qso_id);
    assert!(matches!(resends[0].1, MessageType::CqResponse { .. }));
}

/// resend_last_tx stamps `last_call_at` and bumps `call_count` exactly
/// like the keep-call rearm does, so an operator-triggered resend is
/// accounted for watchdog/cap purposes and the rearm/coalescer
/// machinery is aware of it (double-PTT hardening,
/// docs/qso-tx-deep-review-2026-07-18.md).
#[tokio::test]
async fn resend_last_tx_stamps_last_call_at_and_call_count() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    while events.try_recv().is_ok() {}

    let before = manager.get_qso(qso_id).await.unwrap().metadata;
    assert_eq!(before.call_count, 1);
    let last_call_before = before.last_call_at;

    // Ensure the clock actually advances so last_call_at is observably
    // different, mirroring the rearm test's own timing discipline.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    manager.resend_last_tx(qso_id).await.unwrap();

    let after = manager.get_qso(qso_id).await.unwrap().metadata;
    assert_eq!(
        after.call_count, 2,
        "resend must bump call_count exactly like a rearm re-send"
    );
    assert!(
        after.last_call_at.is_some() && after.last_call_at != last_call_before,
        "resend must stamp last_call_at to now, like rearm_manual_calls_at does"
    );
}

/// resend_last_tx on an unknown QSO id returns QsoNotFound.
#[tokio::test]
async fn resend_last_tx_unknown_id_not_found() {
    let manager = QsoManager::new(test_config());
    let bogus = QsoId::new_v4();
    let err = manager.resend_last_tx(bogus).await.unwrap_err();
    assert!(matches!(err, QsoManagerError::QsoNotFound { .. }));
}

/// apply_tx_offset_switch on a QSO updates frequency and resets stall_cycles.
#[tokio::test]
async fn apply_tx_offset_switch_updates_frequency_and_resets_stall_cycles() {
    let manager = auto_manager(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    // Bump stall_cycles by directly manipulating the QSO's metadata.
    {
        let mut qsos = manager.qsos.write().await;
        if let Some(qso) = qsos.get_mut(&qso_id) {
            qso.metadata.stall_cycles = 5;
        }
    }

    // Verify stall_cycles was bumped.
    let before = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(before.metadata.stall_cycles, 5);
    assert_ne!(before.metadata.frequency, 1800.0);

    // Apply the offset switch.
    manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();

    // Verify frequency was updated and stall_cycles was reset.
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, 1800.0);
    assert_eq!(after.metadata.stall_cycles, 0);
}

/// PAN-72 final review (finding 4): `apply_tx_offset_switch` must also
/// update the QSO **state**'s own embedded frequency, exactly as the
/// Hound QSY block hand-writes `SendingReport.frequency` for the same
/// reason: later transitions (notably `Completed`) are built from the
/// preceding state's `frequency`, so updating only `metadata.frequency`
/// would log the pre-switch offset.
#[tokio::test]
async fn apply_tx_offset_switch_updates_the_state_embedded_frequency_too() {
    let manager = auto_manager(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    let before = manager.get_qso(qso_id).await.unwrap();
    assert_ne!(
        before.state.frequency(),
        Some(1800.0),
        "precondition: the state is not already on the target offset"
    );

    manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.state.frequency(),
        Some(1800.0),
        "the state's embedded frequency must follow metadata.frequency, or \
         the eventual Completed record logs the stale offset"
    );
    assert_eq!(after.metadata.frequency, 1800.0);
}

/// PAN-72 final review (finding 6.4): the offset is clamped defensively,
/// since this is a `pub` method documented as the sole external mutator of
/// an active QSO's TX offset. The applied (clamped) value is returned so
/// the coordinator can mirror it into `active_tx_offsets`.
#[tokio::test]
async fn apply_tx_offset_switch_clamps_out_of_band_offsets() {
    let manager = auto_manager(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    let applied = manager
        .apply_tx_offset_switch(qso_id, 9000.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(applied, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
    assert_eq!(after.state.frequency(), Some(ACTIVE_QSO_TX_OFFSET_MAX_HZ));

    let applied = manager
        .apply_tx_offset_switch(qso_id, -50.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(applied, ACTIVE_QSO_TX_OFFSET_MIN_HZ);
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(after.metadata.frequency, ACTIVE_QSO_TX_OFFSET_MIN_HZ);
    assert_eq!(after.state.frequency(), Some(ACTIVE_QSO_TX_OFFSET_MIN_HZ));

    // In-band values pass through untouched.
    let applied = manager
        .apply_tx_offset_switch(qso_id, 1234.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(applied, 1234.0);
}

/// The clamp must NOT be the narrower autonomous *pick* band
/// (`TX_OFFSET_MIN_HZ`..=`TX_OFFSET_MAX_HZ`, 300–2700). Answering a CQ
/// ties our reply to wherever we decoded the DX, unclamped, up to
/// ~2900 Hz — so a `Revert`'s `target_hz` (a `last_known_good_offset_hz`
/// that was itself such a reply offset) must survive intact. Narrowing it
/// would move the QSO off the very offset it demonstrably worked on,
/// defeating the point of reverting.
#[tokio::test]
async fn apply_tx_offset_switch_preserves_a_legitimate_high_reply_offset() {
    let manager = auto_manager(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    // 2850 Hz: above the autonomous pick band's 2700 ceiling, but a
    // perfectly legitimate place for an in-progress QSO to be sitting.
    let applied = manager
        .apply_tx_offset_switch(qso_id, 2850.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(
        applied, 2850.0,
        "a revert to a real, previously-working high offset must not be \
         narrowed to the autonomous pick band's ceiling"
    );
    assert!(applied > TX_OFFSET_MAX_HZ);

    // Likewise at the low end (the pick band starts at 300).
    let applied = manager
        .apply_tx_offset_switch(qso_id, 250.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert_eq!(applied, 250.0);
    assert!(applied < TX_OFFSET_MIN_HZ);
}

/// apply_tx_offset_switch on an unknown QSO id returns QsoNotFound.
#[tokio::test]
async fn apply_tx_offset_switch_on_unknown_qso_returns_not_found() {
    let manager = QsoManager::new(test_config());
    let result = manager
        .apply_tx_offset_switch(
            QsoId::new_v4(),
            1800.0,
            OffsetRelocationOrigin::OperatorForced,
        )
        .await;
    assert!(matches!(result, Err(QsoManagerError::QsoNotFound { .. })));
}

/// PAN-72 Fix C (Codex round 10, thread on `autonomous.rs:988`): the
/// Hold-mode check that gates a queued Switch/Revert currently happens
/// BEFORE the caller awaits `apply_tx_offset_switch`'s write-lock
/// acquisition. If the operator switches to Hold while that await is
/// blocked, the commit must still be refused -- `tx_freq_mode` is
/// re-checked INSIDE this method's own locked section, mirroring its
/// other in-lock guards (staleness, Hound region, no-op).
#[tokio::test]
async fn apply_tx_offset_switch_refuses_when_hold_flips_after_the_request_was_queued() {
    let mut manager = QsoManager::new(test_config());
    let tx_freq_mode = Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    ));
    manager.set_tx_freq_mode_source(Arc::clone(&tx_freq_mode));

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    // The request was raised (queued) while the mode was still Auto --
    // simulated here by simply calling `apply_tx_offset_switch` only
    // AFTER the mode flips, which is exactly the race: the queueing
    // decision and this commit are separated by an `.await` the operator
    // can act inside.
    tx_freq_mode.store(
        pancetta_core::TxFreqMode::Hold.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );

    let before = manager.get_qso(qso_id).await.unwrap();

    let err = manager
        .apply_tx_offset_switch(
            qso_id,
            14074000.0 + 500.0,
            OffsetRelocationOrigin::OperatorForced,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&err, QsoManagerError::OffsetActionHeld { qso_id: id } if *id == qso_id),
        "expected OffsetActionHeld, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "a Hold-mode race is an expected refusal, not a fault"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, before.metadata.frequency,
        "frequency must be completely unchanged"
    );
    assert_eq!(after.state.frequency(), before.state.frequency());
    assert_eq!(after.metadata.stall_cycles, before.metadata.stall_cycles);
    assert_eq!(
        after.metadata.pre_switch_offset, before.metadata.pre_switch_offset,
        "no pre-switch-offset bookkeeping may be written for a refused switch"
    );
}

/// PAN-140 (Codex P1 on PR #369): a PTT that keys AFTER the drain's own
/// top-of-loop pre-check but BEFORE this method's write lock is acquired
/// must still be caught — the exact same race shape as the Hold test
/// above, closed the same way (re-check inside the commit).
#[tokio::test]
async fn apply_tx_offset_switch_refuses_when_ptt_keys_after_the_request_was_queued() {
    let mut manager = QsoManager::new(test_config());
    manager.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    let ptt_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager.set_ptt_active_source(Arc::clone(&ptt_active));

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    // The request was raised (queued) while PTT was still idle --
    // simulated here by keying PTT only AFTER the request was raised,
    // exactly the race: the queueing decision and this commit are
    // separated by an `.await` (the write-lock acquisition) the TX
    // worker can key inside.
    ptt_active.store(true, std::sync::atomic::Ordering::Release);

    let before = manager.get_qso(qso_id).await.unwrap();

    let err = manager
        .apply_tx_offset_switch(
            qso_id,
            14074000.0 + 500.0,
            OffsetRelocationOrigin::OperatorForced,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(&err, QsoManagerError::OffsetActionPttInFlight { qso_id: id } if *id == qso_id),
        "expected OffsetActionPttInFlight, got {err:?}"
    );
    assert!(
        err.is_expected_offset_action_refusal(),
        "a PTT-in-flight race is an expected refusal, not a fault"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, before.metadata.frequency,
        "frequency must be completely unchanged"
    );
}

#[tokio::test]
async fn apply_tx_offset_switch_bumps_the_relocation_epoch_under_the_sync_gate() {
    let mut manager = auto_manager(test_config());
    let epochs = Arc::new(TxRelocationEpochs::default());
    manager.set_relocation_epoch_source(Arc::clone(&epochs));
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    manager.set_ptt_sync_gate_source(Arc::clone(&gate));
    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    let key = qso_id.to_string();
    assert_eq!(epochs.epoch(&key), 0);
    manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    let first = epochs.epoch(&key);
    assert!(first >= 1);

    let held = gate.lock().await;
    // Poll the real switch to Pending while the gate is held. With no
    // other contended locks, this reaches the gate deterministically.
    let mut switching = Box::pin(manager.apply_tx_offset_switch(
        qso_id,
        2100.0,
        OffsetRelocationOrigin::OperatorForced,
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            switching.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    assert_eq!(epochs.epoch(&key), first);
    drop(held);
    switching.await.unwrap();
    assert!(epochs.epoch(&key) > first);
    assert_eq!(epochs.epoch("another-qso"), 0);
    // QsoManager has a manual Clone implementation; it must share epochs.
    assert_eq!(
        manager.clone().relocation_epochs.epoch(&key),
        epochs.epoch(&key)
    );
}

#[tokio::test]
async fn apply_tx_offset_switch_does_not_bump_the_epoch_on_hold_mode_refusal() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    let result = manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await;
    assert!(matches!(
        result,
        Err(QsoManagerError::OffsetActionHeld { .. })
    ));
    assert_eq!(manager.relocation_epochs.epoch(&qso_id.to_string()), 0);
}

#[tokio::test]
async fn apply_tx_offset_switch_does_not_bump_the_epoch_on_ptt_in_flight_refusal() {
    let manager = auto_manager(test_config());
    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    manager.ptt_active.store(true, Ordering::Release);
    let result = manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await;
    assert!(matches!(
        result,
        Err(QsoManagerError::OffsetActionPttInFlight { .. })
    ));
    assert_eq!(manager.relocation_epochs.epoch(&qso_id.to_string()), 0);
}

#[tokio::test]
async fn relocation_epoch_is_private_and_functional_when_no_source_is_injected() {
    let manager = auto_manager(test_config());
    let other = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, None)
        .await
        .unwrap();
    manager
        .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .unwrap();
    assert!(manager.relocation_epochs.epoch(&qso_id.to_string()) >= 1);
    assert_eq!(other.relocation_epochs.epoch(&qso_id.to_string()), 0);
}

/// PAN-143: the actual regression test for the race PAN-140 round 2
/// found — a bare recheck of `ptt_active` cannot close a TOCTOU gap
/// against a writer with no shared lock, only `ptt_sync_gate` can. This
/// test proves the gate is real mutual exclusion, not just another
/// atomic.
///
/// Uses an explicit handshake via `tokio::sync::oneshot`, not a
/// sleep-based head start (Codex round 1 on this PR, P2: a fixed
/// delay can't distinguish "the gate genuinely serializes" from "the
/// scheduler happened to run things in the lucky order"). `ptt_task`
/// proves (via `gate_held_tx`) that it holds `ptt_sync_gate` BEFORE
/// storing `ptt_active`, and does not store/release until told to
/// (via `proceed_tx`) — so for the entire window between those two
/// signals, `apply_tx_offset_switch`'s own `ptt_sync_gate.lock()`
/// (now `tokio::sync::Mutex`, matching production since round 3 needs
/// the gate held across this method's own `.await` emit points) is
/// *structurally* unable to succeed, regardless of scheduling. The
/// `!apply_handle.is_finished()` assertion (Codex round 2, P2) proves
/// `apply_handle` actually reached and is blocked on that lock attempt,
/// not merely that it hasn't been scheduled yet — a regression that
/// removed the gate would let it run to completion almost immediately
/// once polled, well before `proceed_tx` fires, since a bare atomic
/// check has nothing to block on.
///
/// Purely `.await`-based (no OS-thread blocking anywhere), so the
/// default single-threaded `#[tokio::test]` runtime suffices — tokio's
/// scheduler interleaves separately spawned tasks at their own await
/// points regardless of worker-thread count.
#[tokio::test]
async fn apply_tx_offset_switch_blocks_on_the_sync_gate_until_a_concurrent_ptt_assert_completes() {
    let mut manager = QsoManager::new(test_config());
    manager.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    let ptt_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    manager.set_ptt_active_source(Arc::clone(&ptt_active));
    let ptt_sync_gate = Arc::new(tokio::sync::Mutex::new(()));
    manager.set_ptt_sync_gate_source(Arc::clone(&ptt_sync_gate));

    let qso_id = manager
        .respond_to_cq_manual("K1GHI".to_string(), 14074000.0, None)
        .await
        .unwrap();

    let (gate_held_tx, gate_held_rx) = tokio::sync::oneshot::channel::<()>();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel::<()>();

    // Simulate `PttGuard::new`'s critical section directly: acquire
    // the gate, prove it (before touching `ptt_active` at all), then
    // wait for explicit permission before storing and releasing.
    let gate_for_ptt_task = Arc::clone(&ptt_sync_gate);
    let ptt_active_for_task = Arc::clone(&ptt_active);
    let ptt_task = tokio::spawn(async move {
        let _guard = gate_for_ptt_task.lock().await;
        gate_held_tx
            .send(())
            .expect("test harness: receiver dropped before the gate-held handshake");
        proceed_rx
            .await
            .expect("test harness: sender dropped before the proceed handshake");
        ptt_active_for_task.store(true, std::sync::atomic::Ordering::Release);
        // `_guard` drops here, releasing the gate only now.
    });

    // Deterministic: blocks until `ptt_task` genuinely holds the gate.
    // From this point until `proceed_tx` fires below, any attempt to
    // acquire `ptt_sync_gate` elsewhere is structurally unable to
    // succeed.
    gate_held_rx
        .await
        .expect("ptt_task must signal gate-held before this point");

    // Spawn the method under test concurrently — its own
    // `ptt_sync_gate.lock()` is guaranteed blocked right now.
    let manager_for_apply = manager.clone();
    let apply_handle = tokio::spawn(async move {
        manager_for_apply
            .apply_tx_offset_switch(
                qso_id,
                14074000.0 + 500.0,
                OffsetRelocationOrigin::OperatorForced,
            )
            .await
    });
    // Give the scheduler many chances to actually poll `apply_handle`
    // up to its (currently unavailable) lock attempt.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    // Codex round 2 (P2): the yields above give `apply_handle` a
    // chance to run, but don't by themselves PROVE it reached the
    // gate rather than simply not having been scheduled yet — this
    // is that proof. If `ptt_sync_gate` were ever removed from
    // `apply_tx_offset_switch` (the exact regression this test
    // exists to catch), there would be nothing here to block on: the
    // task would run to completion almost immediately once polled,
    // well before `proceed_tx` fires below. With the real gate in
    // place, `apply_handle` is *structurally* unable to finish yet —
    // `ptt_task` still holds the only lock it could pass through —
    // so this assertion is unconditionally true for the correct
    // implementation and would fail the moment that guarantee is
    // ever removed.
    assert!(
        !apply_handle.is_finished(),
        "apply_tx_offset_switch must still be blocked on ptt_sync_gate at this point — \
         if this fires, the gate is not actually serializing against ptt_task"
    );

    proceed_tx
        .send(())
        .expect("ptt_task must still be waiting on the proceed handshake");
    ptt_task.await.unwrap();

    let before = manager.get_qso(qso_id).await.unwrap();
    let err = apply_handle.await.unwrap().unwrap_err();

    assert!(
        matches!(&err, QsoManagerError::OffsetActionPttInFlight { qso_id: id } if *id == qso_id),
        "expected OffsetActionPttInFlight (the gate must make the commit observe the \
         post-store value deterministically), got {err:?}"
    );
    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.frequency, before.metadata.frequency,
        "a genuinely-serialized refusal must leave the frequency untouched"
    );
}

/// PAN-143 round 3 (Codex P1): `apply_tx_offset_switch` must not
/// release `ptt_sync_gate` between committing the frequency mutation
/// and publishing the resulting `TxOffsetApplied`/one-shot-retransmit
/// `MessageToSend` events — releasing it any earlier would let the TX
/// worker key PTT for an already-queued, now-stale request in that
/// gap. Proven the same way as the test above: hold `ptt_sync_gate`
/// externally (simulating a concurrent `PttGuard::new`) across the
/// entire span of an operator-forced `CallingCq` nudge (which takes
/// the one-shot-retransmit path, exercising the code after the old,
/// too-early release point) and confirm the call is still blocked —
/// not just at the commit, but all the way through where the second
/// `MessageToSend` would be emitted.
#[tokio::test]
async fn apply_tx_offset_switch_holds_the_gate_through_event_publication_not_just_the_commit() {
    let mut manager = auto_manager(test_config());
    let ptt_sync_gate = Arc::new(tokio::sync::Mutex::new(()));
    manager.set_ptt_sync_gate_source(Arc::clone(&ptt_sync_gate));
    let mut events = manager.subscribe();

    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    let _ = drain(&mut events); // the initial CQ MessageToSend start_cq itself emits

    // Hold the gate externally, standing in for a concurrent
    // `PttGuard::new` that won the race to acquire it first.
    let held = Arc::clone(&ptt_sync_gate);
    let guard = held.lock_owned().await;

    let manager_for_apply = manager.clone();
    let apply_handle = tokio::spawn(async move {
        manager_for_apply
            .apply_tx_offset_switch(qso_id, 1800.0, OffsetRelocationOrigin::OperatorForced)
            .await
    });

    // Give the scheduler ample opportunity to run `apply_handle` all
    // the way up to (and including an attempt past) the commit — if
    // the gate were released right after the mutation (the round-3
    // bug), this would be more than enough time for the rest of the
    // function, including both event emissions, to complete.
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }

    assert!(
        !apply_handle.is_finished(),
        "apply_tx_offset_switch must remain blocked on ptt_sync_gate through its own \
         TxOffsetApplied/MessageToSend publication, not just through the frequency commit — \
         if this fires, the gate is being released too early (PAN-143 round 3)"
    );

    drop(guard);
    let result = apply_handle.await.unwrap();
    assert!(
        result.is_ok(),
        "the call should succeed once the externally-held gate is released: {result:?}"
    );
}

/// PAN-72 Fix A (Codex round 10, thread on `autonomous.rs:1829`): an
/// operator-forced nudge (`u` keypress) of an autonomous self-CQ's
/// `CallingCq` must actually retransmit the CQ at the new offset.
/// `rearm_manual_calls_at`'s periodic cadence deliberately never
/// re-sends a `CallingCq` for `CallInitiation::Auto` (an independent,
/// untouched design decision), so without this one-shot path the
/// commit above would silently relocate the QSO with nothing ever sent
/// at the new offset.
#[tokio::test]
async fn apply_tx_offset_switch_retransmits_cq_for_an_operator_forced_auto_calling_cq() {
    let manager = auto_manager(test_config());
    let mut events = manager.subscribe();
    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    // Drain the initial CQ MessageToSend `start_cq` itself emits.
    let _ = drain(&mut events);

    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(before.state, QsoState::CallingCq { .. }),
        "precondition: an Auto self-CQ is a CallingCq"
    );
    assert_eq!(before.metadata.initiated_by, CallInitiation::Auto);
    let call_count_before = before.metadata.call_count;
    let last_call_at_before = before.metadata.last_call_at;

    let applied = manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");
    assert_eq!(applied, 1900.0);

    let events = drain(&mut events);
    let mut saw_cq_at_new_offset = false;
    for event in &events {
        if let QsoEvent::MessageToSend {
            message: MessageType::Cq { callsign, .. },
            frequency,
            ..
        } = event
        {
            assert_eq!(callsign, "W1ABC");
            assert_eq!(*frequency, 1900.0, "must retransmit at the NEW offset");
            saw_cq_at_new_offset = true;
        }
    }
    assert!(
        saw_cq_at_new_offset,
        "expected an immediate CQ retransmission at the new offset, got {events:?}"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.call_count,
        call_count_before + 1,
        "call_count must be bumped exactly like a normal rearm resend \
         (double-send guard)"
    );
    assert!(
        after.metadata.last_call_at.is_some(),
        "last_call_at must be stamped"
    );
    assert_ne!(
        after.metadata.last_call_at, last_call_at_before,
        "last_call_at must actually be updated, not left stale"
    );
}

/// Regression companion (PAN-72 Fix A): the SAME call with a
/// STALL-triggered (not operator-forced) origin on an Auto `CallingCq`
/// must NOT retransmit anything. Proves this fix does not accidentally
/// reopen the protected `rearm_manual_calls_at` `is_manual` gate --
/// the silence-driven stall detector's dormancy for Auto self-CQs
/// remains exactly as it was.
#[tokio::test]
async fn apply_tx_offset_switch_does_not_retransmit_cq_for_a_stall_triggered_auto_calling_cq() {
    let manager = auto_manager(test_config());
    let mut events = manager.subscribe();
    let qso_id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    let _ = drain(&mut events);

    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(before.state, QsoState::CallingCq { .. }));
    assert_eq!(before.metadata.initiated_by, CallInitiation::Auto);
    let call_count_before = before.metadata.call_count;
    let last_call_at_before = before.metadata.last_call_at;

    let origin = OffsetRelocationOrigin::StallDetected {
        raised_at_generation: before.metadata.advance_generation,
    };
    let applied = manager
        .apply_tx_offset_switch(qso_id, 1900.0, origin)
        .await
        .expect("committing the stall-triggered switch");
    assert_eq!(applied, 1900.0, "the relocation itself must still commit");

    let events = drain(&mut events);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::MessageToSend { .. })),
        "a STALL-triggered relocation on an Auto CallingCq must not \
         emit any retransmission -- Auto self-CQs stay exactly as \
         dormant as before this fix, got {events:?}"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.call_count, call_count_before,
        "call_count must be untouched by a non-operator-forced relocation"
    );
    assert_eq!(
        after.metadata.last_call_at, last_call_at_before,
        "last_call_at must be untouched by a non-operator-forced relocation"
    );
}

/// PAN-72 round 14: an Auto-initiated QSO in `RespondingToCq` that has
/// ALREADY exhausted its resend cap (`call_count >= AUTO_RESEND_MAX_CALLS`)
/// still passes `resolve_nudge_tx_offset`'s eligibility check (state +
/// initiation only, no cap awareness), so an operator-forced `u` nudge
/// must not silently relocate it with nothing ever retransmitted --
/// `rearm_manual_calls_at`'s own cap gate would otherwise skip it forever.
#[tokio::test]
async fn apply_tx_offset_switch_retransmits_cq_response_for_auto_responding_to_cq_past_cap() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    let _ = drain(&mut events);

    // Drive call_count to (at least) the cap directly, exactly like the
    // sibling `apply_tx_offset_switch_updates_frequency_and_resets_stall_cycles`
    // test manipulates metadata directly for setup.
    {
        let mut qsos = manager.qsos.write().await;
        let qso = qsos.get_mut(&qso_id).unwrap();
        assert_eq!(qso.metadata.initiated_by, CallInitiation::Auto);
        assert!(matches!(qso.state, QsoState::RespondingToCq { .. }));
        qso.metadata.call_count = AUTO_RESEND_MAX_CALLS;
    }
    let last_call_at_before = manager.get_qso(qso_id).await.unwrap().metadata.last_call_at;

    let applied = manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");
    assert_eq!(applied, 1900.0);

    let events = drain(&mut events);
    let mut saw_cq_response_at_new_offset = false;
    for event in &events {
        if let QsoEvent::MessageToSend {
            message: MessageType::CqResponse {
                calling_station, ..
            },
            frequency,
            ..
        } = event
        {
            assert_eq!(calling_station, "K1DEF");
            assert_eq!(*frequency, 1900.0, "must retransmit at the NEW offset");
            saw_cq_response_at_new_offset = true;
        }
    }
    assert!(
        saw_cq_response_at_new_offset,
        "expected an immediate CqResponse retransmission at the new offset, got {events:?}"
    );

    let after = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        after.metadata.call_count,
        AUTO_RESEND_MAX_CALLS + 1,
        "call_count must be bumped exactly like a normal rearm resend (double-send guard)"
    );
    assert_ne!(
        after.metadata.last_call_at, last_call_at_before,
        "last_call_at must actually be updated, not left stale"
    );
}

/// Same gap, the `SendingReport` (plain-report rung, `their_report ==
/// None`) side: re-sends the SAME `SignalReport` `rearm_manual_calls_at`
/// would, never escalating to R-report on its own.
#[tokio::test]
async fn apply_tx_offset_switch_retransmits_signal_report_for_auto_sending_report_past_cap() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    // DX sends its grid-rung reply; we advance RespondingToCq -> stuck at
    // plain-report rung of SendingReport (their_report == None).
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("EM12".to_string()),
            },
            "K1DEF W1ABC EM12".to_string(),
            1500.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut events);
    {
        let mut qsos = manager.qsos.write().await;
        let qso = qsos.get_mut(&qso_id).unwrap();
        assert_eq!(qso.metadata.initiated_by, CallInitiation::Auto);
        assert!(matches!(
            qso.state,
            QsoState::SendingReport {
                their_report: None,
                ..
            }
        ));
        qso.metadata.call_count = AUTO_RESEND_MAX_CALLS;
    }

    manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");

    let events = drain(&mut events);
    let mut saw_signal_report_at_new_offset = false;
    for event in &events {
        if let QsoEvent::MessageToSend {
            message: MessageType::SignalReport { to_station, .. },
            frequency,
            ..
        } = event
        {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(*frequency, 1900.0);
            saw_signal_report_at_new_offset = true;
        }
    }
    assert!(
        saw_signal_report_at_new_offset,
        "expected an immediate SignalReport retransmission at the new offset, got {events:?}"
    );
}

/// Same gap, the `SendingReport` (R-report rung, `their_report ==
/// Some(_)`) side: re-sends `ReportAck`, never regressing to a plain
/// report.
#[tokio::test]
async fn apply_tx_offset_switch_retransmits_report_ack_for_auto_sending_report_with_their_report_past_cap(
) {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    // DX sends its report -> RespondingToCq -> SendingReport (their_report
    // still None); then DX re-sends its report (didn't copy our R) ->
    // their_report becomes Some.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -9,
            },
            "W1ABC K1DEF -09".to_string(),
            1500.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -9,
            },
            "W1ABC K1DEF -09".to_string(),
            1500.0,
            Some(-11.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut events);
    {
        let mut qsos = manager.qsos.write().await;
        let qso = qsos.get_mut(&qso_id).unwrap();
        assert_eq!(qso.metadata.initiated_by, CallInitiation::Auto);
        assert!(matches!(
            qso.state,
            QsoState::SendingReport {
                their_report: Some(_),
                ..
            }
        ));
        qso.metadata.call_count = AUTO_RESEND_MAX_CALLS;
    }

    manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");

    let events = drain(&mut events);
    let mut saw_report_ack_at_new_offset = false;
    for event in &events {
        if let QsoEvent::MessageToSend {
            message: MessageType::ReportAck { to_station, .. },
            frequency,
            ..
        } = event
        {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(*frequency, 1900.0);
            saw_report_ack_at_new_offset = true;
        }
    }
    assert!(
        saw_report_ack_at_new_offset,
        "expected an immediate ReportAck retransmission at the new offset, got {events:?}"
    );
}

/// Regression: an Auto QSO in `RespondingToCq` BELOW the cap
/// (`call_count < AUTO_RESEND_MAX_CALLS`) must NOT get the one-shot
/// retransmission -- the normal periodic `rearm_manual_calls_at` pass
/// will already retransmit on its own very next slot, so firing here too
/// would be a redundant double-send.
#[tokio::test]
async fn apply_tx_offset_switch_does_not_retransmit_for_auto_responding_to_cq_below_cap() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    let _ = drain(&mut events);
    let before = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(before.metadata.initiated_by, CallInitiation::Auto);
    assert!(before.metadata.call_count < AUTO_RESEND_MAX_CALLS);

    manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");

    let events = drain(&mut events);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::MessageToSend { .. })),
        "an Auto QSO below the resend cap must not get the one-shot path \
         (periodic rearm already covers it), got {events:?}"
    );
}

/// Regression: a MANUAL-initiated QSO in `RespondingToCq` past ITS OWN
/// cap (`manual_call_max_calls`) must NOT get this Auto-scoped one-shot
/// path -- Manual's own watchdog-retirement handling in
/// `rearm_manual_calls_at` is untouched by this fix.
#[tokio::test]
async fn apply_tx_offset_switch_does_not_retransmit_for_manual_responding_to_cq_past_its_cap() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 2;
    let manager = auto_manager(config);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    let _ = drain(&mut events);
    {
        let mut qsos = manager.qsos.write().await;
        let qso = qsos.get_mut(&qso_id).unwrap();
        assert_eq!(qso.metadata.initiated_by, CallInitiation::Manual);
        assert!(matches!(qso.state, QsoState::RespondingToCq { .. }));
        qso.metadata.call_count = 2; // == manual_call_max_calls
    }

    manager
        .apply_tx_offset_switch(qso_id, 1900.0, OffsetRelocationOrigin::OperatorForced)
        .await
        .expect("committing the operator-forced nudge");

    let events = drain(&mut events);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::MessageToSend { .. })),
        "a Manual QSO past its own cap must not get the Auto-scoped \
         one-shot path, got {events:?}"
    );
}

/// Regression: a STALL-triggered (not operator-forced) origin on an Auto
/// QSO past the resend cap in `RespondingToCq` must NOT retransmit --
/// proves this fix doesn't loosen the stall detector's own conservatism
/// for Auto QSOs past their resend cap.
#[tokio::test]
async fn apply_tx_offset_switch_does_not_retransmit_for_stall_triggered_auto_past_cap() {
    let manager = auto_manager(test_config());
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut events = manager.subscribe();
    let qso_id = manager
        .respond_to_cq("K1DEF".to_string(), 1500.0, None)
        .await
        .unwrap();
    let _ = drain(&mut events);
    let advance_generation;
    {
        let mut qsos = manager.qsos.write().await;
        let qso = qsos.get_mut(&qso_id).unwrap();
        assert_eq!(qso.metadata.initiated_by, CallInitiation::Auto);
        assert!(matches!(qso.state, QsoState::RespondingToCq { .. }));
        qso.metadata.call_count = AUTO_RESEND_MAX_CALLS;
        advance_generation = qso.metadata.advance_generation;
    }

    let origin = OffsetRelocationOrigin::StallDetected {
        raised_at_generation: advance_generation,
    };
    manager
        .apply_tx_offset_switch(qso_id, 1900.0, origin)
        .await
        .expect("committing the stall-triggered switch");

    let events = drain(&mut events);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::MessageToSend { .. })),
        "a STALL-triggered relocation on an Auto QSO past the resend cap \
         must not emit any retransmission, got {events:?}"
    );
}

/// FIX 4: a manual QSO in SendingReport (we sent R, DX has not advanced)
/// re-emits our R-report (ReportAck) each slot when re-armed.
#[tokio::test]
async fn rearm_resends_r_report_in_sending_report() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    // Advance to SendingReport: DX sends us a report; we send R-report.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -7,
            },
            "W1ABC K1DEF -07".to_string(),
            14074000.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::SendingReport { .. }));
    // Drain the initial call + the auto-sequenced R-report.
    while events.try_recv().is_ok() {}

    // A slot later, with the DX still not advancing, re-arm re-sends our
    // R-report (ReportAck), not a fresh call.
    let last = progress.metadata.last_call_at.unwrap();
    manager
        .rearm_manual_calls_at(last + Duration::seconds(15))
        .await;
    let mut resends = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            resends.push(message);
        }
    }
    assert_eq!(resends.len(), 1, "exactly one re-send, got {:?}", resends);
    match &resends[0] {
        MessageType::ReportAck {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(from_station, "W1ABC");
            assert_eq!(*report, -12, "re-sends our latched R-report");
        }
        other => panic!("expected ReportAck re-send, got {:?}", other),
    }
}

/// The KJ5NJF bug (docs/archive/qso-engine-bugs.md BUG 2 / qso-state-machine-
/// analysis.md GAP-2): a caller who entered SendingReport via the
/// stuck-at-grid arm (DX re-sent grid/call, not a report — so we sent a
/// PLAIN SignalReport and their_report is still None) must have the rearm
/// re-send that SAME plain SignalReport, never escalate to a ReportAck
/// (R-report) we never actually sent. Escalating here previously drove the
/// QSO's own outbound rung forward with zero input from the DX.
#[tokio::test]
async fn rearm_resends_plain_report_when_their_report_is_none() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("KJ5NJF".to_string(), 1650.0, None)
        .await
        .unwrap();
    // The DX re-sends our grid exchange (K5ARH KJ5NJF EM12) — copied us,
    // but has NOT sent a report yet. This is the stuck-at-grid arm:
    // RespondingToCq + CqResponse -> SendingReport{their_report: None}.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "KJ5NJF".to_string(),
                grid: Some("EM12".to_string()),
            },
            "W1ABC KJ5NJF EM12".to_string(),
            1650.0,
            Some(-14.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    match &progress.state {
        QsoState::SendingReport { their_report, .. } => {
            assert!(
                their_report.is_none(),
                "stuck-at-grid entry must carry their_report=None"
            );
        }
        other => panic!("expected SendingReport, got {:?}", other),
    }
    while events.try_recv().is_ok() {}

    // A slot later, with the DX STILL not having sent a report, the rearm
    // must re-send our plain -NN report, NOT escalate to R-NN.
    let last = progress.metadata.last_call_at.unwrap();
    manager
        .rearm_manual_calls_at(last + Duration::seconds(15))
        .await;
    let mut resends = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            resends.push(message);
        }
    }
    assert_eq!(resends.len(), 1, "exactly one re-send, got {:?}", resends);
    match &resends[0] {
        MessageType::SignalReport {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, "KJ5NJF");
            assert_eq!(from_station, "W1ABC");
            assert_eq!(*report, -14, "re-sends our latched (unacked) report");
        }
        other => panic!(
            "expected a plain SignalReport re-send (NOT ReportAck — the KJ5NJF bug), got {:?}",
            other
        ),
    }
}

/// Rearm-coordination fix (qso-state-machine-analysis.md Symptom A): a
/// forward state advance stamps `last_call_at`, so the per-slot rearm does
/// NOT also fire (with the stale/old rung) in the same slot the DX's
/// response just advanced us. Without this, a late-decoding response and
/// the wall-clock rearm could both queue a TransmitRequest for one slot.
#[tokio::test]
async fn forward_advance_stamps_last_call_at_suppressing_same_slot_rearm() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    while events.try_recv().is_ok() {}

    // The DX's report arrives 12s into the slot (a realistically late
    // decode — well under the 15s SLOT_SECONDS rearm threshold measured
    // from `opened_at`, but this IS a genuine forward advance).
    let response_at = opened_at + Duration::seconds(12);
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -7,
            },
            "W1ABC K1DEF -07".to_string(),
            14074000.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::SendingReport { .. }));
    // The forward advance emits its own auto-sequenced reply (R-report) —
    // drain it so only the REARM's output (if any) remains below.
    while events.try_recv().is_ok() {}

    // Call the rearm at the ORIGINAL slot boundary (opened_at + 15s) --
    // before this fix, last_call_at was untouched by the advance, so this
    // would still see elapsed_since_last >= 15s from opened_at and
    // re-emit a stale rung in the very slot the DX just answered.
    manager
        .rearm_manual_calls_at(opened_at + Duration::seconds(15))
        .await;
    let mut resends = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            resends.push(message);
        }
    }
    assert!(
        resends.is_empty(),
        "the rearm must be suppressed the same slot a forward advance occurred, got {:?}",
        resends
    );

    // Sanity: the rearm is NOT permanently disabled — one full slot after
    // the ADVANCE (not the original open), it does fire again.
    manager
        .rearm_manual_calls_at(response_at + Duration::seconds(15))
        .await;
    let mut resends2 = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            resends2.push(message);
        }
    }
    assert_eq!(
        resends2.len(),
        1,
        "rearm must resume one slot after the advance, got {:?}",
        resends2
    );
}

/// qso-state-machine-analysis GAP-1: a DX that copies us on the first
/// exchange and closes straight from our opening grid (RespondingToCq)
/// with RR73 must complete the QSO (previously stalled at grid forever).
#[tokio::test]
async fn responding_to_cq_early_close_with_rr73_completes() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    while events.try_recv().is_ok() {}

    manager
        .process_message(
            MessageType::FinalConfirmation {
                from_station: "K1DEF".to_string(),
                to_station: "W1ABC".to_string(),
            },
            "W1ABC K1DEF RR73".to_string(),
            14074000.0,
            Some(-9.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        progress.state
    );

    // The auto-sequenced reply must be our closing 73.
    let mut sent = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            sent.push(message);
        }
    }
    assert_eq!(sent.len(), 1, "expected exactly one reply, got {:?}", sent);
    match &sent[0] {
        MessageType::SeventyThree {
            to_station,
            from_station,
        } => {
            assert_eq!(to_station, "K1DEF");
            assert_eq!(from_station, "W1ABC");
        }
        other => panic!("expected SeventyThree reply, got {:?}", other),
    }
}

/// GAP-1: a plain "73" close (not RR73) from RespondingToCq also
/// completes, and — matching the SendingReport+73 arm — we do NOT
/// re-send our own 73 (they're already done; re-sending only adds QRM).
#[tokio::test]
async fn responding_to_cq_early_close_with_plain_73_completes_no_resend() {
    let manager = QsoManager::new(test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    while events.try_recv().is_ok() {}

    manager
        .process_message(
            MessageType::SeventyThree {
                from_station: "K1DEF".to_string(),
                to_station: "W1ABC".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            14074000.0,
            Some(-9.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::Completed { .. }));

    let mut sent = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend { message, .. } = ev {
            sent.push(message);
        }
    }
    assert!(
        sent.is_empty(),
        "must NOT re-send a 73 once the DX already said 73, got {:?}",
        sent
    );
}

/// GAP-1 sender verification: a spoofed RR73 (wrong from_station) must
/// NOT complete the QSO — every other arm is sender-verified and this new
/// one must be too.
#[tokio::test]
async fn responding_to_cq_early_close_rejects_spoofed_sender() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::FinalConfirmation {
                from_station: "W9ZZZ".to_string(), // NOT the QSO partner
                to_station: "W1ABC".to_string(),
            },
            "W1ABC W9ZZZ RR73".to_string(),
            14074000.0,
            Some(-9.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a spoofed RR73 must not advance the QSO, got {:?}",
        progress.state
    );
}

/// FIX 4: the watchdog still retires a SendingReport manual QSO that
/// never advances — re-sending our R-report cannot loop forever.
#[tokio::test]
async fn watchdog_retires_stalled_sending_report() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 1000; // not the binding bound
    config.timeouts.manual_call_watchdog_minutes = 5;
    // Isolate the 5-min manual watchdog from the (tighter) repetitive-TX cap.
    config.timeouts.repetitive_tx_timeout_secs = 100_000;
    let manager = QsoManager::new(config);

    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), 14074000.0, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -7,
            },
            "W1ABC K1DEF -07".to_string(),
            14074000.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::SendingReport { .. }
    ));

    let start = manager.get_qso(qso_id).await.unwrap().metadata.start_time;
    // Just under the watchdog: still alive.
    manager
        .check_timeouts_at(start + Duration::seconds(4 * 60 + 59))
        .await;
    assert!(manager.get_qso(qso_id).await.is_ok());
    // Past the watchdog: retired.
    manager
        .check_timeouts_at(start + Duration::seconds(5 * 60 + 1))
        .await;
    assert!(matches!(
        manager.get_qso(qso_id).await,
        Err(QsoManagerError::QsoNotFound { .. })
    ));
}

// --- Batch 2 #6: CQer (we-CQed) completion path -----------------------

/// A full we-CQed exchange must advance all the way to Completed and emit
/// QsoCompleted (it previously stalled in WaitingForReport — no arm out).
/// We drive it via `process_message`, feeding the caller's messages, and
/// assert the QSO completes and logs the caller's grid (Batch 2 #8).
#[tokio::test]
async fn cqer_full_sequence_completes_and_logs_grid() {
    // our_callsign = W1ABC (from test_config).
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager.start_cq(freq, None, false, None).await.unwrap();
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::CallingCq { .. }
    ));
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.role,
        QsoRole::Cqer
    );

    // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();
    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::WaitingForReport { .. }),
        "CallingCq + CqResponse → WaitingForReport, got {:?}",
        p.state
    );

    // Caller rogers our report with their R-report: "W1ABC K1DEF R-12".
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::WaitingForConfirmation { .. }),
        "WaitingForReport + ReportAck → WaitingForConfirmation, got {:?}",
        p.state
    );

    // Caller closes with 73: "W1ABC K1DEF 73" → Completed.
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::Completed { .. }),
        "WaitingForConfirmation + 73 → Completed, got {:?}",
        p.state
    );
    // Batch 2 #8: the caller's grid latched from the opening CqResponse is
    // carried into the logged metadata.
    assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
    assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
    assert!(p.metadata.end_time.is_some());
}

/// PAN-14: a single station must never end up as the partner of TWO
/// simultaneously-active QSO objects — that produces two independent
/// TX cadences (two different `qso_id`s, potentially two different
/// frequency offsets) for the SAME real-world station, which is exactly
/// the "two very-high-amplitude signals for one station in one TX
/// window" symptom reported on-air 2026-08-11.
///
/// Reproduction (mirrors deep-review finding SM-F7,
/// docs/qso-tx-deep-review-2026-07-18.md §A.3): `find_qsos_for_message`
/// routes a message to EVERY active QSO whose `classify_relevance` arm
/// matches, and `process_message_with_parity` then advances ALL of
/// them independently. A `CallingCq` QSO's routing arms accept a
/// SignalReport/CqResponse from ANY station addressed to us (correct —
/// we don't yet know who will answer our CQ). But if we ALSO already
/// have a separate, already-partnered QSO with that exact station
/// (e.g. we just called their CQ and are `RespondingToCq`, waiting for
/// their report), and that station's report happens to land within the
/// CallingCq QSO's frequency gate, the SAME decoded frame satisfies
/// BOTH QSOs' relevance arms. Unlike the create-time paths
/// (`respond_to_cq_with`/`respond_to_caller`), this message-routing
/// path never calls `supersede_active_qsos_for` — nothing in
/// `find_qsos_for_message` excludes a station that already has an
/// active partner elsewhere. Both QSOs advance, and now K1DEF is the
/// active partner of two different `qso_id`s.
#[tokio::test]
async fn calling_cq_and_established_qso_both_accept_same_partners_frame() {
    let manager = QsoManager::new(test_config());

    // QSO A: we already called K1DEF's CQ (e.g. a manual/auto answer)
    // and are RespondingToCq, waiting for K1DEF's own report of us.
    // K1DEF's real TX frequency is 1500.0 Hz.
    let qso_a = manager
        .respond_to_cq_with(
            "K1DEF".to_string(),
            1500.0,
            None,
            CallInitiation::Manual,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_a).await.unwrap().state,
        QsoState::RespondingToCq { .. }
    ));

    // QSO B: completely independent — we are also running our own CQ
    // loop, TXing at 1505.0 Hz (5 Hz from K1DEF's real frequency — well
    // within the CallingCq arm's 15 Hz gate; a routine coincidence on a
    // busy band, not an attacker-crafted collision).
    let qso_b = manager.start_cq(1505.0, None, false, None).await.unwrap();
    assert!(matches!(
        manager.get_qso(qso_b).await.unwrap().state,
        QsoState::CallingCq { .. }
    ));

    // K1DEF sends their report of us, addressed to us, decoded at their
    // real 1500.0 Hz frequency: "W1ABC K1DEF -09". This is EXACTLY the
    // frame QSO A is waiting for (RespondingToCq + SignalReport{from
    // K1DEF, to us}). It is ALSO, independently, a bare-report answer
    // to QSO B's CQ (CallingCq + SignalReport{to us} — any from_station
    // qualifies, since a CallingCq QSO doesn't know who will answer).
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -9,
            },
            "W1ABC K1DEF -09".to_string(),
            1500.0,
            Some(-9.0),
        )
        .await
        .unwrap();

    let active = manager.get_active_qsos().await;
    let k1def_partners: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
        .collect();

    // BUG (if reproduced): both qso_a and qso_b are active AND both are
    // now partnered with K1DEF — two independent qso_ids that will each
    // generate their own TX cadence for the same real station. The
    // invariant is exactly one active QSO per (callsign, band) —
    // AGENTS.md, "At most one QSO object exists per (callsign, band)".
    assert_eq!(
        k1def_partners.len(),
        1,
        "K1DEF must be the active partner of exactly ONE QSO object, not {}: {:#?}",
        k1def_partners.len(),
        k1def_partners
            .iter()
            .map(|(id, p)| (*id, p.state.clone()))
            .collect::<Vec<_>>()
    );
}

/// PAN-14 P1 (Codex round-1 review of PR #250): TWO unpartnered
/// `CallingCq` QSOs can coexist — nothing supersedes an existing
/// `CallingCq` QSO when `start_cq`/`start_cq_manual` opens another one
/// (repeated manual `c` presses, or Fox mode engaging while a CQ is
/// already live). Both have `metadata.their_callsign == None`, so the
/// `sender_has_other_active_or_recent_partner` guard — keyed on an
/// ESTABLISHED partner — cannot see a conflict between them: a single
/// incoming reply independently satisfies both QSOs' "any station"
/// relevance arms. Without a fix, both would advance and partner the
/// SAME real station with two different `qso_id`s (the same failure
/// class PAN-14 exists to close, via a different pair of states).
#[tokio::test]
async fn one_reply_can_only_advance_the_earliest_calling_cq_not_multiple() {
    let manager = QsoManager::new(test_config());

    // Two independent, still-unpartnered CallingCq QSOs, 5 Hz apart —
    // both well within the CallingCq arm's 15 Hz gate for the same
    // incoming decode.
    let qso_x = manager.start_cq(1500.0, None, false, None).await.unwrap();
    // Sleep so `metadata.start_time` orders deterministically (real
    // `Utc::now()` calls back-to-back could otherwise tie).
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let qso_y = manager.start_cq(1505.0, None, false, None).await.unwrap();
    assert!(matches!(
        manager.get_qso(qso_x).await.unwrap().state,
        QsoState::CallingCq { .. }
    ));
    assert!(matches!(
        manager.get_qso(qso_y).await.unwrap().state,
        QsoState::CallingCq { .. }
    ));

    // K1DEF answers with a bare report (grid skipped), addressed to us —
    // the A4 CallingCq routing arm, which accepts ANY from_station.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -9,
            },
            "W1ABC K1DEF -09".to_string(),
            1500.0,
            Some(-9.0),
        )
        .await
        .unwrap();

    let active = manager.get_active_qsos().await;
    let k1def_partners: Vec<_> = active
        .iter()
        .filter(|(_, p)| p.metadata.their_callsign.as_deref() == Some("K1DEF"))
        .collect();
    assert_eq!(
        k1def_partners.len(),
        1,
        "K1DEF must become the partner of exactly ONE CallingCq QSO, not {}: {:#?}",
        k1def_partners.len(),
        k1def_partners
            .iter()
            .map(|(id, p)| (*id, p.state.clone()))
            .collect::<Vec<_>>()
    );

    // The earliest-created CQ must be the one that claimed it; the
    // later one must remain un-advanced (available for its own future
    // caller).
    let p_x = manager.get_qso(qso_x).await.unwrap();
    assert!(
        matches!(p_x.state, QsoState::WaitingForReport { .. }),
        "earliest CQ (qso_x) should advance, got {:?}",
        p_x.state
    );
    let p_y = manager.get_qso(qso_y).await.unwrap();
    assert!(
        matches!(p_y.state, QsoState::CallingCq { .. }),
        "later CQ (qso_y) must remain un-advanced, got {:?}",
        p_y.state
    );
}

/// PAN-14 P2 (Codex round-1 review of PR #250): the routing guard only
/// checked `p.state.is_active()`, so a QSO that just COMPLETED goes
/// invisible to it the instant it terminates — a stray/duplicate frame
/// from that exact station (or a fresh CQ-answer attempt) landing
/// within an unrelated CallingCq QSO's frequency gate could open a
/// SECOND active QSO for a station we finished working seconds ago.
/// Mirrors the existing `has_active_or_recent_qso_with` /
/// `COMPLETED_QSO_REWORK_GRACE` (45 s) "active-or-recently-completed"
/// pattern used elsewhere in this file.
#[tokio::test]
async fn recently_completed_qso_reserves_the_sender_from_an_unrelated_calling_cq() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;

    // Complete a full CQ exchange with K1DEF (mirrors
    // cqer_full_sequence_completes_and_logs_grid).
    let qso_a = manager.start_cq(freq, None, false, None).await.unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_a).await.unwrap().state,
        QsoState::Completed { .. }
    ));

    // Immediately start a NEW, unrelated CQ 5 Hz away — well within the
    // CallingCq arm's 15 Hz gate for a frame decoded at qso_b's offset.
    let qso_b = manager
        .start_cq(freq + 5.0, None, false, None)
        .await
        .unwrap();

    // K1DEF sends a stray/duplicate CqResponse-shaped frame again,
    // within the completed-QSO grace window (this test runs in
    // milliseconds, well under COMPLETED_QSO_REWORK_GRACE's 45 s).
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq + 5.0,
            Some(-10.0),
        )
        .await
        .unwrap();

    let p_b = manager.get_qso(qso_b).await.unwrap();
    assert!(
        matches!(p_b.state, QsoState::CallingCq { .. }),
        "qso_b must remain un-advanced — K1DEF is reserved by its recent completion, got {:?}",
        p_b.state
    );
}

/// PAN-25 (Codex round-2 review of PR #250): the recently-completed
/// suppression above must be scoped to the completed QSO's band — the
/// uniqueness invariant (AGENTS.md) is per (callsign, band), not per
/// callsign alone. A station worked on 20m and answering a fresh CQ on
/// 40m within the same grace window is a legitimate new QSO and must
/// advance normally.
#[tokio::test]
async fn recently_completed_qso_on_a_different_band_does_not_reserve_the_sender() {
    let manager = QsoManager::new(test_config());
    let freq_20m = 14074000.0;
    let freq_40m = 7074000.0;

    // Complete a full CQ exchange with K1DEF on 20m.
    let qso_a = manager.start_cq(freq_20m, None, false, None).await.unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq_20m,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq_20m,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq_20m,
            Some(-11.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_a).await.unwrap().state,
        QsoState::Completed { .. }
    ));

    // Start a NEW, unrelated CQ on 40m — a different band from the
    // just-completed 20m QSO.
    let qso_b = manager
        .start_cq(freq_40m + 5.0, None, false, None)
        .await
        .unwrap();

    // K1DEF answers on 40m, well within the completed-QSO grace window.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq_40m + 5.0,
            Some(-10.0),
        )
        .await
        .unwrap();

    let p_b = manager.get_qso(qso_b).await.unwrap();
    assert!(
        matches!(p_b.state, QsoState::WaitingForReport { .. }),
        "qso_b must advance — K1DEF's 20m completion must not reserve it on 40m, got {:?}",
        p_b.state
    );
}

/// PAN-25 round 1 (Codex): the test above proves the *comparison logic*
/// but supplies full RF frequencies (14074000.0, 7074000.0) directly as
/// `frequency` — in the live coordinator path `frequency` is always a
/// small AUDIO OFFSET (a few hundred/thousand Hz), and pre-this-round the
/// stored `metadata.frequency` for a Completed QSO was too, so
/// `frequency_to_band` returned the same "0MHZ" bucket for both
/// regardless of the real RF band — the fix looked correct but was
/// inert in production. This test uses realistic small offsets and a
/// shared dial-frequency source that changes between the two QSOs
/// (exactly the "operator changes bands" scenario PAN-25 describes),
/// proving the actual dial-adjustment mechanism the production code
/// path uses.
#[tokio::test]
async fn recently_completed_qso_on_a_different_dial_band_does_not_reserve_the_sender() {
    let mut manager = QsoManager::new(test_config());
    let dial = std::sync::Arc::new(AtomicU64::new(14_074_000)); // 20m
    manager.set_dial_frequency_source(dial.clone());
    let audio_offset = 1500.0; // realistic small in-passband offset

    // Complete a full CQ exchange with K1DEF on 20m (dial 14.074 MHz).
    let qso_a = manager
        .start_cq(audio_offset, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            audio_offset,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    let completed = manager.get_qso(qso_a).await.unwrap();
    assert!(matches!(completed.state, QsoState::Completed { .. }));
    // PAN-25 round 2: `frequency` must stay the audio offset (other
    // consumers like `resend_last_tx` depend on it) -- the true RF
    // frequency is stamped separately into `completed_rf_frequency_hz`.
    assert_eq!(
        completed.metadata.frequency, audio_offset,
        "completed QSO's stored frequency must remain the audio offset, not be overwritten"
    );
    assert_eq!(
        completed.metadata.completed_rf_frequency_hz,
        Some(14_074_000.0 + audio_offset),
        "completed QSO's true RF frequency must be stamped into completed_rf_frequency_hz"
    );

    // Operator moves to 40m — a real dial change, within the grace window.
    dial.store(7_074_000, Ordering::Relaxed);

    // A NEW, unrelated CQ at the SAME small audio offset (plausible: the
    // new CQ just happens to land in a similar spot in the passband).
    let qso_b = manager
        .start_cq(audio_offset + 5.0, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            audio_offset + 5.0,
            Some(-10.0),
        )
        .await
        .unwrap();

    let p_b = manager.get_qso(qso_b).await.unwrap();
    assert!(
        matches!(p_b.state, QsoState::WaitingForReport { .. }),
        "qso_b must advance — K1DEF's 20m completion (different dial band) must not \
         reserve it on 40m, got {:?}",
        p_b.state
    );
}

/// PAN-25 round 2 (Codex P1): a real regression the earlier version of
/// this fix introduced — stamping `metadata.frequency` itself with the
/// RF value made `resend_last_tx` (a close-step retry / 73-recovery)
/// try to key at ~14 MHz, which the TX worker correctly rejects as
/// exceeding its ~3100 Hz modulation limit, so the 73 never actually
/// transmits. Proves `resend_last_tx` on a COMPLETED QSO (with a real
/// dial configured, so `completed_rf_frequency_hz` is genuinely
/// populated) still resends at the audio-offset frequency, not the RF
/// one.
#[tokio::test]
async fn resend_last_tx_on_a_completed_qso_uses_the_audio_offset_not_rf_frequency() {
    let mut manager = QsoManager::new(test_config());
    manager.set_dial_frequency_source(std::sync::Arc::new(AtomicU64::new(14_074_000)));
    let audio_offset = 1500.0;

    let qso_id = manager
        .start_cq(audio_offset, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            audio_offset,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    let completed = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(completed.state, QsoState::Completed { .. }));
    assert!(
        completed.metadata.completed_rf_frequency_hz.is_some(),
        "precondition: this test needs a real dial-adjusted RF frequency stamped, to prove \
         resend_last_tx doesn't accidentally pick it up"
    );

    let mut rx = manager.subscribe();
    manager.resend_last_tx(qso_id).await.unwrap();
    let mut resent_frequency = None;
    while let Ok(event) = rx.try_recv() {
        if let QsoEvent::MessageToSend { frequency, .. } = event {
            resent_frequency = Some(frequency);
        }
    }
    assert_eq!(
        resent_frequency,
        Some(audio_offset),
        "resend_last_tx on a completed QSO must resend at the audio-offset frequency, not \
         the ~14 MHz RF frequency — the TX worker's modulation limit would reject that and \
         the message would never actually key"
    );
}

/// Companion to the test above: the SAME dial (no band change) must
/// still correctly suppress, proving the dial-adjustment doesn't
/// accidentally defeat the original PAN-14 same-band protection.
#[tokio::test]
async fn recently_completed_qso_on_the_same_dial_band_still_reserves_the_sender() {
    let mut manager = QsoManager::new(test_config());
    let dial = std::sync::Arc::new(AtomicU64::new(14_074_000)); // 20m, unchanged throughout
    manager.set_dial_frequency_source(dial);
    let audio_offset = 1500.0;

    let qso_a = manager
        .start_cq(audio_offset, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            audio_offset,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            audio_offset,
            Some(-11.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_a).await.unwrap().state,
        QsoState::Completed { .. }
    ));

    let qso_b = manager
        .start_cq(audio_offset + 5.0, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            audio_offset + 5.0,
            Some(-10.0),
        )
        .await
        .unwrap();

    let p_b = manager.get_qso(qso_b).await.unwrap();
    assert!(
        matches!(p_b.state, QsoState::CallingCq { .. }),
        "qso_b must remain un-advanced — K1DEF is reserved by its recent same-band \
         completion, got {:?}",
        p_b.state
    );
}

/// Layer 2 timeline persistence (docs/observability-diagnostics-plan.md):
/// the `QsoEvent::QsoCompleted` broadcast at the end of a real multi-step
/// exchange must carry the QSO's ACTUAL state_history/messages, not the
/// empty vecs `QsoLogger::handle_qso_completed` used to hard-code. This
/// is what proves the emission sites (not just the DB round-trip) are
/// wired correctly — a persistence subscriber never sees an empty
/// timeline for a QSO that genuinely had one.
#[tokio::test]
async fn qso_completed_event_carries_full_timeline() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager.start_cq(freq, None, false, None).await.unwrap();
    let mut rx = manager.subscribe();

    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    let completed = events
        .into_iter()
        .find_map(|e| match e {
            QsoEvent::QsoCompleted {
                qso_id: id,
                state_history,
                messages,
                ..
            } if id == qso_id => Some((state_history, messages)),
            _ => None,
        })
        .expect("expected a QsoCompleted event for this qso_id");
    let (state_history, messages) = completed;

    // Three forward transitions: CallingCq->WaitingForReport->
    // WaitingForConfirmation->Completed, one per process_message call.
    assert_eq!(
        state_history.len(),
        3,
        "expected 3 state transitions in the completed event, got {:?}",
        state_history
    );
    assert!(
        matches!(state_history[0].from_state, QsoState::CallingCq { .. }),
        "first transition must start from CallingCq, got {:?}",
        state_history[0]
    );
    assert!(
        matches!(
            state_history.last().unwrap().to_state,
            QsoState::Completed { .. }
        ),
        "last transition must land on Completed, got {:?}",
        state_history.last()
    );
    // At minimum the CQ we sent plus the three replies we sent in
    // response — never empty like the old hard-coded discard.
    assert!(
        !messages.is_empty(),
        "expected a non-empty message history in the completed event"
    );
}

/// The manual CQer also EMITS the right reply at each step (the auto-reply
/// path is Manual-gated). Drive a manual CQ QSO and verify the reply
/// sequence SignalReport → FinalConfirmation reaches the event bus.
#[tokio::test]
async fn manual_cqer_emits_report_then_rr73() {
    use tokio::sync::broadcast::error::TryRecvError;
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    // Build a manual CallingCq QSO directly (start_cq is Auto-only; the
    // operator-CQ-as-QSO wiring is a separate deferred item — see report).
    let qso_id = Uuid::new_v4();
    let now = Utc::now();
    let progress = QsoProgress {
        state: QsoState::CallingCq {
            frequency: freq,
            started_at: now,
            call_count: 1,
        },
        state_history: vec![],
        messages: vec![],
        metadata: QsoMetadata {
            qso_id,
            our_callsign: "W1ABC".to_string(),
            their_callsign: None,
            frequency: freq,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: CallInitiation::Manual,
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            remote_client_key_id: None,
            tx_parity_provisional: false,
        },
    };
    manager.qsos.write().await.insert(qso_id, progress);

    let mut events = manager.subscribe();

    // Caller answers → we should emit a SignalReport.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();
    let mut saw_report = false;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::MessageToSend { message, .. }) => {
                if matches!(message, MessageType::SignalReport { .. }) {
                    saw_report = true;
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert!(saw_report, "manual CQer should emit a SignalReport reply");

    // Caller R-reports → we should emit a FinalConfirmation (RR73).
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();
    let mut saw_rr73 = false;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::MessageToSend { message, .. }) => {
                if matches!(message, MessageType::FinalConfirmation { .. }) {
                    saw_rr73 = true;
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert!(
        saw_rr73,
        "manual CQer should emit a FinalConfirmation (RR73)"
    );
}

// --- Manual `c` (CQ) → CallingCq QSO ---------------------------------

/// Pressing `c` (`start_cq_manual`) creates an ACTIVE, manual CallingCq
/// QSO that emits a StateChanged (so the coordinator keys it into
/// `active_tx_qsos`) and an opening Cq MessageToSend.
#[tokio::test]
async fn start_cq_manual_creates_active_calling_cq_qso() {
    use tokio::sync::broadcast::error::TryRecvError;
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let mut events = manager.subscribe();

    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();

    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::CallingCq { .. }),
        "expected CallingCq, got {:?}",
        p.state
    );
    assert_eq!(p.metadata.initiated_by, CallInitiation::Manual);
    assert_eq!(p.metadata.role, QsoRole::Cqer);
    // It is an active QSO (so it shows in the active set).
    assert_eq!(manager.get_active_qsos().await.len(), 1);

    // It emitted a StateChanged into CallingCq (keys active_tx_qsos in the
    // coordinator) and an opening Cq MessageToSend.
    let mut saw_state_change = false;
    let mut saw_cq = false;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::StateChanged { new_state, .. }) => {
                if matches!(new_state, QsoState::CallingCq { .. }) {
                    saw_state_change = true;
                }
            }
            Ok(QsoEvent::MessageToSend { message, .. }) => {
                if matches!(message, MessageType::Cq { .. }) {
                    saw_cq = true;
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert!(
        saw_state_change,
        "start_cq_manual should emit a StateChanged into CallingCq"
    );
    assert!(saw_cq, "start_cq_manual should emit an opening Cq message");
}

/// The full operator-CQ exchange: a caller answers our manual CQ and the
/// exchange auto-sequences (Manual-gated auto-reply) all the way to
/// Completed + QsoCompleted (ADIF log), latching the caller's grid.
#[tokio::test]
async fn start_cq_manual_caller_answer_completes_and_logs() {
    use tokio::sync::broadcast::error::TryRecvError;
    let manager = QsoManager::new(test_config()); // our call = W1ABC
    let freq = 14074000.0;
    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();
    let mut events = manager.subscribe();

    // Caller answers our CQ with their grid: "W1ABC K1DEF FN31".
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::WaitingForReport { .. }
        ),
        "CallingCq + CqResponse → WaitingForReport"
    );

    // Caller rogers our report: "W1ABC K1DEF R-12".
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -12,
            },
            "W1ABC K1DEF R-12".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();

    // Caller closes with 73 → Completed.
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF 73".to_string(),
            freq,
            Some(-11.0),
        )
        .await
        .unwrap();

    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        p.state
    );
    assert_eq!(p.metadata.their_callsign.as_deref(), Some("K1DEF"));
    assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
    assert!(p.metadata.end_time.is_some());

    // A QsoCompleted event fired (ADIF logger subscribes to this).
    let mut saw_completed = false;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::QsoCompleted { qso_id: id, .. }) if id == qso_id => {
                saw_completed = true;
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert!(saw_completed, "completed CQ QSO should emit QsoCompleted");
}

/// While calling CQ (un-answered), the per-slot rearm keeps re-emitting our
/// CQ — exactly ONE keep-call per slot (no double-TX), bounded by the
/// manual watchdog.
#[tokio::test]
async fn manual_cq_rearm_re_emits_one_cq_per_slot() {
    use tokio::sync::broadcast::error::TryRecvError;
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();
    let mut events = manager.subscribe();

    let start = Utc::now();
    // Same slot as creation (< 15s elapsed): rearm must NOT re-emit (the
    // opening CQ already went out — re-emitting now would double-TX).
    manager
        .rearm_manual_calls_at(start + Duration::seconds(5))
        .await;
    // One slot later: rearm re-emits exactly one Cq.
    manager
        .rearm_manual_calls_at(start + Duration::seconds(16))
        .await;

    let mut cq_count = 0;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::MessageToSend { message, .. }) => {
                if matches!(message, MessageType::Cq { .. }) {
                    cq_count += 1;
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert_eq!(
        cq_count, 1,
        "rearm should emit exactly one CQ keep-call across one elapsed slot"
    );

    // Sanity: the QSO is still CallingCq and call_count advanced.
    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(p.state, QsoState::CallingCq { .. }));
    assert_eq!(p.metadata.call_count, 2, "1 opening + 1 rearm");
}

/// docs/archive/qso-engine-bugs.md BUG 1: a `tx_parity: None` (no fixed
/// preference — the Auto-config default) CallingCq QSO MUST latch a
/// single concrete parity at creation and hold it for the life of the
/// QSO — the opening CQ and EVERY keep-call rearm across many slots must
/// all carry the SAME parity. Before the fix, `tx_parity` stayed `None`
/// and each emission re-asked "nearest slot" fresh, alternating parities
/// (i.e. transmitting on both — the station never heard replies).
#[tokio::test]
async fn manual_cq_with_no_parity_preference_latches_one_parity_for_life_of_qso() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();

    // The opening CQ must have latched a CONCRETE (not None) parity.
    let latched = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .tx_parity
        .expect("start_cq_manual(tx_parity: None) must latch a concrete parity, not leave it None");

    let mut events = manager.subscribe();
    let start = Utc::now();

    // Drive 6 consecutive keep-call rearms (6 slots = 90s of CQing).
    for slot in 1..=6i64 {
        manager
            .rearm_manual_calls_at(start + Duration::seconds(15 * slot + 1))
            .await;
    }

    let mut cq_parities = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend {
            message: MessageType::Cq { .. },
            tx_parity,
            ..
        } = ev
        {
            cq_parities.push(tx_parity);
        }
    }
    assert_eq!(cq_parities.len(), 6, "one CQ per slot across 6 slots");
    for (i, p) in cq_parities.iter().enumerate() {
        assert_eq!(
            *p,
            Some(latched),
            "rearm #{i} transmitted on a DIFFERENT parity than the opening CQ \
             — this is BUG 1: transmitting on every window instead of alternating"
        );
    }
}

/// A caller-supplied FIXED parity preference (`Some(_)`) must be honored
/// as-is — the latch-on-None fix must not override an explicit choice.
#[tokio::test]
async fn manual_cq_with_explicit_parity_preference_is_not_overridden() {
    let manager = QsoManager::new(test_config());
    let qso_id = manager
        .start_cq_manual(
            14074000.0,
            Some(pancetta_core::slot::SlotParity::Odd),
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.tx_parity,
        Some(pancetta_core::slot::SlotParity::Odd),
        "an explicit parity preference must be honored unchanged"
    );
}

/// The manual CQ watchdog retires an un-answered CallingCq QSO once it
/// hits the max-calls bound (so we never CQ forever).
#[tokio::test]
async fn manual_cq_watchdog_retires_after_max_calls() {
    // Pin a small call cap so this exercises the cap mechanism
    // deterministically, independent of the (now 25) default.
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 10;
    let manager = QsoManager::new(config);
    let freq = 14074000.0;
    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();

    // Drive enough slots to exceed manual_call_max_calls (10).
    let mut t = Utc::now();
    for _ in 0..15 {
        t += Duration::seconds(16);
        manager.rearm_manual_calls_at(t).await;
    }
    manager.check_timeouts_at(t).await;

    // QSO retired (Failed / mapping cleared → not found via get_qso after
    // cleanup, or in a terminal state).
    match manager.get_qso(qso_id).await {
        Ok(p) => assert!(
            p.state.is_terminal(),
            "watchdog should retire the CQ QSO, got {:?}",
            p.state
        ),
        Err(QsoManagerError::QsoNotFound { .. }) => {}
        Err(e) => panic!("unexpected error: {e}"),
    }
}

/// `cancel_qso` (StopCq path) cancels an un-answered CallingCq QSO.
#[tokio::test]
async fn manual_cq_cancel_stops_calling() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager
        .start_cq_manual(freq, None, false, None)
        .await
        .unwrap();
    assert_eq!(manager.get_active_qsos().await.len(), 1);

    manager.cancel_qso(qso_id).await.unwrap();

    // No longer active; a subsequent rearm emits nothing for it.
    assert!(manager
        .get_active_qsos()
        .await
        .iter()
        .all(|(_, p)| !matches!(p.state, QsoState::CallingCq { .. })));
}

// --- Batch 2 #7 / FIX 1: case-insensitive continue --------------------

/// A manual call to "k1def" must CONTINUE the existing active QSO with
/// "K1DEF" (case-insensitive match), not spawn a duplicate and not
/// supersede. The case-insensitive callsign keying is what makes the
/// FIX-1 active-QSO lookup robust to case/format mismatches between a
/// DX-Hunter call and a Callers reply for the same station.
#[tokio::test]
async fn recall_continue_is_case_insensitive() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let first = manager
        .respond_to_cq_manual("K1DEF".to_string(), freq, None)
        .await
        .unwrap();
    // Re-call with different case — should CONTINUE `first` (same id).
    let second = manager
        .respond_to_cq_manual("k1def".to_string(), freq, None)
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "case-different re-call must continue the same QSO"
    );
    // `first` is NOT superseded — still active.
    let first_state = manager.get_qso(first).await.unwrap().state;
    assert!(
        matches!(first_state, QsoState::RespondingToCq { .. }),
        "first QSO must remain active (not superseded), got {:?}",
        first_state
    );
    let active = manager.get_active_qsos().await;
    assert_eq!(
        active.len(),
        1,
        "exactly one active QSO after case-different re-call"
    );
    assert_eq!(active[0].0, first);
}

// --- Batch 2 #8: grid latched into Completed (Caller path) ------------

/// In the Caller flow the DX's grid arrives in the opening CqResponse and
/// must reach the logged metadata even though the close arm hard-codes
/// grid_square: None. Drive a manual caller QSO and complete it.
#[tokio::test]
async fn caller_grid_latched_into_completed_metadata() {
    let manager = QsoManager::new(test_config());
    let freq = 14074000.0;
    let qso_id = manager
        .respond_to_cq_manual("K1DEF".to_string(), freq, None)
        .await
        .unwrap();

    // The DX re-sends "W1ABC K1DEF FN31" (their grid) — latch it.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: "W1ABC".to_string(),
                responding_station: "K1DEF".to_string(),
                grid: Some("FN31".to_string()),
            },
            "W1ABC K1DEF FN31".to_string(),
            freq,
            Some(-10.0),
        )
        .await
        .unwrap();

    // DX sends our report → SendingReport.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
                report: -9,
            },
            "W1ABC K1DEF -09".to_string(),
            freq,
            Some(-9.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::SendingReport { .. }
    ));

    // DX closes from our R directly with RR73 → Completed (grid_square None
    // in the state, but metadata.grids.theirs already latched FN31).
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: "W1ABC".to_string(),
                from_station: "K1DEF".to_string(),
            },
            "W1ABC K1DEF RR73".to_string(),
            freq,
            Some(-9.0),
        )
        .await
        .unwrap();
    let p = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(p.state, QsoState::Completed { .. }),
        "got {:?}",
        p.state
    );
    assert_eq!(p.metadata.grids.theirs.as_deref(), Some("FN31"));
}
