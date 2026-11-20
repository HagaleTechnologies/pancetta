//! FT8 transmitter component.
//!
//! Owns the encode → modulate → slot-aligned audio output → PTT key/unkey
//! sequence. Runs on the message-bus channel for `MessageType::TransmitRequest`
//! arriving from the QSO state machine, the autonomous operator, or the
//! TUI command-forwarding loop.
//!
//! The `PttGuard` RAII helper ensures the radio is keyed back to RX even
//! if the transmitter task is cancelled mid-transmission — without it a
//! panic in the audio output path would leave the rig stuck on TX.

use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{
    AckState, ComponentId, ComponentMessage, DeliveryAck, MessageBus, MessageType,
    PlaybackStartAck, PlaybackStartState, PlaybackStartWaiter, SendIfOutcome,
};

use super::tx_log::{TxFrameLog, TxFrameRecord, TX_LOG_ACK_WAIT};
use super::tx_outcome::{self, AutonomousOutcome, TxFrame};

mod worker;
#[cfg(test)]
pub(crate) use worker::TxInbox;
use worker::*;
pub use worker::{resolve_required_parity, schedule_tx, TxSchedule};
pub(crate) use worker::{tx_attempts_count, tx_defers_count, TxRequeue};
mod gate;
use gate::*;
pub use gate::{
    remote_tx_permitted, remote_tx_permitted_for, tx_band_plan_mute_reason, tx_rf_for_gate,
    tx_signal_span_hz,
};
pub(crate) use gate::{tx_hard_mute_reason, tx_mute_causes, TxMuteCause};
mod ptt;
#[cfg(test)]
pub(crate) use ptt::KeyDownProbe;
use ptt::*;
pub(crate) use ptt::{send_ptt_command, PendingPttOff, SAFETY_SEND_RETRY_DELAYS};
mod bundle;
use bundle::*;
#[cfg(test)]
pub(crate) use bundle::{
    coalesce_backlog_into, effective_max_concurrent_qsos, live_fox_admitted_count,
    supersede_max_concurrent_qsos,
};
pub use bundle::{
    coalesce_transmit_requests, coalesce_transmit_requests_with_fox_priority, CoalesceEntry,
    CoalesceOutcome,
};
mod supersede;
use supersede::*;
mod report;
use report::*;
pub(crate) use report::{emit_diagnostic, emit_diagnostic_full};

mod drive_calibration;
#[cfg(test)]
#[path = "tx/tests/drive_calibration_tests.rs"]
mod drive_calibration_tests;
mod fox_multiplex;

#[cfg(test)]
#[path = "tx/tests/fox_multiplex_tests.rs"]
mod fox_multiplex_tests;

#[cfg(test)]
#[path = "tx/tests/on_ptt_wait_tests.rs"]
mod on_ptt_wait_tests;

/// Outcome of `interruptible_sleep_or_supersede`.
///
/// Deliberately does NOT derive `PartialEq`/`Eq`: `Superseded` carries the
/// full `MessageType`, and `MessageType` itself does not (and should not,
/// without a much wider cross-crate change — see
/// docs/superpowers/sdd/task-5-report.md) derive `PartialEq`. Tests that
/// need to assert on a `Superseded` payload pattern-match and compare
/// individual fields instead of using `assert_eq!` on the whole enum.
// `Superseded(MessageType)` is intentionally not boxed: this keeps the
// public shape exactly as specified (a later task's re-key consumer wants
// the full `MessageType`, including `MultiTransmitRequest`'s `items` list —
// see task-5-report.md), at the cost of a size-difference lint between
// variants. Silenced rather than changed.
#[cfg(test)]
#[path = "tx/tests/on_sleep_outcome_tests.rs"]
mod on_sleep_outcome_tests;
#[cfg(test)]
#[path = "tx/tests/tx_inbox_tests.rs"]
mod tx_inbox_tests;

#[cfg(test)]
#[path = "tx/tests/interruptible_sleep_tests.rs"]
mod interruptible_sleep_tests;

#[cfg(test)]
#[path = "tx/tests/band_plan_gate_tests.rs"]
mod band_plan_gate_tests;

/// PAN-208: a `custom_bands` entry with two or more `ranges` leaves gaps
/// inside the band. The occupied signal `[floor(low), ceil(low + span)]`
/// must fit inside ONE allowed range; checking the low and high edges
/// independently let a frame whose base tone sat in one range and top
/// tone in the next key across the excluded gap (PR #446 Codex review).
#[cfg(test)]
#[path = "tx/tests/band_plan_gap_tests.rs"]
pub(crate) mod band_plan_gap_tests;

#[cfg(test)]
#[path = "tx/tests/effective_max_concurrent_qsos_tests.rs"]
mod effective_max_concurrent_qsos_tests;
#[cfg(test)]
#[path = "tx/tests/tx_hard_mute_reason_tests.rs"]
mod tx_hard_mute_reason_tests;

#[cfg(test)]
#[path = "tx/tests/fox_admitted_ceiling_tests.rs"]
mod fox_admitted_ceiling_tests;

impl SlotTimingContext {
    /// The mode-scaled late cap, [`tx_late_max_ms_effective`].
    fn late_cap_ms(&self) -> u64 {
        tx_late_max_ms_effective(
            self.active_protocol,
            self.tx_late_max_ms.load(Ordering::Acquire),
        )
    }

    /// The PTT engage instant: `target_slot` minus `ptt_lead_ms`.
    fn ptt_target(
        &self,
        target_slot: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        target_slot - chrono::Duration::milliseconds(self.ptt_lead_ms as i64)
    }
}

#[cfg(test)]
impl SlotTimingContext {
    /// Test-only FT8 context: the supersede tests' old explicit arguments,
    /// with the default 80 ms `ptt_lead_ms` (unused by the supersede path).
    fn ft8_for_test(
        tx_late_max_ms: u64,
        sample_rate: u32,
        slot_ns: i64,
        tx_self_parity: pancetta_config::station::TxSelfParity,
        request_received_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            active_protocol: pancetta_ft8::Protocol::Ft8,
            tx_late_max_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(tx_late_max_ms)),
            tx_self_parity,
            ptt_lead_ms: 80,
            sample_rate,
            slot_ns,
            request_received_at,
            utc_clock: pancetta_core::time::system_utc_clock(),
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/slot_timing_context_tests.rs"]
mod slot_timing_context_tests;

#[cfg(test)]
#[path = "tx/tests/schedule_tx_tests.rs"]
mod schedule_tx_tests;

/// Regression coverage for the silent-TX-failure gap: an encode/modulate
/// error used to only log a WARN and send a TransmitComplete no consumer
/// reads, so the operator had zero visible signal — the QSO just sat until
/// the watchdog timed out, indistinguishable from the DX not answering.
/// `emit_tx_failure_diagnostic` is the shared helper wired into every
/// genuine TX-attempt-failure site in this file (deliberate policy/security
/// skips are NOT routed through it — see its doc comment). This pins its
/// exact DiagnosticEvent contract directly, independent of the full
/// encode/modulate/PTT pipeline the call sites sit inside.
#[cfg(test)]
#[path = "tx/tests/tx_failure_diagnostic_tests.rs"]
mod tx_failure_diagnostic_tests;

#[cfg(test)]
#[path = "tx/tests/disarm_interrupt_signal_tests.rs"]
mod disarm_interrupt_signal_tests;

#[cfg(test)]
#[path = "tx/tests/supersede_rekey_tests.rs"]
mod supersede_rekey_tests;
#[cfg(test)]
#[path = "tx/tests/tx_frame_logged_tests.rs"]
mod tx_frame_logged_tests;

#[cfg(test)]
#[path = "tx/tests/coalesce_tests.rs"]
mod coalesce_tests;
#[cfg(test)]
#[path = "tx/tests/protocol_tx_tests.rs"]
mod protocol_tx_tests;

/// Unit tests for the remote-TX arm gate helper (`remote_tx_permitted`).
///
/// These lock the safety-critical fail direction: a fresh (unarmed) `ArmState`
/// denies, a fully-armed+consented state permits, and a **poisoned** arm lock
/// **fails CLOSED** (denies) — the opposite of `tx_qso_is_live`'s fail-open.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_tests.rs"]
mod remote_arm_gate_tests;

/// Unit tests for PAN-91's client-identity-bound gate (`remote_tx_permitted_for`).
///
/// These lock the fix's actual invariant: a frame bound to one client must
/// key TX only while THAT client is armed — not merely while *some* client
/// is armed, which is exactly the gap that let a different peer's arm
/// authorize a QSO it never requested.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_identity_tests.rs"]
mod remote_arm_gate_identity_tests;

#[cfg(test)]
#[path = "tx/tests/arm_snapshot_attribution_tests.rs"]
mod arm_snapshot_attribution_tests;

#[cfg(test)]
#[path = "tx/tests/tx_counter_tests.rs"]
mod tx_counter_tests;

#[cfg(test)]
#[path = "tx/tests/classifier_tests.rs"]
mod classifier_tests;

/// PAN-115: the two Gherkin scenarios end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`: a request made
/// while the rig CAT link is lost, or while the audio output is dead, is
/// refused at the key-time gate with no `SetPtt{true}` ever reaching Hamlib.
#[cfg(test)]
pub(super) mod pan115_worker_gate_tests {
    use super::super::hamlib::RigConnState;
    use super::super::ApplicationCoordinator;
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, RigControlMessage, TxOrigin,
    };
    use crossbeam_channel::Receiver;
    use pancetta_config::Config;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // `pub(crate)` fields/helpers: shared with the sibling PAN-118 worker-gate
    // test module (`pan118_worker_band_plan_tests`), which reuses this harness
    // exactly as the PAN-115 plan's shape intends, and with the PAN-136
    // bundle-binding module (`pan136_bundle_binding_worker_tests`).
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
        build_worker_harness_in_home(cfg, prepare, arm_clock, utc_clock, register_rig, None).await
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
            .start_transmitter_component_with_clock(arm_clock, utc_clock.clone())
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
                Arc::new(move || base + chrono::Duration::from_std(start.elapsed()).unwrap());
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
                            crate::message_bus::QsoMessage::TxFrameScheduled {
                                target_slot, ..
                            },
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
                let lead = (target - clock()).num_milliseconds();
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
            let now = pancetta_core::time::slot_grid_origin() + chrono::Duration::seconds(2);
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
        let now = pancetta_core::time::slot_grid_origin() + chrono::Duration::seconds(2);
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
        let (success, _, keyed) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
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
        let now = pancetta_core::time::slot_grid_origin() + chrono::Duration::seconds(2);
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
            let now = pancetta_core::time::slot_grid_origin() + chrono::Duration::seconds(2);
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
        let (completions, ptt_on) =
            qso_completions(&h, &[&id_text], 1, Duration::from_secs(60)).await;
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
        while Instant::now() < deadline && !completions.iter().any(|(t, _)| t == "K1ABC N0CALL -10")
        {
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
}

#[cfg(test)]
#[path = "tx/tests/report_transmit_complete_tests.rs"]
mod report_transmit_complete_tests;
/// PAN-118: the band-plan Gherkin end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`, reusing the
/// PAN-115 harness: a transmission whose absolute TX RF (effective
/// split-aware dial + absolute audio offset) falls outside the configured
/// amateur band plan is refused before `SetPtt{true}` ever reaches Hamlib.
/// The dial-0 "not yet known" sentinel (mock/rig-disabled dev modes,
/// `--test-tx`, replay seed) skips the check, keeping those modes
/// byte-identical.
#[cfg(test)]
mod pan118_worker_band_plan_tests {
    use super::pan115_worker_gate_tests::{
        await_completion, await_ptt_on, request, worker_harness, Harness,
    };
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType, TxOrigin};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// 20m: in-band under the default region-2 plan.
    const IN_BAND_DIAL_HZ: u64 = 14_074_000;
    /// 15.000 MHz: between 20m and 17m — inside no amateur allocation.
    const OUT_OF_BAND_DIAL_HZ: u64 = 15_000_000;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn out_of_band_single_tx_is_refused_before_ptt() {
        let h = worker_harness().await;
        // The shared RX-dial atomic, as the Hamlib poll loop would set it.
        h.coordinator
            .operating_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

        request(&h, "CQ N0CALL FN20").await;
        let (completed, diagnostics, ptt_on) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
        assert_eq!(
            completed,
            Some(false),
            "an out-of-band TX must complete as failed"
        );
        assert!(
            !ptt_on,
            "no SetPtt{{true}} may reach Hamlib for an out-of-band TX"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains(
                "TX blocked (TX frequency 15001500 Hz is outside the configured amateur band plan)"
            )),
            "the admission gate must name the absolute RF, got {diagnostics:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_band_single_tx_still_keys() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);

        request(&h, "CQ N0CALL FN20").await;
        assert!(
            await_ptt_on(&h, Duration::from_secs(40)).await,
            "an in-band TX must still key (guard against over-blocking)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_dial_zero_still_keys() {
        let h = worker_harness().await;
        // Dial never set: the unknown-0 sentinel must skip the band-plan
        // check (mock/rig-disabled dev modes, --test-tx, replay seed).
        request(&h, "CQ N0CALL FN20").await;
        assert!(
            await_ptt_on(&h, Duration::from_secs(40)).await,
            "a TX with an unknown (0) dial must still key"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn split_out_of_band_is_refused_even_with_in_band_rx_dial() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        // Split active: the station transmits on the split dial, which is
        // out of band — the gate must resolve the effective TX dial, not
        // the in-band RX dial.
        h.coordinator
            .split_tx_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

        request(&h, "CQ N0CALL FN20").await;
        let (completed, diagnostics, ptt_on) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
        assert_eq!(
            completed,
            Some(false),
            "a split TX landing out of band must complete as failed"
        );
        assert!(!ptt_on, "no SetPtt(true) for an out-of-band split TX");
        assert!(
            diagnostics.iter().any(|d| d.contains(
                "TX blocked (TX frequency 15001500 Hz is outside the configured amateur band plan)"
            )),
            "the gate must name the split-resolved absolute RF, got {diagnostics:?}"
        );
    }

    /// Park the worker at the PTT mutex, then move the dial out of band.
    /// The final pre-PTT check must observe the new dial and refuse to key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dial_moved_out_of_band_before_key_is_refused() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);

        let held = h.coordinator.ptt_sync_gate.lock().await;
        let text = "CQ N0CALL FN20";
        request(&h, text).await;
        tokio::time::timeout(
            Duration::from_secs(40),
            h.coordinator.tx_before_ptt_gate.notified(),
        )
        .await
        .expect("worker must reach the pre-PTT gate before the dial moves");
        assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
        h.coordinator
            .operating_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
        drop(held);

        let (completed, diagnostics, ptt_on) =
            await_completion(&h, text, Duration::from_secs(45)).await;
        assert_eq!(
            completed,
            Some(false),
            "a frame whose dial moved out of band before key must complete as failed"
        );
        assert!(!ptt_on, "no SetPtt(true) after the dial moved out of band");
        assert!(
            diagnostics.iter().any(|d| {
                d.contains("TX blocked at final pre-PTT check")
                    && d.contains("outside the configured amateur band plan")
            }),
            "the final pre-PTT gate must report the band-plan refusal, got {diagnostics:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tune_request_out_of_band_is_refused() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_refusal = false;
        let mut ptt_on = false;
        while Instant::now() < deadline && !saw_refusal {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                ptt_on |= matches!(
                    msg.message_type,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: true
                    })
                );
            }
            while let Ok(msg) = h.tui_rx.try_recv() {
                if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                    if text.contains("outside the configured amateur band plan")
                        && text.contains("tune")
                    {
                        saw_refusal = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_refusal,
            "an out-of-band tune must be refused with a diagnostic"
        );
        assert!(!ptt_on, "no SetPtt(true) for an out-of-band tune");

        // Control: the same tune on an in-band dial still keys.
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Ft8Transmitter,
                MessageType::TuneRequest {
                    duration_secs: 2,
                    tone_offset_hz: 1500.0,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        assert!(
            await_ptt_on(&h, Duration::from_secs(10)).await,
            "an in-band tune must still key"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tune_dial_moved_out_of_band_before_ptt_is_refused_at_last_instant() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        // Hold the PAN-143 PTT sync gate so the worker parks inside
        // `PttGuard::new` — after the tune's admission band-plan check
        // passed on the in-band dial, before the PTT-on send.
        let ptt_sync_gate = h.coordinator.ptt_sync_gate.clone();
        let held = ptt_sync_gate.lock().await;
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Ft8Transmitter,
                MessageType::TuneRequest {
                    duration_secs: 1,
                    tone_offset_hz: 1500.0,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        // Wait for the worker to dequeue the request. From there the tune's
        // admission checks and tone build run without yielding until the
        // held gate, so a short settle covers them.
        let (worker_inbox, _) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Ft8Transmitter)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker_inbox.is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            worker_inbox.is_empty(),
            "the TX worker must dequeue the tune request"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut diagnostics = Vec::new();
        while let Ok(msg) = h.tui_rx.try_recv() {
            if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                diagnostics.push(text);
            }
        }
        assert!(
            diagnostics.iter().all(|d| !d.contains("TX blocked")),
            "the in-band tune must pass admission before the dial moves: {diagnostics:?}"
        );

        // The effective TX dial leaves the band plan while the worker waits.
        h.coordinator
            .operating_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);
        drop(held);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_refusal = false;
        let mut ptt_on = false;
        while Instant::now() < deadline && !saw_refusal && !ptt_on {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                ptt_on |= matches!(
                    msg.message_type,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: true
                    })
                );
            }
            while let Ok(msg) = h.tui_rx.try_recv() {
                if let MessageType::DiagnosticEvent { text, .. } = msg.message_type {
                    saw_refusal |= text.contains("final pre-PTT check")
                        && text.contains("outside the configured amateur band plan")
                        && text.contains("tune");
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !ptt_on,
            "no SetPtt(true) for a tune whose dial left the band plan before PTT"
        );
        assert!(
            saw_refusal,
            "the tune must be refused at the final pre-PTT check with a band-plan diagnostic"
        );
    }

    /// Send one two-item local bundle through the real worker.
    async fn send_bundle(h: &Harness, a_offset: f64, b_offset: f64) {
        use crate::message_bus::TransmitRequestItem;
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    items: vec![
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL FN20".to_string(),
                            frequency_offset: a_offset,
                            qso_id: None,
                            origin: crate::message_bus::TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL EN27".to_string(),
                            frequency_offset: b_offset,
                            qso_id: None,
                            origin: crate::message_bus::TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                    ],
                    tx_parity: None,
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    /// Drain until the TransmitComplete for EVERY text in `texts` has
    /// arrived on Autonomous (bundles complete per item, back-to-back — a
    /// single-text waiter would discard its siblings' completions while
    /// polling), returning each text's `success` in order plus whether any
    /// `SetPtt{true}` reached Hamlib.
    async fn await_bundle_completions(
        h: &Harness,
        texts: &[&str],
        within: Duration,
    ) -> (Vec<Option<bool>>, bool) {
        let deadline = Instant::now() + within;
        let mut ptt_on = false;
        let mut done: Vec<Option<bool>> = vec![None; texts.len()];
        while Instant::now() < deadline && done.iter().any(|d| d.is_none()) {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                ptt_on |= matches!(
                    msg.message_type,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: true
                    })
                );
            }
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    message_text,
                    ..
                } = msg.message_type
                {
                    if let Some(slot) = texts.iter().position(|t| *t == message_text) {
                        if done[slot].is_none() {
                            done[slot] = Some(success);
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        while let Ok(msg) = h.hamlib_rx.try_recv() {
            ptt_on |= matches!(
                msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: true
                })
            );
        }
        (done, ptt_on)
    }

    /// PAN-118 Gherkin, bundle discrimination: at the 20m upper edge
    /// (14.348 MHz dial), a +500 Hz item lands in-band while a +2,500 Hz
    /// sibling lands past the edge (14.3505 MHz) — only the offending
    /// item is dropped; the in-privilege sibling still keys.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_drops_only_the_out_of_band_item() {
        let h = worker_harness().await;
        // 20m region-2 upper edge is 14.350 MHz.
        h.coordinator
            .operating_frequency_hz
            .store(14_348_000, Ordering::Relaxed);

        send_bundle(&h, 500.0, 2_500.0).await;
        // The out-of-band item is dropped at admission with a failed
        // completion naming its absolute RF.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_drop_diag = false;
        while Instant::now() < deadline && !saw_drop_diag {
            saw_drop_diag |= h
                .tui_rx
                .try_iter()
                .any(|msg| matches!(msg.message_type, MessageType::DiagnosticEvent { text, .. } if text.contains("dropping out-of-band multi-TX item") && text.contains("14350500")));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            saw_drop_diag,
            "the dropped item's diagnostic must name 14350500 Hz"
        );
        let (done, ptt_on) = await_bundle_completions(
            &h,
            &["CQ N0CALL FN20", "CQ N0CALL EN27"],
            Duration::from_secs(50),
        )
        .await;
        assert_eq!(
            done[1],
            Some(false),
            "the out-of-band item must complete as failed"
        );
        assert_eq!(
            done[0],
            Some(true),
            "the in-band sibling must complete successfully"
        );
        assert!(ptt_on, "the in-band sibling must key PTT");
    }

    /// Control: both offsets in-band on a normal 20m dial — both items key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_fully_in_band_keys_all_items() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Relaxed);

        send_bundle(&h, 500.0, 2_500.0).await;
        let (done, ptt_on) = await_bundle_completions(
            &h,
            &["CQ N0CALL FN20", "CQ N0CALL EN27"],
            Duration::from_secs(50),
        )
        .await;
        assert_eq!(done[0], Some(true), "item A must complete successfully");
        assert_eq!(done[1], Some(true), "item B must complete successfully");
        assert!(ptt_on, "the bundle must key");
    }

    /// Every item out of privilege (dial between bands): the whole bundle
    /// is refused — nothing keys, both items fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_fully_out_of_band_keys_nothing() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(OUT_OF_BAND_DIAL_HZ, Ordering::Relaxed);

        send_bundle(&h, 500.0, 2_500.0).await;
        let (done, ptt_on) = await_bundle_completions(
            &h,
            &["CQ N0CALL FN20", "CQ N0CALL EN27"],
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(done[0], Some(false), "item A must complete as failed");
        assert_eq!(done[1], Some(false), "item B must complete as failed");
        assert!(!ptt_on, "no SetPtt(true) for a fully out-of-band bundle");
    }
}

/// PAN-202: key-down reaches Hamlib's queue while another component's
/// channel registration holds the bus table, through the real TX worker over
/// a real `MessageBus`, at every key-down site (single TX, bundle, tune), plus
/// the shared helper the TUI's manual PTT uses.
#[cfg(test)]
mod pan202_key_down_worker_tests {
    use super::pan115_worker_gate_tests::{
        await_completion, await_ptt_on, request, worker_harness, Harness,
    };
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, TransmitRequestItem, TxOrigin,
    };
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// Below every mode's effective key-time cap (FT2 ≈ 640 ms, FT4 1500 ms,
    /// FT8 3000 ms at the default `tx_late_max_ms`).
    const KEY_DOWN_BOUND: Duration = Duration::from_millis(500);

    /// 20m: in-band under the default region-2 plan.
    const IN_BAND_DIAL_HZ: u64 = 14_074_000;

    /// The worker has passed every final gate and is parked at key-down by
    /// the armed probe. Hold the bus table the way an in-flight
    /// `create_channel` does, let the worker key, and report whether PTT-on
    /// reached Hamlib within the bound while the table was still held.
    async fn keys_down_while_table_held(h: &Harness) -> bool {
        tokio::time::timeout(
            Duration::from_secs(40),
            h.coordinator.key_down_probe.reached.notified(),
        )
        .await
        .expect("worker must pass every final gate and reach key-down");
        let hold = h.coordinator.message_bus.hold_channel_table().await;
        h.coordinator.key_down_probe.release.notify_one();
        let keyed = await_ptt_on(h, KEY_DOWN_BOUND).await; // try_recv only: no bus lock
        drop(hold);
        keyed
    }

    /// One two-item local bundle, built like
    /// `pan118_worker_band_plan_tests::send_bundle`.
    async fn send_bundle(h: &Harness, a_offset: f64, b_offset: f64) {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    items: vec![
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL FN20".to_string(),
                            frequency_offset: a_offset,
                            qso_id: None,
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL EN27".to_string(),
                            frequency_offset: b_offset,
                            qso_id: None,
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                    ],
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
    async fn single_tx_keys_down_while_a_registration_holds_the_bus_table() {
        let h = worker_harness().await;
        h.coordinator
            .key_down_probe
            .armed
            .store(true, Ordering::Release);
        request(&h, "CQ N0CALL FN20").await;
        assert!(
            keys_down_while_table_held(&h).await,
            "single-TX key-down waited on the table"
        );
        let (completed, _, _) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(20)).await;
        assert_eq!(completed, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_keys_down_while_a_registration_holds_the_bus_table() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        h.coordinator
            .key_down_probe
            .armed
            .store(true, Ordering::Release);
        send_bundle(&h, 500.0, 2_500.0).await;
        assert!(
            keys_down_while_table_held(&h).await,
            "bundle key-down waited on the table"
        );
        let (completed, _, _) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(50)).await;
        assert_eq!(completed, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tune_keys_down_while_a_registration_holds_the_bus_table() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        h.coordinator
            .key_down_probe
            .armed
            .store(true, Ordering::Release);
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Ft8Transmitter,
                MessageType::TuneRequest {
                    duration_secs: 1,
                    tone_offset_hz: 1500.0,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        assert!(
            keys_down_while_table_held(&h).await,
            "tune key-down waited on the table"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_ptt_command_enqueues_both_states_while_the_table_is_held() {
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let (_s, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let _hold = bus.hold_channel_table().await;
        for state in [true, false] {
            assert_eq!(
                super::send_ptt_command(&bus, ComponentId::Tui, state, || true),
                crate::message_bus::SendIfOutcome::Enqueued
            );
            let msg = rx.try_recv().unwrap();
            assert_eq!(msg.source, ComponentId::Tui);
            assert!(matches!(
                msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: s })
                    if s == state
            ));
        }
    }
}

/// PAN-136: a multi-TX bundle whose item is re-bound during the pre-PTT wait
/// (Step 4b-pivot swaps the item's binding with its payload) is gated under
/// the RE-FOLDED binding, through the real TX worker over a real `MessageBus`.
/// Before PAN-136 the bundle kept its coalesce-time binding: a `Local` bundle
/// keyed a frame now bound to an unarmed remote client, and client A's bundle
/// aired client B's frame under A's gate.
#[cfg(test)]
mod pan136_bundle_binding_worker_tests {
    use super::pan115_worker_gate_tests::{worker_harness, Harness};
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, QsoMessage, RigControlMessage,
        TransmitRequestItem, TxOrigin,
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
                if let MessageType::QsoMessage(q @ QsoMessage::TxFrameNotSent { .. }) =
                    msg.message_type
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
        let now_ms = chrono::Utc::now().timestamp_millis();
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
            c.diagnostics.iter().any(|d| d
                .contains("Remote TX denied (not armed/permitted): multi-TX bundle of 2 items")),
            "{:?}",
            c.diagnostics
        );
    }
}

/// PAN-208: the gap-straddling Gherkin end to end, through the real TX
/// worker with a two-range `custom_bands` entry for 20m in its config. A
/// frame whose base tone sits in the first range and top tone in the second
/// is refused before `SetPtt{true}` reaches Hamlib; a frame wholly inside
/// one range still keys.
#[cfg(test)]
mod pan208_worker_band_plan_tests {
    use super::band_plan_gap_tests::gapped_plan;
    use super::pan115_worker_gate_tests::{
        await_completion, await_ptt_on, request, worker_harness_with_config, Harness,
    };
    use pancetta_config::Config;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// The worker snapshots the band plan at spawn, so it goes in the config.
    async fn gapped_plan_harness() -> Harness {
        let mut cfg = Config::default();
        cfg.rig.frequency.band_plan = gapped_plan();
        worker_harness_with_config(cfg).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gap_straddling_custom_plan_tx_is_refused_before_ptt() {
        let h = gapped_plan_harness().await;
        // Dial + 1,500 Hz audio offset = base tone 14,070,990 Hz; the FT8
        // top tone lands at 14,071,033.75 Hz, across the excluded gap.
        h.coordinator
            .operating_frequency_hz
            .store(14_069_490, Ordering::Relaxed);

        request(&h, "CQ N0CALL FN20").await;
        let (completed, diagnostics, ptt_on) =
            await_completion(&h, "CQ N0CALL FN20", Duration::from_secs(5)).await;
        assert_eq!(
            completed,
            Some(false),
            "a gap-straddling TX must complete as failed"
        );
        assert!(
            !ptt_on,
            "no SetPtt{{true}} may reach Hamlib for a gap-straddling TX"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains(
                "TX blocked (TX signal 14070990-14071034 Hz extends outside the configured amateur band plan)"
            )),
            "the admission gate must name the occupied range, got {diagnostics:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fully_inside_custom_plan_tx_still_keys() {
        let h = gapped_plan_harness().await;
        // Base tone 14,071,100 Hz; top tone 14,071,143.75 Hz — one range.
        h.coordinator
            .operating_frequency_hz
            .store(14_069_600, Ordering::Relaxed);

        request(&h, "CQ N0CALL FN20").await;
        assert!(
            await_ptt_on(&h, Duration::from_secs(40)).await,
            "a TX wholly inside one custom range must still key"
        );
    }
}

#[cfg(test)]
#[path = "tx/tests/pan135_ptt_release_tests.rs"]
mod pan135_ptt_release_tests;
/// PAN-273: the real TX worker unkeys through `PttGuard::release`, so a
/// PTT-off dropped on a momentarily full Hamlib channel is retried, and a
/// delivered one stands the guard down (no duplicate unkey from its `Drop`).
#[cfg(test)]
mod pan273_worker_unkey_tests {
    use super::pan115_worker_gate_tests::{await_ptt_on, request, worker_harness, Harness};
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType, RigControlMessage};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    /// 20m: in-band under the default region-2 plan (as the PAN-118 tests).
    const IN_BAND_DIAL_HZ: u64 = 14_074_000;

    fn is_ptt_off(msg: &ComponentMessage) -> bool {
        matches!(
            msg.message_type,
            MessageType::RigControl(RigControlMessage::SetPtt { state: false })
        )
    }

    async fn hamlib_drops(h: &Harness) -> u32 {
        h.coordinator
            .message_bus
            .get_component_health()
            .await
            .into_iter()
            .find(|c| c.component_id == ComponentId::Hamlib)
            .map(|c| c.error_count)
            .expect("the harness registers Hamlib")
    }

    async fn request_tune(h: &Harness, duration_secs: u32) {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Tui,
                ComponentId::Ft8Transmitter,
                MessageType::TuneRequest {
                    duration_secs,
                    tone_offset_hz: 1500.0,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    /// Ticket scenario 1's setup and catch-up, after PTT-on: fill Hamlib's
    /// channel, wait for the worker's first PTT-off attempt to be dropped
    /// on it, then let Hamlib catch up (drain) and count the PTT-offs that
    /// reach it from then on.
    async fn ptt_offs_after_hamlib_catches_up(h: &Harness, within: Duration) -> usize {
        let bus = &h.coordinator.message_bus;
        let mut filled = 0usize;
        loop {
            let enqueued = bus
                .send_message_checked(ComponentMessage::new(
                    ComponentId::Tui,
                    ComponentId::Hamlib,
                    MessageType::AudioData(Vec::new()),
                    Instant::now(),
                ))
                .await
                .unwrap();
            if !enqueued {
                break;
            }
            filled += 1;
            assert!(filled <= 100_000, "Hamlib's channel never filled");
        }
        let full_at = hamlib_drops(h).await;
        let deadline = Instant::now() + within;
        while hamlib_drops(h).await == full_at {
            assert!(
                Instant::now() < deadline,
                "the worker never tried to unkey into the full channel"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Hamlib catches up. A retry can land while this drains, so count
        // PTT-offs in the backlog as well as after it.
        let mut offs = h.hamlib_rx.try_iter().filter(is_ptt_off).count();
        tokio::time::sleep(Duration::from_millis(400)).await;
        offs += h.hamlib_rx.try_iter().filter(is_ptt_off).count();
        offs
    }

    /// Ticket scenario 1 on the tune path (keys at once, no slot wait).
    /// Before PAN-273 this delivered 0 PTT-offs: the dropped send returned
    /// `Ok(())` and the guard was disarmed anyway.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tune_unkey_retries_a_dropped_ptt_off_until_hamlib_has_room() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        request_tune(&h, 2).await;
        assert!(await_ptt_on(&h, Duration::from_secs(5)).await, "tune keyed");
        let offs = ptt_offs_after_hamlib_catches_up(&h, Duration::from_secs(10)).await;
        assert_eq!(offs, 1, "the worker's retry delivers exactly one unkey");
    }

    /// Ticket scenario 2: a delivered PTT-off stands the guard down, so its
    /// `Drop` sends no second unkey and the keyed flag clears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tune_unkey_on_a_healthy_channel_sends_exactly_one_ptt_off() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        request_tune(&h, 1).await;
        assert!(await_ptt_on(&h, Duration::from_secs(5)).await, "tune keyed");
        tokio::time::sleep(Duration::from_millis(1600)).await;
        let offs = h.hamlib_rx.try_iter().filter(is_ptt_off).count();
        assert_eq!(offs, 1, "one unkey, no duplicate from the guard's Drop");
        assert!(!h.coordinator.ptt_active.load(Ordering::Acquire));
    }

    /// Ticket scenario 1 as written: a frame has finished playing and the
    /// worker unkeys at Step 9 into a full Hamlib channel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_tx_unkey_retries_a_dropped_ptt_off_after_the_frame_plays() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(IN_BAND_DIAL_HZ, Ordering::Relaxed);
        request(&h, "CQ N0CALL FN20").await;
        assert!(
            await_ptt_on(&h, Duration::from_secs(40)).await,
            "the frame keyed"
        );
        let offs = ptt_offs_after_hamlib_catches_up(&h, Duration::from_secs(40)).await;
        assert_eq!(offs, 1, "the worker's retry delivers exactly one unkey");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut completed = None;
        while completed.is_none() && Instant::now() < deadline {
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    message_text,
                    ..
                } = msg.message_type
                {
                    if message_text == "CQ N0CALL FN20" {
                        completed = Some(success);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(completed.is_some(), "the frame's TransmitComplete arrives");
    }

    /// Source pin for the two clock-step PTT hold guards (PAN-114), which
    /// need a backward wall-clock step and have no runtime seam: no unkey in
    /// the TX façade or production modules builds its own PTT-off for a
    /// best-effort `send_message`.
    /// The needle is built at runtime so this test does not match itself.
    #[test]
    fn pan273_no_best_effort_ptt_off_remains_in_tx_rs() {
        let source = [
            include_str!("tx.rs"),
            include_str!("tx/worker.rs"),
            include_str!("tx/gate.rs"),
            include_str!("tx/ptt.rs"),
            include_str!("tx/bundle.rs"),
            include_str!("tx/supersede.rs"),
            include_str!("tx/report.rs"),
        ]
        .join("\n");
        let needle = ["let ", "ptt_off_msg"].concat();
        assert!(
            !source.contains(&needle),
            "every TX-worker unkey goes through PttGuard::release (PAN-273)"
        );
        for context in [
            "Step 9 unkey",
            "Multi-TX Step 9 unkey",
            "Tune unkey",
            "Supersede",
            "Clock-step PTT hold guard",
        ] {
            let call = [".release(\"", context, "\")"].concat();
            assert!(source.contains(&call), "missing {call}");
        }
    }
}
#[cfg(test)]
#[path = "tx/tests/disarm_flush_tests.rs"]
mod disarm_flush_tests;
#[cfg(test)]
mod pan135_worker_disarm_tests {
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
}

/// PAN-122: every frame whose audio the real TX worker enqueues gets one
/// durable line in `<home>/.pancetta/tx-log.jsonl` — headless, with the TUI
/// channel full, single or bundled — and nothing else does.
#[cfg(test)]
mod pan122_tx_log_worker_tests {
    use super::super::tx_log::{
        tx_log_path, RotationPolicy, TxFrameLog, TxFrameRecord, TxLogAppend, WriterGate,
        TX_LOG_ACK_WAIT,
    };
    use super::super::ApplicationCoordinator;
    use super::pan115_worker_gate_tests::{
        await_completion, request, worker_harness, worker_harness_with, Harness,
    };
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, RigControlMessage, TransmitRequestItem,
        TxOrigin,
    };
    use pancetta_config::Config;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const TEXT: &str = "CQ N0CALL FN20";

    fn tx_log_lines(h: &Harness) -> Vec<TxFrameRecord> {
        let path = tx_log_path(h.coordinator.pancetta_home_override.as_deref());
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .lines()
                .map(|line| serde_json::from_str(line).expect("one record per line"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read {}: {e}", path.display()),
        }
    }

    async fn await_tx_log_lines(h: &Harness, n: usize, within: Duration) -> Vec<TxFrameRecord> {
        let deadline = Instant::now() + within;
        loop {
            let lines = tx_log_lines(h);
            if lines.len() >= n || Instant::now() >= deadline {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Fill `dst`'s channel until the bus refuses another message.
    /// `pub(crate)`: shared with the PAN-182 audio-delivery module, the way
    /// the PAN-115 harness is shared with this one.
    pub(crate) async fn fill_channel(h: &Harness, dst: ComponentId) -> usize {
        let mut sent = 0;
        while h
            .coordinator
            .message_bus
            .send_message_checked(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                dst,
                MessageType::TxStatus { active: false },
                Instant::now(),
            ))
            .await
            .unwrap()
        {
            sent += 1;
        }
        sent
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn headless_frame_is_durably_logged_even_when_the_tui_channel_is_full() {
        let h = worker_harness().await;
        assert!(fill_channel(&h, ComponentId::Tui).await > 0);
        let (_, audio_rx) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();

        request(&h, TEXT).await;
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut audio_enqueued = false;
        while !audio_enqueued && Instant::now() < deadline {
            while let Ok(msg) = audio_rx.try_recv() {
                audio_enqueued |= matches!(msg.message_type, MessageType::AudioOutput { .. });
            }
            while h.hamlib_rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(audio_enqueued, "the frame's audio must reach the output");

        // File visibility precedes the best-effort TUI relay attempt. Keep
        // TUI full until this frame completes, which happens after the relay.
        // Leave Autonomous unread above so an early completion is retained.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut completed = None;
        while completed.is_none() && Instant::now() < deadline {
            while h.hamlib_rx.try_recv().is_ok() {}
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    message_text,
                    qso_id: None,
                    ..
                } = msg.message_type
                {
                    if message_text == TEXT {
                        completed = Some(success);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            completed,
            Some(true),
            "the frame must complete successfully before draining TUI"
        );

        let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
        assert_eq!(lines.len(), 1, "exactly one record: {lines:?}");
        let r = &lines[0];
        assert_eq!(r.v, 1);
        assert_eq!(r.text, TEXT);
        assert_eq!(r.mode, "FT8");
        assert_eq!(r.offset_hz, 1500.0);
        assert_eq!(r.origin, TxOrigin::Local);
        assert_eq!(r.qso_id, None);
        assert_eq!(r.dial_hz, None, "dial unknown in the harness");

        // Completion proves the relay already encountered the full route;
        // draining now cannot admit a late TxFrameLogged event.
        let tui_frame_events = h
            .tui_rx
            .try_iter()
            .filter(|m| matches!(m.message_type, MessageType::TxFrameLogged { .. }))
            .count();
        assert_eq!(
            tui_frame_events, 0,
            "the TUI route was full, so the record cannot have come from it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_carries_the_effective_split_aware_dial() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Relaxed);
        h.coordinator
            .split_tx_frequency_hz
            .store(14_080_000, Ordering::Relaxed);

        request(&h, TEXT).await;
        let lines = await_tx_log_lines(&h, 1, Duration::from_secs(45)).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].dial_hz, Some(14_080_000), "split dial while split");
        // Let the first frame finish before the next request, so the second
        // is keyed on its own rather than superseding the first mid-playback.
        let (completed, _, _) = await_completion(&h, TEXT, Duration::from_secs(30)).await;
        assert_eq!(completed, Some(true));

        h.coordinator
            .split_tx_frequency_hz
            .store(0, Ordering::Relaxed);
        request(&h, "CQ N0CALL EN27").await;
        let lines = await_tx_log_lines(&h, 2, Duration::from_secs(50)).await;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1].text, "CQ N0CALL EN27");
        assert_eq!(lines[1].dial_hz, Some(14_074_000), "RX dial when simplex");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_writes_one_record_per_item_with_one_shared_timestamp() {
        let h = worker_harness().await;
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    items: vec![
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL FN20".to_string(),
                            frequency_offset: 1000.0,
                            qso_id: None,
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                        TransmitRequestItem {
                            attempt_id: None,
                            message_text: "CQ N0CALL EN27".to_string(),
                            frequency_offset: 1600.0,
                            qso_id: None,
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        },
                    ],
                    tx_parity: None,
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();

        let lines = await_tx_log_lines(&h, 2, Duration::from_secs(45)).await;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].text, "CQ N0CALL FN20");
        assert_eq!(lines[0].offset_hz, 1000.0);
        assert_eq!(lines[1].text, "CQ N0CALL EN27");
        assert_eq!(lines[1].offset_hz, 1600.0);
        assert_eq!(lines[0].utc, lines[1].utc, "one bundle, one key instant");
        assert_eq!(
            [lines[0].origin, lines[1].origin],
            [TxOrigin::Local, TxOrigin::Local],
            "each record carries its own item's origin"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_timestamp_matches_tx_frame_logged() {
        let h = worker_harness().await;
        request(&h, TEXT).await;

        let deadline = Instant::now() + Duration::from_secs(45);
        let mut event = None;
        while event.is_none() && Instant::now() < deadline {
            while let Ok(msg) = h.tui_rx.try_recv() {
                if let MessageType::TxFrameLogged {
                    text,
                    freq_hz,
                    qso_id,
                    timestamp,
                } = msg.message_type
                {
                    event = Some((text, freq_hz, qso_id, timestamp));
                }
            }
            while h.hamlib_rx.try_recv().is_ok() {}
            while h.autonomous_rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (text, freq_hz, qso_id, timestamp) = event.expect("TxFrameLogged must arrive");

        let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].utc, timestamp);
        assert_eq!(lines[0].text, text);
        assert_eq!(lines[0].offset_hz, freq_hz);
        assert_eq!(lines[0].qso_id, qso_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frame_whose_audio_is_not_enqueued_leaves_no_record() {
        let h = worker_harness().await;
        assert!(fill_channel(&h, ComponentId::Audio).await > 0);

        request(&h, TEXT).await;
        let (completed, _, ptt_on) = await_completion(&h, TEXT, Duration::from_secs(45)).await;
        assert!(completed.is_some(), "the frame must complete");
        assert!(ptt_on, "PTT keyed, so only the audio enqueue was dropped");
        assert!(
            tx_log_lines(&h).is_empty(),
            "dropped audio never aired, so it must not be recorded"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frame_refused_before_ptt_leaves_no_record() {
        let h = worker_harness().await;
        h.coordinator
            .operating_frequency_hz
            .store(15_000_000, Ordering::Relaxed);

        request(&h, TEXT).await;
        let (completed, _, ptt_on) = await_completion(&h, TEXT, Duration::from_secs(5)).await;
        assert_eq!(completed, Some(false));
        assert!(!ptt_on);
        assert!(tx_log_lines(&h).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_tx_log_never_holds_ptt_past_playback() {
        let gate = WriterGate::closed();
        let writer_gate = gate.clone();
        let h = worker_harness_with(Config::default(), move |c| {
            let path = tx_log_path(c.pancetta_home_override.as_deref());
            c.tx_frame_log = Some(Arc::new(TxFrameLog::open_gated(
                path,
                RotationPolicy::default(),
                writer_gate,
            )));
        })
        .await;
        let (_, audio_rx) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();

        request(&h, TEXT).await;
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut ptt_on = false;
        let mut ptt_off_after_on = false;
        let mut ptt_off_at = None;
        let mut audio = None;
        let mut completed = None;
        while completed.is_none() && Instant::now() < deadline {
            while let Ok(msg) = audio_rx.try_recv() {
                if let MessageType::AudioOutput {
                    samples,
                    sample_rate,
                    ..
                } = msg.message_type
                {
                    let len = Duration::from_secs_f64(samples.len() as f64 / sample_rate as f64);
                    audio.get_or_insert((Instant::now(), len));
                }
            }
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                    msg.message_type
                {
                    if state {
                        ptt_on = true;
                    } else if ptt_on {
                        ptt_off_after_on = true;
                        ptt_off_at.get_or_insert(Instant::now());
                    }
                }
            }
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    message_text,
                    ..
                } = msg.message_type
                {
                    if message_text == TEXT {
                        completed = Some(success);
                    }
                }
            }
            while h.tui_rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let off_deadline = Instant::now() + Duration::from_secs(2);
        while !ptt_off_after_on && Instant::now() < off_deadline {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if let MessageType::RigControl(RigControlMessage::SetPtt { state: false }) =
                    msg.message_type
                {
                    if ptt_on {
                        ptt_off_after_on = true;
                        ptt_off_at.get_or_insert(Instant::now());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            completed,
            Some(true),
            "the frame completes while the writer is blocked"
        );
        assert!(ptt_on, "PTT keyed");
        assert!(ptt_off_after_on, "PTT released while the writer is blocked");
        // PR #468 review: the full TX_LOG_ACK_WAIT spent waiting on the blocked
        // writer must not be added to key-down after the audio ends.
        let (audio_at, audio_len) = audio.expect("audio enqueued");
        let ptt_off_at = ptt_off_at.expect("PTT-off observed in the playback loop");
        let overhang = ptt_off_at.saturating_duration_since(audio_at + audio_len);
        assert!(
            overhang < TX_LOG_ACK_WAIT - Duration::from_millis(50),
            "PTT stayed keyed {overhang:?} past the audio (log wait leaked into playback)"
        );
        assert!(
            tx_log_lines(&h).is_empty(),
            "the blocked writer wrote nothing"
        );

        gate.open();
        let lines = await_tx_log_lines(&h, 1, Duration::from_secs(5)).await;
        assert_eq!(
            lines.len(),
            1,
            "the queued record lands once the disk responds"
        );
        assert_eq!(lines[0].text, TEXT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_frame_log_is_created_once_and_reused_across_restarts() {
        let mut h = worker_harness().await;
        let first = h
            .coordinator
            .tx_frame_log
            .clone()
            .expect("created by start_transmitter_component");
        let home = h.coordinator.pancetta_home_override.clone().unwrap();
        assert!(first.path().expect("enabled").starts_with(&home));
        let again = h.coordinator.ensure_tx_frame_log();
        assert!(Arc::ptr_eq(&first, &again), "the restart reuses the writer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_mode_tx_log_is_disabled_and_creates_no_file() {
        let mut coordinator = ApplicationCoordinator::new(
            Config::default(),
            None,
            true,                                                // no_audio
            true,                                                // headless
            None,                                                // no WAV
            Some(std::path::PathBuf::from("/some/capture/dir")), // --replay
            None,                                                // no test-tx
            1500.0,
            Arc::new(AtomicBool::new(false)),
            Vec::new(),
            std::env::temp_dir().join(format!(
                "pancetta-pan122-config-{}.toml",
                uuid::Uuid::new_v4()
            )),
            true,
            None, // PAN-270: no session-home override
        )
        .await
        .expect("coordinator creation should succeed");
        let home = tempfile::tempdir().unwrap();
        coordinator.pancetta_home_override = Some(home.path().to_path_buf());

        let log = coordinator.ensure_tx_frame_log();
        assert_eq!(log.path(), None);
        let record = TxFrameRecord::new(
            chrono::Utc::now(),
            pancetta_ft8::Protocol::Ft8,
            0,
            1500.0,
            TEXT.to_string(),
            TxOrigin::Local,
            None,
        );
        assert_eq!(
            log.append(vec![record], Duration::from_secs(1)).await,
            TxLogAppend::Disabled
        );
        assert!(!tx_log_path(Some(home.path())).exists());
    }
}

/// PAN-146: the DX-parity gate, through the real TX worker, scopes its a7
/// evidence to the live dial's band. A DX kept fresh on 20 m Odd holds an
/// Odd call on 20 m, and does not hold the same call after the operator
/// moved to 15 m.
#[cfg(test)]
mod pan146_dx_parity_context_worker_tests {
    use super::pan115_worker_gate_tests::{await_completion, worker_harness, Harness};
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType, TxOrigin};
    use pancetta_core::slot::{SlotParity, SLOT_NS};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const DIAL_20M: u64 = 14_074_000;
    const DIAL_15M: u64 = 21_074_000;
    const TEXT: &str = "5Z4VJ W5AU EM10";

    /// Opens a manual call to 5Z4VJ (latched `tx_parity = Odd`), publishes
    /// the manager to the worker, keeps a 20 m / FT8-grid Odd sighting of
    /// 5Z4VJ fresh, and sends one Odd frame for the QSO with the dial at
    /// `live_dial_hz`. Returns `(completed, diagnostics, ptt_on)`.
    async fn call_with_dx_heard_on_20m(
        h: &Harness,
        live_dial_hz: u64,
    ) -> (Option<bool>, Vec<String>, bool) {
        h.coordinator
            .operating_frequency_hz
            .store(live_dial_hz, Ordering::Relaxed);
        let manager = pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W5AU".into(), // QsoManager refuses the N0CALL placeholder
            ..Default::default()
        });
        let id = manager
            .respond_to_cq_manual("5Z4VJ".into(), 1500.0, Some(SlotParity::Even))
            .await
            .unwrap();
        h.coordinator.qso_manager_watch.send_replace(Some(manager));
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(&id.to_string()));

        let stop = Arc::new(AtomicBool::new(false));
        let refresher = {
            let cts = h.coordinator.cross_time_state.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    cts.a7_recent_calls
                        .write()
                        .unwrap()
                        .record(pancetta_qso::A7ExpectedCall {
                            callsign: "5Z4VJ".into(),
                            freq_hz: 1500.0,
                            slot_parity: 1,
                            decoded_at: std::time::SystemTime::now(),
                            captured_dial_hz: DIAL_20M,
                            slot_ns: SLOT_NS,
                        });
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
        };

        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: TEXT.into(),
                    frequency_offset: 1500.0,
                    qso_id: Some(id.to_string()),
                    tx_parity: Some(SlotParity::Odd),
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                Instant::now(),
            ))
            .await
            .unwrap();
        let result = await_completion(h, TEXT, Duration::from_secs(50)).await;
        stop.store(true, Ordering::Release);
        let _ = refresher.await;
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_band_sighting_still_holds_the_call() {
        let h = worker_harness().await;
        let (completed, diagnostics, ptt_on) = call_with_dx_heard_on_20m(&h, DIAL_20M).await;
        assert_eq!(completed, Some(false));
        assert!(!ptt_on, "a same-band collision must not key");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("DX observed transmitting on the colliding parity")),
            "{diagnostics:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_band_sighting_does_not_hold_the_call() {
        let h = worker_harness().await;
        let (completed, diagnostics, ptt_on) = call_with_dx_heard_on_20m(&h, DIAL_15M).await;
        assert_eq!(completed, Some(true), "{diagnostics:?}");
        assert!(ptt_on, "the 15 m call must key");
        assert!(!diagnostics.iter().any(|d| d.contains("colliding parity")));
    }
}

#[cfg(test)]
mod pan182_audio_delivery_tests {
    use super::pan115_worker_gate_tests::worker_harness;
    use super::pan122_tx_log_worker_tests::fill_channel;
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage, TxOrigin};
    use std::time::{Duration, Instant};

    /// PR #507 review (P1): a Step-7 audio send that comes back `Dropped`
    /// (here: the Audio channel is full) enqueued no samples, so the frame
    /// never aired even though PTT keyed. Step 10 must report it as not
    /// sent: the QSO engine gets the PAN-75 `TxFrameNotSent` mirror (which
    /// rolls the attempt back) and never `TxFrameSent`, whose false
    /// confirmation would release a held PAN-182 stall switch on a frame
    /// that never keyed any audio.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_audio_delivery_reports_the_frame_as_not_sent() {
        let h = worker_harness().await;
        assert!(fill_channel(&h, ComponentId::Audio).await > 0);
        // The defer-time liveness gate needs the QSO id to be TX-live (the
        // QSO component publishes this set; here it is seeded directly).
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert("Q1".into());

        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    attempt_id: None,
                    message_text: "K1ABC N0CALL FN20".to_string(),
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

        let deadline = Instant::now() + Duration::from_secs(45);
        let mut completion = None;
        let mut not_sent_mirror = false;
        let mut sent_mirror = false;
        fn capture(
            completion: &mut Option<bool>,
            not_sent_mirror: &mut bool,
            sent_mirror: &mut bool,
            msg: ComponentMessage,
        ) {
            match msg.message_type {
                MessageType::TransmitComplete {
                    success,
                    ref message_text,
                    ..
                } if message_text == "K1ABC N0CALL FN20" => {
                    *completion = Some(success);
                }
                MessageType::QsoMessage(QsoMessage::TxFrameNotSent { ref qso_id, .. })
                    if qso_id == "q1" =>
                {
                    *not_sent_mirror = true;
                }
                MessageType::QsoMessage(QsoMessage::TxFrameSent { ref qso_id, .. })
                    if qso_id == "q1" =>
                {
                    *sent_mirror = true;
                }
                _ => {}
            }
        }
        while completion.is_none() && Instant::now() < deadline {
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
            }
            while let Ok(msg) = h.qso_rx.try_recv() {
                capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Anything still in flight after the completion.
        while let Ok(msg) = h.qso_rx.try_recv() {
            capture(&mut completion, &mut not_sent_mirror, &mut sent_mirror, msg);
        }

        assert_eq!(
            completion,
            Some(false),
            "a frame whose audio never enqueued must complete as failed"
        );
        assert!(
            not_sent_mirror,
            "the QSO engine must get the PAN-75 not-sent mirror"
        );
        assert!(
            !sent_mirror,
            "no aired mirror may be sent for a frame that never keyed audio"
        );
    }
}

/// PAN-278: a PTT-on that `send_ptt_command` returns `Dropped` while Hamlib's
/// route is registered (here: Hamlib's channel is full) never keyed the rig.
/// The worker must unwind like a pre-PTT refusal, play no audio, and settle
/// the frame as not sent. Without a route (`--replay`) the drop is by
/// design and the frame still plays.
///
/// Hamlib's backlog is read only after the completion: draining it earlier
/// (as `await_completion` does) un-saturates the channel and lets the
/// key-down through, which hides the defect.
#[cfg(test)]
mod pan278_dropped_ptt_on_tests {
    use super::pan115_worker_gate_tests::{worker_harness, worker_harness_without_rig, Harness};
    use super::pan122_tx_log_worker_tests::fill_channel;
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, QsoMessage, RigControlMessage,
        TransmitRequestItem, TxOrigin,
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
                        qso_id,
                        attempt_id,
                        ..
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
}

/// PAN-223: `report_frame_scheduled` carries the worker's own
/// `TxSchedule::target_slot` to the QSO engine, durably, and only for frames
/// that name both a QSO and a ledger attempt.
#[cfg(test)]
#[path = "tx/tests/frame_scheduled_report_tests.rs"]
mod frame_scheduled_report_tests;

/// PAN-223: the real TX worker reports the slot `schedule_tx` picked from its
/// own dequeue instant. Each test ends at the report and never waits for the
/// slot itself.
#[cfg(test)]
mod pan223_scheduled_slot_worker_tests {
    use super::pan115_worker_gate_tests::{worker_harness, Harness};
    use crate::message_bus::{
        ComponentId, ComponentMessage, MessageType, QsoMessage, TransmitRequestItem, TxOrigin,
    };
    use pancetta_core::slot::{current_slot_start_with_period, SlotParity, SLOT_NS};
    use pancetta_qso::TxAttemptId;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    type Report = (String, TxAttemptId, chrono::DateTime<chrono::Utc>);

    /// Dial on 20m and `qsos` live, as the QSO coordinator leaves them.
    fn setup(h: &Harness, qsos: &[&str]) {
        h.coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Relaxed);
        let mut live = h.coordinator.active_tx_qsos.write().unwrap();
        for qso in qsos {
            live.insert(super::super::active_tx_qso_key(qso));
        }
    }

    async fn send(h: &Harness, message_type: MessageType) {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                message_type,
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    fn request(qso: &str, id: u64, parity: SlotParity) -> MessageType {
        MessageType::TransmitRequest {
            attempt_id: Some(TxAttemptId(id)),
            message_text: "K9ZZ K5ARH EM12".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some(qso.to_string()),
            tx_parity: Some(parity),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        }
    }

    /// Every `TxFrameScheduled` on the Qso channel until `count` arrived or
    /// 5 s passed, then 300 ms more so a duplicate would show up.
    async fn scheduled_reports(h: &Harness, count: usize) -> Vec<Report> {
        let mut reports = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let drain = |reports: &mut Vec<Report>| {
            while let Ok(msg) = h.qso_rx.try_recv() {
                if let MessageType::QsoMessage(QsoMessage::TxFrameScheduled {
                    qso_id,
                    attempt_id,
                    target_slot,
                }) = msg.message_type
                {
                    reports.push((qso_id, attempt_id, target_slot));
                }
            }
        };
        while reports.len() < count && Instant::now() < deadline {
            drain(&mut reports);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        drain(&mut reports);
        reports
    }

    /// The start of the slot after the current one, and its parity: a
    /// request with that parity always defers to it.
    fn next_slot() -> (chrono::DateTime<chrono::Utc>, SlotParity) {
        let next = current_slot_start_with_period(chrono::Utc::now(), SLOT_NS)
            + chrono::Duration::nanoseconds(SLOT_NS);
        (next, SlotParity::of_with_period(next, SLOT_NS))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_request_reports_its_attempt_and_deferred_target() {
        let h = worker_harness().await;
        setup(&h, &["qa"]);
        let (next, parity) = next_slot();

        send(&h, request("qa", 41, parity)).await;

        assert_eq!(
            scheduled_reports(&h, 1).await,
            vec![("qa".to_string(), TxAttemptId(41), next)]
        );
    }

    /// AC 1, worker half: a request the worker dequeues past own slot S's
    /// 1.7 s acceptance cap is reported at S + 30 s, never S.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_dequeued_past_the_acceptance_cap_reports_the_next_own_slot() {
        let h = worker_harness().await;
        setup(&h, &["qa"]);
        // Wait until 2.0-12.0 s into a slot (at most ~5 s).
        let now = chrono::Utc::now();
        let phase = now - current_slot_start_with_period(now, SLOT_NS);
        let wait = if phase < chrono::Duration::milliseconds(2_000) {
            chrono::Duration::milliseconds(2_000) - phase
        } else if phase > chrono::Duration::milliseconds(12_000) {
            chrono::Duration::milliseconds(17_000) - phase
        } else {
            chrono::Duration::zero()
        };
        tokio::time::sleep(wait.to_std().unwrap()).await;
        let s = current_slot_start_with_period(chrono::Utc::now(), SLOT_NS);
        let own = SlotParity::of_with_period(s, SLOT_NS);

        send(&h, request("qa", 42, own)).await;

        let reports = scheduled_reports(&h, 1).await;
        assert_eq!(
            reports,
            vec![(
                "qa".to_string(),
                TxAttemptId(42),
                s + chrono::Duration::seconds(30)
            )],
            "dequeued past the cap of own slot {s}, the frame is scheduled into the next own slot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bundle_reports_each_item_at_the_bundle_target() {
        let h = worker_harness().await;
        setup(&h, &["qa", "qb"]);
        let (next, parity) = next_slot();
        let item = |text: &str, offset: f64, qso: &str, id: u64| TransmitRequestItem {
            attempt_id: Some(TxAttemptId(id)),
            message_text: text.to_string(),
            frequency_offset: offset,
            qso_id: Some(qso.to_string()),
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        };

        send(
            &h,
            MessageType::MultiTransmitRequest {
                items: vec![
                    item("SM2LIY K5ARH R-14", 1000.0, "qa", 51),
                    item("C6AVD K5ARH EM10", 1600.0, "qb", 52),
                ],
                tx_parity: Some(parity),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
        )
        .await;

        assert_eq!(
            scheduled_reports(&h, 2).await,
            vec![
                ("qa".to_string(), TxAttemptId(51), next),
                ("qb".to_string(), TxAttemptId(52), next),
            ]
        );
    }
}

/// PAN-183: each frame's keep-call ledger attempt rides its request through
/// coalescing, pivots and supersedes, and comes back on `TxFrameNotSent`, so
/// the QSO engine rolls back exactly the attempt that failed.
#[cfg(test)]
#[path = "tx/tests/attempt_identity_tests.rs"]
mod attempt_identity_tests;
#[cfg(test)]
mod pan71_playback_start_tests {
    use super::super::tx_log::{tx_log_path, RotationPolicy, WriterGate};
    use super::pan115_worker_gate_tests::{build_worker_harness, request, Harness};
    use super::*;
    use crate::coordinator::ApplicationCoordinator;
    use crate::message_bus::{RigControlMessage, TransmitRequestItem, TxOrigin};
    use pancetta_config::Config;
    use std::sync::{Arc, Mutex};

    // Tokio's elapsed time and UTC share one origin on each paused runtime.

    fn virtual_clock() -> pancetta_core::time::UtcClock {
        // All harnesses in a test are constructed before virtual time advances.
        let origin = tokio::time::Instant::now();
        Arc::new(move || {
            pancetta_core::time::slot_grid_origin()
                + chrono::Duration::from_std(origin.elapsed()).unwrap()
        })
    }

    async fn worker_harness() -> Harness {
        worker_harness_with(Config::default(), |_| {}).await
    }

    async fn worker_harness_with(
        config: Config,
        prepare: impl FnOnce(&mut ApplicationCoordinator),
    ) -> Harness {
        build_worker_harness(
            config,
            prepare,
            Arc::new(pancetta_agent::arm::ArmTime::now),
            virtual_clock(),
            true,
        )
        .await
    }

    const TEXT: &str = "CQ N0CALL FN20";
    const REPLACEMENT: &str = "CQ N0CALL EM12";

    #[derive(Clone, Copy)]
    enum Behaviour {
        StartAfter(Duration),
        NotStarted,
        Drop,
        Hold,
    }

    #[derive(Clone, Debug)]
    struct Seen {
        received_at: chrono::DateTime<chrono::Utc>,
        flush_first: bool,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    struct FakeAudio {
        seen: Arc<Mutex<Vec<Seen>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeAudio {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn spawn_fake_audio_thread(h: &Harness, behaviour: Behaviour) -> FakeAudio {
        let (_, rx) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let utc_clock = h.utc_clock.clone();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                while let Ok(msg) = rx.try_recv() {
                    if let MessageType::AudioOutput {
                        playback_started,
                        flush_first,
                        ..
                    } = msg.message_type
                    {
                        let ack = playback_started.expect("logged frame must carry an ack");
                        let received_at = utc_clock();
                        let started_at = if let Behaviour::StartAfter(d) = behaviour {
                            Some(received_at + chrono::Duration::from_std(d).unwrap())
                        } else {
                            None
                        };
                        captured.lock().unwrap().push(Seen {
                            received_at,
                            flush_first,
                            started_at,
                        });
                        match behaviour {
                            Behaviour::StartAfter(d) => {
                                sleep(d).await;
                                ack.started(started_at.unwrap());
                            }
                            Behaviour::NotStarted => ack.not_started(),
                            Behaviour::Drop => drop(ack),
                            Behaviour::Hold => held.push(ack),
                        }
                    }
                }
                sleep(Duration::from_millis(1)).await;
            }
        });
        FakeAudio { seen, task }
    }

    async fn await_event(h: &Harness, wanted: &str) -> chrono::DateTime<chrono::Utc> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            while let Ok(msg) = h.tui_rx.try_recv() {
                if let MessageType::TxFrameLogged {
                    text, timestamp, ..
                } = msg.message_type
                {
                    if text == wanted {
                        return timestamp;
                    }
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no TxFrameLogged for {wanted}"
            );
            sleep(Duration::from_millis(1)).await;
        }
    }

    fn tx_log_lines(h: &Harness) -> Vec<TxFrameRecord> {
        let path = tx_log_path(h.coordinator.pancetta_home_override.as_deref());
        match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("reading TX log: {e}"),
        }
    }

    async fn await_tx_log_lines(h: &Harness, n: usize) -> Vec<TxFrameRecord> {
        h.coordinator
            .tx_frame_log
            .as_ref()
            .unwrap()
            .drain_for_test();
        let lines = tx_log_lines(h);
        assert!(
            lines.len() >= n,
            "writer drained without the expected records"
        );
        lines
    }

    #[tokio::test(start_paused = true)]
    async fn worker_stamps_come_from_the_injected_clock() {
        let mut config = Config::default();
        config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
        let pinned = pancetta_core::time::slot_grid_origin();
        let h = build_worker_harness(
            config,
            |_| {},
            Arc::new(pancetta_agent::arm::ArmTime::now),
            Arc::new(move || pinned),
            true,
        )
        .await;
        let _audio = spawn_fake_audio_thread(&h, Behaviour::NotStarted).await;
        request(&h, TEXT).await;
        assert_eq!(await_event(&h, TEXT).await, pinned);
        assert_eq!(await_record(&h).await.utc, pinned);
    }

    #[tokio::test(start_paused = true)]
    async fn first_pass_frame_is_stamped_at_the_reported_playback_start() {
        let h = worker_harness().await;
        let audio =
            spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
        request(&h, TEXT).await;
        let at = await_event(&h, TEXT).await;
        let seen = audio.seen.lock().unwrap()[0].clone();
        assert!(!seen.flush_first);
        assert_eq!(Some(at), seen.started_at);
        assert!(at > seen.received_at);
        let lines = await_tx_log_lines(&h, 1).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].utc, at);
    }

    #[tokio::test(start_paused = true)]
    async fn rekeyed_frame_is_stamped_at_the_replacement_audios_playback_start() {
        // Auto picks the NEXT slot on each request. Pin parity so the
        // replacement can re-key this slot within the normal 3000 ms cap.
        let mut config = Config::default();
        config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
        let h = worker_harness_with(config, |_| {}).await;
        let audio =
            spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
        request(&h, TEXT).await;
        let first = await_event(&h, TEXT).await;
        request(&h, REPLACEMENT).await;
        let second = await_event(&h, REPLACEMENT).await;
        let seen = audio.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].flush_first, "exercise an actual mid-frame re-key");
        assert_eq!(Some(second), seen[1].started_at);
        assert!(second > seen[1].received_at);
        assert!(second > first);
        let lines = await_tx_log_lines(&h, 2).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, REPLACEMENT);
        assert_eq!(lines[1].utc, second);
    }

    /// One frame through `h`: `(stamp, when the event was observed, what audio saw)`.
    async fn fallback_event(
        h: &Harness,
        behaviour: Behaviour,
    ) -> (
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        Seen,
    ) {
        let audio = spawn_fake_audio_thread(h, behaviour).await;
        request(h, TEXT).await;
        let at = await_event(h, TEXT).await;
        let observed = (h.utc_clock)();
        let seen = audio.seen.lock().unwrap()[0].clone();
        // The enqueue instant, not the timeout instant a bound later.
        assert!(
            at <= seen.received_at
                + chrono::Duration::from_std(TX_PLAYBACK_START_WAIT / 2).unwrap()
        );
        (at, observed, seen)
    }

    /// `h`'s one TX-log record. `TX_LOG_ACK_WAIT` caps how long the worker
    /// waits for the append, not the write, so the record can land after
    /// `TxFrameLogged` (PAN-248).
    async fn await_record(h: &Harness) -> TxFrameRecord {
        let lines = await_tx_log_lines(h, 1).await;
        lines.into_iter().next().expect("the frame's TX-log record")
    }

    async fn assert_fallback(behaviour: Behaviour) {
        let h = worker_harness().await;
        if matches!(behaviour, Behaviour::Hold) {
            let (at, observed, _) = fallback_event(&h, behaviour).await;
            assert!(observed - at >= chrono::Duration::milliseconds(98));
            assert_eq!(await_record(&h).await.utc, at);
            return;
        }
        // PAN-248: the durable append (up to TX_LOG_ACK_WAIT) runs between the
        // wait and TxFrameLogged, so time the settled ack on a harness without it.
        let timed = worker_harness_with(Config::default(), |c| {
            c.tx_frame_log = Some(Arc::new(TxFrameLog::disabled()))
        })
        .await;
        let ((at, _, _), (_, observed, seen)) = tokio::join!(
            fallback_event(&h, behaviour),
            fallback_event(&timed, behaviour)
        );
        assert!(
            observed - seen.received_at
                < chrono::Duration::from_std(TX_PLAYBACK_START_WAIT).unwrap()
        );
        assert_eq!(await_record(&h).await.utc, at);
    }

    #[tokio::test(start_paused = true)]
    async fn unconfirmed_playback_start_falls_back_to_the_enqueue_instant_after_the_bound() {
        assert_fallback(Behaviour::Hold).await;
    }
    #[tokio::test(start_paused = true)]
    async fn not_started_report_falls_back_to_the_enqueue_instant_without_waiting() {
        assert_fallback(Behaviour::NotStarted).await;
    }
    #[tokio::test(start_paused = true)]
    async fn dropped_ack_falls_back_to_the_enqueue_instant_without_waiting() {
        assert_fallback(Behaviour::Drop).await;
    }

    #[tokio::test(start_paused = true)]
    async fn bundle_keys_without_waiting_on_wall_time() {
        use pancetta_core::slot::SlotParity;

        let mut config = Config::default();
        config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
        // Headroom for a loaded runner: the requeued replacement must still
        // be accepted into the slot the first bundle keyed at its boundary.
        config.station.tx_late_max_ms = 10_000;
        let h = worker_harness_with(config, |_| {}).await;
        let (_, audio_rx) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();
        {
            let mut live = h.coordinator.active_tx_qsos.write().unwrap();
            for id in ["pan294-a", "pan294-b"] {
                live.insert(super::super::active_tx_qso_key(id));
            }
        }
        let submit = |texts: [&str; 2]| {
            ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    // Matching live identities and changed text force a supersede.
                    items: texts
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| TransmitRequestItem {
                            attempt_id: None,
                            message_text: text.to_owned(),
                            frequency_offset: 1000.0 + i as f64 * 600.0,
                            qso_id: Some(["pan294-a", "pan294-b"][i].to_owned()),
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        })
                        .collect(),
                    tx_parity: Some(SlotParity::Even),
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                tokio::time::Instant::now().into_std(),
            )
        };
        let initial = [TEXT, "CQ K1ABC FN31"];
        let replacement = [REPLACEMENT, "CQ K1ABC EM12"];
        let mut waveforms = Vec::new();
        let mut buffered = Vec::new();
        let mut flushes = Vec::new();
        let mut starts = Vec::new();
        let mut ptt = Vec::new();
        // A requeued bundle is dequeued afresh and only takes the current slot
        // inside `multi_tx_acceptance_cap_ms`. Submit from an odd slot so the
        // even bundle keys at its boundary, not late in a slot already running.
        tokio::time::advance(Duration::from_secs(15)).await;
        h.coordinator
            .message_bus
            .send_message(submit(initial))
            .await
            .unwrap();
        for texts in [initial, replacement] {
            // Simulate an audio callback that starts playback but retains all
            // queued samples. Only a flush can remove the predecessor.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
            loop {
                if let Ok(msg) = audio_rx.try_recv() {
                    if let MessageType::AudioOutput {
                        samples,
                        flush_first,
                        playback_started,
                        ..
                    } = msg.message_type
                    {
                        assert!(!samples.is_empty());
                        if flush_first {
                            buffered.clear();
                        }
                        buffered.extend_from_slice(&samples);
                        waveforms.push(samples);
                        flushes.push(flush_first);
                        playback_started
                            .expect("bundle playback ack")
                            .started((h.utc_clock)());
                        break;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no bundle audio for {texts:?}"
                );
                sleep(Duration::from_millis(1)).await;
            }
            // Both log events prove both streams reached playback. Awaiting
            // them also ensures replacement arrives after the first ack.
            let first = await_event(&h, texts[0]).await;
            assert_eq!(first, await_event(&h, texts[1]).await);
            starts.push(first);
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                    msg.message_type
                {
                    ptt.push(state);
                }
            }
            if waveforms.len() == 1 {
                assert_eq!(buffered, waveforms[0]);
                h.coordinator
                    .message_bus
                    .send_message(submit(replacement))
                    .await
                    .unwrap();
            }
        }
        assert_eq!(waveforms.len(), 2);
        assert_ne!(waveforms[0], waveforms[1], "supersede changes the waveform");
        assert_eq!(
            starts[0].timestamp().div_euclid(15),
            starts[1].timestamp().div_euclid(15),
            "replacement must requeue into the same FT8 slot"
        );
        assert_eq!(
            ptt,
            [true, false, true],
            "unkey before the replacement keys"
        );
        assert!(flushes[1], "replacement bundle must flush stale audio");
        assert_eq!(buffered, waveforms[1], "only replacement samples remain");
    }

    #[tokio::test(start_paused = true)]
    async fn bundle_items_share_the_reported_playback_start() {
        let h = worker_harness().await;
        let audio =
            spawn_fake_audio_thread(&h, Behaviour::StartAfter(Duration::from_millis(37))).await;
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Autonomous,
                ComponentId::Ft8Transmitter,
                MessageType::MultiTransmitRequest {
                    items: [TEXT, REPLACEMENT]
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| TransmitRequestItem {
                            attempt_id: None,
                            message_text: text.to_owned(),
                            frequency_offset: 1000.0 + i as f64 * 600.0,
                            qso_id: None,
                            origin: TxOrigin::Local,
                            remote_client_key_id: None,
                        })
                        .collect(),
                    tx_parity: None,
                    origin: TxOrigin::Local,
                    remote_client_key_id: None,
                },
                tokio::time::Instant::now().into_std(),
            ))
            .await
            .unwrap();
        let first = await_event(&h, TEXT).await;
        let second = await_event(&h, REPLACEMENT).await;
        let seen = audio.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(Some(first), seen[0].started_at);
        assert_eq!(first, second);
        let lines = await_tx_log_lines(&h, 2).await;
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|r| r.utc == first));
    }

    #[tokio::test(start_paused = true)]
    async fn frame_log_instant_started_uses_reported_time() {
        let (ack, mut waiter) = PlaybackStartAck::new();
        let fallback = pancetta_core::time::slot_grid_origin();
        let at = fallback + chrono::Duration::milliseconds(37);
        ack.started(at);
        assert_eq!(frame_log_instant(&mut waiter, fallback, TEXT).await, at);
    }
    #[tokio::test(start_paused = true)]
    async fn frame_log_instant_not_started_uses_fallback() {
        let (ack, mut waiter) = PlaybackStartAck::new();
        let fallback = pancetta_core::time::slot_grid_origin();
        ack.not_started();
        assert_eq!(
            frame_log_instant(&mut waiter, fallback, TEXT).await,
            fallback
        );
    }
    #[tokio::test(start_paused = true)]
    async fn frame_log_instant_dropped_uses_fallback() {
        let (ack, mut waiter) = PlaybackStartAck::new();
        let fallback = pancetta_core::time::slot_grid_origin();
        drop(ack);
        assert_eq!(
            frame_log_instant(&mut waiter, fallback, TEXT).await,
            fallback
        );
    }
    #[tokio::test(start_paused = true)]
    async fn frame_log_instant_pending_uses_fallback_after_bound() {
        let (_ack, mut waiter) = PlaybackStartAck::new();
        let fallback = pancetta_core::time::slot_grid_origin();
        let before = tokio::time::Instant::now();
        assert_eq!(
            frame_log_instant(&mut waiter, fallback, TEXT).await,
            fallback
        );
        assert!(before.elapsed() >= TX_PLAYBACK_START_WAIT);
    }
    #[tokio::test(start_paused = true)]
    async fn a_held_playback_start_ack_never_holds_ptt_past_playback() {
        let gate = WriterGate::closed();
        let writer_gate = gate.clone();
        let h = worker_harness_with(Config::default(), move |c| {
            let path = tx_log_path(c.pancetta_home_override.as_deref());
            c.tx_frame_log = Some(Arc::new(TxFrameLog::open_gated(
                path,
                RotationPolicy::default(),
                writer_gate,
            )));
        })
        .await;
        let (_, audio_rx) = h
            .coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Audio)
            .await
            .unwrap();

        request(&h, TEXT).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        let mut ptt_on = false;
        let mut ptt_off_after_on = false;
        let mut ptt_off_at = None;
        let mut audio = None;
        let mut held = Vec::new();
        let mut completed = None;
        while completed.is_none() && tokio::time::Instant::now() < deadline {
            while let Ok(msg) = audio_rx.try_recv() {
                if let MessageType::AudioOutput {
                    samples,
                    sample_rate,
                    playback_started,
                    ..
                } = msg.message_type
                {
                    held.push(playback_started.expect("frame ack"));
                    let len = Duration::from_secs_f64(samples.len() as f64 / sample_rate as f64);
                    audio.get_or_insert((tokio::time::Instant::now(), len));
                }
            }
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if let MessageType::RigControl(RigControlMessage::SetPtt { state }) =
                    msg.message_type
                {
                    if state {
                        ptt_on = true;
                    } else if ptt_on {
                        ptt_off_after_on = true;
                        ptt_off_at.get_or_insert(tokio::time::Instant::now());
                    }
                }
            }
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    message_text,
                    ..
                } = msg.message_type
                {
                    if message_text == TEXT {
                        completed = Some(success);
                    }
                }
            }
            while h.tui_rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let off_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !ptt_off_after_on && tokio::time::Instant::now() < off_deadline {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if let MessageType::RigControl(RigControlMessage::SetPtt { state: false }) =
                    msg.message_type
                {
                    if ptt_on {
                        ptt_off_after_on = true;
                        ptt_off_at.get_or_insert(tokio::time::Instant::now());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            completed,
            Some(true),
            "the frame completes while the writer is blocked"
        );
        assert!(ptt_on, "PTT keyed");
        assert!(ptt_off_after_on, "PTT released while the writer is blocked");
        // PR #468 review: the full TX_LOG_ACK_WAIT spent waiting on the blocked
        // writer must not be added to key-down after the audio ends.
        let (audio_at, audio_len) = audio.expect("audio enqueued");
        let ptt_off_at = ptt_off_at.expect("PTT-off observed in the playback loop");
        let overhang = ptt_off_at.saturating_duration_since(audio_at + audio_len);
        assert!(
            overhang < TX_LOG_ACK_WAIT - Duration::from_millis(50),
            "PTT stayed keyed {overhang:?} past the audio (log wait leaked into playback)"
        );
        assert!(
            tx_log_lines(&h).is_empty(),
            "the blocked writer wrote nothing"
        );

        gate.open();
        let lines = await_tx_log_lines(&h, 1).await;
        assert_eq!(
            lines.len(),
            1,
            "the queued record lands once the disk responds"
        );
        assert_eq!(lines[0].text, TEXT);
    }
}

#[cfg(test)]
#[path = "tx/tests/receive_overload_ptt_admission_tests.rs"]
mod receive_overload_ptt_admission_tests;

/// PAN-279: every unaired frame settles through `tx_outcome`, driven
/// through the real TX worker.
#[cfg(test)]
mod pan279_settlement_worker_tests {
    use super::pan115_worker_gate_tests::{
        await_ptt_on, worker_harness, worker_harness_with_config, Harness,
    };
    use super::*;
    use crate::message_bus::{QsoMessage, RigControlMessage, TxOrigin};
    use pancetta_qso::TxAttemptId;
    use std::time::Duration;

    fn live(h: &Harness, qso_id: &str) {
        h.coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert(super::super::active_tx_qso_key(qso_id));
    }

    fn request(
        text: &str,
        hz: f64,
        qso_id: Option<&str>,
        attempt: u64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> MessageType {
        MessageType::TransmitRequest {
            attempt_id: Some(TxAttemptId(attempt)),
            message_text: text.into(),
            frequency_offset: hz,
            qso_id: qso_id.map(str::to_string),
            tx_parity,
            origin: TxOrigin::Local,
            remote_client_key_id: None,
        }
    }

    async fn send(h: &Harness, message_type: MessageType) {
        h.coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Qso,
                ComponentId::Ft8Transmitter,
                message_type,
                Instant::now(),
            ))
            .await
            .unwrap();
    }

    fn parity_now() -> pancetta_core::slot::SlotParity {
        let now = chrono::Utc::now();
        pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        )
    }

    #[derive(Default, Debug)]
    struct Observed {
        /// Autonomous `(qso_id, success, attempt_id)` completions.
        completions: Vec<(Option<String>, bool, Option<TxAttemptId>)>,
        /// Qso-bound reports, as short strings in arrival order.
        qso: Vec<String>,
        ptt_on: usize,
    }

    /// Drain every receiver for `within`, or until `done` holds.
    async fn observe(h: &Harness, within: Duration, done: impl Fn(&Observed) -> bool) -> Observed {
        let mut o = Observed::default();
        let deadline = Instant::now() + within;
        while Instant::now() < deadline && !done(&o) {
            while let Ok(msg) = h.hamlib_rx.try_recv() {
                if matches!(
                    msg.message_type,
                    MessageType::RigControl(RigControlMessage::SetPtt { state: true })
                ) {
                    o.ptt_on += 1;
                }
            }
            while h.tui_rx.try_recv().is_ok() {}
            while let Ok(msg) = h.autonomous_rx.try_recv() {
                if let MessageType::TransmitComplete {
                    success,
                    qso_id,
                    attempt_id,
                    ..
                } = msg.message_type
                {
                    o.completions.push((qso_id, success, attempt_id));
                }
            }
            while let Ok(msg) = h.qso_rx.try_recv() {
                match msg.message_type {
                    MessageType::QsoMessage(QsoMessage::TxFrameNotSent {
                        message_text,
                        attempt_id,
                        ..
                    }) => o.qso.push(format!("NotSent {message_text} {attempt_id:?}")),
                    MessageType::QsoMessage(QsoMessage::TxFrameScheduled {
                        attempt_id, ..
                    }) => o.qso.push(format!("Scheduled {attempt_id:?}")),
                    MessageType::QsoMessage(QsoMessage::TxFrameSent { message_text, .. }) => {
                        o.qso.push(format!("Sent {message_text}"))
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        o
    }

    fn for_qso(o: &Observed, qso_id: &str) -> Vec<(bool, Option<TxAttemptId>)> {
        o.completions
            .iter()
            .filter(|(q, ..)| q.as_deref() == Some(qso_id))
            .map(|(_, ok, a)| (*ok, *a))
            .collect()
    }

    /// Key `first`, then 600 ms after PTT-on (inside the keyed slot) send
    /// the superseder `second` built with the current slot's parity.
    async fn key_then_supersede(
        h: &Harness,
        first: MessageType,
        second: impl FnOnce(pancetta_core::slot::SlotParity) -> MessageType,
    ) {
        send(h, first).await;
        assert!(
            await_ptt_on(h, Duration::from_secs(20)).await,
            "worker did not key the first frame"
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        send(h, second(parity_now())).await;
    }

    /// Scenario 2: an autonomous self-CQ re-keyed by a newer frame for the
    /// same QSO settles once, as sent, under the replacement's attempt; the
    /// abandoned frame only rolls back its own ledger attempt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan279_same_qso_replace_settles_the_self_cq_once_as_sent() {
        let h = worker_harness().await;
        live(&h, "qso-cq");
        key_then_supersede(
            &h,
            request("CQ N0CALL FN20", 1500.0, Some("qso-cq"), 21, None),
            |p| request("CQ N0CALL FN20", 1700.0, Some("qso-cq"), 22, Some(p)),
        )
        .await;
        let o = observe(&h, Duration::from_secs(20), |o| {
            o.qso.iter().any(|q| q.starts_with("Sent "))
        })
        .await;
        assert_eq!(
            for_qso(&o, "qso-cq"),
            vec![(true, Some(TxAttemptId(22)))],
            "{o:?}"
        );
        assert!(
            o.qso
                .contains(&"NotSent CQ N0CALL FN20 Some(TxAttemptId(21))".to_string()),
            "{o:?}"
        );
        assert!(o.qso.contains(&"Sent CQ N0CALL FN20".to_string()), "{o:?}");
    }

    /// PAN-38 round 5 kept: a manual (`qso_id: None`) superseder does not
    /// own the QSO's outcome, so the abandoned frame still fails to
    /// Autonomous under its own attempt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan279_manual_replace_still_fails_the_abandoned_frame() {
        let h = worker_harness().await;
        live(&h, "qso-cq");
        key_then_supersede(
            &h,
            request("CQ N0CALL FN20", 1500.0, Some("qso-cq"), 21, None),
            |p| MessageType::TransmitRequest {
                attempt_id: None,
                message_text: "CQ TEST N0CALL FN20".into(),
                frequency_offset: 1700.0,
                qso_id: None,
                tx_parity: Some(p),
                origin: TxOrigin::Local,
                remote_client_key_id: None,
            },
        )
        .await;
        let o = observe(&h, Duration::from_secs(10), |o| {
            !for_qso(o, "qso-cq").is_empty()
        })
        .await;
        assert_eq!(
            for_qso(&o, "qso-cq"),
            vec![(false, Some(TxAttemptId(21)))],
            "{o:?}"
        );
    }

    /// Scenario 1 through the worker: a superseder too late for the keyed
    /// slot (no explicit parity, so it resolves to the other parity) is
    /// requeued and airs in its own next slot. The abandoned frame rolls
    /// back its attempt; Autonomous hears only the superseder's success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan279_notviable_superseder_airs_in_its_own_next_slot() {
        let h = worker_harness().await;
        live(&h, "qso-sv");
        key_then_supersede(
            &h,
            request("K1ABC W9XYZ FN20", 1500.0, Some("qso-sv"), 11, None),
            |_| request("K1ABC W9XYZ -10", 1500.0, Some("qso-sv"), 12, None),
        )
        .await;
        let o = observe(&h, Duration::from_secs(45), |o| {
            o.qso.iter().any(|q| q == "Sent K1ABC W9XYZ -10")
        })
        .await;
        let not_sent = o
            .qso
            .iter()
            .position(|q| q == "NotSent K1ABC W9XYZ FN20 Some(TxAttemptId(11))");
        let scheduled = o.qso.iter().position(|q| q == "Scheduled TxAttemptId(12)");
        let sent = o.qso.iter().position(|q| q == "Sent K1ABC W9XYZ -10");
        assert!(
            not_sent.is_some() && scheduled.is_some() && sent.is_some(),
            "{o:?}"
        );
        assert!(not_sent < scheduled && scheduled < sent, "{o:?}");
        assert_eq!(
            for_qso(&o, "qso-sv"),
            vec![(true, Some(TxAttemptId(12)))],
            "{o:?}"
        );
        assert_eq!(
            o.ptt_on, 1,
            "one more key-down after the first, no re-supersede loop: {o:?}"
        );
    }

    /// Single-arm Bundle success: with room for two streams, a same-QSO
    /// superseder at a new offset is requeued as a bundle that no longer
    /// carries the in-flight frame. The replaced frame (attempt 1), dropped
    /// silently before PAN-279, rolls back; Autonomous hears no failure for
    /// it, and later the superseder's success under attempt 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pan279_single_arm_bundle_success_rolls_back_the_replaced_frame() {
        let mut cfg = pancetta_config::Config::default();
        cfg.autonomous.max_concurrent_qsos = 2;
        let h = worker_harness_with_config(cfg).await;
        live(&h, "qso-1");
        key_then_supersede(
            &h,
            request("K1ABC W9XYZ R-10", 1000.0, Some("qso-1"), 1, None),
            |p| request("K1ABC W9XYZ R-10", 1600.0, Some("qso-1"), 2, Some(p)),
        )
        .await;
        let o = observe(&h, Duration::from_secs(45), |o| {
            !for_qso(o, "qso-1").is_empty()
        })
        .await;
        assert!(
            o.qso
                .contains(&"NotSent K1ABC W9XYZ R-10 Some(TxAttemptId(1))".to_string()),
            "{o:?}"
        );
        assert_eq!(
            for_qso(&o, "qso-1"),
            vec![(true, Some(TxAttemptId(2)))],
            "{o:?}"
        );
    }
}

#[cfg(test)]
mod abort_transcripts;
