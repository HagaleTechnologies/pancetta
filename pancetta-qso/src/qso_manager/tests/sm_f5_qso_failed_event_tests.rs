//! SM-F5 (Batch 4): `QsoEvent::QsoFailed` was never emitted by `cancel_qso`,
//! `check_timeouts_at`'s retirement loop, or `supersede_active_qsos_for` —
//! only `StateChanged → Failed`. The coordinator's priority-scoring
//! failure backoff (`record_failure`) subscribes to `QsoFailed`
//! specifically, so this dropped the backoff signal entirely for every
//! failure path. This module proves all three producer sites now emit
//! `QsoFailed` with the correct reason and non-empty metadata.
use super::*;

const OUR: &str = "W1ABC"; // test_config()'s our_callsign
const DX: &str = "K1DEF";
const FREQ: f64 = 14074000.0;

fn test_config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: OUR.to_string(),
        our_grid: Some("FN42".to_string()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: default_active_mode(),
        activation: crate::sig_reference::ActivationReference::default(),
    }
}

fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn qso_failed_events(events: &[QsoEvent]) -> Vec<(QsoId, QsoFailureReason, QsoMetadata)> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::QsoFailed {
                qso_id,
                reason,
                metadata,
                ..
            } => Some((*qso_id, reason.clone(), metadata.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn cancel_qso_emits_qso_failed_with_user_cancelled() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    let _ = drain(&mut rx);

    manager.cancel_qso(qso_id).await.unwrap();

    let events = drain(&mut rx);
    // Both surfaces must fire: StateChanged -> Failed (existing) AND the
    // revived QsoFailed (SM-F5).
    assert!(
        events.iter().any(|e| matches!(
            e,
            QsoEvent::StateChanged {
                new_state: QsoState::Failed { .. },
                ..
            }
        )),
        "expected a StateChanged -> Failed, got {events:?}"
    );
    let failed = qso_failed_events(&events);
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one QsoFailed, got {failed:?}"
    );
    let (failed_id, reason, metadata) = &failed[0];
    assert_eq!(*failed_id, qso_id);
    assert_eq!(*reason, QsoFailureReason::UserCancelled);
    assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
}

#[tokio::test]
async fn fail_qso_emits_qso_failed_with_the_given_reason() {
    // Task 6 (task-supervision): the coordinator's task supervisor calls
    // `fail_qso` (not `cancel_qso`, which is hardcoded to
    // UserCancelled) to surface a QSO dropped by a Qso-component
    // restart as SupervisorRestart specifically.
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    let _ = drain(&mut rx);

    manager
        .fail_qso(qso_id, QsoFailureReason::SupervisorRestart)
        .await
        .unwrap();

    let events = drain(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            QsoEvent::StateChanged {
                new_state: QsoState::Failed { .. },
                ..
            }
        )),
        "expected a StateChanged -> Failed, got {events:?}"
    );
    let failed = qso_failed_events(&events);
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one QsoFailed, got {failed:?}"
    );
    let (failed_id, reason, metadata) = &failed[0];
    assert_eq!(*failed_id, qso_id);
    assert_eq!(*reason, QsoFailureReason::SupervisorRestart);
    assert_eq!(metadata.their_callsign.as_deref(), Some(DX));

    // The QSO must actually leave the active map -- otherwise it would
    // both show as "dropped" in Recent-QSOs AND still be sitting in
    // get_active_qsos() for a subsequent supervisor pass to double-fail.
    assert!(manager.get_qso(qso_id).await.is_err());
}

#[tokio::test]
async fn check_timeouts_at_retirement_emits_qso_failed_with_timeout() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    let _ = drain(&mut rx);

    // Past the 30s report_timeout, past the one bounded Auto resend cap
    // too, so this retires cleanly via the timeout path.
    manager
        .check_timeouts_at(opened_at + Duration::seconds(45))
        .await;

    let events = drain(&mut rx);
    let failed = qso_failed_events(&events);
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one QsoFailed, got {failed:?}"
    );
    let (failed_id, reason, metadata) = &failed[0];
    assert_eq!(*failed_id, qso_id);
    assert_eq!(*reason, QsoFailureReason::Timeout);
    assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
}

/// Layer 2 timeline persistence: `check_timeouts_at`'s retirement loop is
/// the single most common "QSO leaves the active map" site (every
/// watchdog/timeout retirement runs through it) and used to just drop
/// `progress` — state_history and messages included — on the floor after
/// cloning out `metadata`. The `QsoFailed` event it emits must now carry
/// the real timeline instead.
#[tokio::test]
async fn check_timeouts_at_retirement_qso_failed_event_carries_timeline() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    manager
        .check_timeouts_at(opened_at + Duration::seconds(45))
        .await;

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    let (last_state, state_history, messages) = events
        .into_iter()
        .find_map(|e| match e {
            QsoEvent::QsoFailed {
                qso_id: id,
                last_state,
                state_history,
                messages,
                ..
            } if id == qso_id => Some((last_state, state_history, messages)),
            _ => None,
        })
        .expect("expected a QsoFailed event for this qso_id");

    // `respond_to_cq` records the opening CqResponse reply as a Sent
    // message at construction, so this must be non-empty — the old
    // hard-coded discard would have made this an empty vec regardless
    // of what the live QSO actually held.
    assert!(
        !messages.is_empty(),
        "QsoFailed must carry the QSO's real message history, not an empty vec"
    );
    // This particular QSO times out before any `process_message`-driven
    // forward advance, so 0 transitions is the CORRECT real value here
    // (not a sign of discard) — the type-check that `state_history`
    // exists on the event at all is what the earlier compile-time
    // wiring enforces.
    assert_eq!(state_history, Vec::<StateTransition>::new());
    assert!(
        matches!(last_state, QsoState::RespondingToCq { .. }),
        "QsoFailed must preserve the operational state even with empty history"
    );
}

#[tokio::test]
async fn supersede_active_qsos_for_emits_qso_failed_with_superseded() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id_1 = manager
        .respond_to_cq_manual(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    let _ = drain(&mut rx);

    // Drive the producer directly (FIX 3's own entry point) — a fresh
    // `respond_to_cq_manual` for the SAME (callsign, band) instead hits the
    // idempotent re-call branch (`find_active_manual_qso_for`) while the
    // first QSO is still active, which is a separate code path from the
    // genuine supersede this test targets.
    manager.supersede_active_qsos_for(DX, FREQ).await;

    let events = drain(&mut rx);
    let failed = qso_failed_events(&events);
    assert_eq!(
        failed.len(),
        1,
        "expected exactly one QsoFailed for the superseded QSO, got {failed:?}"
    );
    let (failed_id, reason, metadata) = &failed[0];
    assert_eq!(
        *failed_id, qso_id_1,
        "the OLDER qso must be the one superseded"
    );
    assert_eq!(*reason, QsoFailureReason::Superseded);
    assert_eq!(metadata.their_callsign.as_deref(), Some(DX));
}
