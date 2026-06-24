use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::{
    component_criticality, degradation_message, ComponentCriticality, ComponentState,
    ComponentStatus,
};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// C19 — classification of a config hot-reload while QSO state may be latched.
///
/// A config reload must **never** clobber an in-progress QSO's latched partner
/// callsign / `tx_parity`, and must never rebuild the QSO manager (or the
/// autonomous operator) in a way that drops active QSOs. Some config sections
/// are safe to apply on the fly (UI theme, most network toggles); others are
/// snapshotted into the QSO/autonomous machinery at startup and, if re-applied
/// mid-QSO, would invalidate the latched identity/parity.
///
/// This enum is the single decision point a hot-reload apply-handler must
/// consult before touching anything. Today the wired hot-reload task is a
/// no-op (config is loaded once at startup), so this is also a regression
/// guard: it documents and pins down which fields are unsafe to apply live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigReloadApplicability {
    /// Nothing changed that matters; ignore.
    NoChange,
    /// Only live-safe sections changed (UI, network) — safe to apply now.
    SafeLive,
    /// A QSO-latched field changed (`station.callsign`, `station.grid_square`,
    /// or `autonomous.slot_parity`) and a QSO is currently active. The change
    /// MUST be deferred (not applied to the running QSO/autonomous state)
    /// until no QSO is active, so we never clobber the latched partner/parity
    /// or drop the in-progress exchange.
    DeferQsoLatched,
    /// A QSO-latched field changed but no QSO is active — safe to apply (it
    /// will be picked up the normal way, and there is nothing to clobber).
    SafeQuiescent,
}

/// Decide how a hot-reloaded config (`new` vs `old`) may be applied, given
/// whether any QSO is currently active (`qso_active`).
///
/// Live-safe sections (applied immediately, never deferred): `ui`, `network`,
/// `audio` (device switches are already "restart to apply"), `rig`,
/// `metadata`.
///
/// QSO-latched fields (deferred while a QSO is active): `station.callsign`,
/// `station.grid_square`, `autonomous.slot_parity`. These are snapshotted into
/// the task-local `QsoManager` / `AutonomousOperator` at startup; re-applying
/// them mid-QSO would break sender verification (`from_station == expected DX`)
/// and the QSO's latched `tx_parity`.
pub fn classify_config_reload(
    old: &pancetta_config::Config,
    new: &pancetta_config::Config,
    qso_active: bool,
) -> ConfigReloadApplicability {
    let latched_changed = old.station.callsign != new.station.callsign
        || old.station.grid_square != new.station.grid_square
        || old.autonomous.slot_parity != new.autonomous.slot_parity;

    if latched_changed {
        // The only fields that can clobber a latched QSO. Defer while a QSO is
        // active; otherwise safe to pick up.
        return if qso_active {
            ConfigReloadApplicability::DeferQsoLatched
        } else {
            ConfigReloadApplicability::SafeQuiescent
        };
    }

    // No QSO-latched field changed. Detect whether *anything* changed at all so
    // a no-op reload (file touched, content identical) doesn't churn. We avoid
    // requiring `PartialEq` on every config section by comparing serialized
    // forms; a reload that changed only live-safe sections (UI / network /
    // audio / rig) is safe to apply now and can never touch latched QSO state.
    let unchanged = match (toml::to_string(old), toml::to_string(new)) {
        (Ok(a), Ok(b)) => a == b,
        // If we can't serialize for comparison, assume something changed and
        // treat it as live-safe (latched fields already ruled out above).
        _ => false,
    };
    if unchanged {
        ConfigReloadApplicability::NoChange
    } else {
        ConfigReloadApplicability::SafeLive
    }
}

/// C20 — RF-present-but-zero-decodes detector (mode / clock fault).
///
/// Per JTDX guidance: strong RF energy with zero decodes over several slots
/// usually means the wrong mode (FT8 vs FT4) or a bad system clock (DT way
/// off). This monitor watches the per-window DSP RMS and the running decode
/// count and raises an operator warning when there is clear signal energy but
/// no decodes for [`RfNoDecodeMonitor::WARN_AFTER_SLOTS`] consecutive slots.
///
/// Inputs are the **cumulative** health atomics the pipeline already maintains
/// (`health_dsp_windows`, `health_total_decodes`) plus the latest per-window
/// RMS (`health_last_rms`). The monitor is fed once per relay health tick and
/// derives per-slot behavior from the deltas, so it lives entirely off the
/// existing telemetry — no changes to the hot DSP/FT8 threads.
#[derive(Debug, Clone)]
pub struct RfNoDecodeMonitor {
    last_windows: u64,
    last_decodes: u64,
    /// Consecutive slots seen with RF present but zero decodes.
    consecutive: u32,
    /// Whether the warning is currently latched (so we emit on edges only).
    warning_active: bool,
    /// Consecutive slots seen with the input at digital silence (RMS≈0).
    consecutive_silent: u32,
    /// Whether the silent-input warning is currently latched.
    silent_active: bool,
    initialized: bool,
}

/// Warning-state edges returned by [`RfNoDecodeMonitor::observe`]. Each field
/// is `Some(true)` when that warning turns **on**, `Some(false)` when it turns
/// **off**, and `None` when there's no change (so the caller emits only on
/// edges). The two warnings are independent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthEdges {
    /// "RF present but no decodes" (likely wrong mode / bad clock).
    pub rf_no_decode: Option<bool>,
    /// "Input audio is silent" — RMS≈0 for several slots while the stream is
    /// running. Distinct from a quiet-but-live band (which has a noise floor
    /// above the silence threshold). Points at a device/permission/routing
    /// problem rather than band conditions.
    pub silent_input: Option<bool>,
}

impl Default for RfNoDecodeMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl RfNoDecodeMonitor {
    /// RMS floor (raw, un-normalized FT8 window RMS as computed in `dsp.rs`)
    /// above which we consider RF to be present. A genuinely quiet band sits
    /// well below this, so a quiet band never raises the warning. Calibrated
    /// conservatively: only clear signal energy counts.
    pub const RF_PRESENT_RMS_FLOOR: f32 = 0.02;

    /// Number of consecutive RF-present / zero-decode slots before warning.
    /// Several slots avoids false alarms on a single noisy/empty slot while
    /// still catching a persistent mode/clock fault within ~1 minute.
    pub const WARN_AFTER_SLOTS: u32 = 4;

    /// RMS ceiling below which the input is treated as **digital silence**
    /// (not merely a quiet band). A live soundcard/CODEC always carries some
    /// self-noise above this; an RMS this close to zero means the stream is
    /// running but carrying all-zero samples — the classic signature of a
    /// muted/missing device, denied microphone permission, or a remote-desktop
    /// client (e.g. Jump Desktop) having grabbed the CODEC. Well below
    /// `RF_PRESENT_RMS_FLOOR`, so the two states never overlap.
    pub const SILENT_RMS_CEILING: f32 = 0.0005;

    /// Create a fresh monitor.
    pub fn new() -> Self {
        Self {
            last_windows: 0,
            last_decodes: 0,
            consecutive: 0,
            warning_active: false,
            consecutive_silent: 0,
            silent_active: false,
            initialized: false,
        }
    }

    /// Whether the RF-present/no-decode warning is currently latched on.
    pub fn warning_active(&self) -> bool {
        self.warning_active
    }

    /// Whether the silent-input warning is currently latched on.
    pub fn silent_input_active(&self) -> bool {
        self.silent_active
    }

    /// Current consecutive RF-present/no-decode slot count (for tests/inspection).
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }

    /// Feed the latest cumulative telemetry. Returns `Some(true)` when the
    /// warning transitions **on**, `Some(false)` when it transitions **off**,
    /// and `None` when there is no edge (so the caller emits only on change).
    ///
    /// - `dsp_windows`: cumulative count of FT8 windows the DSP has produced.
    /// - `total_decodes`: cumulative count of decodes.
    /// - `last_rms`: most recent per-window RMS.
    pub fn observe(
        &mut self,
        dsp_windows: u64,
        total_decodes: u64,
        last_rms: f32,
    ) -> HealthEdges {
        // First observation just seeds the baseline; we can't compute a delta.
        if !self.initialized {
            self.last_windows = dsp_windows;
            self.last_decodes = total_decodes;
            self.initialized = true;
            return HealthEdges::default();
        }

        let windows_delta = dsp_windows.saturating_sub(self.last_windows);
        let decodes_delta = total_decodes.saturating_sub(self.last_decodes);
        self.last_windows = dsp_windows;
        self.last_decodes = total_decodes;

        // No new window ran since last tick — nothing to judge this tick.
        if windows_delta == 0 {
            return HealthEdges::default();
        }

        let rf_present = last_rms >= Self::RF_PRESENT_RMS_FLOOR;
        let silent = last_rms < Self::SILENT_RMS_CEILING;
        let zero_decodes = decodes_delta == 0;

        // RF-present / no-decode streak (wrong mode / bad clock).
        if rf_present && zero_decodes {
            self.consecutive = self.consecutive.saturating_add(1);
        } else {
            // Either the band is quiet (no RF) or we decoded something — the
            // pipeline is healthy; reset the streak.
            self.consecutive = 0;
        }

        // Silent-input streak (RMS≈0 = device/permission/routing problem).
        if silent {
            self.consecutive_silent = self.consecutive_silent.saturating_add(1);
        } else {
            self.consecutive_silent = 0;
        }

        let rf_no_decode = {
            let should_warn = self.consecutive >= Self::WARN_AFTER_SLOTS;
            if should_warn && !self.warning_active {
                self.warning_active = true;
                Some(true)
            } else if !should_warn && self.warning_active {
                self.warning_active = false;
                Some(false)
            } else {
                None
            }
        };

        let silent_input = {
            let should_warn = self.consecutive_silent >= Self::WARN_AFTER_SLOTS;
            if should_warn && !self.silent_active {
                self.silent_active = true;
                Some(true)
            } else if !should_warn && self.silent_active {
                self.silent_active = false;
                Some(false)
            } else {
                None
            }
        };

        HealthEdges {
            rf_no_decode,
            silent_input,
        }
    }
}

impl super::ApplicationCoordinator {
    /// Start coordinator management tasks
    pub(crate) async fn start_coordinator_tasks(&mut self) -> Result<()> {
        // Initialize component status for all registered task handles
        {
            let mut status_map = self.component_status.write().await;
            for (id, _) in &self.named_task_handles {
                status_map
                    .entry(*id)
                    .or_insert_with(ComponentStatus::new_running);
            }
        }

        // Health monitoring task -- checks task handles and message bus health
        let health_handle = self.start_health_monitor().await;

        // Configuration hot-reload task.
        //
        // C19 — by design this task does NOT apply any reloaded config to the
        // running QSO / autonomous state. Config is snapshotted once at startup
        // into the task-local `QsoManager` (callsign/grid, owned by value with
        // no setter) and `AutonomousOperator`; nothing here rebuilds them or
        // mutates a latched partner/`tx_parity`. This is the C19 guarantee:
        // a hot-reload can never clobber an in-progress QSO or drop active
        // QSOs, because no reload path reaches QSO state. Should a real apply
        // handler ever be wired here, it MUST gate every change through
        // `classify_config_reload(...)` and refuse/defer `DeferQsoLatched`
        // changes while a QSO is active (see `active_tx_qsos`).
        let config_handle = {
            let _config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    sleep(Duration::from_secs(1)).await;
                }
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Coordinator, health_handle));
        self.named_task_handles
            .push((ComponentId::Coordinator, config_handle));

        Ok(())
    }

    /// Start the health monitor task.
    ///
    /// Runs every `health_check_interval` (5s) and:
    /// 1. Reads the component_status map (populated by `check_task_handles()`)
    /// 2. Sends a health status summary to the TUI via the message bus
    ///
    /// Note: heartbeat checking via `message_bus.get_component_health()` was
    /// removed because no component ever sends heartbeat messages. Failure
    /// detection is handled by `check_task_handles()` in the main loop.
    pub(crate) async fn start_health_monitor(&self) -> JoinHandle<Result<()>> {
        let message_bus = self.message_bus.clone();
        let shutdown = self.shutdown_signal.clone();
        let component_status = self.component_status.clone();
        let mut health_interval = interval(Duration::from_secs(5));

        tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                health_interval.tick().await;

                // Build a status summary from the component_status map
                // (populated by check_task_handles in the main loop)
                let status_map = component_status.read().await;
                let mut summary_parts: Vec<String> = Vec::new();
                let mut any_failed = false;

                for (id, status) in status_map.iter() {
                    match &status.state {
                        ComponentState::Running => {
                            // Component is fine
                        }
                        ComponentState::Failed(err) => {
                            any_failed = true;
                            summary_parts.push(format!("{}: {}", id, err));
                        }
                        ComponentState::NotStarted => {
                            // Not started / disabled -- don't report
                        }
                    }
                }

                // Send health summary to TUI
                if any_failed {
                    let summary = format!("Degraded -- {}", summary_parts.join("; "));
                    let msg = ComponentMessage::new(
                        ComponentId::Coordinator,
                        ComponentId::Tui,
                        MessageType::StatusUpdate(summary),
                        Instant::now(),
                    );
                    if let Err(e) = message_bus.send_message(msg).await {
                        debug!("Failed to send health summary to TUI: {}", e);
                    }
                }
            }

            Ok(())
        })
    }

    /// Main application loop
    ///
    /// Periodically checks task handles for unexpected termination and updates
    /// the component_status map so the health monitor can report to the TUI.
    pub(crate) async fn run_main_loop(&mut self) -> Result<()> {
        info!("Entering main application loop");

        let mut stats_interval = interval(Duration::from_secs(30));
        let mut health_check_interval = interval(Duration::from_secs(5));

        while !self.shutdown_signal.load(Ordering::Acquire) {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.log_performance_stats().await;
                }
                _ = health_check_interval.tick() => {
                    self.check_task_handles().await;
                }
                _ = sleep(Duration::from_millis(100)) => {
                    // Main loop iteration
                }
            }
        }

        info!("Main application loop completed");
        Ok(())
    }

    /// Check all named task handles for unexpected termination.
    ///
    /// When a component task finishes (is_finished() == true), we inspect
    /// the result and update the component_status map. The health monitor
    /// task picks this up on its next cycle and reports to the TUI.
    ///
    /// Graceful degradation: no single component failure shuts down the
    /// application. Critical components are logged at error level, others
    /// at warn level.
    pub(crate) async fn check_task_handles(&mut self) {
        for (component_id, handle) in &self.named_task_handles {
            // Skip coordinator's own tasks and already-known failures
            if *component_id == ComponentId::Coordinator {
                continue;
            }

            if !handle.is_finished() {
                // Task is still running -- update last_seen
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(component_id) {
                    if status.state == ComponentState::Running {
                        status.last_seen = Instant::now();
                    }
                }
                continue;
            }

            // Task has finished -- check if we already know about it
            let mut status_map = self.component_status.write().await;
            let status = status_map
                .entry(*component_id)
                .or_insert_with(ComponentStatus::new_running);

            if status.state != ComponentState::Running {
                // Already recorded this failure
                continue;
            }

            // First time seeing this component as finished
            let degradation = degradation_message(*component_id);
            let criticality = component_criticality(*component_id);

            status.error_count += 1;
            status.state = ComponentState::Failed(degradation.to_string());

            match criticality {
                ComponentCriticality::Important => {
                    error!(
                        "CRITICAL component {} has stopped unexpectedly: {}",
                        component_id, degradation
                    );
                }
                ComponentCriticality::NonCritical => {
                    warn!("Component {} has stopped: {}", component_id, degradation);
                }
            }

            // For Hamlib failure: ensure PTT defaults to off for safety
            if *component_id == ComponentId::Hamlib {
                warn!("PTT safety: forcing PTT off due to Hamlib disconnect");
                let ptt_off_msg = ComponentMessage::new(
                    ComponentId::Coordinator,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: false,
                    }),
                    Instant::now(),
                );
                // Best-effort: channel may be disconnected
                let _ = self.message_bus.send_message(ptt_off_msg).await;
            }

            // Notify TUI of the component failure
            let error_msg = ComponentMessage::new(
                ComponentId::Coordinator,
                ComponentId::Tui,
                MessageType::StatusUpdate(format!("{}: {}", component_id, degradation)),
                Instant::now(),
            );
            let _ = self.message_bus.send_message(error_msg).await;
        }
    }

    /// Log performance statistics
    pub(crate) async fn log_performance_stats(&self) {
        let message_count = self.message_count.load(Ordering::Relaxed);
        let uptime = self.startup_time.elapsed();

        let audio_status = {
            let timestamp = self.last_audio_timestamp.read().await;
            match *timestamp {
                Some(ts) => format!("active (last: {:.2}s ago)", ts.elapsed().as_secs_f64()),
                None => "inactive".to_string(),
            }
        };

        let decode_status = {
            let timestamp = self.last_decode_timestamp.read().await;
            match *timestamp {
                Some(ts) => format!("active (last: {:.2}s ago)", ts.elapsed().as_secs_f64()),
                None => "inactive".to_string(),
            }
        };

        info!(
            "Performance stats - Uptime: {:.0}s, Messages: {}, Audio: {}, Decode: {}",
            uptime.as_secs_f64(),
            message_count,
            audio_status,
            decode_status
        );
    }
}
