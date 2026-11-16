// PAN-6. Formerly the inline module `pan6_diagnostic_tests`.
use super::*;
use pancetta_core::slot::SlotParity;
use pancetta_qso::{CallInitiation, QsoMetadata, QsoState};

/// PAN-259: a Hound completion is never stashed for the auto-73 resend,
/// so a duplicate RR73 from the Fox never makes a Hound send 73.
#[test]
fn hound_completion_is_not_stashed_for_auto_73() {
    let mut md = metadata(CallInitiation::Manual, 1, 0);
    assert!(stashes_auto_73(&md));
    md.hound = true;
    assert!(!stashes_auto_73(&md));
    let auto = metadata(CallInitiation::Auto, 1, 0);
    assert!(!stashes_auto_73(&auto));
}

fn metadata(initiated_by: CallInitiation, call_count: u32, age_minutes: i64) -> QsoMetadata {
    let now = chrono::Utc::now();
    QsoMetadata {
        qso_id: pancetta_qso::QsoId::new_v4(),
        our_callsign: "K5ARH".into(),
        their_callsign: Some("W1AW".into()),
        frequency: 14_074_000.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".into(),
        first_answer_at: None,
        start_time: now,
        end_time: None,
        reports: Default::default(),
        grids: Default::default(),
        their_state: None,
        contest_info: None,
        tags: Default::default(),
        adif_field_types: Default::default(),
        notes: None,
        tx_parity: None,
        tx_parity_provisional: false,
        initiated_by,
        role: Default::default(),
        call_count,
        first_call_at: Some(now - chrono::Duration::minutes(age_minutes)),
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
        remote_origin: false,
        remote_client_key_id: None,
    }
}

fn calling() -> QsoState {
    QsoState::CallingCq {
        frequency: 14_074_000.0,
        started_at: chrono::Utc::now(),
        call_count: 1,
    }
}

#[test]
fn call_cap_wording_excludes_time_bound() {
    let detail = timeout_detail(
        Some(&calling()),
        &metadata(CallInitiation::Manual, 25, 10),
        25,
        5,
    );
    assert!(detail.contains("25 calls"));
    assert!(!detail.contains("5 min"));
}

#[test]
fn time_bound_wording_excludes_call_count() {
    let detail = timeout_detail(
        Some(&calling()),
        &metadata(CallInitiation::Manual, 2, 6),
        25,
        5,
    );
    assert!(detail.contains("5 min"));
    assert!(!detail.contains("2 calls"));
}

#[test]
fn engaged_qso_names_report_stage_before_elapsed_bound() {
    let state = QsoState::WaitingForReport {
        their_callsign: "W1AW".into(),
        frequency: 14_074_000.0,
        started_at: chrono::Utc::now(),
        their_grid: None,
        our_report: -10,
    };
    assert_eq!(
        timeout_detail(Some(&state), &metadata(CallInitiation::Manual, 2, 6), 25, 5),
        "watchdog timeout — no reply after we sent the report"
    );
}

#[test]
fn auto_pounce_and_ambiguous_fallback_are_pinned() {
    assert_eq!(
        timeout_detail(
            Some(&calling()),
            &metadata(CallInitiation::Auto, 1, 0),
            25,
            5
        ),
        "watchdog timeout — autonomous pounce never answered"
    );
    assert_eq!(
        timeout_detail(None, &metadata(CallInitiation::Auto, 1, 0), 25, 5),
        "watchdog timeout"
    );
    assert!(format!(
        "QSO failed: {}",
        timeout_detail(None, &metadata(CallInitiation::Auto, 1, 0), 25, 5)
    )
    .starts_with("QSO failed: "));
}

#[test]
fn all_skip_sites_have_operator_facing_text_without_counter_prefix_collisions() {
    let diagnostics = [
        skip_diagnostic(SkipSite::CrossParityDeferral {
            callsign: Some("W1AW".into()),
            active_side: Some(SlotParity::Even),
            wanted: Some(SlotParity::Odd),
        }),
        skip_diagnostic(SkipSite::AutonomousOpenFailed {
            callsign: None,
            error: "busy".into(),
        }),
        skip_diagnostic(SkipSite::AutoAnswerCrossParity {
            callsign: "W1AW".into(),
            active_side: Some(SlotParity::Even),
        }),
        skip_diagnostic(SkipSite::AutoAnswerAtCapacity {
            callsign: "W1AW".into(),
            active: 2,
            cap: 2,
            fox_mode: true,
        }),
    ];
    assert!(diagnostics.iter().all(|d| !d.text.is_empty()));
    assert!(diagnostics.iter().all(|d| !d.text.starts_with("QSO with")
        && !d.text.starts_with("QSO failed:")
        && !d.text.starts_with("dropping stale")));
}

/// PAN-78: exact wording of the manual-open capacity refusal — the
/// Diagnostics entry, the status-bar line and the remote reason — for
/// every entry point, Fox mode on and off.
#[test]
fn manual_open_at_capacity_wording() {
    use pancetta_core::DiagnosticLevel;
    let cases: &[(ManualOpen, Option<&str>, &str, &str)] = &[
        (
            ManualOpen::Call,
            Some("K2BBB"),
            "call to K2BBB",
            "calling K2BBB",
        ),
        (
            ManualOpen::Reply,
            Some("K2BBB"),
            "reply to K2BBB",
            "replying to K2BBB",
        ),
        (
            ManualOpen::Hound,
            Some("K2BBB"),
            "Hound call to K2BBB",
            "calling Fox K2BBB",
        ),
        (ManualOpen::Cq, None, "CQ", "calling CQ"),
    ];
    for &(open, callsign, what, verb) in cases {
        for (fox_mode, limit) in [(false, "max_concurrent_qsos"), (true, "Fox mode")] {
            let snap = CapacitySnapshot {
                active: 1,
                cap: 1,
                fox_mode,
            };
            let d = skip_diagnostic(SkipSite::ManualOpenAtCapacity {
                open,
                callsign: callsign.map(str::to_string),
                active: snap.active,
                cap: snap.cap,
                fox_mode,
            });
            assert_eq!(
                d.text,
                format!("Refused {what} — at capacity 1/1 ({limit})")
            );
            assert_eq!(d.level, DiagnosticLevel::Warn);
            assert_eq!(d.target, "qso");
            if open == ManualOpen::Cq {
                assert_eq!(d.callsign, None, "a CQ has no callsign to attribute");
            } else {
                assert_eq!(d.callsign.as_deref(), callsign);
            }
            assert_eq!(
                manual_capacity_status(open, callsign, snap),
                format!("Not {verb} — 1/1 QSOs active ({limit}); finish or abort one first")
            );
            assert_eq!(
                manual_capacity_remote_reason(open, callsign, snap),
                format!("{what} refused — at capacity 1/1 ({limit})")
            );
        }
    }
}

#[tokio::test]
async fn a_skip_diagnostic_reaches_the_tui_channel() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
    emit_skip_diagnostic(
        &bus,
        SkipSite::AutoAnswerAtCapacity {
            callsign: "W1AW".into(),
            active: 2,
            cap: 2,
            fox_mode: false,
        },
    )
    .await;
    let message = receiver.try_recv().expect("skip diagnostic");
    assert_eq!(message.source, ComponentId::Qso);
    match message.message_type {
        MessageType::DiagnosticEvent { callsign, text, .. } => {
            assert_eq!(callsign.as_deref(), Some("W1AW"));
            assert!(text.contains("at capacity 2/2"));
        }
        other => panic!("expected DiagnosticEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_failed_notification_sends_exactly_once_when_delivered_immediately() {
    let bus = MessageBus::new(16).unwrap();
    let (_sender, receiver) = bus.create_channel(ComponentId::Autonomous).await.unwrap();
    let pending = std::sync::Mutex::new(Vec::new());

    notify_autonomous_cq_dispatch_failed_if_self_cq(&bus, &None, Some(42), &pending).await;

    let message = receiver
        .try_recv()
        .expect("exactly one dispatch-failed message");
    match message.message_type {
        MessageType::QsoMessage(crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
            attempt_id,
        }) => assert_eq!(attempt_id, 42),
        other => panic!("expected AutonomousCqDispatchFailed, got {other:?}"),
    }
    assert!(
        receiver.try_recv().is_err(),
        "must send exactly once when the first attempt is delivered, not keep retrying"
    );
    assert!(
        pending.lock().unwrap().is_empty(),
        "the queue-independent fallback must NOT be used when the bus delivery succeeded"
    );
}

/// PAN-43/45 follow-up (Codex round 1 on PR #342): a single unchecked
/// `send_message` used to be this notification's only delivery
/// attempt. If the Autonomous channel happened to be transiently full
/// -- exactly the condition PAN-45's registration-decline path can
/// itself trigger -- the notification silently dropped and the
/// speculative streak/offset mutation it exists to roll back was
/// never corrected. This must now retry past a transient full channel
/// instead of giving up on the first attempt.
#[tokio::test]
async fn dispatch_failed_notification_retries_past_a_transient_full_channel() {
    let bus = MessageBus::new(1).unwrap(); // capacity 1 -- easy to fill
    let (sender, receiver) = bus.create_channel(ComponentId::Autonomous).await.unwrap();

    // Fill the one slot with an unrelated message so the notification's
    // FIRST send attempt is guaranteed to see a full channel.
    sender
        .try_send(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Autonomous,
            MessageType::QsoMessage(crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
                attempt_id: 0, // unrelated filler, distinguishable from attempt_id below
            }),
            Instant::now(),
        ))
        .expect("fill the one slot");

    // Drain that filler shortly after the notification's first (failed)
    // attempt but well before its bounded retries give up -- simulates
    // the transient congestion clearing on its own.
    let receiver_for_drain = receiver.clone();
    tokio::spawn(async move {
        loop {
            if receiver_for_drain.try_recv().is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let pending = std::sync::Mutex::new(Vec::new());
    notify_autonomous_cq_dispatch_failed_if_self_cq(&bus, &None, Some(99), &pending).await;

    let message = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(m) = receiver.try_recv() {
                return m;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the real AutonomousCqDispatchFailed must eventually arrive");
    match message.message_type {
        MessageType::QsoMessage(crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
            attempt_id,
        }) => assert_eq!(
            attempt_id, 99,
            "the retried send must be the real notification, not the filler"
        ),
        other => panic!("expected AutonomousCqDispatchFailed, got {other:?}"),
    }
    assert!(
        pending.lock().unwrap().is_empty(),
        "the queue-independent fallback must NOT be used when a retry succeeded"
    );
}

/// PAN-43/45 follow-up (Codex round 2 on PR #342) regression: a
/// destination that stays unreachable through every bounded
/// message-bus retry (here: no channel registered at all, simulating
/// sustained congestion outlasting the ~75ms retry budget) must fall
/// back to the queue-independent pending-failures list instead of
/// silently losing the rollback forever -- and must still return in
/// bounded time, not hang.
#[tokio::test]
async fn dispatch_failed_notification_falls_back_to_the_pending_list_when_never_delivered_via_the_bus(
) {
    let bus = MessageBus::new(16).unwrap(); // no Autonomous channel registered
    let pending = std::sync::Mutex::new(Vec::new());
    tokio::time::timeout(
        Duration::from_secs(2),
        notify_autonomous_cq_dispatch_failed_if_self_cq(&bus, &None, Some(7), &pending),
    )
    .await
    .expect("must give up on the bus after its bounded retries, not hang indefinitely");
    assert_eq!(
        *pending.lock().unwrap(),
        vec![7],
        "a rollback that never got through the bus must land in the queue-independent \
         fallback instead of being lost"
    );
}
