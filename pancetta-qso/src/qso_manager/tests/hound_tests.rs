use super::*;
use crate::states::{GridSquares, SignalReports};
use chrono::Utc;
use pancetta_core::slot::SlotParity;
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

// ── QsoMetadata default field values ────────────────────────────────────

fn make_metadata() -> QsoMetadata {
    let now = Utc::now();
    QsoMetadata {
        qso_id: Uuid::new_v4(),
        our_callsign: "K5ARH".into(),
        their_callsign: Some("KH8/K5ARH".into()),
        frequency: 600.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".into(),
        first_answer_at: None,
        start_time: now,
        end_time: None,
        reports: SignalReports::default(),
        grids: GridSquares::default(),
        their_state: None,
        contest_info: None,
        tags: HashMap::new(),
        adif_field_types: HashMap::new(),
        notes: None,
        tx_parity: Some(SlotParity::Even),
        initiated_by: CallInitiation::Manual,
        role: QsoRole::Caller,
        call_count: 1,
        first_call_at: Some(now),
        last_call_at: Some(now),
        progressed_this_cycle: false,
        stall_cycles: 0,
        last_known_good_offset_hz: None,
        advance_generation: 0,
        pre_switch_offset: None,
        hound: false,
        partner_freq: None,
        pending_freq_drift: None,
        hound_qsyed: false,
        fox: false,
        remote_origin: false,
        remote_client_key_id: None,
        tx_parity_provisional: false,
    }
}

#[test]
fn qso_metadata_hound_defaults_false() {
    let md = make_metadata();
    assert!(!md.hound, "hound must default to false");
}

#[test]
fn qso_metadata_partner_freq_defaults_none() {
    let md = make_metadata();
    assert_eq!(md.partner_freq, None, "partner_freq must default to None");
}

#[test]
fn qso_metadata_hound_qsyed_defaults_false() {
    let md = make_metadata();
    assert!(!md.hound_qsyed, "hound_qsyed must default to false");
}

// ── hound_offset_for ────────────────────────────────────────────────────

#[test]
fn hound_offset_in_range() {
    for seed in &["K5ARH", "VK9X", "KH8B", "FT5ZM", "3B9FR"] {
        let off = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        assert!(
            (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(&off),
            "offset {off} out of range [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}] for seed {seed}"
        );
    }
}

#[test]
fn hound_offset_deterministic() {
    let seed = "K5ARH";
    let a = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
    let b = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
    assert_eq!(a, b, "same seed must produce same offset (determinism)");
}

#[test]
fn hound_offset_spreads_distinct_seeds() {
    // These two seeds must produce different offsets under FNV-1a.
    let a = hound_offset_for("K5ARH", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
    let b = hound_offset_for("VK9X", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
    assert_ne!(a, b, "distinct seeds must produce distinct offsets");
}

#[test]
fn hound_offset_snapped_to_5hz() {
    for seed in &["K5ARH", "VK9X", "KH8B", "FT5ZM", "ZL9A"] {
        let off = hound_offset_for(seed, HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
        let remainder = off % 5.0;
        assert!(
            remainder.abs() < 1e-9,
            "offset {off} for seed {seed} is not a multiple of 5 Hz (remainder {remainder})"
        );
    }
}

#[test]
fn hound_offset_degenerate_lo_eq_hi() {
    let result = hound_offset_for("K5ARH", 500.0, 500.0);
    assert_eq!(result, 500.0, "lo == hi must return lo");
}

// ── engage_hound constructor ─────────────────────────────────────────────

fn hound_test_config() -> QsoManagerConfig {
    QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM20".to_string()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
        activation: crate::sig_reference::ActivationReference::default(),
    }
}

/// engage_hound creates a QSO in RespondingToCq with correct Hound metadata.
///
/// Verifies:
/// - state == RespondingToCq
/// - metadata.hound == true
/// - metadata.partner_freq == Some(fox_freq)
/// - metadata.frequency in [300.0, 900.0]  (the low calling region)
/// - metadata.initiated_by == Manual
/// - metadata.role == Caller
/// - tx_parity == Some(SlotParity::Odd)  (opposite of Even DX parity)
#[tokio::test]
async fn engage_hound_creates_qso_with_correct_metadata() {
    let manager = QsoManager::new(hound_test_config());

    let qso_id = manager
        .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound should succeed");

    let progress = manager.get_qso(qso_id).await.unwrap();

    // State
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "expected RespondingToCq, got {:?}",
        progress.state
    );

    // Hound-specific fields
    assert!(
        progress.metadata.hound,
        "metadata.hound must be true for a Hound QSO"
    );
    assert_eq!(
        progress.metadata.partner_freq,
        Some(1800.0),
        "metadata.partner_freq must be Some(fox_freq)"
    );

    // Low calling offset in [300, 900]
    let freq = progress.metadata.frequency;
    assert!(
        (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(&freq),
        "metadata.frequency {freq} must be in [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}]"
    );

    // initiation / role
    assert_eq!(
        progress.metadata.initiated_by,
        CallInitiation::Manual,
        "Hound v1 QSO must be Manual"
    );
    assert_eq!(
        progress.metadata.role,
        QsoRole::Caller,
        "Hound is the Caller (we chase the Fox)"
    );

    // tx_parity = opposite of dx_parity (Even → Odd)
    assert_eq!(
        progress.metadata.tx_parity,
        Some(SlotParity::Odd),
        "tx_parity must be opposite of Fox's Even parity"
    );
}

/// engage_hound emits an opening MessageToSend on the LOW calling offset
/// whose text targets the Fox callsign and includes our station callsign.
#[tokio::test]
async fn engage_hound_emits_opening_message_on_low_offset() {
    let manager = QsoManager::new(hound_test_config());
    let mut events = manager.subscribe();

    let qso_id = manager
        .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound should succeed");

    // Collect all events emitted during the call.
    let mut msg_events: Vec<(f64, MessageType)> = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let QsoEvent::MessageToSend {
            qso_id: eid,
            message,
            frequency,
            ..
        } = ev
        {
            if eid == qso_id {
                msg_events.push((frequency, message));
            }
        }
    }

    assert!(
        !msg_events.is_empty(),
        "at least one MessageToSend must be emitted for the opening call"
    );

    // The first MessageToSend is our CqResponse call to the Fox.
    let (event_freq, message) = &msg_events[0];

    // Frequency must be in the low calling region.
    assert!(
        (HOUND_CALL_MIN_HZ..=HOUND_CALL_MAX_HZ).contains(event_freq),
        "opening MessageToSend frequency {event_freq} must be in the low calling region [{HOUND_CALL_MIN_HZ}, {HOUND_CALL_MAX_HZ}]"
    );

    // Message must be CqResponse directed at the Fox with our callsign.
    match message {
        MessageType::CqResponse {
            calling_station,
            responding_station,
            ..
        } => {
            assert_eq!(
                calling_station, "D2UY",
                "CqResponse calling_station must be the Fox"
            );
            assert_eq!(
                responding_station, "K5ARH",
                "CqResponse responding_station must be our callsign"
            );
        }
        other => panic!("expected CqResponse, got {:?}", other),
    }
}

/// engage_hound is deterministic: two calls for the same Fox callsign yield
/// the same low calling offset.
#[tokio::test]
async fn engage_hound_same_fox_same_low_offset() {
    let manager = QsoManager::new(hound_test_config());

    let a = manager
        .engage_hound("D2UY", 1800.0, None, Some(SlotParity::Even))
        .await
        .expect("first engage_hound");
    // The second call supersedes the first (same callsign, same band) —
    // that's fine, we just need both to agree on the offset.
    let b = manager
        .engage_hound("D2UY", 1800.0, None, Some(SlotParity::Even))
        .await
        .expect("second engage_hound");

    let freq_a = manager.get_qso(a).await.map(|p| p.metadata.frequency);
    let freq_b = manager.get_qso(b).await.unwrap().metadata.frequency;

    // If a was superseded its progress may be gone (Error); b always exists.
    // Either way both were computed from the same seed so they're identical.
    let expected_offset = hound_offset_for("D2UY", HOUND_CALL_MIN_HZ, HOUND_CALL_MAX_HZ);
    assert_eq!(
        freq_b, expected_offset,
        "second QSO offset must equal hound_offset_for(D2UY)"
    );
    if let Ok(fa) = freq_a {
        assert_eq!(
            fa, expected_offset,
            "first QSO offset must equal hound_offset_for(D2UY)"
        );
    }
}

/// engage_hound with dx_parity=None (Fox's parity unknown) now LATCHES a
/// concrete provisional parity via `engage_hound` → `respond_to_cq_with`'s
/// `None`-dx_parity fallback (Batch 2 remediation), instead of leaving
/// `tx_parity` permanently `None` (the old behavior, which made the TX
/// scheduler re-resolve "nearest next slot" independently every
/// subsequent slot and alternate parity — the exact bug this fixes).
#[tokio::test]
async fn engage_hound_no_parity_when_dx_parity_unknown() {
    let manager = QsoManager::new(hound_test_config());

    let qso_id = manager
        .engage_hound("VK9M", 1500.0, None, None)
        .await
        .expect("engage_hound with no parity");

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.tx_parity.is_some(),
        "tx_parity should be latched to a concrete parity even when \
         dx_parity is None (provisional latch, not left None forever)"
    );
    assert!(
        progress.metadata.tx_parity_provisional,
        "a parity latched with no observed dx_parity must be marked provisional"
    );
    assert!(
        progress.metadata.hound,
        "hound flag must still be set even with no parity"
    );
}

// ── Hound QSY-on-report (Task 5) ────────────────────────────────────────

/// Helper: drain all pending events from the broadcast receiver.
fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// Collect all `MessageToSend` events from a drained list.
fn message_to_sends(events: &[QsoEvent]) -> Vec<(f64, MessageType)> {
    events
        .iter()
        .filter_map(|e| match e {
            QsoEvent::MessageToSend {
                message, frequency, ..
            } => Some((*frequency, message.clone())),
            _ => None,
        })
        .collect()
}

fn fox_report(fox: &str, report: i8) -> MessageType {
    MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: fox.into(),
        report,
    }
}

fn fox_rr73(fox: &str) -> MessageType {
    MessageType::FinalConfirmation {
        from_station: fox.into(),
        to_station: "K5ARH".into(),
    }
}

fn seventy_threes(events: &[QsoEvent]) -> usize {
    message_to_sends(events)
        .iter()
        .filter(|(_, m)| matches!(m, MessageType::SeventyThree { .. }))
        .count()
}

/// PAN-259 scenario 1: a Hound chasing a Fox at 600 Hz calls between
/// 1000 and 2700 Hz, where a WSJT-X Fox listens.
#[tokio::test]
async fn hound_calls_a_600_hz_fox_between_1000_and_2700_hz() {
    let manager = QsoManager::new(hound_test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .engage_hound("D2UY", 600.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");
    let sends = message_to_sends(&drain_events(&mut rx));
    let (open_freq, open_msg) = sends.first().expect("opening call must be emitted");
    assert!(
        (1000.0..=2700.0).contains(open_freq),
        "opening call at {open_freq} Hz must be in 1000-2700 Hz"
    );
    assert!(matches!(open_msg, MessageType::CqResponse { .. }));
    let freq = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
    assert!((1000.0..=2700.0).contains(&freq));
}

/// T5-1 / PAN-259 scenarios 1 and 2: full Hound exchange end-to-end.
///
/// The Hound calls a 600 Hz Fox in 1000-2700 Hz. The Fox's report moves
/// every frequency mirror to exactly 600 Hz and the R-report rides it. The
/// Fox's RR73 completes the QSO with no 73, and a duplicate RR73 sends no
/// 73 either.
#[tokio::test]
async fn hound_qsy_on_fox_report_full_exchange() {
    let manager = QsoManager::new(hound_test_config());
    let mut rx = manager.subscribe();

    let qso_id = manager
        .engage_hound("D2UY", 600.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");

    let opening_sends = message_to_sends(&drain_events(&mut rx));
    let (open_freq, _) = opening_sends.first().expect("opening call");
    assert!(
        (1000.0..=2700.0).contains(open_freq),
        "opening call freq {open_freq} must be in 1000-2700 Hz"
    );

    manager
        .process_message(
            fox_report("D2UY", -12),
            "K5ARH D2UY -12".into(),
            600.0,
            Some(-12.0),
        )
        .await
        .expect("process_message for Fox SignalReport must succeed");

    let after_report_events = drain_events(&mut rx);
    let progress = manager.get_qso(qso_id).await.unwrap();
    match &progress.state {
        QsoState::SendingReport { frequency, .. } => assert_eq!(*frequency, 600.0),
        other => panic!("state must be SendingReport after Fox report, got {other:?}"),
    }
    assert!(progress.metadata.hound_qsyed);
    assert_eq!(
        progress.metadata.frequency, 600.0,
        "TX moves to the Fox offset"
    );
    assert_eq!(progress.metadata.partner_freq, Some(600.0));
    assert_eq!(progress.metadata.last_known_good_offset_hz, Some(600.0));

    let sends = message_to_sends(&after_report_events);
    let report_acks: Vec<_> = sends
        .iter()
        .filter(|(_, m)| matches!(m, MessageType::ReportAck { .. }))
        .collect();
    let (ack_freq, ack_msg) = report_acks.first().expect("a ReportAck must be emitted");
    assert_eq!(*ack_freq, 600.0, "R-report must ride the Fox's own offset");
    match ack_msg {
        MessageType::ReportAck {
            to_station,
            from_station,
            ..
        } => {
            assert_eq!(to_station, "D2UY");
            assert_eq!(from_station, "K5ARH");
        }
        other => panic!("expected ReportAck, got {:?}", other),
    }

    manager
        .process_message(
            fox_rr73("D2UY"),
            "K5ARH D2UY RR73".into(),
            600.0,
            Some(-10.0),
        )
        .await
        .expect("process_message for Fox RR73 must succeed");
    let rr73_events = drain_events(&mut rx);
    let final_progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(final_progress.state, QsoState::Completed { .. }),
        "QSO must reach Completed after Fox RR73, got {:?}",
        final_progress.state
    );
    assert_eq!(seventy_threes(&rr73_events), 0, "a Hound sends no 73");
    let completions = rr73_events
        .iter()
        .filter(|e| matches!(e, QsoEvent::QsoCompleted { qso_id: id, .. } if *id == qso_id))
        .count();
    assert_eq!(completions, 1, "the QSO completes (and logs) exactly once");

    // Duplicate RR73: still no 73 and no second completion.
    let _ = manager
        .process_message(
            fox_rr73("D2UY"),
            "K5ARH D2UY RR73".into(),
            600.0,
            Some(-10.0),
        )
        .await;
    let dup_events = drain_events(&mut rx);
    assert_eq!(
        seventy_threes(&dup_events),
        0,
        "a duplicate RR73 sends no 73"
    );
    assert!(
        !dup_events
            .iter()
            .any(|e| matches!(e, QsoEvent::QsoCompleted { .. })),
        "a duplicate RR73 must not complete the QSO again"
    );
}

#[tokio::test]
async fn hound_later_report_waits_for_ptt_gate_before_relocation_and_publication() {
    let mut manager = QsoManager::new(hound_test_config());
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    manager.set_ptt_sync_gate_source(gate.clone());
    let id = manager
        .engage_hound("D2UY", 600.0, None, Some(SlotParity::Even))
        .await
        .unwrap();
    manager
        .process_message(
            fox_report("D2UY", -8),
            "K5ARH D2UY -08".into(),
            600.0,
            Some(-8.0),
        )
        .await
        .unwrap();
    let mut rx = manager.subscribe();
    let key = id.to_string();
    let before = manager.relocation_epochs.epoch(&key);
    let held = gate.lock().await;
    let mut moving = Box::pin(manager.process_message(
        fox_report("D2UY", -8),
        "K5ARH D2UY -08".into(),
        780.0,
        Some(-8.0),
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            moving.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    assert_eq!(manager.relocation_epochs.epoch(&key), before);
    assert!(message_to_sends(&drain_events(&mut rx)).is_empty());
    drop(held);
    moving.await.unwrap();
    assert!(manager.relocation_epochs.epoch(&key) > before);
    assert_eq!(manager.get_qso(id).await.unwrap().metadata.frequency, 780.0);
    assert!(message_to_sends(&drain_events(&mut rx))
        .iter()
        .all(|(hz, _)| *hz == 780.0));
}

/// PAN-259 scenario 2: a Hound calling at 1500 Hz answers the Fox's report
/// at the Fox's 600 Hz offset.
#[tokio::test]
async fn hound_calling_at_1500_sends_r_report_at_fox_offset() {
    let mut config = hound_test_config();
    config.hound.call_min_hz = 1500.0;
    config.hound.call_max_hz = 1500.0;
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();
    let qso_id = manager
        .engage_hound("D2UY", 600.0, None, Some(SlotParity::Even))
        .await
        .unwrap();
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.frequency,
        1500.0
    );
    drain_events(&mut rx);

    manager
        .process_message(
            fox_report("D2UY", -8),
            "K5ARH D2UY -08".into(),
            600.0,
            Some(-8.0),
        )
        .await
        .unwrap();
    let sends = message_to_sends(&drain_events(&mut rx));
    assert!(
        sends
            .iter()
            .any(|(f, m)| *f == 600.0 && matches!(m, MessageType::ReportAck { .. })),
        "R-report must go out at 600 Hz, got {sends:?}"
    );
}

/// PAN-259: a later addressed report from the Fox on another admissible
/// stream, more than the 100 Hz established tolerance away, re-anchors
/// the Hound onto that stream.
#[tokio::test]
async fn hound_follows_fox_report_to_another_stream_beyond_100_hz() {
    let manager = QsoManager::new(hound_test_config());
    let mut rx = manager.subscribe();
    let qso_id = manager
        .engage_hound("D2UY", 600.0, None, Some(SlotParity::Even))
        .await
        .unwrap();
    manager
        .process_message(
            fox_report("D2UY", -12),
            "K5ARH D2UY -12".into(),
            600.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    drain_events(&mut rx);

    manager
        .process_message(
            fox_report("D2UY", -12),
            "K5ARH D2UY -12".into(),
            780.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.frequency, 780.0);
    assert_eq!(progress.metadata.partner_freq, Some(780.0));
    assert_eq!(progress.metadata.last_known_good_offset_hz, Some(780.0));
    // A repeated report queues no immediate reply (PAN-129); the
    // per-slot resend reads `metadata.frequency`, now the new stream. No
    // R-report may go out on the old stream.
    let sends = message_to_sends(&drain_events(&mut rx));
    assert!(
        sends.iter().all(|(f, _)| *f == 780.0),
        "nothing may be sent on the Fox's old stream, got {sends:?}"
    );
}

/// PAN-259 (self-review): a report addressed to us by a station other
/// than the Fox, inside the Fox envelope, must not move the Hound.
#[tokio::test]
async fn hound_ignores_a_non_fox_report_inside_the_envelope() {
    let manager = QsoManager::new(hound_test_config());
    let qso_id = manager
        .engage_hound("D2UY", 600.0, None, Some(SlotParity::Even))
        .await
        .unwrap();
    manager
        .process_message(
            fox_report("D2UY", -12),
            "K5ARH D2UY -12".into(),
            600.0,
            Some(-12.0),
        )
        .await
        .unwrap();
    let _ = manager
        .process_message(
            fox_report("W1XYZ", -10),
            "K5ARH W1XYZ -10".into(),
            820.0,
            Some(-10.0),
        )
        .await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.frequency, 600.0);
    assert_eq!(progress.metadata.partner_freq, Some(600.0));
    assert_eq!(progress.metadata.last_known_good_offset_hz, Some(600.0));
}

/// PAN-259: a Fox report outside the response envelope, or at a
/// non-finite offset, is ignored: no R-report and no state advance.
#[tokio::test]
async fn hound_ignores_fox_report_outside_the_response_envelope() {
    for bad_hz in [1800.0, f64::NAN, 250.0] {
        let manager = QsoManager::new(hound_test_config());
        let mut rx = manager.subscribe();
        let qso_id = manager
            .engage_hound("D2UY", 600.0, None, Some(SlotParity::Even))
            .await
            .unwrap();
        drain_events(&mut rx);
        let _ = manager
            .process_message(
                fox_report("D2UY", -12),
                "K5ARH D2UY -12".into(),
                bad_hz,
                Some(-12.0),
            )
            .await;
        let progress = manager.get_qso(qso_id).await.unwrap();
        assert!(
            matches!(progress.state, QsoState::RespondingToCq { .. }),
            "a report at {bad_hz} Hz must not advance the exchange"
        );
        assert!(!progress.metadata.hound_qsyed);
        assert!(
            !message_to_sends(&drain_events(&mut rx))
                .iter()
                .any(|(_, m)| matches!(m, MessageType::ReportAck { .. })),
            "no R-report for a report at {bad_hz} Hz"
        );
    }
}

/// PAN-259: the report-driven offset is exactly the Fox offset, admitted
/// only inside the envelope and the TX range.
#[test]
fn hound_report_offset_is_exact_and_bounded() {
    let regions = HoundRegions::default();
    assert_eq!(hound_report_offset(600.0, &regions), Some(600.0));
    assert_eq!(hound_report_offset(612.5, &regions), Some(612.5));
    assert_eq!(hound_report_offset(300.0, &regions), Some(300.0));
    assert_eq!(hound_report_offset(900.0, &regions), Some(900.0));
    assert_eq!(hound_report_offset(1500.0, &regions), None);
    assert_eq!(hound_report_offset(f64::INFINITY, &regions), None);
    let wide = HoundRegions {
        response_min_hz: 100.0,
        ..HoundRegions::default()
    };
    assert_eq!(
        hound_report_offset(150.0, &wide),
        None,
        "below the transmittable TX range"
    );
}

/// PAN-259: a non-Hound exchange still closes RR73 with its normal 73.
#[tokio::test]
async fn non_hound_exchange_still_sends_final_73() {
    let manager = QsoManager::new(hound_test_config());
    let mut rx = manager.subscribe();
    manager
        .respond_to_cq_manual("D2UY".into(), 1500.0, None)
        .await
        .unwrap();
    manager
        .process_message(
            fox_report("D2UY", -9),
            "K5ARH D2UY -09".into(),
            1500.0,
            Some(-9.0),
        )
        .await
        .unwrap();
    drain_events(&mut rx);
    manager
        .process_message(
            fox_rr73("D2UY"),
            "K5ARH D2UY RR73".into(),
            1500.0,
            Some(-9.0),
        )
        .await
        .unwrap();
    assert_eq!(seventy_threes(&drain_events(&mut rx)), 1);
}

#[test]
fn hound_suppresses_only_the_terminal_73() {
    let seventy_three = MessageType::SeventyThree {
        to_station: "D2UY".into(),
        from_station: "K5ARH".into(),
    };
    assert!(hound_suppresses_reply(Some(&seventy_three)));
    assert!(!hound_suppresses_reply(Some(&fox_rr73("D2UY"))));
    assert!(!hound_suppresses_reply(None));
}

/// T5-2: QSY fires even in `TxFreqMode::Hold`.
///
/// The Hound QSY is procedure-mandated — independent of the autonomous
/// silence-driven stall detector's gate (`TxFreqMode::Auto`). In the
/// default Hold mode the Hound must still move to the Fox's offset on the
/// Fox's first report.
#[tokio::test]
async fn hound_qsy_fires_in_hold_mode() {
    let manager = QsoManager::new(hound_test_config());

    let qso_id = manager
        .engage_hound("D2UY", 600.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");

    let before_freq = manager.get_qso(qso_id).await.unwrap().metadata.frequency;
    assert!(
        (1000.0..=2700.0).contains(&before_freq),
        "before QSY, offset {before_freq} must be in the calling region"
    );

    manager
        .process_message(
            fox_report("D2UY", -12),
            "K5ARH D2UY -12".into(),
            600.0,
            Some(-12.0),
        )
        .await
        .expect("process_message for Fox SignalReport in Hold mode must succeed");

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.frequency, 600.0,
        "the Hound must move to the Fox offset even in Hold mode"
    );
    assert!(progress.metadata.hound_qsyed);
}

/// Task 6 (updated): engage_hound must NOT insert a bare "HOUND" key into
/// metadata.tags — that would produce an invalid `<HOUND:4>true` ADIF field
/// (non-`APP_`-prefixed) that can trip LoTW.  The COMMENT "HOUND" signal and
/// the `APP_PANCETTA_HOUND` machine-readable field are both written by
/// `AdifProcessor::qso_to_adif` from `metadata.hound` directly, so the tag
/// is redundant AND harmful.  What we do require is `metadata.hound == true`.
#[tokio::test]
async fn engage_hound_no_stray_hound_tag_in_metadata() {
    let manager = QsoManager::new(hound_test_config());

    let qso_id = manager
        .engage_hound("D2UY", 1800.0, Some("JI64"), Some(SlotParity::Even))
        .await
        .expect("engage_hound must succeed");

    let progress = manager.get_qso(qso_id).await.unwrap();

    // The hound flag must be set — it drives COMMENT+APP_PANCETTA_HOUND in ADIF.
    assert!(
        progress.metadata.hound,
        "metadata.hound must be true after engage_hound"
    );

    // The bare "HOUND" tag must NOT be present — it would generate an invalid
    // ADIF field `<HOUND:4>true` that can fail LoTW validation.
    assert!(
        !progress.metadata.tags.contains_key("HOUND"),
        "metadata.tags must NOT contain a bare 'HOUND' key (would produce invalid ADIF); got tags={:?}",
        progress.metadata.tags
    );
}

/// T5-3: Non-Hound Caller QSO is completely unaffected by the QSY hook.
///
/// A normal `respond_to_cq_manual` QSO (hound=false) receiving a
/// `SignalReport` from the DX must NOT move its TX offset — `metadata.frequency`
/// stays at the latched value, and `hound_qsyed` stays false.
#[tokio::test]
async fn non_hound_caller_qso_not_affected_by_qsy_hook() {
    let manager = QsoManager::new(hound_test_config());
    // Normal (non-Hound) QSO at 1500 Hz.
    let normal_freq = 1500.0;
    let qso_id = manager
        .respond_to_cq_manual("D2UY".into(), normal_freq, None)
        .await
        .expect("respond_to_cq_manual must succeed");

    let before = manager.get_qso(qso_id).await.unwrap();
    assert!(
        !before.metadata.hound,
        "hound must be false for a non-Hound QSO"
    );
    assert_eq!(
        before.metadata.frequency, normal_freq,
        "frequency must be the latched value before any report"
    );

    // DX sends a signal report.
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "D2UY".into(),
                report: -9,
            },
            "K5ARH D2UY -09".into(),
            normal_freq,
            Some(-9.0),
        )
        .await
        .expect("process_message for SignalReport must succeed");

    let after = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(after.state, QsoState::SendingReport { .. }),
        "state must advance to SendingReport"
    );
    // Frequency must NOT move.
    assert_eq!(
        after.metadata.frequency, normal_freq,
        "non-Hound QSO: metadata.frequency must NOT change on report (got {})",
        after.metadata.frequency
    );
    // hound_qsyed must stay false.
    assert!(
        !after.metadata.hound_qsyed,
        "non-Hound QSO: hound_qsyed must stay false"
    );
}
