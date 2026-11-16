use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[tokio::test]
async fn pan105_playback_wait_reports_aborted_by_disarm_at_capability_boundary() {
    use pancetta_agent::arm::{ArmState, ArmTime, ConsentChannel, VerifiedArmGrant};
    let base = ArmTime::now();
    let arm = Arc::new(std::sync::Mutex::new(ArmState::new()));
    {
        let mut state = arm.lock().unwrap();
        state.set_channel_consent(ConsentChannel::StationAgent, true, base.unix_ms);
        state.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: 900_000,
                armed_until_ms: base.unix_ms + 900_000,
                capability_exp_ms: base.unix_ms + 600_000,
                tx_enabled_until_ms: base.unix_ms + 600_000,
                effective_ttl_ms: 600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan105".into(),
                client_key_id: "client-a".into(),
            },
            base,
        );
    }
    let (_tx, rx) = crossbeam_channel::bounded(8);
    let inbox = TxInbox::new(rx, TxRequeue::default());
    let outcome = interruptible_sleep_or_supersede_with_clock(
        Duration::from_secs(1),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        &inbox,
        &[],
        &std::collections::HashMap::new(),
        Some((&arm, Some("client-a"))),
        &|| ArmTime {
            unix_ms: base.unix_ms + 600_000,
            ..base
        },
    )
    .await;
    assert!(matches!(outcome, SleepOutcome::AbortedByDisarm));
}

#[tokio::test]
async fn returns_false_and_waits_full_duration_when_not_aborted() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let start = tokio::time::Instant::now();
    let aborted = interruptible_sleep(Duration::from_millis(100), &shutdown, &abort).await;
    assert!(!aborted);
    assert!(start.elapsed() >= Duration::from_millis(100));
}

#[tokio::test]
async fn returns_true_promptly_when_abort_flips_mid_sleep() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let abort_clone = abort.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        abort_clone.store(true, Ordering::Release);
    });
    let start = tokio::time::Instant::now();
    let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
    assert!(aborted);
    // Should wake within the ~50ms poll granularity of when the flag flipped
    // (flag flips at ~20ms), not wait out the full 5s sleep.
    assert!(start.elapsed() < Duration::from_millis(200));
}

#[tokio::test]
async fn returns_true_promptly_when_shutdown_flips_mid_sleep() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown_clone.store(true, Ordering::Release);
    });
    let start = tokio::time::Instant::now();
    let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
    assert!(aborted);
    assert!(start.elapsed() < Duration::from_millis(200));
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_completes_normally_with_no_incoming_message() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();
    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(80),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;
    assert!(matches!(outcome, super::SleepOutcome::Completed));
}

/// Round-3 review (Codex P2): a message already siphoned into this
/// wait's local queue (an unrelated local `TuneRequest`, here) before a
/// later-arriving disarm is detected must be back in the inbox when the
/// `AbortedByDisarm` outcome returns, not be silently dropped. The
/// disarm's own caller-side PTT-off ordering is exercised in
/// `pancetta/tests/coord_sim.rs`'s end-to-end tests; this pins the
/// lower-level contract this function itself is now responsible for.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_carries_siphoned_messages_on_abort_by_disarm() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    // An unrelated, non-supersede-candidate message (a TUI TuneRequest)
    // is already queued BEFORE the wait starts — it will be siphoned on
    // the very first poll tick, before the disarm below has a chance to
    // fire.
    tx.send(crate::message_bus::ComponentMessage::new(
        crate::message_bus::ComponentId::Tui,
        crate::message_bus::ComponentId::Ft8Transmitter,
        MessageType::TuneRequest {
            duration_secs: 5,
            tone_offset_hz: 1000.0,
        },
        Instant::now(),
    ))
    .unwrap();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut st = pancetta_agent::arm::ArmState::new();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "disarm-preserves-siphoned-jti".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    let arm_clone = arm.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        arm_clone
            .lock()
            .unwrap()
            .disarm(chrono::Utc::now().timestamp_millis());
    });

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_secs(5),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        Some((&arm, Some("client-a"))),
    )
    .await;
    assert!(
        matches!(outcome, super::SleepOutcome::AbortedByDisarm),
        "expected AbortedByDisarm, got {outcome:?}"
    );
    let restored = inbox
        .try_recv()
        .expect("the already-siphoned TuneRequest must be back in the inbox, not dropped");
    assert!(matches!(
        restored.message_type,
        MessageType::TuneRequest { .. }
    ));
    assert!(inbox.try_recv().is_err(), "restored exactly once");
}

/// Round-8 review (Codex P2): a message that arrives in the channel
/// AFTER an earlier message was already siphoned, but BEFORE the disarm
/// is detected, is left untouched in the channel — `check_once` checks
/// disarm before it ever calls `try_recv()` again, so it returns
/// immediately without draining that later arrival.
///
/// Round-9 review (Codex P1): draining the channel here, before
/// returning, would let concurrent producers delay the caller's first
/// (safety-critical) PTT-off send indefinitely. No drain is needed: the
/// already-siphoned message goes back on the inbox front, which is read
/// before the channel, so the inbox yields both in arrival order.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_returns_only_already_siphoned_messages_on_abort_by_disarm() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    // Older message: siphoned on the very first (pre-sleep) poll tick.
    tx.send(crate::message_bus::ComponentMessage::new(
        crate::message_bus::ComponentId::Tui,
        crate::message_bus::ComponentId::Ft8Transmitter,
        MessageType::TuneRequest {
            duration_secs: 5,
            tone_offset_hz: 1000.0,
        },
        Instant::now(),
    ))
    .unwrap();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut st = pancetta_agent::arm::ArmState::new();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "disarm-preserves-fifo-jti".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    // Newer message: arrives at ~10ms, after the pre-sleep poll already
    // ran (so it's NOT siphoned yet) but before the disarm at ~20ms —
    // it sits untouched in `tx_rx` until the poll tick that discovers
    // the disarm, which returns before ever draining it.
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        tx_clone
            .send(crate::message_bus::ComponentMessage::new(
                crate::message_bus::ComponentId::Tui,
                crate::message_bus::ComponentId::Ft8Transmitter,
                MessageType::TuneRequest {
                    duration_secs: 5,
                    tone_offset_hz: 2000.0,
                },
                Instant::now(),
            ))
            .unwrap();
    });

    let arm_clone = arm.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        arm_clone
            .lock()
            .unwrap()
            .disarm(chrono::Utc::now().timestamp_millis());
    });

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_secs(5),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        Some((&arm, Some("client-a"))),
    )
    .await;
    assert!(
        matches!(outcome, super::SleepOutcome::AbortedByDisarm),
        "expected AbortedByDisarm, got {outcome:?}"
    );
    let freqs: Vec<f64> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::TuneRequest { tone_offset_hz, .. } => tone_offset_hz,
            other => panic!("expected TuneRequest, got {other:?}"),
        })
        .collect();
    assert_eq!(
        freqs,
        vec![1000.0, 2000.0],
        "the siphoned message (on the front) must precede the later arrival \
         still in the channel"
    );
}

/// PAN-92: an already-keyed Remote transmission must be interrupted the
/// moment the arm stops permitting it (here: an explicit disarm fired by
/// a concurrent task partway through the sleep), not just have future
/// transmissions blocked.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_aborts_by_disarm_when_remote_arm_disarms_mid_sleep() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut st = pancetta_agent::arm::ArmState::new();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.set_channel_consent(pancetta_agent::arm::ConsentChannel::WsjtxUdp, true, now_ms);
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "disarm-mid-sleep-jti".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));

    let arm_clone = arm.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        arm_clone
            .lock()
            .unwrap()
            .disarm(chrono::Utc::now().timestamp_millis());
    });

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let start = tokio::time::Instant::now();
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_secs(5),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        Some((&arm, Some("client-a"))),
    )
    .await;
    assert!(
        matches!(outcome, super::SleepOutcome::AbortedByDisarm),
        "expected AbortedByDisarm, got {outcome:?}"
    );
    // Should wake within the ~50ms poll granularity of the disarm (fired
    // at ~20ms), not wait out the full 5s sleep.
    assert!(start.elapsed() < Duration::from_millis(200));
}

/// Sibling of the above: a Local in-flight frame (`remote_tx_arm: None`)
/// must never be interrupted by the remote arm, even if it would deny —
/// byte-identical to pre-PAN-92 behavior for every non-remote TX.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_ignores_arm_state_for_a_local_frame() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(80),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None, // Local frame: no arm to check
    )
    .await;
    assert!(matches!(outcome, super::SleepOutcome::Completed));
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_detects_a_qualifying_incoming_message() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    let new_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "KA1ABC K5ARH RR73".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    tx.send(crate::message_bus::ComponentMessage::new(
        crate::message_bus::ComponentId::Autonomous,
        crate::message_bus::ComponentId::Ft8Transmitter,
        new_request.clone(),
        std::time::Instant::now(),
    ))
    .unwrap();

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(500),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    match outcome {
        super::SleepOutcome::Superseded(MessageType::TransmitRequest {
            message_text,
            qso_id,
            frequency_offset,
            ..
        }) => {
            assert_eq!(message_text, "KA1ABC K5ARH RR73");
            assert_eq!(qso_id.as_deref(), Some("qso-1"));
            assert_eq!(frequency_offset, 1500.0);
        }
        other => panic!("expected Superseded(TransmitRequest), got {:?}", other),
    }
    assert!(
        abort.load(Ordering::Acquire),
        "should set abort_current_tx itself"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_drops_non_qualifying_message_and_keeps_waiting() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let (tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();

    // Identical content to what's in flight — should be Dropped, not treated
    // as a trigger, and the sleep should complete normally.
    let duplicate = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    tx.send(crate::message_bus::ComponentMessage::new(
        crate::message_bus::ComponentId::Autonomous,
        crate::message_bus::ComponentId::Ft8Transmitter,
        duplicate,
        std::time::Instant::now(),
    ))
    .unwrap();

    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(80),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    assert!(matches!(outcome, super::SleepOutcome::Completed));
    assert!(!abort.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_still_honors_shutdown() {
    let shutdown = Arc::new(AtomicBool::new(true));
    let abort = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = crossbeam_channel::unbounded();
    let pivoted_once = std::collections::HashMap::new();
    let inbox = super::TxInbox::new(rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_secs(60),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "in flight text".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;
    assert!(matches!(outcome, super::SleepOutcome::AbortedByShutdown));
}

/// I1 REGRESSION: a message that is NOT a supersede candidate (here a
/// `TuneRequest` — a real top-level worker arm) arriving during a supersede
/// sleep must NOT be silently swallowed. Before the fix it was fed to the
/// classifier's catch-all `Drop` arm and consumed with no re-enqueue, so an
/// operator's F4 single-tone tune issued while a TX was on the air simply
/// vanished instead of being processed once TX finished. The sleep must
/// complete normally (a TuneRequest is not a supersede trigger) and the
/// message must be re-enqueued to the worker's own channel for the main
/// loop's next dequeue.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_reenqueues_non_candidate_tune_request() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    // The worker's own channel: the function polls its receiver AND
    // re-enqueues to it (routed by ComponentId::Ft8Transmitter).
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    // Operator F4 tune request lands on the TX channel mid-transmission.
    let tune = MessageType::TuneRequest {
        duration_secs: 5,
        tone_offset_hz: 1500.0,
    };
    tx_sender
        .send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Tui,
            crate::message_bus::ComponentId::Ft8Transmitter,
            tune,
            std::time::Instant::now(),
        ))
        .unwrap();

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(120),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    // A TuneRequest is not a supersede trigger: the sleep runs to completion
    // and never sets the abort flag.
    assert!(
        matches!(outcome, super::SleepOutcome::Completed),
        "a non-candidate TuneRequest must not supersede — got {outcome:?}"
    );
    assert!(
        !abort.load(Ordering::Acquire),
        "a TuneRequest must not set abort_current_tx"
    );

    // The TuneRequest was re-enqueued (siphoned during the sleep, flushed
    // back after) — NOT lost.
    let reenqueued = inbox
        .try_recv()
        .expect("the TuneRequest must be re-enqueued, not swallowed");
    assert!(
        matches!(
            reenqueued.message_type,
            MessageType::TuneRequest {
                duration_secs: 5,
                ..
            }
        ),
        "expected the re-enqueued TuneRequest, got {:?}",
        reenqueued.message_type
    );
    // Exactly one — no duplication.
    assert!(
        inbox.try_recv().is_err(),
        "the TuneRequest is re-enqueued exactly once"
    );
}

/// Codex finding 4 (PR #346 round 1): a `TransmitRequest` for a
/// DIFFERENT QSO than the one in flight must be re-enqueued for the
/// worker's own next dequeue, not silently discarded. Before this fix,
/// `classify_incoming_during_tx` returned `Drop` for this shape, which
/// `check_once` consumes with no re-enqueue and no `TransmitComplete` —
/// fine for a genuine exact-duplicate/pivot-tombstone, but for a
/// different QSO's real content (e.g. a one-shot closing RR73/73) that
/// permanently loses the frame instead of merely delaying it.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_reenqueues_different_qsos_transmit_request_instead_of_dropping() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    // A different QSO's own closing RR73 arrives while qso-1 is in flight.
    let other_qso_request = MessageType::TransmitRequest {
        attempt_id: None,
        message_text: "OTHER1 W5AU RR73".to_string(),
        frequency_offset: 900.0,
        qso_id: Some("qso-other1".to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    tx_sender
        .send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Qso,
            crate::message_bus::ComponentId::Ft8Transmitter,
            other_qso_request,
            std::time::Instant::now(),
        ))
        .unwrap();

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(120),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    assert!(
        matches!(outcome, super::SleepOutcome::Completed),
        "a different QSO's request must not supersede the in-flight TX — got {outcome:?}"
    );
    assert!(
        !abort.load(Ordering::Acquire),
        "a different QSO's request must not set abort_current_tx"
    );

    let reenqueued = inbox
        .try_recv()
        .expect("the different QSO's request must be re-enqueued, not dropped");
    match reenqueued.message_type {
        MessageType::TransmitRequest { qso_id, .. } => {
            assert_eq!(qso_id.as_deref(), Some("qso-other1"));
        }
        other => panic!("expected the re-enqueued TransmitRequest, got {other:?}"),
    }
    assert!(
        inbox.try_recv().is_err(),
        "the request is re-enqueued exactly once"
    );
}

/// Codex round 7 (PR #346): an unrelated (Requeue-worthy) message
/// sitting at the head of `tx_rx`, with a genuinely fresher same-QSO
/// update already queued directly behind it, must not hide that update
/// even when the wait's total duration is vanishingly short. Before the
/// fix, `check_once` handled exactly one message per call and returned;
/// with `total` this short, the outer loop's very next `now < deadline`
/// check could already be false, ending the wait as `Completed` with
/// the fresher update never even examined — keying the STALE in-flight
/// frame for the whole slot. Both messages are pre-queued before the
/// function is even called, so this is deterministic: the fix drains
/// everything currently available in one `check_once` call, not just
/// one message, before ever consulting `total`/`deadline`.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_finds_same_qso_update_behind_unrelated_traffic_even_with_near_zero_budget()
{
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    let send = |qso_id: &str, text: &str| {
        tx_sender
            .send(crate::message_bus::ComponentMessage::new(
                crate::message_bus::ComponentId::Qso,
                crate::message_bus::ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: text.to_string(),
                    frequency_offset: 900.0,
                    qso_id: Some(qso_id.to_string()),
                    tx_parity: None,
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                },
                std::time::Instant::now(),
            ))
            .unwrap();
    };
    // B1 (unrelated, Requeue-worthy) queued directly ahead of A (a
    // genuinely fresher message for the in-flight QSO).
    send("qso-b1", "B1 W5AU RR73");
    send("qso-1", "KA1ABC K5ARH RR73");

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_nanos(1), // effectively zero budget
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    match outcome {
        super::SleepOutcome::Superseded(MessageType::TransmitRequest { qso_id, .. }) => {
            assert_eq!(
                qso_id.as_deref(),
                Some("qso-1"),
                "A must be found and win the supersede despite the near-zero budget"
            );
        }
        other => panic!(
            "expected Superseded(A) even with a near-zero budget, got {other:?} — \
             the fresher update behind unrelated traffic was hidden"
        ),
    }
}

/// Codex round 7 (PR #346): requeuing a `MultiTransmitRequest` bundle
/// classified `Requeue` (round 6: overlap with no changed content, or
/// round 2: no overlap at all) must strip any item that's already
/// byte-identical to its matching in-flight counterpart before putting
/// it back on the channel. Otherwise a closing 73 (or any other
/// already-sent frame) rides along and can be keyed a SECOND time once
/// the multi-TX arm's next natural dequeue picks up the requeued
/// bundle — the multi-TX arm bypasses single-request coalescing, so
/// nothing else would catch the duplicate.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_strips_already_sent_items_from_a_requeued_bundle() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    // Bundle: one item byte-identical to the in-flight qso-1 frame
    // (already sent), plus one genuinely unrelated item for qso-other.
    let bundle = MessageType::MultiTransmitRequest {
        items: vec![
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "KA1ABC K5ARH R-15".to_string(),
                frequency_offset: 1500.0,
                qso_id: Some("qso-1".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
            crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: "OTHER W5AU RR73".to_string(),
                frequency_offset: 900.0,
                qso_id: Some("qso-other".to_string()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        ],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    tx_sender
        .send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            bundle,
            std::time::Instant::now(),
        ))
        .unwrap();

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(120),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    assert!(matches!(outcome, super::SleepOutcome::Completed));

    let reenqueued = inbox
        .try_recv()
        .expect("the unrelated item must still be re-enqueued");
    match reenqueued.message_type {
        MessageType::MultiTransmitRequest { items, .. } => {
            assert_eq!(
                items.len(),
                1,
                "the already-sent qso-1 item must be stripped, got {items:?}"
            );
            assert_eq!(items[0].qso_id.as_deref(), Some("qso-other"));
        }
        other => panic!("expected the stripped MultiTransmitRequest, got {other:?}"),
    }
    assert!(inbox.try_recv().is_err(), "re-enqueued exactly once");
}

/// The all-items-already-sent case: the whole bundle must be DROPPED,
/// not requeued as an empty (or otherwise degenerate) MultiTransmitRequest.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_drops_a_requeued_bundle_entirely_when_every_item_is_already_sent() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    let bundle = MessageType::MultiTransmitRequest {
        items: vec![crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    };
    tx_sender
        .send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            bundle,
            std::time::Instant::now(),
        ))
        .unwrap();

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(120),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    assert!(matches!(outcome, super::SleepOutcome::Completed));
    assert!(
        inbox.try_recv().is_err(),
        "a bundle with nothing left after stripping must be dropped entirely, not requeued"
    );
}

/// PAN-136 (Codex P2 on PR #456): an in-flight Remote-A bundle of two
/// Remote-A QSOs, then a refresh where the operator has taken over one of
/// them locally with identical text. Both bundles fold to Remote/client-a,
/// so a bundle-level comparison saw nothing new: the refresh was requeued,
/// stripped item by item as "already sent", and dropped. Client A's disarm
/// then aborted the old bundle and the locally entitled refresh was gone.
/// The refresh must instead supersede, carrying the local item's binding.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_keeps_a_refresh_whose_item_binding_changed_under_the_same_fold() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();
    let item = |text: &str, qso: &str, origin, client: Option<&str>| {
        crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: text.to_string(),
            frequency_offset: 1500.0,
            qso_id: Some(qso.to_string()),
            origin,
            remote_client_key_id: client.map(str::to_string),
        }
    };
    let remote = crate::message_bus::TxOrigin::Remote;
    let local = crate::message_bus::TxOrigin::Local;
    let in_flight = [
        item("SM2LIY K5ARH R-14", "qso-1", remote, Some("client-a")),
        item("C6AVD K5ARH EM10", "qso-2", remote, Some("client-a")),
    ];
    let refresh_items = vec![
        item("SM2LIY K5ARH R-14", "qso-1", remote, Some("client-a")),
        item("C6AVD K5ARH EM10", "qso-2", local, None),
    ];
    assert_eq!(
        crate::coordinator::fold_bundle_identity(&refresh_items),
        (remote, Some("client-a".to_string())),
        "precondition: the refresh folds to the in-flight bundle's binding"
    );
    tx_sender
        .send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: refresh_items,
                tx_parity: None,
                origin: remote,
                remote_client_key_id: Some("client-a".to_string()),
            },
            std::time::Instant::now(),
        ))
        .unwrap();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut st = pancetta_agent::arm::ArmState::new();
    st.set_channel_consent(
        pancetta_agent::arm::ConsentChannel::StationAgent,
        true,
        now_ms,
    );
    st.arm(
        pancetta_agent::arm::VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            armed_until_ms: now_ms + 120_000,
            capability_exp_ms: now_ms + 120_000 + 3_600_000,
            tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
            effective_ttl_ms: 120_000,
            heartbeat_interval_sec: 10,
            scope_tx: true,
            jti: "pan136-item-binding-refresh".to_string(),
            client_key_id: "client-a".to_string(),
        },
        pancetta_agent::arm::ArmTime::now(),
    );
    let arm = Arc::new(std::sync::Mutex::new(st));
    let arm_clone = arm.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        arm_clone
            .lock()
            .unwrap()
            .disarm(chrono::Utc::now().timestamp_millis());
    });

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_secs(5),
        &shutdown,
        &abort,
        &inbox,
        &in_flight,
        &pivoted_once,
        Some((&arm, Some("client-a"))),
    )
    .await;

    let super::SleepOutcome::Superseded(MessageType::MultiTransmitRequest { items, .. }) = outcome
    else {
        panic!(
            "the re-bound refresh must supersede, not be dropped as already sent \
             (then lost to client A's disarm); got {outcome:?}, inbox next: {:?}",
            inbox.try_recv().ok().map(|m| m.message_type)
        );
    };
    let qso2 = items
        .iter()
        .find(|it| it.qso_id.as_deref() == Some("qso-2"))
        .expect("the locally taken-over item survives");
    assert_eq!(qso2.origin, local);
    assert_eq!(qso2.remote_client_key_id, None);
}

/// Codex round 3 (PR #346): what a `Superseded` wait siphoned must
/// preserve TRUE arrival order relative to anything left untouched in
/// the live channel when the loop exits early on a supersede. Sequence queued (in this order) BEFORE the
/// function is even called: B1 (a different QSO — gets siphoned by the
/// very first `check_once` poll), A (a fresher message for the SAME
/// in-flight QSO — drained by the second `check_once` poll, classified
/// `Supersede`, and returned immediately WITHOUT further draining), then
/// B2 (a different QSO again — never drained by this function at all,
/// since the loop already returned). Before the fix, only `siphoned`
/// (containing B1) was appended to the live channel's tail, landing
/// AFTER the untouched B2 and inverting their true arrival order. Now
/// B1 goes back on the inbox front, which is read before the channel,
/// so the inbox yields B1 then B2 with no drain (see
/// `tx_inbox_tests::requeue_front_is_read_before_the_channel_in_order`).
///
/// Round 4 (Codex): a caller's own supersede-derived content goes on the
/// front after this returns, so it lands ahead of B1.
#[tokio::test(flavor = "current_thread")]
async fn supersede_sleep_preserves_arrival_order_between_siphoned_and_untouched_messages() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let bus = crate::message_bus::MessageBus::new(16).unwrap();
    let (tx_sender, tx_rx) = bus
        .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let pivoted_once = std::collections::HashMap::new();

    let send = |qso_id: &str, text: &str| {
        tx_sender
            .send(crate::message_bus::ComponentMessage::new(
                crate::message_bus::ComponentId::Qso,
                crate::message_bus::ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: text.to_string(),
                    frequency_offset: 900.0,
                    qso_id: Some(qso_id.to_string()),
                    tx_parity: None,
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                },
                std::time::Instant::now(),
            ))
            .unwrap();
    };
    // Pre-queue all three before the function ever runs, in true
    // arrival order: B1, A, B2.
    send("qso-b1", "B1 W5AU RR73");
    send("qso-1", "KA1ABC K5ARH RR73"); // A: fresher text for the in-flight QSO
    send("qso-b2", "B2 W5AU RR73");

    let inbox = super::TxInbox::new(tx_rx.clone(), Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(500),
        &shutdown,
        &abort,
        &inbox,
        &[crate::message_bus::TransmitRequestItem {
            attempt_id: None,
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        }],
        &pivoted_once,
        None,
    )
    .await;

    match outcome {
        super::SleepOutcome::Superseded(MessageType::TransmitRequest { qso_id, .. }) => {
            assert_eq!(qso_id.as_deref(), Some("qso-1"), "A must win the supersede");
        }
        other => panic!("expected Superseded(A), got {other:?}"),
    }

    // True arrival order preserved in the inbox: B1 first, then B2 —
    // never B2 before B1.
    let order: Vec<Option<String>> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|m| match m.message_type {
            MessageType::TransmitRequest { qso_id, .. } => qso_id,
            other => panic!("expected a TransmitRequest, got {other:?}"),
        })
        .collect();
    assert_eq!(
        order,
        vec![Some("qso-b1".to_string()), Some("qso-b2".to_string())],
        "B1 must come out FIRST, then B2"
    );
}

fn pan73_request(text: String, qso: String) -> ComponentMessage {
    ComponentMessage::new(
        crate::message_bus::ComponentId::Qso,
        crate::message_bus::ComponentId::Ft8Transmitter,
        MessageType::TransmitRequest {
            attempt_id: None,
            message_text: text,
            frequency_offset: 900.0,
            qso_id: Some(qso),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        },
        Instant::now(),
    )
}

fn pan73_text(m: &ComponentMessage) -> String {
    match &m.message_type {
        MessageType::TransmitRequest { message_text, .. } => message_text.clone(),
        MessageType::MultiTransmitRequest { items, .. } => {
            format!("bundle:{}", items[0].message_text)
        }
        other => format!("{other:?}"),
    }
}

fn pan73_in_flight() -> [crate::message_bus::TransmitRequestItem; 1] {
    [crate::message_bus::TransmitRequestItem {
        attempt_id: None,
        message_text: "KA1ABC K5ARH R-15".to_string(),
        frequency_offset: 1500.0,
        qso_id: Some("qso-1".to_string()),
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }]
}

/// PAN-73 follow-up (PR #346 round-4 finding): a producer sending into
/// the worker's channel while a wait ends must never land ahead of what
/// the wait siphoned. The producer starts just before the wait's end and
/// keeps sending after it returns, so its sends straddle the point where
/// siphoned messages used to be re-sent through the bus (100/100 trials
/// out of order before the fix).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_producer_never_overtakes_messages_the_wait_siphoned() {
    for trial in 0..100 {
        let bus = crate::message_bus::MessageBus::new(8192).unwrap();
        let (sender, rx) = bus
            .get_or_create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
            .await
            .unwrap();
        for i in 0..64 {
            sender
                .send(pan73_request(format!("old-{i:04}"), format!("qso-old-{i}")))
                .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let bus = bus.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(29)).await;
                let mut j = 0usize;
                while !stop.load(Ordering::Acquire) && j < 6000 {
                    bus.send_message(pan73_request(format!("new-{j:04}"), format!("qso-new-{j}")))
                        .await
                        .unwrap();
                    j += 1;
                    tokio::task::yield_now().await;
                }
            })
        };
        let inbox = super::TxInbox::new(rx, Default::default());
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(30),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &inbox,
            &pan73_in_flight(),
            &std::collections::HashMap::new(),
            None,
        )
        .await;
        assert!(matches!(outcome, super::SleepOutcome::Completed));
        // Keep producing after the wait returned.
        tokio::time::sleep(Duration::from_millis(2)).await;
        stop.store(true, Ordering::Release);
        producer.await.unwrap();

        let order: Vec<String> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|m| pan73_text(&m))
            .collect();
        // Arrival order: every old-* (queued before the wait), then the
        // producer's new-* in send order.
        let mut expected: Vec<String> = order.clone();
        expected.sort_by_key(|t| (t.starts_with("new-"), t.clone()));
        assert_eq!(order, expected, "trial {trial}: inbox out of arrival order");
        assert!(order.iter().take(64).all(|t| t.starts_with("old-")));
    }
}

/// PAN-73 round 4 under concurrency: the supersede winner's own content
/// goes ahead of the siphoned traffic, which goes ahead of everything a
/// concurrent producer sent, no matter how the sends interleave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supersede_winner_then_siphoned_then_concurrent_producer() {
    for trial in 0..50 {
        let bus = crate::message_bus::MessageBus::new(8192).unwrap();
        let (sender, rx) = bus
            .get_or_create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
            .await
            .unwrap();
        sender
            .send(pan73_request("B1".into(), "qso-b1".into()))
            .unwrap();
        sender
            .send(pan73_request("KA1ABC K5ARH RR73".into(), "qso-1".into()))
            .unwrap();
        sender
            .send(pan73_request("B2".into(), "qso-b2".into()))
            .unwrap();
        let producer = {
            let bus = bus.clone();
            tokio::spawn(async move {
                for j in 0..200 {
                    bus.send_message(pan73_request(format!("new-{j:04}"), format!("qso-new-{j}")))
                        .await
                        .unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        let inbox = super::TxInbox::new(rx, Default::default());
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(500),
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(false)),
            &inbox,
            &pan73_in_flight(),
            &std::collections::HashMap::new(),
            None,
        )
        .await;
        assert!(matches!(outcome, super::SleepOutcome::Superseded(_)));
        // The caller's supersede work awaits (PTT-off etc.) while the
        // producer keeps sending; then it requeues the winner.
        tokio::task::yield_now().await;
        inbox.requeue_front(vec![super::self_message(
            MessageType::MultiTransmitRequest {
                items: vec![crate::message_bus::TransmitRequestItem {
                    attempt_id: None,
                    message_text: "WINNER".into(),
                    frequency_offset: 1500.0,
                    qso_id: Some("qso-1".into()),
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                }],
                tx_parity: None,
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        )]);
        producer.await.unwrap();
        let order: Vec<String> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|m| pan73_text(&m))
            .collect();
        assert_eq!(&order[..3], ["bundle:WINNER", "B1", "B2"], "trial {trial}");
        let news: Vec<String> = (0..200).map(|j| format!("new-{j:04}")).collect();
        assert_eq!(&order[3..], &news[..], "trial {trial}");
    }
}

/// PAN-73 follow-up, acceptance criterion 2: with the channel full, the
/// siphoned messages still come back, all of them, in order. Before the
/// fix the bus re-send dropped them and reported nothing.
#[tokio::test(flavor = "current_thread")]
async fn full_channel_cannot_lose_siphoned_messages() {
    let bus = crate::message_bus::MessageBus::new(2).unwrap();
    let (sender, rx) = bus
        .get_or_create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    sender
        .send(pan73_request("old-0".into(), "qso-a".into()))
        .unwrap();
    sender
        .send(pan73_request("old-1".into(), "qso-b".into()))
        .unwrap();
    let refill = {
        let bus = bus.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            for j in 0..2 {
                bus.send_message(pan73_request(format!("new-{j}"), format!("qso-n{j}")))
                    .await
                    .unwrap();
            }
        })
    };
    let inbox = super::TxInbox::new(rx, Default::default());
    let outcome = super::interruptible_sleep_or_supersede(
        Duration::from_millis(40),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        &inbox,
        &pan73_in_flight(),
        &std::collections::HashMap::new(),
        None,
    )
    .await;
    assert!(matches!(outcome, super::SleepOutcome::Completed));
    refill.await.unwrap();
    assert_eq!(inbox.len(), 4);
    let order: Vec<String> = std::iter::from_fn(|| inbox.try_recv().ok())
        .map(|m| pan73_text(&m))
        .collect();
    assert_eq!(order, ["old-0", "old-1", "new-0", "new-1"]);
}
