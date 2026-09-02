use anyhow::Result;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::{
    component_criticality, component_restart_policy, degradation_message, ComponentCriticality,
    ComponentState, ComponentStatus, RestartPolicy,
};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

struct TxInhibitGuard {
    counter: Option<Arc<AtomicU32>>,
}

impl TxInhibitGuard {
    fn for_component(component: ComponentId, counter: Arc<AtomicU32>) -> Self {
        if component == ComponentId::Hamlib {
            counter.fetch_add(1, Ordering::AcqRel);
            Self {
                counter: Some(counter),
            }
        } else {
            Self { counter: None }
        }
    }
}

impl Drop for TxInhibitGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

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
        let mut hamlib_reconnect_rx = self
            .hamlib_reconnect_rx
            .take()
            .expect("run_main_loop must only be called once per coordinator lifetime");

        while !self.shutdown_signal.load(Ordering::Acquire) {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.log_performance_stats().await;
                }
                _ = health_check_interval.tick() => {
                    self.check_task_handles().await;
                }
                Some(req) = hamlib_reconnect_rx.recv() => {
                    self.handle_hamlib_reconnect_request(req).await;
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
        let shutting_down = self.shutdown_signal.load(Ordering::Acquire);
        let unexpected_clean_exit = !shutting_down
            && ((component_id == ComponentId::Audio && self.audio_path_supervised)
                || matches!(
                    component_id,
                    ComponentId::Hamlib | ComponentId::StationAgent
                ));
        let is_clean_exit = matches!(outcome, Ok(Ok(()))) && !unexpected_clean_exit;

        // PAN-33: `start_*_component` (Hamlib in particular) can register a
        // fresh task handle in `named_task_handles` and THEN bail (a
        // startup-window crash/timeout in its own readiness handshake) --
        // that handle is already finished, so a LATER health pass
        // rediscovers it here, but by then the FIRST processing of the
        // original crash already marked `status.state` `Failed` via
        // `restart_component`'s own `Err(e)` branch. Unconditionally
        // returning below would silently swallow that rediscovery forever,
        // even with `RestartBudget` attempts left -- permanently stranding
        // the component. Only treat this as a genuine no-op when there is
        // truly nothing left to try (not restartable, or budget exhausted);
        // otherwise fall through and give it a real, freshly-budgeted
        // restart attempt below, exactly as if this were a fresh crash.
        let can_retry_despite_prior_failure = matches!(
            component_restart_policy(component_id),
            RestartPolicy::Restartable
        ) && self
            .restart_budget
            .may_restart(component_id, Instant::now());

        {
            let mut status_map = self.component_status.write().await;
            let status = status_map
                .entry(component_id)
                .or_insert_with(ComponentStatus::new_running);
            if status.state != ComponentState::Running && !can_retry_despite_prior_failure {
                // Already recorded this failure/restart-exhaustion, and
                // there is no budget left to retry with -- genuine no-op.
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

        let tx_inhibit =
            TxInhibitGuard::for_component(component_id, self.tx_restart_inhibit.clone());
        // PAN-19 MEDIUM #3: `tx_inhibit`'s `Drop` un-inhibits TX -- correct
        // for a normal successful restart, but WRONG whenever this function
        // ends in a terminal Hamlib failure (hard restart error, budget
        // exhausted, or a non-restartable policy): in every one of those
        // cases Hamlib is dead for good and no task is left consuming the
        // Hamlib bus channel, so `SetPtt` will never reach the rig again.
        // Releasing the inhibit there would un-mute TX with zero PTT
        // control -- contradicting "TX stays inhibited throughout a Hamlib
        // restart window" (AGENTS.md), and this isn't even a bounded
        // "window" in that case, it's permanent. Each terminal-failure arm
        // below sets this so the guard is leaked (never decremented)
        // instead of dropped normally; the one arm that DOES represent a
        // real recovery (`Ok(())` from `restart_component`) leaves it
        // false, so the guard still releases normally there. For any
        // component other than Hamlib the guard is already a documented
        // no-op (`counter: None`), so leaking it is harmless.
        let mut leak_tx_inhibit = false;

        // Task 6 (task-supervision): a crashed Qso-component task drops
        // whatever QSOs were in-flight at that moment -- the fresh
        // `QsoManager` a restart constructs starts with an empty map, so
        // without this those QSOs would just silently vanish from the
        // operator's view. Surface each with the component-specific failure
        // through the manager's real state machine (`fail_qso`), reading off
        // the cheap `Arc`-backed clone the coordinator stashed at
        // `start_qso_component` time -- that clone shares the crashed
        // task's `qsos` map and stays valid/readable regardless of whether
        // the restart below actually happens (budget-exhausted degrade
        // still means the in-flight QSOs are gone). Runs before the restart
        // dispatch so this fires exactly once per crash, not once per
        // (possibly repeated) restart attempt.
        //
        // Fix-review followup (2026-07-25): this MUST run on the pre-restart
        // stashed clone, and MUST run before `restart_component` below --
        // do not "fix" this into restart-then-fail. `fail_qso` looks the
        // QSO up in the clone's OWN `qsos` map; a post-restart `QsoManager`
        // is a fresh `QsoManager::new()` with an empty map (and its own new
        // `event_sender`), so calling `fail_qso` on it for a pre-restart
        // qso_id would silently no-op -- no manager instance both knows the
        // QSO's metadata and feeds a guaranteed-fresh subscriber. This is
        // safe as-is because the `QsoFailed` broadcast's real consumer -- the
        // `tokio::spawn`ed forwarder inside `start_qso_component`
        // (qso.rs ~L1583) that relays it to `MessageType::RecentQsoOutcome`
        // -- is a genuinely independent task (its `JoinHandle` is discarded,
        // never `.abort()`-ed), so it survives the "outer" Qso task's panic
        // per tokio's task-independence guarantee and stays subscribed to
        // the SAME `event_sender` this stashed clone shares (every
        // `QsoManager::clone()` shares one `Arc`-backed `qsos` map and one
        // `broadcast::Sender`). Proved end-to-end (real `start_qso_component`,
        // real forwarder, real Tui bus channel -- not a fresh test
        // subscriber) by
        // `qso_restart_delivers_recent_qso_outcome_through_the_real_forwarder`
        // below.
        if let Some((scope, reason)) = super::qso_drop_for(component_id) {
            self.fail_qsos_dropped_by(component_id, scope, reason).await;
        }

        self.teardown_component(component_id).await;

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
                        // PAN-33: this call can be a rediscovery-driven retry
                        // (see the early-return guard above) of a component
                        // the FIRST processing of its crash already marked
                        // `Failed` -- that pass also leaked a `TxInhibitGuard`
                        // increment (Hamlib only) believing the component was
                        // dead for good. Reaching a genuine success here means
                        // it wasn't: reset `status.state` back to `Running`
                        // (nothing else does, since a first-time crash that
                        // restarts cleanly never left `Failed` set to begin
                        // with) and pay back every such leaked increment now,
                        // on top of this call's own `tx_inhibit` guard
                        // dropping normally below -- otherwise TX would stay
                        // permanently inhibited even after Hamlib recovers.
                        {
                            let mut status_map = self.component_status.write().await;
                            if let Some(status) = status_map.get_mut(&component_id) {
                                status.state = ComponentState::Running;
                            }
                        }
                        if component_id == ComponentId::Hamlib && self.hamlib_leaked_tx_inhibits > 0
                        {
                            let owed = std::mem::take(&mut self.hamlib_leaked_tx_inhibits);
                            self.tx_restart_inhibit.fetch_sub(owed, Ordering::AcqRel);
                        }
                    }
                    Err(e) => {
                        error!("Component {} restart failed: {}", component_id, e);
                        {
                            let mut status_map = self.component_status.write().await;
                            if let Some(status) = status_map.get_mut(&component_id) {
                                status.state = ComponentState::Failed(degradation.to_string());
                            }
                        }
                        self.notify_tui_of_failure(component_id, degradation).await;
                        // Terminal: the restart itself failed, no further
                        // automatic attempt follows from here.
                        leak_tx_inhibit = true;
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
                crate::coordinator::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Error,
                    format!("{component_id} {degradation}"),
                    None,
                )
                .await;
                self.notify_tui_of_failure(component_id, degradation).await;
                // Terminal: budget exhausted, no further automatic restart
                // will ever be attempted for this component.
                leak_tx_inhibit = true;
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
                crate::coordinator::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Error,
                    format!("{component_id} {degradation}"),
                    None,
                )
                .await;
                self.notify_tui_of_failure(component_id, degradation).await;
                // Terminal: not restartable at all. Not currently reachable
                // for Hamlib (`component_restart_policy` always returns
                // `Restartable` for it), but included for defense-in-depth
                // should that policy ever change.
                leak_tx_inhibit = true;
            }
        }

        if leak_tx_inhibit {
            // PAN-33: track every leaked increment so a LATER successful
            // restart (reached via the early-return guard's budget-aware
            // fallthrough above, discovering this same crash on a
            // subsequent health pass) knows exactly how much to pay back --
            // see that `Ok(())` arm's comment.
            if component_id == ComponentId::Hamlib {
                self.hamlib_leaked_tx_inhibits += 1;
            }
            std::mem::forget(tx_inhibit);
        }
    }

    async fn fail_qsos_dropped_by(
        &self,
        component_id: ComponentId,
        scope: super::QsoDropScope,
        reason: pancetta_qso::QsoFailureReason,
    ) {
        let Some(manager) = self.qso_manager_for_supervisor.clone() else {
            return;
        };
        let mut dropped = 0usize;
        for (qso_id, progress) in manager.get_active_qsos().await {
            if scope == super::QsoDropScope::RemoteOnly && !progress.metadata.remote_origin {
                continue;
            }
            match manager.fail_qso(qso_id, reason.clone()).await {
                Ok(()) => dropped += 1,
                Err(error) => {
                    error!(
                        %qso_id,
                        %component_id,
                        %error,
                        "failed to drop in-flight QSO after component crash"
                    );
                }
            }
        }
        if dropped > 0 {
            crate::coordinator::tx::emit_diagnostic(
                &self.message_bus,
                "supervisor",
                pancetta_core::DiagnosticLevel::Warn,
                format!("{component_id} crashed -- dropped {dropped} in-flight QSO(s)"),
                None,
            )
            .await;
        }
    }

    async fn teardown_component(&mut self, component_id: ComponentId) {
        match component_id {
            #[cfg(feature = "pancetta-hamlib")]
            ComponentId::Hamlib => self.teardown_hamlib().await,
            ComponentId::StationAgent => {
                if let Some(poll) = self.station_agent_poll.take() {
                    poll.abort();
                }
                self.fail_safe_disarm_remote_tx("station-agent component crash")
                    .await;
            }
            _ => {}
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

    /// Re-invoke the given component's start method. Only the seven components
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
            ComponentId::Dsp => self.start_dsp_pipeline().await,
            ComponentId::Ft8Decoder => self.start_ft8_pipeline().await,
            #[cfg(feature = "pancetta-hamlib")]
            ComponentId::Hamlib => self.start_hamlib_component().await,
            ComponentId::StationAgent => self.start_station_agent_component().await,
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
    use super::TxInhibitGuard;
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType};
    use pancetta_config::Config;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    /// Mirrors the direct-construction pattern used by `mod.rs`'s
    /// `test_coordinator_creation` / `autonomous.rs`'s `build_coordinator`:
    /// `Config::default()`, `no_audio=true`, `headless=true`. No shared
    /// helper exists yet for `health.rs` (this is its first test module),
    /// so this is a local copy of that same pattern rather than a new one —
    /// EXCEPT for `replay_path`, which (unlike those other copies) is
    /// deliberately `Some(..)`: several tests below call the real
    /// `start_qso_component`, which in its normal (non-`--replay`) mode
    /// opens/creates the operator's real `~/.pancetta/qsos.adi` and
    /// `qso.db` (PAN-41). Mirrors `coordinator::mod`'s own
    /// `build_coordinator_with_replay` / `coordinator::qso`'s
    /// `replay_local_log_tests::test_coordinator` pattern — the SAME dummy,
    /// never-read path value — so `self.replay_mode()` is `true` and the
    /// real logbook is never touched for WRITES.
    ///
    /// PAN-41 round 4: `replay_path` above only satisfies `replay_mode()` --
    /// it is never itself read as a filesystem path. `--replay`'s own
    /// documented contract is to still READ real history for duplicate/
    /// DX-Hunter seeding, so without `pancetta_home_override` these tests
    /// would read whatever real `~/.pancetta/qsos.adi` happens to exist on
    /// the machine running them. Point that read at a per-call, process-
    /// lifetime-scoped temp dir instead -- deliberately leaked (not a
    /// `TempDir` guard) since this helper has many callers today that hold
    /// no guard for the returned coordinator; the directory is empty, so
    /// history-seeding simply finds nothing, same as a fresh install.
    async fn test_coordinator() -> ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None,                                                // no WAV
            Some(std::path::PathBuf::from("/some/capture/dir")), // --replay: never touch the real logbook
            None,                                                // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed");

        coordinator.pancetta_home_override = Some(std::env::temp_dir().join(format!(
            "pancetta-test-home-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        )));

        coordinator
    }

    async fn assert_decode_component_restarts(component: ComponentId) {
        let mut coordinator = test_coordinator().await;
        coordinator.init_decode_handles();
        crate::coordinator::pipeline::register_decode_bus_channels(&coordinator.message_bus)
            .await
            .unwrap();
        let handle = tokio::spawn(async {
            panic!("injected test panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        coordinator.named_task_handles.push((component, handle));
        for _ in 0..100 {
            if coordinator
                .named_task_handles
                .last()
                .unwrap()
                .1
                .is_finished()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(coordinator
            .named_task_handles
            .last()
            .unwrap()
            .1
            .is_finished());
        coordinator.check_task_handles().await;
        assert_eq!(
            coordinator
                .component_status
                .read()
                .await
                .get(&component)
                .map(|status| &status.state),
            Some(&super::ComponentState::Running)
        );
        assert!(coordinator
            .named_task_handles
            .iter()
            .any(|(id, _)| *id == component));
        tokio::task::yield_now().await;
        assert!(coordinator
            .named_task_handles
            .iter()
            .find(|(id, _)| *id == component)
            .is_some_and(|(_, handle)| !handle.is_finished()));
        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[tokio::test]
    async fn panicking_dsp_component_is_restarted_after_backoff() {
        assert_decode_component_restarts(ComponentId::Dsp).await;
    }

    #[tokio::test]
    async fn panicking_ft8_decoder_component_is_restarted_after_backoff() {
        assert_decode_component_restarts(ComponentId::Ft8Decoder).await;
    }

    #[tokio::test]
    async fn decode_stages_start_twice_from_self_alone() {
        let mut coordinator = test_coordinator().await;
        coordinator.init_decode_handles();
        coordinator.start_dsp_pipeline().await.unwrap();
        coordinator.start_dsp_pipeline().await.unwrap();
        coordinator.start_ft8_pipeline().await.unwrap();
        coordinator.start_ft8_pipeline().await.unwrap();
        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// `TxInhibitGuard`'s OWN Drop semantics in isolation (increments on
    /// construction, decrements on drop) -- unrelated to whether
    /// `handle_finished_task` chooses to leak it on a terminal Hamlib
    /// failure (see `hamlib_restart_budget_exhausted_leaves_tx_permanently_
    /// inhibited` below for that caller-level behavior, PAN-19 MEDIUM #3).
    #[test]
    fn budget_exhausted_degrade_still_clears_the_tx_inhibit() {
        let inhibit = Arc::new(AtomicU32::new(0));
        {
            let _guard = TxInhibitGuard::for_component(ComponentId::Hamlib, inhibit.clone());
            assert_eq!(inhibit.load(Ordering::Acquire), 1);
        }
        assert_eq!(inhibit.load(Ordering::Acquire), 0);
    }

    /// PAN-19 MEDIUM #3: once Hamlib's restart budget is exhausted,
    /// `handle_finished_task` degrades it to `Failed` permanently -- no
    /// further automatic restart will ever be attempted, so nothing is
    /// left to consume the Hamlib bus channel and `SetPtt` will never
    /// reach the rig again. `TxInhibitGuard`'s `Drop` un-inhibiting TX in
    /// that case would un-mute TX with zero PTT control. Pre-fill the
    /// restart budget so `handle_finished_task` takes the budget-exhausted
    /// branch deterministically, without needing a real restart attempt or
    /// backoff sleep.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn hamlib_restart_budget_exhausted_leaves_tx_permanently_inhibited() {
        let mut coordinator = test_coordinator().await;
        assert_eq!(coordinator.tx_restart_inhibit.load(Ordering::Acquire), 0);

        let now = std::time::Instant::now();
        for _ in 0..5 {
            coordinator
                .restart_budget
                .record_attempt_and_backoff(ComponentId::Hamlib, now);
        }
        assert!(
            !coordinator
                .restart_budget
                .may_restart(ComponentId::Hamlib, now),
            "test setup: restart budget must actually be exhausted"
        );

        let handle: tokio::task::JoinHandle<anyhow::Result<()>> =
            tokio::spawn(async { Err(anyhow::anyhow!("simulated hamlib crash")) });
        coordinator
            .named_task_handles
            .push((ComponentId::Hamlib, handle));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        coordinator.check_task_handles().await;

        assert!(
            matches!(
                coordinator
                    .component_status
                    .read()
                    .await
                    .get(&ComponentId::Hamlib)
                    .map(|s| &s.state),
                Some(super::ComponentState::Failed(_))
            ),
            "Hamlib should be Failed once its restart budget is exhausted"
        );
        assert!(
            coordinator.tx_restart_inhibit.load(Ordering::Acquire) > 0,
            "TX must stay inhibited once Hamlib's restart budget is exhausted -- Hamlib is \
             dead for good and nothing will restart it further, so un-muting TX here means \
             modulating audio with zero PTT control"
        );
    }

    /// PAN-33 regression guard. Reproduces the double-registration /
    /// early-return-swallowing scenario: `start_hamlib_component` registers
    /// a fresh task handle in `named_task_handles` and then bails (a
    /// startup-window crash between publishing its child handles and
    /// confirming message-loop readiness) -- `restart_component`'s own
    /// `Err(e)` branch marks Hamlib `Failed` and leaks one
    /// `TxInhibitGuard` increment (PAN-19 MEDIUM #3) synchronously, in the
    /// SAME pass that dispatched the restart. That freshly-registered
    /// handle is already finished, so it sits in `named_task_handles`
    /// until the NEXT health pass rediscovers it -- this test simulates
    /// exactly that end state (Failed status + leaked inhibit + a stale
    /// finished handle under `ComponentId::Hamlib`, budget not exhausted)
    /// and drives one more `check_task_handles()` pass over it, standing
    /// in for that subsequent health pass.
    ///
    /// Before the fix, `handle_finished_task`'s early-return guard
    /// unconditionally swallowed this rediscovery (`status.state !=
    /// Running`), so Hamlib stayed permanently `Failed` despite
    /// `RestartBudget` having attempts left. After the fix, the guard
    /// falls through (budget remains), the REAL `start_hamlib_component`
    /// runs again (mock-rig path, deterministic), succeeds, and the
    /// earlier leaked `tx_restart_inhibit` increment is paid back --
    /// proving both the restart itself and the TX-inhibit accounting
    /// recover correctly.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn hamlib_stale_failed_status_gets_a_fresh_restart_when_budget_remains() {
        let mut coordinator = test_coordinator().await;

        // Simulate the FIRST pass's outcome: Hamlib already marked Failed,
        // with one TxInhibitGuard increment leaked believing it was
        // terminal.
        {
            let mut status_map = coordinator.component_status.write().await;
            status_map.insert(
                ComponentId::Hamlib,
                super::ComponentStatus {
                    state: super::ComponentState::Failed(
                        "simulated startup-window crash".to_string(),
                    ),
                    last_seen: std::time::Instant::now(),
                    error_count: 1,
                },
            );
        }
        coordinator
            .tx_restart_inhibit
            .fetch_add(1, Ordering::AcqRel);
        coordinator.hamlib_leaked_tx_inhibits = 1;
        assert!(
            coordinator
                .restart_budget
                .may_restart(ComponentId::Hamlib, std::time::Instant::now()),
            "test setup: restart budget must still have attempts left"
        );

        // Simulate the freshly-registered generation `start_hamlib_component`
        // left behind in `named_task_handles` before its own readiness
        // handshake bailed -- already finished by the time a later health
        // pass discovers it.
        let handle: tokio::task::JoinHandle<anyhow::Result<()>> =
            tokio::spawn(async { Err(anyhow::anyhow!("simulated startup-window crash")) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            handle.is_finished(),
            "test setup: the stale handle must already be finished"
        );
        coordinator
            .named_task_handles
            .push((ComponentId::Hamlib, handle));

        // The "subsequent health pass" that rediscovers it.
        coordinator.check_task_handles().await;

        assert_eq!(
            coordinator
                .component_status
                .read()
                .await
                .get(&ComponentId::Hamlib)
                .map(|s| &s.state),
            Some(&super::ComponentState::Running),
            "a rediscovered stale handle must get a fresh, successful restart attempt instead \
             of staying permanently Failed while restart budget remained"
        );
        assert_eq!(
            coordinator.tx_restart_inhibit.load(Ordering::Acquire),
            0,
            "the earlier leaked TxInhibitGuard increment must be paid back once Hamlib \
             actually recovers -- otherwise TX would stay permanently inhibited even after a \
             successful recovery"
        );
        assert_eq!(
            coordinator.hamlib_leaked_tx_inhibits, 0,
            "the leaked-increment ledger must be cleared once it's paid back"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }

    /// PAN-33 companion guard: once `RestartBudget` is genuinely exhausted,
    /// a rediscovered stale handle for an already-`Failed` component must
    /// remain a true no-op -- the fix must not reopen restart attempts
    /// forever, only when budget is actually available.
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn hamlib_stale_failed_status_stays_failed_once_budget_is_truly_exhausted() {
        let mut coordinator = test_coordinator().await;

        {
            let mut status_map = coordinator.component_status.write().await;
            status_map.insert(
                ComponentId::Hamlib,
                super::ComponentStatus {
                    state: super::ComponentState::Failed(
                        "simulated startup-window crash".to_string(),
                    ),
                    last_seen: std::time::Instant::now(),
                    error_count: 1,
                },
            );
        }
        let now = std::time::Instant::now();
        for _ in 0..5 {
            coordinator
                .restart_budget
                .record_attempt_and_backoff(ComponentId::Hamlib, now);
        }
        assert!(
            !coordinator
                .restart_budget
                .may_restart(ComponentId::Hamlib, now),
            "test setup: restart budget must actually be exhausted"
        );

        let handle: tokio::task::JoinHandle<anyhow::Result<()>> =
            tokio::spawn(async { Err(anyhow::anyhow!("simulated startup-window crash")) });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        coordinator
            .named_task_handles
            .push((ComponentId::Hamlib, handle));

        coordinator.check_task_handles().await;

        assert!(
            matches!(
                coordinator
                    .component_status
                    .read()
                    .await
                    .get(&ComponentId::Hamlib)
                    .map(|s| &s.state),
                Some(super::ComponentState::Failed(_))
            ),
            "once restart budget is genuinely exhausted, a rediscovered stale handle must \
             remain a no-op, not silently reopen restart attempts forever"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }

    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn hamlib_crash_mid_ptt_forces_ptt_off_on_the_rig() {
        use pancetta_hamlib::{PttState, RigControl, Vfo};

        let mut coordinator = test_coordinator().await;
        let mock = Arc::new(pancetta_hamlib::MockRig::default());
        mock.connect().await.expect("test rig should connect");
        mock.set_ptt(Vfo::Current, PttState::On)
            .await
            .expect("test rig should key");
        coordinator.rig_handle = Some(mock.clone());
        coordinator.ptt_active.store(true, Ordering::Release);

        coordinator.teardown_hamlib().await;

        assert_eq!(
            mock.get_ptt(Vfo::Current).await.expect("PTT state"),
            PttState::Off
        );
        assert!(!coordinator.ptt_active.load(Ordering::Acquire));
    }

    /// PAN-59: `run_main_loop` must actually process a `HamlibReconnectRequest`
    /// sent on `hamlib_reconnect_tx` -- this is the integration point that
    /// closes the loop between `tui_relay.rs` (Task 7, which only holds a
    /// cloned sender, never `&mut ApplicationCoordinator`) and
    /// `handle_hamlib_reconnect_request` (Task 5, which needs `&mut self`).
    #[cfg(feature = "pancetta-hamlib")]
    #[tokio::test]
    async fn run_main_loop_processes_a_hamlib_reconnect_request() {
        let mut coordinator = test_coordinator().await;
        let reconnect_tx = coordinator.hamlib_reconnect_tx.clone();
        let shutdown = coordinator.shutdown_signal.clone();

        let loop_handle = tokio::spawn(async move {
            coordinator.run_main_loop().await.unwrap();
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        reconnect_tx
            .send(super::super::hamlib::HamlibReconnectRequest { respond: tx })
            .await
            .expect("reconnect channel must accept the request");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("run_main_loop must process the reconnect request within 5s")
            .expect("handler must always respond");
        assert!(
            result.is_ok(),
            "reconnect should succeed via the mock rig path: {:?}",
            result.err()
        );

        shutdown.store(true, Ordering::Release);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), loop_handle).await;
    }

    #[tokio::test]
    async fn audio_handle_exit_on_a_live_audio_path_is_treated_as_a_failure() {
        let mut coordinator = test_coordinator().await;
        coordinator.audio_path_supervised = true;
        coordinator
            .named_task_handles
            .push((ComponentId::Audio, tokio::spawn(async { Ok(()) })));
        tokio::task::yield_now().await;
        coordinator.check_task_handles().await;
        assert!(matches!(
            coordinator
                .component_status
                .read()
                .await
                .get(&ComponentId::Audio)
                .map(|status| &status.state),
            Some(super::super::ComponentState::Failed(_))
        ));
    }

    #[tokio::test]
    async fn audio_handle_exit_is_benign_without_a_real_audio_path() {
        let mut coordinator = test_coordinator().await;
        assert!(!coordinator.audio_path_supervised);
        coordinator
            .named_task_handles
            .push((ComponentId::Audio, tokio::spawn(async { Ok(()) })));
        tokio::task::yield_now().await;
        coordinator.check_task_handles().await;
        assert!(!matches!(
            coordinator
                .component_status
                .read()
                .await
                .get(&ComponentId::Audio)
                .map(|status| &status.state),
            Some(super::super::ComponentState::Failed(_))
        ));
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

    /// Task 6 (task-supervision): a panicked Qso-component task must not
    /// let its in-flight QSOs silently vanish. `start_qso_component` stashes
    /// a cheap `Arc`-based `QsoManager` clone at
    /// `coordinator.qso_manager_for_supervisor` specifically so
    /// `handle_finished_task` can still enumerate and fail whatever was
    /// active at crash time, even though the panicked task's own
    /// `QsoManager` instance is gone.
    #[tokio::test]
    async fn qso_restart_emits_supervisor_restart_failure_for_each_active_qso() {
        let mut coordinator = test_coordinator().await;
        // `respond_to_cq` refuses a placeholder callsign, so give the
        // station a real one before the Qso component reads it.
        coordinator.config.write().await.station.callsign = "K1TEST".to_string();
        coordinator.start_qso_component().await.unwrap();

        // The coordinator's stashed handle shares the same `qsos` map as
        // whatever `QsoManager` is running inside the (about to be killed)
        // Qso-component task -- inserting an active QSO through it is
        // equivalent to the real task having opened one.
        let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
        let qso_id = manager
            .respond_to_cq("K1ABC".to_string(), 1500.0, None)
            .await
            .expect("seeding an active QSO via the real public API");

        // Subscribe BEFORE triggering the restart so the QsoFailed emitted
        // during `check_task_handles` isn't missed.
        let mut qso_events_rx = manager.subscribe();

        // Simulate a Qso-task panic: swap out whatever real task
        // `start_qso_component` spawned for a fake one that panics
        // immediately, mirroring `panicking_restartable_component_is_
        // restarted_after_backoff` above.
        let handle = tokio::spawn(async {
            panic!("injected test panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        coordinator
            .named_task_handles
            .retain(|(id, _)| *id != ComponentId::Qso);
        coordinator
            .named_task_handles
            .push((ComponentId::Qso, handle));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        coordinator.check_task_handles().await;

        let mut events = Vec::new();
        while let Ok(event) = qso_events_rx.try_recv() {
            events.push(event);
        }
        let failed = events.iter().find_map(|e| match e {
            pancetta_qso::QsoEvent::QsoFailed {
                qso_id: id, reason, ..
            } if *id == qso_id => Some(reason.clone()),
            _ => None,
        });
        assert_eq!(
            failed,
            Some(pancetta_qso::QsoFailureReason::SupervisorRestart),
            "expected a QsoFailed{{SupervisorRestart}} for the seeded QSO, got {events:?}"
        );

        // The Qso component was restarted (Restartable + within budget), so
        // it's Running again -- the drop-surfacing must not have interfered
        // with the restart path itself.
        let status = coordinator.component_status.read().await;
        assert_eq!(
            status.get(&ComponentId::Qso).map(|s| &s.state),
            Some(&super::ComponentState::Running)
        );
    }

    async fn failure_reason_after_component_restart(
        component: ComponentId,
    ) -> Option<pancetta_qso::QsoFailureReason> {
        let mut coordinator = test_coordinator().await;
        coordinator.config.write().await.station.callsign = "K1TEST".to_string();
        coordinator.start_qso_component().await.unwrap();
        if matches!(component, ComponentId::Dsp | ComponentId::Ft8Decoder) {
            coordinator.init_decode_handles();
        }
        let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
        let qso_id = manager
            .respond_to_cq("K1ABC".to_string(), 1500.0, None)
            .await
            .unwrap();
        let mut events = manager.subscribe();
        let handle = tokio::spawn(async {
            panic!("injected test panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        coordinator.named_task_handles.push((component, handle));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        coordinator.check_task_handles().await;
        while let Ok(event) = events.try_recv() {
            if let pancetta_qso::QsoEvent::QsoFailed {
                qso_id: failed_id,
                reason,
                ..
            } = event
            {
                if failed_id == qso_id {
                    return Some(reason);
                }
            }
        }
        None
    }

    #[tokio::test]
    async fn dsp_restart_does_not_fail_active_qsos() {
        assert_eq!(
            failure_reason_after_component_restart(ComponentId::Dsp).await,
            None
        );
    }

    #[tokio::test]
    async fn ft8_restart_does_not_fail_active_qsos() {
        assert_eq!(
            failure_reason_after_component_restart(ComponentId::Ft8Decoder).await,
            None
        );
    }

    #[tokio::test]
    async fn non_decode_non_qso_restart_does_not_fail_active_qsos() {
        assert_eq!(
            failure_reason_after_component_restart(ComponentId::DxCluster).await,
            None
        );
    }

    /// Task 6 fix-review followup (2026-07-25): the sibling test above only
    /// proves `fail_qso`'s `QsoFailed` reaches a subscriber the TEST ITSELF
    /// creates via `manager.subscribe()`. The real operator-facing path is
    /// different: the `QsoFailed` -> `MessageType::RecentQsoOutcome` relay
    /// lives in a `tokio::spawn`ed subscriber task INSIDE
    /// `start_qso_component` (qso.rs, ~L1568/1583/2271), and a
    /// `tokio::sync::broadcast` channel only delivers to subscribers alive
    /// AT SEND TIME. This test exercises that REAL relay path end to end --
    /// real `start_qso_component`, real forwarder task, real Tui bus
    /// channel -- instead of a fresh test-created subscriber.
    ///
    /// Why this is expected to pass with no reordering of the
    /// enumerate-then-restart sequence in `handle_finished_task`: the
    /// forwarder at qso.rs ~L1583 is `tokio::spawn`ed as a genuinely
    /// independent task -- its `JoinHandle` is discarded (never stored in
    /// `named_task_handles`, never `.abort()`-ed). Per tokio's task model, a
    /// panic inside one spawned task only unwinds THAT task's stack and is
    /// reported via ITS OWN `JoinHandle`; it does not cancel sibling tasks
    /// that task itself spawned earlier. So when the "outer" Qso-component
    /// task (the one `named_task_handles` tracks under `ComponentId::Qso`)
    /// dies, the forwarder it spawned keeps running, still subscribed to
    /// the same `event_sender` the coordinator's stashed
    /// `qso_manager_for_supervisor` clone shares (every `QsoManager::clone()`
    /// shares one `Arc`-backed `qsos` map and one `broadcast::Sender` --
    /// only a fresh `QsoManager::new()`, as a *post-restart* component build
    /// performs, gets independent ones). `fail_qso` MUST run on that
    /// pre-restart stashed clone anyway, because it looks the QSO up in its
    /// OWN `qsos` map (`qsos.write().await.remove(&qso_id)`) -- a
    /// freshly-restarted manager's map is empty, so calling `fail_qso` on
    /// the POST-restart clone for a PRE-restart qso_id would silently no-op.
    /// That rules out "restart first, then fail on the new clone" as a fix:
    /// there is no manager instance that both knows the QSO's metadata AND
    /// feeds a guaranteed-fresh forwarder. Enumerate-then-fail-then-restart
    /// (current code) is therefore correct as long as the pre-restart
    /// forwarder survives the crash, which this test proves it does.
    ///
    /// A readiness handshake (a throwaway QSO, failed and observed on the
    /// Tui channel) runs first so the test never races the real forwarder's
    /// startup (DB/ADIF init) -- without it, a `fail_qso` broadcast sent
    /// before the forwarder's `.subscribe()` call would be silently missed
    /// even by a forwarder that is definitely alive, since `broadcast`
    /// subscribers only see sends that happen after they subscribe.
    #[tokio::test]
    async fn qso_restart_delivers_recent_qso_outcome_through_the_real_forwarder() {
        let mut coordinator = test_coordinator().await;
        coordinator.config.write().await.station.callsign = "K1TEST".to_string();

        // Real last hop of the real relay path: the forwarder sends
        // `RecentQsoOutcome` to `ComponentId::Tui` via the real message
        // bus. Create that channel before starting the Qso component so
        // there's somewhere for it to land.
        let (_tui_tx, tui_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .unwrap();

        coordinator.start_qso_component().await.unwrap();

        let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();

        async fn poll_for(
            rx: &crossbeam_channel::Receiver<ComponentMessage>,
            want_callsign: &str,
            max_tries: u32,
        ) -> Option<crate::message_bus::RecentQsoOutcome> {
            for _ in 0..max_tries {
                if let Ok(msg) = rx.try_recv() {
                    if let MessageType::RecentQsoOutcome(outcome) = &msg.message_type {
                        if outcome.callsign == want_callsign {
                            return Some(outcome.clone());
                        }
                    }
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            None
        }

        // Readiness handshake: `start_qso_component` returns as soon as the
        // outer task is *spawned*, well before the tokio scheduler has
        // polled it even once -- the forwarder's `.subscribe()` call (deep
        // inside that task, after `.start()` + DB/ADIF init) has not
        // necessarily happened yet. A `QsoFailed` broadcast sent before that
        // `.subscribe()` call is silently missed forever by
        // `tokio::sync::broadcast` semantics, even though the forwarder is
        // "alive" moments later -- so a single fixed sleep-then-probe would
        // just trade one race for a slower one. Instead, retry with a FRESH
        // probe QSO each attempt (a consumed `fail_qso` cannot be re-fired
        // for the same id) until one lands on the real Tui channel, proving
        // the real forwarder is now subscribed before the real scenario
        // below relies on it.
        let mut ready = false;
        for i in 0..20 {
            let probe_call = format!("K9RD{i}");
            let probe_id = manager
                .respond_to_cq(probe_call.clone(), 1500.0, None)
                .await
                .expect("seeding a readiness-probe QSO");
            manager
                .fail_qso(probe_id, pancetta_qso::QsoFailureReason::UserCancelled)
                .await
                .expect("failing a readiness-probe QSO");
            // Generous per-attempt window (up to 300ms) -- unlike a tight
            // flood of probes, this gives the forwarder room to actually
            // catch up and relay before we give up on this attempt and
            // (if needed) fire another.
            if poll_for(&tui_rx, &probe_call, 30).await.is_some() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            ready,
            "readiness probe: real forwarder never relayed a RecentQsoOutcome to the Tui channel"
        );

        // Now the real scenario: seed a real active QSO, then simulate the
        // outer Qso-component task crashing exactly like the sibling test
        // above (swap its tracked JoinHandle for a fake panicking one).
        // This does NOT touch the REAL task or its REAL forwarder child --
        // they keep running completely undisturbed, which is exactly the
        // production scenario being validated: the supervisor only ever
        // observes the tracked JoinHandle, never the forwarder directly.
        manager
            .respond_to_cq("K1ABC".to_string(), 1501.0, None)
            .await
            .expect("seeding an active QSO via the real public API");

        let handle = tokio::spawn(async {
            panic!("injected test panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        coordinator
            .named_task_handles
            .retain(|(id, _)| *id != ComponentId::Qso);
        coordinator
            .named_task_handles
            .push((ComponentId::Qso, handle));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        coordinator.check_task_handles().await;

        let outcome = poll_for(&tui_rx, "K1ABC", 500).await.expect(
            "expected a RecentQsoOutcome for K1ABC to reach the Tui channel via the REAL \
             forwarder (not a test-created subscriber) after a supervisor restart",
        );
        assert!(
            matches!(
                outcome.outcome,
                crate::message_bus::QsoOutcome::Failed(
                    pancetta_qso::QsoFailureReason::SupervisorRestart
                )
            ),
            "expected Failed(SupervisorRestart), got {:?}",
            outcome.outcome
        );

        // Same regression guard as the sibling test: the restart itself
        // must still have succeeded.
        let status = coordinator.component_status.read().await;
        assert_eq!(
            status.get(&ComponentId::Qso).map(|s| &s.state),
            Some(&super::ComponentState::Running)
        );
    }
}
