// PAN-115. Formerly the inline module `pan115_worker_gate_tests`.
use super::super::hamlib::RigConnState;
use super::super::ApplicationCoordinator;
use crate::message_bus::{ComponentId, ComponentMessage, MessageType, RigControlMessage, TxOrigin};
use crossbeam_channel::Receiver;
use pancetta_config::Config;
use pancetta_core::time::SignedDurationExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// `pub(crate)` fields/helpers: shared with the sibling PAN-118 worker-gate
// test module (`worker_band_plan_tests`), which reuses this harness
// exactly as the PAN-115 plan's shape intends, and with the PAN-136
// bundle-binding module (`bundle_binding_worker_tests`).
pub(crate) struct Harness {
    pub(super) utc_clock: pancetta_core::time::UtcClock,
    pub(crate) coordinator: ApplicationCoordinator,
    shutdown: Arc<AtomicBool>,
    pub(crate) hamlib_rx: Receiver<ComponentMessage>,
    pub(crate) autonomous_rx: Receiver<ComponentMessage>,
    pub(crate) tui_rx: Receiver<ComponentMessage>,
    pub(crate) qso_rx: Receiver<ComponentMessage>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

pub(crate) async fn worker_harness() -> Harness {
    worker_harness_with_config(Config::default()).await
}

/// [`worker_harness`] over a caller-built `Config` — the worker snapshots
/// `rig.frequency.band_plan` at spawn, so a custom plan must be in the
/// config the coordinator is built from (PAN-208).
pub(crate) async fn worker_harness_with_config(cfg: Config) -> Harness {
    worker_harness_with(cfg, |_| {}).await
}

/// [`worker_harness_with_config`] with a `prepare` hook run on the
/// coordinator after `pancetta_home_override` is set and before
/// `start_transmitter_component` (PAN-122: inject a gated TX log).
pub(crate) async fn worker_harness_with(
    cfg: Config,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
) -> Harness {
    worker_harness_with_clock(cfg, prepare, Arc::new(pancetta_agent::arm::ArmTime::now)).await
}

pub(super) async fn worker_harness_with_clock(
    cfg: Config,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
    arm_clock: Arc<dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync>,
) -> Harness {
    build_worker_harness(
        cfg,
        prepare,
        arm_clock,
        pancetta_core::time::system_utc_clock(),
        true,
    )
    .await
}

/// [`worker_harness`] with no Hamlib channel registered, as in
/// `--replay`: the bus has no rig route, so every PTT-on comes back `Dropped` by design (PAN-278).
/// `hamlib_rx` is a detached receiver that never sees a message.
pub(crate) async fn worker_harness_without_rig() -> Harness {
    build_worker_harness(
        Config::default(),
        |_| {},
        Arc::new(pancetta_agent::arm::ArmTime::now),
        pancetta_core::time::system_utc_clock(),
        false,
    )
    .await
}

pub(super) async fn build_worker_harness(
    cfg: Config,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
    arm_clock: Arc<dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync>,
    utc_clock: pancetta_core::time::UtcClock,
    register_rig: bool,
) -> Harness {
    build_worker_harness_in_home(cfg, prepare, arm_clock, utc_clock, register_rig, None, None).await
}

/// PAN-483: [`worker_harness`] whose worker reads `slot_clock`, so a
/// detecting clock can land a resync on it.
pub(super) async fn worker_harness_with_slot_clock(
    slot_clock: pancetta_core::SlotClock,
) -> Harness {
    let utc_clock: pancetta_core::time::UtcClock = {
        let clock = slot_clock.clone();
        Arc::new(move || clock.now())
    };
    build_worker_harness_in_home(
        Config::default(),
        |_| {},
        Arc::new(pancetta_agent::arm::ArmTime::now),
        utc_clock,
        true,
        None,
        Some(slot_clock),
    )
    .await
}

/// PAN-439: [`worker_harness_with`] whose coordinator starts from the
/// session home `home`, so a restart reads that home's saved session.
pub(crate) async fn worker_harness_in_home(
    cfg: Config,
    home: std::path::PathBuf,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
) -> Harness {
    build_worker_harness_in_home(
        cfg,
        prepare,
        Arc::new(pancetta_agent::arm::ArmTime::now),
        pancetta_core::time::system_utc_clock(),
        true,
        Some(home),
        None,
    )
    .await
}

async fn build_worker_harness_in_home(
    mut cfg: Config,
    prepare: impl FnOnce(&mut ApplicationCoordinator),
    arm_clock: Arc<dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync>,
    utc_clock: pancetta_core::time::UtcClock,
    register_rig: bool,
    home: Option<std::path::PathBuf>,
    slot_clock: Option<pancetta_core::SlotClock>,
) -> Harness {
    // This TX-only fixture historically sends CQ requests as Autonomous.
    // Grant their config permission so each test reaches the gate it targets;
    // permission-revocation tests explicitly close the live gate afterwards.
    cfg.autonomous.enabled = true;
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut coordinator = ApplicationCoordinator::new(
        cfg,
        None,
        true, // no_audio
        true, // headless
        None, // no WAV
        None, // no replay
        None, // no test-tx
        1500.0,
        shutdown.clone(),
        Vec::new(),
        std::env::temp_dir().join(format!(
            "pancetta-pan115-config-{}.toml",
            uuid::Uuid::new_v4()
        )),
        true,
        home.clone(), // PAN-270: session home (None: no override)
    )
    .await
    .expect("coordinator creation should succeed");
    coordinator.pancetta_home_override = Some(home.unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "pancetta-pan115-home-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }));
    prepare(&mut coordinator);
    // As in a live session once the Hamlib message loop is consuming.
    coordinator
        .hamlib_command_loop_ready
        .store(true, Ordering::Release);
    let bus = coordinator.message_bus.clone();
    let hamlib_rx = if register_rig {
        bus.get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap()
            .1
    } else {
        crossbeam_channel::bounded(1).1
    };
    let (_, autonomous_rx) = bus
        .get_or_create_channel(ComponentId::Autonomous)
        .await
        .unwrap();
    let (_, qso_rx) = bus.get_or_create_channel(ComponentId::Qso).await.unwrap();
    let (_, tui_rx) = bus.get_or_create_channel(ComponentId::Tui).await.unwrap();
    // PAN-134 logs audio start only after an actual enqueue. The worker
    // fixture needs an Audio destination even though it uses no device.
    bus.get_or_create_channel(ComponentId::Audio).await.unwrap();
    coordinator
        .start_transmitter_component_with_clock(
            arm_clock,
            slot_clock.unwrap_or_else(|| {
                pancetta_core::SlotClock::without_step_detection(
                    utc_clock.clone(),
                    coordinator.active_slot_ns(),
                )
            }),
        )
        .await
        .expect("TX worker starts");
    Harness {
        utc_clock,
        coordinator,
        shutdown,
        hamlib_rx,
        autonomous_rx,
        tui_rx,
        qso_rx,
    }
}

pub(crate) async fn request(h: &Harness, text: &str) {
    let msg = ComponentMessage::new(
        ComponentId::Autonomous,
        ComponentId::Ft8Transmitter,
        MessageType::TransmitRequest {
            attempt_id: None,
            message_text: text.to_string(),
            frequency_offset: 1500.0,
            qso_id: None,
            tx_parity: None,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        },
        Instant::now(),
    );
    h.coordinator.message_bus.send_message(msg).await.unwrap();
}

fn is_ptt_on(msg: &ComponentMessage) -> bool {
    matches!(
        msg.message_type,
        MessageType::RigControl(RigControlMessage::SetPtt { state: true })
    )
}

/// Drains all three receivers until `TransmitComplete` for `text` arrives
/// on Autonomous (or `within` elapses). Returns that completion's
/// `success`, every TUI diagnostic text seen, and whether any
/// `SetPtt{true}` reached Hamlib.
pub(crate) async fn await_completion(
    h: &Harness,
    text: &str,
    within: Duration,
) -> (Option<bool>, Vec<String>, bool) {
    let deadline = Instant::now() + within;
    let mut diagnostics = Vec::new();
    let mut ptt_on = false;
    let mut completed = None;
    while Instant::now() < deadline && completed.is_none() {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            ptt_on |= is_ptt_on(&msg);
        }
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                diagnostics.push(text);
            }
        }
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                if message_text == text {
                    completed = Some(success);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Anything the worker emitted alongside the completion.
    while let Ok(msg) = h.hamlib_rx.try_recv() {
        ptt_on |= is_ptt_on(&msg);
    }
    while let Ok(msg) = h.tui_rx.try_recv() {
        if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
            diagnostics.push(text);
        }
    }
    (completed, diagnostics, ptt_on)
}

pub(crate) async fn await_ptt_on(h: &Harness, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            if is_ptt_on(&msg) {
                return true;
            }
        }
        while h.tui_rx.try_recv().is_ok() {}
        while h.autonomous_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[test]
fn relocation_epoch_stale_is_per_qso_and_skips_none_ids() {
    let epochs = pancetta_qso::qso_manager::TxRelocationEpochs::default();
    let snapshot = vec![(Some("qa".to_string()), 0), (None, 0)];
    assert!(!super::relocation_epoch_stale(&epochs, &snapshot));
    epochs.bump("qb");
    epochs.bump("");
    assert!(!super::relocation_epoch_stale(&epochs, &snapshot));
    epochs.bump("qa");
    assert!(super::relocation_epoch_stale(&epochs, &snapshot));
    let current = vec![(Some("qa".to_string()), epochs.epoch("qa"))];
    assert!(!super::relocation_epoch_stale(&epochs, &current));
    epochs.bump("qa");
    assert!(super::relocation_epoch_stale(&epochs, &current));
}

async fn relocation_manager(h: &Harness) -> (pancetta_qso::QsoManager, pancetta_qso::QsoId) {
    let mut manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
        our_callsign: "W5AU".into(),
        our_grid: Some("FN20".into()),
        ..Default::default()
    });
    manager.set_ptt_sync_gate_source(h.coordinator.ptt_sync_gate.clone());
    manager.set_ptt_active_source(h.coordinator.ptt_active.clone());
    manager.set_relocation_epoch_source(h.coordinator.relocation_epochs.clone());
    manager.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    let id = manager.start_cq(1500.0, None, false, None).await.unwrap();
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&id.to_string()));
    (manager, id)
}

fn mirror_intent(h: &Harness, id: &str, text: &str, frequency_offset: f64) {
    h.coordinator.latest_tx_intent.write().unwrap().insert(
        super::super::active_tx_qso_key(id),
        super::super::LatestTxIntent {
            attempt_id: None,
            message_text: text.into(),
            frequency_offset,
            tx_parity: None,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
            request_queued: true,
        },
    );
}

async fn request_qso(h: &Harness, id: &str, frequency_offset: f64) {
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "CQ W5AU FN20".into(),
                frequency_offset,
                qso_id: Some(id.into()),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

async fn wait_for_ptt_gate(h: &Harness) {
    tokio::time::timeout(
        Duration::from_secs(40),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("worker must reach its PTT gate after building the waveform");
    // These tests use the current-thread runtime. notify_one does not
    // yield, so the worker has polled the held gate before this resumes.
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
}

/// Every item here pivoted before it was refused, so (PAN-185 ownership
/// rule) Autonomous hears nothing from the refusal: the requests that
/// produced the pivoted payloads report their own outcomes.
async fn assert_relocation_refusal(h: &Harness, expected: &[(&str, f64)], diagnostic: &str) {
    let expected_ids: std::collections::HashSet<_> =
        expected.iter().map(|(id, _)| (*id).to_string()).collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut unaired = std::collections::HashSet::new();
    let mut queue_cleared = false;
    let mut saw_diagnostic = false;
    while Instant::now() < deadline
        && (unaired != expected_ids || !queue_cleared || !saw_diagnostic)
    {
        assert!(
            !h.hamlib_rx.try_iter().any(|msg| is_ptt_on(&msg)),
            "relocated content must never reach hardware PTT"
        );
        for msg in h.autonomous_rx.try_iter() {
            if let MessageType::TransmitComplete {
                qso_id: Some(qso_id),
                ..
            } = msg.message_type
            {
                assert!(
                    !expected_ids.contains(&qso_id),
                    "a pivot-refused frame must not report to Autonomous: {qso_id}"
                );
            }
        }
        for msg in h.qso_rx.try_iter() {
            if let MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                qso_id,
                replaced,
                ..
            }) = msg.message_type
            {
                let (_, expected_offset) = expected
                    .iter()
                    .find(|(id, _)| *id == qso_id)
                    .expect("report must identify an item in this request");
                assert_eq!(
                    replaced.expect("pivot must be unwound").frequency_hz,
                    *expected_offset
                );
                assert!(unaired.insert(qso_id), "one rollback per unaired frame");
            }
        }
        for msg in h.tui_rx.try_iter() {
            match msg.message_type {
                MessageType::DiagnosticEvent {
                    target,
                    level,
                    text,
                    ..
                } if text == diagnostic => {
                    assert_eq!(target, "tx.policy");
                    assert_eq!(level, pancetta_core::DiagnosticLevel::Info);
                    saw_diagnostic = true;
                }
                MessageType::TxQueueStatus {
                    sending: None,
                    queued,
                } if queued.is_empty() => {
                    queue_cleared = true;
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(unaired, expected_ids);
    assert!(queue_cleared, "refusal must clear the queue display");
    assert!(
        saw_diagnostic,
        "missing expected Info diagnostic: {diagnostic}"
    );
    assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
    assert!(!h.hamlib_rx.try_iter().any(|msg| is_ptt_on(&msg)));
}

#[tokio::test(start_paused = true)]
async fn pan302_new_attempt_uses_live_timing_preferences() {
    use pancetta_config::station::TxSelfParity;
    use pancetta_core::slot::SlotParity;
    for contact_parity in [None, Some(SlotParity::Even)] {
        let start = tokio::time::Instant::now();
        let base = pancetta_core::time::slot_grid_origin();
        let clock: pancetta_core::time::UtcClock =
            Arc::new(move || base + jiff::SignedDuration::try_from(start.elapsed()).unwrap());
        let mut config = Config::default();
        config.station.tx_self_parity = TxSelfParity::Even;
        let h = build_worker_harness(
            config,
            |_| {},
            Arc::new(pancetta_agent::arm::ArmTime::now),
            clock.clone(),
            true,
        )
        .await;
        {
            let mut config = h.coordinator.config.write().await;
            config.station.tx_self_parity = TxSelfParity::Odd;
            config.station.ptt_lead_ms = 500;
        }
        let id = uuid::Uuid::new_v4().to_string();
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(&id));
        let held = h.coordinator.ptt_sync_gate.lock().await;
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    message_text: "CQ N0CALL FN20".into(),
                    frequency_offset: 1500.0,
                    qso_id: Some(id),
                    attempt_id: Some(pancetta_qso::TxAttemptId(1)),
                    tx_parity: contact_parity,
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        let target = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for message in h.qso_rx.try_iter() {
                    if let MessageType::QsoMessage(
                        crate::message_bus::QsoMessage::TxFrameScheduled { target_slot, .. },
                    ) = message.message_type
                    {
                        return target_slot;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let expected_parity = contact_parity.unwrap_or(SlotParity::Odd);
        assert_eq!(
            SlotParity::of(target),
            expected_parity,
            "new attempt reads live preference, active QSO parity wins"
        );
        // This request is already sleeping. A later edit must not move its lead or target.
        {
            let mut config = h.coordinator.config.write().await;
            config.station.tx_self_parity = TxSelfParity::Even;
            config.station.ptt_lead_ms = 10;
        }
        wait_for_ptt_gate(&h).await;
        if target > base {
            let lead = (target - clock()).as_millis_i64();
            assert!(
                (450..=500).contains(&lead),
                "sleep retains its scheduled 500ms lead: {lead}"
            );
        }
        drop(held);
        assert!(await_ptt_on(&h, Duration::from_secs(5)).await);
        h.coordinator.config.write().await.station.ptt_lead_ms = 900;
        assert!(
            !h.coordinator.abort_current_tx.load(Ordering::Acquire),
            "editing timing does not abort a keyed attempt"
        );
        assert!(!h.hamlib_rx.try_iter().any(|m| matches!(
            m.message_type,
            MessageType::RigControl(RigControlMessage::SetPtt { state: false })
        )));
    }
}

#[tokio::test(start_paused = true)]
async fn pan302_queued_self_cq_refuses_after_disable_but_manual_cq_keys() {
    for autonomous in [false, true] {
        let mut config = Config::default();
        config.station.tx_late_max_ms = 8000;
        let now = pancetta_core::time::slot_grid_origin() + jiff::SignedDuration::from_secs(2);
        let h = build_worker_harness(
            config,
            |_| {},
            Arc::new(pancetta_agent::arm::ArmTime::now),
            Arc::new(move || now),
            true,
        )
        .await;
        h.coordinator
            .autonomous_config_enabled
            .store(false, Ordering::Release);
        let id = uuid::Uuid::new_v4().to_string();
        let attempt = Some(pancetta_qso::TxAttemptId(1));
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(&id));
        if autonomous {
            h.coordinator
                .autonomous_cq_attempts
                .lock()
                .unwrap()
                .insert((id.clone(), attempt));
        }
        let items = vec![crate::message_bus::TransmitRequestItem {
            message_text: "CQ N0CALL FN20".into(),
            frequency_offset: 1500.0,
            qso_id: Some(id),
            attempt_id: attempt,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        }];
        // Source Qso deliberately models the real event forwarder and proves
        // the per-attempt marker survives bundle construction.
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    items,
                    tx_parity: Some(pancetta_core::slot::SlotParity::Even),
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        if autonomous {
            let (success, _, keyed) =
                await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
            assert_eq!(success, Some(false));
            assert!(!keyed, "queued autonomous CQ cannot key after file disable");
        } else {
            assert!(
                await_ptt_on(&h, Duration::from_secs(5)).await,
                "manual CQ retains permission"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pan302_unmarked_autonomous_source_cq_still_keys_after_disable() {
    // PAN-277 regression: `source == Autonomous` alone must not make the
    // pre-PTT recheck refuse a CQ after a file disable — only a self-CQ
    // the engine actually dispatched (marked at send) is refused. The
    // PAN-277 shutdown fixture sends exactly the self-CQ shape with
    // `source == Autonomous` and `Config::default()`, and must key.
    let mut config = Config::default();
    config.station.tx_late_max_ms = 8000;
    let now = pancetta_core::time::slot_grid_origin() + jiff::SignedDuration::from_secs(2);
    let h = build_worker_harness(
        config,
        |_| {},
        Arc::new(pancetta_agent::arm::ArmTime::now),
        Arc::new(move || now),
        true,
    )
    .await;
    h.coordinator
        .autonomous_config_enabled
        .store(false, Ordering::Release);
    let send = |marked: bool| {
        let msg = ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "CQ N0CALL FN20".into(),
                frequency_offset: 1500.0,
                qso_id: None,
                tx_parity: Some(pancetta_core::slot::SlotParity::Even),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        );
        if marked {
            super::super::autonomous::mark_autonomous_self_cq_dispatch(
                &h.coordinator.autonomous_self_cq_dispatches,
                msg.id,
            );
        }
        msg
    };
    // Engine-dispatched (marked): refused, settled unaired.
    let marked = send(true);
    h.coordinator
        .message_bus
        .send_message(marked)
        .await
        .unwrap();
    let (success, _, keyed) = await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(success, Some(false));
    assert!(
        !keyed,
        "engine-dispatched self-CQ cannot key after file disable"
    );
    // Synthetic (unmarked, same shape): keys as before PAN-302.
    let unmarked = send(false);
    h.coordinator
        .message_bus
        .send_message(unmarked)
        .await
        .unwrap();
    assert!(
        await_ptt_on(&h, Duration::from_secs(5)).await,
        "unmarked Autonomous-source request must still key"
    );
}

#[tokio::test(start_paused = true)]
async fn pan302_queued_frame_uses_live_late_cap_on_rekey() {
    let mut config = Config::default();
    config.station.tx_late_max_ms = 8000;
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let now = pancetta_core::time::slot_grid_origin() + jiff::SignedDuration::from_secs(2);
    let h = build_worker_harness(
        config,
        |_| {},
        Arc::new(pancetta_agent::arm::ArmTime::now),
        Arc::new(move || now),
        true,
    )
    .await;
    let id = uuid::Uuid::new_v4().to_string();
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&id));
    async fn send_attempt(h: &Harness, id: &str, text: &str, attempt: u64) {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    message_text: text.into(),
                    frequency_offset: 1500.0,
                    qso_id: Some(id.into()),
                    attempt_id: Some(pancetta_qso::TxAttemptId(attempt)),
                    tx_parity: Some(pancetta_core::slot::SlotParity::Even),
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
    }
    send_attempt(&h, &id, "CQ N0CALL FN20", 1).await;
    assert!(await_ptt_on(&h, Duration::from_secs(5)).await);
    h.coordinator.tx_before_ptt_gate.notified().await; // consume initial key's notification
    let audio = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Audio)
        .await
        .unwrap()
        .1;
    // The initial key may still be before audio enqueue; the replacement is
    // captured by the real worker's keyed supersede poll in either case.
    let held = h.coordinator.ptt_sync_gate.lock().await;
    send_attempt(&h, &id, "CQ N0CALL EM12", 2).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        h.coordinator.tx_before_ptt_gate.notified(),
    )
    .await
    .expect("replacement must reach re-key gate");
    let _ = audio.try_iter().collect::<Vec<_>>();
    h.coordinator
        .tx_late_max_ms_live
        .store(1000, Ordering::Release);
    drop(held);
    let (success, diagnostics, keyed) =
        await_completion(&h, "CQ N0CALL EM12", Duration::from_secs(5)).await;
    assert_eq!(success, Some(false));
    let mut rolled_back: Vec<_> = h
        .qso_rx
        .try_iter()
        .filter_map(|message| match message.message_type {
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                qso_id,
                attempt_id,
                ..
            }) if qso_id == id => attempt_id.map(|id| id.0),
            _ => None,
        })
        .collect();
    rolled_back.sort_unstable();
    assert_eq!(
        rolled_back,
        vec![1, 2],
        "original and refused replacement each settle exactly once"
    );
    assert!(!keyed, "re-key refuses the newly over-cap replacement");
    assert!(
        diagnostics.iter().any(|s| s.contains("1000ms")),
        "{diagnostics:?}"
    );
    assert!(!audio
        .try_iter()
        .any(|m| matches!(m.message_type, MessageType::AudioOutput { .. })));
}

#[tokio::test(start_paused = true)]
async fn pan302_queued_frame_uses_live_late_cap() {
    for bundle in [false, true] {
        let mut config = Config::default();
        config.station.tx_late_max_ms = 8000;
        config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
        let now = pancetta_core::time::slot_grid_origin() + jiff::SignedDuration::from_secs(2);
        let h = build_worker_harness(
            config,
            |_| {},
            Arc::new(pancetta_agent::arm::ArmTime::now),
            Arc::new(move || now),
            true,
        )
        .await;
        let id = uuid::Uuid::new_v4().to_string();
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(&id));
        let held = h.coordinator.ptt_sync_gate.lock().await;
        let item = crate::message_bus::TransmitRequestItem {
            message_text: "CQ N0CALL FN20".into(),
            frequency_offset: 1500.0,
            qso_id: Some(id.clone()),
            attempt_id: Some(pancetta_qso::TxAttemptId(1)),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        };
        let msg = if bundle {
            MessageType::MultiTransmitRequest {
                items: vec![item],
                tx_parity: Some(pancetta_core::slot::SlotParity::Even),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            }
        } else {
            MessageType::TransmitRequest {
                message_text: item.message_text,
                frequency_offset: item.frequency_offset,
                qso_id: item.qso_id,
                attempt_id: item.attempt_id,
                tx_parity: Some(pancetta_core::slot::SlotParity::Even),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            }
        };
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Ft8Transmitter,
                msg,
                Instant::now(),
            ))
            .await
            .unwrap();
        wait_for_ptt_gate(&h).await;
        h.coordinator
            .tx_late_max_ms_live
            .store(1000, Ordering::Release);
        drop(held);
        let (success, diagnostics, keyed) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
        assert_eq!(success, Some(false));
        assert!(!keyed);
        let unaired: Vec<_> = h
            .qso_rx
            .try_iter()
            .filter_map(|m| match m.message_type {
                MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent {
                    qso_id,
                    attempt_id,
                    ..
                }) => Some((qso_id, attempt_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            unaired,
            vec![(id, Some(pancetta_qso::TxAttemptId(1)))],
            "one durable rollback for the refused attempt"
        );
        assert!(
            diagnostics
                .iter()
                .any(|s| s.contains("final pre-PTT check") && s.contains("1000ms")),
            "{diagnostics:?}"
        );
        let audio = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap()
            .1;
        assert!(!audio
            .try_iter()
            .any(|m| matches!(m.message_type, MessageType::AudioOutput { .. })));
    }
}

#[tokio::test]
async fn single_tx_drops_without_keying_when_qso_relocates_during_gate_wait() {
    let h = worker_harness().await;
    let (manager, id) = relocation_manager(&h).await;
    let id_text = id.to_string();
    // Force a pivot so this also exercises tombstone rollback.
    mirror_intent(&h, &id_text, "CQ W5AU FN20", 1550.0);
    let held = h.coordinator.ptt_sync_gate.lock().await;
    let mut switching = Box::pin(manager.apply_tx_offset_switch(
        id,
        1800.0,
        pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
    ));
    // Queue the real relocation FIRST on tokio's FIFO mutex. Do not run
    // the event consumer: the intent mirror deliberately stays stale.
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            switching.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    request_qso(&h, &id_text, 1500.0).await;
    wait_for_ptt_gate(&h).await;
    assert_eq!(h.coordinator.relocation_epochs.epoch(&id_text), 0);
    drop(held);
    switching.await.unwrap();
    assert_relocation_refusal(
        &h,
        &[(&id_text, 1500.0)],
        "TX blocked at final pre-PTT check (QSO relocated during gate wait): 'CQ W5AU FN20'",
    )
    .await;

    // The request that produced the first pivot must remain eligible.
    // A leaked tombstone would suppress it before it can pivot to 1800.
    mirror_intent(&h, &id_text, "CQ W5AU FN20", 1800.0);
    request_qso(&h, &id_text, 1550.0).await;
    let (completions, ptt_on) = qso_completions(&h, &[&id_text], 1, Duration::from_secs(60)).await;
    assert!(
        ptt_on,
        "refusal must unwind the pivot tombstone so the fresh request can key"
    );
    assert_eq!(
        completions,
        vec![vec![true]],
        "the producing request's outcome is the QSO's only one (PAN-185)"
    );
}

/// Every `TransmitComplete` for each of `ids` until each has `count` (or
/// `within` elapses), then a settle window that would catch a duplicate;
/// and whether any PTT-on reached Hamlib.
async fn qso_completions(
    h: &Harness,
    ids: &[&str],
    count: usize,
    within: Duration,
) -> (Vec<Vec<bool>>, bool) {
    let mut completions = vec![Vec::new(); ids.len()];
    let mut ptt_on = false;
    let mut drain = |completions: &mut Vec<Vec<bool>>| {
        ptt_on |= h.hamlib_rx.try_iter().any(|msg| is_ptt_on(&msg));
        while h.tui_rx.try_recv().is_ok() {}
        for msg in h.autonomous_rx.try_iter() {
            if let MessageType::TransmitComplete {
                success,
                qso_id: Some(qso_id),
                ..
            } = msg.message_type
            {
                if let Some(slot) = ids.iter().position(|id| *id == qso_id) {
                    completions[slot].push(success);
                }
            }
        }
    };
    let deadline = Instant::now() + within;
    while Instant::now() < deadline && completions.iter().any(|c| c.len() < count) {
        drain(&mut completions);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    drain(&mut completions);
    (completions, ptt_on)
}

/// PAN-185 scenario 2: a self-CQ frame X pivots to a fresher queued
/// intent and waits at the PTT gate while a relocation is queued on it;
/// the request R that produced the intent is enqueued meanwhile (after X
/// left the coalescer, as in production). `before_release` runs while X
/// is still held; then the gate opens and the relocation lands, so X is
/// refused after its pivot. Returns the QSO id.
async fn refuse_pivoted_self_cq_with_queued_request(
    h: &Harness,
    before_release: impl FnOnce(&Harness),
) -> String {
    let (manager, id) = relocation_manager(h).await;
    let id_text = id.to_string();
    mirror_intent(h, &id_text, "CQ W5AU FN20", 1550.0);
    let held = h.coordinator.ptt_sync_gate.lock().await;
    let mut switching = Box::pin(manager.apply_tx_offset_switch(
        id,
        1800.0,
        pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            switching.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    request_qso(h, &id_text, 1500.0).await;
    wait_for_ptt_gate(h).await;
    request_qso(h, &id_text, 1550.0).await;
    before_release(h);
    drop(held);
    switching.await.unwrap();
    id_text
}

/// Wait for X's refusal (the replaced 1500 Hz frame's rollback reaches
/// the QSO engine) and assert Autonomous heard nothing meanwhile.
async fn await_silent_pivot_refusal(h: &Harness, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for msg in h.autonomous_rx.try_iter() {
            if let MessageType::TransmitComplete {
                qso_id: Some(qso_id),
                ..
            } = msg.message_type
            {
                assert_ne!(
                    qso_id, id,
                    "no completion for the self-CQ while its queued request is pending"
                );
            }
        }
        let rolled_back = h.qso_rx.try_iter().any(|msg| {
            matches!(
                msg.message_type,
                MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { ref qso_id, replaced: Some(ref p), .. })
                    if qso_id == id && p.frequency_hz == 1500.0
            )
        });
        if rolled_back {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the pivoted frame was never refused"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// PAN-185 scenario 2: the pivot is refused, then the queued request airs.
/// Autonomous sees exactly one outcome for the self-CQ: the aired one.
#[tokio::test]
async fn pan185_pivot_refused_self_cq_settles_once_when_the_queued_request_airs() {
    let h = worker_harness().await;
    let id = refuse_pivoted_self_cq_with_queued_request(&h, |_| {}).await;
    await_silent_pivot_refusal(&h, &id).await;
    let (completions, ptt_on) = qso_completions(&h, &[&id], 1, Duration::from_secs(60)).await;
    assert!(ptt_on, "the queued request must key");
    assert_eq!(completions, vec![vec![true]]);
}

/// PAN-185 scenario 2: the pivot is refused, then the queued request is
/// refused too (the rig link dropped). One outcome: the failure.
#[tokio::test]
async fn pan185_pivot_refused_self_cq_settles_once_when_the_queued_request_is_refused() {
    let h = worker_harness().await;
    let id = refuse_pivoted_self_cq_with_queued_request(&h, |h| {
        h.coordinator
            .rig_conn_state
            .store(RigConnState::PollingFailed.as_u8(), Ordering::Release);
    })
    .await;
    await_silent_pivot_refusal(&h, &id).await;
    let mut diagnostics = Vec::new();
    let mut completions = Vec::new();
    let mut ptt_on = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && completions.is_empty() {
        ptt_on |= h.hamlib_rx.try_iter().any(|msg| is_ptt_on(&msg));
        for msg in h.tui_rx.try_iter() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                diagnostics.push(text);
            }
        }
        for msg in h.autonomous_rx.try_iter() {
            if let MessageType::TransmitComplete {
                success,
                qso_id: Some(qso_id),
                ..
            } = msg.message_type
            {
                if qso_id == id {
                    completions.push(success);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (more, more_ptt) = qso_completions(&h, &[&id], 0, Duration::ZERO).await;
    completions.extend(more.concat());
    assert_eq!(completions, vec![false]);
    assert!(
        !ptt_on && !more_ptt,
        "no PTT keying while the rig is disconnected"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("TX blocked (rig is disconnected)")),
        "the queued request must be refused by the hard mute, got {diagnostics:?}"
    );
}

#[tokio::test]
async fn single_tx_keys_normally_when_no_relocation_races() {
    let h = worker_harness().await;
    let (_manager, id) = relocation_manager(&h).await;
    let held = h.coordinator.ptt_sync_gate.lock().await;
    request_qso(&h, &id.to_string(), 1500.0).await;
    wait_for_ptt_gate(&h).await;
    drop(held);
    assert!(await_ptt_on(&h, Duration::from_secs(5)).await);
}

#[test]
fn bundle_epoch_check_ignores_items_with_no_qso_id() {
    let epochs = pancetta_qso::qso_manager::TxRelocationEpochs::default();
    let snapshots = vec![
        (None, 0),
        (Some("qa".to_string()), 0),
        (Some("qb".to_string()), 0),
    ];
    epochs.bump("");
    epochs.bump("not-in-the-bundle");
    assert!(!super::relocation_epoch_stale(&epochs, &snapshots));
    epochs.bump("qb");
    assert!(super::relocation_epoch_stale(&epochs, &snapshots));
    assert!(!super::relocation_epoch_stale(&epochs, &[(None, 123)]));
    assert!(!super::relocation_epoch_stale(&epochs, &[]));
}

async fn second_relocation_qso(h: &Harness, manager: &pancetta_qso::QsoManager) -> String {
    let id = manager
        .respond_to_cq_manual("K1ABC".into(), 2200.0, None)
        .await
        .unwrap()
        .to_string();
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key(&id));
    id
}

async fn request_qso_bundle(h: &Harness, ids: &[String; 2], offsets: [f64; 2]) {
    let texts = ["CQ W5AU FN20", "K1ABC W5AU FN20"];
    let items = ids
        .iter()
        .zip(offsets)
        .zip(texts)
        .map(
            |((id, frequency_offset), text)| crate::message_bus::TransmitRequestItem {
                attempt_id: None,
                message_text: text.into(),
                frequency_offset,
                qso_id: Some(id.clone()),
                origin: crate::message_bus::TxOrigin::Local,
                remote_client_key_id: None,
            },
        )
        .collect();
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items,
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn bundle_drops_without_keying_when_any_live_item_relocates_during_gate_wait() {
    let h = worker_harness().await;
    let (manager, id) = relocation_manager(&h).await;
    let ids = [id.to_string(), second_relocation_qso(&h, &manager).await];
    mirror_intent(&h, &ids[0], "CQ W5AU FN20", 1550.0);
    mirror_intent(&h, &ids[1], "K1ABC W5AU FN20", 2250.0);
    let held = h.coordinator.ptt_sync_gate.lock().await;
    let mut switching = Box::pin(manager.apply_tx_offset_switch(
        id,
        1800.0,
        pancetta_qso::qso_manager::OffsetRelocationOrigin::OperatorForced,
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            switching.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    request_qso_bundle(&h, &ids, [1500.0, 2200.0]).await;
    wait_for_ptt_gate(&h).await;
    drop(held);
    switching.await.unwrap();

    assert_relocation_refusal(
        &h,
        &[(&ids[0], 1500.0), (&ids[1], 2200.0)],
        "dropping multi-TX bundle at final pre-PTT check: QSO relocated during gate wait",
    )
    .await;

    // Both prior pivot requests remain eligible after the whole-bundle
    // refusal. Leaving either tombstone behind would drop its item.
    mirror_intent(&h, &ids[0], "CQ W5AU FN20", 1800.0);
    request_qso_bundle(&h, &ids, [1550.0, 2250.0]).await;
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut sending_ids = std::collections::HashSet::new();
    let mut keyed = false;
    while Instant::now() < deadline && !keyed {
        for msg in h.tui_rx.try_iter() {
            if let MessageType::TxQueueStatus {
                sending: Some(head),
                queued,
            } = msg.message_type
            {
                sending_ids = std::iter::once(head)
                    .chain(queued)
                    .filter_map(|it| it.qso_id)
                    .collect();
            }
        }
        keyed |= h.hamlib_rx.try_iter().any(|msg| is_ptt_on(&msg));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(keyed, "the fresh bundle must remain eligible to key");
    assert_eq!(
        sending_ids,
        ids.iter().cloned().collect(),
        "both tombstones must be unwound"
    );
    // PAN-185: the producing requests own each QSO's outcome; the
    // refused pivots reported nothing, so each QSO completes once.
    let (completions, _) = qso_completions(
        &h,
        &[ids[0].as_str(), ids[1].as_str()],
        1,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(completions, vec![vec![true], vec![true]]);
}

#[tokio::test]
async fn bundle_keys_normally_when_no_relocation_races() {
    let h = worker_harness().await;
    let (manager, id) = relocation_manager(&h).await;
    let ids = [id.to_string(), second_relocation_qso(&h, &manager).await];
    let held = h.coordinator.ptt_sync_gate.lock().await;
    request_qso_bundle(&h, &ids, [1500.0, 2200.0]).await;
    wait_for_ptt_gate(&h).await;
    drop(held);
    assert!(await_ptt_on(&h, Duration::from_secs(5)).await);
}

/// PAN-119: a single TX-worker panic must not end the operating session.
/// The supervisor (`check_task_handles` → `component_restart_policy` →
/// `restart_component`) has to restart the worker within its restart
/// budget, and transmission must resume with no operator intervention:
/// a CQ sent after the restart keys PTT again. Before PAN-119 the
/// policy was `DegradeOnly`, so this same sequence left the component
/// `Failed` for the rest of the session and no PTT ever arrived.
///
/// This test also guards the two hidden prerequisites of a working
/// restart: `restart_component` must have an `Ft8Transmitter` arm, and
/// `start_transmitter_component` must register its channel with
/// `get_or_create_channel` so the second invocation does not fail with
/// "Channel already exists" (the failure mode the DxCluster analog
/// documented before `get_or_create_channel` existed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tx_worker_panic_is_restarted_and_keys_again() {
    let mut h = worker_harness().await;

    // Remove the real TX worker's handle and stop the task itself: the
    // bus receiver is multi-consumer, so if the original worker kept
    // running it could key the CQ below itself and the test would pass
    // without any restart actually happening.
    let real = h
        .coordinator
        .named_task_handles
        .iter()
        .position(|(id, _)| *id == ComponentId::Ft8Transmitter)
        .map(|i| h.coordinator.named_task_handles.remove(i).1)
        .expect("worker_harness registers the TX worker handle");
    real.abort();
    let _ = real.await;

    // A task that panics immediately under ComponentId::Ft8Transmitter —
    // exactly what the supervisor observes on a real TX-worker panic.
    h.coordinator.named_task_handles.push((
        ComponentId::Ft8Transmitter,
        tokio::spawn(async {
            panic!("injected TX-worker panic");
            #[allow(unreachable_code)]
            Ok(())
        }),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // One supervisor pass: detects the panic and applies the policy.
    h.coordinator.check_task_handles().await;

    assert_eq!(
        h.coordinator
            .component_status
            .read()
            .await
            .get(&ComponentId::Ft8Transmitter)
            .map(|s| &s.state),
        Some(&super::super::ComponentState::Running),
        "a single TX-worker panic must be restarted, not left Failed"
    );

    // Transmission resumes without operator intervention.
    request(&h, "CQ N0CALL FN20").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "the restarted TX worker must key PTT again"
    );
}

/// PAN-73 follow-up: a message the dead worker had taken off its channel
/// but not handled (held in the coordinator-owned requeue front) is keyed
/// by the restarted worker, with no new request sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requeued_message_survives_a_tx_worker_restart() {
    let mut h = worker_harness().await;
    let real = h
        .coordinator
        .named_task_handles
        .iter()
        .position(|(id, _)| *id == ComponentId::Ft8Transmitter)
        .map(|i| h.coordinator.named_task_handles.remove(i).1)
        .expect("worker_harness registers the TX worker handle");
    real.abort();
    let _ = real.await;

    let (_, rx) = h
        .coordinator
        .message_bus
        .get_or_create_channel(ComponentId::Ft8Transmitter)
        .await
        .unwrap();
    super::TxInbox::new(rx, h.coordinator.tx_requeue.clone()).requeue_front(vec![
        ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "CQ N0CALL FN20".into(),
                frequency_offset: 1500.0,
                qso_id: None,
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ),
    ]);

    h.coordinator.named_task_handles.push((
        ComponentId::Ft8Transmitter,
        tokio::spawn(async {
            panic!("injected TX-worker panic");
            #[allow(unreachable_code)]
            Ok(())
        }),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.coordinator.check_task_handles().await;

    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "the restarted TX worker must key the request its predecessor was holding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_f8_abort_reports_frame_not_sent_to_qso() {
    let h = worker_harness().await;
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-f8"));
    let text = "K1ABC N0CALL FN20";
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: text.into(),
                frequency_offset: 1500.0,
                qso_id: Some("qso-f8".into()),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if h.tui_rx.try_iter().any(|msg| matches!(msg.message_type, MessageType::TxQueueStatus { queued, .. } if queued.iter().any(|item| item.text == text))) { break; }
        assert!(Instant::now() < deadline, "worker did not queue the frame");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.coordinator
        .abort_current_tx
        .store(true, Ordering::Release);
    assert_eq!(
        await_completion(&h, text, Duration::from_secs(5)).await.0,
        Some(false)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(msg) = h.qso_rx.try_recv() {
            assert!(
                matches!(msg.message_type, MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { attempt_id: _, qso_id, message_text, replaced: None }) if qso_id == "qso-f8" && message_text == text)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "F8 failed to notify QSO accounting"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// PAN-75: a keyed frame superseded by fresher content the single-TX
/// re-key path cannot use (`SupersedeOutcome::NotViable`; here a bundle,
/// which that path never folds) is abandoned. It must be reported as not
/// sent, like the `Replace` arm's abandoned frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_viable_supersede_reports_abandoned_frame_not_sent_to_qso() {
    assert_not_viable_supersede_reports_abandoned_frame(false).await;
}

/// The same, superseding only once Step 7 has routed the audio, so the
/// Step 8 playback wait's `NotViable` arm handles it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_viable_supersede_during_playback_reports_abandoned_frame_not_sent_to_qso() {
    assert_not_viable_supersede_reports_abandoned_frame(true).await;
}

async fn assert_not_viable_supersede_reports_abandoned_frame(during_playback: bool) {
    let h = worker_harness().await;
    h.coordinator
        .active_tx_qsos
        .write()
        .unwrap()
        .insert(super::super::active_tx_qso_key("qso-sv"));
    let text = "K1ABC N0CALL FN20";
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Ft8Transmitter,
            MessageType::TransmitRequest {
                attempt_id: None,
                message_text: text.into(),
                frequency_offset: 1500.0,
                qso_id: Some("qso-sv".into()),
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(
        await_ptt_on(&h, Duration::from_secs(20)).await,
        "worker did not key the frame"
    );
    if during_playback {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !h
            .tui_rx
            .try_iter()
            .any(|msg| matches!(msg.message_type, MessageType::TxFrameLogged { .. }))
        {
            assert!(Instant::now() < deadline, "worker did not start the audio");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    h.coordinator
        .message_bus
        .send_message(ComponentMessage::new(
            ComponentId::Autonomous,
            ComponentId::Ft8Transmitter,
            MessageType::MultiTransmitRequest {
                items: vec![crate::message_bus::TransmitRequestItem {
                    attempt_id: None,
                    message_text: "K1ABC N0CALL -10".into(),
                    frequency_offset: 1500.0,
                    qso_id: Some("qso-sv".into()),
                    origin: crate::message_bus::TxOrigin::Local,
                    remote_client_key_id: None,
                }],
                tx_parity: None,
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(msg) = h.qso_rx.try_recv() {
            assert!(
                matches!(msg.message_type, MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameNotSent { attempt_id: _, qso_id, message_text, replaced: None }) if qso_id == "qso-sv" && message_text == text)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "abandoned frame was not reported to QSO accounting"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // PAN-279: the same-QSO superseding bundle is requeued and airs in
    // its own next slot, so it owns qso-sv's Autonomous outcome: the
    // abandoned frame sends Autonomous no failure.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut completions: Vec<(String, bool)> = Vec::new();
    while Instant::now() < deadline && !completions.iter().any(|(t, _)| t == "K1ABC N0CALL -10") {
        while let Ok(msg) = h.autonomous_rx.try_recv() {
            if let MessageType::TransmitComplete {
                success,
                message_text,
                ..
            } = msg.message_type
            {
                completions.push((message_text, success));
            }
        }
        while h.hamlib_rx.try_recv().is_ok() {}
        while h.tui_rx.try_recv().is_ok() {}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !completions.iter().any(|(t, ok)| t == text && !ok),
        "abandoned frame must not fail to Autonomous: {completions:?}"
    );
    assert!(
        completions.iter().any(|(t, _)| t == "K1ABC N0CALL -10"),
        "the requeued superseder must reach its own outcome: {completions:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_rig_blocks_ptt_until_the_link_recovers() {
    let h = worker_harness().await;
    h.coordinator
        .rig_conn_state
        .store(RigConnState::PollingFailed.as_u8(), Ordering::Release);

    request(&h, "CQ N0CALL FN20").await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(
        completed,
        Some(false),
        "a TX requested while the rig is disconnected must complete as failed"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("TX blocked (rig is disconnected)")),
        "the key-time gate must report the rig as disconnected, got {diagnostics:?}"
    );
    assert!(h.qso_rx.try_recv().is_err());
    assert!(!ptt_on, "no PTT keying while the rig is disconnected");

    // The link recovers: the next request keys again (a different text,
    // so no duplicate-suppression path can swallow it).
    h.coordinator
        .rig_conn_state
        .store(RigConnState::Connected.as_u8(), Ordering::Release);
    request(&h, "CQ N0CALL FN21").await;
    assert!(
        await_ptt_on(&h, Duration::from_secs(40)).await,
        "TX must key again once the rig connection has recovered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_audio_output_blocks_ptt() {
    let h = worker_harness().await;
    h.coordinator
        .audio_output_alive
        .store(false, Ordering::Release);

    request(&h, "CQ N0CALL FN20").await;
    let (completed, diagnostics, ptt_on) =
        await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
    assert_eq!(
        completed,
        Some(false),
        "a TX requested while the audio output is dead must complete as failed"
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("TX blocked (audio output device is dead)")),
        "the key-time gate must report the dead output device, got {diagnostics:?}"
    );
    assert!(h.qso_rx.try_recv().is_err());
    assert!(!ptt_on, "no PTT keying while the audio output is dead");
}
