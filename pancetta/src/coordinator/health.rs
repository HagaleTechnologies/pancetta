use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::{
    component_criticality, component_restart_policy, degradation_message, ComponentCriticality,
    ComponentState, ComponentStatus, RestartPolicy,
};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// Count of panics observed process-wide since startup, via the top-level
/// panic hook installed by `main.rs`'s `install_panic_hook`. Lives here
/// (not in `main.rs`) because `main.rs` is the separate `pancetta` BIN
/// crate: a lib crate's sibling modules (like `tui_relay.rs`, which reads
/// this counter for the Layer 3 health panel) can already see a private
/// `coordinator::health` module fine — the real constraint is that
/// `main.rs`, being a DIFFERENT crate, can only reach this counter through
/// a `pub` path. [`record_panic`]/[`panic_count`] are re-exported at
/// `coordinator::{record_panic, panic_count}` for exactly that reason,
/// while `mod health` itself and this static both stay non-`pub` so nothing
/// else in this file becomes part of `pancetta_lib`'s public surface.
/// docs/task-supervision-plan.md item 4 ("panics are never silent") —
/// mirrors the existing `DECODE_PANIC_COUNT` pattern in `coordinator/ft8.rs`,
/// but process-wide rather than scoped to the two catch_unwind sites in the
/// decode loop.
static PANIC_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Increment the process-wide panic counter, returning the new count. Called
/// from `main.rs`'s `install_panic_hook` closure (the bin crate) — the only
/// mutation point, kept as a function rather than a public static so the
/// counter itself stays private to this module.
pub fn record_panic() -> u64 {
    PANIC_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// Read-only accessor for the Layer 3 health panel (`coordinator/tui_relay.rs`).
pub fn panic_count() -> u64 {
    PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

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
    pub fn observe(&mut self, dsp_windows: u64, total_decodes: u64, last_rms: f32) -> HealthEdges {
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
        // Perf (Pass 1 / infra-A5): the former `config_handle` here was a
        // do-nothing placeholder task (`while !shutdown { sleep(1s) }`) with no
        // config-apply logic wired — a pure 1 Hz wakeup forever. Removed. If a
        // real hot-reload apply handler is ever added, it MUST gate every
        // change through `classify_config_reload(...)` and refuse/defer
        // `DeferQsoLatched` changes while a QSO is active (see `active_tx_qsos`).

        self.named_task_handles
            .push((ComponentId::Coordinator, health_handle));

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
                _ = sleep(Duration::from_secs(1)) => {
                    // Perf (Pass 1 / infra-A4): this arm only bounds how quickly
                    // the `while !shutdown` guard is re-checked — it does no work.
                    // It was firing 10×/s (100ms); 1s cuts that to 1/s with no
                    // behavior change beyond at-most-1s extra shutdown latency on
                    // this top-level housekeeping loop. (Not removed outright:
                    // that would defer the shutdown check to the 5s health tick.)
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
        // Collect indices of finished, non-Coordinator handles first (a
        // read-only pass over `named_task_handles` by reference). We must
        // NOT restart a component while still holding this by-reference
        // iteration: restart dispatch needs `&mut self` (it pushes a new
        // handle back onto `named_task_handles` via the component's own
        // `start_*_component`), which would conflict with an outstanding
        // shared borrow of the vector. So instead we collect indices here,
        // drop the borrow, then remove-and-process one at a time below.
        let finished_indices: Vec<usize> = self
            .named_task_handles
            .iter()
            .enumerate()
            .filter(|(_, (id, handle))| *id != ComponentId::Coordinator && handle.is_finished())
            .map(|(i, _)| i)
            .collect();

        // Remove from the back forward so earlier indices stay valid as
        // later ones are removed.
        for &i in finished_indices.iter().rev() {
            let (component_id, handle) = self.named_task_handles.remove(i);
            self.handle_finished_task(component_id, handle).await;
        }

        // Unchanged: update last_seen for still-running handles.
        for (component_id, handle) in &self.named_task_handles {
            if !handle.is_finished() {
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(component_id) {
                    if status.state == ComponentState::Running {
                        status.last_seen = Instant::now();
                    }
                }
            }
        }
    }

    /// Handle a single finished (no longer running) task: classify the
    /// outcome, apply the Hamlib PTT-off safety behavior verbatim, and
    /// either dispatch a restart (Task 2's `component_restart_policy` +
    /// Task 3's `RestartBudget`) or degrade to `Failed`, exactly as the
    /// pre-restructure `check_task_handles` did for every non-restartable
    /// component.
    async fn handle_finished_task(
        &mut self,
        component_id: ComponentId,
        handle: tokio::task::JoinHandle<Result<()>>,
    ) {
        let outcome = handle.await;
        let is_clean_exit = matches!(outcome, Ok(Ok(())));

        {
            let mut status_map = self.component_status.write().await;
            let status = status_map
                .entry(component_id)
                .or_insert_with(ComponentStatus::new_running);
            if status.state != ComponentState::Running {
                // Already recorded this failure/restart-exhaustion.
                return;
            }
            status.error_count += 1;
        }

        if is_clean_exit {
            // Intentional exit (e.g. a disabled component's drain task) --
            // do not restart, do not mark Failed either. Log at info, not
            // warn/error.
            info!("Component {} exited cleanly, not restarting", component_id);
            return;
        }

        let degradation = degradation_message(component_id);

        // For Hamlib failure: ensure PTT defaults to off for safety.
        // Preserved verbatim from the pre-restructure logic.
        if component_id == ComponentId::Hamlib {
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

        match component_restart_policy(component_id) {
            RestartPolicy::Restartable
                if self
                    .restart_budget
                    .may_restart(component_id, Instant::now()) =>
            {
                let backoff = self
                    .restart_budget
                    .record_attempt_and_backoff(component_id, Instant::now());
                crate::coordinator::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Warn,
                    format!(
                        "{} {} -- restarting in {:?}",
                        component_id, degradation, backoff
                    ),
                    None,
                )
                .await;
                tokio::time::sleep(backoff).await;
                match self.restart_component(component_id).await {
                    Ok(()) => {
                        info!("Component {} restarted successfully", component_id);
                    }
                    Err(e) => {
                        error!("Component {} restart failed: {}", component_id, e);
                        let mut status_map = self.component_status.write().await;
                        if let Some(status) = status_map.get_mut(&component_id) {
                            status.state = ComponentState::Failed(degradation.to_string());
                        }
                        self.notify_tui_of_failure(component_id, degradation).await;
                    }
                }
            }
            RestartPolicy::Restartable => {
                // Budget exhausted -- degrade instead of restarting again.
                warn!(
                    "Component {} exceeded restart budget -- degrading",
                    component_id
                );
                crate::coordinator::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Error,
                    format!(
                        "{} restarted too many times -- giving up, needs a manual restart",
                        component_id
                    ),
                    None,
                )
                .await;
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(&component_id) {
                    status.state = ComponentState::Failed(degradation.to_string());
                }
                drop(status_map);
                self.notify_tui_of_failure(component_id, degradation).await;
            }
            RestartPolicy::DegradeOnly | RestartPolicy::FatalAbort => {
                // ComponentCriticality-based log-level split, preserved
                // from the pre-restructure behavior.
                match component_criticality(component_id) {
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
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(&component_id) {
                    status.state = ComponentState::Failed(degradation.to_string());
                }
                drop(status_map);
                self.notify_tui_of_failure(component_id, degradation).await;
            }
        }
    }

    /// Best-effort immediate TUI notification of a component transitioning
    /// to `Failed`, preserved from the pre-restructure `check_task_handles`
    /// (which sent this for every finished, non-restartable component).
    /// The 5s periodic health-summary tick (`start_health_monitor`) also
    /// reports any `Failed` component, so this is a lower-latency nudge,
    /// not the only path.
    async fn notify_tui_of_failure(&self, component_id: ComponentId, degradation: &str) {
        let error_msg = ComponentMessage::new(
            ComponentId::Coordinator,
            ComponentId::Tui,
            MessageType::StatusUpdate(format!("{}: {}", component_id, degradation)),
            Instant::now(),
        );
        let _ = self.message_bus.send_message(error_msg).await;
    }

    /// Re-invoke the given component's start method. Only the 5 components
    /// this plan covers are wired; anything else reaching here is a bug
    /// (Task 2's `component_restart_policy` should have already routed it
    /// to `DegradeOnly`).
    async fn restart_component(&mut self, id: ComponentId) -> Result<()> {
        match id {
            ComponentId::Autonomous => self.start_autonomous_component().await,
            ComponentId::DxCluster => self.start_dx_cluster_component().await,
            ComponentId::PskReporter => self.start_pskreporter_component().await,
            ComponentId::RemoteGateway => self.start_remote_gateway_component().await,
            ComponentId::Qso => self.start_qso_component().await,
            other => anyhow::bail!("restart_component called for non-restartable {other}"),
        }
    }

    /// Log performance statistics
    pub(crate) async fn log_performance_stats(&self) {
        let message_count = self.message_count.load(Ordering::Relaxed);
        let uptime = self.startup_time.elapsed();

        let audio_status = {
            // Perf (Pass 1 / A10): lock-free read of the epoch-ms atomic.
            let ms = self.last_audio_timestamp.load(Ordering::Relaxed);
            if ms == 0 {
                "inactive".to_string()
            } else {
                let ago_s = super::now_epoch_ms().saturating_sub(ms) as f64 / 1000.0;
                format!("active (last: {:.2}s ago)", ago_s)
            }
        };

        let decode_status = {
            let ms = self
                .last_decode_timestamp
                .load(std::sync::atomic::Ordering::Relaxed);
            if ms == 0 {
                "inactive".to_string()
            } else {
                let ago_s = super::now_epoch_ms().saturating_sub(ms) as f64 / 1000.0;
                format!("active (last: {:.2}s ago)", ago_s)
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

#[cfg(test)]
mod supervisor_tests {
    use super::super::ApplicationCoordinator;
    use crate::message_bus::ComponentId;
    use pancetta_config::Config;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Mirrors the direct-construction pattern used by `mod.rs`'s
    /// `test_coordinator_creation` / `autonomous.rs`'s `build_coordinator`:
    /// `Config::default()`, `no_audio=true`, `headless=true`. No shared
    /// helper exists yet for `health.rs` (this is its first test module),
    /// so this is a local copy of that same pattern rather than a new one.
    async fn test_coordinator() -> ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// Task 5's central regression guard. DxCluster is `Restartable`
    /// (Task 2's `component_restart_policy`) so a panicking task under
    /// `ComponentId::DxCluster` must be restarted, not just marked
    /// `Failed`, by `check_task_handles`.
    ///
    /// The pre-created channel is the load-bearing part of this test: it
    /// simulates "this component already ran once" so the restart path
    /// exercises the SECOND registration for `ComponentId::DxCluster`.
    /// `Config::default()` has DX cluster disabled, so the real
    /// `start_dx_cluster_component` restart call takes its cheap
    /// disabled-branch return (channel re-create + immediate `Ok(())`),
    /// no network I/O — safe to invoke as the actual production entry
    /// point rather than a fixture stand-in. Before Task 1's
    /// `get_or_create_channel` wiring (Step 1 above), this second
    /// registration would error with "Channel already exists" and the
    /// restart would silently fail, leaving the component `Failed`.
    #[tokio::test]
    async fn panicking_restartable_component_is_restarted_after_backoff() {
        let mut coordinator = test_coordinator().await;

        // Pre-create DxCluster's channel, simulating "this component
        // already ran once".
        coordinator
            .message_bus
            .create_channel(ComponentId::DxCluster)
            .await
            .unwrap();

        // Spawn a fake task under ComponentId::DxCluster that panics
        // immediately.
        let handle = tokio::spawn(async {
            panic!("injected test panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        coordinator
            .named_task_handles
            .push((ComponentId::DxCluster, handle));

        // Give the spawned task a chance to actually panic and finish
        // before polling — `tokio::spawn` does not guarantee the task has
        // run by the time control returns here, so without this the poll
        // below can race and see `is_finished() == false`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First poll: detects the panic, classifies it, restarts.
        coordinator.check_task_handles().await;

        let status = coordinator.component_status.read().await;
        // After a successful restart, the component is Running again, not
        // Failed.
        assert_eq!(
            status.get(&ComponentId::DxCluster).map(|s| &s.state),
            Some(&super::ComponentState::Running)
        );
    }
}
