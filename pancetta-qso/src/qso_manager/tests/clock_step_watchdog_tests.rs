// PAN-296. Formerly the inline module `pan296_watchdog_tests`.
use super::*;
use pancetta_core::slot_clock::ClockStepDetector;
use std::time::{Duration as StdDuration, Instant};

fn manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        our_grid: Some("FN42".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: default_active_mode(),
        activation: crate::sig_reference::ActivationReference::default(),
    })
}

async fn open(manager: &QsoManager, call: &str, at: DateTime<Utc>) -> QsoId {
    let id = manager
        .respond_to_cq(call.into(), 14074000.0, None)
        .await
        .unwrap();
    let mut qsos = manager.qsos.write().await;
    let qso = qsos.get_mut(&id).unwrap();
    if let QsoState::RespondingToCq { started_at, .. } = &mut qso.state {
        *started_at = at;
    }
    qso.metadata.first_call_at = Some(at);
    qso.metadata.last_call_at = Some(at);
    id
}

fn failures(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoFailureReason> {
    let mut failures = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let QsoEvent::QsoFailed { reason, .. } = event {
            failures.push(reason);
        }
    }
    failures
}

#[tokio::test]
async fn three_qsos_survive_forward_step_then_expire_from_recovery() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let base = Utc::now();
    let mono = Instant::now();
    let mut last = mono;
    let mut detector = ClockStepDetector::new(base);
    let mut ids = Vec::new();
    for call in ["K1DEF", "K2DEF", "K3DEF"] {
        ids.push(open(&manager, call, base).await);
    }
    for (wall, elapsed) in [(20, 20), (80, 20), (85, 25), (110, 50)] {
        manager
            .watchdog_pass_at(
                base + Duration::seconds(wall),
                mono + StdDuration::from_secs(elapsed),
                &mut detector,
                &mut last,
            )
            .await;
        for id in &ids {
            assert!(manager.get_qso(*id).await.is_ok(), "retired at {wall}");
        }
        assert!(failures(&mut rx).is_empty());
    }
    manager
        .watchdog_pass_at(
            base + Duration::seconds(111),
            mono + StdDuration::from_secs(51),
            &mut detector,
            &mut last,
        )
        .await;
    for id in ids {
        assert!(manager.get_qso(id).await.is_err());
    }
    let reasons = failures(&mut rx);
    assert_eq!(reasons.len(), 3);
    assert!(reasons
        .iter()
        .all(|reason| matches!(reason, QsoFailureReason::Timeout)));
    assert!(manager.qsos_by_callsign.read().await.is_empty());
}

#[tokio::test]
async fn backward_and_repeated_steps_reanchor_but_ordinary_delay_expires() {
    let manager = manager();
    let base = Utc::now();
    let id = open(&manager, "K1DEF", base).await;
    let mono = Instant::now();
    let mut last = mono;
    let mut detector = ClockStepDetector::new(base);
    for (wall, elapsed) in [(20, 20), (-40, 20), (-10, 50), (90, 50), (120, 80)] {
        manager
            .watchdog_pass_at(
                base + Duration::seconds(wall),
                mono + StdDuration::from_secs(elapsed),
                &mut detector,
                &mut last,
            )
            .await;
        assert!(manager.get_qso(id).await.is_ok());
    }
    manager
        .watchdog_pass_at(
            base + Duration::seconds(160),
            mono + StdDuration::from_secs(120),
            &mut detector,
            &mut last,
        )
        .await;
    assert!(manager.get_qso(id).await.is_err());
}
async fn recover(manager: &QsoManager, base: DateTime<Utc>) {
    let mono = Instant::now();
    let mut last = mono;
    let mut detector = ClockStepDetector::new(base);
    manager
        .watchdog_pass_at(
            base + Duration::seconds(80),
            mono + StdDuration::from_secs(20),
            &mut detector,
            &mut last,
        )
        .await;
}

#[tokio::test]
async fn repetitive_and_manual_watchdogs_keep_their_boundaries() {
    for manual in [false, true] {
        let mut manager = manager();
        manager.config.timeouts.report_timeout = 1000;
        manager.config.timeouts.repetitive_tx_timeout_secs = if manual { 1000 } else { 30 };
        manager.config.timeouts.manual_call_watchdog_minutes = 1;
        let base = Utc::now();
        let id = open(&manager, "K1DEF", base).await;
        if manual {
            manager
                .qsos
                .write()
                .await
                .get_mut(&id)
                .unwrap()
                .metadata
                .initiated_by = CallInitiation::Manual;
        }
        recover(&manager, base).await;
        assert!(manager.get_qso(id).await.is_ok());
        let boundary = base + Duration::seconds(if manual { 140 } else { 110 });
        manager
            .check_timeouts_at(boundary - Duration::milliseconds(1))
            .await;
        assert!(manager.get_qso(id).await.is_ok());
        manager.check_timeouts_at(boundary).await;
        assert_eq!(manager.get_qso(id).await.is_ok(), !manual);
        manager
            .check_timeouts_at(boundary + Duration::seconds(1))
            .await;
        assert!(manager.get_qso(id).await.is_err());
    }
}

#[tokio::test]
async fn recovery_preserves_count_cap_and_unencodable_failures() {
    for unencodable in [false, true] {
        let manager = manager();
        let mut rx = manager.subscribe();
        let base = Utc::now();
        let id = open(&manager, "K1DEF", base).await;
        {
            let mut qsos = manager.qsos.write().await;
            let qso = qsos.get_mut(&id).unwrap();
            qso.metadata.initiated_by = CallInitiation::Manual;
            qso.metadata.call_count = manager.config.timeouts.manual_call_max_calls;
            if unencodable {
                qso.metadata.their_callsign = Some("WAYTOOLONGCALLSIGN".into());
            }
        }
        recover(&manager, base).await;
        assert!(manager.get_qso(id).await.is_err());
        let reasons = failures(&mut rx);
        assert_eq!(reasons.len(), 1);
        assert_eq!(
            matches!(&reasons[0], QsoFailureReason::MessageUnencodable(_)),
            unencodable
        );
    }
}

#[tokio::test]
async fn transitions_new_contacts_and_clones_use_the_right_anchors() {
    let mut manager = manager();
    manager.config.timeouts.repetitive_tx_timeout_secs = 1000;
    manager.config.timeouts.manual_call_watchdog_minutes = 1;
    let base = Utc::now();
    let auto = open(&manager, "K1DEF", base).await;
    let manual = open(&manager, "K2DEF", base).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&manual)
        .unwrap()
        .metadata
        .initiated_by = CallInitiation::Manual;
    let history = manager.get_qso(manual).await.unwrap().state_history;
    let clone = manager.clone();
    recover(&clone, base).await;
    let original = manager.get_qso(manual).await.unwrap();
    assert_eq!(original.metadata.first_call_at, Some(base));
    assert_eq!(original.state.state_duration(base), Some(Duration::zero()));
    assert_eq!(
        format!("{:?}", original.state_history),
        format!("{history:?}")
    );
    // A new state, including one with an unchanged started_at, invalidates
    // the old state identity. First-call grace survives both transitions.
    {
        let mut qsos = manager.qsos.write().await;
        for id in [auto, manual] {
            qsos.get_mut(&id).unwrap().state = QsoState::WaitingForReport {
                their_callsign: if id == auto { "K1DEF" } else { "K2DEF" }.into(),
                started_at: base + Duration::seconds(100),
                frequency: 14074000.0,
                their_grid: None,
                our_report: -10,
            };
        }
    }
    let new = open(&manager, "K3DEF", base + Duration::seconds(100)).await;
    manager
        .check_timeouts_at(base + Duration::seconds(130))
        .await;
    for id in [auto, manual, new] {
        assert!(manager.get_qso(id).await.is_ok());
    }
    clone.check_timeouts_at(base + Duration::seconds(131)).await;
    assert!(manager.get_qso(auto).await.is_err());
    assert!(manager.get_qso(new).await.is_err());
    assert!(manager.get_qso(manual).await.is_ok());
    clone.check_timeouts_at(base + Duration::seconds(140)).await;
    assert!(
        manager.get_qso(manual).await.is_err(),
        "first-call recovery must persist across a state advance"
    );
}

#[tokio::test]
async fn state_variant_and_first_call_identity_changes_discard_grace() {
    let manager = manager();
    let base = Utc::now();
    let id = open(&manager, "K1DEF", base).await;
    recover(&manager, base).await;
    manager.qsos.write().await.get_mut(&id).unwrap().state = QsoState::WaitingForReport {
        their_callsign: "K1DEF".into(),
        frequency: 14074000.0,
        started_at: base,
        their_grid: None,
        our_report: -10,
    };
    manager
        .check_timeouts_at(base + Duration::seconds(81))
        .await;
    assert!(
        manager.get_qso(id).await.is_err(),
        "variant change must invalidate grace even with the same start time"
    );
    assert!(manager.watchdog_anchors.lock().unwrap().is_empty());

    let id = open(&manager, "K2DEF", base).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .initiated_by = CallInitiation::Manual;
    recover(&manager, base).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .first_call_at = None;
    manager
        .check_timeouts_at(base + Duration::seconds(81))
        .await;
    assert!(
        manager.get_qso(id).await.is_ok(),
        "missing first call retains zero elapsed behavior"
    );
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .first_call_at = Some(base - Duration::minutes(10));
    manager
        .check_timeouts_at(base + Duration::seconds(82))
        .await;
    assert!(
        manager.get_qso(id).await.is_err(),
        "new first-call identity uses its own timestamp"
    );
}

#[tokio::test]
async fn contacts_created_between_observations_receive_grace_and_same_state_restart_expires() {
    let manager = manager();
    let base = Utc::now();
    let mono = Instant::now();
    let mut last = mono;
    let mut detector = ClockStepDetector::new(base);
    manager
        .watchdog_pass_at(
            base + Duration::seconds(20),
            mono + StdDuration::from_secs(20),
            &mut detector,
            &mut last,
        )
        .await;
    let id = open(&manager, "K1DEF", base + Duration::seconds(25)).await;
    manager
        .watchdog_pass_at(
            base + Duration::seconds(85),
            mono + StdDuration::from_secs(25),
            &mut detector,
            &mut last,
        )
        .await;
    assert!(manager.get_qso(id).await.is_ok());
    assert_eq!(
        manager
            .get_qso(id)
            .await
            .unwrap()
            .state
            .state_duration(base + Duration::seconds(25)),
        Some(Duration::zero())
    );
    if let QsoState::RespondingToCq { started_at, .. } =
        &mut manager.qsos.write().await.get_mut(&id).unwrap().state
    {
        *started_at = base + Duration::seconds(40);
    }
    manager
        .check_timeouts_at(base + Duration::seconds(86))
        .await;
    assert!(
        manager.get_qso(id).await.is_err(),
        "a changed start invalidates grace even in the same state variant"
    );
}
