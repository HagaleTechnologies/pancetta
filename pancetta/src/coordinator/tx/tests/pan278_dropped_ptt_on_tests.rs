use super::pan115_worker_gate_tests::{worker_harness, worker_harness_without_rig, Harness};
use super::pan122_tx_log_worker_tests::fill_channel;
use crate::message_bus::{
    ComponentId, ComponentMessage, MessageType, QsoMessage, RigControlMessage, TransmitRequestItem,
    TxOrigin,
};
use crossbeam_channel::Receiver;
use pancetta_qso::TxAttemptId;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const Q1_TEXT: &str = "K1ABC N0CALL FN20";
const QA_TEXT: &str = "SM2LIY K5ARH R-14";
const QB_TEXT: &str = "C6AVD K5ARH EM10";

#[derive(Default)]
struct Seen {
    /// `(message_text, success)` per `TransmitComplete` on Autonomous.
    completions: Vec<(String, bool)>,
    /// `(qso_id, attempt_id)` per `TxFrameNotSent` on Qso.
    not_sent: Vec<(String, Option<TxAttemptId>)>,
    /// `qso_id` per `TxFrameSent` on Qso.
    sent: Vec<String>,
    /// `(target, text)` per `DiagnosticEvent` on the TUI.
    diagnostics: Vec<(&'static str, String)>,
    audio_msgs: usize,
}

impl Seen {
    fn drain(&mut self, h: &Harness, audio_rx: &Receiver<ComponentMessage>) {
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                self.completions.push((message_text, success));
            }
        }
        while let Ok(msg) = h.qso_rx.try_recv() {
            match msg.message_type {
                MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                    qso_id, attempt_id, ..
                }) => self.not_sent.push((qso_id, attempt_id)),
                MessageType::QsoMessage(QsoMessage::TxFrameSent { qso_id, .. }) => {
                    self.sent.push(qso_id)
                }
                _ => {}
            }
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { target, text, .. } = msg.message_type {
                self.diagnostics.push((target, text));
            }
        }
        while let Ok(msg) = audio_rx.try_recv() {
            if matches!(msg.message_type, MessageType::AudioOutput { .. }) {
                self.audio_msgs += 1;
            }
        }
    }

    fn completed(&self, text: &str) -> bool {
        self.completions.iter().any(|(t, _)| t == text)
    }

    fn ptt_drop_diagnostics(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|(target, text)| *target == "tx.ptt" && text.contains("PTT-on dropped"))
            .count()
    }
}

/// Drain every receiver except Hamlib's until each of `texts` has a
/// `TransmitComplete` (or `within` elapses), then once more for anything
/// still in flight.
async fn collect(
    h: &Harness,
    audio_rx: &Receiver<ComponentMessage>,
    texts: &[&str],
    within: Duration,
) -> Seen {
    let mut seen = Seen::default();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline && !texts.iter().all(|t| seen.completed(t)) {
        seen.drain(h, audio_rx);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.drain(h, audio_rx);
    seen
}

/// `SetPtt` messages in Hamlib's backlog, read after the outcome settled.
fn hamlib_set_ptt_count(h: &Harness) -> usize {
    let mut n = 0;
    while let Ok(msg) = h.hamlib_rx.try_recv() {
        if matches!(
            msg.message_type,
            MessageType::RigControl(RigControlMessage::SetPtt { .. })
        ) {
            n += 1;
        }
    }
    n
}

async fn audio_rx(h: &Harness) -> Receiver<ComponentMessage> {
    h.coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1
}

/// `q1` TX-live and one `TransmitRequest` for it carrying attempt 7.
async fn send_q1_request(h: &Harness) {
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("q1"));
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: Some(TxAttemptId(7)),
                message_text: Q1_TEXT.to_string(),
                frequency_offset: 1500.0,
                qso_id: Some("q1".into()),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_ptt_on_settles_a_single_frame_as_not_sent() {
    let h = worker_harness().await;
    assert!(h.coordinator.message_bus.has_rig_route());
    let audio_rx = audio_rx(&h).await;
    assert!(fill_channel(&h, ComponentId::Hamlib).await > 0);
    send_q1_request(&h).await;

    let seen = collect(&h, &audio_rx, &[Q1_TEXT], Duration::from_secs(45)).await;

    assert_eq!(
        seen.completions,
        vec![(Q1_TEXT.to_string(), false)],
        "a frame whose PTT-on was dropped must complete once, as failed"
    );
    assert_eq!(
        seen.not_sent,
        vec![("q1".to_string(), Some(TxAttemptId(7)))],
        "the QSO engine gets exactly one TxFrameNotSent for that attempt"
    );
    assert!(
        seen.sent.is_empty(),
        "no TxFrameSent for a frame that never keyed: {:?}",
        seen.sent
    );
    assert_eq!(seen.audio_msgs, 0, "no audio for a frame that never keyed");
    assert_eq!(
        hamlib_set_ptt_count(&h),
        0,
        "no SetPtt reached Hamlib: the PTT-on was dropped and no PTT-off follows"
    );
    assert_eq!(
        seen.ptt_drop_diagnostics(),
        1,
        "one tx.ptt diagnostic names the drop: {:?}",
        seen.diagnostics
    );
    assert!(
        !h.coordinator.ptt_active.load(Ordering::Acquire),
        "the in-process PTT flag is released"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_ptt_on_settles_every_bundle_item_as_not_sent() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    {
        let mut live = h.coordinator.active_tx_qsos.write().unwrap();
        live.insert(super::super::active_tx_qso_key("qa"));
        live.insert(super::super::active_tx_qso_key("qb"));
    }
    let audio_rx = audio_rx(&h).await;
    assert!(fill_channel(&h, ComponentId::Hamlib).await > 0);
    let item = |text: &str, offset: f64, qso: &str| TransmitRequestItem {
        attempt_id: None,
        message_text: text.to_string(),
        frequency_offset: offset,
        qso_id: Some(qso.to_string()),
        origin: TxOrigin::Local,
        remote_client_key_id: None,
    };
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![item(QA_TEXT, 1000.0, "qa"), item(QB_TEXT, 1600.0, "qb")],
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();

    let seen = collect(&h, &audio_rx, &[QA_TEXT, QB_TEXT], Duration::from_secs(45)).await;

    let mut completions = seen.completions.clone();
    completions.sort();
    assert_eq!(
        completions,
        vec![(QB_TEXT.to_string(), false), (QA_TEXT.to_string(), false)],
        "each bundle item completes once, as failed"
    );
    let mut not_sent: Vec<&str> = seen.not_sent.iter().map(|(q, _)| q.as_str()).collect();
    not_sent.sort();
    assert_eq!(
        not_sent,
        vec!["qa", "qb"],
        "each item's QSO gets one TxFrameNotSent"
    );
    assert!(
        seen.sent.is_empty(),
        "no TxFrameSent for a bundle that never keyed: {:?}",
        seen.sent
    );
    assert_eq!(seen.audio_msgs, 0, "no audio for a bundle that never keyed");
    assert_eq!(hamlib_set_ptt_count(&h), 0, "no SetPtt reached Hamlib");
    assert_eq!(
        seen.ptt_drop_diagnostics(),
        1,
        "one tx.ptt diagnostic names the dropped bundle: {:?}",
        seen.diagnostics
    );
}

/// Decision 1: with no Hamlib route every PTT-on is `Dropped` by design,
/// and the frame still plays and reports as aired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rig_route_keeps_the_proceed_path() {
    let h = worker_harness_without_rig().await;
    assert!(
        !h.coordinator.message_bus.has_rig_route(),
        "the harness must register no Hamlib channel"
    );
    let audio_rx = audio_rx(&h).await;
    send_q1_request(&h).await;

    let seen = collect(&h, &audio_rx, &[Q1_TEXT], Duration::from_secs(45)).await;

    assert_eq!(
        seen.completions,
        vec![(Q1_TEXT.to_string(), true)],
        "without a rig route the frame plays and completes as aired"
    );
    assert_eq!(seen.sent, vec!["q1".to_string()], "one TxFrameSent");
    assert!(seen.not_sent.is_empty(), "no TxFrameNotSent");
    assert_eq!(seen.audio_msgs, 1, "the frame's audio is enqueued");
    assert_eq!(
        seen.ptt_drop_diagnostics(),
        0,
        "the no-route path raises no PTT-drop diagnostic"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_tune_ptt_on_abandons_the_tune() {
    let h = worker_harness().await;
    h.coordinator
        .operating_frequency_hz
        .store(14_074_000, Ordering::Relaxed);
    let audio_rx = audio_rx(&h).await;
    assert!(fill_channel(&h, ComponentId::Hamlib).await > 0);
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Ft8Transmitter,
            MessageType::TuneRequest {
                duration_secs: 5,
                tone_offset_hz: 1500.0,
            },
            Instant::now(),
        ))
        .await
        .unwrap();

    let mut seen = Seen::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && !seen
            .diagnostics
            .iter()
            .any(|(target, text)| *target == "tx.ptt" && text.contains("tune abandoned"))
    {
        seen.drain(&h, &audio_rx);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.drain(&h, &audio_rx);

    assert!(
        seen.diagnostics
            .iter()
            .any(|(target, text)| *target == "tx.ptt"
                && text == "Tune: PTT-on dropped (rig not keyed); tune abandoned"),
        "a tune whose PTT-on was dropped is abandoned with a diagnostic: {:?}",
        seen.diagnostics
    );
    assert_eq!(seen.audio_msgs, 0, "no tune tone without a keyed rig");
    assert_eq!(hamlib_set_ptt_count(&h), 0, "no SetPtt reached Hamlib");
    assert!(
        !h.coordinator.ptt_active.load(Ordering::Acquire),
        "the in-process PTT flag is released"
    );
}
