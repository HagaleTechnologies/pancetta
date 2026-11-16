//! PAN-138: `respond_to_caller_with_outcome` reports whether a frame was
//! emitted, so a caller that charged its own budget first (auto-73) can
//! roll that charge back.
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn manager(permitted: Arc<AtomicBool>) -> QsoManager {
    let mut manager = QsoManager::new(QsoManagerConfig {
        our_callsign: "K5ARH".into(),
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
            None,
            None,
            None,
            remote.is_some(),
            remote.map(str::to_string),
        )
        .await
        .unwrap()
}

fn sends(rx: &mut broadcast::Receiver<QsoEvent>) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        n += usize::from(matches!(ev, QsoEvent::MessageToSend { .. }));
    }
    n
}

/// The attempt id on the newest `MessageToSend` the receiver holds.
fn last_send_attempt(rx: &mut broadcast::Receiver<QsoEvent>) -> Option<TxAttemptId> {
    let mut last = None;
    while let Ok(ev) = rx.try_recv() {
        if let QsoEvent::MessageToSend { attempt_id, .. } = ev {
            last = attempt_id;
        }
    }
    last
}

#[tokio::test]
async fn outcome_names_what_each_branch_did() {
    let permitted = Arc::new(AtomicBool::new(true));
    let manager = manager(Arc::clone(&permitted));
    let (id, opened) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    assert!(matches!(opened, ManualCallOutcome::Opened(_)));
    let (rid, resent) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    assert_eq!(rid, id);
    assert!(matches!(resent, ManualCallOutcome::Resent(_)));
    let (aid, advanced) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert_eq!(aid, id);
    assert!(matches!(advanced, ManualCallOutcome::Advanced(_)));
    let (fid, fix_b) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert_eq!(fid, id);
    assert!(
        matches!(fix_b, ManualCallOutcome::Resent(_)),
        "FIX B re-keys the completed QSO"
    );
    assert!(ManualCallOutcome::Resent(TxAttemptId(1)).emitted_frame());
    assert!(!ManualCallOutcome::NotPermitted.emitted_frame());
    assert!(!ManualCallOutcome::NothingToResend.emitted_frame());
}

/// PAN-280: every frame-emitting outcome carries the attempt id of the
/// `MessageToSend` it emitted, so the coordinator can match that frame's
/// later TX report (the auto-73 refund) to the call that charged it.
#[tokio::test]
async fn manual_call_outcome_carries_the_emitted_attempt_id() {
    let permitted = Arc::new(AtomicBool::new(true));
    let manager = manager(Arc::clone(&permitted));
    let mut rx = manager.subscribe();
    let (id, opened) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    assert!(matches!(opened, ManualCallOutcome::Opened(_)));
    assert_eq!(opened.attempt_id(), last_send_attempt(&mut rx));
    let (_, resent) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    assert!(matches!(resent, ManualCallOutcome::Resent(_)));
    assert_eq!(resent.attempt_id(), last_send_attempt(&mut rx));
    let (_, advanced) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert!(matches!(advanced, ManualCallOutcome::Advanced(_)));
    assert_eq!(advanced.attempt_id(), last_send_attempt(&mut rx));
    let (_, fix_b) = reply(&manager, ResponseStep::SeventyThree, Some("client-a")).await;
    assert!(matches!(fix_b, ManualCallOutcome::Resent(_)));
    assert_eq!(fix_b.attempt_id(), last_send_attempt(&mut rx));
    let ids = [opened, resent, advanced, fix_b].map(|o| o.attempt_id().unwrap());
    for (i, a) in ids.iter().enumerate() {
        assert!(!ids[i + 1..].contains(a), "attempt ids are never reused");
    }
    permitted.store(false, SeqCst);
    let (denied_id, denied) = reply(&manager, ResponseStep::SeventyThree, Some("client-b")).await;
    assert_eq!(denied_id, id);
    assert_eq!(denied, ManualCallOutcome::NotPermitted);
    assert_eq!(denied.attempt_id(), None);
    assert_eq!(ManualCallOutcome::NothingToResend.attempt_id(), None);
}

#[tokio::test]
async fn a_denied_rekey_reports_not_permitted_and_changes_nothing() {
    for step in [ResponseStep::Report, ResponseStep::SeventyThree] {
        let permitted = Arc::new(AtomicBool::new(true));
        let manager = manager(Arc::clone(&permitted));
        // Report: idempotent re-send of an active QSO; SeventyThree: FIX B
        // re-key of the QSO this open just completed.
        let (id, _) = reply(&manager, step, Some("client-a")).await;
        let before = manager.get_qso(id).await.unwrap();
        let mut rx = manager.subscribe();
        permitted.store(false, SeqCst);
        assert_eq!(
            reply(&manager, step, Some("client-b")).await,
            (id, ManualCallOutcome::NotPermitted)
        );
        let after = manager.get_qso(id).await.unwrap();
        assert_eq!(after.metadata.call_count, before.metadata.call_count);
        assert_eq!(after.metadata.last_call_at, before.metadata.last_call_at);
        assert_eq!(
            after.metadata.remote_client_key_id.as_deref(),
            Some("client-a")
        );
        assert_eq!(sends(&mut rx), 0);
    }
}

#[tokio::test]
async fn a_disarm_while_waiting_for_the_qsos_lock_is_honoured() {
    let permitted = Arc::new(AtomicBool::new(true));
    let manager = Arc::new(manager(Arc::clone(&permitted)));
    let (id, _) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    let mut rx = manager.subscribe();
    // Concurrent QSO processing holds the lock; the client disarms
    // before it lets go.
    let busy = Arc::clone(&manager.qsos).write_owned().await;
    let m = Arc::clone(&manager);
    let task =
        tokio::spawn(async move { reply(&m, ResponseStep::SeventyThree, Some("client-a")).await });
    tokio::task::yield_now().await;
    permitted.store(false, SeqCst);
    drop(busy);
    assert_eq!(task.await.unwrap(), (id, ManualCallOutcome::NotPermitted));
    let after = manager.get_qso(id).await.unwrap();
    assert!(matches!(after.state, QsoState::SendingReport { .. }));
    assert_eq!(sends(&mut rx), 0);
}

#[tokio::test]
async fn a_local_rekey_ignores_the_remote_predicate() {
    let permitted = Arc::new(AtomicBool::new(false));
    let manager = manager(permitted);
    let (id, _) = reply(&manager, ResponseStep::Report, None).await;
    let (rid, outcome) = reply(&manager, ResponseStep::Report, None).await;
    assert_eq!(rid, id);
    assert!(matches!(outcome, ManualCallOutcome::Resent(_)));
}

#[tokio::test]
async fn a_qso_with_nothing_sent_reports_nothing_to_resend() {
    let manager = manager(Arc::new(AtomicBool::new(true)));
    let (id, _) = reply(&manager, ResponseStep::Report, Some("client-a")).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .messages
        .clear();
    assert_eq!(
        reply(&manager, ResponseStep::Report, Some("client-a")).await,
        (id, ManualCallOutcome::NothingToResend)
    );
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.call_count, 1);
}
