//! Internal access to the production TX worker for device-free integration tests.
use super::*;
use crate::message_bus::{DurableInbox, MessageType};

pub struct WorkerHarness {
    coordinator: ApplicationCoordinator,
    outcomes: super::qso::HarnessOutcomes,
    pub bus: MessageBus,
    pub active_tx_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    pub active_tx_offsets: Arc<std::sync::RwLock<std::collections::HashMap<String, f64>>>,
    pub tx_policy: Arc<std::sync::atomic::AtomicU8>,
    pub decode_window: Arc<tokio::sync::RwLock<DecodeWindowConfig>>,
    pub active_protocol_mode: Arc<std::sync::atomic::AtomicU8>,
    pub active_slot_ns: Arc<std::sync::atomic::AtomicI64>,
    pub active_decode_phase_ns: Arc<std::sync::atomic::AtomicI64>,
    pub current_decode_effort: Arc<std::sync::atomic::AtomicU8>,
    pub resolved_hardware_tier: Arc<std::sync::atomic::AtomicU8>,
    pub operating_frequency_hz: Arc<std::sync::atomic::AtomicU64>,
    pub split_tx_frequency_hz: Arc<std::sync::atomic::AtomicU64>,
    pub remote_tx_arm: Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,

    pub hamlib: crossbeam_channel::Receiver<ComponentMessage>,
    pub audio: crossbeam_channel::Receiver<ComponentMessage>,
    pub qso: Arc<DurableInbox>,
    pub autonomous: Arc<DurableInbox>,
    pub tui: crossbeam_channel::Receiver<ComponentMessage>,
}

impl WorkerHarness {
    pub async fn new(
        config: pancetta_config::Config,
        home: std::path::PathBuf,
        arm_clock: Arc<dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,
            true,
            None,
            None,
            None,
            1500.0,
            shutdown,
            Vec::new(),
            home.join("config.toml"),
            true,
            Some(home.clone()),
        )
        .await
        .expect("coordinator");
        coordinator.pancetta_home_override = Some(home);
        coordinator
            .hamlib_command_loop_ready
            .store(true, Ordering::Release);
        let bus = coordinator.message_bus.clone();
        let hamlib = bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap()
            .1;
        let audio = bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap()
            .1;
        let qso = Arc::new(bus.durable_inbox(ComponentId::Qso).await.unwrap());
        let autonomous = Arc::new(bus.durable_inbox(ComponentId::Autonomous).await.unwrap());
        let tui = bus.get_or_create_channel(ComponentId::Tui).await.unwrap().1;
        coordinator
            .start_transmitter_component_with_clock(
                arm_clock,
                pancetta_core::time::system_utc_clock(),
            )
            .await
            .unwrap();
        Self {
            active_tx_qsos: coordinator.active_tx_qsos.clone(),
            active_tx_offsets: coordinator.active_tx_offsets.clone(),
            tx_policy: coordinator.tx_policy.clone(),
            decode_window: coordinator.decode_window.clone(),
            active_protocol_mode: coordinator.active_protocol_mode.clone(),
            active_slot_ns: coordinator.active_slot_ns.clone(),
            active_decode_phase_ns: coordinator.active_decode_phase_ns.clone(),
            current_decode_effort: coordinator.current_decode_effort.clone(),
            resolved_hardware_tier: coordinator.resolved_hardware_tier.clone(),
            operating_frequency_hz: coordinator.operating_frequency_hz.clone(),
            split_tx_frequency_hz: coordinator.split_tx_frequency_hz.clone(),
            remote_tx_arm: coordinator.remote_tx_arm.clone(),
            outcomes: Default::default(),
            coordinator,
            bus,
            hamlib,
            audio,
            qso,
            autonomous,
            tui,
        }
    }

    pub fn wire_manager(&self, manager: &mut pancetta_qso::QsoManager) {
        let c = &self.coordinator;
        manager.set_dial_frequency_source(c.operating_frequency_hz.clone());
        manager.set_split_tx_frequency_source(c.split_tx_frequency_hz.clone());
        manager.set_tx_freq_mode_source(c.tx_freq_mode.clone());
        manager.set_ptt_active_source(c.ptt_active.clone());
        manager.set_ptt_sync_gate_source(c.ptt_sync_gate.clone());
        manager.set_relocation_epoch_source(c.relocation_epochs.clone());
        manager.set_tx_policy_source(c.tx_policy.clone());
        manager.set_active_slot_ns_source(c.active_slot_ns.clone());
    }

    /// Call only after bus acceptance, matching the coordinator event adapter.
    pub fn accepted(&self, message: &MessageType) {
        if let MessageType::TransmitRequest {
            qso_id: Some(id),
            attempt_id,
            message_text,
            frequency_offset,
            tx_parity,
            origin,
            remote_client_key_id,
        } = message
        {
            self.coordinator.latest_tx_intent.write().unwrap().insert(
                super::active_tx_qso_key(id),
                super::LatestTxIntent {
                    attempt_id: *attempt_id,
                    message_text: message_text.clone(),
                    frequency_offset: *frequency_offset,
                    tx_parity: *tx_parity,
                    origin: *origin,
                    remote_client_key_id: remote_client_key_id.clone(),
                    request_queued: true,
                },
            );
        }
    }

    pub async fn apply_outcome(
        &self,
        manager: &pancetta_qso::QsoManager,
        outcome: &crate::message_bus::QsoMessage,
    ) {
        self.outcomes.apply(manager, outcome).await;
    }

    pub async fn shutdown(&mut self) {
        self.coordinator
            .shutdown_signal
            .store(true, Ordering::Release);
        for (_, task) in self.coordinator.named_task_handles.drain(..) {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("worker shutdown timeout")
                .expect("worker join")
                .expect("worker result");
        }
        self.coordinator.pending_ptt_off.settle().await;
    }
}

impl Drop for WorkerHarness {
    fn drop(&mut self) {
        self.coordinator
            .shutdown_signal
            .store(true, Ordering::Release);
    }
}
