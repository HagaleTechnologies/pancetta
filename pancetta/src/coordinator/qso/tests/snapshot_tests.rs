use super::snapshot_item_from_progress;
use chrono::{Duration, Utc};
use pancetta_qso::{
    GridSquares, MessageDirection, QsoMetadata, QsoProgress, QsoState, SignalReports,
};

/// Build a QsoProgress mid-exchange: we called them, sent our grid,
/// and just received their report.
fn fixture_progress() -> QsoProgress {
    let start = Utc::now() - Duration::seconds(45);
    let their_call = "JA1ABC".to_string();
    let messages = vec![
        pancetta_qso::states::QsoMessage {
            timestamp: start + Duration::seconds(15),
            direction: MessageDirection::Sent,
            message_type: pancetta_qso::states::MessageType::CqResponse {
                calling_station: their_call.clone(),
                responding_station: "K5ARH".to_string(),
                grid: Some("EM10".to_string()),
            },
            raw_text: "JA1ABC K5ARH EM10".to_string(),
            signal_strength: None,
            frequency: 1500.0,
        },
        pancetta_qso::states::QsoMessage {
            timestamp: start + Duration::seconds(30),
            direction: MessageDirection::Received,
            message_type: pancetta_qso::states::MessageType::SignalReport {
                to_station: "K5ARH".to_string(),
                from_station: their_call.clone(),
                report: -12,
            },
            raw_text: "K5ARH JA1ABC -12".to_string(),
            signal_strength: Some(-12.4),
            frequency: 1500.0,
        },
    ];
    QsoProgress {
        state: QsoState::SendingReport {
            their_callsign: their_call.clone(),
            their_report: Some(-12),
            our_report: -8,
            frequency: 1500.0,
            started_at: start,
        },
        state_history: Vec::new(),
        messages,
        metadata: QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".to_string(),
            their_callsign: Some(their_call),
            frequency: 1500.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: start,
            end_time: None,
            reports: SignalReports {
                sent: Some(-8),
                received: Some(-12),
            },
            grids: GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: std::collections::HashMap::new(),
            adif_field_types: std::collections::HashMap::new(),
            notes: None,
            tx_parity: Some(pancetta_core::slot::SlotParity::Odd),
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
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
    }
}

/// Default watchdog config for snapshot tests (matches TimeoutConfig
/// defaults: 10 calls / 5 minutes).
const TEST_MAX_CALLS: u32 = 10;
const TEST_WATCHDOG_MIN: u64 = 5;

/// Thin wrapper so the existing tests don't each repeat the watchdog args.
fn snap(progress: &QsoProgress) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
    snapshot_item_from_progress(progress, TEST_MAX_CALLS, TEST_WATCHDOG_MIN)
}

/// All detail-panel fields derive from state the engine already
/// tracks: last message per direction, measured RX SNR, reports,
/// exchange count, plus the original banner fields.
#[test]
fn snapshot_derives_detail_fields_from_progress() {
    let item = snap(&fixture_progress()).expect("item");
    assert_eq!(item.their_callsign, "JA1ABC");
    assert_eq!(item.state, "sending rpt");
    assert_eq!(item.frequency_hz, 1500.0);
    assert_eq!(item.tx_parity, Some(pancetta_core::slot::SlotParity::Odd));
    assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH EM10"));
    assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
    assert!(item.last_tx_at.is_some());
    assert!(item.last_rx_at.is_some());
    assert_eq!(item.snr_rx, Some(-12));
    assert_eq!(item.report_sent, Some(-8));
    assert_eq!(item.report_received, Some(-12));
    assert_eq!(item.exchange_count, 2);
}

/// The most recent message per direction wins, not the first.
#[test]
fn snapshot_picks_latest_message_per_direction() {
    let mut progress = fixture_progress();
    progress.messages.push(pancetta_qso::states::QsoMessage {
        timestamp: Utc::now(),
        direction: MessageDirection::Sent,
        message_type: pancetta_qso::states::MessageType::ReportAck {
            to_station: "JA1ABC".to_string(),
            from_station: "K5ARH".to_string(),
            report: -8,
        },
        raw_text: "JA1ABC K5ARH R-8".to_string(),
        signal_strength: None,
        frequency: 1500.0,
    });
    let item = snap(&progress).expect("item");
    assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH R-8"));
    // RX side unchanged by a new TX.
    assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
    assert_eq!(item.exchange_count, 3);
}

/// No callsign known yet (e.g. CallingCq with empty metadata) →
/// nothing useful to render → None.
#[test]
fn snapshot_skips_qso_without_callsign() {
    let mut progress = fixture_progress();
    progress.state = QsoState::CallingCq {
        frequency: 1500.0,
        started_at: Utc::now(),
        call_count: 1,
    };
    progress.metadata.their_callsign = None;
    assert!(snap(&progress).is_none());
}

/// A QSO with no messages yet (just started) still produces an item
/// with empty detail fields — the panel renders placeholders.
#[test]
fn snapshot_handles_empty_message_history() {
    let mut progress = fixture_progress();
    progress.messages.clear();
    let item = snap(&progress).expect("item");
    assert!(item.last_tx_text.is_none());
    assert!(item.last_rx_text.is_none());
    assert!(item.snr_rx.is_none());
    assert_eq!(item.exchange_count, 0);
}

/// Batch 2 #1: a MANUAL QSO in a keep-calling state surfaces the
/// watchdog countdown fields (call N/M + deadline).
#[test]
fn snapshot_surfaces_watchdog_for_manual_keep_calling() {
    let mut progress = fixture_progress();
    let start = Utc::now() - Duration::seconds(20);
    progress.state = QsoState::RespondingToCq {
        target_callsign: "JA1ABC".to_string(),
        frequency: 1500.0,
        started_at: start,
    };
    progress.metadata.initiated_by = pancetta_qso::CallInitiation::Manual;
    progress.metadata.call_count = 4;
    progress.metadata.first_call_at = Some(start);
    let item = snap(&progress).expect("item");
    assert_eq!(item.call_count, 4);
    assert_eq!(item.max_calls, TEST_MAX_CALLS);
    let deadline = item.watchdog_deadline.expect("deadline");
    assert_eq!(
        deadline,
        start + Duration::minutes(TEST_WATCHDOG_MIN as i64)
    );
}

/// An AUTO QSO (or a manual QSO past the keep-calling phase) shows no
/// watchdog fields — they would be misleading.
#[test]
fn snapshot_no_watchdog_for_auto_qso() {
    let mut progress = fixture_progress();
    progress.metadata.initiated_by = pancetta_qso::CallInitiation::Auto;
    progress.metadata.call_count = 3;
    progress.metadata.first_call_at = Some(Utc::now());
    let item = snap(&progress).expect("item");
    assert_eq!(item.call_count, 0);
    assert_eq!(item.max_calls, 0);
    assert!(item.watchdog_deadline.is_none());
}

/// PAN-280: an open at 73 whose 73 never aired has its own failure text.
#[test]
fn close_not_aired_has_its_own_failure_text() {
    assert_eq!(
        super::failure_reason_text(&pancetta_qso::QsoFailureReason::CloseNotAired),
        "closing 73 not sent, not logged"
    );
}

/// Batch 2 #3: every failure reason maps to an operator-readable string.
#[test]
fn failure_reason_text_is_human_readable() {
    use pancetta_qso::QsoFailureReason as R;
    assert_eq!(
        super::failure_reason_text(&R::ControlTransferred),
        "remote control passed to another client"
    );
    assert_eq!(super::failure_reason_text(&R::Timeout), "watchdog timeout");
    assert_eq!(
        super::failure_reason_text(&R::Superseded),
        "superseded by a newer call"
    );
    assert_eq!(
        super::failure_reason_text(&R::UserCancelled),
        "cancelled by operator"
    );
    assert_eq!(
        super::failure_reason_text(&R::ProtocolError("boom".to_string())),
        "protocol error: boom"
    );
}

#[test]
fn rejection_reason_text_covers_every_variant_and_fits_overlay() {
    use pancetta_qso::RejectionReason as R;
    for reason in [
        R::SenderNotPartner,
        R::AddresseeNotUs,
        R::SenderAndAddresseeMismatch,
        R::UnsafeCompoundUpgrade,
    ] {
        assert!(super::rejection_reason_text(&reason).len() <= 40);
    }
    assert_eq!(
        super::rejection_reason_text(&R::SenderNotPartner),
        "sender is not our QSO partner"
    );
}

/// observability-diagnostics-plan.md Layer 2 (Task 1): the `Completed`
/// path of `recent_qso_outcome` — correct callsign, outcome, frequency,
/// and a brief timeline that reflects the exchanged reports.
#[test]
fn recent_qso_outcome_completed_carries_callsign_and_reports() {
    let progress = fixture_progress();
    let mut metadata = progress.metadata;
    let their_call = metadata.their_callsign.clone().expect("fixture has a call");

    let outcome = super::recent_qso_outcome(
        &their_call,
        crate::message_bus::QsoOutcome::Completed,
        &metadata,
    );

    assert_eq!(
        outcome.brief_timeline[0],
        format!(
            "JA1ABC started at {}",
            metadata.start_time.format("%H:%M:%S")
        )
    );
    metadata.first_answer_at = Some(metadata.start_time + chrono::Duration::minutes(4));
    let answered = super::recent_qso_outcome(
        &their_call,
        crate::message_bus::QsoOutcome::Completed,
        &metadata,
    );
    assert_eq!(
        answered.brief_timeline[0],
        format!(
            "JA1ABC started at {}",
            metadata.first_answer_at.unwrap().format("%H:%M:%S")
        )
    );
    assert_eq!(outcome.callsign, "JA1ABC");
    assert!(matches!(
        outcome.outcome,
        crate::message_bus::QsoOutcome::Completed
    ));
    assert_eq!(outcome.last_state, "Completed");
    assert_eq!(outcome.freq_hz, 1500);
    assert!(
        outcome
            .brief_timeline
            .iter()
            .any(|l| l.contains("Report sent: -8")),
        "brief_timeline should mention the sent report: {:?}",
        outcome.brief_timeline
    );
    assert!(
        outcome
            .brief_timeline
            .iter()
            .any(|l| l.contains("Report received: -12")),
        "brief_timeline should mention the received report: {:?}",
        outcome.brief_timeline
    );
    assert_eq!(
        outcome.brief_timeline.last().map(String::as_str),
        Some("Completed")
    );
}

/// observability-diagnostics-plan.md Layer 2 (Task 1): the `Failed`
/// path of `recent_qso_outcome` — correct callsign + reason, and the
/// reason surfaces in the brief timeline's final line.
#[test]
fn recent_qso_outcome_failed_timeout_carries_reason() {
    let progress = fixture_progress();
    let metadata = progress.metadata;
    let their_call = metadata.their_callsign.clone().expect("fixture has a call");

    let outcome = super::recent_qso_outcome(
        &their_call,
        crate::message_bus::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout),
        &metadata,
    );

    assert_eq!(outcome.callsign, "JA1ABC");
    assert!(matches!(
        outcome.outcome,
        crate::message_bus::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout)
    ));
    assert_eq!(outcome.last_state, "Failed");
    assert_eq!(
        outcome.brief_timeline.last().map(String::as_str),
        Some("Failed: watchdog timeout")
    );
}
