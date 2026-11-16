use super::pan115_worker_gate_tests::{
    await_ptt_on, worker_harness, worker_harness_with_config, Harness,
};
use super::*;
use crate::message_bus::{RigControlMessage, TxOrigin};
use crossbeam_channel::Receiver;
use pancetta_agent::arm::{ArmTime, ConsentChannel, VerifiedArmGrant};
use std::sync::Arc;

const TEXT: &str = "CQ N0CALL FN20";

async fn remote_playback() -> (Harness, Receiver<ComponentMessage>) {
    remote_playback_with(worker_harness().await).await
}

async fn remote_playback_with(h: Harness) -> (Harness, Receiver<ComponentMessage>) {
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    {
        let mut arm = h.coordinator.remote_tx_arm.lock().unwrap();
        let now_ms = super::super::now_epoch_ms() as i64;
        arm.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
        arm.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: 120_000,
                armed_until_ms: now_ms + 120_000,
                capability_exp_ms: now_ms + 120_000 + 3_600_000,
                tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
                effective_ttl_ms: 120_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan135-worker".into(),
                client_key_id: "client-a".into(),
            },
            ArmTime::now(),
        );
    }
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::StationAgent,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: TEXT.into(),
                frequency_offset: 1500.0,
                qso_id: None,
                tx_parity: None,
                origin: TxOrigin::Remote,
                remote_client_key_id: Some("client-a".into()),
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "remote frame must key"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if audio_rx.try_iter().any(|m| {
                matches!(m.message_type,
                MessageType::AudioOutput { samples, .. } if !samples.is_empty())
            }) {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("frame must enter playback");
    (h, audio_rx)
}

async fn pan105_playback(
    bundle: bool,
    origin: TxOrigin,
) -> (
    Harness,
    Receiver<ComponentMessage>,
    Arc<std::sync::atomic::AtomicI64>,
    ArmTime,
) {
    let base = ArmTime::now();
    let elapsed = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let clock_elapsed = elapsed.clone();
    let clock = Arc::new(move || {
        let ms = clock_elapsed.load(Ordering::SeqCst);
        ArmTime {
            unix_ms: base.unix_ms + ms,
            monotonic: base.monotonic + Duration::from_millis(ms as u64),
            suspend_aware_ms: base.suspend_aware_ms + ms,
        }
    });
    let h = super::pan115_worker_gate_tests::worker_harness_with_clock(
        pancetta_config::Config::default(),
        |_| {},
        clock,
    )
    .await;
    let (_, audio_rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap();
    {
        let mut arm = h.coordinator.remote_tx_arm.lock().unwrap();
        arm.set_channel_consent(ConsentChannel::StationAgent, true, base.unix_ms);
        arm.arm(
            VerifiedArmGrant {
                operator_callsign: "N0CALL".into(),
                ttl_ms: 900_000,
                armed_until_ms: base.unix_ms + 900_000,
                capability_exp_ms: base.unix_ms + 600_000,
                tx_enabled_until_ms: base.unix_ms + 600_000,
                effective_ttl_ms: 600_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan105-playback".into(),
                client_key_id: "client-a".into(),
            },
            base,
        );
    }
    let identity = (origin == TxOrigin::Remote).then(|| "client-a".to_string());
    let message = if bundle {
        MessageType::MultiTransmitRequest {
            items: [1500.0, 1700.0]
                .into_iter()
                .map(|frequency_offset| crate::message_bus::TransmitRequestItem {
                    attempt_id: None,
                    message_text: TEXT.into(),
                    frequency_offset,
                    qso_id: None,
                    origin,
                    remote_client_key_id: identity.clone(),
                })
                .collect(),
            tx_parity: None,
            origin,
            remote_client_key_id: identity,
        }
    } else {
        MessageType::TransmitRequest {
            attempt_id: None,
            message_text: TEXT.into(),
            frequency_offset: 1500.0,
            qso_id: None,
            tx_parity: None,
            origin,
            remote_client_key_id: identity,
        }
    };
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::StationAgent,
            ComponentId::Ft8Transmitter,
            message,
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "frame must key before authority expires"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if audio_rx.try_iter().any(|m| {
                matches!(m.message_type,
                MessageType::AudioOutput { samples, .. } if !samples.is_empty())
            }) {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("frame entered playback");
    (h, audio_rx, elapsed, base)
}

async fn pan105_expiry_interrupts_playback(bundle: bool) {
    let (h, audio_rx, elapsed, base) = pan105_playback(bundle, TxOrigin::Remote).await;
    elapsed.store(600_000, Ordering::SeqCst);
    let ack = flush_ack(&audio_rx).await;
    assert_eq!(ptt_offs(&h), 1, "PTT-off precedes flush acknowledgment");
    {
        let arm = h.coordinator.remote_tx_arm.lock().unwrap();
        assert!(
            arm.is_armed(),
            "the worker uses a pure permission check, not disarm()"
        );
        assert_eq!(
            arm.tx_permit_reason(
                ArmTime {
                    unix_ms: base.unix_ms + 600_000,
                    ..base
                },
                ConsentChannel::StationAgent
            ),
            pancetta_agent::arm::TxPermit::Denied(
                pancetta_agent::arm::DenyReason::CapabilityExpired
            )
        );
    }
    sleep(Duration::from_millis(150)).await;
    assert_eq!(completed(&h), None, "no completion before the flush ack");
    ack.complete(true);
    let expected_completions = if bundle { 2 } else { 1 };
    let mut failures = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while failures < expected_completions {
            for message in h.autonomous_rx.try_iter() {
                if let MessageType::TransmitComplete {
                    message_text,
                    success,
                    ..
                } = message.message_type
                {
                    assert_eq!(message_text, TEXT);
                    assert!(!success, "every expired bundle item must fail");
                    failures += 1;
                }
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("every interrupted item completes after flush acknowledgment");
    assert_eq!(failures, expected_completions);
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
    assert_eq!(ptt_offs(&h), 0, "no duplicate unkey after flush");
    assert!(
        !audio_rx
            .try_iter()
            .any(|m| matches!(m.message_type, MessageType::AudioOutput { .. })),
        "expired audio is not requeued after flush"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan105_capability_expiry_interrupts_active_single_tx() {
    pan105_expiry_interrupts_playback(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan105_capability_expiry_interrupts_active_summed_bundle() {
    pan105_expiry_interrupts_playback(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pan105_remote_expiry_does_not_interrupt_local_playback() {
    let (h, audio_rx, elapsed, _) = pan105_playback(false, TxOrigin::Local).await;
    elapsed.store(600_000, Ordering::SeqCst);
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                !audio_rx
                    .try_iter()
                    .any(|m| matches!(m.message_type, MessageType::AudioFlush { .. })),
                "remote arm expiry must not flush local audio"
            );
            if let Some(success) = completed(&h) {
                break success;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("local frame completes");
    assert!(result);
}

fn disarm(h: &Harness) {
    h.coordinator
        .remote_tx_arm
        .lock()
        .unwrap()
        .disarm(super::super::now_epoch_ms() as i64);
}

async fn flush_ack(rx: &Receiver<ComponentMessage>) -> DeliveryAck {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(m) = rx.try_recv() {
                if let MessageType::AudioFlush { ack, .. } = m.message_type {
                    return ack;
                }
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("mid-playback disarm must send a flush")
}

fn completed(h: &Harness) -> Option<bool> {
    h.autonomous_rx
        .try_iter()
        .find_map(|m| match m.message_type {
            MessageType::TransmitComplete {
                message_text,
                success,
                ..
            } if message_text == TEXT => Some(success),
            _ => None,
        })
}

// Unlike the shared await_completion helper, preserve the Hamlib messages
// so the tests can count every unkey, including one sent by Drop.
async fn await_failed_completion(h: &Harness) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(success) = completed(h) {
                assert!(!success, "revoked frame must be reported unsuccessful");
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("completion must follow the flush ack");
}

fn ptt_offs(h: &Harness) -> usize {
    h.hamlib_rx
        .try_iter()
        .filter(|m| {
            matches!(
                m.message_type,
                MessageType::RigControl(RigControlMessage::SetPtt { state: false })
            )
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_playback_disarm_unkeys_once_then_waits_for_the_flush_ack() {
    let (h, audio_rx) = remote_playback().await;
    disarm(&h);
    let ack = flush_ack(&audio_rx).await;
    assert_eq!(ptt_offs(&h), 1, "PTT-off precedes the flush");
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        completed(&h),
        None,
        "no completion while the flush ack is held"
    );
    ack.complete(true);
    await_failed_completion(&h).await;
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        ptt_offs(&h),
        0,
        "the stood-down guard must not duplicate the unkey"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// PAN-104: a failing audit sink never prevents or reorders the unkey;
/// the interrupt's audit record is attempted after PTT-off and its
/// failure is surfaced through the shared audit health.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_playback_disarm_unkeys_with_a_failing_audit_sink() {
    let (h, audio_rx) =
        remote_playback_with(worker_harness_with_config(failing_audit_config()).await).await;
    disarm(&h);
    let ack = flush_ack(&audio_rx).await;
    assert_eq!(ptt_offs(&h), 1, "PTT-off precedes the flush");
    ack.complete(true);
    await_failed_completion(&h).await;
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !h.coordinator.audit_log().health().unavailable {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the failed audit write is surfaced");
}

fn failing_audit_config() -> pancetta_config::Config {
    let mut cfg = pancetta_config::Config::default();
    cfg.network.station_agent.audit_log_path = Some(
        std::env::temp_dir()
            .join(format!("pancetta-pan104-missing-{}", uuid::Uuid::new_v4()))
            .join("audit.log")
            .to_string_lossy()
            .into_owned(),
    );
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_playback_disarm_with_hamlib_backlogged_still_unkeys() {
    let (h, audio_rx) = remote_playback().await;
    backlogged_disarm_still_unkeys(h, audio_rx).await;
}

/// PAN-104: the backlogged-Hamlib unkey and its flush-ack ordering hold
/// with a failing audit sink too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlogged_disarm_unkeys_with_a_failing_audit_sink() {
    let (h, audio_rx) =
        remote_playback_with(worker_harness_with_config(failing_audit_config()).await).await;
    backlogged_disarm_still_unkeys(h, audio_rx).await;
}

async fn backlogged_disarm_still_unkeys(h: Harness, audio_rx: Receiver<ComponentMessage>) {
    let bus = &h.coordinator.message_bus;
    let mut filled = 0;
    while bus
        .send_message_checked(ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::AudioData(Vec::new()),
            Instant::now(),
        ))
        .await
        .unwrap()
    {
        filled += 1;
    }
    assert_eq!(filled, 10_000, "exercise a completely full Hamlib channel");
    disarm(&h);
    let ack = flush_ack(&audio_rx).await;
    assert!(
        h.coordinator.ptt_active.load(Ordering::Acquire),
        "unkey not delivered: still keyed"
    );
    assert_eq!(completed(&h), None);
    assert_eq!(
        ptt_offs(&h),
        0,
        "only backlog existed; no unkey was delivered yet"
    );
    ack.complete(true);
    await_failed_completion(&h).await;
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        ptt_offs(&h),
        1,
        "the armed guard retries on Drop once Hamlib catches up"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}
