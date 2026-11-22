//! State-regression intelligence ("back up to where the DX thinks we are").
//!
//! When a MANUAL QSO's DX re-sends an EARLIER-stage message — meaning they
//! never copied our most-recent transmission — the QSO machine regresses to
//! match the DX and re-sends the appropriate response instead of stalling.
//!
//! Re-send duty split:
//! - REGRESSION 1 (WaitingForConfirmation + repeated report → SendingReport):
//!   `process_message_for_qso` emits the R-report IMMEDIATELY this slot (via
//!   the reply emitter's new (WaitingForConfirmation, SignalReport) arm); the
//!   per-slot `rearm_manual_calls_at` owns subsequent slots.
//! - REGRESSION 2 (SendingReport + repeated report → stays SendingReport):
//!   the transition does NOT emit (exchange has no (SendingReport,
//!   SignalReport) arm); `rearm_manual_calls_at` (FIX 4) owns the R re-send.
//!   The transition only updates the latched reports. The repeat leaves
//!   `last_call_at` and `call_count` alone: the rearm paces and counts its
//!   own re-sends, exactly as it does while the DX is silent (PAN-129).
use super::*;

const OUR: &str = "K5ARH";
const DX: &str = "K9ZZ";
const FREQ: f64 = 1500.0;

fn manager_with(max_calls: u32, watchdog_min: u64) -> QsoManager {
    let mut config = QsoManagerConfig {
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
    config.timeouts.manual_call_max_calls = max_calls;
    config.timeouts.manual_call_watchdog_minutes = watchdog_min;
    QsoManager::new(config)
}

fn manager() -> QsoManager {
    manager_with(10, 5)
}

fn drain(rx: &mut broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Drive a manual QSO to WaitingForConfirmation (CqResponse → R → RR73 to
/// the DX), returning the qso_id.
async fn manual_to_waiting_confirmation(manager: &QsoManager) -> QsoId {
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
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::WaitingForConfirmation { .. }
    ));
    qso_id
}

/// REGRESSION 1: WaitingForConfirmation + repeated report → SendingReport,
/// an R-report is re-emitted, and reports are updated to the newest value.
#[tokio::test]
async fn manual_waiting_confirmation_plus_repeated_report_regresses_to_sending_report() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manual_to_waiting_confirmation(&manager).await;
    let _ = drain(&mut rx);
    let calls_before = manager.get_qso(qso_id).await.unwrap().metadata.call_count;

    // DX re-sends their report — with a NEW value — having never copied
    // our RR73. snr -9 → our report -9.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -3,
            },
            format!("{} {} -03", OUR, DX),
            FREQ,
            Some(-9.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    // Regressed two steps back.
    match &progress.state {
        QsoState::SendingReport {
            their_report,
            our_report,
            ..
        } => {
            assert_eq!(*their_report, Some(-3), "their report updated to newest");
            assert_eq!(*our_report, -9, "our report recomputed from newest SNR");
        }
        other => panic!("expected SendingReport, got {:?}", other),
    }
    // The regression's in-slot R re-send counts against the keep-calling
    // budget exactly once.
    assert_eq!(
        progress.metadata.call_count,
        calls_before + 1,
        "REGRESSION 1's re-send must count as one call"
    );

    // R-report re-emitted this slot.
    let emitted = sends(&drain(&mut rx));
    assert_eq!(
        emitted.len(),
        1,
        "expected one R re-send, got {:?}",
        emitted
    );
    match &emitted[0] {
        MessageType::ReportAck {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, DX);
            assert_eq!(from_station, OUR);
            assert_eq!(*report, -9);
        }
        other => panic!("expected ReportAck, got {:?}", other),
    }
}

/// REGRESSION 2: SendingReport + repeated report → stays SendingReport (no
/// spurious double-advance); rearm re-sends R (transition itself does not,
/// avoiding a same-slot double-send).
#[tokio::test]
async fn manual_sending_report_repeated_report_stays_and_resends() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manager
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
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::SendingReport { .. }
    ));
    let _ = drain(&mut rx);

    // DX re-sends their report (didn't copy our R).
    let result = manager
        .determine_state_transition(
            qso_id,
            &manager.get_qso(qso_id).await.unwrap().state,
            &MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            Some(-15.0),
            CallInitiation::Manual,
            false,
        )
        .await
        .unwrap();
    assert!(
        matches!(result, QsoState::SendingReport { .. }),
        "must stay in SendingReport, got {:?}",
        result
    );

    // Now exercise the full path: it must NOT emit from the transition (no
    // exchange arm); the per-slot rearm owns the R re-send.
    let last_before = manager.get_qso(qso_id).await.unwrap().metadata.last_call_at;
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
    let from_transition = sends(&drain(&mut rx));
    assert!(
        from_transition.is_empty(),
        "transition must not re-send (rearm owns it), got {:?}",
        from_transition
    );
    // PAN-129: the repeat sends nothing, so it must not hold the rearm
    // back either — `last_call_at` is left where our last call put it.
    let last = manager.get_qso(qso_id).await.unwrap().metadata.last_call_at;
    assert_eq!(
        last, last_before,
        "a repeated report must leave last_call_at unchanged"
    );
    let last = last.unwrap();

    // A slot after our last call, rearm re-sends our R-report: it paces
    // from its own last call, exactly as it does while the DX is silent.
    manager
        .rearm_manual_calls_at(last + Duration::seconds(15))
        .await;
    let rearmed = sends(&drain(&mut rx));
    assert_eq!(rearmed.len(), 1, "rearm re-sends R, got {:?}", rearmed);
    assert!(
        matches!(rearmed[0], MessageType::ReportAck { .. }),
        "rearm must re-send ReportAck, got {:?}",
        rearmed[0]
    );
}

/// PAN-129: a repeated report in SendingReport queues no reply, so it
/// spends no call and does not restamp `last_call_at` — the rearm, which
/// owns that re-send, paces and counts it exactly as it does for silence.
/// It still cancels a pending C3 reprieve (the DX is not progressing).
#[tokio::test]
async fn repeated_report_leaves_call_count_and_last_call_at_alone() {
    let manager = manager();
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
    // Pin `last_call_at` well away from "now" so a restamp to the decode
    // time cannot coincide with it, and arm a C3 reprieve to cancel.
    let pinned_last_call_at = Utc::now() - Duration::seconds(20);
    {
        let mut qsos = manager.qsos.write().await;
        let progress = qsos.get_mut(&qso_id).unwrap();
        assert!(
            matches!(
                progress.state,
                QsoState::SendingReport {
                    their_report: Some(_),
                    ..
                }
            ),
            "precondition: SendingReport with the DX's report, got {:?}",
            progress.state
        );
        progress.metadata.last_call_at = Some(pinned_last_call_at);
        progress.metadata.progressed_this_cycle = true;
    }
    let calls_before = manager.get_qso(qso_id).await.unwrap().metadata.call_count;
    let mut rx = manager.subscribe();

    // The DX repeats the identical report (it did not copy our R).
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
    assert!(
        matches!(
            progress.state,
            QsoState::SendingReport {
                their_report: Some(-7),
                ..
            }
        ),
        "must stay in SendingReport, got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.call_count, calls_before,
        "a repeat that sends nothing must not spend a call"
    );
    assert_eq!(
        progress.metadata.last_call_at,
        Some(pinned_last_call_at),
        "a repeat that sends nothing must not hold the rearm back"
    );
    assert!(
        !progress.metadata.progressed_this_cycle,
        "a repeat is not progress: it must cancel the C3 reprieve"
    );
    let emitted = sends(&drain(&mut rx));
    assert!(
        emitted.is_empty(),
        "the repeat itself must not emit (the rearm owns the re-send), got {emitted:?}"
    );
}

/// On-air regression (9K2MP, 2026-06-22): once the DX has ENGAGED (we're in
/// SendingReport — we received their report), the call-count cap must NOT
/// retire the QSO. Previously a DX that re-requested several times blew past
/// the 10-call cap and the watchdog retired the QSO one slot before the DX's
/// closing RR73 — the RR73 then landed on a dead QSO, the contact was lost,
/// and the operator had to close it by hand. The cap now governs only the
/// initial-call states (CallingCq/RespondingToCq); an engaged exchange runs
/// to completion (bounded only by the time watchdog).
#[tokio::test]
async fn engaged_sending_report_survives_call_cap_and_completes_on_rr73() {
    let manager = manager_with(3, 60); // cap 3, generous 60-min time window
    let qso_id = manual_to_waiting_confirmation(&manager).await;

    // DX re-requests its report many times → regress to SendingReport, and
    // keep-calling re-sends our R after each repeat until the call cap is
    // reached. A repeated report itself spends no call (PAN-129), so the
    // cap is reached through the rearm's real re-sends, one own TX slot
    // (30 s) apart, starting a slot after the regression's re-send.
    let mut rearm_at: Option<DateTime<Utc>> = None;
    for _ in 0..6 {
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
        let at = match rearm_at {
            Some(previous) => previous + Duration::seconds(30),
            None => {
                manager
                    .get_qso(qso_id)
                    .await
                    .unwrap()
                    .metadata
                    .last_call_at
                    .unwrap()
                    + Duration::seconds(15)
            }
        };
        manager.rearm_manual_calls_at(at).await;
        rearm_at = Some(at);
    }
    let prog = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(prog.state, QsoState::SendingReport { .. }),
        "should be engaged (SendingReport), got {:?}",
        prog.state
    );
    assert!(
        prog.metadata.call_count >= 3,
        "call cap should be exceeded, got {}",
        prog.metadata.call_count
    );
    let first = prog.metadata.first_call_at.unwrap();

    // Within the time window: the cap must NOT retire an engaged QSO.
    manager
        .check_timeouts_at(first + Duration::minutes(1))
        .await;
    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "engaged SendingReport must NOT be retired by the call cap"
    );

    // The DX's closing RR73 now completes the QSO (it was alive to copy it).
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
    assert!(
        matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::Completed { .. }
        ),
        "RR73 must complete the engaged QSO instead of being dropped"
    );
}

/// The TIME watchdog still bounds a SendingReport that never closes (DX
/// re-requests forever): removing the call cap there must not make it
/// immortal.
#[tokio::test]
async fn engaged_sending_report_still_retires_on_time_watchdog() {
    let manager = manager_with(3, 5); // cap 3, 5-min time watchdog
    let qso_id = manual_to_waiting_confirmation(&manager).await;
    for _ in 0..4 {
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
    }
    let first = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .first_call_at
        .unwrap();
    // Past the 5-min watchdog → retired even though "engaged".
    manager
        .check_timeouts_at(first + Duration::minutes(6))
        .await;
    assert!(
        matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "time watchdog must still bound a never-closing SendingReport"
    );
}

/// Our SENT report value stays latched across DX re-requests — it must not
/// jitter with per-decode SNR noise (observed on-air: R-7 → R-9 → R-6).
#[tokio::test]
async fn our_report_value_latched_across_repeated_dx_reports() {
    let manager = manager_with(20, 60);
    let qso_id = manager
        .respond_to_cq_manual(DX.into(), FREQ, None)
        .await
        .unwrap();
    // First DX report at SNR -15 → we latch our_report.
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
    let first_report = match manager.get_qso(qso_id).await.unwrap().state {
        QsoState::SendingReport { our_report, .. } => our_report,
        other => panic!("expected SendingReport, got {other:?}"),
    };
    // DX re-requests; this decode has a very different SNR. our_report must
    // NOT change.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.into(),
                from_station: DX.into(),
                report: -7,
            },
            format!("{} {} -07", OUR, DX),
            FREQ,
            Some(-2.0),
        )
        .await
        .unwrap();
    let second_report = match manager.get_qso(qso_id).await.unwrap().state {
        QsoState::SendingReport { our_report, .. } => our_report,
        other => panic!("expected SendingReport, got {other:?}"),
    };
    assert_eq!(
        first_report, second_report,
        "our sent report must stay latched across DX re-requests"
    );
}

/// A spurious sender (correct to:, wrong from:) does NOT trigger regression.
#[tokio::test]
async fn regression_requires_matching_sender() {
    let manager = manager();
    let mut rx = manager.subscribe();
    let qso_id = manual_to_waiting_confirmation(&manager).await;
    let before = manager.get_qso(qso_id).await.unwrap().metadata.call_count;
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

    // No regression: still WaitingForConfirmation, no re-send, no count bump.
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "spurious sender must not regress, got {:?}",
        progress.state
    );
    assert!(
        sends(&drain(&mut rx)).is_empty(),
        "spurious sender must not re-send"
    );
    assert_eq!(
        progress.metadata.call_count, before,
        "spurious sender must not count against cap"
    );
}

/// An AUTO-initiated QSO with a repeated earlier-stage message does NOT
/// regress or auto-resend (manual-only gate).
#[tokio::test]
async fn auto_qso_does_not_regress() {
    let manager = manager();
    manager
        .dial_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let mut rx = manager.subscribe();
    // Build an AUTO QSO and drive it forward to WaitingForConfirmation. The
    // auto path does not auto-reply, so we drive the state directly via
    // process_message (state machine advances regardless of mode).
    let qso_id = manager.respond_to_cq(DX.into(), FREQ, None).await.unwrap();
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
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::WaitingForConfirmation { .. }
    ));
    let _ = drain(&mut rx);

    // DX repeats their report. Auto QSO must NOT regress and must NOT
    // auto-resend.
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
    assert!(
        matches!(progress.state, QsoState::WaitingForConfirmation { .. }),
        "auto QSO must NOT regress, got {:?}",
        progress.state
    );
    assert!(
        sends(&drain(&mut rx)).is_empty(),
        "auto QSO must not auto-resend on regression"
    );
}
