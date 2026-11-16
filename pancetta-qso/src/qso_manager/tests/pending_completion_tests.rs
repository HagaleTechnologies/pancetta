//! PAN-280: a completion that queues a closing frame emits `QsoCompleted`
//! only when that frame airs; a frame that never airs reverts it.
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::SeqCst};

const OUR: &str = "K5ARH";
const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn manager(permitted: Arc<AtomicBool>) -> QsoManager {
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        ..QsoManagerConfig::default()
    });
    manager.set_remote_tx_permitted_source(Arc::new(move |_| permitted.load(SeqCst)));
    manager
}

async fn reply(
    manager: &QsoManager,
    step: ResponseStep,
    remote: Option<&str>,
) -> (QsoId, ManualCallOutcome) {
    manager
        .respond_to_caller_with_outcome(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            step,
            Some(-8.0),
            Some(-4),
            None,
            remote.is_some(),
            remote.map(str::to_string),
        )
        .await
        .unwrap()
}

fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn completions(events: &[QsoEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, QsoEvent::QsoCompleted { .. }))
        .count()
}

fn payload() -> UnsentAttemptPayload {
    UnsentAttemptPayload {
        frequency_hz: FREQ,
        remote_binding: None,
    }
}

async fn air(manager: &QsoManager, id: QsoId, attempt: TxAttemptId) -> CompletionAired {
    manager
        .confirm_completion_aired(id, Some(attempt), "", &payload(), Utc::now())
        .await
}

fn newest_sent_text(progress: &QsoProgress) -> Option<String> {
    progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Sent)
        .map(|m| m.raw_text.clone())
}

/// Ticket scenario 1: a remote-origin QSO advances to 73, the client's
/// arm expires before the 73 keys, the frame is not sent, and nothing is
/// logged; the QSO is back in its pre-73 state.
#[tokio::test]
async fn dropped_remote_ladder_73_is_not_logged_and_reopens() {
    let permitted = Arc::new(AtomicBool::new(true));
    let manager = manager(Arc::clone(&permitted));
    let (id, _) = reply(&manager, ResponseStep::ReportAck, Some("client-a")).await;
    let before = manager.get_qso(id).await.unwrap();
    assert!(matches!(before.state, QsoState::SendingReport { .. }));
    let mut rx = manager.subscribe();

    let (aid, advanced) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert_eq!(aid, id);
    let attempt = advanced.attempt_id().expect("the advance emitted a 73");
    let events = drain(&mut rx);
    assert_eq!(completions(&events), 0, "nothing logs at queue time");
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));

    permitted.store(false, SeqCst);
    assert_eq!(
        manager
            .resolve_unsent_completion_at(id, Some(attempt), Utc::now())
            .await,
        UnsentCompletion::Reverted(RevertedTo::Reopened)
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.metadata.end_time, before.metadata.end_time);
    assert_eq!(
        after.metadata.completed_rf_frequency_hz,
        before.metadata.completed_rf_frequency_hz
    );
    assert_eq!(after.metadata.reports.sent, before.metadata.reports.sent);
    assert_eq!(
        after.metadata.reports.received,
        before.metadata.reports.received
    );
    assert_eq!(after.messages.len(), before.messages.len());
    assert_eq!(newest_sent_text(&after), newest_sent_text(&before));

    let events = drain(&mut rx);
    assert_eq!(completions(&events), 0);
    assert!(events.iter().any(|e| matches!(
        e,
        QsoEvent::StateChanged {
            old_state: QsoState::Completed { .. },
            new_state: QsoState::SendingReport { .. },
            ..
        }
    )));
    assert_eq!(
        air(&manager, id, attempt).await,
        CompletionAired::NotPending
    );
    assert_eq!(completions(&drain(&mut rx)), 0);
}

/// Ticket scenario 1: the reverted QSO stays in its pre-73 state for one
/// rework grace, then the normal timeouts apply.
#[tokio::test]
async fn reverted_qso_is_exempt_from_timeouts_for_one_rework_grace() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let (id, _) = reply(&manager, ResponseStep::Rr73, None).await;
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::WaitingForConfirmation { .. }
    ));
    let (_, advanced) = reply(&manager, ResponseStep::SeventyThree, None).await;
    let t0 = Utc::now();
    assert_eq!(
        manager
            .resolve_unsent_completion_at(id, advanced.attempt_id(), t0)
            .await,
        UnsentCompletion::Reverted(RevertedTo::Reopened)
    );
    // confirmation_timeout is 30 s: without the exemption this retires.
    manager.check_timeouts_at(t0 + Duration::seconds(44)).await;
    assert!(matches!(
        manager.get_qso(id).await.map(|p| p.state),
        Ok(QsoState::WaitingForConfirmation { .. })
    ));
    manager.check_timeouts_at(t0 + Duration::seconds(46)).await;
    assert!(!matches!(
        manager.get_qso(id).await.map(|p| p.state),
        Ok(QsoState::WaitingForConfirmation { .. })
    ));
}

#[tokio::test]
async fn aired_ladder_73_logs_once_with_the_queue_time_record() {
    let mut manager = manager(Arc::new(AtomicBool::new(true)));
    manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
    let (id, _) = reply(&manager, ResponseStep::ReportAck, None).await;
    let mut rx = manager.subscribe();
    let (_, advanced) = reply(&manager, ResponseStep::SeventyThree, None).await;
    let stored = manager.get_qso(id).await.unwrap();
    assert_eq!(completions(&drain(&mut rx)), 0);

    assert_eq!(
        air(&manager, id, advanced.attempt_id().unwrap()).await,
        CompletionAired::Committed
    );
    let events = drain(&mut rx);
    assert_eq!(completions(&events), 1);
    let metadata = events
        .iter()
        .find_map(|e| match e {
            QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(metadata.frequency, 14_074_000.0 + FREQ);
    assert_eq!(
        metadata.end_time, stored.metadata.end_time,
        "TIME_OFF is the queue time"
    );
    assert_eq!(metadata.reports.sent, stored.metadata.reports.sent);
    assert_eq!(
        air(&manager, id, advanced.attempt_id().unwrap()).await,
        CompletionAired::NotPending
    );
    assert_eq!(completions(&drain(&mut rx)), 0, "logged exactly once");
}

/// Drive a manual QSO to WaitingForConfirmation (our RR73 sent).
async fn manual_qso_awaiting_confirmation(manager: &QsoManager) -> QsoId {
    let id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    for (message, text) in [
        (
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -10,
            },
            format!("{OUR} {DX} -10"),
        ),
        (
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{OUR} {DX} R-07"),
        ),
    ] {
        manager
            .process_message(message, text, FREQ, Some(-15.0))
            .await
            .unwrap();
    }
    id
}

async fn dx_sends_rr73(manager: &QsoManager) {
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{OUR} {DX} RR73"),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
}

fn last_send(events: &[QsoEvent]) -> Option<(TxAttemptId, MessageType)> {
    events.iter().rev().find_map(|e| match e {
        QsoEvent::MessageToSend {
            attempt_id: Some(id),
            message,
            ..
        } => Some((*id, message.clone())),
        _ => None,
    })
}

#[tokio::test]
async fn dx_rr73_reply_73_is_held_until_aired() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let id = manual_qso_awaiting_confirmation(&manager).await;
    let mut rx = manager.subscribe();
    dx_sends_rr73(&manager).await;
    let events = drain(&mut rx);
    let (attempt, message) = last_send(&events).expect("our 73 is queued");
    assert!(matches!(message, MessageType::SeventyThree { .. }));
    assert_eq!(completions(&events), 0);
    assert_eq!(air(&manager, id, attempt).await, CompletionAired::Committed);
    assert_eq!(completions(&drain(&mut rx)), 1);
}

#[tokio::test]
async fn dx_rr73_reply_73_dropped_reopens_and_a_repeat_rr73_completes_again() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let id = manual_qso_awaiting_confirmation(&manager).await;
    let before = manager.get_qso(id).await.unwrap();
    let mut rx = manager.subscribe();
    dx_sends_rr73(&manager).await;
    let (attempt, _) = last_send(&drain(&mut rx)).unwrap();
    assert_eq!(
        manager
            .resolve_unsent_completion_at(id, Some(attempt), Utc::now())
            .await,
        UnsentCompletion::Reverted(RevertedTo::Reopened)
    );
    let after = manager.get_qso(id).await.unwrap();
    assert_eq!(after.state, before.state);
    // Our 73's Sent record is gone; the DX's received RR73 stays.
    let sent = |p: &QsoProgress| {
        p.messages
            .iter()
            .filter(|m| m.direction == MessageDirection::Sent)
            .count()
    };
    assert_eq!(sent(&after), sent(&before));
    assert_eq!(newest_sent_text(&after), newest_sent_text(&before));
    assert_eq!(completions(&drain(&mut rx)), 0);

    // The DX repeats RR73: the QSO completes again and queues a new 73.
    dx_sends_rr73(&manager).await;
    let events = drain(&mut rx);
    let (again, _) = last_send(&events).expect("a new 73 is queued");
    assert_ne!(again, attempt);
    assert_eq!(completions(&events), 0);
    assert_eq!(air(&manager, id, again).await, CompletionAired::Committed);
    assert_eq!(completions(&drain(&mut rx)), 1);
}

async fn pending_partners(manager: &QsoManager) -> Vec<String> {
    manager
        .active_qsos_and_pending_completion_partners()
        .await
        .1
}

/// PAN-287: a completion waiting on its closing frame names its partner
/// until that frame airs.
#[tokio::test]
async fn pending_completion_partners_lists_unaired_close_until_it_airs() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    assert!(pending_partners(&manager).await.is_empty());
    let (id, opened) = reply(&manager, ResponseStep::SeventyThree, None).await;
    let attempt = opened.attempt_id().expect("the open queued a 73");
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    let (active, partners) = manager.active_qsos_and_pending_completion_partners().await;
    assert!(active.is_empty(), "the Completed QSO is not active");
    assert_eq!(partners, vec![DX]);
    assert_eq!(air(&manager, id, attempt).await, CompletionAired::Committed);
    assert!(pending_partners(&manager).await.is_empty());
}

/// Guard: a completion that queues no closing frame (we sent RR73, the
/// DX's 73 closes it) logs at once, as before.
#[tokio::test]
async fn bare_73_completion_without_a_reply_logs_immediately() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let (id, _) = reply(&manager, ResponseStep::Rr73, None).await;
    let mut rx = manager.subscribe();
    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{OUR} {DX} 73"),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let events = drain(&mut rx);
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    assert!(last_send(&events).is_none(), "no closing frame is queued");
    assert_eq!(completions(&events), 1, "a frame-less completion logs now");
    assert!(manager
        .pending_completions
        .lock()
        .unwrap()
        .get(&id)
        .is_none());
}

/// Ticket scenario 3 (second half): an open at 73 whose 73 the TX worker
/// rejects is retired, never logged.
#[tokio::test]
async fn dropped_open_at_close_is_retired_close_not_aired() {
    let permitted = Arc::new(AtomicBool::new(true));
    let manager = manager(Arc::clone(&permitted));
    let mut rx = manager.subscribe();
    let (id, opened) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert_eq!(completions(&drain(&mut rx)), 0);
    permitted.store(false, SeqCst);
    assert_eq!(
        manager
            .resolve_unsent_completion_at(id, opened.attempt_id(), Utc::now())
            .await,
        UnsentCompletion::Reverted(RevertedTo::Retired)
    );
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Failed {
            reason: QsoFailureReason::CloseNotAired,
            ..
        }
    ));
    let events = drain(&mut rx);
    assert_eq!(completions(&events), 0);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                QsoEvent::QsoFailed {
                    reason: QsoFailureReason::CloseNotAired,
                    ..
                }
            ))
            .count(),
        1
    );
    // FIX B no longer matches the failed QSO: a new press opens a new one.
    permitted.store(true, SeqCst);
    let (new_id, again) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert_ne!(new_id, id);
    assert!(matches!(again, ManualCallOutcome::Opened(_)));
}

#[tokio::test]
async fn fix_b_rekey_while_pending_joins_the_pending_set() {
    for b_airs in [true, false] {
        let manager = manager(Arc::new(AtomicBool::new(true)));
        let mut rx = manager.subscribe();
        let (id, opened) = reply(&manager, ResponseStep::SeventyThree, None).await;
        let (rid, rekey) = reply(&manager, ResponseStep::SeventyThree, None).await;
        assert_eq!(rid, id);
        assert!(matches!(rekey, ManualCallOutcome::Resent(_)));
        let a = opened.attempt_id().unwrap();
        let b = rekey.attempt_id().unwrap();
        assert_eq!(
            manager
                .resolve_unsent_completion_at(id, Some(a), Utc::now())
                .await,
            UnsentCompletion::StillPending
        );
        if b_airs {
            assert_eq!(air(&manager, id, b).await, CompletionAired::Committed);
            assert_eq!(completions(&drain(&mut rx)), 1);
        } else {
            assert_eq!(
                manager
                    .resolve_unsent_completion_at(id, Some(b), Utc::now())
                    .await,
                UnsentCompletion::Reverted(RevertedTo::Retired)
            );
            assert_eq!(completions(&drain(&mut rx)), 0);
        }
    }
}

#[tokio::test]
async fn id_less_not_sent_never_reverts() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let mut rx = manager.subscribe();
    let (id, opened) = reply(&manager, ResponseStep::SeventyThree, None).await;
    assert_eq!(
        manager
            .resolve_unsent_completion_at(id, None, Utc::now())
            .await,
        UnsentCompletion::NotPending
    );
    assert_eq!(
        air(&manager, id, opened.attempt_id().unwrap()).await,
        CompletionAired::Committed
    );
    assert_eq!(completions(&drain(&mut rx)), 1);
}

#[tokio::test]
async fn aired_report_without_id_commits_on_content_match_only_after_queue() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let mut rx = manager.subscribe();
    let before_queue = Utc::now() - Duration::seconds(5);
    let (id, _) = reply(&manager, ResponseStep::SeventyThree, None).await;
    let text = format!("{DX} {OUR} 73");
    let confirm = |text: String, at: DateTime<Utc>| {
        let manager = &manager;
        async move {
            manager
                .confirm_completion_aired(id, None, &text, &payload(), at)
                .await
        }
    };
    assert_eq!(
        confirm("K9ZZ K5ARH RR73".into(), Utc::now()).await,
        CompletionAired::NotPending
    );
    assert_eq!(
        confirm(text.clone(), before_queue).await,
        CompletionAired::NotPending
    );
    assert_eq!(confirm(text, Utc::now()).await, CompletionAired::Committed);
    assert_eq!(completions(&drain(&mut rx)), 1);
}

#[tokio::test]
async fn pending_completion_expires_as_not_aired_per_mode() {
    for (slot_ns, expiry_s) in [
        (15_000_000_000_i64, 90),
        (7_500_000_000, 45),
        (3_200_000_000, 30),
    ] {
        let mut manager = manager(Arc::new(AtomicBool::new(true)));
        manager.set_active_slot_ns_source(Arc::new(AtomicI64::new(slot_ns)));
        let mut rx = manager.subscribe();
        let t0 = Utc::now();
        let (id, _) = reply(&manager, ResponseStep::SeventyThree, None).await;
        assert_eq!(
            manager
                .expire_pending_completions_at(t0 + Duration::seconds(expiry_s - 1))
                .await,
            0,
            "slot {slot_ns}"
        );
        assert_eq!(
            manager
                .expire_pending_completions_at(t0 + Duration::seconds(expiry_s + 1))
                .await,
            1,
            "slot {slot_ns}"
        );
        assert!(matches!(
            manager.get_qso(id).await.unwrap().state,
            QsoState::Failed {
                reason: QsoFailureReason::CloseNotAired,
                ..
            }
        ));
        assert_eq!(completions(&drain(&mut rx)), 0);
    }
}
