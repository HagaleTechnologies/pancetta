// PAN-275. Formerly the inline module `pan275_completed_close_tests`.
//! PAN-275: the coordinator's auto-73 re-keys a COMPLETED manual QSO by
//! id through `resend_completed_close`, however long ago it completed,
//! and never opens a second QSO or emits a second `QsoCompleted`.
use super::*;
use jiff::SignedDuration;
use pancetta_core::slot::SlotParity;
use pancetta_core::time::UtcTimestamp;
use pancetta_core::ResponseStep;
use std::sync::atomic::AtomicBool;

const OUR: &str = "K5ARH";
const DX: &str = "DL1ABC";
const FREQ: f64 = 1500.0;

fn config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        ..Default::default()
    }
}

fn manager() -> QsoManager {
    QsoManager::new(config())
}

fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn sends_for(events: &[QsoEvent], id: QsoId) -> Vec<MessageType> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend {
                qso_id, message, ..
            } if *qso_id == id => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn count_sends(events: &[QsoEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, QsoEvent::MessageToSend { .. }))
        .count()
}

fn count_completed(events: &[QsoEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, QsoEvent::QsoCompleted { .. }))
        .count()
}

fn count_state_changes(events: &[QsoEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, QsoEvent::StateChanged { .. }))
        .count()
}

async fn complete_manual(
    manager: &QsoManager,
    remote_origin: bool,
    remote_client_key_id: Option<String>,
) -> QsoId {
    manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            ResponseStep::SeventyThree,
            Some(-10.0),
            Some(-12),
            None,
            remote_origin,
            remote_client_key_id,
        )
        .await
        .unwrap()
}

async fn call_count(manager: &QsoManager, id: QsoId) -> u32 {
    manager.get_qso(id).await.unwrap().metadata.call_count
}

#[tokio::test]
async fn resend_completed_close_past_the_grace_rekeys_the_same_qso() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let id = complete_manual(&manager, false, None).await;
    drain(&mut rx);
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    let before = call_count(&manager, id).await;
    assert!(manager.completed_close_rekeyable_now(id, FREQ).await);

    let outcome = manager.resend_completed_close(id, FREQ, false, None).await;
    assert!(
        matches!(outcome, ManualCallOutcome::Resent(_)),
        "{outcome:?}"
    );
    let events = drain(&mut rx);
    let sends = sends_for(&events, id);
    assert_eq!(sends.len(), 1, "exactly one MessageToSend for the QSO");
    assert!(
        matches!(sends[0], MessageType::SeventyThree { .. }),
        "the re-key carries the QSO's 73, got {:?}",
        sends[0]
    );
    assert_eq!(count_sends(&events), 1, "no frame for any other QSO");
    assert_eq!(count_completed(&events), 0, "no second QsoCompleted");
    assert_eq!(count_state_changes(&events), 0, "no state change");
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    assert_eq!(call_count(&manager, id).await, before + 1);

    // Control: the operator path past the grace still opens a new QSO.
    let (new_id, outcome) = manager
        .respond_to_caller_with_outcome(
            DX.into(),
            FREQ,
            Some(SlotParity::Even),
            ResponseStep::SeventyThree,
            Some(-10.0),
            Some(-12),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ManualCallOutcome::Opened(_)),
        "{outcome:?}"
    );
    assert_ne!(new_id, id);
}

/// Codex P2 on PR #588: a QSO that completed on a plain incoming 73 sent
/// no 73, so its last sent frame is a report. The re-key past the grace
/// still sends a 73 to the QSO's station, not that report.
#[tokio::test]
async fn resend_completed_close_sends_a_73_when_the_last_sent_frame_is_a_report() {
    let manager = manager();
    let id = complete_manual(&manager, false, None).await;
    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&id).unwrap();
        progress
            .messages
            .retain(|m| m.direction != MessageDirection::Sent);
        progress.messages.push(QsoMessage {
            timestamp: UtcTimestamp::now(),
            direction: MessageDirection::Sent,
            message_type: MessageType::ReportAck {
                to_station: DX.into(),
                from_station: OUR.into(),
                report: -10,
            },
            raw_text: format!("{DX} {OUR} R-10"),
            signal_strength: None,
            frequency: FREQ,
        });
    }
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    let mut rx = manager.subscribe();

    let outcome = manager.resend_completed_close(id, FREQ, false, None).await;
    assert!(
        matches!(outcome, ManualCallOutcome::Resent(_)),
        "{outcome:?}"
    );
    let sends = sends_for(&drain(&mut rx), id);
    assert_eq!(
        sends,
        vec![MessageType::SeventyThree {
            to_station: DX.into(),
            from_station: OUR.into(),
        }],
        "the auto-73 re-key sends a 73, never the last sent report"
    );
}

/// The re-key addresses its 73 like the exchange's own 73: to the state's
/// call, not to the logged call a C18 compound upgrade lengthened.
#[tokio::test]
async fn resend_completed_close_addresses_the_73_to_the_worked_call() {
    let manager = manager();
    let id = complete_manual(&manager, false, None).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .their_callsign = Some(format!("EA8/{DX}"));
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    let mut rx = manager.subscribe();

    let outcome = manager.resend_completed_close(id, FREQ, false, None).await;
    assert!(
        matches!(outcome, ManualCallOutcome::Resent(_)),
        "{outcome:?}"
    );
    assert_eq!(
        sends_for(&drain(&mut rx), id),
        vec![MessageType::SeventyThree {
            to_station: DX.into(),
            from_station: OUR.into(),
        }]
    );
}

async fn assert_refused(manager: &QsoManager, id: QsoId, freq: f64) {
    let mut rx = manager.subscribe();
    let before = manager
        .get_qso(id)
        .await
        .ok()
        .map(|p| p.metadata.call_count);
    assert!(
        !manager.completed_close_rekeyable_now(id, freq).await,
        "the read-only preview agrees with the refusal"
    );
    let outcome = manager.resend_completed_close(id, freq, false, None).await;
    assert_eq!(outcome, ManualCallOutcome::NothingToResend);
    let events = drain(&mut rx);
    assert_eq!(count_sends(&events), 0, "a refused re-key emits nothing");
    assert_eq!(count_completed(&events), 0);
    let after = manager
        .get_qso(id)
        .await
        .ok()
        .map(|p| p.metadata.call_count);
    assert_eq!(before, after, "a refused re-key charges nothing");
}

#[tokio::test]
async fn resend_completed_close_refuses_what_it_must_not_rekey() {
    // An unknown id.
    let manager = manager();
    assert_refused(&manager, Uuid::new_v4(), FREQ).await;

    // An ACTIVE manual QSO.
    let manager = self::manager();
    let active = manager
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Even))
        .await
        .unwrap();
    assert!(manager.get_qso(active).await.unwrap().state.is_active());
    assert_refused(&manager, active, FREQ).await;

    // A completed AUTONOMOUS QSO.
    let manager = self::manager();
    let id = complete_manual(&manager, false, None).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .initiated_by = CallInitiation::Auto;
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    assert_refused(&manager, id, FREQ).await;

    // A completed Hound QSO.
    let manager = self::manager();
    let id = complete_manual(&manager, false, None).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .hound = true;
    assert_refused(&manager, id, FREQ).await;

    // A completed QSO on ANOTHER band than the dial maps to now.
    let dial = Arc::new(AtomicU64::new(14_074_000));
    let mut manager = self::manager();
    manager.set_dial_frequency_source(Arc::clone(&dial));
    let id = complete_manual(&manager, false, None).await;
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    dial.store(7_074_000, Ordering::Relaxed);
    assert_refused(&manager, id, FREQ).await;
    // Same QSO, back on its own band: it re-keys.
    dial.store(14_074_000, Ordering::Relaxed);
    assert!(matches!(
        manager.resend_completed_close(id, FREQ, false, None).await,
        ManualCallOutcome::Resent(_)
    ));
}

#[tokio::test]
async fn resend_completed_close_denied_remote_client_charges_nothing() {
    let permits = Arc::new(AtomicBool::new(true));
    let mut manager = manager();
    let p = Arc::clone(&permits);
    manager.set_remote_tx_permitted_source(Arc::new(move |_| p.load(Ordering::SeqCst)));
    let id = complete_manual(&manager, true, Some("client-a".into())).await;
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    let before = call_count(&manager, id).await;
    permits.store(false, Ordering::SeqCst);
    let mut rx = manager.subscribe();

    let outcome = manager
        .resend_completed_close(id, FREQ, true, Some("client-a".into()))
        .await;
    assert_eq!(outcome, ManualCallOutcome::NotPermitted);
    let events = drain(&mut rx);
    assert_eq!(count_sends(&events), 0);
    assert_eq!(call_count(&manager, id).await, before);
    assert_eq!(
        manager
            .get_qso(id)
            .await
            .unwrap()
            .metadata
            .remote_client_key_id
            .as_deref(),
        Some("client-a")
    );
}

#[tokio::test]
async fn backdate_completion_for_test_moves_only_completed_qsos() {
    let manager = manager();
    assert!(
        !manager
            .backdate_completion_for_test(Uuid::new_v4(), SignedDuration::from_secs(60))
            .await
    );

    let other = self::manager();
    let active = other
        .respond_to_cq_manual(DX.into(), FREQ, Some(SlotParity::Even))
        .await
        .unwrap();
    let before = other.get_qso(active).await.unwrap();
    assert!(
        !other
            .backdate_completion_for_test(active, SignedDuration::from_secs(60))
            .await
    );
    let after = other.get_qso(active).await.unwrap();
    assert_eq!(format!("{:?}", before.state), format!("{:?}", after.state));

    let id = complete_manual(&manager, false, None).await;
    let completed_at = |p: QsoProgress| match p.state {
        QsoState::Completed { completed_at, .. } => completed_at,
        other => panic!("expected Completed, got {other:?}"),
    };
    let original = completed_at(manager.get_qso(id).await.unwrap());
    assert_eq!(
        manager
            .find_recently_completed_manual_qso_for(DX, FREQ, COMPLETED_QSO_REWORK_GRACE)
            .await,
        Some(id)
    );
    assert!(
        manager
            .backdate_completion_for_test(id, SignedDuration::from_secs(60))
            .await
    );
    let moved = completed_at(manager.get_qso(id).await.unwrap());
    assert_eq!(original - moved, SignedDuration::from_secs(60));
    assert_eq!(
        manager
            .find_recently_completed_manual_qso_for(DX, FREQ, COMPLETED_QSO_REWORK_GRACE)
            .await,
        None
    );
}
