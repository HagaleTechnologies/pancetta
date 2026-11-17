//! PAN-386: Fox-role QSOs (the Fox CQ and each Hound answer in FT8 Fox
//! mode) are marked after creation, split out of the active offsets for
//! slot selection, pinned to their slot, and never drift-relatched.
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;

const FOX: &str = "KH7Z";

fn manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: FOX.into(),
        our_grid: Some("BK29".into()),
        ..QsoManagerConfig::default()
    })
}

async fn answer(manager: &QsoManager, hound: &str, slot: f64, called_at: f64) -> QsoId {
    manager
        .respond_to_caller(
            hound.into(),
            slot,
            Some(SlotParity::Odd),
            ResponseStep::Report,
            Some(-8.0),
            None,
            Some(called_at),
            false,
            None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn mark_fox_role_sets_the_flag_once() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1500.0).await;
    assert!(manager.mark_fox_role(id).await);
    assert!(manager.get_qso(id).await.unwrap().metadata.fox);

    // Unknown id.
    assert!(!manager.mark_fox_role(Uuid::new_v4()).await);

    // A Hound QSO is never Fox-role.
    let hound = answer(&manager, "K1ABC", 1200.0, 1300.0).await;
    manager
        .qsos
        .write()
        .await
        .get_mut(&hound)
        .unwrap()
        .metadata
        .hound = true;
    assert!(!manager.mark_fox_role(hound).await);
    assert!(!manager.get_qso(hound).await.unwrap().metadata.fox);
}

#[tokio::test]
async fn active_tx_offsets_split_by_fox_role() {
    let manager = manager();
    let fox = answer(&manager, "W9XYZ", 360.0, 1240.0).await;
    assert!(manager.mark_fox_role(fox).await);
    let _ordinary = answer(&manager, "K1ABC", 1500.0, 1500.0).await;
    let (fox_offsets, others) = manager.active_tx_offsets_by_fox_role().await;
    assert_eq!(fox_offsets, vec![360.0]);
    assert_eq!(others, vec![1500.0]);
}

#[tokio::test]
async fn fox_stream_is_pinned_to_its_slot() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1240.0).await;
    assert!(manager.mark_fox_role(id).await);
    let progress = manager.get_qso(id).await.unwrap();
    assert_eq!(
        hound_switch_range_hz(&progress, manager.config()),
        Some((360.0, 360.0))
    );

    let generation = progress.metadata.advance_generation;
    let result = manager
        .apply_tx_offset_switch(
            id,
            600.0,
            OffsetRelocationOrigin::StallDetected {
                raised_at_generation: generation,
            },
        )
        .await;
    assert!(
        matches!(
            result,
            Err(QsoManagerError::OffsetActionOutsideHoundRegion { .. })
        ),
        "a Fox stream must refuse a move off its slot, got {result:?}"
    );
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.frequency, 360.0);
}

#[tokio::test]
async fn ordinary_stream_has_no_switch_window() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 1500.0, 1500.0).await;
    let progress = manager.get_qso(id).await.unwrap();
    assert_eq!(hound_switch_range_hz(&progress, manager.config()), None);
}

#[tokio::test]
async fn completed_fox_stream_is_not_rekeyable() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1240.0).await;
    assert!(manager.mark_fox_role(id).await);
    {
        let mut qsos = manager.qsos.write().await;
        let p = qsos.get_mut(&id).unwrap();
        p.metadata.initiated_by = CallInitiation::Manual;
        p.state = QsoState::Completed {
            their_callsign: "W9XYZ".into(),
            their_report: -8,
            our_report: -8,
            frequency: 360.0,
            grid_square: None,
            completed_at: Utc::now(),
            duration_seconds: 30,
        };
    }
    assert!(!manager.completed_close_rekeyable_now(id, 360.0).await);

    // Control: the same completed QSO without the Fox flag is re-keyable, so
    // the refusal above comes from the Fox exclusion alone.
    manager
        .qsos
        .write()
        .await
        .get_mut(&id)
        .unwrap()
        .metadata
        .fox = false;
    assert!(manager.completed_close_rekeyable_now(id, 360.0).await);
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

fn last_send(events: &[QsoEvent]) -> Option<(Option<TxAttemptId>, MessageType)> {
    events.iter().rev().find_map(|e| match e {
        QsoEvent::MessageToSend {
            attempt_id,
            message,
            ..
        } => Some((*attempt_id, message.clone())),
        _ => None,
    })
}

/// The Hound W9XYZ called at 1500 Hz; the Fox answered on its 360 Hz slot.
/// A WSJT-X Hound then sends its R-report at 360 Hz.
async fn hound_sends_r_report_at_our_slot(fox_role: bool) -> (QsoManager, QsoId, Vec<QsoEvent>) {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1500.0).await;
    if fox_role {
        assert!(manager.mark_fox_role(id).await);
    }
    let mut rx = manager.subscribe();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: FOX.into(),
                from_station: "W9XYZ".into(),
                report: -8,
            },
            format!("{FOX} W9XYZ R-08"),
            360.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    let events = drain(&mut rx);
    (manager, id, events)
}

#[tokio::test]
async fn fox_stream_hears_the_r_report_on_its_own_slot() {
    let (_manager, _id, events) = hound_sends_r_report_at_our_slot(true).await;
    let (_, message) = last_send(&events).expect("the Fox answers the R-report");
    assert!(
        matches!(message, MessageType::FinalConfirmation { .. }),
        "expected RR73, got {message:?}"
    );
}

#[tokio::test]
async fn non_fox_split_qso_still_drops_a_reply_at_our_offset() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1500.0).await;
    let before = manager.get_qso(id).await.unwrap().state;
    let mut rx = manager.subscribe();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: FOX.into(),
                from_station: "W9XYZ".into(),
                report: -8,
            },
            format!("{FOX} W9XYZ R-08"),
            360.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    assert!(last_send(&drain(&mut rx)).is_none());
    assert_eq!(manager.get_qso(id).await.unwrap().state, before);
}

#[tokio::test]
async fn fox_r_report_completes_when_rr73_airs() {
    let (manager, id, events) = hound_sends_r_report_at_our_slot(true).await;
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    let (attempt, _) = last_send(&events).unwrap();
    let attempt = attempt.expect("the RR73 carries an attempt id");
    assert_eq!(
        completions(&events),
        0,
        "nothing is logged before RR73 airs"
    );

    let mut rx = manager.subscribe();
    let payload = UnsentAttemptPayload {
        frequency_hz: 360.0,
        remote_binding: None,
    };
    assert_eq!(
        manager
            .confirm_completion_aired(id, Some(attempt), "", &payload, Utc::now())
            .await,
        CompletionAired::Committed
    );
    assert_eq!(completions(&drain(&mut rx)), 1);
    assert_eq!(
        manager
            .confirm_completion_aired(id, Some(attempt), "", &payload, Utc::now())
            .await,
        CompletionAired::NotPending
    );
    assert_eq!(completions(&drain(&mut rx)), 0);
}

#[tokio::test]
async fn fox_rr73_not_aired_reopens_the_qso() {
    let manager = manager();
    let id = answer(&manager, "W9XYZ", 360.0, 1500.0).await;
    assert!(manager.mark_fox_role(id).await);
    let before = manager.get_qso(id).await.unwrap().state;
    let mut rx = manager.subscribe();
    manager
        .process_message(
            MessageType::ReportAck {
                to_station: FOX.into(),
                from_station: "W9XYZ".into(),
                report: -8,
            },
            format!("{FOX} W9XYZ R-08"),
            360.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    let (attempt, _) = last_send(&drain(&mut rx)).unwrap();
    assert!(matches!(
        manager
            .resolve_unsent_completion_at(id, attempt, Utc::now())
            .await,
        UnsentCompletion::Reverted(_)
    ));
    assert_eq!(manager.get_qso(id).await.unwrap().state, before);
    assert_eq!(completions(&drain(&mut rx)), 0);
}

#[tokio::test]
async fn fox_relevance_classification_matches_the_gate() {
    let r_report = MessageType::ReportAck {
        to_station: FOX.into(),
        from_station: "W9XYZ".into(),
        report: -8,
    };
    for fox_role in [true, false] {
        let manager = manager();
        let id = answer(&manager, "W9XYZ", 360.0, 1500.0).await;
        if fox_role {
            assert!(manager.mark_fox_role(id).await);
        }
        let progress = manager.get_qso(id).await.unwrap();
        let gate = manager.is_message_relevant(
            &progress.state,
            &progress.metadata,
            &r_report,
            360.0,
            false,
        );
        let verdict = manager.classify_relevance(
            &progress.state,
            &progress.metadata,
            &r_report,
            360.0,
            false,
        );
        assert_eq!(gate, fox_role, "gate verdict for fox_role={fox_role}");
        assert_eq!(
            verdict.relevant, gate,
            "classify_relevance must agree with is_message_relevant (fox_role={fox_role})"
        );
    }
}

#[tokio::test]
async fn fox_stream_skips_drift_relatch() {
    let frame = MessageType::SignalReport {
        to_station: FOX.into(),
        from_station: "W9XYZ".into(),
        report: -8,
    };
    for fox_role in [true, false] {
        let manager = manager();
        let id = answer(&manager, "W9XYZ", 360.0, 1240.0).await;
        if fox_role {
            assert!(manager.mark_fox_role(id).await);
        }
        let t0 = manager.now_utc();
        manager
            .maybe_confirm_frequency_drift_at(&frame, 700.0, t0)
            .await;
        manager
            .maybe_confirm_frequency_drift_at(&frame, 700.0, t0 + chrono::Duration::seconds(6))
            .await;
        let after = manager.get_qso(id).await.unwrap();
        if fox_role {
            assert_eq!(after.metadata.partner_freq, Some(1240.0));
            assert_eq!(after.metadata.frequency, 360.0);
            assert_eq!(after.metadata.pending_freq_drift, None);
        } else {
            // Control: the same sightings relatch an ordinary split QSO, so
            // the Fox case above is held by the Fox skip alone.
            assert_eq!(after.metadata.partner_freq, Some(700.0));
        }
    }
}

/// A Fox-role QSO whose RR73 is still closing keeps its slot, so the next
/// Hound is not answered on top of the RR73 (PAN-386).
#[tokio::test]
async fn completing_fox_stream_keeps_its_slot() {
    let (manager, id, _events) = hound_sends_r_report_at_our_slot(true).await;
    assert!(matches!(
        manager.get_qso(id).await.unwrap().state,
        QsoState::Completed { .. }
    ));
    let (fox_offsets, _others) = manager.active_tx_offsets_by_fox_role().await;
    assert_eq!(fox_offsets, vec![360.0]);
    assert_eq!(crate::fox::fox_slot_offset(&fox_offsets, &[]), Some(300.0));
}
