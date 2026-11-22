// PAN-136. Formerly the inline module `pan136_bundle_binding_worker_tests`.
use super::worker_gate_tests::{worker_harness, Harness};
use crate::message_bus::{
    ComponentId, ComponentMessage, MessageType, QsoMessage, RigControlMessage, TransmitRequestItem,
    TxOrigin,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const QA_TEXT: &str = "SM2LIY K5ARH R-14";
const QB_TEXT: &str = "C6AVD K5ARH EM10";

/// Dial on 20m, `qa` and `qb` live, and (optionally) a fresher intent for
/// `qa` at its own 1000 Hz offset carrying `(text, origin, client key)`.
/// Returns the intent's producing `TransmitRequest`, which [`collect`]
/// enqueues behind the bundle as the QSO forwarder does in production
/// (PAN-185: a refused pivot leaves that request to report the outcome).
fn setup(h: &Harness, intent: Option<(&str, TxOrigin, Option<&str>)>) -> Option<MessageType> {
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    {
        let mut live = h.coordinator.active_tx_qsos.write().unwrap();
        live.insert(super::super::active_tx_qso_key("qa"));
        live.insert(super::super::active_tx_qso_key("qb"));
    }
    if let Some((text, origin, key)) = intent {
        h.coordinator.latest_tx_intent.write().unwrap().insert(
            super::super::active_tx_qso_key("qa"),
            super::super::LatestTxIntent {
                attempt_id: None,
                message_text: text.to_string(),
                frequency_offset: 1000.0,
                tx_parity: None,
                origin,
                remote_client_key_id: key.map(str::to_string),
                request_queued: true,
            },
        );
    }
    intent.map(|(text, origin, key)| MessageType::TransmitRequest {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: 1000.0,
        qso_id: Some("qa".to_string()),
        tx_parity: None,
        origin,
        remote_client_key_id: key.map(str::to_string),
    })
}

/// A two-item bundle whose items carry the bundle's own binding, sent
/// straight to the worker (no coalescing), so the pivot fires at Step
/// 4b-pivot after the slot wait.
async fn send_bundle(h: &Harness, origin: TxOrigin, key: Option<&str>) {
    let item = |text: &str, offset: f64, qso: &str| TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: offset,
        qso_id: Some(qso.to_string()),
        origin,
        remote_client_key_id: key.map(str::to_string),
    };
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![item(QA_TEXT, 1000.0, "qa"), item(QB_TEXT, 1600.0, "qb")],
                tx_parity: None,
                origin,
                remote_client_key_id: key.map(str::to_string),
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

struct Collected {
    completions: Vec<Option<bool>>,
    ptt_on: bool,
    diagnostics: Vec<String>,
    frames_not_sent: Vec<QsoMessage>,
}

/// Drain every receiver until a `TransmitComplete` has arrived for each
/// of `texts` (or `within` elapses). `producing` is enqueued once every
/// other text has completed, i.e. after the bundle was handled. A second
/// completion for the same text fails the test (PAN-185 exactly-once).
async fn collect(
    h: &Harness,
    texts: &[&str],
    within: Duration,
    mut producing: Option<MessageType>,
) -> Collected {
    let producing_slot = producing.as_ref().and_then(|m| match m {
        MessageType::TransmitRequest { message_text, .. } => {
            texts.iter().position(|t| t == message_text)
        }
        _ => None,
    });
    let mut c = Collected {
        completions: vec![None; texts.len()],
        ptt_on: false,
        diagnostics: Vec::new(),
        frames_not_sent: Vec::new(),
    };
    let deadline = Instant::now() + within;
    let drain = |c: &mut Collected| {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            c.ptt_on |= matches!(
                msg.message_type,
                MessageType::RigControl(RigControlMessage::SetPtt { state: true })
            );
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                c.diagnostics.push(text);
            }
        }
        while let Ok(msg) = h.qso_rx.try_recv() {
            if let MessageType::QsoMessage(q @ QsoMessage::TxFrameNotSent { .. }) = msg.message_type
            {
                c.frames_not_sent.push(q);
            }
        }
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if let Some(slot) = texts.iter().position(|t| *t == message_text) {
                    assert!(
                        c.completions[slot].is_none(),
                        "a second completion for '{message_text}'"
                    );
                    c.completions[slot] = Some(success);
                }
            }
        }
    };
    let send = |message_type: MessageType| {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                message_type,
                Instant::now(),
            ))
    };
    while Instant::now() < deadline
        && (producing.is_some() || c.completions.iter().any(|d| d.is_none()))
    {
        drain(&mut c);
        let others_done = c
            .completions
            .iter()
            .enumerate()
            .all(|(slot, d)| Some(slot) == producing_slot || d.is_some());
        if others_done {
            if let Some(message_type) = producing.take() {
                send(message_type).await.unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Anything the worker emitted alongside the last completion, and
    // time for a duplicate to show up.
    tokio::time::sleep(Duration::from_secs(1)).await;
    drain(&mut c);
    c
}

/// Arm `client_key` over the station-agent channel, as a verified
/// `txArm` would, and keep the dead-man window open with a heartbeat
/// every 3 s (`HEARTBEAT_TIMEOUT_MS` is 30 s; a bundle waits up to one
/// slot pair). Abort the returned task when the test ends.
fn arm_client(h: &Harness, client_key: &str) -> tokio::task::JoinHandle<()> {
    use pancetta_agent::arm::{ArmTime, ConsentChannel, VerifiedArmGrant};
    let jti = format!("pan136-{client_key}");
    let now_ms = pancetta_core::time::UtcTimestamp::now().as_millisecond();
    {
        let mut arm = h.coordinator.remote_tx_arm.lock().unwrap();
        arm.set_channel_consent(ConsentChannel::StationAgent, true, now_ms);
        arm.arm(
            VerifiedArmGrant {
                operator_callsign: "K5ARH".to_string(),
                ttl_ms: 120_000,
                armed_until_ms: now_ms + 120_000,
                capability_exp_ms: now_ms + 120_000 + 3_600_000,
                tx_enabled_until_ms: now_ms + 120_000 + 3_600_000,
                effective_ttl_ms: 120_000,
                heartbeat_interval_sec: 5,
                scope_tx: true,
                jti: jti.clone(),
                client_key_id: client_key.to_string(),
            },
            ArmTime::now(),
        );
    }
    let arm = h.coordinator.remote_tx_arm.clone();
    tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            seq += 1;
            arm.lock().unwrap().heartbeat(&jti, seq, ArmTime::now());
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_bundle_item_rebound_to_unarmed_client_is_denied() {
    let h = worker_harness().await;
    let producing = setup(&h, Some((QA_TEXT, TxOrigin::Remote, Some("client-B"))));
    send_bundle(&h, TxOrigin::Local, None).await;
    let c = collect(&h, &[QA_TEXT, QB_TEXT], Duration::from_secs(90), producing).await;
    assert!(
        !c.ptt_on,
        "a bundle carrying a frame re-bound to an unarmed client must not key"
    );
    assert_eq!(c.completions, vec![Some(false), Some(false)]);
    assert!(
        c.diagnostics.iter().any(|d| {
            d.contains(
            "Remote TX denied (arm went stale during slot wait: 'SM2LIY K5ARH R-14' at 1000 Hz)"
        )
        }),
        "the re-folded bundle must be denied by the key-time arm gate: {:?}",
        c.diagnostics
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_bundle_item_pivoting_to_unarmed_remote_text_is_denied() {
    let h = worker_harness().await;
    let producing = setup(
        &h,
        Some(("SM2LIY K5ARH 73", TxOrigin::Remote, Some("client-B"))),
    );
    send_bundle(&h, TxOrigin::Local, None).await;
    let c = collect(
        &h,
        &["SM2LIY K5ARH 73", QB_TEXT],
        Duration::from_secs(90),
        producing,
    )
    .await;
    assert!(
        !c.ptt_on,
        "a bundle whose item pivoted to an unarmed client's text must not key"
    );
    assert_eq!(c.completions, vec![Some(false), Some(false)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_clients_bundle_holds_item_rebound_to_another_client() {
    let h = worker_harness().await;
    let heartbeat = arm_client(&h, "client-A");
    let producing = setup(&h, Some((QA_TEXT, TxOrigin::Remote, Some("client-B"))));
    send_bundle(&h, TxOrigin::Remote, Some("client-A")).await;
    let c = collect(&h, &[QA_TEXT, QB_TEXT], Duration::from_secs(90), producing).await;
    heartbeat.abort();
    assert!(c.ptt_on, "client A's own frame must still key");
    assert_eq!(
        c.completions,
        vec![Some(false), Some(true)],
        "client B's re-bound frame is held; client A's sibling airs"
    );
    assert!(
        c.diagnostics.iter().any(|d| d
            == "holding multi-TX item: its remote binding changed during the pre-PTT wait and conflicts with the rest of the bundle: 'SM2LIY K5ARH R-14'"),
        "the held item must be named: {:?}",
        c.diagnostics
    );
    assert!(
        !c.diagnostics.iter().any(|d| d.contains("for ended QSO")),
        "a held item must not be mislabelled as an ended QSO: {:?}",
        c.diagnostics
    );
    let replaced = c.frames_not_sent.iter().find_map(|q| match q {
        QsoMessage::TxFrameNotSent {
            attempt_id: _,
            qso_id,
            message_text,
            replaced: Some(p),
        } if qso_id == "qa" && message_text == QA_TEXT => Some(p.clone()),
        _ => None,
    });
    let replaced = replaced.unwrap_or_else(|| {
        panic!(
            "qa's replaced frame must be rolled back: {:?}",
            c.frames_not_sent
        )
    });
    assert_eq!(replaced.frequency_hz, 1000.0);
    assert_eq!(
        replaced.remote_binding,
        Some((true, Some("client-A".to_string()))),
        "the replaced frame carries its own pre-pivot binding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_bundle_item_rebound_to_armed_client_keys_under_it() {
    let h = worker_harness().await;
    let heartbeat = arm_client(&h, "client-B");
    let producing = setup(&h, Some((QA_TEXT, TxOrigin::Remote, Some("client-B"))));
    send_bundle(&h, TxOrigin::Local, None).await;
    let c = collect(&h, &[QA_TEXT, QB_TEXT], Duration::from_secs(90), producing).await;
    heartbeat.abort();
    assert!(
        c.ptt_on,
        "the re-folded bundle passes client B's gate: {:?}",
        c.diagnostics
    );
    assert_eq!(c.completions, vec![Some(true), Some(true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_bundle_without_pivot_is_still_denied_unarmed() {
    let h = worker_harness().await;
    let producing = setup(&h, None);
    send_bundle(&h, TxOrigin::Remote, Some("client-B")).await;
    let c = collect(&h, &[QA_TEXT, QB_TEXT], Duration::from_secs(50), producing).await;
    assert!(!c.ptt_on, "an unarmed Remote bundle must not key");
    assert!(
        c.diagnostics
            .iter()
            .any(|d| d
                .contains("Remote TX denied (not armed/permitted): multi-TX bundle of 2 items")),
        "{:?}",
        c.diagnostics
    );
}
