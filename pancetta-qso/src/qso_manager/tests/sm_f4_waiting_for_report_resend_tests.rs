//! SM-F4 (Batch 4): the CQer role previously had NO resilience to a
//! caller repeating their grid (CqResponse) after missing our
//! SignalReport in `WaitingForReport` — the (WaitingForReport,
//! CqResponse) pair fell through to a no-op, so a caller who never copied
//! our report caused the QSO to silently die at the 30s report_timeout.
//!
//! This module tests the new regression arm (stay in WaitingForReport,
//! re-send the SAME latched `our_report` — no SNR jitter), its sender
//! verification, and the watchdog-exemption change that lets a CQer in
//! WaitingForReport survive the generic 30s timeout under the manual
//! keep-call watchdog (mirroring SendingReport's existing exemption).
use super::*;
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;

const OUR: &str = "W1ABC"; // test_config()'s our_callsign
const CALLER: &str = "K1DEF";
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

fn sends(events: &[QsoEvent]) -> Vec<MessageType> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Drive a Manual CQ to `WaitingForReport` (opening CQ, then a caller's
/// grid-bearing CqResponse), returning the qso_id and the `our_report`
/// value latched on that transition.
async fn manual_cq_to_waiting_for_report(manager: &QsoManager, snr: f32) -> (QsoId, i8) {
    let qso_id = manager
        .start_cq_manual(FREQ, None, false, None)
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.to_string(),
                responding_station: CALLER.to_string(),
                grid: Some("FN31".to_string()),
            },
            format!("{OUR} {CALLER} FN31"),
            FREQ,
            Some(snr),
        )
        .await
        .unwrap();
    let our_report = match manager.get_qso(qso_id).await.unwrap().state {
        QsoState::WaitingForReport { our_report, .. } => our_report,
        other => panic!("expected WaitingForReport, got {other:?}"),
    };
    (qso_id, our_report)
}

/// A repeated CqResponse from the SAME caller stays in WaitingForReport
/// (does not advance, does not fail) and the rearm re-sends the SAME
/// `our_report` value both times — proving no SNR-jitter across repeats,
/// mirroring REGRESSION 2's anti-jitter contract for the CQer role.
#[tokio::test]
async fn repeated_cq_response_stays_and_resends_identical_report() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let (qso_id, first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;
    let _ = drain(&mut rx);

    // Caller repeats their grid (never copied our report) — a fresh
    // decode with a DIFFERENT SNR, to prove our_report does NOT jitter.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.to_string(),
                responding_station: CALLER.to_string(),
                grid: Some("FN31".to_string()),
            },
            format!("{OUR} {CALLER} FN31"),
            FREQ,
            Some(-2.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    let (state_report, their_grid) = match &progress.state {
        QsoState::WaitingForReport {
            our_report,
            their_grid,
            ..
        } => (*our_report, their_grid.clone()),
        other => panic!("expected to stay in WaitingForReport, got {other:?}"),
    };
    assert_eq!(
        state_report, first_report,
        "our_report must stay latched across a repeated CqResponse (no SNR jitter)"
    );
    assert_eq!(their_grid, Some("FN31".to_string()));

    // The transition itself must not emit (exchange.rs has no
    // (WaitingForReport, CqResponse) arm) — the rearm owns the re-send.
    assert!(
        sends(&drain(&mut rx)).is_empty(),
        "transition must not re-send directly; rearm owns it"
    );

    // A slot later, the rearm re-sends our SignalReport with the
    // IDENTICAL report value.
    let last_call_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(last_call_at + SignedDuration::from_secs(15))
        .await;
    let rearmed = sends(&drain(&mut rx));
    assert_eq!(
        rearmed.len(),
        1,
        "expected one rearm re-send, got {rearmed:?}"
    );
    match &rearmed[0] {
        MessageType::SignalReport {
            to_station,
            from_station,
            report,
        } => {
            assert_eq!(to_station, CALLER);
            assert_eq!(from_station, OUR);
            assert_eq!(
                *report, first_report,
                "rearm must re-send the SAME report value latched at the first send"
            );
        }
        other => panic!("expected SignalReport, got {other:?}"),
    }

    // Drive a SECOND repeat + rearm cycle and confirm the value is STILL
    // byte-identical across both repeats (not just the first one).
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.to_string(),
                responding_station: CALLER.to_string(),
                grid: Some("FN31".to_string()),
            },
            format!("{OUR} {CALLER} FN31"),
            FREQ,
            Some(12.0),
        )
        .await
        .unwrap();
    let last_call_at2 = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(last_call_at2 + SignedDuration::from_secs(15))
        .await;
    let rearmed2 = sends(&drain(&mut rx));
    assert_eq!(rearmed2.len(), 1);
    match &rearmed2[0] {
        MessageType::SignalReport { report, .. } => {
            assert_eq!(
                *report, first_report,
                "report must remain byte-identical across BOTH repeats, no jitter"
            );
        }
        other => panic!("expected SignalReport, got {other:?}"),
    }
}

/// A repeated CqResponse from a DIFFERENT (non-partner) station is
/// rejected via the qso.security path and does NOT trigger the stay-arm.
#[tokio::test]
async fn repeated_cq_response_from_non_partner_is_rejected() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let (qso_id, first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;
    let _ = drain(&mut rx);

    // A DIFFERENT station answers our CQ with the SAME slot pattern —
    // must not be treated as our caller's repeat.
    manager
        .process_message(
            MessageType::CqResponse {
                calling_station: OUR.to_string(),
                responding_station: "NF4KE".to_string(),
                grid: Some("EM12".to_string()),
            },
            format!("{OUR} NF4KE EM12"),
            FREQ,
            Some(5.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    match &progress.state {
        QsoState::WaitingForReport {
            their_callsign,
            our_report,
            ..
        } => {
            assert_eq!(their_callsign, CALLER, "partner must not change");
            assert_eq!(
                *our_report, first_report,
                "our_report must not change from a spurious sender"
            );
        }
        other => panic!("expected to remain in WaitingForReport, got {other:?}"),
    }
    assert!(
        sends(&drain(&mut rx)).is_empty(),
        "spurious sender must not trigger a re-send"
    );
}

/// A Manual CQer in WaitingForReport is NOT retired by the generic 30s
/// report_timeout — it survives well past 30s under the manual
/// watchdog's longer bound (mirrors the 9K2MP-style SendingReport
/// watchdog-exemption test) but IS eventually retired by the 5-min /
/// max-calls watchdog bound.
#[tokio::test]
async fn manual_waiting_for_report_survives_generic_timeout_but_not_watchdog() {
    let mut config = test_config();
    config.timeouts.manual_call_max_calls = 20;
    config.timeouts.manual_call_watchdog_minutes = 5;
    let manager = QsoManager::new(config);
    let (qso_id, _first_report) = manual_cq_to_waiting_for_report(&manager, -9.0).await;

    // Well past the generic 30s report_timeout, but well within the 5-min
    // manual watchdog — must still be alive.
    manager
        .check_timeouts_at(UtcTimestamp::now() + SignedDuration::from_secs(90))
        .await;
    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "a Manual CQer in WaitingForReport must survive past the generic 30s report_timeout"
    );

    // Past the 5-min watchdog bound — must now be retired (not immortal).
    let first_call_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .first_call_at
        .unwrap();
    manager
        .check_timeouts_at(first_call_at + SignedDuration::from_mins(6))
        .await;
    assert!(
        matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "the manual watchdog must still retire a WaitingForReport CQer eventually"
    );
}
