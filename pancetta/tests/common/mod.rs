//! Shared input/output adapter for the production transmitter.
#![allow(clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use pancetta_hamlib::{MockRig, PttState, RigControl, Vfo};
use pancetta_lib::coordinator::{active_tx_qso_key, compute_manual_tx_offset, DecodeWindowConfig};
use pancetta_lib::message_bus::{
    ComponentId, ComponentMessage, MessageBus, MessageType, RigControlMessage,
};
use pancetta_qso::{CallInitiation, QsoEvent, QsoManager, QsoManagerConfig, QsoMetadata, QsoState};

use pancetta_core::slot::SlotParity;
use pancetta_core::TxPolicy;

// ---------------------------------------------------------------------------
// Timeline (mirrors the style of pancetta-qso::sim::Timeline)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct KeyedTx {
    pub slot: u64,
    pub text: String,
    pub freq_hz: f64,
    pub qso_id: Option<String>,
    pub ptt_keyed: bool,
    pub ptt_released: bool,
}

#[derive(Debug, Clone)]
pub struct DroppedTx {
    pub slot: u64,
    pub text: String,
    pub qso_id: Option<String>,
    pub reason: DropReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    WorkerRefused,
}

#[derive(Debug, Default)]
pub struct Timeline {
    pub keyed: Vec<KeyedTx>,
    pub dropped: Vec<DroppedTx>,
    pub last_slot: u64,
}

impl Timeline {
    pub fn keyed_for_qso(&self, qso_id: &str) -> Vec<&KeyedTx> {
        let key = active_tx_qso_key(qso_id);
        self.keyed
            .iter()
            .filter(|k| k.qso_id.as_deref().map(active_tx_qso_key) == Some(key.clone()))
            .collect()
    }

    pub fn keyed_any_for_qso(&self, qso_id: &str) -> bool {
        self.keyed_for_qso(qso_id).iter().any(|k| k.ptt_keyed)
    }

    pub fn keyed_anything(&self) -> bool {
        self.keyed.iter().any(|k| k.ptt_keyed)
    }

    pub fn offsets_in_slot(&self, slot: u64) -> Vec<f64> {
        self.keyed
            .iter()
            .filter(|k| k.slot == slot && k.ptt_keyed)
            .map(|k| k.freq_hz)
            .collect()
    }

    // --- Assertion helpers (panic with a readable timeline on failure) ---

    pub fn assert_keyed_for_qso(&self, qso_id: &str) {
        assert!(
            self.keyed_any_for_qso(qso_id),
            "expected PTT to key for QSO {qso_id}, but it never did.\n{self}"
        );
    }

    pub fn assert_not_keyed_for_qso(&self, qso_id: &str) {
        assert!(
            !self.keyed_any_for_qso(qso_id),
            "expected PTT to NEVER key for QSO {qso_id}, but it did.\n{self}"
        );
    }

    pub fn assert_silent(&self) {
        assert!(
            !self.keyed_anything(),
            "expected the rig to stay silent, but PTT keyed.\n{self}"
        );
    }

    pub fn assert_keyed_at_offset(&self, qso_id: &str, expected: f64) {
        let keyed = self.keyed_for_qso(qso_id);
        let found = keyed
            .iter()
            .any(|k| k.ptt_keyed && (k.freq_hz - expected).abs() < 0.001);
        assert!(
            found,
            "expected QSO {qso_id} to key at {expected} Hz, but offsets were {:?}.\n{self}",
            keyed.iter().map(|k| k.freq_hz).collect::<Vec<_>>()
        );
    }

    pub fn assert_all_released(&self) {
        for k in &self.keyed {
            if k.ptt_keyed {
                assert!(
                    k.ptt_released,
                    "PTT keyed for '{}' (qso {:?}) but was never released.\n{self}",
                    k.text, k.qso_id
                );
            }
        }
    }

    pub fn assert_distinct_offsets_in_slot(&self, slot: u64, n: usize) {
        let mut offs = self.offsets_in_slot(slot);
        offs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        offs.dedup();
        assert_eq!(
            offs.len(),
            n,
            "expected {n} distinct keyed offset(s) in slot {slot}, saw {:?}.\n{self}",
            offs
        );
    }
}

impl std::fmt::Display for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "=== Coord TX Timeline (slots 0..={}) ===",
            self.last_slot
        )?;
        writeln!(
            f,
            "{:>4} | {:<6} | {:<22} | {:>7} | {:<10} | qso",
            "slot", "PTT", "text", "freq", "released?"
        )?;
        for k in &self.keyed {
            writeln!(
                f,
                "{:>4} | {:<6} | {:<22} | {:>7.0} | {:<10} | {}",
                k.slot,
                if k.ptt_keyed { "ON" } else { "(none)" },
                truncate(&k.text, 22),
                k.freq_hz,
                if k.ptt_released { "released" } else { "STUCK" },
                k.qso_id.as_deref().unwrap_or("-"),
            )?;
        }
        for d in &self.dropped {
            writeln!(
                f,
                "{:>4} | DROP   | {:<22} | {:>7} | {:<10} | {} ({:?})",
                d.slot,
                truncate(&d.text, 22),
                "-",
                "-",
                d.qso_id.as_deref().unwrap_or("-"),
                d.reason,
            )?;
        }
        Ok(())
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

// ---------------------------------------------------------------------------
// CoordSim fixture
// ---------------------------------------------------------------------------

pub struct CoordSim {
    pub bus: MessageBus,
    pub manager: QsoManager,
    pub qso_rx: tokio::sync::broadcast::Receiver<QsoEvent>,
    rig: Arc<MockRig>,
    pub active_tx_qsos: Arc<RwLock<HashSet<String>>>,
    pub active_tx_offsets: Arc<RwLock<HashMap<String, f64>>>,
    pub tx_policy: Arc<AtomicU8>,
    pub decode_window: Arc<tokio::sync::RwLock<DecodeWindowConfig>>,
    pub active_protocol_mode: Arc<AtomicU8>,
    pub active_slot_ns: Arc<std::sync::atomic::AtomicI64>,
    pub active_decode_phase_ns: Arc<std::sync::atomic::AtomicI64>,
    pub current_decode_effort: Arc<std::sync::atomic::AtomicU8>,
    pub resolved_hardware_tier: Arc<std::sync::atomic::AtomicU8>,
    worker: pancetta_lib::coordinator::testing::WorkerHarness,
    home: tempfile::TempDir,
    pub observations: Arc<Mutex<Observations>>,
    stop_consumers: Arc<AtomicBool>,
    consumers: Vec<tokio::task::JoinHandle<()>>,
    outcomes: crossbeam_channel::Receiver<ComponentMessage>,
    arm_now: pancetta_agent::arm::ArmTime,
    pub our_callsign: String,
    pub timeline: Timeline,
    slot: u64,
    pub dial_frequency_hz: Arc<AtomicU64>,
    pub split_tx_frequency_hz: Arc<AtomicU64>,
    pub completed: Vec<QsoMetadata>,
    deferred_pending: Vec<PendingTx>,
    pub remote_tx_arm: Arc<Mutex<pancetta_agent::arm::ArmState>>,
}

impl CoordSim {
    pub async fn new(our_callsign: &str) -> Self {
        let mut config = pancetta_config::Config::default();
        config.station.callsign = our_callsign.to_string();
        config.autonomous.max_concurrent_qsos = 8;
        let home = tempfile::tempdir().expect("temporary station home");
        let arm_now = pancetta_agent::arm::ArmTime::now();
        let worker = pancetta_lib::coordinator::testing::WorkerHarness::new(
            config,
            home.path().to_owned(),
            Arc::new(move || arm_now),
        )
        .await;
        let mut manager = QsoManager::new(QsoManagerConfig {
            our_callsign: our_callsign.to_string(),
            our_grid: Some("EM10".into()),
            ..Default::default()
        });
        worker.wire_manager(&mut manager);
        let qso_rx = manager.subscribe();
        let rig = Arc::new(MockRig::default());
        rig.connect().await.expect("mock connect");
        let observations = Arc::new(Mutex::new(Observations::default()));
        let stop_consumers = Arc::new(AtomicBool::new(false));
        let mut consumers = Vec::new();
        {
            let rx = worker.hamlib.clone();
            let rig = rig.clone();
            let stop = stop_consumers.clone();
            let obs = observations.clone();
            consumers.push(tokio::spawn(async move {
                loop {
                    while let Ok(message) = rx.try_recv() {
                        if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                            message.message_type
                        {
                            rig.set_ptt(
                                Vfo::Current,
                                if state { PttState::On } else { PttState::Off },
                            )
                            .await
                            .expect("mock PTT");
                            obs.lock().unwrap().ptt.push(state);
                        }
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }));
        }
        {
            let rx = worker.audio.clone();
            let stop = stop_consumers.clone();
            let obs = observations.clone();
            consumers.push(tokio::spawn(async move {
                loop {
                    while let Ok(message) = rx.try_recv() {
                        match message.message_type {
                            MessageType::AudioOutput {
                                samples,
                                playback_started,
                                ..
                            } => {
                                if !samples.is_empty() {
                                    assert!(
                                        samples.iter().any(|x| *x != 0.0),
                                        "worker waveform is silent"
                                    );
                                    obs.lock().unwrap().audio += 1;
                                }
                                if let Some(ack) = playback_started {
                                    ack.started(chrono::Utc::now());
                                }
                            }
                            MessageType::AudioFlush { ack, .. } => {
                                ack.complete(true);
                            }
                            _ => {}
                        }
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }));
        }
        let (events_tx, outcomes) = crossbeam_channel::unbounded();
        {
            let qso = worker.qso.clone();
            let autonomous = worker.autonomous.clone();
            let tui = worker.tui.clone();
            let stop = stop_consumers.clone();
            let obs = observations.clone();
            consumers.push(tokio::spawn(async move {
                loop {
                    for inbox in [&qso, &autonomous] {
                        while let Ok(message) = inbox.try_recv() {
                            events_tx.send(message).expect("observation receiver");
                        }
                    }
                    while let Ok(message) = tui.try_recv() {
                        if let MessageType::DiagnosticEvent { text, .. } = message.message_type {
                            obs.lock().unwrap().diagnostics.push(text);
                        }
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }));
        }
        Self {
            bus: worker.bus.clone(),
            manager,
            qso_rx,
            rig,
            active_tx_qsos: worker.active_tx_qsos.clone(),
            active_tx_offsets: worker.active_tx_offsets.clone(),
            tx_policy: worker.tx_policy.clone(),
            decode_window: worker.decode_window.clone(),
            active_protocol_mode: worker.active_protocol_mode.clone(),
            active_slot_ns: worker.active_slot_ns.clone(),
            active_decode_phase_ns: worker.active_decode_phase_ns.clone(),
            current_decode_effort: worker.current_decode_effort.clone(),
            resolved_hardware_tier: worker.resolved_hardware_tier.clone(),
            dial_frequency_hz: worker.operating_frequency_hz.clone(),
            split_tx_frequency_hz: worker.split_tx_frequency_hz.clone(),
            remote_tx_arm: worker.remote_tx_arm.clone(),

            our_callsign: our_callsign.into(),
            timeline: Timeline::default(),
            slot: 0,
            completed: Vec::new(),
            deferred_pending: Vec::new(),
            worker,
            home,
            observations,
            stop_consumers,
            consumers,
            outcomes,
            arm_now,
        }
    }

    pub fn set_rx_dial(&self, hz: u64) {
        self.dial_frequency_hz.store(hz, Ordering::Relaxed);
    }

    pub fn set_split_tx(&self, hz: u64) {
        self.split_tx_frequency_hz.store(hz, Ordering::Relaxed);
    }

    pub fn set_policy(&self, policy: TxPolicy) {
        self.tx_policy.store(policy.as_u8(), Ordering::Release);
    }

    pub fn arm_remote_tx(&self) {
        self.arm_remote_tx_as("sim-client-key-id");
    }

    pub fn arm_remote_tx_as(&self, client_key_id: &str) {
        let now_ms = self.arm_now.unix_ms;
        let mut st = self.remote_tx_arm.lock().expect("arm lock");
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
                jti: "sim-arm-jti".to_string(),
                client_key_id: client_key_id.to_string(),
            },
            self.arm_now,
        );
        st.heartbeat("sim-arm-jti", 1, self.arm_now);
    }

    pub fn pump_qso_events(&mut self) -> Vec<PendingTx> {
        let mut pending = std::mem::take(&mut self.deferred_pending);
        loop {
            match self.qso_rx.try_recv() {
                Ok(ev) => self.apply_event(ev, &mut pending),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        pending
    }

    pub fn apply_event(&mut self, ev: QsoEvent, pending: &mut Vec<PendingTx>) {
        match ev {
            QsoEvent::StateChanged {
                qso_id, new_state, ..
            } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                if new_state.is_active() {
                    self.active_tx_qsos.write().unwrap().insert(key.clone());
                    // FQ-F3 mirror: keep the offset map in sync alongside
                    // active_tx_qsos, exactly as coordinator/qso.rs does.
                    if let Some(freq) = new_state.frequency() {
                        self.active_tx_offsets.write().unwrap().insert(key, freq);
                    }
                } else if let QsoState::Completed { frequency, .. } = &new_state {
                    // PAN-280 mirror: grace window on entering `Completed`,
                    // so the closing 73 still keys (the coordinator no longer
                    // does this on `QsoCompleted`, which waits for the 73 to
                    // air). The scenario removes it explicitly via
                    // `expire_qso` when it wants to model grace elapsing.
                    self.active_tx_qsos.write().unwrap().insert(key.clone());
                    self.active_tx_offsets
                        .write()
                        .unwrap()
                        .insert(key, *frequency);
                } else if matches!(new_state, QsoState::Failed { .. }) {
                    self.active_tx_qsos.write().unwrap().remove(&key);
                    self.active_tx_offsets.write().unwrap().remove(&key);
                }
            }
            QsoEvent::QsoCompleted { metadata, .. } => {
                // Capture the completed metadata so scenarios can assert on the
                // stamped RF frequency (dial + audio offset).
                self.completed.push(metadata);
            }
            QsoEvent::QsoFailed { qso_id, .. } => {
                let key = active_tx_qso_key(&qso_id.to_string());
                self.active_tx_qsos.write().unwrap().remove(&key);
                self.active_tx_offsets.write().unwrap().remove(&key);
            }
            QsoEvent::MessageToSend {
                attempt_id,
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
                remote_client_key_id,
            } => {
                // SECURITY (B1 mirror): derive the TransmitRequest origin from
                // the QSO's `remote_origin` exactly as coordinator/qso.rs does,
                // so a remote QSO's forwarded TX is `Remote` (arm-gated) and a
                // local QSO's is `Local` (byte-identical).
                let text = pancetta_qso::utils::generate_ft8_message(&message, &self.our_callsign)
                    .expect("FT8 text");
                pending.push(PendingTx {
                    text,
                    frequency_offset: frequency,
                    qso_id: Some(qso_id.to_string()),
                    tx_parity,
                    origin: if remote_origin {
                        pancetta_lib::message_bus::TxOrigin::Remote
                    } else {
                        pancetta_lib::message_bus::TxOrigin::Local
                    },
                    remote_client_key_id,
                    attempt_id,
                });
            }
            _ => {}
        }
    }

    pub fn expire_qso(&mut self, qso_id: &str) {
        let key = active_tx_qso_key(qso_id);
        self.active_tx_qsos.write().unwrap().remove(&key);
        self.active_tx_offsets.write().unwrap().remove(&key);
    }

    pub fn manual_tx(&self, text: &str, freq_hz: f64) -> PendingTx {
        PendingTx {
            text: text.to_string(),
            frequency_offset: freq_hz,
            qso_id: None,
            tx_parity: None,
            origin: pancetta_lib::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
            attempt_id: None,
        }
    }

    pub fn remote_tx(&self, text: &str, freq_hz: f64) -> PendingTx {
        PendingTx {
            text: text.to_string(),
            frequency_offset: freq_hz,
            qso_id: None,
            tx_parity: None,
            origin: pancetta_lib::message_bus::TxOrigin::Remote,
            remote_client_key_id: None,
            attempt_id: None,
        }
    }

    pub async fn submit(&self, pending: &[PendingTx]) {
        for p in pending {
            let request = MessageType::TransmitRequest {
                attempt_id: p.attempt_id,
                message_text: p.text.clone(),
                frequency_offset: p.frequency_offset,
                qso_id: p.qso_id.clone(),
                tx_parity: p.tx_parity,
                origin: p.origin,
                remote_client_key_id: p.remote_client_key_id.clone(),
            };
            assert!(
                self.bus
                    .send_message_checked(ComponentMessage::new(
                        ComponentId::Qso,
                        ComponentId::Ft8Transmitter,
                        request.clone(),
                        Instant::now(),
                    ))
                    .await
                    .expect("submit worker request"),
                "worker route refused request"
            );
            self.worker.accepted(&request);
        }
    }

    pub async fn drive_slot(&mut self, pending: Vec<PendingTx>) -> u64 {
        let slot = self.slot;
        self.slot += 1;
        self.timeline.last_slot = slot;
        let before = self.observations.lock().unwrap().audio;
        let ptt_start = self.observations.lock().unwrap().ptt.len();
        self.submit(&pending).await;
        let mut remaining = pending.clone();
        let deadline = Instant::now() + Duration::from_secs(45);
        while !remaining.is_empty() {
            while let Ok(message) = self.outcomes.try_recv() {
                if let MessageType::QsoMessage(ref outcome) = message.message_type {
                    use pancetta_lib::message_bus::QsoMessage;
                    self.worker.apply_outcome(&self.manager, outcome).await;
                    let settled = match outcome {
                        QsoMessage::TxFrameSent {
                            qso_id,
                            message_text,
                            attempt_id,
                            aired,
                            ..
                        } => Some((qso_id, message_text, attempt_id, Some(aired.frequency_hz))),
                        QsoMessage::TxFrameNotSent {
                            qso_id,
                            message_text,
                            attempt_id,
                            ..
                        } => Some((qso_id, message_text, attempt_id, None)),
                        _ => None,
                    };
                    if let Some((id, text, attempt, aired_frequency)) = settled {
                        self.observations.lock().unwrap().settlements.push((
                            id.clone(),
                            *attempt,
                            aired_frequency.is_some(),
                        ));
                        if let Some(i) = remaining.iter().position(|p| {
                            p.qso_id.as_ref() == Some(id)
                                && p.text == *text
                                && p.attempt_id == *attempt
                        }) {
                            let mut p = remaining.remove(i);
                            if let Some(hz) = aired_frequency {
                                p.frequency_offset = hz;
                            }
                            self.record_settlement(slot, p, aired_frequency.is_some(), ptt_start)
                                .await;
                        }
                    }
                }
                if let MessageType::TransmitComplete {
                    message_text,
                    success,
                    ..
                } = message.message_type
                {
                    if let Some(i) = remaining
                        .iter()
                        .position(|p| p.qso_id.is_none() && p.text == message_text)
                    {
                        let p = remaining.remove(i);
                        self.record_settlement(slot, p, success, ptt_start).await;
                    }
                }
            }

            assert!(
                Instant::now() < deadline,
                "worker settlement timeout: {remaining:?}\n{}\n{:?}",
                self.timeline,
                self.observations.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if self.timeline.keyed.iter().any(|k| k.slot == slot) {
            assert!(
                self.observations.lock().unwrap().audio > before,
                "sent without audio"
            );
        }
        if !self.timeline.keyed.iter().any(|k| k.slot == slot) {
            let obs = self.observations.lock().unwrap();
            assert!(
                !obs.ptt[ptt_start..].contains(&true),
                "refused requests keyed PTT"
            );
            assert_eq!(obs.audio, before, "refused requests produced playback");
        }
        let more = self.pump_qso_events();
        self.deferred_pending = more;
        slot
    }

    async fn record_settlement(
        &mut self,
        slot: u64,
        p: PendingTx,
        success: bool,
        ptt_start: usize,
    ) {
        if success {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if self.observations.lock().unwrap().ptt.last() == Some(&false) {
                    break;
                }
                assert!(Instant::now() < deadline, "sent without observed PTT-off");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let obs = self.observations.lock().unwrap();
            assert!(
                obs.ptt[ptt_start..].contains(&true),
                "sent without a new PTT-on"
            );
            assert_eq!(obs.ptt.last(), Some(&false), "sent without PTT-off");
            self.timeline.keyed.push(KeyedTx {
                slot,
                text: p.text,
                freq_hz: p.frequency_offset,
                qso_id: p.qso_id,
                ptt_keyed: true,
                ptt_released: true,
            });
        } else {
            self.timeline.dropped.push(DroppedTx {
                slot,
                text: p.text,
                qso_id: p.qso_id,
                reason: DropReason::WorkerRefused,
            });
        }
    }

    pub async fn wait_for_audio(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if self.observations.lock().unwrap().audio >= count {
                return;
            }
            assert!(Instant::now() < deadline, "worker audio timeout");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    pub async fn autonomous_pounce(
        &self,
        dx: &str,
        dx_freq_hz: f64,
        dx_parity: Option<SlotParity>,
    ) -> String {
        let active = self.manager.active_tx_offsets().await;
        let (tx_off, partner) = compute_manual_tx_offset(dx_freq_hz, false, 0, &active);
        self.manager
            .respond_to_cq_with(
                dx.to_string(),
                tx_off,
                dx_parity,
                CallInitiation::Auto,
                partner,
                false,
                None,
            )
            .await
            .expect("autonomous respond_to_cq_with")
            .to_string()
    }

    pub async fn inject_decode(&self, text: &str, freq_hz: f64) {
        let msg = pancetta_qso::utils::parse_ft8_message(text, &self.our_callsign)
            .unwrap_or_else(|e| panic!("parse_ft8_message('{text}'): {e}"));
        self.manager
            .process_message(msg, text.to_string(), freq_hz, Some(-12.0))
            .await
            .expect("process_message");
    }

    pub async fn shutdown(&mut self) {
        self.worker.shutdown().await;
        self.stop_consumers.store(true, Ordering::Release);
        for task in self.consumers.drain(..) {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("consumer timeout")
                .expect("consumer join");
        }
        assert_eq!(self.rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
    }

    pub fn home(&self) -> &std::path::Path {
        self.home.path()
    }
}

impl Drop for CoordSim {
    fn drop(&mut self) {
        self.stop_consumers.store(true, Ordering::Release);
    }
}

#[derive(Default, Debug)]
pub struct Observations {
    pub ptt: Vec<bool>,
    pub audio: usize,
    pub diagnostics: Vec<String>,
    pub settlements: Vec<(String, Option<pancetta_qso::TxAttemptId>, bool)>,
}

#[derive(Debug, Clone)]
pub struct PendingTx {
    pub text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
    pub tx_parity: Option<SlotParity>,
    pub origin: pancetta_lib::message_bus::TxOrigin,
    pub remote_client_key_id: Option<String>,
    pub attempt_id: Option<pancetta_qso::TxAttemptId>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn current_policy(p: &Arc<AtomicU8>) -> TxPolicy {
    TxPolicy::from_u8(p.load(Ordering::Acquire))
}
