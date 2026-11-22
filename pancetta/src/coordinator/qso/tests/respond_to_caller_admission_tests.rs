//! PAN-23 round-2 (Codex review of PR #283): the backend guards added to
//! `qso_manager::respond_to_caller`/`respond_to_cq_with` only fire when
//! a call is actually PROCESSED. But `RespondToCaller`'s (and
//! `StartQso`'s — round-3 finding) admission path (the
//! `if matches!(admit_new_qso(...), TxAdmission::Queue)` block above,
//! guarding `pending_manual_calls`) can queue a request FIRST —
//! whenever both TX parities are contested (an active QSO occupies the
//! side opposite the one this caller wants) — and only run the
//! `qso_manager` guard later, at promotion time
//! (`promote_pending_manual_calls`). Since the unresolved-hash
//! placeholder `"<...>"` is deterministically un-transmittable no
//! matter how many times it's retried, letting it reach that point
//! would re-queue it at the front of `pending_manual_calls` on every
//! failed promotion attempt — repeatedly holding the parity slot
//! against legitimate opposite-parity calls for up to the full
//! `QUEUED_CALL_TTL` (10 minutes).
//!
//! The fix (this module proves it): reject the placeholder at
//! ADMISSION time, in both match arms, BEFORE the opposite-parity
//! queueing check ever runs — so it can never occupy a slot in the
//! first place. This exercises the REAL `start_qso_component` message
//! loop end to end (real coordinator, real message bus, real admission
//! logic), not a synthetic reproduction — a lower-level unit test of
//! `qso_manager`'s guards alone (see `pancetta-qso/src/qso_manager.rs`)
//! cannot see this gap, since it never goes through the queueing path
//! at all.
//!
//! PAN-23 round-3 (Codex review, two more findings):
//!
//! 1. **Never touch the operator's real logbook.** `start_qso_component`
//!    in its normal (non-`--replay`) mode opens/creates
//!    `~/.pancetta/qsos.adi` and `~/.pancetta/qso.db` unconditionally
//!    (see `start_local_qso_log_writers`'s `replay` gate above). Running
//!    these tests on an operator's real workstation must never touch
//!    that file, and two tests racing `AdifLogWriter::open`'s
//!    existence check could even corrupt it. `test_coordinator` below
//!    passes `replay_path: Some(..)` (mirroring
//!    `coordinator::mod`'s own `build_coordinator_with_replay` /
//!    `replay_mode_tracks_the_replay_path` pattern — the SAME dummy,
//!    never-read path value) so `self.replay_mode()` is `true` and
//!    `start_local_qso_log_writers` returns `(None, None)` without ever
//!    opening `adif_path`/`db_path`. Confirmed this doesn't interfere
//!    with anything these tests exercise: `replay_mode` only gates the
//!    local ADIF/DB writers and the (here, unconfigured) upload
//!    subscriber — never the `RespondToCaller`/`StartQso` admission
//!    logic, the Tui message-bus channel, or diagnostic emission.
//! 2. **Don't discard messages while polling.** The original
//!    `poll_until` consumed messages from the channel while searching
//!    for the wanted diagnostic and discarded every non-matching one —
//!    so a regression that emitted the forbidden `"Queued <...>"`
//!    status BEFORE the diagnostic would have that status silently
//!    thrown away by the search itself, and the later separate
//!    "must never have seen Queued" check would find nothing (already
//!    consumed), giving a false pass. `drain_all` below instead
//!    collects EVERY message observed into one `Vec` regardless of
//!    order, and both assertions inspect that same complete buffer.
use super::super::{ActiveQsoFreq, ApplicationCoordinator};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage};
use pancetta_config::Config;
use pancetta_core::slot::SlotParity;
use pancetta_core::ResponseStep;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

/// Local copy of the `test_coordinator` pattern used elsewhere in this
/// crate (`coordinator::health`, `coordinator::hamlib`) — no shared
/// helper exists yet — EXCEPT for `replay_path`, which (unlike those
/// other copies) is deliberately `Some(..)`: see this module's
/// doc-comment, finding 1. The path itself is never read (only
/// `.is_some()` matters to `replay_mode()`); the value mirrors
/// `coordinator::mod`'s own replay-mode tests
/// (`build_coordinator_with_replay`).
///
/// PAN-41 round 4: `pancetta_home_override` points the replay-history-
/// seed read (which DOES still read real history by `--replay`'s own
/// contract — see `coordinator::health`'s `test_coordinator` sibling
/// copy for the full explanation) at an isolated, empty temp dir instead
/// of the operator's real `~/.pancetta`.
async fn test_coordinator() -> ApplicationCoordinator {
    let config = Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        config,
        None,
        true,                                                // no_audio
        true,                                                // headless
        None,                                                // no WAV
        Some(std::path::PathBuf::from("/some/capture/dir")), // --replay: never touch the real logbook
        None,                                                // no test-tx
        1500.0,
        shutdown,
        Vec::new(),                                             // no config warnings
        std::env::temp_dir().join("pancetta-test-config.toml"), // test-only config path
        true,                                                   // test-only: assume TOML
        None,                                                   // PAN-270: no session-home override
    )
    .await
    .expect("coordinator creation should succeed");

    coordinator.pancetta_home_override = Some(std::env::temp_dir().join(format!(
        "pancetta-test-home-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));

    coordinator
}

/// PAN-182: the Qso loop hands a `TxFrameSent` for a live QSO to
/// `confirm_stall_trigger_aired`, releasing the stall switch held on that
/// frame; a malformed id is dropped without disturbing the next message.
#[tokio::test]
async fn tx_frame_sent_releases_a_held_stall_switch() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "W1ABC".to_string();
    coordinator
        .config
        .write()
        .await
        .autonomous
        .qso_stall_switch_after = 1;
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );
    coordinator.start_qso_component().await.unwrap();
    // See `tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_requests`:
    // this clone must read the coordinator's live Auto mode too.
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    manager.set_tx_freq_mode_source(coordinator.tx_freq_mode.clone());
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + chrono::Duration::seconds(15))
        .await;
    let qso = manager.get_qso(id).await.unwrap();
    let text = qso.messages.last().unwrap().raw_text.clone();
    let origin = pancetta_qso::qso_manager::OffsetRelocationOrigin::StallDetected {
        raised_at_generation: qso.metadata.advance_generation,
    };
    assert!(
        matches!(
            manager.apply_tx_offset_switch(id, 1800.0, origin).await,
            Err(pancetta_qso::QsoManagerError::OffsetActionTriggerOutcomePending { .. })
        ),
        "precondition: the stall switch is held on the rearm's frame"
    );

    for qso_id in ["not-a-uuid".to_string(), id.to_string()] {
        coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Qso,
                MessageType::QsoMessage(QsoMessage::TxFrameSent {
                    qso_id,
                    message_text: text.clone(),
                    aired: pancetta_qso::UnsentAttemptPayload {
                        frequency_hz: 1500.0,
                        remote_binding: Some((false, None)),
                    },
                    // After the rearm's own TX slot has played out.
                    aired_at: opened + chrono::Duration::minutes(2),
                    attempt_id: None,
                }),
                Instant::now(),
            ))
            .await
            .unwrap();
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match manager.apply_tx_offset_switch(id, 1800.0, origin).await {
            Ok(applied) => {
                assert_eq!(applied, 1800.0);
                break;
            }
            Err(pancetta_qso::QsoManagerError::OffsetActionTriggerOutcomePending { .. }) => {}
            Err(e) => panic!("unexpected refusal: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "QSO loop did not release the held switch"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

async fn check_tx_frame_not_sent(malformed_first: bool) {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "W1ABC".to_string();
    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + chrono::Duration::seconds(15))
        .await;
    let before = manager.get_qso(id).await.unwrap();
    assert_eq!(before.metadata.call_count, 2);
    let text = before.messages.last().unwrap().raw_text.clone();
    let ids = if malformed_first {
        vec!["not-a-uuid".to_string(), id.to_string()]
    } else {
        vec![id.to_string()]
    };
    for qso_id in ids {
        coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Qso,
                MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                    attempt_id: None,
                    qso_id,
                    message_text: text.clone(),
                    replaced: None,
                }),
                Instant::now(),
            ))
            .await
            .unwrap();
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let qso = manager.get_qso(id).await.unwrap();
        if qso.metadata.call_count == 1 {
            assert_eq!(
                qso.messages
                    .iter()
                    .filter(|m| m.direction == pancetta_qso::MessageDirection::Sent)
                    .count(),
                1
            );
            break;
        }
        assert!(Instant::now() < deadline, "QSO loop did not apply rollback");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

#[tokio::test]
async fn tx_frame_not_sent_rolls_back_keep_call_bookkeeping() {
    check_tx_frame_not_sent(false).await;
}

#[tokio::test]
async fn tx_frame_not_sent_with_malformed_qso_id_is_ignored() {
    check_tx_frame_not_sent(true).await;
}

/// PAN-183: the next `TransmitRequest` the QSO loop forwards for `qso`,
/// as `(text, attempt_id)`.
async fn pan183_next_transmit_request(
    tx: &crossbeam_channel::Receiver<ComponentMessage>,
    qso: &str,
) -> (String, Option<pancetta_qso::TxAttemptId>) {
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        while let Ok(msg) = tx.try_recv() {
            if let MessageType::TransmitRequest {
                message_text,
                qso_id: Some(id),
                attempt_id,
                ..
            } = msg.message_type
            {
                if id == qso {
                    return (message_text, attempt_id);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "QSO loop forwarded no TransmitRequest"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// PAN-183: the QSO loop forwards each `MessageToSend`'s attempt id on
/// both the `TransmitRequest` and the `LatestTxIntent` a key-time pivot
/// adopts.
#[tokio::test]
async fn pan183_latest_tx_intent_carries_attempt_id() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (_tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    // Yield so the spawned event-forwarding task subscribes first.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let mut events = manager.subscribe();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let (_, opening) = pan183_next_transmit_request(&tx, &id.to_string()).await;
    assert!(opening.is_some(), "every engine emission carries an id");
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + chrono::Duration::seconds(15))
        .await;
    let mut emitted = None;
    while let Ok(event) = events.try_recv() {
        if let pancetta_qso::QsoEvent::MessageToSend { attempt_id, .. } = event {
            emitted = attempt_id;
        }
    }
    let emitted = emitted.expect("the rearm records a ledger attempt");
    let (_, forwarded) = pan183_next_transmit_request(&tx, &id.to_string()).await;
    assert_eq!(forwarded, Some(emitted));
    assert_ne!(forwarded, opening);
    let intent = coordinator
        .latest_tx_intent
        .read()
        .unwrap()
        .get(&super::super::active_tx_qso_key(&id.to_string()))
        .cloned()
        .expect("the funnel records the intent");
    assert_eq!(intent.attempt_id, Some(emitted));
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-183 gherkin through the running QSO loop: a keep-call rearm frame
/// waits before PTT, the operator resends the same text, and the TX
/// worker reports the OLDER rearm frame as not sent with the attempt id
/// its request carried. The rearm's call, stall increment and Sent entry
/// roll back; the resend's accounting stays.
#[tokio::test]
async fn pan183_tx_frame_not_sent_rolls_back_the_reported_attempt_not_a_newer_resend() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (_tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    // Yield so the spawned event-forwarding task subscribes first.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let qso = id.to_string();
    pan183_next_transmit_request(&tx, &qso).await;
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + chrono::Duration::seconds(15))
        .await;
    let (rearm_text, rearm_id) = pan183_next_transmit_request(&tx, &qso).await;
    manager.resend_last_tx(id).await.unwrap();
    let (resend_text, resend_id) = pan183_next_transmit_request(&tx, &qso).await;
    assert_eq!(rearm_text, resend_text);
    assert!(rearm_id.is_some() && resend_id.is_some() && rearm_id != resend_id);
    let before = manager.get_qso(id).await.unwrap();
    assert_eq!(before.metadata.call_count, 3);
    assert_eq!(before.metadata.stall_cycles, 1);

    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                qso_id: qso.clone(),
                message_text: rearm_text,
                replaced: None,
                attempt_id: rearm_id,
            }),
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let after = manager.get_qso(id).await.unwrap();
        if after.metadata.call_count == 2 {
            assert_eq!(
                after.metadata.stall_cycles, 0,
                "the rearm's stall increment"
            );
            assert_eq!(
                after
                    .messages
                    .iter()
                    .filter(|m| m.direction == pancetta_qso::MessageDirection::Sent)
                    .count(),
                1,
                "the rearm's Sent entry"
            );
            assert_eq!(
                after.metadata.last_call_at, before.metadata.last_call_at,
                "the resend is still the latest call"
            );
            break;
        }
        assert!(Instant::now() < deadline, "QSO loop did not apply rollback");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-223 through the running QSO loop: an in-cap rearm and its
/// off-parity successor both counted (the late-report order), and the TX
/// worker's `TxFrameScheduled` for the first one, deferred to the slot the
/// successor feeds, takes the duplicate count back off.
#[tokio::test]
async fn pan223_tx_frame_scheduled_reaches_the_keep_call_ledger() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (_tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    // Yield so the spawned event-forwarding task subscribes first.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let qso = id.to_string();
    pan183_next_transmit_request(&tx, &qso).await;
    let opened = manager.get_qso(id).await.unwrap().metadata;
    let own = opened
        .tx_parity
        .expect("a manual QSO latches its TX parity");
    let s0 = opened.stall_cycles;
    let x = pancetta_core::slot::next_slot_with_parity_with_period(
        opened.last_call_at.unwrap() + chrono::Duration::seconds(15),
        own,
        15_000_000_000,
    );
    manager
        .rearm_manual_calls_at(x + chrono::Duration::milliseconds(1_600))
        .await;
    let (_, a) = pan183_next_transmit_request(&tx, &qso).await;
    let a = a.expect("the rearm records a ledger attempt");
    manager
        .rearm_manual_calls_at(x + chrono::Duration::milliseconds(16_600))
        .await;
    assert_eq!(
        manager.get_qso(id).await.unwrap().metadata.stall_cycles,
        s0 + 2,
        "precondition: before the worker's report the prediction double-counts"
    );

    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::TxFrameScheduled {
                qso_id: qso.clone(),
                attempt_id: a,
                target_slot: x + chrono::Duration::seconds(30),
            }),
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    while manager.get_qso(id).await.unwrap().metadata.stall_cycles != s0 + 1 {
        assert!(
            Instant::now() < deadline,
            "QSO loop did not apply the scheduled-slot report"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-185 scenario 1 through the running QSO loop: the keep-call
/// rearm's rollback was parked on the Qso inbox's durable lane (its
/// channel was full when the TX worker reported it). The loop still
/// applies it: the rearm's call, stall increment and Sent entry reverse.
#[tokio::test]
async fn pan185_qso_loop_applies_a_parked_tx_frame_not_sent() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (_tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let qso = id.to_string();
    pan183_next_transmit_request(&tx, &qso).await;
    let opened = manager
        .get_qso(id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    manager
        .rearm_manual_calls_at(opened + chrono::Duration::seconds(15))
        .await;
    let (rearm_text, rearm_id) = pan183_next_transmit_request(&tx, &qso).await;
    let before = manager.get_qso(id).await.unwrap();
    assert_eq!(before.metadata.call_count, 2);
    assert_eq!(before.metadata.stall_cycles, 1);

    coordinator.message_bus.park_for_test(ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Qso,
        MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
            qso_id: qso.clone(),
            message_text: rearm_text,
            replaced: None,
            attempt_id: rearm_id,
        }),
        Instant::now(),
    ));
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let after = manager.get_qso(id).await.unwrap();
        if after.metadata.call_count == 1 {
            assert_eq!(
                after.metadata.stall_cycles, 0,
                "the rearm's stall increment"
            );
            assert_eq!(
                after
                    .messages
                    .iter()
                    .filter(|m| m.direction == pancetta_qso::MessageDirection::Sent)
                    .count(),
                1,
                "the rearm's Sent entry"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "QSO loop did not apply the parked rollback"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        coordinator.message_bus.durable_lane_len(ComponentId::Qso),
        0
    );
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-185: once the bus accepts a forwarded `TransmitRequest`, its
/// intent is marked queued (pivotable) and keeps its attempt id.
#[tokio::test]
async fn pan185_delivered_request_marks_its_intent_queued() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (_tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let (_, forwarded) = pan183_next_transmit_request(&tx, &id.to_string()).await;
    let key = super::super::active_tx_qso_key(&id.to_string());
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let intent = coordinator
            .latest_tx_intent
            .read()
            .unwrap()
            .get(&key)
            .cloned()
            .expect("a delivered request keeps its intent");
        assert_eq!(intent.attempt_id, forwarded);
        if intent.request_queued {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the delivered request's intent was never marked queued"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PAN-185 (Codex 4244560866): a request the TX worker's channel never
/// accepted leaves no pivotable intent behind, and the QSO engine rolls
/// back the attempt it recorded for it.
#[tokio::test]
async fn pan185_undelivered_request_withdraws_its_intent_and_rolls_back_its_attempt() {
    let mut coordinator = test_coordinator().await;
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
    }
    let (tx_s, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 1500.0, Some(SlotParity::Even))
        .await
        .unwrap();
    let qso = id.to_string();
    pan183_next_transmit_request(&tx, &qso).await;
    let pre_resend = manager.get_qso(id).await.unwrap().metadata.call_count;
    // The TX worker is not running here, so nothing drains the channel.
    while tx_s
        .try_send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .is_ok()
    {}
    manager.resend_last_tx(id).await.unwrap();
    let key = super::super::active_tx_qso_key(&qso);
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let intent_gone = !coordinator
            .latest_tx_intent
            .read()
            .unwrap()
            .contains_key(&key);
        let call_count = manager.get_qso(id).await.unwrap().metadata.call_count;
        if intent_gone && call_count == pre_resend {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "undelivered request: intent_gone={intent_gone}, call_count={call_count} (want {pre_resend})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

fn pan238_speculative_cq() -> (pancetta_qso::AutonomousOperator, u64, f64) {
    let mut config = pancetta_qso::AutonomousConfig {
        enabled: true,
        slot_parity: pancetta_qso::SlotParityConfig::Even,
        cq_after_idle_cycles: 2,
        cq_no_response_switch_after: 1,
        tx_offset_hz: 1700.0,
        ..Default::default()
    };
    config.listen_cycle.initial_interval = 100;
    let mut op = pancetta_qso::AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
    op.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
        power_bins: vec![0.0; 140],
        freq_min_hz: 200.0,
        freq_max_hz: 2800.0,
    });
    for _ in 0..8 {
        op.feed_decoded_messages(&[], &pancetta_qso::autonomous::NullDxEvaluator);
    }
    let before_offset = op.tx_offset_hz();
    op.decide_at(0);
    let actions = op.decide_at(0);
    assert!(actions
        .iter()
        .any(|a| matches!(a, pancetta_qso::OperatorAction::Transmit { .. })));
    let attempt = op.last_cq_attempt_id().unwrap();
    assert_ne!(
        op.tx_offset_hz(),
        before_offset,
        "fixture must speculate an offset change"
    );
    (op, attempt, before_offset)
}

async fn pan238_wait_until(description: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

async fn pan238_dispatch_cq(coordinator: &ApplicationCoordinator, attempt: u64, frequency: f64) {
    assert!(coordinator
        .message_bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::StartAutonomousQso {
                callsign: None,
                frequency,
                parity: Some(SlotParity::Even),
                cq_attempt_id: Some(attempt),
            }),
            Instant::now(),
        ))
        .await
        .unwrap());
}

async fn pan238_coordinator() -> ApplicationCoordinator {
    let mut coordinator = test_coordinator().await;
    coordinator.message_bus = crate::message_bus::MessageBus::new(1).unwrap();
    {
        let mut cfg = coordinator.config.write().await;
        cfg.station.callsign = "W1ABC".into();
        cfg.station.grid_square = "FN42".into();
    }
    // These fixtures explicitly dispatch autonomous openings.
    coordinator.config.write().await.autonomous.enabled = true;
    coordinator
        .autonomous_config_enabled
        .store(true, std::sync::atomic::Ordering::Release);
    coordinator
}

/// PAN-279: the forwarder settles a rejected opening through
/// `tx_outcome::settle_unaired_frame`, so Autonomous's failed completion
/// carries the opening's own QSO-engine attempt id (the same id its
/// self-addressed `TxFrameNotSent` carries, by construction).
#[tokio::test]
async fn pan279_rejected_opening_completion_carries_its_attempt_id() {
    let mut coordinator = pan238_coordinator().await;
    let (tx_sender, _tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_, _autonomous_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    let autonomous = coordinator
        .message_bus
        .durable_inbox(ComponentId::Autonomous)
        .await
        .unwrap();
    tx_sender
        .try_send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    let (op, attempt, _) = pan238_speculative_cq();
    pan238_dispatch_cq(&coordinator, attempt, op.tx_offset_hz()).await;
    pan238_wait_until("failed opening completion parked", || {
        coordinator
            .message_bus
            .durable_lane_len(ComponentId::Autonomous)
            == 1
    })
    .await;
    assert!(matches!(
        autonomous.try_recv().unwrap().message_type,
        MessageType::QsoMessage(QsoMessage::AutonomousCqOpened { .. })
    ));
    let MessageType::TransmitComplete {
        success: false,
        qso_id: Some(_),
        attempt_id,
        ..
    } = autonomous.try_recv().unwrap().message_type
    else {
        panic!("rejected opening must have a failed completion");
    };
    assert!(
        attempt_id.is_some(),
        "the completion carries the opening's own attempt id"
    );
}

#[tokio::test]
async fn pan238_rejected_opening_settles_after_cancellation_with_autonomous_backpressure() {
    let mut coordinator = pan238_coordinator().await;
    let (tx_sender, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_, autonomous_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    let autonomous = coordinator
        .message_bus
        .durable_inbox(ComponentId::Autonomous)
        .await
        .unwrap();
    tx_sender
        .try_send(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    let (op, attempt, before_offset) = pan238_speculative_cq();
    pan238_dispatch_cq(&coordinator, attempt, op.tx_offset_hz()).await;

    // Registration occupies the only Autonomous channel slot. No TX worker
    // runs, so the rejected opening must produce its own durable completion.
    pan238_wait_until(
        "failed opening completion parked behind registration",
        || {
            coordinator
                .message_bus
                .durable_lane_len(ComponentId::Autonomous)
                == 1
        },
    )
    .await;
    assert_eq!(autonomous_rx.len(), 1);
    let registration = autonomous.try_recv().unwrap();
    let MessageType::QsoMessage(QsoMessage::AutonomousCqOpened {
        qso_id,
        attempt_id: cq_attempt_id,
    }) = registration.message_type
    else {
        panic!("registration must precede completion");
    };
    assert_eq!(cq_attempt_id, attempt);
    let mut pending = std::collections::HashMap::from([(qso_id.clone(), attempt)]);
    let id = qso_id.parse().unwrap();
    assert!(manager.get_qso(id).await.is_ok());
    let key = super::super::active_tx_qso_key(&qso_id);
    assert!(!coordinator
        .latest_tx_intent
        .read()
        .unwrap()
        .contains_key(&key));
    coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(key.clone());
    manager.cancel_qso(id).await.unwrap();
    pan238_wait_until("cancelled opening processed", || {
        !coordinator.active_tx_qsos.read().unwrap().contains(&key)
    })
    .await;
    assert!(manager.get_qso(id).await.is_err());
    assert_eq!(tx.len(), 1, "no later request was accepted");
    assert!(matches!(
        tx.try_recv().unwrap().message_type,
        MessageType::AudioData(_)
    ));

    let completion = autonomous.try_recv().unwrap();
    let MessageType::TransmitComplete {
        success,
        message_text,
        duration_ms,
        qso_id: Some(completed_id),
        attempt_id: completed_attempt_id,
    } = completion.message_type
    else {
        panic!("rejected opening must have a completion");
    };
    assert!(!success);
    assert_eq!(duration_ms, 0);
    assert_eq!(message_text, "CQ W1ABC FN42");
    assert_eq!(completed_id, qso_id);
    let operator = tokio::sync::Mutex::new(op);
    super::super::autonomous::settle_self_cq_completion(
        &operator,
        &mut pending,
        &completed_id,
        success,
        completed_attempt_id,
    )
    .await;
    assert!(pending.is_empty());
    let mut op = operator.lock().await;
    assert_eq!(op.last_cq_attempt_id(), None, "matching snapshot consumed");
    assert_eq!(
        op.tx_offset_hz(),
        before_offset,
        "pre-attempt offset restored"
    );
    op.decide_at(0);
    let next = op.decide_at(0);
    assert!(next
        .iter()
        .any(|a| matches!(a, pancetta_qso::OperatorAction::Transmit { .. })));
    assert!(
        !next
            .iter()
            .any(|a| matches!(a, pancetta_qso::OperatorAction::FrequencyShift { .. })),
        "failed CQ must not count toward the switch threshold"
    );
    assert!(autonomous.try_recv().is_err(), "exactly one completion");
    assert_eq!(
        coordinator
            .message_bus
            .durable_lane_len(ComponentId::Autonomous),
        0
    );
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

#[tokio::test]
async fn pan238_accepted_opening_keeps_outcome_ownership_when_resend_is_rejected() {
    let mut coordinator = pan238_coordinator().await;
    let (_, tx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    let (_, autonomous_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    let autonomous = coordinator
        .message_bus
        .durable_inbox(ComponentId::Autonomous)
        .await
        .unwrap();
    coordinator.start_qso_component().await.unwrap();
    pan238_dispatch_cq(&coordinator, 42, 1500.0).await;
    pan238_wait_until("opening accepted", || {
        !tx.is_empty() && !autonomous_rx.is_empty()
    })
    .await;
    let MessageType::QsoMessage(QsoMessage::AutonomousCqOpened { qso_id, .. }) =
        autonomous.try_recv().unwrap().message_type
    else {
        panic!("expected registration");
    };
    let key = super::super::active_tx_qso_key(&qso_id);
    pan238_wait_until("opening intent confirmed queued", || {
        coordinator
            .latest_tx_intent
            .read()
            .unwrap()
            .get(&key)
            .is_some_and(|intent| intent.request_queued)
    })
    .await;
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // Keep the accepted opening in the capacity-one channel while a resend
    // tries to enqueue. The opening still owns the eventual worker outcome.
    let id = qso_id.parse().unwrap();
    // The autonomous opening has no Sent log entry for resend_last_tx.
    // Emit the repeated CQ through the manager's normal message path.
    manager
        .send_message(
            id,
            pancetta_qso::MessageType::Cq {
                callsign: "W1ABC".into(),
                grid: Some("FN42".into()),
            },
            1500.0,
        )
        .await;
    pan238_wait_until("rejected resend intent withdrawn", || {
        !coordinator
            .latest_tx_intent
            .read()
            .unwrap()
            .contains_key(&key)
    })
    .await;
    // Seed the active-set entry as a terminal-event barrier. start_cq
    // emits only MessageToSend, so it does not populate this set itself.
    // The forwarder removes the entry after finishing the rejected resend.
    coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(key.clone());
    manager.cancel_qso(id).await.unwrap();
    pan238_wait_until("terminal event processed", || {
        !coordinator.active_tx_qsos.read().unwrap().contains(&key)
    })
    .await;
    assert!(
        autonomous.try_recv().is_err(),
        "resend must not settle the accepted opening"
    );
    let MessageType::TransmitRequest {
        qso_id: Some(queued_id),
        ..
    } = tx.try_recv().unwrap().message_type
    else {
        panic!("the accepted opening must remain queued");
    };
    assert_eq!(queued_id, qso_id);
    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

#[tokio::test]
async fn pan238_completion_settles_once_and_preserves_success_and_unmatched_attempts() {
    let (op, attempt, _) = pan238_speculative_cq();
    let operator = tokio::sync::Mutex::new(op);
    let mut pending = std::collections::HashMap::from([("opening".to_string(), attempt)]);
    super::super::autonomous::settle_self_cq_completion(
        &operator,
        &mut pending,
        "unrelated",
        false,
        None,
    )
    .await;
    assert_eq!(pending.len(), 1);
    assert_eq!(operator.lock().await.last_cq_attempt_id(), Some(attempt));
    super::super::autonomous::settle_self_cq_completion(
        &operator,
        &mut pending,
        "opening",
        true,
        None,
    )
    .await;
    assert!(pending.is_empty());
    assert_eq!(operator.lock().await.last_cq_attempt_id(), Some(attempt));
    // Success preserves the streak: the next CQ must reach the configured
    // one-CQ switch threshold. This also validates the rollback test's oracle.
    let newer = {
        let mut op = operator.lock().await;
        op.decide_at(0);
        let actions = op.decide_at(0);
        assert!(actions
            .iter()
            .any(|a| matches!(a, pancetta_qso::OperatorAction::FrequencyShift { .. })));
        op.last_cq_attempt_id().unwrap()
    };
    assert_ne!(newer, attempt);
    pending.insert("newer".into(), newer);
    let offset = operator.lock().await.tx_offset_hz();
    super::super::autonomous::settle_self_cq_completion(
        &operator,
        &mut pending,
        "opening",
        false,
        None,
    )
    .await;
    assert_eq!(
        pending,
        std::collections::HashMap::from([("newer".to_string(), newer)])
    );
    assert_eq!(operator.lock().await.last_cq_attempt_id(), Some(newer));
    assert_eq!(operator.lock().await.tx_offset_hz(), offset);
}

/// Drain the Tui channel (bounded crossbeam queue, not broadcast — a
/// message sent before the consuming loop starts polling simply waits
/// in the queue, so no readiness handshake is needed here), collecting
/// EVERY message observed — not just ones matching some predicate — so
/// callers can run multiple independent assertions against the same
/// complete stream afterward (see this module's doc-comment, finding
/// 2). Polls every 10ms up to `max_tries`, but exits early once the
/// channel has gone quiet for `idle_stop` consecutive empty polls after
/// at least one message has been seen — keeps the common case fast
/// without truncating a slow-arriving message burst.
async fn drain_all(
    rx: &crossbeam_channel::Receiver<ComponentMessage>,
    max_tries: u32,
    idle_stop: u32,
) -> Vec<ComponentMessage> {
    let mut out = Vec::new();
    let mut idle = 0u32;
    for _ in 0..max_tries {
        match rx.try_recv() {
            Ok(msg) => {
                out.push(msg);
                idle = 0;
            }
            Err(_) => {
                idle += 1;
                if !out.is_empty() && idle >= idle_stop {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
    out
}

fn rejects_placeholder_at_admission(messages: &[ComponentMessage]) -> bool {
    messages.iter().any(|m| {
        matches!(
            &m.message_type,
            MessageType::DiagnosticEvent { target, callsign, .. }
                if *target == "qso.security" && callsign.as_deref() == Some("<...>")
        )
    })
}

fn ever_queued_placeholder(messages: &[ComponentMessage]) -> bool {
    messages.iter().any(|m| {
        matches!(
            &m.message_type,
            MessageType::StatusUpdate { text, .. }
                if text.contains("Queued") && text.contains("<...>")
        )
    })
}

#[tokio::test]
async fn respond_to_caller_placeholder_rejected_at_admission_not_queued() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();

    let (_tui_tx, tui_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();

    coordinator.start_qso_component().await.unwrap();
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    manager.attach_database(Arc::new(
        pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap(),
    ));
    manager.set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));

    // Pin current_tx_side to Odd: DX CQs on Even, so we answer on the
    // opposite side (Odd) — mirrors
    // `current_tx_side_none_when_idle_then_pins_after_admit`
    // (pancetta-qso/src/qso_manager.rs).
    let priming_id = manager
        .respond_to_cq("K9RDY".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("seeding the parity-pinning QSO");
    assert_eq!(
        manager.current_tx_side().await,
        Some(SlotParity::Odd),
        "priming QSO must pin current_tx_side to Odd"
    );

    // A RespondToCaller whose caller transmits on Odd wants us to reply
    // on the OPPOSITE side (Even) — conflicting with the pinned Odd
    // side. Pre-fix, this exact shape is what got queued into
    // `pending_manual_calls` instead of rejected.
    let msg = ComponentMessage::new(
        ComponentId::Tui,
        ComponentId::Qso,
        MessageType::QsoMessage(QsoMessage::RespondToCaller {
            callsign: "<...>".to_string(),
            frequency: 2000,
            dx_parity: Some(SlotParity::Odd),
            step: ResponseStep::ReportAck,
            snr: Some(-8.0),
            remote_origin: false,
            remote_client_key_id: None,
        }),
        Instant::now(),
    );
    coordinator.message_bus.send_message(msg).await.unwrap();

    // Capture the WHOLE Tui-bound stream this command produced, then
    // check both properties against that one complete buffer — neither
    // assertion can miss a message the other one consumed.
    let messages = drain_all(&tui_rx, 200, 5).await;
    assert!(
        rejects_placeholder_at_admission(&messages),
        "expected a qso.security DiagnosticEvent refusing the unresolved \
         hash placeholder at admission time; observed: {messages:?}"
    );
    assert!(
        !ever_queued_placeholder(&messages),
        "the unresolved hash placeholder must never be queued — even \
         when the opposite-parity gate would otherwise defer the call — \
         or it would repeatedly hold the parity slot against legitimate \
         opposite-parity calls for up to the full queue TTL; observed: \
         {messages:?}"
    );

    // Cleanup (best-effort, not asserted): retire the priming QSO.
    let _ = manager
        .fail_qso(priming_id, pancetta_qso::QsoFailureReason::UserCancelled)
        .await;
}

/// PAN-23 round-2 (Codex review of #283, second finding): the SAME bug
/// class as the `RespondToCaller` test above, via the OTHER admission
/// path — `StartQso`, reachable via a remote `callStation` request whose
/// callsign/parity are accepted without validation upstream. A
/// cross-parity `StartQso` targeting the placeholder would (pre-fix)
/// queue into `pending_manual_calls` first and only hit the
/// deterministic `InvalidCallsign` rejection later at promotion, where
/// the generic failure path re-queues it at the front — repeatedly
/// holding the parity slot for up to the full queue TTL. Identical
/// assertions to the sibling test: the admission-time guard must fire
/// (a `qso.security` diagnostic lands) and the placeholder must never be
/// queued.
#[tokio::test]
async fn start_qso_placeholder_rejected_at_admission_not_queued() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();

    let (_tui_tx, tui_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();

    coordinator.start_qso_component().await.unwrap();
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    manager.attach_database(Arc::new(
        pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .unwrap(),
    ));
    manager.set_dial_frequency_source(Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)));

    // Pin current_tx_side to Odd, same as the RespondToCaller test.
    let priming_id = manager
        .respond_to_cq("K9RDY".to_string(), 1500.0, Some(SlotParity::Even))
        .await
        .expect("seeding the parity-pinning QSO");
    assert_eq!(
        manager.current_tx_side().await,
        Some(SlotParity::Odd),
        "priming QSO must pin current_tx_side to Odd"
    );

    // A StartQso whose DX transmits on Odd wants us to reply on the
    // OPPOSITE side (Even) — conflicting with the pinned Odd side.
    // Pre-fix, this exact shape is what got queued into
    // `pending_manual_calls` instead of rejected.
    let msg = ComponentMessage::new(
        ComponentId::Tui,
        ComponentId::Qso,
        MessageType::QsoMessage(QsoMessage::StartQso {
            callsign: "<...>".to_string(),
            frequency: 2000,
            dx_parity: Some(SlotParity::Odd),
            remote_origin: false,
            remote_client_key_id: None,
            dx_offset_unknown: false,
        }),
        Instant::now(),
    );
    coordinator.message_bus.send_message(msg).await.unwrap();

    // Capture the WHOLE Tui-bound stream this command produced, then
    // check both properties against that one complete buffer.
    let messages = drain_all(&tui_rx, 200, 5).await;
    assert!(
        rejects_placeholder_at_admission(&messages),
        "expected a qso.security DiagnosticEvent refusing the unresolved \
         hash placeholder at admission time; observed: {messages:?}"
    );
    assert!(
        !ever_queued_placeholder(&messages),
        "the unresolved hash placeholder must never be queued — even \
         when the opposite-parity gate would otherwise defer the call — \
         or it would repeatedly hold the parity slot against legitimate \
         opposite-parity calls for up to the full queue TTL; observed: \
         {messages:?}"
    );

    // Cleanup (best-effort, not asserted): retire the priming QSO.
    let _ = manager
        .fail_qso(priming_id, pancetta_qso::QsoFailureReason::UserCancelled)
        .await;
}

/// PAN-72 (Task 7): `start_qso_component`'s `QsoManagerConfig` literal
/// must thread `config.autonomous.qso_stall_switch_after` (Task 6's
/// TOML-facing field) into `TimeoutConfig::qso_stall_switch_after`
/// (Task 2's QSO-engine field) rather than always taking the Rust
/// default — proven here with a non-default value (7; both sides'
/// defaults are 4, so a default-value test couldn't distinguish
/// "threaded" from "coincidentally defaulted").
#[tokio::test]
async fn qso_stall_switch_after_threads_from_autonomous_config() {
    let mut coordinator = test_coordinator().await;
    coordinator
        .config
        .write()
        .await
        .autonomous
        .qso_stall_switch_after = 7;

    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();

    assert_eq!(
        manager.config().timeouts.qso_stall_switch_after,
        7,
        "operator's autonomous.qso_stall_switch_after must reach the \
         QsoManager's TimeoutConfig, not the Rust-side default"
    );
}

/// PAN-72 (Task 8): the QSO event-forwarding task (this file, the
/// `match qso_events.recv().await { ... }` loop spawned in
/// `start_qso_component`) must push `QsoEvent::TxOffsetActionNeeded`
/// (Task 3's variant, emitted for real by `QsoManager::rearm_manual_
/// calls_at` once `QsoMetadata::stall_cycles` hits `TimeoutConfig::
/// qso_stall_switch_after` — see pancetta-qso's own
/// `silence_increments_stall_cycles_via_rearm_and_emits_switch_at_
/// threshold`) onto the new `pending_qso_offset_requests` mailbox.
///
/// Drives the REAL forwarder end to end (real `start_qso_component`,
/// real spawned forwarder task, real `QsoManager` silence-driven stall
/// detector) rather than injecting a synthetic event — there is no
/// public way to inject an arbitrary `QsoEvent` onto a `QsoManager`'s
/// broadcast channel from outside `pancetta-qso`, so a real stall is
/// the only way to exercise this arm end to end.
///
/// Readiness handshake (mirrors `coordinator::health`'s
/// `qso_restart_delivers_recent_qso_outcome_through_the_real_
/// forwarder`): the forwarder's `.subscribe()` happens inside the
/// spawned Qso-component task, not synchronously before
/// `start_qso_component` returns, and a `broadcast` channel only
/// delivers to subscribers already subscribed at send time. Probe with
/// a real `StateChanged`-producing call and poll `active_tx_qsos`
/// (populated by the SAME forwarder loop, same `match` block) until it
/// lands, before relying on the same subscription for the real
/// stall-and-Switch scenario below.
#[tokio::test]
async fn tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_requests() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    coordinator
        .config
        .write()
        .await
        .autonomous
        .qso_stall_switch_after = 2;
    // Auto TX-freq mode -- the silence-driven stall detector only fires
    // there (Hold keeps the operator's offset sticky, no autonomous
    // action is ever emitted).
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );

    coordinator.start_qso_component().await.unwrap();
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // `qso_manager_for_supervisor` is cloned INSIDE `start_qso_component`
    // before `set_tx_freq_mode_source` reassigns the (moved-into-the-
    // spawned-task) manager's `tx_freq_mode` to the coordinator's shared
    // `Arc<AtomicU8>` -- so this clone's own `tx_freq_mode` field is
    // still the manager's original Hold-default `Arc`, disconnected from
    // `coordinator.tx_freq_mode`. Re-point it here so driving the stall
    // through THIS clone (its `qsos` map is the same shared `Arc` either
    // way) observes the same Auto mode the real forwarder-attached
    // manager does.
    manager.set_tx_freq_mode_source(coordinator.tx_freq_mode.clone());

    let mut ready = false;
    for i in 0..20 {
        let probe_call = format!("K9RD{i}");
        let probe_id = manager
            .respond_to_cq_manual(probe_call.clone(), 1500.0, None)
            .await
            .expect("seeding a readiness-probe QSO");
        let key = crate::coordinator::active_tx_qso_key(&probe_id.to_string());
        for _ in 0..30 {
            if coordinator.active_tx_qsos.read().unwrap().contains(&key) {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if ready {
            break;
        }
    }
    assert!(
        ready,
        "readiness probe: real forwarder never populated active_tx_qsos"
    );

    // Real scenario: a manual QSO stalls twice (threshold = 2) and the
    // manager emits `TxOffsetActionNeeded::Switch`.
    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), 1501.0, None)
        .await
        .expect("seeding the stalling QSO");
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();
    // PAN-128: the two rearms are two slots apart so each feeds a
    // distinct own-parity TX slot — the stall counter counts own TX
    // slots, not protocol slots.
    manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::seconds(15))
        .await;
    manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::seconds(45))
        .await;

    let mut found = false;
    for _ in 0..100 {
        if coordinator
            .pending_qso_offset_requests
            .lock()
            .unwrap()
            .iter()
            .any(|req| req.qso_id == qso_id)
        {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        found,
        "TxOffsetActionNeeded must be forwarded into pending_qso_offset_requests"
    );
    assert!(
        coordinator
            .pending_qso_offset_requests
            .lock()
            .unwrap()
            .iter()
            .find(|req| req.qso_id == qso_id)
            .map(|req| req.origin.raised_at_generation().is_some())
            .unwrap_or(false),
        "a stall-detected request must carry the QSO's advance generation \
         so a DX advance landing before the once-per-slot drain invalidates \
         it (PAN-72 finding 8)"
    );
}

/// PAN-79 item 2, coordinator-level pin: the round-2 `TxPolicy` gate and
/// the round-10 `hamlib_hard_muted` wiring (`start_qso_component`'s
/// "Fix E" block) must together mean that a Hamlib supervisor-restart
/// window — a designed multi-slot TX inhibition (AGENTS.md: "TX stays
/// inhibited throughout a Hamlib restart window") — never accumulates
/// stall evidence on an established QSO, and that counting resumes the
/// moment the window clears.
///
/// Drives the REAL `start_qso_component` wiring. The manager comes from
/// `qso_manager_watch`, the Autonomous task's own consumption path: the
/// `qso_manager_for_supervisor` clone is taken before the `set_*_source`
/// calls and still carries the unwired `|| false` default, so the test
/// above (which drives that clone) cannot see this wiring at all. The
/// mute is raised on the coordinator's own `tx_restart_inhibit` atomic,
/// exactly as the supervisor's restart teardown does. The unit-level
/// companion (`a_hamlib_hard_muted_rearm_does_not_count_as_a_stall_cycle`
/// in pancetta-qso) proves the gate through an injected stub; this test
/// proves the production closure actually reads the coordinator's live
/// state.
#[tokio::test]
async fn a_hamlib_restart_window_accumulates_no_stall_cycles_or_offset_actions() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    coordinator
        .config
        .write()
        .await
        .autonomous
        .qso_stall_switch_after = 2;
    // Auto TX-freq mode -- the silence-driven stall detector only fires
    // there.
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );

    coordinator.start_qso_component().await.unwrap();
    // Mirror `autonomous.rs`'s consumption path: `send_replace` runs
    // AFTER every `set_*_source`, so this clone carries the real
    // `hamlib_hard_muted` closure (see
    // `qso_manager_watch_publishes_a_clone_with_injected_sources_not_defaults`).
    let manager = coordinator
        .qso_manager_watch
        .subscribe()
        .borrow()
        .clone()
        .expect("start_qso_component must publish a manager onto the watch channel");

    // Readiness probe, as in the forwarder test above: the forwarder
    // subscribes asynchronously, so wait until a seeded QSO lands in
    // `active_tx_qsos` before relying on the offset-request mailbox.
    let mut ready = false;
    for i in 0..20 {
        let probe_call = format!("K9RD{i}");
        let probe_id = manager
            .respond_to_cq_manual(probe_call.clone(), 1500.0, None)
            .await
            .expect("seeding a readiness-probe QSO");
        let key = crate::coordinator::active_tx_qso_key(&probe_id.to_string());
        for _ in 0..30 {
            if coordinator.active_tx_qsos.read().unwrap().contains(&key) {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if ready {
            break;
        }
    }
    assert!(
        ready,
        "readiness probe: real forwarder never populated active_tx_qsos"
    );

    let qso_id = manager
        .respond_to_cq_manual("K9RST".to_string(), 1501.0, None)
        .await
        .expect("seeding the stalling QSO");
    let opened_at = manager
        .get_qso(qso_id)
        .await
        .unwrap()
        .metadata
        .last_call_at
        .unwrap();

    // Raise the restart window on the coordinator's own atomic -- the one
    // the wiring's closure reads and the supervisor's teardown raises.
    coordinator
        .tx_restart_inhibit
        .store(1, std::sync::atomic::Ordering::Relaxed);

    // Four muted rearms, twice the threshold, two slots apart so each
    // would feed a distinct own-parity TX slot (PAN-128).
    for slot in [15i64, 45, 75, 105] {
        manager
            .rearm_manual_calls_at(opened_at + chrono::Duration::seconds(slot))
            .await;
    }

    let muted = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        muted.metadata.stall_cycles, 0,
        "a Hamlib restart window keys nothing, so the DX's silence is not \
         stall evidence (PAN-79 item 2)"
    );
    assert_eq!(
        muted.metadata.last_call_at,
        Some(opened_at),
        "a muted rearm must not spend any attempt bookkeeping"
    );
    for _ in 0..100 {
        assert!(
            !coordinator
                .pending_qso_offset_requests
                .lock()
                .unwrap()
                .iter()
                .any(|req| req.qso_id == qso_id),
            "no TxOffsetActionNeeded may be raised during a Hamlib restart window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The window clears (the Hamlib restart path's decrement in
    // production). Two more rearms cross the threshold of 2: counting
    // resumes from zero, so the earlier zero was the mute, not a dead
    // detector.
    coordinator
        .tx_restart_inhibit
        .store(0, std::sync::atomic::Ordering::Relaxed);
    manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::seconds(135))
        .await;
    manager
        .rearm_manual_calls_at(opened_at + chrono::Duration::seconds(165))
        .await;
    assert_eq!(
        manager.get_qso(qso_id).await.unwrap().metadata.stall_cycles,
        2,
        "once the restart window clears, each unanswered rearm counts again"
    );

    let mut found = false;
    for _ in 0..100 {
        if coordinator
            .pending_qso_offset_requests
            .lock()
            .unwrap()
            .iter()
            .any(|req| req.qso_id == qso_id)
        {
            found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        found,
        "after the restart window clears, a genuine stall must still raise \
         TxOffsetActionNeeded into pending_qso_offset_requests"
    );
}

/// PAN-72 round 11 (P1 regression): `start_qso_component` used to call
/// `self.qso_manager_watch.send_replace(Some(qso_manager.clone()))`
/// BEFORE any `set_*_source` setter ran on `qso_manager`. Each setter
/// does `self.field = new_arc`, reassigning a field on the ORIGINAL
/// `qso_manager` -- which has no effect whatsoever on a clone already
/// taken, since each is an independent `Arc` handle by that point. So
/// the published clone's atomics (e.g. `tx_freq_mode`) stayed pinned to
/// `QsoManager::new`'s private defaults (`Hold`) forever, no matter what
/// the coordinator's real shared atomics later read.
///
/// The Autonomous task (`autonomous.rs`) exclusively obtains its
/// `QsoManager` handle via `self.qso_manager_watch.subscribe()` +
/// `.borrow().clone()` -- confirmed at `autonomous.rs:1751` and `:2001`
/// -- never through `qso_manager_for_supervisor` or any other path. So
/// with the bug, `apply_tx_offset_switch`'s Hold re-check (Fix 5, Codex
/// round 10) refused EVERY offset switch in production, silently
/// disabling both stall-triggered relocation and operator `u`-nudges
/// end to end, despite this and every other mechanism already being
/// unit-tested in isolation.
///
/// Existing unit tests never caught this because they either construct
/// a `QsoManager` directly and call `set_*_source` on the SAME instance
/// they read back from (no clone-then-publish step at all -- see
/// `qso_manager_watch_refresh_tests` in `autonomous.rs`), or they read
/// back through `qso_manager_for_supervisor` and manually
/// `set_tx_freq_mode_source` on THAT clone to work around the exact
/// staleness this test exists to catch (see the comment on
/// `manager.set_tx_freq_mode_source(...)` in the test above). Neither
/// path exercises the real "clone captured before injection, then
/// published" sequence `start_qso_component` actually runs.
///
/// This test drives the REAL sequence: real `start_qso_component`, then
/// reads back through the EXACT channel Autonomous subscribes to, with
/// no manual re-pointing of any source. It fails on the pre-fix
/// ordering (`Err(OffsetActionHeld)` -- the clone's `tx_freq_mode` never
/// left its private `Hold` default) and passes once `send_replace` runs
/// after every `set_*_source` call.
#[tokio::test]
async fn qso_manager_watch_publishes_a_clone_with_injected_sources_not_defaults() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    // The coordinator's real shared atomic -- Auto is what a normal
    // operating station usually runs, and is the mode the stall
    // detector and `u`-nudge both require to do anything at all.
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );

    coordinator.start_qso_component().await.unwrap();

    // Mirror `autonomous.rs`'s ACTUAL consumption path exactly: fresh
    // `.subscribe()` then `.borrow().clone()` -- no other accessor, no
    // manual re-pointing of any `set_*_source`.
    let manager = coordinator
        .qso_manager_watch
        .subscribe()
        .borrow()
        .clone()
        .expect("start_qso_component must publish a manager onto the watch channel");

    // The `qsos` map is a shared `Arc` regardless of clone timing, so
    // seeding through this clone is unaffected by the bug either way.
    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
        .await
        .expect("seeding the QSO");

    let result = manager
        .apply_tx_offset_switch(
            qso_id,
            1900.0,
            pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
        )
        .await;

    assert!(
        result.is_ok(),
        "the qso_manager_watch clone's tx_freq_mode must reflect the \
         coordinator's live Auto setting injected by `set_tx_freq_mode_source`, \
         not `QsoManager::new`'s private Hold default -- got {result:?}. An \
         `Err(OffsetActionHeld)` here means `send_replace` is (again) \
         publishing the clone before the `set_*_source` calls run."
    );
}

/// PAN-72 (Codex round 1 on PR #350, finding 7): committing an offset
/// action must refresh the `ActiveQsosSnapshot` the TUI banner and the
/// remote gateway render from.
///
/// `apply_tx_offset_switch` is not a state transition, and the snapshot
/// was previously rebuilt only by the `StateChanged`/`QsoCompleted`/
/// `QsoFailed` arms. A stalled exchange is exactly the case where no
/// further transition arrives, so without the new `TxOffsetApplied` arm
/// the operator watches multiple switch/revert cycles while the displayed
/// offset never moves.
#[tokio::test]
async fn applying_an_offset_switch_pushes_a_refreshed_active_qso_snapshot() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    // Register the TUI channel so the forwarder's snapshot sends have
    // somewhere to land (headless test coordinators have no real TUI).
    let (_tui_tx, tui_rx) = coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Tui)
        .await
        .expect("registering a TUI channel");

    coordinator.start_qso_component().await.unwrap();
    // PAN-72 Fix C: apply_tx_offset_switch now re-validates tx_freq_mode
    // inside its own commit lock, and the coordinator's real default is
    // Hold (see the field's doc comment) -- this test drives the direct
    // commit path outside the Auto-gated drain, so it must set Auto
    // itself, exactly as the drain would have already confirmed before
    // ever reaching this call in production.
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // `qso_manager_for_supervisor` is cloned INSIDE `start_qso_component`
    // before `set_tx_freq_mode_source` reassigns the (moved-into-the-
    // spawned-task) manager's `tx_freq_mode` to the coordinator's shared
    // `Arc<AtomicU8>` -- so this clone's own `tx_freq_mode` field is
    // still the manager's original Hold-default `Arc`, disconnected from
    // `coordinator.tx_freq_mode` (see the identical re-pointing done by
    // `tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_
    // requests` above). Re-point it here so this clone's own
    // `apply_tx_offset_switch` call observes the same Auto mode the real
    // forwarder-attached manager does.
    manager.set_tx_freq_mode_source(coordinator.tx_freq_mode.clone());

    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), 1500.0, None)
        .await
        .expect("seeding the QSO");

    // Let the QSO-creation StateChanged snapshot flush through, then
    // clear it so the assertion below can only be satisfied by a NEW push.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        while tui_rx.try_recv().is_ok() {}
        if coordinator
            .active_tx_qsos
            .read()
            .unwrap()
            .contains(&crate::coordinator::active_tx_qso_key(&qso_id.to_string()))
            || std::time::Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    while tui_rx.try_recv().is_ok() {}

    let new_offset = 1900.0;
    let applied = manager
        .apply_tx_offset_switch(
            qso_id,
            new_offset,
            pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
        )
        .await
        .expect("committing the offset switch");
    assert_eq!(applied, new_offset);

    let mut saw_refresh = false;
    for _ in 0..300 {
        while let Ok(msg) = tui_rx.try_recv() {
            if let MessageType::ActiveQsosSnapshot { ref qsos, .. } = msg.message_type {
                if qsos
                    .iter()
                    .any(|q| q.qso_id == qso_id.to_string() && q.frequency_hz == new_offset)
                {
                    saw_refresh = true;
                }
            }
        }
        if saw_refresh {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        saw_refresh,
        "an applied TX-offset move must push a fresh ActiveQsosSnapshot \
         carrying the NEW offset — no state transition follows a stall, so \
         nothing else would ever refresh the banner"
    );
}

/// Block until the QSO component's spawned event-forwarding task has
/// actually `.subscribe()`d, then leave no trace behind.
///
/// The subscription happens INSIDE the spawned task, not synchronously
/// before `start_qso_component` returns, and a `broadcast` channel only
/// delivers to subscribers that already exist at send time — so a QSO
/// opened immediately after `start_qso_component` can have its
/// `StateChanged` dropped on the floor. Mirrors the probe loop in
/// `tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_requests`,
/// with one addition: the probe QSO is retired afterwards (which also
/// clears the shared decoder hint the probe's own `StateChanged` set), so
/// the caller starts from a clean slate.
async fn await_forwarder_subscribed(
    coordinator: &ApplicationCoordinator,
    manager: &pancetta_qso::QsoManager,
) {
    for i in 0..20 {
        let probe_id = manager
            .respond_to_cq_manual(format!("K9RD{i}"), 2400.0, None)
            .await
            .expect("seeding a readiness-probe QSO");
        let key = crate::coordinator::active_tx_qso_key(&probe_id.to_string());
        let mut seen = false;
        for _ in 0..50 {
            if coordinator.active_tx_qsos.read().unwrap().contains(&key) {
                seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = manager
            .fail_qso(probe_id, pancetta_qso::QsoFailureReason::UserCancelled)
            .await;
        if seen {
            // Let the retiring StateChanged flush through, so the probe's
            // own decoder-hint write cannot race the caller's.
            for _ in 0..50 {
                if coordinator.active_qso_freq_hz.read().unwrap().is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            return;
        }
    }
    panic!("readiness probe: the real forwarder never populated active_tx_qsos");
}

/// PAN-72 (Codex round 7 on PR #350, finding 6) — the CRITICAL one.
///
/// An unanswered `CallingCq` that relocates changes the frequency callers
/// will answer on. With the QSO filter enabled (the default) the main FT8
/// decoder is collapsed to ±60 Hz around `active_qso_freq_hz`, while the
/// allocator guarantees the relocation is at least `min_separation_hz`
/// (75 Hz) away — so an answer on the NEW CQ offset landed outside the
/// decode search. And because only a decode can produce the `StateChanged`
/// that refreshes the hint, nothing ever recovered: the station kept
/// calling CQ on a frequency it was structurally unable to hear replies on.
///
/// Drives the REAL forwarder end to end (real `start_qso_component`, real
/// spawned event task, real `apply_tx_offset_switch`) and asserts on the
/// coordinator's own shared decoder hint — the exact value
/// `coordinator::ft8`'s decode loop reads each window.
#[tokio::test]
async fn an_applied_offset_switch_recenters_the_decoder_on_an_unanswered_cq() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    let (_tui_tx, _tui_rx) = coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Tui)
        .await
        .expect("registering a TUI channel");

    coordinator.start_qso_component().await.unwrap();
    // PAN-72 Fix C: apply_tx_offset_switch now re-validates tx_freq_mode
    // inside its own commit lock, and the coordinator's real default is
    // Hold (see the field's doc comment) -- this test drives the direct
    // commit path outside the Auto-gated drain, so it must set Auto
    // itself, exactly as the drain would have already confirmed before
    // ever reaching this call in production.
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // `qso_manager_for_supervisor` is cloned INSIDE `start_qso_component`
    // before `set_tx_freq_mode_source` reassigns the (moved-into-the-
    // spawned-task) manager's `tx_freq_mode` to the coordinator's shared
    // `Arc<AtomicU8>` -- so this clone's own `tx_freq_mode` field is
    // still the manager's original Hold-default `Arc`, disconnected from
    // `coordinator.tx_freq_mode` (see the identical re-pointing done by
    // `tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_
    // requests` above). Re-point it here so this clone's own
    // `apply_tx_offset_switch` call observes the same Auto mode the real
    // forwarder-attached manager does.
    manager.set_tx_freq_mode_source(coordinator.tx_freq_mode.clone());
    await_forwarder_subscribed(&coordinator, &manager).await;

    const CQ_HZ: f64 = 1500.0;
    const NEW_HZ: f64 = 1900.0;
    let qso_id = manager
        .start_cq_manual(CQ_HZ, None, false, None)
        .await
        .expect("seeding an unanswered CQ");

    // The forwarder's StateChanged arm seeds the hint with the CQ offset.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if coordinator.active_qso_freq_hz.read().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the real forwarder never seeded the decoder hint"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        *coordinator.active_qso_freq_hz.read().unwrap(),
        Some(ActiveQsoFreq {
            primary_hz: CQ_HZ,
            secondary: None
        }),
        "precondition: the decoder is centred on the original CQ offset, \
         with no secondary hint before any relocation has happened"
    );

    let applied = manager
        .apply_tx_offset_switch(
            qso_id,
            NEW_HZ,
            pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
        )
        .await
        .expect("committing the offset switch");
    assert_eq!(applied, NEW_HZ);
    assert!(
        (NEW_HZ - CQ_HZ).abs() > crate::coordinator::qso_filter::DEFAULT_HALF_WINDOW_HZ,
        "test premise: the move must be bigger than the decoder's own \
         half-window, or a stale hint would still cover the new offset"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if coordinator
            .active_qso_freq_hz
            .read()
            .unwrap()
            .is_some_and(|hint| hint.primary_hz == NEW_HZ)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an applied TX-offset move on an unanswered CQ must re-centre \
             the decoder on the NEW offset — otherwise the ±60 Hz QSO \
             filter stays parked on the abandoned frequency and an answer \
             to our own CQ can never be decoded, so no StateChanged ever \
             arrives to fix it; hint was {:?}",
            *coordinator.active_qso_freq_hz.read().unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // PAN-72 round-8 redesign (fix 2): the vacated CQ offset must STILL
    // be covered by the decoder hint (as the secondary), so a caller
    // that answered the pre-switch CQ frame is not filtered out —
    // Codex round 8, finding 1.
    //
    // PAN-72 round-9 fix (finding 1): the secondary now also carries an
    // `expires_at` (real wall-clock, so not asserted for an exact
    // value) alongside the frequency — destructure rather than a plain
    // tuple `assert_eq!`.
    match *coordinator.active_qso_freq_hz.read().unwrap() {
        Some(ActiveQsoFreq {
            primary_hz: primary,
            secondary: Some((secondary_hz, expires_at)),
        }) => {
            assert_eq!(primary, NEW_HZ, "primary must be the new offset");
            assert_eq!(
                secondary_hz, CQ_HZ,
                "the vacated CQ offset must remain a live secondary decoder \
                 hint for the grace window so a reply to the pre-switch \
                 frame is not dropped by the band-collapse filter"
            );
            assert!(
                expires_at > chrono::Utc::now(),
                "the secondary hint's expires_at must still be in the future \
                 immediately after the switch"
            );
        }
        other => {
            panic!("expected a primary+secondary decoder hint after the switch, got {other:?}")
        }
    }

    // The QSO's own state must agree — the hint is derived, not invented.
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.partner_freq, None,
        "an unanswered CQ has no DX to point partner_freq at (round 5's \
         fix); the state frequency is the only correct hint"
    );
    assert_eq!(
        progress.metadata.pre_switch_offset.map(|p| p.offset_hz),
        Some(CQ_HZ),
        "precondition: the QSO's own pre_switch_offset is what the \
         secondary hint is derived from"
    );
}

/// The established-QSO half of finding 6: after a switch the DX is still
/// transmitting on the offset we vacated, so the decoder hint must follow
/// `partner_freq` (which `apply_tx_offset_switch` latches to the old
/// offset), NOT our new TX offset. Guards against "fixing" the CQ case by
/// blindly pushing the applied offset into the hint.
#[tokio::test]
async fn an_applied_offset_switch_keeps_an_established_qso_pointed_at_the_dx() {
    let mut coordinator = test_coordinator().await;
    coordinator.config.write().await.station.callsign = "K1TEST".to_string();
    let (_tui_tx, _tui_rx) = coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Tui)
        .await
        .expect("registering a TUI channel");

    coordinator.start_qso_component().await.unwrap();
    // PAN-72 Fix C: apply_tx_offset_switch now re-validates tx_freq_mode
    // inside its own commit lock, and the coordinator's real default is
    // Hold (see the field's doc comment) -- this test drives the direct
    // commit path outside the Auto-gated drain, so it must set Auto
    // itself, exactly as the drain would have already confirmed before
    // ever reaching this call in production.
    coordinator.tx_freq_mode.store(
        pancetta_core::TxFreqMode::Auto.as_u8(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // `qso_manager_for_supervisor` is cloned INSIDE `start_qso_component`
    // before `set_tx_freq_mode_source` reassigns the (moved-into-the-
    // spawned-task) manager's `tx_freq_mode` to the coordinator's shared
    // `Arc<AtomicU8>` -- so this clone's own `tx_freq_mode` field is
    // still the manager's original Hold-default `Arc`, disconnected from
    // `coordinator.tx_freq_mode` (see the identical re-pointing done by
    // `tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_
    // requests` above). Re-point it here so this clone's own
    // `apply_tx_offset_switch` call observes the same Auto mode the real
    // forwarder-attached manager does.
    manager.set_tx_freq_mode_source(coordinator.tx_freq_mode.clone());
    await_forwarder_subscribed(&coordinator, &manager).await;

    const DX_HZ: f64 = 1500.0;
    const NEW_HZ: f64 = 1900.0;
    let qso_id = manager
        .respond_to_cq_manual("K9ZZ".to_string(), DX_HZ, None)
        .await
        .expect("seeding an established QSO");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if coordinator.active_qso_freq_hz.read().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the real forwarder never seeded the decoder hint"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    manager
        .apply_tx_offset_switch(
            qso_id,
            NEW_HZ,
            pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
        )
        .await
        .expect("committing the offset switch");

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.partner_freq,
        Some(DX_HZ),
        "precondition: the switch latched the vacated offset as the DX's"
    );

    // Give the forwarder ample opportunity to get this wrong.
    for _ in 0..30 {
        assert_eq!(
            *coordinator.active_qso_freq_hz.read().unwrap(),
            Some(ActiveQsoFreq {
                primary_hz: DX_HZ,
                secondary: None
            }),
            "the decoder must stay on the DX's transmit offset after OUR \
             side moves — the switch is TX-only; an established QSO's \
             stable partner_freq hint never gains a PAN-72 round-8 \
             secondary window (that's reserved for an unanswered CallingCq)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// PAN-72 final-review CRITICAL regression: `start_qso_component` must
/// publish its fresh `QsoManager` onto `qso_manager_watch` in a way that
/// is observable by a receiver that does not yet exist.
///
/// `ApplicationCoordinator::new` builds the channel with
/// `watch::channel(None).0`, immediately dropping the initial `Receiver`;
/// `run()` then awaits `start_qso_component` BEFORE
/// `start_autonomous_component` (the sole `.subscribe()` site). Tokio's
/// `watch::Sender::send` returns `Err` *without storing the value* when
/// `receiver_count() == 0`, so the original `let _ = ...send(..)` silently
/// discarded the very first handle: every later `subscribe()` was seeded
/// with `None`, the Autonomous task's per-tick `borrow()` never yielded a
/// manager, and `drain_pending_qso_offset_requests` never ran — the whole
/// mid-QSO adaptive-offset feature was inert in production until the first
/// Qso-component crash+restart. `send_replace` stores unconditionally.
///
/// Deliberately asserts against the REAL production path (a real
/// coordinator, a real `start_qso_component`), not a test-constructed
/// channel with an artificially retained seed receiver — that retained
/// receiver is exactly what hid this bug from
/// `autonomous::qso_manager_watch_refresh_tests`.
#[tokio::test]
async fn start_qso_component_seeds_qso_manager_watch_with_no_receivers_yet() {
    let mut coordinator = test_coordinator().await;

    // Precondition: the coordinator ships with zero live receivers, which
    // is precisely the condition under which `send` would no-op.
    assert_eq!(
        coordinator.qso_manager_watch.receiver_count(),
        0,
        "precondition: no receiver exists before start_autonomous_component"
    );
    assert!(
        coordinator.qso_manager_watch.borrow().is_none(),
        "precondition: the watch starts empty"
    );

    coordinator.start_qso_component().await.unwrap();

    assert!(
        coordinator.qso_manager_watch.borrow().is_some(),
        "start_qso_component must store the fresh QsoManager on the watch \
         channel even with zero receivers (send_replace, not send)"
    );

    // The consumer-side view: a receiver subscribing AFTER the fact (the
    // real ordering — `start_autonomous_component` runs later) must be
    // seeded with the stored handle, not `None`.
    let rx = coordinator.qso_manager_watch.subscribe();
    assert!(
        rx.borrow().is_some(),
        "a late subscriber (the Autonomous task) must be seeded with the \
         stored QsoManager, or drain_pending_qso_offset_requests never runs"
    );
}

// ── PAN-76: Fox-admitted QSOs through the real Qso component ────────────

/// Poll `cond` every 10 ms for up to 2 s.
async fn pan76_poll(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Drain `rx` until a message matching `pred` arrives (bounded, 2 s).
async fn pan76_wait_for(
    rx: &crossbeam_channel::Receiver<ComponentMessage>,
    pred: impl Fn(&MessageType) -> bool,
) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        while let Ok(msg) = rx.try_recv() {
            if pred(&msg.message_type) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The real Qso component for K5ARH with `max_concurrent_qsos = 1`, its
/// event forwarder proven subscribed (readiness probe, then the probe QSO
/// failed and gone from `active_tx_qsos`), and a Tui channel to observe.
pub(super) async fn pan76_station() -> (
    ApplicationCoordinator,
    pancetta_qso::QsoManager,
    crossbeam_channel::Receiver<ComponentMessage>,
) {
    let mut coordinator = test_coordinator().await;
    {
        let mut config = coordinator.config.write().await;
        config.station.callsign = "K5ARH".to_string();
        config.autonomous.max_concurrent_qsos = 1;
    }
    let (_tui_tx, tui_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Tui)
        .await
        .unwrap();
    // PAN-280: a TX worker channel, so closing frames are accepted and
    // stay in flight as on a live station. Without one every
    // `TransmitRequest` is self-reported not sent, and a Hound opened at
    // its close is retired `CloseNotAired` instead of entering its grace.
    let (_tx_worker_tx, tx_worker_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    std::mem::forget(tx_worker_rx);
    coordinator.start_qso_component().await.unwrap();
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();

    let mut probe = None;
    for i in 0..20 {
        let probe_id = manager
            .respond_to_cq_manual(format!("K9RD{i}"), 2400.0, None)
            .await
            .expect("seeding a readiness-probe QSO");
        let key = crate::coordinator::active_tx_qso_key(&probe_id.to_string());
        if pan76_poll(|| coordinator.active_tx_qsos.read().unwrap().contains(&key)).await {
            probe = Some((probe_id, key));
            break;
        }
        manager.cancel_qso(probe_id).await.unwrap();
    }
    let (probe_id, probe_key) =
        probe.expect("readiness probe: real forwarder never populated active_tx_qsos");
    manager
        .fail_qso(probe_id, pancetta_qso::QsoFailureReason::UserCancelled)
        .await
        .unwrap();
    assert!(
        pan76_poll(|| !coordinator
            .active_tx_qsos
            .read()
            .unwrap()
            .contains(&probe_key))
        .await,
        "the failed probe QSO must leave active_tx_qsos"
    );
    while tui_rx.try_recv().is_ok() {}
    (coordinator, manager, tui_rx)
}

pub(super) async fn pan76_set_fox_mode(
    coordinator: &ApplicationCoordinator,
    tui_rx: &crossbeam_channel::Receiver<ComponentMessage>,
    on: bool,
) {
    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::SetFoxMode { on }),
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        pan76_wait_for(
            tui_rx,
            |m| matches!(m, MessageType::FoxModeStatus { on: o } if *o == on)
        )
        .await,
        "SetFoxMode {{ on: {on} }} must be acknowledged with FoxModeStatus"
    );
}

fn pan76_key(id: &pancetta_qso::QsoId) -> String {
    crate::coordinator::active_tx_qso_key(&id.to_string())
}

fn pan76_request(qso_id: &pancetta_qso::QsoId, text: &str, freq: f64) -> MessageType {
    MessageType::TransmitRequest {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: freq,
        qso_id: Some(qso_id.to_string()),
        tx_parity: None,
        origin: crate::message_bus::TxOrigin::Local,
        remote_client_key_id: None,
    }
}

/// The next backlog through the real `coalesce_backlog_into`: `head`
/// first, then `queued` in order, under `max_concurrent_qsos`.
async fn pan76_coalesce(
    coordinator: &ApplicationCoordinator,
    head: MessageType,
    queued: Vec<MessageType>,
    max_concurrent_qsos: u32,
) -> MessageType {
    let (backlog_tx, backlog_rx) = crossbeam_channel::unbounded();
    for request in queued {
        backlog_tx
            .send(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                request,
                Instant::now(),
            ))
            .unwrap();
    }
    let arm = Arc::new(std::sync::Mutex::new(pancetta_agent::arm::ArmState::new()));
    let audit_log = pancetta_agent::audit::AuditLog::new(std::env::temp_dir().join(format!(
        "pancetta-pan76-audit-{}-{}.log",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));
    let display_feed_enabled = Arc::new(AtomicBool::new(false));
    let backlog_inbox =
        crate::coordinator::tx::TxInbox::new(backlog_rx.clone(), Default::default());
    crate::coordinator::tx::coalesce_backlog_into(
        head,
        &backlog_inbox,
        &coordinator.message_bus,
        &coordinator.active_tx_qsos,
        &coordinator.fox_admitted_qsos,
        max_concurrent_qsos,
        &arm,
        &audit_log,
        &display_feed_enabled,
        None,
    )
    .await
}

/// AC1: engage Fox mode, admit three Hounds (one by converting the Fox
/// CQ, two through the real `maybe_answer_caller`), turn Fox mode off
/// mid-exchange, and the next backlog keeps all three on the air under
/// the restored cap of 1.
#[tokio::test]
async fn pan76_fox_disengage_keeps_admitted_hounds_on_air() {
    use crate::coordinator::tx::{
        effective_max_concurrent_qsos, live_fox_admitted_count, supersede_max_concurrent_qsos,
    };

    let (coordinator, manager, tui_rx) = pan76_station().await;

    // 1. Engage: the Fox CQ is recorded.
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cq_id = manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, p)| matches!(p.state, pancetta_qso::QsoState::CallingCq { .. }))
        .map(|(id, _)| id)
        .expect("Fox mode must start a CQ");
    assert_eq!(
        coordinator.fox_admitted_qsos.read().unwrap().clone(),
        std::collections::HashSet::from([pan76_key(&cq_id)]),
        "the Fox CQ must be the only Fox-admitted QSO after engage"
    );

    // 2. Hound 1 answers within 15 Hz of the CQ and converts it.
    manager
        .process_message(
            pancetta_qso::states::MessageType::CqResponse {
                calling_station: "K5ARH".to_string(),
                responding_station: "W1AAA".to_string(),
                grid: Some("FN31".to_string()),
            },
            "K5ARH W1AAA FN31".to_string(),
            305.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            manager.get_qso(cq_id).await.unwrap().state,
            pancetta_qso::QsoState::WaitingForReport { .. }
        ),
        "Hound 1 must convert the Fox CQ into its exchange"
    );

    // 3. Hounds 2 and 3 through the real auto-answer path.
    let side = manager.current_tx_side().await;
    let mut hound_ids = vec![cq_id];
    for (call, freq) in [("W2BBB", 1200.0), ("W3CCC", 1800.0)] {
        super::maybe_answer_caller(
            &pancetta_qso::states::MessageType::CqResponse {
                calling_station: "K5ARH".to_string(),
                responding_station: call.to_string(),
                grid: Some("FN31".to_string()),
            },
            "K5ARH",
            freq,
            side.map(|s| s.opposite()),
            -12.0,
            &manager,
            &coordinator.tx_policy,
            pancetta_config::UnattendedPolicy::Off,
            true,
            1,
            &coordinator.message_bus,
            &coordinator.fox_mode,
            &coordinator.fox_max_streams,
            &coordinator.fox_admitted_qsos,
        )
        .await;
        let id = manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(_, p)| p.metadata.their_callsign.as_deref() == Some(call))
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("{call} must be auto-answered in Fox mode"));
        hound_ids.push(id);
    }

    // 4. All three are TX-live and Fox-admitted.
    let keys: Vec<String> = hound_ids.iter().map(pan76_key).collect();
    assert!(
        pan76_poll(|| {
            let active = coordinator.active_tx_qsos.read().unwrap();
            let fox = coordinator.fox_admitted_qsos.read().unwrap();
            keys.iter().all(|k| active.contains(k) && fox.contains(k))
        })
        .await,
        "all three Hounds must be in both active_tx_qsos and fox_admitted_qsos"
    );

    // 5. Disengage mid-exchange: the Hounds keep running.
    pan76_set_fox_mode(&coordinator, &tui_rx, false).await;
    assert!(!coordinator
        .fox_mode
        .load(std::sync::atomic::Ordering::Relaxed));
    let active_ids: Vec<_> = manager
        .get_active_qsos()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for id in &hound_ids {
        assert!(active_ids.contains(id), "Hound {id} must still be active");
    }

    // 6. The next backlog under the restored cap keeps all three.
    let cap = effective_max_concurrent_qsos(1, &coordinator.fox_mode, &coordinator.fox_max_streams);
    assert_eq!(cap, 1, "Fox mode is off, so the base cap is back to 1");
    let result = pan76_coalesce(
        &coordinator,
        pan76_request(&hound_ids[0], "W1AAA K5ARH -10", 305.0),
        vec![
            pan76_request(&hound_ids[1], "W2BBB K5ARH -12", 1200.0),
            pan76_request(&hound_ids[2], "W3CCC K5ARH -08", 1800.0),
        ],
        cap,
    )
    .await;
    match result {
        MessageType::MultiTransmitRequest { items, .. } => {
            let mut got: Vec<String> = items.iter().filter_map(|it| it.qso_id.clone()).collect();
            let mut want: Vec<String> = hound_ids.iter().map(|id| id.to_string()).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "all three Hounds must stay in the bundle");
        }
        other => panic!("expected a three-Hound MultiTransmitRequest, got {other:?}"),
    }

    // 7. A mid-TX refresh keeps the three-stream bundle too: the three
    //    live Hounds on top of the restored cap of 1.
    assert_eq!(
        supersede_max_concurrent_qsos(
            cap,
            live_fox_admitted_count(&coordinator.fox_admitted_qsos, &coordinator.active_tx_qsos)
        ),
        4
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// Turning Fox mode off cancels an unanswered Fox CQ, and the forwarder's
/// `Failed` removal takes it out of the Fox set with `active_tx_qsos`.
#[tokio::test]
async fn pan76_unanswered_fox_cq_leaves_the_fox_set_on_disengage() {
    let (coordinator, manager, tui_rx) = pan76_station().await;

    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cq_key = manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, p)| matches!(p.state, pancetta_qso::QsoState::CallingCq { .. }))
        .map(|(id, _)| pan76_key(&id))
        .expect("Fox mode must start a CQ");
    assert!(coordinator
        .fox_admitted_qsos
        .read()
        .unwrap()
        .contains(&cq_key));

    pan76_set_fox_mode(&coordinator, &tui_rx, false).await;
    assert!(
        pan76_poll(|| {
            !coordinator
                .fox_admitted_qsos
                .read()
                .unwrap()
                .contains(&cq_key)
                && !coordinator.active_tx_qsos.read().unwrap().contains(&cq_key)
        })
        .await,
        "the cancelled Fox CQ must leave both fox_admitted_qsos and active_tx_qsos"
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// AC2: with Fox mode never engaged, three ordinary QSOs opened over
/// `max_concurrent_qsos = 1` stay capped: the Fox set is empty and the
/// next backlog keeps one stream and reports the other two as not sent.
#[tokio::test]
async fn pan76_never_fox_manual_pileup_is_still_capped() {
    use crate::coordinator::tx::{
        effective_max_concurrent_qsos, live_fox_admitted_count, supersede_max_concurrent_qsos,
    };

    let (coordinator, manager, tui_rx) = pan76_station().await;
    let (_autonomous_tx, autonomous_rx) = coordinator
        .message_bus
        .create_channel(ComponentId::Autonomous)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for (call, freq) in [("K1AAA", 1000.0), ("K2BBB", 1300.0), ("K3CCC", 1600.0)] {
        ids.push(
            manager
                .respond_to_cq_manual(call.to_string(), freq, Some(SlotParity::Even))
                .await
                .unwrap(),
        );
    }
    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::StartQso {
                callsign: "K4DDD".to_string(),
                frequency: 1900,
                dx_parity: Some(SlotParity::Even),
                remote_origin: false,
                remote_client_key_id: None,
                dx_offset_unknown: false,
            }),
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        pan76_wait_for(
            &tui_rx,
            |m| matches!(m, MessageType::StatusUpdate { text: t, .. } if t.contains("K4DDD"))
        )
        .await,
        "the Qso component must handle the StartQso"
    );
    let keys: Vec<String> = ids.iter().map(pan76_key).collect();
    assert!(
        pan76_poll(|| {
            let active = coordinator.active_tx_qsos.read().unwrap();
            keys.iter().all(|k| active.contains(k))
        })
        .await,
        "the three manual QSOs must be TX-live"
    );
    assert!(
        coordinator.fox_admitted_qsos.read().unwrap().is_empty(),
        "no ordinary open path may record a Fox-admitted QSO"
    );

    let cap = effective_max_concurrent_qsos(1, &coordinator.fox_mode, &coordinator.fox_max_streams);
    assert_eq!(cap, 1);
    let result = pan76_coalesce(
        &coordinator,
        pan76_request(&ids[0], "K1AAA K5ARH EM10", 1000.0),
        vec![
            pan76_request(&ids[1], "K2BBB K5ARH EM10", 1300.0),
            pan76_request(&ids[2], "K3CCC K5ARH EM10", 1600.0),
        ],
        cap,
    )
    .await;
    let ids_s: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    assert!(
        matches!(
            &result,
            MessageType::TransmitRequest { qso_id: Some(id), .. } if *id == ids_s[0]
        ),
        "only K1AAA may transmit under the cap of 1, got {result:?}"
    );
    // `report_transmit_complete` sends each truncated frame's failed
    // completion here and its `TxFrameNotSent` to the (running) Qso loop.
    let mut not_sent = Vec::new();
    while let Ok(msg) = autonomous_rx.try_recv() {
        if let MessageType::TransmitComplete {
            success: false,
            qso_id: Some(id),
            ..
        } = msg.message_type
        {
            not_sent.push(id);
        }
    }
    not_sent.sort();
    let mut truncated = vec![ids_s[1].clone(), ids_s[2].clone()];
    truncated.sort();
    assert_eq!(
        not_sent, truncated,
        "K2BBB and K3CCC must each be reported as not sent"
    );

    assert_eq!(
        supersede_max_concurrent_qsos(
            cap,
            live_fox_admitted_count(&coordinator.fox_admitted_qsos, &coordinator.active_tx_qsos)
        ),
        1
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// Send `SetFoxMode { on: true }` and return the status lines emitted
/// before its `FoxModeStatus` acknowledgement.
async fn pan76_engage_fox_statuses(
    coordinator: &ApplicationCoordinator,
    tui_rx: &crossbeam_channel::Receiver<ComponentMessage>,
) -> Vec<String> {
    coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::SetFoxMode { on: true }),
            Instant::now(),
        ))
        .await
        .unwrap();
    let mut statuses = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        while let Ok(msg) = tui_rx.try_recv() {
            match msg.message_type {
                MessageType::StatusUpdate { text, .. } => statuses.push(text),
                MessageType::FoxModeStatus { on: true } => return statuses,
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "SetFoxMode {{ on: true }} must be acknowledged with FoxModeStatus"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn pan76_calling_cqs(manager: &pancetta_qso::QsoManager) -> Vec<pancetta_qso::QsoId> {
    manager
        .get_active_qsos()
        .await
        .into_iter()
        .filter(|(_, p)| matches!(p.state, pancetta_qso::QsoState::CallingCq { .. }))
        .map(|(id, _)| id)
        .collect()
}

/// Re-engaging Fox mode starts no second CQ while the Fox CQ is still
/// calling, and none while Hounds from an earlier engage still fill
/// `fox_max_streams`; a Hound converting that CQ would bypass
/// `maybe_answer_caller`'s cap. Once the finishing Hounds drop below the
/// cap, a re-engage starts the CQ again.
#[tokio::test]
async fn pan76_fox_reengage_starts_no_cq_while_finishing_hounds_fill_the_cap() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    coordinator
        .fox_max_streams
        .store(2, std::sync::atomic::Ordering::Relaxed);

    // 1. Engage, then engage again while the Fox CQ is still calling.
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let cq_id = match pan76_calling_cqs(&manager).await.as_slice() {
        [id] => *id,
        other => panic!("Fox mode must start exactly one CQ, got {other:?}"),
    };
    let statuses = pan76_engage_fox_statuses(&coordinator, &tui_rx).await;
    assert_eq!(
        pan76_calling_cqs(&manager).await,
        vec![cq_id],
        "a repeated engage must not start a second Fox CQ"
    );
    assert!(
        statuses.iter().any(|s| s.contains("no new CQ")),
        "the operator must be told no CQ was started, got {statuses:?}"
    );

    // 2. Hound 1 converts the Fox CQ; Hound 2 through the real
    //    auto-answer path. Two Fox-admitted Hounds = fox_max_streams.
    manager
        .process_message(
            pancetta_qso::states::MessageType::CqResponse {
                calling_station: "K5ARH".to_string(),
                responding_station: "W1AAA".to_string(),
                grid: Some("FN31".to_string()),
            },
            "K5ARH W1AAA FN31".to_string(),
            305.0,
            Some(-10.0),
        )
        .await
        .unwrap();
    let side = manager.current_tx_side().await;
    super::maybe_answer_caller(
        &pancetta_qso::states::MessageType::CqResponse {
            calling_station: "K5ARH".to_string(),
            responding_station: "W2BBB".to_string(),
            grid: Some("FN31".to_string()),
        },
        "K5ARH",
        1200.0,
        side.map(|s| s.opposite()),
        -12.0,
        &manager,
        &coordinator.tx_policy,
        pancetta_config::UnattendedPolicy::Off,
        true,
        1,
        &coordinator.message_bus,
        &coordinator.fox_mode,
        &coordinator.fox_max_streams,
        &coordinator.fox_admitted_qsos,
    )
    .await;
    let w2_id = manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(_, p)| p.metadata.their_callsign.as_deref() == Some("W2BBB"))
        .map(|(id, _)| id)
        .expect("W2BBB must be auto-answered in Fox mode");
    assert!(pan76_calling_cqs(&manager).await.is_empty());

    // 3. Off and on again while both Hounds are finishing: no new CQ.
    pan76_set_fox_mode(&coordinator, &tui_rx, false).await;
    let statuses = pan76_engage_fox_statuses(&coordinator, &tui_rx).await;
    assert!(coordinator
        .fox_mode
        .load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        pan76_calling_cqs(&manager).await.is_empty(),
        "re-engage must not start a CQ while finishing Hounds fill fox_max_streams"
    );
    assert!(
        statuses.iter().any(|s| s.contains("no new CQ")),
        "the operator must be told no CQ was started, got {statuses:?}"
    );

    // 4. One Hound finishes: below the cap, a re-engage starts the CQ.
    manager
        .fail_qso(w2_id, pancetta_qso::QsoFailureReason::UserCancelled)
        .await
        .unwrap();
    pan76_set_fox_mode(&coordinator, &tui_rx, false).await;
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let new_cq = match pan76_calling_cqs(&manager).await.as_slice() {
        [id] => *id,
        other => panic!("re-engage below the cap must start one CQ, got {other:?}"),
    };
    assert!(coordinator
        .fox_admitted_qsos
        .read()
        .unwrap()
        .contains(&pan76_key(&new_cq)));

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PR #421 review (Codex P2): Fox off then straight back on. The Fox CQ
/// the disengage cancelled has left `get_active_qsos()` but, until the
/// event forwarder handles its failure, would still sit in both shared
/// sets and read as a Hound in its completed grace. With
/// `fox_max_streams = 1` that filled the cap and the re-engage started no
/// CQ. The disengage drops a cancelled CQ from the Fox set itself.
#[tokio::test]
async fn pan76_fox_off_and_straight_on_starts_a_fresh_cq() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    coordinator
        .fox_max_streams
        .store(1, std::sync::atomic::Ordering::Relaxed);
    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    let old_cq = match pan76_calling_cqs(&manager).await.as_slice() {
        [id] => *id,
        other => panic!("Fox mode must start exactly one CQ, got {other:?}"),
    };
    let old_key = pan76_key(&old_cq);
    assert!(
        pan76_poll(|| coordinator
            .active_tx_qsos
            .read()
            .unwrap()
            .contains(&old_key))
        .await,
        "the Fox CQ must be TX-live before the toggle"
    );

    // Both toggles back to back: no wait for the off acknowledgement, so
    // the re-engage can run before the forwarder sees the cancel.
    for on in [false, true] {
        coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Qso,
                MessageType::QsoMessage(QsoMessage::SetFoxMode { on }),
                Instant::now(),
            ))
            .await
            .unwrap();
    }
    assert!(
        pan76_wait_for(&tui_rx, |m| matches!(
            m,
            MessageType::FoxModeStatus { on: true }
        ))
        .await,
        "the re-engage must be acknowledged"
    );
    match pan76_calling_cqs(&manager).await.as_slice() {
        [id] => assert_ne!(*id, old_cq, "the old CQ was cancelled"),
        other => panic!("the re-engage must start one fresh CQ, got {other:?}"),
    }
    assert!(
        !coordinator
            .fox_admitted_qsos
            .read()
            .unwrap()
            .contains(&old_key),
        "the cancelled CQ must leave the Fox set"
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}

/// PR #421 review (Codex P2): a completed Hound keeps its Fox-admitted
/// TX slot for the 45 s completed grace so its final RR73/73 can air, but
/// it has already left `get_active_qsos()`. Re-engaging Fox mode while
/// such Hounds fill `fox_max_streams` must start no CQ, or the old closing
/// frames plus a fresh set of Hounds can exceed `MAX_RETAINED_TX_STREAMS`
/// and the hard clamp cuts exactly those closing frames.
#[tokio::test]
async fn pan76_fox_reengage_counts_hounds_in_their_completed_grace() {
    let (coordinator, manager, tui_rx) = pan76_station().await;
    coordinator
        .fox_max_streams
        .store(2, std::sync::atomic::Ordering::Relaxed);

    pan76_set_fox_mode(&coordinator, &tui_rx, true).await;
    assert_eq!(pan76_calling_cqs(&manager).await.len(), 1);
    let mut known = coordinator.fox_admitted_qsos.read().unwrap().clone();

    // Two Hounds send RR73 straight to us: the real auto-answer path
    // opens each at its close, so it completes at once and enters grace.
    let mut hound_keys = Vec::new();
    for (call, freq) in [("W1AAA", 1200.0), ("W2BBB", 1800.0)] {
        let side = manager.current_tx_side().await;
        super::maybe_answer_caller(
            &pancetta_qso::states::MessageType::FinalConfirmation {
                to_station: "K5ARH".to_string(),
                from_station: call.to_string(),
            },
            "K5ARH",
            freq,
            side.map(|s| s.opposite()),
            -12.0,
            &manager,
            &coordinator.tx_policy,
            pancetta_config::UnattendedPolicy::Off,
            true,
            1,
            &coordinator.message_bus,
            &coordinator.fox_mode,
            &coordinator.fox_max_streams,
            &coordinator.fox_admitted_qsos,
        )
        .await;
        let now = coordinator.fox_admitted_qsos.read().unwrap().clone();
        let new: Vec<String> = now.difference(&known).cloned().collect();
        assert_eq!(new.len(), 1, "{call} must be recorded as Fox-admitted");
        hound_keys.push(new[0].clone());
        known = now;
    }
    for key in &hound_keys {
        assert!(
            pan76_poll(|| coordinator.active_tx_qsos.read().unwrap().contains(key)).await,
            "a completed Hound stays TX-live through its grace"
        );
    }
    assert_eq!(
        manager.active_caller_qso_count().await,
        0,
        "both Hounds are completed, not active"
    );

    // Off (cancels the unanswered Fox CQ) and on again inside the grace.
    pan76_set_fox_mode(&coordinator, &tui_rx, false).await;
    assert!(pan76_calling_cqs(&manager).await.is_empty());
    let statuses = pan76_engage_fox_statuses(&coordinator, &tui_rx).await;
    assert!(
        pan76_calling_cqs(&manager).await.is_empty(),
        "re-engage must not start a CQ while grace-period Hounds fill fox_max_streams"
    );
    assert!(
        statuses.iter().any(|s| s.contains("no new CQ")),
        "the operator must be told no CQ was started, got {statuses:?}"
    );

    coordinator
        .shutdown_signal
        .store(true, std::sync::atomic::Ordering::Release);
}
