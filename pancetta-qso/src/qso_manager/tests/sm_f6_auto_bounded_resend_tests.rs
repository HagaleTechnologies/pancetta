//! SM-F6 (Batch 4): bounded re-send/regression resilience for AUTO QSOs.
//!
//! `rearm_manual_calls_at` previously skipped every Auto QSO outright
//! (`if initiated_by != Manual { continue; }`), so an autonomous pounce
//! whose reply the DX missed got ZERO re-sends and silently died at the
//! existing 30s `report_timeout`. This module proves the new bounded
//! resend (`AUTO_RESEND_MAX_CALLS = 2`: the opening send plus exactly one
//! resend) lands within that unchanged 30s window and does NOT make the
//! QSO immortal.
use super::*;
use jiff::SignedDuration;

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

/// An Auto QSO in `RespondingToCq` that never gets a DX reply receives
/// exactly ONE bounded resend around the ~15s mark (the SLOT_SECONDS
/// cadence), and is still retired at the (unmodified) 30s report_timeout
/// — not immortal.
#[tokio::test]
async fn auto_responding_to_cq_gets_one_bounded_resend_then_retires_at_30s() {
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

    // At +15s (one FT8 slot): the bounded Auto resend fires.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(15))
        .await;
    let resent = sends(&drain(&mut rx));
    assert_eq!(
        resent.len(),
        1,
        "expected exactly one bounded Auto resend at +15s, got {resent:?}"
    );
    assert!(
        matches!(resent[0], MessageType::CqResponse { .. }),
        "Auto resend in RespondingToCq must re-send our CqResponse (call), got {:?}",
        resent[0]
    );

    // At +30s (another slot): the cap (AUTO_RESEND_MAX_CALLS = 2) must
    // now block any further resend — call_count is already 2.
    manager
        .rearm_manual_calls_at(opened_at + SignedDuration::from_secs(30))
        .await;
    assert!(
        sends(&drain(&mut rx)).is_empty(),
        "Auto resend must be capped — no second resend"
    );

    // Still alive at +30s exactly (state_duration comparison is strict
    // `>`), then check_timeouts_at retires it once duration exceeds the
    // 30s report_timeout — not immortal.
    assert!(
        manager.get_qso(qso_id).await.is_ok(),
        "QSO should still be alive exactly at the 30s boundary"
    );
    manager
        .check_timeouts_at(opened_at + SignedDuration::from_secs(31))
        .await;
    assert!(
        matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "an unanswered Auto pounce must still retire at the unmodified 30s report_timeout"
    );
}

/// The bounded Auto resend also covers `SendingReport` (the DX never
/// rogers our R-report): one resend, then the same 30s bound applies.
#[tokio::test]
async fn auto_sending_report_gets_one_bounded_resend_then_retires_at_30s() {
    let manager = QsoManager::new(test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .respond_to_cq(DX.to_string(), FREQ, None)
        .await
        .unwrap();
    // DX sends their report → advances RespondingToCq -> SendingReport.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: OUR.to_string(),
                from_station: DX.to_string(),
                report: -9,
            },
            format!("{OUR} {DX} -09"),
            FREQ,
            Some(-11.0),
        )
        .await
        .unwrap();
    let entered_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    assert!(matches!(
        manager.get_qso(qso_id).await.unwrap().state,
        QsoState::SendingReport { .. }
    ));
    let _ = drain(&mut rx);

    // +15s: one bounded resend of our R-report (ReportAck) — the DX
    // already gave us their plain report to get here (their_report is
    // Some on this rung), so the rearm arm re-sends ReportAck, not a
    // plain SignalReport (see the SendingReport rearm arm's own comment).
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(15))
        .await;
    let resent = sends(&drain(&mut rx));
    assert_eq!(
        resent.len(),
        1,
        "expected one bounded resend, got {resent:?}"
    );
    assert!(matches!(resent[0], MessageType::ReportAck { .. }));

    // +30s: capped, no further resend.
    manager
        .rearm_manual_calls_at(entered_at + SignedDuration::from_secs(30))
        .await;
    assert!(sends(&drain(&mut rx)).is_empty(), "resend must be capped");

    // Still bounded by the unmodified 30s report_timeout. `entered_at` is
    // `last_call_at` stamped when the QSO OPENED (Auto forward advances
    // do not stamp `last_call_at` — see `process_message_for_qso`'s
    // Manual-only gate), which lands a fraction of a millisecond BEFORE
    // the SendingReport state's own `started_at` (stamped moments later
    // by the SignalReport transition). `SignedDuration::num_seconds`
    // truncates towards zero, so a tight +31s margin can read as 30
    // (e.g. 30.9996s) and spuriously miss the boundary — use a
    // comfortable +33s margin instead.
    manager
        .check_timeouts_at(entered_at + SignedDuration::from_secs(33))
        .await;
    assert!(
        matches!(
            manager.get_qso(qso_id).await,
            Err(QsoManagerError::QsoNotFound { .. })
        ),
        "an Auto SendingReport that never closes must still retire at 30s"
    );
}
