// PAN-120. Formerly the inline module `pan120_tests`.
use super::*;
use pancetta_config::UnattendedPolicy;
use pancetta_core::{slot::SlotParity, TxPolicy};
use pancetta_qso::{states::MessageType as Mt, QsoEvent, QsoManager, QsoManagerConfig};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
use std::sync::{Arc, RwLock};

const OUR: &str = "K5ARH";
const DX: &str = "W1ABC";

fn call(text: &str) -> Mt {
    pancetta_qso::utils::parse_ft8_message(text, OUR).unwrap()
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>) -> Vec<QsoEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return events,
            Err(error) => panic!("event subscription lost: {error}"),
        }
    }
}

fn sends(events: &[QsoEvent]) -> Vec<&Mt> {
    events
        .iter()
        .filter_map(|event| match event {
            QsoEvent::MessageToSend { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

#[derive(Clone)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Station {
    manager: QsoManager,
    bus: MessageBus,
    tx_policy: AtomicU8,
    fox_mode: AtomicBool,
    fox_cap: AtomicUsize,
    fox_admitted: RwLock<std::collections::HashSet<String>>,
}

impl Station {
    async fn new(fox: bool, policy: TxPolicy) -> Self {
        let manager = QsoManager::new(QsoManagerConfig {
            our_callsign: OUR.into(),
            our_grid: Some("EM10".into()),
            ..Default::default()
        });
        manager.start().await.unwrap();
        Self {
            manager,
            bus: MessageBus::new(1000).unwrap(),
            tx_policy: AtomicU8::new(policy.as_u8()),
            fox_mode: AtomicBool::new(fox),
            fox_cap: AtomicUsize::new(1),
            fox_admitted: RwLock::new(Default::default()),
        }
    }

    async fn answer(
        &self,
        message: &Mt,
        unattended: UnattendedPolicy,
        present: bool,
    ) -> Vec<serde_json::Value> {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = Capture(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        maybe_answer_caller(
            message,
            OUR,
            1500.0,
            Some(SlotParity::Even),
            -10.0,
            &self.manager,
            &self.tx_policy,
            unattended,
            present,
            1,
            &self.bus,
            &self.fox_mode,
            &self.fox_cap,
            &self.fox_admitted,
        )
        .await;
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// Answer `text` as a present operator, report the answer's 73 as aired
/// (PAN-280: the QSO logs only then) and return the id of the QSO the
/// answer completed.
async fn complete_by_answer(st: &Station, text: &str) -> pancetta_qso::QsoId {
    let mut rx = st.manager.subscribe();
    st.answer(&call(text), UnattendedPolicy::Off, true).await;
    let (qso_id, attempt_id) = drain(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            QsoEvent::MessageToSend {
                qso_id, attempt_id, ..
            } => Some((qso_id, attempt_id)),
            _ => None,
        })
        .expect("the RR73 answer opens a QSO and queues its 73");
    st.manager
        .confirm_completion_aired(
            qso_id,
            attempt_id,
            "",
            &pancetta_qso::UnsentAttemptPayload {
                frequency_hz: 1500.0,
                remote_binding: Some((false, None)),
            },
            chrono::Utc::now(),
        )
        .await;
    drain(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            QsoEvent::QsoCompleted { qso_id, .. } => Some(qso_id),
            _ => None,
        })
        .expect("the RR73 answer completes a QSO once its 73 airs")
}

/// PAN-275: an RR73 repeat 130 s after the contact (past the old 120 s
/// window, inside the 3 min auto-73 window) belongs to the auto-73, so
/// the answer-caller path opens no second QSO for it.
#[tokio::test]
async fn pan275_rr73_repeat_inside_the_auto_73_window_opens_no_caller_qso() {
    let st = Station::new(false, TxPolicy::Full).await;
    let id = complete_by_answer(&st, "K5ARH W1ABC RR73").await;
    assert!(
        st.manager
            .backdate_completion_for_test(id, chrono::Duration::seconds(130))
            .await
    );
    let mut rx = st.manager.subscribe();

    st.answer(&call("K5ARH W1ABC RR73"), UnattendedPolicy::Off, true)
        .await;

    let events = drain(&mut rx);
    assert!(
        events.is_empty(),
        "no caller QSO for an RR73 repeat: {events:?}"
    );
}

/// Control: only the close step got the longer window. A fresh grid call
/// from a station worked 130 s ago is a new exchange and is answered.
#[tokio::test]
async fn pan275_new_exchange_from_a_station_worked_130_s_ago_is_still_answered() {
    let st = Station::new(false, TxPolicy::Full).await;
    let id = complete_by_answer(&st, "K5ARH W1ABC RR73").await;
    assert!(
        st.manager
            .backdate_completion_for_test(id, chrono::Duration::seconds(130))
            .await
    );
    let mut rx = st.manager.subscribe();

    st.answer(&call("K5ARH W1ABC FN31"), UnattendedPolicy::Off, true)
        .await;

    let events = drain(&mut rx);
    assert_eq!(
        sends(&events).len(),
        1,
        "a new exchange is answered: {events:?}"
    );
}

#[test]
fn pan275_answer_caller_recent_window_covers_the_auto_73_window() {
    use pancetta_core::ResponseStep;
    assert_eq!(
        answer_caller_recent_window(ResponseStep::SeventyThree),
        AUTO_73_WINDOW.to_std().unwrap()
    );
    assert_eq!(ANSWER_CALLER_RECENT_QSO_WINDOW, Duration::from_secs(120));
    for step in [
        ResponseStep::Report,
        ResponseStep::ReportAck,
        ResponseStep::Rr73,
    ] {
        assert_eq!(
            answer_caller_recent_window(step),
            ANSWER_CALLER_RECENT_QSO_WINDOW,
            "{step:?}"
        );
    }
}

#[tokio::test]
async fn pan120_new_caller_policy_presence_and_reply_matrix() {
    for unattended in [UnattendedPolicy::Off, UnattendedPolicy::RespondOnly] {
        for present in [false, true] {
            for policy in [TxPolicy::Full, TxPolicy::RespondOnly, TxPolicy::Disabled] {
                for (text, reply) in [
                    ("K5ARH W1ABC FN31", "report"),
                    ("K5ARH W1ABC -12", "report_ack"),
                    ("K5ARH W1ABC R-12", "rr73"),
                    ("K5ARH W1ABC RR73", "73"),
                ] {
                    for fox in [false, true] {
                        let st = Station::new(fox, policy).await;
                        let (_, tui) = st.bus.create_channel(ComponentId::Tui).await.unwrap();
                        let mut rx = st.manager.subscribe();
                        st.answer(&call(text), unattended, present).await;
                        let events = drain(&mut rx);
                        let allowed = policy.allows_any_tx()
                            && (present || unattended == UnattendedPolicy::RespondOnly);
                        let exists = st
                            .manager
                            .has_active_or_recent_qso_with(DX, 1500.0, Duration::from_secs(120))
                            .await;
                        assert_eq!(
                            exists, allowed,
                            "{unattended:?} {present} {policy:?} {text} fox={fox}"
                        );
                        if !allowed {
                            assert!(
                                events.is_empty(),
                                "refusal emitted manager events: {events:?}"
                            );
                            assert!(st.fox_admitted.read().unwrap().is_empty());
                            assert!(!tui.try_iter().any(|m| matches!(m.message_type, MessageType::StatusUpdate { text: ref t, .. } if t.starts_with("Answering "))));
                        } else {
                            let outgoing = sends(&events);
                            assert_eq!(outgoing.len(), 1, "{events:?}");
                            assert!(
                                matches!(
                                    (reply, outgoing[0]),
                                    ("report", Mt::SignalReport { .. })
                                        | ("report_ack", Mt::ReportAck { .. })
                                        | ("rr73", Mt::FinalConfirmation { .. })
                                        | ("73", Mt::SeventyThree { .. })
                                ),
                                "wrong reply: {outgoing:?}"
                            );
                            assert_eq!(st.fox_admitted.read().unwrap().len(), usize::from(fox));
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn pan120_bypass_preserves_classification_parity_capacity_and_dedup() {
    for text in ["W2XYZ W1ABC FN31", "CQ W1ABC FN31", "K5ARH W1ABC 73"] {
        let st = Station::new(false, TxPolicy::Full).await;
        let mut rx = st.manager.subscribe();
        let logs = st
            .answer(&call(text), UnattendedPolicy::RespondOnly, false)
            .await;
        assert!(drain(&mut rx).is_empty());
        assert_eq!(st.manager.active_qso_count().await, 0);
        assert!(!logs
            .iter()
            .any(|v| v["fields"]["respond_only_admitted"] == 1));
    }
    let st = Station::new(false, TxPolicy::Full).await;
    let mut rx = st.manager.subscribe();
    st.answer(
        &Mt::NonStandard {
            text: "unsupported".into(),
        },
        UnattendedPolicy::RespondOnly,
        false,
    )
    .await;
    assert!(drain(&mut rx).is_empty());
    st.answer(
        &call("K5ARH EA8/G8BCG -12"),
        UnattendedPolicy::RespondOnly,
        false,
    )
    .await;
    assert!(matches!(sends(&drain(&mut rx))[0], Mt::ReportAck { .. }));

    for obstruction in [
        "parity",
        "normal_capacity",
        "fox_capacity",
        "active",
        "recent",
    ] {
        let st = Station::new(obstruction == "fox_capacity", TxPolicy::Full).await;
        match obstruction {
            "parity" => {
                st.manager
                    .respond_to_cq_manual("W2XYZ".into(), 1000.0, Some(SlotParity::Odd))
                    .await
                    .unwrap();
            }
            "normal_capacity" | "fox_capacity" => {
                st.answer(
                    &call("K5ARH W2XYZ FN31"),
                    UnattendedPolicy::RespondOnly,
                    false,
                )
                .await;
            }
            "active" => {
                st.answer(
                    &call("K5ARH W1ABC FN31"),
                    UnattendedPolicy::RespondOnly,
                    false,
                )
                .await;
            }
            "recent" => {
                st.answer(
                    &call("K5ARH W1ABC RR73"),
                    UnattendedPolicy::RespondOnly,
                    false,
                )
                .await;
            }
            _ => unreachable!(),
        }
        let count = st.manager.active_qso_count().await;
        let fox_set = st.fox_admitted.read().unwrap().clone();
        let mut rx = st.manager.subscribe();
        let logs = st
            .answer(
                &call("K5ARH W1ABC FN31"),
                UnattendedPolicy::RespondOnly,
                false,
            )
            .await;
        assert!(drain(&mut rx).is_empty(), "{obstruction} bypassed");
        assert_eq!(st.manager.active_qso_count().await, count);
        assert_eq!(*st.fox_admitted.read().unwrap(), fox_set);
        assert!(
            !logs
                .iter()
                .any(|v| v["fields"]["respond_only_admitted"] == 1),
            "{logs:?}"
        );
    }
}

#[tokio::test]
async fn pan120_caller_logs_report_actual_outcomes() {
    for (unattended, present, policy, message, counts) in [
        (
            UnattendedPolicy::Off,
            false,
            TxPolicy::Full,
            "Direct-call reply suppressed by operator-presence gate",
            (0, 1, 0),
        ),
        (
            UnattendedPolicy::Off,
            false,
            TxPolicy::Disabled,
            "Direct-call reply suppressed by TX policy",
            (1, 0, 0),
        ),
        (
            UnattendedPolicy::RespondOnly,
            false,
            TxPolicy::Disabled,
            "Direct-call reply suppressed by TX policy",
            (1, 0, 0),
        ),
        (
            UnattendedPolicy::RespondOnly,
            false,
            TxPolicy::Full,
            "Direct-call reply admitted without console presence",
            (0, 0, 1),
        ),
    ] {
        let st = Station::new(false, policy).await;
        let logs = st
            .answer(&call("K5ARH W1ABC FN31"), unattended, present)
            .await;
        let event = logs
            .iter()
            .find(|v| v["fields"]["message"] == message)
            .unwrap_or_else(|| panic!("missing {message}: {logs:?}"));
        assert_eq!(event["target"], "tx.policy");
        assert_eq!(event["level"], "INFO");
        let fields = &event["fields"];
        assert_eq!(fields["callsign"], DX);
        assert_eq!(fields["reply_step"], "Report");
        assert_eq!(
            fields["unattended_policy"],
            if unattended == UnattendedPolicy::Off {
                "off"
            } else {
                "respond_only"
            }
        );
        assert_eq!(fields["policy_dropped"], counts.0);
        assert_eq!(fields["presence_dropped"], counts.1);
        assert_eq!(fields["respond_only_admitted"], counts.2);
        if counts.2 == 1 {
            let id = fields["qso_id"]
                .as_str()
                .expect("admission identifies actual QSO");
            assert!(st
                .manager
                .get_qso(uuid::Uuid::parse_str(id).unwrap())
                .await
                .is_ok());
            assert_eq!(fields["operator_present"], false);
        }
        if policy == TxPolicy::Disabled {
            assert_eq!(fields["tx_policy"], "DISABLED");
        }
    }
    let st = Station::new(false, TxPolicy::Full).await;
    let logs = st
        .answer(
            &call("K5ARH W1ABC FN31"),
            UnattendedPolicy::RespondOnly,
            true,
        )
        .await;
    assert_eq!(st.manager.active_qso_count().await, 1);
    assert!(!logs
        .iter()
        .any(|v| v["fields"]["respond_only_admitted"] == 1));
}

#[tokio::test]
async fn pan120_existing_or_recent_qso_has_no_new_caller_refusal_log() {
    for existing in ["active", "recent"] {
        for policy in [TxPolicy::Full, TxPolicy::Disabled] {
            let st = Station::new(false, TxPolicy::Full).await;
            let opening = if existing == "active" {
                "K5ARH W1ABC FN31"
            } else {
                "K5ARH W1ABC RR73"
            };
            st.answer(&call(opening), UnattendedPolicy::Off, true).await;
            st.tx_policy.store(policy.as_u8(), Ordering::Relaxed);

            let mut rx = st.manager.subscribe();
            let logs = st
                .answer(&call("K5ARH W1ABC R-12"), UnattendedPolicy::Off, false)
                .await;
            assert!(drain(&mut rx).is_empty(), "{existing} {policy:?}");
            assert!(
                !logs.iter().any(|event| event["target"] == "tx.policy"),
                "existing={existing} policy={policy:?} logs={logs:?}"
            );
        }
    }
}

struct LoopHarness {
    coordinator: super::super::ApplicationCoordinator,
    manager: QsoManager,
    tui: crossbeam_channel::Receiver<ComponentMessage>,
    shutdown: Arc<AtomicBool>,
    _home: tempfile::TempDir,
}

impl LoopHarness {
    async fn new(unattended: Option<UnattendedPolicy>, enabled: bool, last_input_ms: u64) -> Self {
        let home = tempfile::tempdir().unwrap();
        let mut config = pancetta_config::Config::default();
        config.station.callsign = OUR.into();
        config.station.grid_square = "EM10".into();
        config.autonomous.enabled = enabled;
        if let Some(policy) = unattended {
            config.autonomous.unattended_policy = policy;
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut coordinator = super::super::ApplicationCoordinator::new(
            config,
            None,
            true,
            true,
            None,
            Some(home.path().join("replay")),
            None,
            1500.0,
            shutdown.clone(),
            Vec::new(),
            home.path().join("pancetta.toml"),
            true,
            None, // PAN-270: no session-home override
        )
        .await
        .unwrap();
        coordinator.pancetta_home_override = Some(home.path().to_path_buf());
        coordinator
            .last_operator_input_ms
            .store(last_input_ms, Ordering::Relaxed);
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Relaxed);
        let (_, tui) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .unwrap();
        // No transmitter, audio or rig component is started.
        coordinator.start_qso_component().await.unwrap();
        let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
        Self {
            coordinator,
            manager,
            tui,
            shutdown,
            _home: home,
        }
    }

    fn present(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.coordinator
            .last_operator_input_ms
            .store(now, Ordering::Relaxed);
    }

    async fn send(&self, message: MessageType) {
        self.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Decoder,
                ComponentId::Qso,
                message,
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    async fn decode(&self, text: &str, slot: u64, captured_dial: u64) {
        // Use the existing text-parser fallback, just like compound-call decodes.
        let mut decoded = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message::default(),
            -10.0,
            0.9,
            1500.0,
            0.0,
        );
        decoded.text = text.into();
        decoded.timestamp = std::time::UNIX_EPOCH + Duration::from_secs(slot * 15);
        decoded.slot_parity = Some(SlotParity::Even);
        decoded.captured_dial_hz = Some(captured_dial);
        self.send(MessageType::DecodedMessage(decoded)).await;
    }

    /// The false Fox echo is produced by this same FIFO consumer after all
    /// earlier decodes. Unlike a quiet-time sleep, it proves processing is done.
    /// These fixtures never enable Fox; the barrier also asserts no CQ exists.
    async fn barrier(&self) -> Vec<ComponentMessage> {
        assert!(!self
            .manager
            .get_active_qsos()
            .await
            .iter()
            .any(|(_, q)| matches!(q.state, pancetta_qso::QsoState::CallingCq { .. })));
        self.send(MessageType::QsoMessage(
            crate::message_bus::QsoMessage::SetFoxMode { on: false },
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut messages = Vec::new();
            loop {
                while let Ok(message) = self.tui.try_recv() {
                    let done = matches!(
                        message.message_type,
                        MessageType::FoxModeStatus { on: false }
                    );
                    messages.push(message);
                    if done {
                        return messages;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("QSO loop must acknowledge the barrier")
    }

    async fn wait_for_send(
        &self,
        rx: &mut tokio::sync::broadcast::Receiver<QsoEvent>,
    ) -> (uuid::Uuid, Mt) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let QsoEvent::MessageToSend {
                    qso_id, message, ..
                } = rx.recv().await.unwrap()
                {
                    return (qso_id, message);
                }
            }
        })
        .await
        .expect("expected reply from the real QSO loop")
    }
}

impl Drop for LoopHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn pan120_real_loop_default_and_off_read_live_presence_and_deduplicate_refusals() {
    for setting in [None, Some(UnattendedPolicy::Off)] {
        for last_input in [0, 1] {
            let h = LoopHarness::new(setting, false, last_input).await;
            let mut rx = h.manager.subscribe();
            h.decode("K5ARH W1ABC FN31", 100, 14_074_000).await;
            let messages = h.barrier().await;
            assert!(drain(&mut rx).is_empty());
            assert_eq!(h.manager.active_qso_count().await, 0);
            assert!(!messages.iter().any(|m| matches!(&m.message_type, MessageType::StatusUpdate { text: t, .. } if t.starts_with("Answering "))));
            h.present();
            // Refreshing presence cannot bypass the same-slot attempt dedup.
            h.decode("K5ARH W1ABC FN31", 100, 14_074_000).await;
            h.barrier().await;
            assert!(drain(&mut rx).is_empty());
            assert_eq!(h.manager.active_qso_count().await, 0);
            h.decode("K5ARH W1ABC FN31", 102, 14_074_000).await;
            let (id, reply) = h.wait_for_send(&mut rx).await;
            assert!(matches!(reply, Mt::SignalReport { .. }));
            h.decode("K5ARH W1ABC FN31", 102, 14_074_000).await;
            h.barrier().await;
            assert_eq!(h.manager.get_active_qsos().await.len(), 1);
            assert!(h.manager.get_qso(id).await.is_ok());
        }
    }
}

#[tokio::test]
async fn pan120_real_loop_respond_only_ignores_autonomous_toggle_but_not_stale_band() {
    for enabled in [false, true] {
        let h = LoopHarness::new(Some(UnattendedPolicy::RespondOnly), enabled, 0).await;
        let mut rx = h.manager.subscribe();
        h.decode("K5ARH W1ABC FN31", 100, 7_074_000).await;
        h.barrier().await;
        assert!(drain(&mut rx).is_empty());
        assert_eq!(h.manager.active_qso_count().await, 0);
        h.decode("K5ARH W1ABC FN31", 100, 14_074_000).await;
        let (id, reply) = h.wait_for_send(&mut rx).await;
        assert!(matches!(reply, Mt::SignalReport { .. }));
        h.decode("K5ARH W1ABC FN31", 100, 14_074_000).await;
        h.barrier().await;
        assert_eq!(h.manager.get_active_qsos().await.len(), 1);
        assert!(h.manager.get_qso(id).await.is_ok());
    }
}

#[tokio::test]
async fn pan120_real_loop_existing_exchange_and_cq_continue_after_presence_expires() {
    for started_with_cq in [false, true] {
        for setting in [UnattendedPolicy::Off, UnattendedPolicy::RespondOnly] {
            let h = LoopHarness::new(Some(setting), false, 0).await;
            h.present();
            let id = if started_with_cq {
                h.manager
                    .start_cq(1500.0, Some(SlotParity::Odd), false, None)
                    .await
                    .unwrap()
            } else {
                let mut opened = h.manager.subscribe();
                h.decode("K5ARH W1ABC FN31", 100, 14_074_000).await;
                h.wait_for_send(&mut opened).await.0
            };
            h.coordinator
                .last_operator_input_ms
                .store(0, Ordering::Relaxed);
            let mut rx = h.manager.subscribe();
            let incoming = if started_with_cq {
                "K5ARH W1ABC FN31"
            } else {
                "K5ARH W1ABC R-12"
            };
            // The QSO loop is a task on this current-thread runtime, so a
            // thread-scoped subscriber sees its caller-admission records.
            let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = Capture(bytes.clone());
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish();
            let guard = tracing::subscriber::set_default(subscriber);
            h.decode(incoming, 102, 14_074_000).await;
            let (continued_id, reply) = h.wait_for_send(&mut rx).await;
            h.barrier().await;
            drop(guard);
            let logs: Vec<serde_json::Value> = String::from_utf8(bytes.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(
                logs.iter().any(|event| event["fields"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("active or recently-completed QSO exists"))),
                "loop must reach caller admission for the continued QSO: {logs:?}"
            );
            assert!(
                !logs.iter().any(|event| event["fields"]["message"]
                    .as_str()
                    .is_some_and(|m| m.starts_with("Direct-call reply"))),
                "continued QSO {setting:?} cq={started_with_cq} logged as a caller outcome: {logs:?}"
            );
            assert_eq!(continued_id, id, "existing QSO must be reused");
            if started_with_cq {
                assert!(
                    matches!(reply, Mt::SignalReport { .. }),
                    "must answer, never reissue CQ: {reply:?}"
                );
            } else {
                assert!(
                    matches!(reply, Mt::FinalConfirmation { .. }),
                    "must advance existing exchange: {reply:?}"
                );
            }
            assert_eq!(h.manager.active_qso_count().await, 1);
        }
    }
}
