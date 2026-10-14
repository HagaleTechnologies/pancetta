//! PAN-103 — applying saved config-file edits to a running station.
//!
//! The watcher (`pancetta_config::hot_reload::ConfigHotReload`) tells the
//! coordinator that the config file changed; `run_main_loop` hands each
//! reloaded config to [`ApplicationCoordinator::handle_config_reload`]. What a
//! reload may do is decided by the pure [`ReloadApplyPlan::decide`]:
//!
//! - **Remote-access revocations apply live** ([`SecurityConfigDiff`]): the
//!   per-channel consent bits, `tx_allow_list` removals (session eviction via
//!   the PAN-96 generation/reconcile path) and `station_agent.enabled = false`
//!   (session + poll stopped, definitive disarm). They are never deferred.
//!   The TX worker's key-time and per-chunk playback re-checks then refuse
//!   queued remote frames and stop a transmission in progress.
//! - **Grant-direction security edits and every other key are
//!   restart-required.** The handler says so in a diagnostic line.
//! - The shared config snapshot is replaced through the C19 classifier
//!   ([`classify_config_reload`]): a QSO-latched field (callsign, grid, slot
//!   parity) is held back while a QSO is active and applied once the band is
//!   quiet.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pancetta_config::hot_reload::{ConfigHotReload, ConfigReloadEvent};
use pancetta_config::Config;
use pancetta_core::DiagnosticLevel;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use super::health::{classify_config_reload, ConfigReloadApplicability};
use crate::message_bus::ComponentId;
use pancetta_agent::arm::ConsentChannel;

/// What the watcher forwards to the main loop: the reloaded, validated
/// config, or the load/validation error text.
pub(crate) type ConfigReloadOutcome = Result<Config, String>;

/// How long to wait after the first watcher event before re-reading the file.
/// Editors often truncate and then write, which produces several events; the
/// library's own debounce drops events inside 500 ms of a reload, so the
/// forwarder re-reads the file itself once the writes have settled.
const RELOAD_SETTLE: Duration = Duration::from_millis(750);

/// TUI diagnostics target for every line this module emits.
const DIAG_TARGET: &str = "config.reload";

pub(crate) const MSG_RELOADED: &str = "Config reloaded";
pub(crate) const MSG_STATION_AGENT_CONSENT_OFF: &str = "Remote TX consent for the station-agent channel is now OFF — remote transmission refused until re-enabled";
pub(crate) const MSG_STATION_AGENT_CONSENT_ON: &str =
    "Remote TX consent for the station-agent channel is now ON";
pub(crate) const MSG_WSJTX_CONSENT_OFF: &str =
    "Remote TX consent for the WSJT-X UDP channel is now OFF — Reply-initiated QSO creation refused";
pub(crate) const MSG_WSJTX_CONSENT_ON: &str =
    "Remote TX consent for the WSJT-X UDP channel is now ON";
pub(crate) const MSG_ALLOW_LIST_ADDED: &str = "tx_allow_list additions apply after a restart";
pub(crate) const MSG_STATION_AGENT_DISABLED: &str =
    "station_agent disabled — remote sessions closed and remote TX disarmed";
pub(crate) const MSG_STATION_AGENT_ENABLED: &str =
    "station_agent enabled — restart required before it takes effect";
pub(crate) const MSG_DEFERRED: &str =
    "Config change deferred while a QSO is active — applied when the band goes quiet";
pub(crate) const MSG_OTHER_KEYS_RESTART: &str =
    "Config changes outside the remote-access settings take effect after a restart";

/// The remote-access settings that changed between two configs, by
/// direction. Revocations are applied live; grants and transport/identity
/// changes are reported as restart-required.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecurityConfigDiff {
    /// New value of the station-agent channel's consent
    /// (`[network.station_agent].remote_tx_enabled`) when it changed.
    pub station_agent_consent: Option<bool>,
    /// New value of the WSJT-X UDP channel's consent
    /// (`[network.wsjtx_udp].enabled && allow_tx_initiation`) when it changed.
    pub wsjtx_consent: Option<bool>,
    /// keyIds removed from `tx_allow_list` (sorted, deduplicated).
    pub allow_list_removed: Vec<String>,
    /// keyIds added to `tx_allow_list` (sorted, deduplicated).
    pub allow_list_added: Vec<String>,
    /// `[network.station_agent].enabled` went `true` → `false`.
    pub station_agent_disabled: bool,
    /// `[network.station_agent].enabled` went `false` → `true`.
    pub station_agent_enabled: bool,
    /// Station-agent transport/identity fields that changed; each needs a
    /// restart.
    pub restart_required_fields: Vec<&'static str>,
}

impl SecurityConfigDiff {
    /// Compare the remote-access settings of `old` and `new`.
    pub fn diff(old: &Config, new: &Config) -> Self {
        let o = &old.network.station_agent;
        let n = &new.network.station_agent;

        let station_agent_consent =
            (o.remote_tx_enabled != n.remote_tx_enabled).then_some(n.remote_tx_enabled);
        let old_wsjtx = wsjtx_consent(old);
        let new_wsjtx = wsjtx_consent(new);
        let wsjtx_consent = (old_wsjtx != new_wsjtx).then_some(new_wsjtx);

        let old_set: std::collections::BTreeSet<&String> = o.tx_allow_list.iter().collect();
        let new_set: std::collections::BTreeSet<&String> = n.tx_allow_list.iter().collect();
        let allow_list_removed = old_set.difference(&new_set).map(|k| (*k).clone()).collect();
        let allow_list_added = new_set.difference(&old_set).map(|k| (*k).clone()).collect();

        let mut restart_required_fields = Vec::new();
        if o.relay_url != n.relay_url {
            restart_required_fields.push("relay_url");
        }
        if o.pairing_api_url != n.pairing_api_url {
            restart_required_fields.push("pairing_api_url");
        }
        if o.pairing_origin != n.pairing_origin {
            restart_required_fields.push("pairing_origin");
        }
        if o.key_dir != n.key_dir {
            restart_required_fields.push("key_dir");
        }
        if o.audit_log_path != n.audit_log_path {
            restart_required_fields.push("audit_log_path");
        }

        Self {
            station_agent_consent,
            wsjtx_consent,
            allow_list_removed,
            allow_list_added,
            station_agent_disabled: o.enabled && !n.enabled,
            station_agent_enabled: !o.enabled && n.enabled,
            restart_required_fields,
        }
    }

    /// No remote-access setting changed.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The WSJT-X UDP channel's consent bit as the coordinator seeds it: gated
/// on `enabled` too (no Reply-handling path while the component is off).
fn wsjtx_consent(config: &Config) -> bool {
    config.network.wsjtx_udp.enabled && config.network.wsjtx_udp.allow_tx_initiation
}

/// What happens to the shared config snapshot (`ApplicationCoordinator::config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAction {
    /// Replace the snapshot with the reloaded config.
    Replace,
    /// C19: a QSO-latched field changed while a QSO is active. Store the
    /// reloaded config with the latched fields kept at their running values
    /// and apply the rest once no QSO is active.
    Defer,
    /// Nothing changed; leave the snapshot alone.
    Keep,
}

/// One decision for one reload: the security actions to apply now (never
/// deferred) and what to do with the shared snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadApplyPlan {
    /// Remote-access changes. Applied immediately, even when the snapshot
    /// is deferred: a revocation never waits for a QSO to end.
    pub security: SecurityConfigDiff,
    /// What happens to the shared snapshot, from [`classify_config_reload`].
    pub snapshot: SnapshotAction,
    /// The C19 classification the snapshot action came from.
    pub applicability: ConfigReloadApplicability,
    /// Keys outside the live remote-access settings changed. They are
    /// restart-required.
    pub other_keys_changed: bool,
}

impl ReloadApplyPlan {
    /// Decide how to apply `new` over the running `old`, given whether a QSO
    /// is active.
    pub fn decide(old: &Config, new: &Config, qso_active: bool) -> Self {
        let applicability = classify_config_reload(old, new, qso_active);
        let snapshot = match applicability {
            ConfigReloadApplicability::NoChange => SnapshotAction::Keep,
            ConfigReloadApplicability::DeferQsoLatched => SnapshotAction::Defer,
            ConfigReloadApplicability::SafeLive | ConfigReloadApplicability::SafeQuiescent => {
                SnapshotAction::Replace
            }
        };
        let other_keys_changed = snapshot != SnapshotAction::Keep && {
            let mut normalized = new.clone();
            let n = &mut normalized.network.station_agent;
            let o = &old.network.station_agent;
            n.remote_tx_enabled = o.remote_tx_enabled;
            n.tx_allow_list = o.tx_allow_list.clone();
            n.enabled = o.enabled;
            n.relay_url = o.relay_url.clone();
            n.pairing_api_url = o.pairing_api_url.clone();
            n.pairing_origin = o.pairing_origin.clone();
            n.key_dir = o.key_dir.clone();
            n.audit_log_path = o.audit_log_path.clone();
            normalized.network.wsjtx_udp.allow_tx_initiation =
                old.network.wsjtx_udp.allow_tx_initiation;
            !matches!(
                (toml::to_string(old), toml::to_string(&normalized)),
                (Ok(a), Ok(b)) if a == b
            )
        };
        Self {
            security: SecurityConfigDiff::diff(old, new),
            snapshot,
            applicability,
            other_keys_changed,
        }
    }
}

/// `new` with the C19 QSO-latched fields kept at `running`'s values — what
/// the shared snapshot holds while a latched change is deferred.
pub(crate) fn with_latched_fields_from(new: &Config, running: &Config) -> Config {
    let mut merged = new.clone();
    merged.station.callsign = running.station.callsign.clone();
    merged.station.grid_square = running.station.grid_square.clone();
    merged.autonomous.slot_parity = running.autonomous.slot_parity.clone();
    merged
}

/// Load and validate `path` the way the watcher does.
fn load_config_for_reload(path: &Path) -> ConfigReloadOutcome {
    let (config, warnings) =
        Config::load_from_file_with_warnings(path).map_err(|e| e.to_string())?;
    for w in &warnings {
        warn!("Config reload warning: {w}");
    }
    config.validate().map_err(|e| e.to_string())?;
    Ok(config)
}

/// Turn watcher events into reload outcomes for the main loop. Each burst of
/// events is settled for `settle`, then the file is read once from disk, so
/// the outcome always reflects the file's final content.
async fn forward_config_reload_events(
    mut events: broadcast::Receiver<ConfigReloadEvent>,
    tx: mpsc::Sender<ConfigReloadOutcome>,
    path: PathBuf,
    settle: Duration,
) {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    loop {
        match events.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
        tokio::time::sleep(settle).await;
        loop {
            match events.try_recv() {
                Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => return,
            }
        }
        let p = path.clone();
        let outcome = tokio::task::spawn_blocking(move || load_config_for_reload(&p))
            .await
            .unwrap_or_else(|e| Err(format!("config reload task failed: {e}")));
        if tx.send(outcome).await.is_err() {
            return;
        }
    }
}

/// Receive the next reload outcome, or wait forever when no watcher runs.
/// A closed channel is dropped so the main loop's `select!` stops polling it.
pub(crate) async fn recv_config_reload(
    rx: &mut Option<mpsc::Receiver<ConfigReloadOutcome>>,
) -> Option<ConfigReloadOutcome> {
    if let Some(r) = rx.as_mut() {
        if let Some(outcome) = r.recv().await {
            return Some(outcome);
        }
        *rx = None;
    }
    std::future::pending().await
}

impl super::ApplicationCoordinator {
    /// Start watching `config_path` for saved edits. Only runs when that path
    /// is an existing file (a defaults-only run has nothing to watch). A
    /// watcher that fails to start is logged and skipped: the station then
    /// runs exactly as it did before PAN-103, with every edit needing a
    /// restart.
    pub(crate) async fn start_config_reload_watcher(&mut self) -> anyhow::Result<()> {
        self.start_config_reload_watcher_with_settle(RELOAD_SETTLE)
            .await
    }

    async fn start_config_reload_watcher_with_settle(
        &mut self,
        settle: Duration,
    ) -> anyhow::Result<()> {
        if !self.config_path.is_file() {
            info!(
                "Config hot reload not started: {} is not a file",
                self.config_path.display()
            );
            return Ok(());
        }
        let initial = self.config.read().await.clone();
        let (mut watcher, events) = match ConfigHotReload::new(initial, &self.config_path) {
            Ok(pair) => pair,
            Err(e) => {
                info!("Config hot reload not started: {e}");
                return Ok(());
            }
        };
        if let Err(e) = watcher.start_watching().await {
            info!("Config hot reload not started: {e}");
            return Ok(());
        }
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(forward_config_reload_events(
            events,
            tx,
            self.config_path.clone(),
            settle,
        ));
        self.config_reload_watcher = Some(watcher);
        self.config_reload_rx = Some(rx);
        info!(
            "Watching {} for config changes (remote-access revocations apply live)",
            self.config_path.display()
        );
        Ok(())
    }

    /// C19: a poisoned `active_tx_qsos` lock counts as "a QSO is active", so
    /// a latched change is deferred rather than risked.
    fn config_reload_qso_active(&self) -> bool {
        self.active_tx_qsos
            .read()
            .map(|set| !set.is_empty())
            .unwrap_or(true)
    }

    /// Apply one reload outcome: security changes first-class and never
    /// deferred, the snapshot through C19, then one diagnostic line per
    /// action.
    pub(crate) async fn handle_config_reload(&mut self, outcome: ConfigReloadOutcome) {
        let new = match outcome {
            Ok(config) => config,
            Err(err) => {
                warn!("Config reload failed: {err}");
                self.emit_config_reload_lines(vec![(
                    DiagnosticLevel::Error,
                    format!("Config reload failed: {err} — previous configuration still active"),
                )])
                .await;
                return;
            }
        };
        let old = self.config.read().await.clone();
        let plan = ReloadApplyPlan::decide(&old, &new, self.config_reload_qso_active());
        if plan.snapshot == SnapshotAction::Keep && plan.security.is_empty() {
            debug!("Config file event with no content change — nothing to apply");
            return;
        }

        // The snapshot is written before the security actions: disabling the
        // station agent restarts it, and the restart re-reads this snapshot.
        match plan.snapshot {
            SnapshotAction::Replace => {
                *self.config.write().await = new;
                self.pending_config = None;
            }
            SnapshotAction::Defer => {
                *self.config.write().await = with_latched_fields_from(&new, &old);
                self.pending_config = Some(new);
            }
            SnapshotAction::Keep => {}
        }
        info!(?plan.applicability, "Config reloaded");

        let mut lines = vec![(DiagnosticLevel::Info, MSG_RELOADED.to_string())];
        lines.extend(self.apply_security_config(&plan.security).await);
        if plan.snapshot == SnapshotAction::Defer {
            lines.push((DiagnosticLevel::Info, MSG_DEFERRED.to_string()));
        }
        if plan.other_keys_changed {
            lines.push((DiagnosticLevel::Warn, MSG_OTHER_KEYS_RESTART.to_string()));
        }
        self.emit_config_reload_lines(lines).await;
    }

    /// Apply a held-back (C19) reload once no QSO is active. Called from the
    /// main loop's periodic health tick.
    pub(crate) async fn flush_pending_config_if_quiescent(&mut self) {
        if self.pending_config.is_none() || self.config_reload_qso_active() {
            return;
        }
        if let Some(pending) = self.pending_config.take() {
            info!("Applying deferred config change (no QSO active)");
            self.handle_config_reload(Ok(pending)).await;
        }
    }

    /// Make the running station enforce `diff`'s revocations, and report the
    /// grant-direction changes as restart-required. Returns the diagnostic
    /// lines, in order.
    pub(crate) async fn apply_security_config(
        &mut self,
        diff: &SecurityConfigDiff,
    ) -> Vec<(DiagnosticLevel, String)> {
        let mut lines = Vec::new();
        let now = chrono::Utc::now().timestamp_millis();

        // 1. Per-channel consent (audited `LocalConsentChanged`). The TX
        //    worker's key-time and playback re-checks read this bit live.
        for (channel, value, on_msg, off_msg) in [
            (
                ConsentChannel::StationAgent,
                diff.station_agent_consent,
                MSG_STATION_AGENT_CONSENT_ON,
                MSG_STATION_AGENT_CONSENT_OFF,
            ),
            (
                ConsentChannel::WsjtxUdp,
                diff.wsjtx_consent,
                MSG_WSJTX_CONSENT_ON,
                MSG_WSJTX_CONSENT_OFF,
            ),
        ] {
            let Some(enabled) = value else { continue };
            let effects = {
                // A poisoned arm already refuses all remote TX; the bit is
                // still written so the state is right once poison clears.
                let mut st = match self.remote_tx_arm.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                st.set_channel_consent(channel, enabled, now)
            };
            let audit = self.audit_log();
            for effect in &effects {
                if let pancetta_agent::arm::ArmEffect::Audit(ev) = effect {
                    audit.append(ev);
                }
            }
            if enabled {
                lines.push((DiagnosticLevel::Info, on_msg.to_string()));
            } else {
                warn!("{off_msg}");
                lines.push((DiagnosticLevel::Warn, off_msg.to_string()));
            }
        }

        // 2. tx_allow_list removals: prune the live sets and bump the
        //    authorization generation; the session loop evicts on its next
        //    tick (PAN-96 reconcile).
        if !diff.allow_list_removed.is_empty() {
            match self.station_agent_live.clone() {
                Some(live) => {
                    super::station_agent::revoke_static_allow_list_entries(
                        &live,
                        &self.remote_tx_arm,
                        &self.audit_log(),
                        &diff.allow_list_removed,
                        now,
                    );
                    for key in &diff.allow_list_removed {
                        warn!(peer = %key, "tx_allow_list entry removed by config edit — evicting");
                        lines.push((
                            DiagnosticLevel::Warn,
                            format!(
                                "tx_allow_list entry removed: {key} — session evicted, authorization revoked"
                            ),
                        ));
                    }
                }
                None => {
                    for key in &diff.allow_list_removed {
                        lines.push((
                            DiagnosticLevel::Info,
                            format!(
                                "tx_allow_list entry removed: {key} — station agent not running, no session to evict"
                            ),
                        ));
                    }
                }
            }
        }
        if !diff.allow_list_added.is_empty() {
            lines.push((DiagnosticLevel::Warn, MSG_ALLOW_LIST_ADDED.to_string()));
        }

        // 3. station_agent.enabled true -> false: stop the session and poll
        //    tasks, disarm, and restart the component into its inert branch
        //    (it re-reads the snapshot written by the caller).
        if diff.station_agent_disabled {
            self.stop_station_agent_for_config().await;
            lines.push((
                DiagnosticLevel::Warn,
                MSG_STATION_AGENT_DISABLED.to_string(),
            ));
        }

        // 4. Restart-required: report only.
        if diff.station_agent_enabled {
            lines.push((DiagnosticLevel::Warn, MSG_STATION_AGENT_ENABLED.to_string()));
        }
        for field in &diff.restart_required_fields {
            lines.push((
                DiagnosticLevel::Warn,
                format!("station_agent transport settings changed ({field}) — restart required"),
            ));
        }
        lines
    }

    /// Deliberately stop the running station-agent component (config
    /// disable). The task handles are removed from `named_task_handles`
    /// before they are aborted, so the supervisor never counts this as a
    /// crash or spends restart budget on it.
    async fn stop_station_agent_for_config(&mut self) {
        let mut stopped = Vec::new();
        let mut i = 0;
        while i < self.named_task_handles.len() {
            if self.named_task_handles[i].0 == ComponentId::StationAgent {
                stopped.push(self.named_task_handles.remove(i).1);
            } else {
                i += 1;
            }
        }
        if let Some(poll) = self.station_agent_poll.take() {
            poll.abort();
        }
        self.station_agent_live = None;
        for handle in &stopped {
            handle.abort();
        }
        // Wait (bounded) for the session task to drop, which closes the relay
        // socket and every peer's Noise transport.
        for handle in stopped {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
        self.fail_safe_disarm_remote_tx("station agent disabled by config")
            .await;
        if let Err(e) = self.start_station_agent_component().await {
            warn!("station_agent restart into inert mode failed after config disable: {e}");
        }
    }

    async fn emit_config_reload_lines(&self, lines: Vec<(DiagnosticLevel, String)>) {
        for (level, text) in lines {
            super::tx::emit_diagnostic_full(
                &self.message_bus,
                ComponentId::Coordinator,
                DIAG_TARGET,
                level,
                text,
                None,
                None,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::ApplicationCoordinator;
    use crate::message_bus::MessageType;
    use pancetta_agent::arm::{ArmTime, VerifiedArmGrant};
    use pancetta_config::SlotParitySetting;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn base() -> Config {
        let mut c = Config::default();
        c.network.station_agent.remote_tx_enabled = true;
        c.network.station_agent.tx_allow_list = vec!["A".to_string(), "B".to_string()];
        c
    }

    // --- SecurityConfigDiff --------------------------------------------------

    #[test]
    fn diff_remote_tx_enabled_each_direction() {
        let old = base();
        let mut off = old.clone();
        off.network.station_agent.remote_tx_enabled = false;
        assert_eq!(
            SecurityConfigDiff::diff(&old, &off).station_agent_consent,
            Some(false)
        );
        assert_eq!(
            SecurityConfigDiff::diff(&off, &old).station_agent_consent,
            Some(true)
        );
        assert!(SecurityConfigDiff::diff(&old, &old.clone()).is_empty());
    }

    #[test]
    fn diff_wsjtx_consent_follows_enabled_and_allow_tx_initiation() {
        let mut old = base();
        old.network.wsjtx_udp.enabled = true;
        old.network.wsjtx_udp.allow_tx_initiation = true;

        let mut disabled = old.clone();
        disabled.network.wsjtx_udp.enabled = false; // allow_tx_initiation stays true
        assert_eq!(
            SecurityConfigDiff::diff(&old, &disabled).wsjtx_consent,
            Some(false)
        );

        let mut no_init = old.clone();
        no_init.network.wsjtx_udp.allow_tx_initiation = false;
        assert_eq!(
            SecurityConfigDiff::diff(&old, &no_init).wsjtx_consent,
            Some(false)
        );
        assert_eq!(
            SecurityConfigDiff::diff(&no_init, &old).wsjtx_consent,
            Some(true)
        );

        // Both bits off before and after: no consent change.
        let mut a = base();
        a.network.wsjtx_udp.enabled = false;
        a.network.wsjtx_udp.allow_tx_initiation = false;
        let mut b = a.clone();
        b.network.wsjtx_udp.allow_tx_initiation = true;
        assert_eq!(SecurityConfigDiff::diff(&a, &b).wsjtx_consent, None);
    }

    #[test]
    fn diff_allow_list_removal_addition_both_and_reorder() {
        let old = base();
        let mut removed = old.clone();
        removed.network.station_agent.tx_allow_list = vec!["B".to_string()];
        let d = SecurityConfigDiff::diff(&old, &removed);
        assert_eq!(d.allow_list_removed, vec!["A".to_string()]);
        assert!(d.allow_list_added.is_empty());

        let mut added = old.clone();
        added
            .network
            .station_agent
            .tx_allow_list
            .push("C".to_string());
        let d = SecurityConfigDiff::diff(&old, &added);
        assert!(d.allow_list_removed.is_empty());
        assert_eq!(d.allow_list_added, vec!["C".to_string()]);

        let mut both = old.clone();
        both.network.station_agent.tx_allow_list = vec!["B".to_string(), "C".to_string()];
        let d = SecurityConfigDiff::diff(&old, &both);
        assert_eq!(d.allow_list_removed, vec!["A".to_string()]);
        assert_eq!(d.allow_list_added, vec!["C".to_string()]);

        let mut reordered = old.clone();
        reordered.network.station_agent.tx_allow_list = vec!["B".to_string(), "A".to_string()];
        assert!(SecurityConfigDiff::diff(&old, &reordered).is_empty());
    }

    #[test]
    fn diff_station_agent_enabled_directions_are_distinct() {
        let mut on = base();
        on.network.station_agent.enabled = true;
        let mut off = on.clone();
        off.network.station_agent.enabled = false;

        let d = SecurityConfigDiff::diff(&on, &off);
        assert!(d.station_agent_disabled);
        assert!(!d.station_agent_enabled);

        let d = SecurityConfigDiff::diff(&off, &on);
        assert!(d.station_agent_enabled);
        assert!(!d.station_agent_disabled);
    }

    #[test]
    fn diff_transport_fields_are_restart_required() {
        let old = base();
        let mut new = old.clone();
        new.network.station_agent.relay_url = Some("wss://relay.example".to_string());
        new.network.station_agent.pairing_api_url = Some("https://pair.example".to_string());
        new.network.station_agent.key_dir = Some("/tmp/keys".to_string());
        let d = SecurityConfigDiff::diff(&old, &new);
        assert_eq!(
            d.restart_required_fields,
            vec!["relay_url", "pairing_api_url", "key_dir"]
        );
        assert_eq!(d.station_agent_consent, None);
        assert!(d.allow_list_removed.is_empty());
    }

    // --- ReloadApplyPlan -----------------------------------------------------

    #[test]
    fn decide_defers_latched_snapshot_but_never_the_revocation() {
        let old = base();
        let mut new = old.clone();
        new.station.callsign = "K9NEW".to_string();
        new.network.station_agent.remote_tx_enabled = false;
        let plan = ReloadApplyPlan::decide(&old, &new, true);
        assert_eq!(plan.snapshot, SnapshotAction::Defer);
        assert_eq!(
            plan.applicability,
            ConfigReloadApplicability::DeferQsoLatched
        );
        assert_eq!(plan.security.station_agent_consent, Some(false));
    }

    #[test]
    fn decide_replaces_on_safe_live_and_safe_quiescent() {
        let old = base();
        let mut ui = old.clone();
        ui.ui.theme = format!("{}-x", old.ui.theme);
        let plan = ReloadApplyPlan::decide(&old, &ui, true);
        assert_eq!(plan.snapshot, SnapshotAction::Replace);
        assert!(plan.other_keys_changed);
        assert!(plan.security.is_empty());

        let mut call = old.clone();
        call.station.callsign = "K9NEW".to_string();
        let plan = ReloadApplyPlan::decide(&old, &call, false);
        assert_eq!(plan.applicability, ConfigReloadApplicability::SafeQuiescent);
        assert_eq!(plan.snapshot, SnapshotAction::Replace);
    }

    #[test]
    fn decide_security_only_edit_reports_no_other_keys() {
        let old = base();
        let mut new = old.clone();
        new.network.station_agent.remote_tx_enabled = false;
        new.network.station_agent.tx_allow_list = vec!["B".to_string()];
        let plan = ReloadApplyPlan::decide(&old, &new, false);
        assert_eq!(plan.snapshot, SnapshotAction::Replace);
        assert!(!plan.other_keys_changed);
    }

    #[test]
    fn decide_identical_config_is_a_no_op() {
        let old = base();
        let plan = ReloadApplyPlan::decide(&old, &old.clone(), true);
        assert_eq!(plan.snapshot, SnapshotAction::Keep);
        assert!(plan.security.is_empty());
        assert!(!plan.other_keys_changed);
    }

    #[test]
    fn with_latched_fields_keeps_only_the_running_latched_values() {
        let running = base();
        let mut new = running.clone();
        new.station.callsign = "K9NEW".to_string();
        new.station.grid_square = "EM10".to_string();
        new.autonomous.slot_parity = SlotParitySetting::Odd;
        new.network.station_agent.remote_tx_enabled = false;
        let merged = with_latched_fields_from(&new, &running);
        assert_eq!(merged.station.callsign, running.station.callsign);
        assert_eq!(merged.station.grid_square, running.station.grid_square);
        assert_eq!(
            merged.autonomous.slot_parity,
            running.autonomous.slot_parity
        );
        assert!(!merged.network.station_agent.remote_tx_enabled);
    }

    // --- classify_config_reload (C19; first direct tests) -------------------

    #[test]
    fn classify_config_reload_each_latched_field_defers_only_while_qso_active() {
        let old = base();
        let mut call = old.clone();
        call.station.callsign = "K9NEW".to_string();
        let mut grid = old.clone();
        grid.station.grid_square = "EM10".to_string();
        let mut parity = old.clone();
        parity.autonomous.slot_parity = SlotParitySetting::Even;
        for new in [&call, &grid, &parity] {
            assert_eq!(
                classify_config_reload(&old, new, true),
                ConfigReloadApplicability::DeferQsoLatched
            );
            assert_eq!(
                classify_config_reload(&old, new, false),
                ConfigReloadApplicability::SafeQuiescent
            );
        }
    }

    #[test]
    fn classify_config_reload_live_safe_and_identical() {
        let old = base();
        let mut net = old.clone();
        net.network.station_agent.remote_tx_enabled = false;
        assert_eq!(
            classify_config_reload(&old, &net, true),
            ConfigReloadApplicability::SafeLive
        );
        assert_eq!(
            classify_config_reload(&old, &old.clone(), true),
            ConfigReloadApplicability::NoChange
        );
    }

    // --- coordinator integration ---------------------------------------------

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pancetta-pan103-{}-{}-{name}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    /// Same direct-construction pattern as `health.rs`'s `test_coordinator`
    /// (`--replay` so no real logbook is touched), with the agent audit log
    /// pointed at a temp file instead of `~/.pancetta`.
    async fn test_coordinator(mut config: Config) -> ApplicationCoordinator {
        config.network.station_agent.audit_log_path =
            Some(temp_path("audit.log").to_string_lossy().into_owned());
        let mut coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,
            true,
            false,
            9090,
            None,
            Some(std::path::PathBuf::from("/some/capture/dir")),
            None,
            1500.0,
            Arc::new(AtomicBool::new(false)),
            Vec::new(),
            temp_path("missing-config.toml"),
            true,
        )
        .await
        .expect("coordinator creation should succeed");
        coordinator.pancetta_home_override = Some(temp_path("home"));
        coordinator
    }

    async fn tui_rx(
        coordinator: &ApplicationCoordinator,
    ) -> crossbeam_channel::Receiver<crate::message_bus::ComponentMessage> {
        coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Tui)
            .await
            .unwrap()
            .1
    }

    fn diagnostics(
        rx: &crossbeam_channel::Receiver<crate::message_bus::ComponentMessage>,
    ) -> Vec<(DiagnosticLevel, String)> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let MessageType::DiagnosticEvent {
                target: DIAG_TARGET,
                level,
                text,
                ..
            } = msg.message_type
            {
                out.push((level, text));
            }
        }
        out
    }

    fn texts(lines: &[(DiagnosticLevel, String)]) -> Vec<&str> {
        lines.iter().map(|(_, t)| t.as_str()).collect()
    }

    fn arm_client(coordinator: &ApplicationCoordinator, client: &str) {
        let now = ArmTime::now();
        let mut st = coordinator.remote_tx_arm.lock().unwrap();
        st.arm(
            VerifiedArmGrant {
                operator_callsign: "K1TEST".to_string(),
                ttl_ms: 300_000,
                armed_until_ms: now.unix_ms + 300_000,
                heartbeat_interval_sec: 10,
                scope_tx: true,
                jti: "pan103-arm".to_string(),
                client_key_id: client.to_string(),
            },
            now,
        );
    }

    fn permitted(coordinator: &ApplicationCoordinator, client: &str) -> bool {
        crate::coordinator::tx::remote_tx_permitted_for(
            &coordinator.remote_tx_arm,
            ArmTime::now,
            Some(client),
        )
    }

    #[tokio::test]
    async fn reload_with_a_live_safe_change_replaces_the_snapshot_and_reports_it() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.ui.theme = format!("{}-x", new.ui.theme);
        coordinator.handle_config_reload(Ok(new.clone())).await;

        assert_eq!(coordinator.config.read().await.ui.theme, new.ui.theme);
        let lines = diagnostics(&rx);
        assert_eq!(
            lines.iter().filter(|(_, t)| t == MSG_RELOADED).count(),
            1,
            "{lines:?}"
        );
        assert!(texts(&lines).contains(&MSG_OTHER_KEYS_RESTART));
    }

    #[tokio::test]
    async fn reload_with_identical_content_emits_nothing() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let same = coordinator.config.read().await.clone();
        coordinator.handle_config_reload(Ok(same)).await;
        assert!(diagnostics(&rx).is_empty());
    }

    #[tokio::test]
    async fn reload_failure_keeps_the_previous_config_and_reports_an_error() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let before = toml::to_string(&*coordinator.config.read().await).unwrap();
        coordinator
            .handle_config_reload(Err("expected `=`".to_string()))
            .await;
        assert_eq!(
            toml::to_string(&*coordinator.config.read().await).unwrap(),
            before
        );
        let lines = diagnostics(&rx);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, DiagnosticLevel::Error);
        assert!(lines[0].1.contains("previous configuration still active"));
    }

    #[tokio::test]
    async fn latched_change_is_deferred_while_a_qso_is_active_then_flushed() {
        let mut coordinator = test_coordinator(base()).await;
        let running_call = coordinator.config.read().await.station.callsign.clone();
        coordinator
            .active_tx_qsos
            .write()
            .unwrap()
            .insert("qso-1".to_string());
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.station.callsign = "K9NEW".to_string();
        new.network.station_agent.remote_tx_enabled = false;
        coordinator.handle_config_reload(Ok(new)).await;

        {
            let cfg = coordinator.config.read().await;
            assert_eq!(cfg.station.callsign, running_call, "latched field held");
            assert!(
                !cfg.network.station_agent.remote_tx_enabled,
                "the rest of the edit is in the snapshot"
            );
        }
        assert!(
            !coordinator
                .remote_tx_arm
                .lock()
                .unwrap()
                .channel_consent(ConsentChannel::StationAgent),
            "the revocation was not deferred"
        );
        let lines = diagnostics(&rx);
        assert!(texts(&lines).contains(&MSG_DEFERRED), "{lines:?}");
        assert!(coordinator.pending_config.is_some());

        // Still active: the flush does nothing.
        coordinator.flush_pending_config_if_quiescent().await;
        assert_eq!(
            coordinator.config.read().await.station.callsign,
            running_call
        );

        coordinator.active_tx_qsos.write().unwrap().clear();
        coordinator.flush_pending_config_if_quiescent().await;
        assert_eq!(coordinator.config.read().await.station.callsign, "K9NEW");
        assert!(coordinator.pending_config.is_none());
    }

    #[tokio::test]
    async fn consent_off_refuses_remote_tx_live_and_consent_on_restores_it() {
        let mut coordinator = test_coordinator(base()).await;
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, 0);
        arm_client(&coordinator, "A");
        assert!(permitted(&coordinator, "A"), "setup: armed");
        let rx = tui_rx(&coordinator).await;

        let mut off = coordinator.config.read().await.clone();
        off.network.station_agent.remote_tx_enabled = false;
        coordinator.handle_config_reload(Ok(off.clone())).await;
        assert!(
            !permitted(&coordinator, "A"),
            "queued/in-flight remote TX refused"
        );
        let lines = diagnostics(&rx);
        assert!(
            texts(&lines).contains(&MSG_STATION_AGENT_CONSENT_OFF),
            "{lines:?}"
        );
        assert!(!texts(&lines).contains(&MSG_OTHER_KEYS_RESTART));
        let audit = std::fs::read_to_string(coordinator.audit_log().path()).unwrap();
        assert!(
            audit.contains("local consent station_agent=false"),
            "consent change audited: {audit}"
        );

        let mut on = off;
        on.network.station_agent.remote_tx_enabled = true;
        coordinator.handle_config_reload(Ok(on)).await;
        assert!(permitted(&coordinator, "A"), "arm kept; consent restored");
        assert!(texts(&diagnostics(&rx)).contains(&MSG_STATION_AGENT_CONSENT_ON));
    }

    #[tokio::test]
    async fn wsjtx_consent_follows_the_saved_file() {
        let mut config = base();
        config.network.wsjtx_udp.enabled = true;
        config.network.wsjtx_udp.allow_tx_initiation = true;
        let mut coordinator = test_coordinator(config).await;
        assert!(coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::WsjtxUdp));
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network.wsjtx_udp.allow_tx_initiation = false;
        coordinator.handle_config_reload(Ok(new)).await;
        assert!(!coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::WsjtxUdp));
        assert!(texts(&diagnostics(&rx)).contains(&MSG_WSJTX_CONSENT_OFF));
    }

    #[tokio::test]
    async fn allow_list_removal_on_an_inert_agent_updates_the_snapshot_only() {
        let mut coordinator = test_coordinator(base()).await;
        assert!(coordinator.station_agent_live.is_none());
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.tx_allow_list = vec!["B".to_string()];
        coordinator.handle_config_reload(Ok(new)).await;
        assert_eq!(
            coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .tx_allow_list,
            vec!["B".to_string()]
        );
        let lines = diagnostics(&rx);
        assert!(
            texts(&lines).iter().any(
                |t| t.starts_with("tx_allow_list entry removed: A — station agent not running")
            ),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn allow_list_removal_with_a_live_session_prunes_disarms_and_bumps_generation() {
        let mut coordinator = test_coordinator(base()).await;
        let live = crate::coordinator::station_agent::StationAgentLiveHandles {
            tx_allow_list: Arc::new(std::sync::RwLock::new(
                ["A".to_string(), "B".to_string()].into_iter().collect(),
            )),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(
                ["A".to_string(), "B".to_string()].into_iter().collect(),
            )),
            client_keys: Arc::new(std::sync::RwLock::new(Default::default())),
            revocation_pending: Arc::new(AtomicBool::new(false)),
            authz_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        coordinator.station_agent_live = Some(live.clone());
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, 0);
        arm_client(&coordinator, "A");
        let rx = tui_rx(&coordinator).await;

        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.tx_allow_list = vec!["B".to_string()];
        coordinator.handle_config_reload(Ok(new)).await;

        assert!(!live.tx_allow_list.read().unwrap().contains("A"));
        assert!(!live.tx_arm_allow_list.read().unwrap().contains("A"));
        assert_eq!(
            live.authz_generation
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert!(live
            .revocation_pending
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(!permitted(&coordinator, "A"));
        assert!(texts(&diagnostics(&rx))
            .contains(&"tx_allow_list entry removed: A — session evicted, authorization revoked"));
    }

    #[tokio::test]
    async fn allow_list_addition_and_transport_change_are_restart_required() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network
            .station_agent
            .tx_allow_list
            .push("C".to_string());
        new.network.station_agent.relay_url = Some("wss://relay.example".to_string());
        coordinator.handle_config_reload(Ok(new)).await;
        let lines = diagnostics(&rx);
        let t = texts(&lines);
        assert!(t.contains(&MSG_ALLOW_LIST_ADDED), "{lines:?}");
        assert!(
            t.contains(&"station_agent transport settings changed (relay_url) — restart required")
        );
    }

    #[tokio::test]
    async fn disabling_the_station_agent_stops_it_without_a_supervisor_crash() {
        let mut config = base();
        config.network.station_agent.enabled = true;
        let mut coordinator = test_coordinator(config).await;
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, 0);
        arm_client(&coordinator, "A");
        // A stand-in for the running session and poll tasks.
        let session = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok::<(), anyhow::Error>(())
        });
        let session_abort = session.abort_handle();
        coordinator
            .named_task_handles
            .push((ComponentId::StationAgent, session));
        let poll = tokio::spawn(std::future::pending::<()>());
        coordinator.station_agent_poll = Some(poll.abort_handle());
        coordinator.station_agent_live =
            Some(crate::coordinator::station_agent::StationAgentLiveHandles {
                tx_allow_list: Default::default(),
                tx_arm_allow_list: Default::default(),
                client_keys: Default::default(),
                revocation_pending: Default::default(),
                authz_generation: Default::default(),
            });
        let rx = tui_rx(&coordinator).await;

        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.enabled = false;
        coordinator.handle_config_reload(Ok(new)).await;

        assert!(
            !coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .enabled
        );
        assert!(session_abort.is_finished(), "session task stopped");
        assert!(poll.await.unwrap_err().is_cancelled(), "poll task stopped");
        assert!(coordinator.station_agent_poll.is_none());
        assert!(coordinator.station_agent_live.is_none());
        assert!(!permitted(&coordinator, "A"), "definitively disarmed");
        assert_eq!(
            coordinator
                .named_task_handles
                .iter()
                .filter(|(id, _)| *id == ComponentId::StationAgent)
                .count(),
            1,
            "only the inert drain task remains"
        );
        assert!(texts(&diagnostics(&rx)).contains(&MSG_STATION_AGENT_DISABLED));

        coordinator.check_task_handles().await;
        let status = coordinator.component_status.read().await;
        assert!(
            !matches!(
                status.get(&ComponentId::StationAgent).map(|s| &s.state),
                Some(crate::coordinator::ComponentState::Failed(_))
            ),
            "a deliberate stop is not a crash"
        );
    }

    #[tokio::test]
    async fn enabling_the_station_agent_only_reports_restart_required() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.enabled = true;
        coordinator.handle_config_reload(Ok(new)).await;
        assert!(coordinator
            .named_task_handles
            .iter()
            .all(|(id, _)| *id != ComponentId::StationAgent));
        assert!(texts(&diagnostics(&rx)).contains(&MSG_STATION_AGENT_ENABLED));
    }

    #[tokio::test]
    async fn watcher_is_not_started_without_a_config_file() {
        let mut coordinator = test_coordinator(base()).await;
        coordinator.start_config_reload_watcher().await.unwrap();
        assert!(coordinator.config_reload_rx.is_none());
        assert!(coordinator.config_reload_watcher.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_forwards_a_saved_edit_to_the_main_loop_channel() {
        let dir = temp_path("watch");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        let mut config = base();
        config.station.callsign = "K1TEST".to_string();
        config.station.grid_square = "FN42".to_string();
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();

        let mut coordinator = test_coordinator(config.clone()).await;
        coordinator.config_path = path.clone();
        coordinator
            .start_config_reload_watcher_with_settle(Duration::from_millis(100))
            .await
            .unwrap();
        let mut rx = coordinator
            .config_reload_rx
            .take()
            .expect("watcher started");

        let mut edited = config.clone();
        edited.network.station_agent.remote_tx_enabled = false;
        // Give the watcher a moment to register before the write.
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&path, toml::to_string(&edited).unwrap()).unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(15), rx.recv())
            .await
            .expect("a reload outcome within 15 s")
            .expect("channel open");
        let reloaded = outcome.expect("the saved file loads and validates");
        assert!(!reloaded.network.station_agent.remote_tx_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
