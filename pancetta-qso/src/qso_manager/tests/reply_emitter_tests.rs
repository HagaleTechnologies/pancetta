//! Auto-sequence reply emitter for MANUAL QSOs.
//!
//! Drives a manual QSO through the full inbound exchange and asserts the
//! outbound `MessageToSend` replies (R-report → RR73 → 73) are emitted,
//! that the QSO completes + logs, and that autonomous QSOs do NOT
//! auto-reply.
use super::*;
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;

const OUR: &str = "K5ARH";
const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn manager() -> QsoManager {
    let config = QsoManagerConfig {
        our_callsign: OUR.into(),
        our_grid: Some("EM12".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
        activation: crate::sig_reference::ActivationReference::default(),
    };
    QsoManager::new(config)
}

/// Drain currently-buffered events into a Vec.
fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn messages_to_send(events: &[QsoEvent]) -> Vec<MessageType> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// On-air text the coordinator would generate for an emitted reply.
fn on_air(msg: &MessageType) -> String {
    crate::utils::generate_ft8_message(msg, OUR).unwrap()
}

/// PAN-280: report the newest frame emitted among `events` as aired, as
/// the TX worker's `TxFrameSent` does, and return `events` plus what that
/// produced (a held `QsoCompleted`).
async fn air_last_send(
    manager: &QsoManager,
    rx: &mut broadcast::Receiver<QsoEvent>,
    mut events: Vec<QsoEvent>,
) -> Vec<QsoEvent> {
    let last = events.iter().rev().find_map(|e| match e {
        QsoEvent::MessageToSend {
            qso_id,
            frequency,
            attempt_id: Some(id),
            ..
        } => Some((*qso_id, *id, *frequency)),
        _ => None,
    });
    if let Some((qso_id, id, frequency)) = last {
        manager
            .confirm_completion_aired(
                qso_id,
                Some(id),
                "",
                &UnsentAttemptPayload {
                    frequency_hz: frequency,
                    remote_binding: None,
                },
                UtcTimestamp::now(),
            )
            .await;
        events.extend(drain(rx));
    }
    events
}

/// 1. Manual QSO in RespondingToCq + SignalReport → emits ReportAck
///    (R+report) and state advances to SendingReport.
#[tokio::test]
async fn manual_signal_report_emits_report_ack() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let _ = drain(&mut rx); // discard the initial CqResponse call

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let events = drain(&mut rx);
    let sends = messages_to_send(&events);
    assert_eq!(
        sends.len(),
        1,
        "expected exactly one reply, got {:?}",
        sends
    );
    match &sends[0] {
        MessageType::ReportAck {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
            // snr -15 → our report -15 (matches SendingReport.our_report).
            assert_eq!(*report, -15);
        }
        other => panic!("expected ReportAck, got {:?}", other),
    }
    assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH R-15");

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "expected SendingReport, got {:?}",
        progress.state
    );
}

/// 2. Manual QSO in SendingReport + ReportAck → emits FinalConfirmation
///    (RR73) and state advances to WaitingForConfirmation.
#[tokio::test]
async fn manual_report_ack_emits_final_confirmation() {
    let manager = manager();
    let mut rx = manager.subscribe();
    manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // Advance to SendingReport.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);

    manager
        .process_message(
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} R-07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1, "expected one reply, got {:?}", sends);
    match &sends[0] {
        MessageType::FinalConfirmation {
            to_station,
            from_station,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
        }
        other => panic!("expected FinalConfirmation, got {:?}", other),
    }
    assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH RR73");
}

/// 3. Manual QSO in WaitingForConfirmation + FinalConfirmation → emits
///    SeventyThree (73), QSO → Completed, and a QsoCompleted event fires
///    (so the ADIF logger logs it).
#[tokio::test]
async fn manual_final_confirmation_emits_73_and_completes() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} R-07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);

    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} RR73", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let sends = messages_to_send(&events);
    assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
    match &sends[0] {
        MessageType::SeventyThree {
            to_station,
            from_station,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
        }
        other => panic!("expected SeventyThree, got {:?}", other),
    }
    assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

    // QSO completed.
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        progress.state
    );
    // QsoCompleted event fired (drives ADIF logging).
    assert!(
        events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "expected a QsoCompleted event"
    );
}

/// FIX 2: Manual QSO in SendingReport + RR73 (FinalConfirmation) → the
/// DX rogered our R-report directly. We emit our 73, the QSO completes,
/// and a QsoCompleted event fires (drives ADIF logging). This is the
/// "never sent 73 / QSO stalled one message short" bug.
#[tokio::test]
async fn manual_sending_report_plus_rr73_emits_73_and_completes() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // Advance to SendingReport (DX sent their report; we send R-report).
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::SendingReport { .. }));
    let _ = drain(&mut rx);

    // DX closes directly with RR73 (skips a separate RRR/report-ack).
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} RR73", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let sends = messages_to_send(&events);
    assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
    assert!(
        matches!(sends[0], MessageType::SeventyThree { .. }),
        "expected SeventyThree, got {:?}",
        sends[0]
    );
    assert_eq!(on_air(&sends[0]), "K9ZZ K5ARH 73");

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        progress.state
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "expected a QsoCompleted event (ADIF log)"
    );
}

/// A completed QSO logs the RF frequency (dial + audio offset), not the
/// bare offset, when a dial-frequency source is shared. Regression for the
/// ADIF FREQ ~0.001 / BAND 0MHZ bug.
#[tokio::test]
async fn completed_metadata_logs_dial_plus_offset() {
    let mut manager = manager();
    manager.set_dial_frequency_source(Arc::new(AtomicU64::new(14_074_000)));
    let mut rx = manager.subscribe();
    let _qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} RR73", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let completed = events.iter().find_map(|e| match e {
        QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
        _ => None,
    });
    let metadata = completed.expect("expected a QsoCompleted event");
    // dial 14_074_000 + offset 1500 = 14_075_500 Hz (20m).
    assert_eq!(metadata.frequency, 14_074_000.0 + FREQ);
}

/// Without a dial source (e.g. unit tests / no rig), completed metadata
/// keeps the value it was created with — no spurious offset added.
#[tokio::test]
async fn completed_metadata_unchanged_without_dial_source() {
    let manager = manager();
    let mut rx = manager.subscribe();
    manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} RR73", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let metadata = events
        .iter()
        .find_map(|e| match e {
            QsoEvent::QsoCompleted { metadata, .. } => Some(metadata.clone()),
            _ => None,
        })
        .expect("expected a QsoCompleted event");
    assert_eq!(metadata.frequency, FREQ);
}

// --- respond_to_caller: open the exchange at a chosen ResponseStep ----

use pancetta_core::ResponseStep;

/// `Grid` opens exactly like the historical manual call: state
/// `RespondingToCq` and a first message of `CqResponse` carrying our grid.
#[tokio::test]
async fn respond_to_caller_grid_matches_legacy_manual() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Grid,
            Some(-12.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    match &sends[0] {
        MessageType::CqResponse {
            calling_station,
            responding_station,
            grid,
        } => {
            assert_eq!(calling_station, DX);
            assert_eq!(responding_station, OUR);
            assert_eq!(grid.as_deref(), Some("EM12"));
        }
        other => panic!("expected CqResponse, got {other:?}"),
    }
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
    assert_eq!(progress.metadata.initiated_by, CallInitiation::Manual);
}

/// `Report` opens at state `SendingReport` (their_report None) and a first
/// message of `SignalReport` carrying the report derived from our SNR.
#[tokio::test]
async fn respond_to_caller_report_emits_signal_report() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Report,
            Some(-9.0),
            None,
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    match &sends[0] {
        MessageType::SignalReport {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
            assert_eq!(*report, -9);
        }
        other => panic!("expected SignalReport, got {other:?}"),
    }
    let progress = manager.get_qso(qso_id).await.unwrap();
    match progress.state {
        QsoState::SendingReport {
            their_report,
            our_report,
            ..
        } => {
            assert_eq!(their_report, None);
            assert_eq!(our_report, -9);
        }
        other => panic!("expected SendingReport, got {other:?}"),
    }
}

/// `ReportAck` opens at state `SendingReport` (their_report Some) and a
/// first message of `ReportAck` (R-report).
#[tokio::test]
async fn respond_to_caller_report_ack_emits_report_ack() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::ReportAck,
            Some(-10.0),
            Some(-3),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    match &sends[0] {
        MessageType::ReportAck {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
            assert_eq!(*report, -10);
        }
        other => panic!("expected ReportAck, got {other:?}"),
    }
    let progress = manager.get_qso(qso_id).await.unwrap();
    match progress.state {
        QsoState::SendingReport { their_report, .. } => {
            assert_eq!(their_report, Some(-3));
        }
        other => panic!("expected SendingReport, got {other:?}"),
    }
}

/// `Rr73` opens at state `WaitingForConfirmation` and a first message of
/// `FinalConfirmation` (RR73).
#[tokio::test]
async fn respond_to_caller_rr73_emits_final_confirmation() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Rr73,
            Some(-5.0),
            Some(-7),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    assert!(
        matches!(sends[0], MessageType::FinalConfirmation { .. }),
        "expected FinalConfirmation, got {:?}",
        sends[0]
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(
        progress.state,
        QsoState::WaitingForConfirmation { .. }
    ));
}

/// `SeventyThree` opens directly at `Completed`, emits a `SeventyThree`
/// first message AND a `QsoCompleted` event so the QSO is logged.
#[tokio::test]
async fn respond_to_caller_seventy_three_completes_and_logs() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let sends = messages_to_send(&events);
    assert_eq!(sends.len(), 1);
    assert!(
        matches!(sends[0], MessageType::SeventyThree { .. }),
        "expected SeventyThree, got {:?}",
        sends[0]
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "expected a QsoCompleted event (ADIF log)"
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(progress.state, QsoState::Completed { .. }));
}

/// PAN-139 (Codex round 23 on PR #362): a brand-new QSO opened directly
/// at `Rr73` by a remote client the identity-bound TX gate denies must
/// be refused BEFORE any mutation — no `WaitingForConfirmation` QSO
/// created, no phantom `Sent` `FinalConfirmation` recorded, no
/// `MessageToSend` emitted. On current main the open unconditionally
/// mutated first: the TX worker correctly dropped the unauthorized frame
/// at key-time, but the QSO then sat in `WaitingForConfirmation` — a
/// state `rearm_manual_calls_at` deliberately never retries (`_ =>
/// continue`) — until `confirmation_timeout` retired it as a silently
/// lost QSO.
#[tokio::test]
async fn denied_remote_rr73_open_is_refused_before_any_mutation() {
    let mut manager = manager();
    // Simulate the TX worker's identity-bound arm gate: client-A is NOT
    // currently TX-permitted (control transferred to another client).
    manager.set_remote_tx_permitted_source(Arc::new(|client_key_id: Option<&str>| {
        client_key_id != Some("client-A")
    }));
    let mut rx = manager.subscribe();

    let err = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Rr73,
            Some(-5.0),
            Some(-7),
            None, // partner_freq
            true, // remote_origin
            Some("client-A".into()),
        )
        .await
        .expect_err("a denied remote Rr73 open must be refused, not mutated");
    assert!(
        err.to_string().contains("not currently TX-permitted"),
        "expected the denial to name the TX-permission failure, got {err:?}"
    );

    let events = drain(&mut rx);
    assert!(
        messages_to_send(&events).is_empty(),
        "nothing may be emitted for a refused open, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::StateChanged { .. })),
        "no QSO may be created (or superseded) for a refused open, got {events:?}"
    );
}

/// PAN-139: the same refusal pins round-15's original `SeventyThree`
/// guard, which shipped with no test of its own. A brand-new QSO opened
/// directly at `SeventyThree` by a denied remote client must be refused
/// before any mutation — completing instead would emit an un-retractable
/// `QsoCompleted` (ADIF log entry).
#[tokio::test]
async fn denied_remote_seventythree_open_is_refused_before_any_mutation() {
    let mut manager = manager();
    manager.set_remote_tx_permitted_source(Arc::new(|client_key_id: Option<&str>| {
        client_key_id != Some("client-A")
    }));
    let mut rx = manager.subscribe();

    let err = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None, // partner_freq
            true, // remote_origin
            Some("client-A".into()),
        )
        .await
        .expect_err("a denied remote SeventyThree open must be refused, not mutated");
    assert!(
        err.to_string().contains("not currently TX-permitted"),
        "expected the denial to name the TX-permission failure, got {err:?}"
    );

    let events = drain(&mut rx);
    assert!(
        messages_to_send(&events).is_empty(),
        "nothing may be emitted for a refused open, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, QsoEvent::StateChanged { .. })),
        "no QSO may be created (or superseded) for a refused open, got {events:?}"
    );
}

/// PAN-280 scenario 3 (structural half): the close-step permission check
/// runs while the open holds the `qsos` write guard, so nothing can run
/// between the check and the insert. Before PAN-280 the predicate below
/// could take the lock (no guard was held at the check) and the open then
/// parked in `supersede_active_qsos_for`, i.e. other `qsos` holders could
/// run between the check and the insert.
#[tokio::test]
async fn close_step_open_checks_permission_inside_the_insert_guard() {
    use std::sync::atomic::AtomicBool;
    type Stash = Arc<
        std::sync::Mutex<Option<tokio::sync::OwnedRwLockWriteGuard<HashMap<QsoId, QsoProgress>>>>,
    >;
    let mut manager = manager();
    let qsos = Arc::clone(&manager.qsos);
    let took_lock = Arc::new(AtomicBool::new(false));
    let stash: Stash = Default::default();
    let (flag, st) = (Arc::clone(&took_lock), Arc::clone(&stash));
    manager.set_remote_tx_permitted_source(Arc::new(move |k: Option<&str>| {
        if k == Some("client-A") {
            if let Ok(g) = Arc::clone(&qsos).try_write_owned() {
                flag.store(true, Ordering::SeqCst);
                *st.lock().unwrap() = Some(g);
            }
        }
        k == Some("client-A")
    }));
    let mut rx = manager.subscribe();
    let call = manager.respond_to_caller(
        DX.into(),
        FREQ,
        None,
        ResponseStep::SeventyThree,
        Some(-8.0),
        Some(-4),
        None,
        true,
        Some("client-A".into()),
    );
    tokio::pin!(call);
    let parked = tokio::select! {
        biased;
        _ = &mut call => false,
        _ = tokio::task::yield_now() => true,
    };
    // Never leave the test hung on the stashed guard.
    drop(stash.lock().unwrap().take());
    if parked {
        let _ = call.await;
    }
    assert!(
        !took_lock.load(Ordering::SeqCst),
        "the permission check ran while no `qsos` guard was held"
    );
    assert!(
        !parked,
        "the open must not suspend between its check and its insert"
    );
    let events = drain(&mut rx);
    assert_eq!(messages_to_send(&events).len(), 1);
}

/// PAN-280: the single-guard open still supersedes an older active
/// same-call QSO, still emits `StateChanged(Failed{Superseded})` and
/// `QsoFailed{Superseded}` for it, and the new QSO's `StateChanged`
/// and `MessageToSend` follow them.
#[tokio::test]
async fn single_guard_open_still_supersedes_an_active_same_call_qso() {
    let manager = manager();
    // Autonomous QSOs are invisible to the manual lookups (FIX 1), so
    // this open reaches the supersede.
    let old_id = manager.respond_to_cq(DX.into(), FREQ, None).await.unwrap();
    let mut rx = manager.subscribe();
    let new_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert_ne!(new_id, old_id);
    let old = manager.get_qso(old_id).await.unwrap();
    assert!(matches!(
        old.state,
        QsoState::Failed {
            reason: QsoFailureReason::Superseded,
            ..
        }
    ));
    assert_eq!(
        manager.qsos_by_callsign.read().await.get(DX).cloned(),
        Some(vec![new_id]),
        "the callsign mapping holds only the new QSO"
    );
    let events = drain(&mut rx);
    let order: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::StateChanged {
                qso_id, new_state, ..
            } if *qso_id == old_id && matches!(new_state, QsoState::Failed { .. }) => {
                Some("old-failed")
            }
            QsoEvent::QsoFailed {
                qso_id,
                reason: QsoFailureReason::Superseded,
                ..
            } if *qso_id == old_id => Some("old-qso-failed"),
            QsoEvent::StateChanged {
                qso_id, old_state, ..
            } if *qso_id == new_id && matches!(old_state, QsoState::Idle) => Some("new-opened"),
            QsoEvent::MessageToSend { qso_id, .. } if *qso_id == new_id => Some("new-send"),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["old-failed", "old-qso-failed", "new-opened", "new-send"]
    );
}

/// PAN-139 control: the guard is identity-bound, not a blanket remote
/// block — a remote client the predicate PERMITS must still open at
/// `Rr73` exactly as before (`WaitingForConfirmation` + one
/// `FinalConfirmation` send).
#[tokio::test]
async fn permitted_remote_client_can_still_open_at_rr73() {
    let mut manager = manager();
    manager.set_remote_tx_permitted_source(Arc::new(|client_key_id: Option<&str>| {
        client_key_id == Some("client-B")
    }));
    let mut rx = manager.subscribe();

    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Rr73,
            Some(-5.0),
            Some(-7),
            None, // partner_freq
            true, // remote_origin
            Some("client-B".into()),
        )
        .await
        .expect("a permitted remote client must still open at Rr73");

    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    assert!(
        matches!(sends[0], MessageType::FinalConfirmation { .. }),
        "expected FinalConfirmation, got {:?}",
        sends[0]
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(
        progress.state,
        QsoState::WaitingForConfirmation { .. }
    ));
}

/// PAN-139 control: the guard is remote-only — a LOCAL-origin `Rr73`
/// open must ignore the deny predicate entirely (a disarmed station
/// must not freeze local manual QSOs; the arm gate only applies to
/// `TxOrigin::Remote` frames). Same control pattern as
/// `a_disarmed_arm_gate_does_not_suppress_local_qso_stall_counting`.
#[tokio::test]
async fn local_origin_rr73_open_ignores_the_remote_deny_predicate() {
    let mut manager = manager();
    manager.set_remote_tx_permitted_source(Arc::new(|_| false));
    let mut rx = manager.subscribe();

    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Rr73,
            Some(-5.0),
            Some(-7),
            None,  // partner_freq
            false, // remote_origin
            None,
        )
        .await
        .expect("a local-origin Rr73 open must not consult the remote deny predicate");

    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    assert!(matches!(sends[0], MessageType::FinalConfirmation { .. }));
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(matches!(
        progress.state,
        QsoState::WaitingForConfirmation { .. }
    ));
}

/// FIX B (one-QSO-per-(callsign,band) close idempotency,
/// docs/qso-tx-deep-review-2026-07-18.md): a station keeps sending us
/// RR73 (never copied our 73). The operator presses Space again — a
/// repeat `respond_to_caller(SeventyThree)` for the SAME callsign must
/// re-emit another 73 (so the DX still gets it) WITHOUT spawning a
/// sibling `QsoId` or logging a second ADIF entry. Superseded 2026-07-18:
/// this test used to assert the OPPOSITE — that a repeat press "should
/// build a fresh QSO" — which was the live double-73 duplicate-log bug
/// itself (3 SeventyThree presses in 8s produced 4 duplicate ADIF
/// entries for SM2LIY on-air). `find_recently_completed_manual_qso_for`
/// now catches the just-completed QSO within its grace window and
/// re-keys its last frame instead.
#[tokio::test]
async fn repeat_respond_to_caller_seventy_three_resends_73() {
    let manager = manager();
    let mut rx = manager.subscribe();

    // First 73.
    let first = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    assert_eq!(sends.len(), 1);
    assert!(matches!(sends[0], MessageType::SeventyThree { .. }));

    // They send us RR73 again; operator presses Space again → second 73.
    let second = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "repeat press within the grace window must resolve to the SAME QSO, \
         never spawn a sibling"
    );
    let events2 = drain(&mut rx);
    let sends2 = messages_to_send(&events2);
    assert_eq!(
        sends2.len(),
        1,
        "repeat Space must re-send a 73, not no-op: {sends2:?}"
    );
    assert!(
        matches!(sends2[0], MessageType::SeventyThree { .. }),
        "expected a second SeventyThree (re-sent), got {:?}",
        sends2[0]
    );

    // Exactly ONE QsoCompleted must have fired across BOTH calls — the
    // second call resolved via resend_last_tx, not a fresh completion.
    assert!(
        !events2
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "second call must NOT emit a new QsoCompleted: {events2:?}"
    );

    // `qsos_by_callsign` must have exactly one entry for DX — no sibling
    // QSO id was ever recorded against this callsign.
    let ids = manager
        .qsos_by_callsign
        .read()
        .await
        .get(&DX.to_uppercase())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        ids.len(),
        1,
        "exactly one QSO id must be mapped to {DX}, got {ids:?}"
    );
}

/// FIX B: three RAPID `SeventyThree` presses (mirroring the live incident
/// — 3 presses in 8 seconds) all resolve to the SAME QSO object;
/// `call_count` increments once per press (verified via `get_qso`).
#[tokio::test]
async fn triple_rapid_seventy_three_press_stays_one_qso_call_count_increments() {
    let manager = manager();
    let mut rx = manager.subscribe();

    let first = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);
    let call_count_after_first = manager.get_qso(first).await.unwrap().metadata.call_count;

    let second = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);
    let call_count_after_second = manager.get_qso(second).await.unwrap().metadata.call_count;

    let third = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);
    let call_count_after_third = manager.get_qso(third).await.unwrap().metadata.call_count;

    assert_eq!(first, second);
    assert_eq!(first, third);
    assert!(
        call_count_after_second > call_count_after_first,
        "call_count must increment on the 2nd press: {call_count_after_first} -> {call_count_after_second}"
    );
    assert!(
        call_count_after_third > call_count_after_second,
        "call_count must increment on the 3rd press: {call_count_after_second} -> {call_count_after_third}"
    );
}

/// FIX B: a `SeventyThree` reply made AFTER `COMPLETED_QSO_REWORK_GRACE`
/// has elapsed since the prior QSO completed must NOT re-key the old
/// QSO — it opens a genuinely NEW one (the grace window bounds how long
/// "still trying to close out the same contact" is assumed). Uses the
/// explicit-`now` testable variant
/// (`find_recently_completed_manual_qso_for_at`) instead of a real 45s+
/// `tokio::time::sleep`.
#[tokio::test]
async fn seventy_three_after_grace_elapsed_opens_new_qso() {
    let manager = manager();
    let mut rx = manager.subscribe();

    let first = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);

    // Directly exercise the explicit-`now` lookup past the grace window —
    // proves the grace-window boundary itself, independent of
    // `respond_to_caller`'s real-clock dispatch.
    let completed_at = match manager.get_qso(first).await.unwrap().state {
        QsoState::Completed { completed_at, .. } => completed_at,
        other => panic!("expected Completed state, got {other:?}"),
    };
    let just_within = completed_at + COMPLETED_QSO_REWORK_GRACE - SignedDuration::from_secs(1);
    let just_past = completed_at + COMPLETED_QSO_REWORK_GRACE + SignedDuration::from_secs(1);

    assert_eq!(
        manager
            .find_recently_completed_manual_qso_for_at(
                DX,
                FREQ,
                COMPLETED_QSO_REWORK_GRACE,
                just_within,
            )
            .await,
        Some(first),
        "within the grace window the completed QSO must still be found"
    );
    assert_eq!(
        manager
            .find_recently_completed_manual_qso_for_at(
                DX,
                FREQ,
                COMPLETED_QSO_REWORK_GRACE,
                just_past,
            )
            .await,
        None,
        "past the grace window the completed QSO must no longer be found"
    );
}

/// FIX B regression: a `Grid` or `Report`/`ReportAck` reply for a station
/// we JUST completed, within the grace window, is the deliberate
/// legitimate-rework path (e.g. working the same station again on a
/// fresh exchange) — it must still open a NEW QSO, not be swallowed into
/// the just-completed one. Only `Rr73`/`SeventyThree` (close steps) get
/// the grace-window idempotent-close treatment.
#[tokio::test]
async fn grid_or_report_step_within_grace_window_still_opens_new_qso() {
    let manager = manager();
    let mut rx = manager.subscribe();

    let first = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);

    // A fresh Grid-step call (the legitimate-rework case) must build a
    // new QSO, not resolve back to the just-completed one.
    let grid_reopen = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Grid,
            Some(-8.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        first, grid_reopen,
        "a Grid-step reply within the grace window must open a NEW QSO \
         (legitimate rework), not re-key the completed one"
    );
    drain(&mut rx);

    // Complete the reopened QSO's grace-window state doesn't matter here;
    // check Report similarly against a fresh completed QSO.
    let second_complete = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::SeventyThree,
            Some(-8.0),
            Some(-4),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    drain(&mut rx);

    let report_reopen = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Report,
            Some(-8.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        second_complete, report_reopen,
        "a Report-step reply within the grace window must open a NEW QSO \
         (legitimate rework), not re-key the completed one"
    );
}

/// `our_snr_of_them = None` falls back to a sane default report (-15).
#[tokio::test]
async fn respond_to_caller_defaults_report_when_no_snr() {
    let manager = manager();
    let mut rx = manager.subscribe();
    manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Report,
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let sends = messages_to_send(&drain(&mut rx));
    match &sends[0] {
        MessageType::SignalReport { report, .. } => assert_eq!(*report, -15),
        other => panic!("expected SignalReport, got {other:?}"),
    }
}

/// A full sequence opened at `ReportAck`: we send R-report, the DX answers
/// RR73, and the QSO completes (and logs) via the normal state machine.
#[tokio::test]
async fn respond_to_caller_report_ack_through_rr73_completes() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::ReportAck,
            Some(-10.0),
            Some(-3),
            None, // partner_freq
            false,
            None,
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);

    // DX rogers our R-report with RR73 → we close with 73 and complete.
    manager
        .process_message(
            MessageType::FinalConfirmation {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} RR73", OUR, DX),
            FREQ,
            Some(-12.0),
        )
        .await
        .unwrap();

    // PAN-280: the QsoCompleted is held until the 73 airs.
    let events = drain(&mut rx);
    let events = air_last_send(&manager, &mut rx, events).await;
    let sends = messages_to_send(&events);
    assert_eq!(sends.len(), 1, "expected one reply (73), got {:?}", sends);
    assert!(
        matches!(sends[0], MessageType::SeventyThree { .. }),
        "expected SeventyThree, got {:?}",
        sends[0]
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        progress.state
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "expected a QsoCompleted event"
    );
}

/// FIX 2: Manual QSO in SendingReport + a bare "73" (DX skipped RR73) →
/// QSO completes and logs, and we do NOT re-send a 73 (they are done).
#[tokio::test]
async fn manual_sending_report_plus_bare_73_completes_without_resend() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();
    let _ = drain(&mut rx);

    manager
        .process_message(
            MessageType::SeventyThree {
                to_station: OUR.into(),
                from_station: DX.into(),
            },
            format!("{} {} 73", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let events = drain(&mut rx);
    let sends = messages_to_send(&events);
    assert!(
        sends.is_empty(),
        "bare 73 close must not re-send a 73, got {:?}",
        sends
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::Completed { .. }),
        "expected Completed, got {:?}",
        progress.state
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "expected a QsoCompleted event (ADIF log)"
    );
}

/// 4. AUTONOMOUS QSO in RespondingToCq + SignalReport → state advances AND
///    the forward reply (our R-report / ReportAck) is auto-emitted. This is
///    the Phase-5 autonomous auto-completion behavior: forward auto-sequencing
///    now fires for Auto QSOs exactly as for Manual (regression handling stays
///    Manual-only). (Previously the emitter was Manual-gated and an Auto QSO
///    advanced silently — that gate was removed when Phase 5 landed.)
#[tokio::test]
async fn auto_qso_advances_and_auto_replies() {
    let manager = manager();
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.into(), FREQ, None) // CallInitiation::Auto
        .await
        .unwrap();
    let _ = drain(&mut rx); // discard initial CqResponse call

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let events = drain(&mut rx);
    let sends = messages_to_send(&events);
    // The forward reply IS emitted now (our R-report = ReportAck).
    assert!(
        sends
            .iter()
            .any(|m| matches!(m, MessageType::ReportAck { .. })),
        "autonomous QSO must auto-reply with our R-report (ReportAck), got {:?}",
        sends
    );
    // And the state advanced.
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "auto QSO state must advance to SendingReport, got {:?}",
        progress.state
    );
}

/// 5. Spurious sender (wrong from/to) is still ignored: no state advance
///    and no reply emitted, even for a manual QSO.
#[tokio::test]
async fn spurious_sender_ignored_no_reply() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    let _ = drain(&mut rx);

    // Properly-addressed report but from a DIFFERENT callsign.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: "NF4KE".into(),
                report: -7,
            },
            format!("{} NF4KE -07", OUR),
            FREQ,
            Some(-15.0),
        )
        .await
        .unwrap();

    let sends = messages_to_send(&drain(&mut rx));
    assert!(
        sends.is_empty(),
        "spurious sender must not trigger a reply, got {:?}",
        sends
    );
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "spurious report must not advance state, got {:?}",
        progress.state
    );
}

// --- active_tx_offsets (T3) ----------------------------------------------

/// `active_tx_offsets()` returns the `metadata.frequency` of every
/// currently-active (non-terminal) QSO. Used by the coordinator to
/// de-conflict a new QSO's TX offset against live concurrent streams.
#[tokio::test]
async fn active_tx_offsets_returns_open_qso_offsets() {
    let manager = manager();
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    // Initially empty — no QSOs.
    assert!(
        manager.active_tx_offsets().await.is_empty(),
        "no QSOs → offsets must be empty"
    );

    // Open one QSO at 1234.0 Hz.
    manager
        .respond_to_cq("VK3XYZ".to_string(), 1234.0, None)
        .await
        .expect("open first QSO");

    let offsets = manager.active_tx_offsets().await;
    assert_eq!(offsets.len(), 1, "one active QSO");
    assert!(
        (offsets[0] - 1234.0).abs() < 0.1,
        "offset must match the QSO's audio frequency"
    );

    // Open a second QSO at a different offset.
    manager
        .respond_to_cq("ZL2ABC".to_string(), 1800.0, None)
        .await
        .expect("open second QSO");

    let offsets2 = manager.active_tx_offsets().await;
    assert_eq!(offsets2.len(), 2, "two active QSOs");
    let mut sorted = offsets2.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!((sorted[0] - 1234.0).abs() < 0.1);
    assert!((sorted[1] - 1800.0).abs() < 0.1);
}

// --- respond_to_cq_with partner_freq param (T3) --------------------------

/// When `partner_freq = Some(x)` is passed, the created QSO's
/// `metadata.partner_freq` is `Some(x)` and `metadata.frequency` stays at
/// the TX offset — so the two fields are independently settable.
#[tokio::test]
async fn respond_to_cq_with_partner_freq_some_sets_both_fields() {
    let manager = manager();
    let tx_off = 1234.0_f64;
    let dx_rx = 1500.0_f64;
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            tx_off,
            None,
            CallInitiation::Manual,
            Some(dx_rx), // partner_freq = DX's RX offset
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        (progress.metadata.frequency - tx_off).abs() < 0.1,
        "metadata.frequency must be our TX offset ({tx_off}), got {}",
        progress.metadata.frequency
    );
    assert_eq!(
        progress.metadata.partner_freq,
        Some(dx_rx),
        "metadata.partner_freq must be the DX's RX offset ({dx_rx})"
    );
}

/// Regression: `partner_freq = None` (Tx=Rx path) leaves `metadata.partner_freq`
/// as `None` — behavior identical to pre-T3. The coordinator passes `None`
/// when Auto mode and no collision.
#[tokio::test]
async fn respond_to_cq_with_partner_freq_none_is_txrx_regression() {
    let manager = manager();
    let qso_id = manager
        .respond_to_cq_with(
            DX.into(),
            FREQ,
            None,
            CallInitiation::Manual,
            None, // Tx=Rx regression path — partner_freq must stay None
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.partner_freq, None,
        "partner_freq=None must be preserved on the Tx=Rx (Auto+no-collision) path"
    );
    assert!(
        (progress.metadata.frequency - FREQ).abs() < 0.1,
        "metadata.frequency must equal the passed frequency"
    );
}

/// Regression: `respond_to_caller` with `partner_freq = None` (Grid step
/// routes through `respond_to_cq_with`) — partner_freq must stay None.
#[tokio::test]
async fn respond_to_caller_partner_freq_none_is_txrx_regression() {
    let manager = manager();
    let qso_id = manager
        .respond_to_caller(
            DX.into(),
            FREQ,
            None,
            ResponseStep::Grid,
            None,
            None,
            None, // Tx=Rx — partner_freq must stay None
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.partner_freq, None,
        "Grid/Tx=Rx path: partner_freq must be None (regression)"
    );
}

/// PAN-23: `respond_to_caller` — the TUI Callers "reply" path — must
/// refuse the literal unresolved-hash placeholder `"<...>"` regardless
/// of which sequence step the operator (or a queued replay) opens at.
/// This is the exact failure mode observed on-air (production logs
/// 2026-08-20..22): the operator selected `"<...>"` from the Callers
/// panel at step `ReportAck`, pancetta queued/started the QSO, and the
/// eventual encode attempt failed with "callsign '<...>' cannot be
/// represented in any FT8 message format". The TUI-side fix (PAN-23,
/// `app.rs`'s `displayed_callers`) means the operator can no longer
/// select it, but this backend guard is belt-and-suspenders against any
/// other path reaching `RespondToCaller` with that target — and it
/// short-circuits BEFORE any `MessageToSend` is emitted, so no doomed
/// encode is ever attempted.
#[tokio::test]
async fn respond_to_caller_rejects_unresolved_hash_placeholder() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let err = manager
        .respond_to_caller(
            "<...>".to_string(),
            FREQ,
            None,
            ResponseStep::ReportAck,
            Some(-8.0),
            None,
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, QsoManagerError::InvalidCallsign { callsign } if callsign == "<...>"),
        "expected InvalidCallsign for the unresolved hash placeholder, got {err:?}"
    );
    assert!(
        messages_to_send(&drain(&mut rx)).is_empty(),
        "no MessageToSend may be emitted for the unresolved hash placeholder \
         (it can never be encoded into a valid FT8 message)"
    );
}
