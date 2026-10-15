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
//!   restart-required.** The handler says so in a diagnostic line naming the
//!   keys. Only the live remote-access fields are written into the shared
//!   config snapshot: other code reads that snapshot while running (e.g. the
//!   shutdown PTT-off safety net reads `rig.interface.enabled`), so writing a
//!   restart-required key into it would half-apply that key.
//! - Changes are measured between the previous and the new result of the
//!   same load startup did, not against the running config (which the TUI
//!   also changes live). With `--config` that is the watched file alone.
//!   Without it, startup merges every discovered config source and the
//!   watched file is only one of them, so a reload re-loads and validates the
//!   merged configuration (environment included) and diffs merged results: a
//!   grant saved in a file that a higher-priority source overrides changes
//!   nothing, and a change in a higher-priority source is applied by the next
//!   reload.
//! - An edit saved between the startup load and the watcher's first read is
//!   reconciled when the watcher starts: its live remote-access changes are
//!   applied then (see `start_config_reload_watcher`).
//! - C19: every reload is classified by [`classify_config_reload`]. A
//!   QSO-latched field (callsign, grid, slot parity) is never written to the
//!   running state at all — it is restart-required — so no reload can clobber
//!   an in-progress QSO.

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
/// config with its non-fatal load warnings (e.g. an unrecognized section),
/// or the load/validation error text.
pub(crate) type ConfigReloadOutcome = Result<(Config, Vec<String>), String>;

/// Loads (and validates) the configuration a reload applies: the watched
/// file alone with `--config`, otherwise every merged source the way startup
/// loads them (see [`ApplicationCoordinator::set_config_reload_loader`]).
pub(crate) type ConfigReloadLoader = std::sync::Arc<dyn Fn() -> ConfigReloadOutcome + Send + Sync>;

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
/// First line of a startup reconciliation (see
/// `start_config_reload_watcher_with_settle`).
pub(crate) const MSG_CHANGED_DURING_STARTUP: &str =
    "Config changed while pancetta was starting — applying remote-access changes";
/// Prefix of the line listing saved keys that need a restart.
pub(crate) const MSG_RESTART_REQUIRED_PREFIX: &str = "Saved changes take effect after a restart:";
/// At most this many key names are listed in that line.
const RESTART_KEYS_LISTED: usize = 6;

/// The station-agent fields [`SecurityConfigDiff`] covers. Changes to them are
/// reported by the security lines, never by the generic restart line.
const STATION_AGENT_SECURITY_KEYS: [&str; 8] = [
    "network.station_agent.remote_tx_enabled",
    "network.station_agent.tx_allow_list",
    "network.station_agent.enabled",
    "network.station_agent.relay_url",
    "network.station_agent.pairing_api_url",
    "network.station_agent.pairing_origin",
    "network.station_agent.key_dir",
    "network.station_agent.audit_log_path",
];

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
    /// Write the applied remote-access fields into the snapshot (see
    /// [`patch_live_security_fields`]); nothing else in it changes.
    PatchSecurity,
    /// No remote-access field changed; leave the snapshot alone.
    Keep,
}

/// One decision for one saved edit: the security actions to apply now (never
/// deferred) and the keys that need a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadApplyPlan {
    /// Remote-access changes. Applied immediately, even while a QSO is
    /// active: a revocation never waits.
    pub apply_security: SecurityConfigDiff,
    /// What happens to the shared snapshot.
    pub snapshot: SnapshotAction,
    /// C19 classification of the edit. A QSO-latched change is never
    /// applied to running state (it appears in `restart_required_keys`), so
    /// `DeferQsoLatched` can never clobber an in-progress QSO.
    pub applicability: ConfigReloadApplicability,
    /// Dotted names of the changed keys outside the remote-access settings,
    /// sorted. All restart-required.
    pub restart_required_keys: Vec<String>,
}

impl ReloadApplyPlan {
    /// Decide how to apply a saved edit, from the watched file's previous
    /// content `old` and its new content `new`, given whether a QSO is
    /// active.
    pub fn decide(old: &Config, new: &Config, qso_active: bool) -> Self {
        let applicability = classify_config_reload(old, new, qso_active);
        let apply_security = SecurityConfigDiff::diff(old, new);
        let snapshot = if apply_security.is_empty() {
            SnapshotAction::Keep
        } else {
            SnapshotAction::PatchSecurity
        };
        let restart_required_keys = if applicability == ConfigReloadApplicability::NoChange {
            Vec::new()
        } else {
            changed_keys(old, new)
                .into_iter()
                .filter(|k| !is_security_key(k))
                .collect()
        };
        Self {
            apply_security,
            snapshot,
            applicability,
            restart_required_keys,
        }
    }
}

/// Whether a dotted key is covered by [`SecurityConfigDiff`] (and so has its
/// own diagnostic line rather than the generic restart line).
fn is_security_key(key: &str) -> bool {
    STATION_AGENT_SECURITY_KEYS
        .iter()
        .any(|k| key == *k || key.starts_with(&format!("{k}.")))
        || key == "network.wsjtx_udp.allow_tx_initiation"
}

/// The dotted names of every leaf value that differs between `a` and `b`
/// (arrays compare whole), sorted. Empty when either side fails to serialize.
pub(crate) fn changed_keys(a: &Config, b: &Config) -> Vec<String> {
    let (Ok(a), Ok(b)) = (toml::Value::try_from(a), toml::Value::try_from(b)) else {
        return Vec::new();
    };
    let mut fa = std::collections::BTreeMap::new();
    let mut fb = std::collections::BTreeMap::new();
    flatten_toml("", &a, &mut fa);
    flatten_toml("", &b, &mut fb);
    let mut keys: Vec<String> = fa
        .keys()
        .chain(fb.keys())
        .filter(|k| fa.get(*k) != fb.get(*k))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn flatten_toml(
    prefix: &str,
    value: &toml::Value,
    out: &mut std::collections::BTreeMap<String, toml::Value>,
) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_toml(&key, v, out);
            }
        }
        leaf => {
            out.insert(prefix.to_string(), leaf.clone());
        }
    }
}

/// Write what [`ApplicationCoordinator::apply_security_config`] applies into
/// the running config snapshot, and nothing more: consent values, the
/// allow-list minus removed entries (additions are restart-required), and
/// `station_agent.enabled = false`. A component the supervisor restarts then
/// comes back with the same remote-access state the running station has.
pub(crate) fn patch_live_security_fields(
    running: &mut Config,
    diff: &SecurityConfigDiff,
    new: &Config,
) {
    if let Some(on) = diff.station_agent_consent {
        running.network.station_agent.remote_tx_enabled = on;
    }
    if diff.wsjtx_consent.is_some() {
        running.network.wsjtx_udp.allow_tx_initiation = new.network.wsjtx_udp.allow_tx_initiation;
        running.network.wsjtx_udp.enabled = new.network.wsjtx_udp.enabled;
    }
    if !diff.allow_list_removed.is_empty() {
        running
            .network
            .station_agent
            .tx_allow_list
            .retain(|k| !diff.allow_list_removed.contains(k));
    }
    if diff.station_agent_disabled {
        running.network.station_agent.enabled = false;
    }
}

/// Load and validate `path` alone (the `--config` reload loader).
fn load_config_for_reload(path: &Path) -> ConfigReloadOutcome {
    let (config, warnings) =
        Config::load_from_file_with_warnings(path).map_err(|e| e.to_string())?;
    for w in &warnings {
        warn!("Config reload warning: {w}");
    }
    config.validate().map_err(|e| e.to_string())?;
    Ok((config, warnings))
}

/// Turn watcher events into reload outcomes for the main loop. Each burst of
/// events is settled for `settle`, then the file is read once from disk, so
/// the outcome always reflects the file's final content.
async fn forward_config_reload_events(
    mut events: broadcast::Receiver<ConfigReloadEvent>,
    tx: mpsc::Sender<ConfigReloadOutcome>,
    load: ConfigReloadLoader,
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
        let outcome = run_reload_loader(load.clone()).await;
        if tx.send(outcome).await.is_err() {
            return;
        }
    }
}

/// Run a reload loader off the async runtime (it reads files).
async fn run_reload_loader(load: ConfigReloadLoader) -> ConfigReloadOutcome {
    tokio::task::spawn_blocking(move || load())
        .await
        .unwrap_or_else(|e| Err(format!("config reload task failed: {e}")))
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
    /// The config file path as the operator gave it (`--config`, or the
    /// default location), symlinks NOT resolved. Reloads read and watch this
    /// path, so repointing a symlinked config at a new file is picked up.
    pub fn set_config_watch_path(&mut self, path: PathBuf) {
        self.config_watch_path = path;
    }

    /// Set how a reload loads the configuration when the watched file is
    /// only one of several merged sources (no `--config`): the loader must
    /// load and validate the merged configuration exactly as startup did.
    /// Without one, a reload loads `config_watch_path` alone.
    pub fn set_config_reload_loader(
        &mut self,
        loader: impl Fn() -> ConfigReloadOutcome + Send + Sync + 'static,
    ) {
        self.config_reload_loader = Some(std::sync::Arc::new(loader));
    }

    /// The loader reloads use (see [`Self::set_config_reload_loader`]).
    fn reload_loader(&self) -> ConfigReloadLoader {
        match &self.config_reload_loader {
            Some(load) => load.clone(),
            None => {
                let path = self.config_watch_path.clone();
                std::sync::Arc::new(move || load_config_for_reload(&path))
            }
        }
    }

    /// Start watching `config_watch_path` for saved edits. Only runs when
    /// that path is an existing file (a defaults-only run has nothing to
    /// watch). The path's own directory is watched (a symlink repoint shows
    /// up there), and so is the directory of its current symlink target (an
    /// edit to the target shows up there). A watcher that fails to start is
    /// logged and skipped: the station then runs exactly as it did before
    /// PAN-103, with every edit needing a restart.
    pub(crate) async fn start_config_reload_watcher(&mut self) -> anyhow::Result<()> {
        self.start_config_reload_watcher_with_settle(RELOAD_SETTLE)
            .await
    }

    async fn start_config_reload_watcher_with_settle(
        &mut self,
        settle: Duration,
    ) -> anyhow::Result<()> {
        // Absolute but not symlink-resolved: the watcher also watches the
        // path's parent, which a bare relative file name does not have.
        let watch_path = std::path::absolute(&self.config_watch_path)
            .unwrap_or_else(|_| self.config_watch_path.clone());
        if !watch_path.is_file() {
            info!(
                "Config hot reload not started: {} is not a file",
                watch_path.display()
            );
            return Ok(());
        }
        // What the loader returns now is the baseline edits are measured
        // from (see the module doc).
        self.config_watch_path = watch_path.clone();
        let load = self.reload_loader();
        let baseline = match run_reload_loader(load.clone()).await {
            Ok(pair) => pair,
            Err(e) => {
                info!("Config hot reload not started: {e}");
                return Ok(());
            }
        };
        // Startup already showed this file's warnings (`config_warnings`).
        let (baseline, baseline_warnings) = baseline;
        let (mut watcher, events) = match ConfigHotReload::new(baseline.clone(), &watch_path) {
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
        tokio::spawn(forward_config_reload_events(events, tx, load, settle));
        self.config_reload_watcher = Some(watcher);
        self.config_reload_rx = Some(rx);
        self.reconcile_startup_window(&baseline).await;
        self.config_file_baseline = Some(baseline);
        self.config_file_warnings = baseline_warnings;
        // `start_watching` already watches the path's own directory; record
        // its resolved form too so a target in that same directory is not
        // watched twice.
        self.config_link_dirs.clear();
        self.config_target_dir = None;
        if let Some(dir) = watch_path.parent() {
            self.config_link_dirs.push(dir.to_path_buf());
            if let Ok(resolved) = std::fs::canonicalize(dir) {
                self.config_link_dirs.push(resolved);
            }
        }
        self.config_watch_path = watch_path;
        self.watch_config_target_dir().await;
        info!(
            "Watching {} for config changes (remote-access revocations apply live)",
            self.config_watch_path.display()
        );
        Ok(())
    }

    /// Watch the directory of the config path's current symlink target in
    /// place of the previous target's directory. Run at start and after every
    /// reload, so edits to a target that a symlink repoint moved to another
    /// directory are still seen. Returns `true` when it started watching a
    /// directory it did not watch before. The config path's own directory
    /// (`config_link_dirs`) is always watched and never handled here.
    async fn watch_config_target_dir(&mut self) -> bool {
        if self.config_reload_watcher.is_none() {
            return false;
        }
        let path = self.config_watch_path.clone();
        let Ok(Ok(target)) = tokio::task::spawn_blocking(move || std::fs::canonicalize(path)).await
        else {
            return false;
        };
        let dir = target
            .parent()
            .map(Path::to_path_buf)
            .filter(|d| !self.config_link_dirs.contains(d));
        if dir == self.config_target_dir {
            return false;
        }
        let Some(watcher) = self.config_reload_watcher.as_mut() else {
            return false;
        };
        if let Some(dir) = &dir {
            if let Err(e) = watcher.watch_also(dir) {
                warn!("Config hot reload: {e:#}");
                return false;
            }
            info!(
                "Also watching {} (target of {})",
                dir.display(),
                self.config_watch_path.display()
            );
        }
        if let Some(old) = self.config_target_dir.take() {
            if let Err(e) = watcher.unwatch_also(&old) {
                debug!("Config hot reload: {e:#}");
            }
        }
        self.config_target_dir = dir;
        self.config_target_dir.is_some()
    }

    /// C19: a poisoned `active_tx_qsos` lock counts as "a QSO is active", so
    /// a latched change is deferred rather than risked.
    fn config_reload_qso_active(&self) -> bool {
        self.active_tx_qsos
            .read()
            .map(|set| !set.is_empty())
            .unwrap_or(true)
    }

    /// Apply one reload outcome (see [`Self::apply_config_reload`]), then
    /// follow a symlink repoint: when the config's target moved to a
    /// directory not watched yet, watch it and re-read the file once, so an
    /// edit made there before the watch existed is not missed.
    pub(crate) async fn handle_config_reload(&mut self, outcome: ConfigReloadOutcome) {
        self.apply_config_reload(outcome).await;
        if self.watch_config_target_dir().await {
            let outcome = run_reload_loader(self.reload_loader()).await;
            self.apply_config_reload(outcome).await;
        }
    }

    /// Apply one reload outcome: the remote-access changes now (never
    /// deferred), every other changed key reported as restart-required, then
    /// one diagnostic line per action.
    async fn apply_config_reload(&mut self, outcome: ConfigReloadOutcome) {
        let (new, warnings) = match outcome {
            Ok(pair) => pair,
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
        // Measure the edit against the file's previous content; without a
        // baseline (watcher not started) fall back to the running config.
        let old = match self.config_file_baseline.take() {
            Some(baseline) => baseline,
            None => self.config.read().await.clone(),
        };
        // Like the startup `config_warnings`: a misspelled or unrecognized
        // key is otherwise silently ignored. Only warnings the previous load
        // did not have are shown, since a write to any other file in a
        // watched directory also causes a reload.
        let warning_lines: Vec<(DiagnosticLevel, String)> = warnings
            .iter()
            .filter(|w| !self.config_file_warnings.contains(w))
            .map(|w| (DiagnosticLevel::Warn, format!("Config warning: {w}")))
            .collect();
        self.config_file_warnings = warnings;
        let plan = ReloadApplyPlan::decide(&old, &new, self.config_reload_qso_active());
        if plan.applicability == ConfigReloadApplicability::NoChange
            && plan.apply_security.is_empty()
        {
            self.config_file_baseline = Some(new);
            debug!("Config file event with no content change — nothing to apply");
            self.emit_config_reload_lines(warning_lines).await;
            return;
        }

        // A key already holding the saved value in the running config was
        // applied by the action that saved it (e.g. the TUI device picker
        // persists after applying live); it needs no restart.
        let restart_keys: Vec<String> = {
            let running = self.config.read().await;
            let not_yet_running = changed_keys(&running, &new);
            plan.restart_required_keys
                .iter()
                .filter(|k| not_yet_running.contains(k))
                .cloned()
                .collect()
        };

        // The snapshot is patched before the security actions: disabling the
        // station agent restarts it, and the restart re-reads this snapshot.
        if plan.snapshot == SnapshotAction::PatchSecurity {
            patch_live_security_fields(&mut *self.config.write().await, &plan.apply_security, &new);
        }
        info!(?plan.applicability, "Config reloaded");

        let mut lines = vec![(DiagnosticLevel::Info, MSG_RELOADED.to_string())];
        lines.extend(warning_lines);
        lines.extend(self.apply_security_config(&plan.apply_security).await);
        if !restart_keys.is_empty() {
            let mut listed = restart_keys
                .iter()
                .take(RESTART_KEYS_LISTED)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if restart_keys.len() > RESTART_KEYS_LISTED {
                listed.push_str(&format!(
                    " (+{} more)",
                    restart_keys.len() - RESTART_KEYS_LISTED
                ));
            }
            lines.push((
                DiagnosticLevel::Warn,
                format!("{MSG_RESTART_REQUIRED_PREFIX} {listed}"),
            ));
        }
        self.config_file_baseline = Some(new);
        self.emit_config_reload_lines(lines).await;
    }

    /// Apply the live remote-access changes between the configuration the
    /// station started with (the running snapshot) and `baseline`, the
    /// watcher's first read. They differ only when the file was saved after
    /// the startup load and before the watcher existed, an edit no watcher
    /// event will ever report. Only the live parts (consent, allow-list
    /// removals, disabling the agent) are applied; everything else stays as
    /// it would after any reload.
    async fn reconcile_startup_window(&mut self, baseline: &Config) {
        let running = self.config.read().await.clone();
        let full = SecurityConfigDiff::diff(&running, baseline);
        let live = SecurityConfigDiff {
            station_agent_consent: full.station_agent_consent,
            wsjtx_consent: full.wsjtx_consent,
            allow_list_removed: full.allow_list_removed,
            station_agent_disabled: full.station_agent_disabled,
            ..Default::default()
        };
        if live.is_empty() {
            return;
        }
        warn!("{MSG_CHANGED_DURING_STARTUP}");
        patch_live_security_fields(&mut *self.config.write().await, &live, baseline);
        let mut lines = vec![(
            DiagnosticLevel::Warn,
            MSG_CHANGED_DURING_STARTUP.to_string(),
        )];
        lines.extend(self.apply_security_config(&live).await);
        self.emit_config_reload_lines(lines).await;
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
        // Tell the session's disarm guard this is a deliberate stop, so the
        // abort below audits `operator-disarm`, not `component-crash`.
        if let Some(live) = self.station_agent_live.take() {
            live.stop_requested
                .store(true, std::sync::atomic::Ordering::Release);
        }
        for handle in &stopped {
            handle.abort();
        }
        // Wait (bounded) for the session task to drop, which closes the relay
        // socket and every peer's Noise transport.
        for handle in stopped {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
        // Definitive disarm, attributed to the operator's config edit. A no-op
        // when the session's guard already disarmed on drop.
        let effects = {
            let mut st = match self.remote_tx_arm.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            st.disarm_with_reason(
                pancetta_agent::arm::DisarmReason::OperatorDisarm,
                chrono::Utc::now().timestamp_millis(),
            )
        };
        let audit = self.audit_log();
        for effect in &effects {
            if let pancetta_agent::arm::ArmEffect::Audit(ev) = effect {
                audit.append(ev);
            }
        }
        self.remote_tx_arm.clear_poison();
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
    fn decide_never_defers_the_revocation_and_reports_latched_fields_restart_required() {
        let old = base();
        let mut new = old.clone();
        new.station.callsign = "K9NEW".to_string();
        new.network.station_agent.remote_tx_enabled = false;
        let plan = ReloadApplyPlan::decide(&old, &new, true);
        assert_eq!(
            plan.applicability,
            ConfigReloadApplicability::DeferQsoLatched
        );
        assert_eq!(plan.snapshot, SnapshotAction::PatchSecurity);
        assert_eq!(plan.apply_security.station_agent_consent, Some(false));
        assert_eq!(plan.restart_required_keys, vec!["station.callsign"]);
    }

    #[test]
    fn decide_reports_non_security_keys_and_keeps_the_snapshot() {
        let old = base();
        let mut ui = old.clone();
        ui.ui.theme = format!("{}-x", old.ui.theme);
        ui.rig.interface.enabled = !old.rig.interface.enabled;
        let plan = ReloadApplyPlan::decide(&old, &ui, true);
        assert_eq!(plan.applicability, ConfigReloadApplicability::SafeLive);
        assert_eq!(plan.snapshot, SnapshotAction::Keep);
        assert_eq!(
            plan.restart_required_keys,
            vec!["rig.interface.enabled", "ui.theme"]
        );
        assert!(plan.apply_security.is_empty());

        let mut call = old.clone();
        call.station.callsign = "K9NEW".to_string();
        let plan = ReloadApplyPlan::decide(&old, &call, false);
        assert_eq!(plan.applicability, ConfigReloadApplicability::SafeQuiescent);
        assert_eq!(plan.snapshot, SnapshotAction::Keep);
        assert_eq!(plan.restart_required_keys, vec!["station.callsign"]);
    }

    #[test]
    fn decide_security_only_edit_lists_no_restart_keys() {
        let old = base();
        let mut new = old.clone();
        new.network.station_agent.remote_tx_enabled = false;
        new.network.station_agent.tx_allow_list = vec!["B".to_string()];
        new.network.station_agent.relay_url = Some("wss://relay.example".to_string());
        new.network.wsjtx_udp.allow_tx_initiation = !old.network.wsjtx_udp.allow_tx_initiation;
        let plan = ReloadApplyPlan::decide(&old, &new, false);
        assert_eq!(plan.snapshot, SnapshotAction::PatchSecurity);
        assert!(
            plan.restart_required_keys.is_empty(),
            "{:?}",
            plan.restart_required_keys
        );
    }

    #[test]
    fn decide_identical_config_is_a_no_op() {
        let old = base();
        let plan = ReloadApplyPlan::decide(&old, &old.clone(), true);
        assert_eq!(plan.applicability, ConfigReloadApplicability::NoChange);
        assert_eq!(plan.snapshot, SnapshotAction::Keep);
        assert!(plan.apply_security.is_empty());
        assert!(plan.restart_required_keys.is_empty());
    }

    #[test]
    fn patch_writes_only_the_applied_security_fields() {
        let mut running = base();
        running.network.station_agent.enabled = true;
        let mut new = running.clone();
        new.station.callsign = "K9NEW".to_string();
        new.rig.interface.enabled = !running.rig.interface.enabled;
        new.network.station_agent.remote_tx_enabled = false;
        new.network.station_agent.tx_allow_list = vec!["B".to_string(), "C".to_string()];
        new.network.station_agent.enabled = false;
        let diff = SecurityConfigDiff::diff(&running, &new);
        let before_rig = running.rig.interface.enabled;
        let before_call = running.station.callsign.clone();
        patch_live_security_fields(&mut running, &diff, &new);
        assert!(!running.network.station_agent.remote_tx_enabled);
        assert_eq!(
            running.network.station_agent.tx_allow_list,
            vec!["B".to_string()],
            "removal applied, addition not"
        );
        assert!(!running.network.station_agent.enabled);
        assert_eq!(running.rig.interface.enabled, before_rig);
        assert_eq!(running.station.callsign, before_call);

        // Enabling is restart-required: never written.
        let mut off = base();
        let mut on = off.clone();
        on.network.station_agent.enabled = true;
        let diff = SecurityConfigDiff::diff(&off, &on);
        patch_live_security_fields(&mut off, &diff, &on);
        assert!(!off.network.station_agent.enabled);
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
    async fn reload_with_a_non_security_change_keeps_running_config_and_says_restart() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let before = toml::to_string(&*coordinator.config.read().await).unwrap();
        let ptt_off_before = coordinator.wants_direct_rigctld_ptt_off().await;
        let mut new = coordinator.config.read().await.clone();
        new.ui.theme = format!("{}-x", new.ui.theme);
        new.rig.interface.enabled = !new.rig.interface.enabled;
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;

        assert_eq!(
            toml::to_string(&*coordinator.config.read().await).unwrap(),
            before,
            "a restart-required key is never half-applied"
        );
        assert_eq!(
            coordinator.wants_direct_rigctld_ptt_off().await,
            ptt_off_before
        );
        let lines = diagnostics(&rx);
        assert_eq!(
            lines.iter().filter(|(_, t)| t == MSG_RELOADED).count(),
            1,
            "{lines:?}"
        );
        assert!(texts(&lines).contains(
            &"Saved changes take effect after a restart: rig.interface.enabled, ui.theme"
        ));
    }

    #[tokio::test]
    async fn a_key_the_tui_already_applied_is_not_reported_as_restart_required() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.audio.output_device = "Picked In TUI".to_string();
        // What `TuiCommand::SelectDevice` does: apply live, then persist.
        coordinator.config.write().await.audio.output_device = "Picked In TUI".to_string();
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
        let lines = diagnostics(&rx);
        assert!(
            !lines
                .iter()
                .any(|(_, t)| t.starts_with(MSG_RESTART_REQUIRED_PREFIX)),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn edits_are_measured_against_the_file_not_values_from_other_sources() {
        // Running config: consent OFF (e.g. from another config file); the
        // watched file has said `true` all along.
        let mut running = base();
        running.network.station_agent.remote_tx_enabled = false;
        let mut coordinator = test_coordinator(running).await;
        let mut file = coordinator.config.read().await.clone();
        file.network.station_agent.remote_tx_enabled = true;
        coordinator.config_file_baseline = Some(file.clone());

        // An unrelated save must not turn consent on.
        let mut saved = file.clone();
        saved.ui.theme = format!("{}-x", saved.ui.theme);
        coordinator
            .handle_config_reload(Ok((saved.clone(), Vec::new())))
            .await;
        assert!(!coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::StationAgent));
        assert!(
            !coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .remote_tx_enabled
        );
        assert_eq!(
            coordinator.config_file_baseline.as_ref().unwrap().ui.theme,
            saved.ui.theme,
            "the baseline follows the file"
        );
    }

    /// The merged-mode loader `main.rs` installs, over explicit search
    /// directories (lowest precedence first) instead of the real ones.
    fn merged_loader(dirs: Vec<std::path::PathBuf>) -> impl Fn() -> ConfigReloadOutcome {
        move || {
            let loader = pancetta_config::ConfigLoader::with_search_paths(dirs.clone())
                .map_err(|e| e.to_string())?;
            let mut config = loader.load().map_err(|e| e.to_string())?;
            config.metadata = None;
            Ok((config, loader.load_warnings()))
        }
    }

    const STATION_TOML: &str = "[station]\ncallsign = \"K1TEST\"\ngrid_square = \"FN42\"\n";

    fn station_agent_toml(remote_tx_enabled: bool) -> String {
        format!(
            "{STATION_TOML}[network.station_agent]\nremote_tx_enabled = {remote_tx_enabled}\ntx_allow_list = [\"A\", \"B\"]\n"
        )
    }

    /// A merged-mode coordinator: `dirs` each hold a `pancetta.toml` (lowest
    /// precedence first); the running config and the baseline are the merged
    /// result, as at startup.
    async fn merged_coordinator(
        dirs: Vec<std::path::PathBuf>,
    ) -> (ApplicationCoordinator, impl Fn() -> ConfigReloadOutcome) {
        let load = merged_loader(dirs.clone());
        let (startup, _) = load().expect("merged config loads");
        let mut coordinator = test_coordinator(startup.clone()).await;
        let seeded = startup.network.station_agent.remote_tx_enabled;
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, seeded, 0);
        coordinator.set_config_reload_loader(merged_loader(dirs));
        // The running config also carries test_coordinator's audit-log path;
        // the baseline is the merged load, like the watcher's first read.
        coordinator.config_file_baseline = Some(load().unwrap().0);
        (coordinator, load)
    }

    fn station_agent_consent(coordinator: &ApplicationCoordinator) -> bool {
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::StationAgent)
    }

    /// Codex P1 on #436: without `--config`, a higher-priority source that
    /// keeps `remote_tx_enabled = false` wins over a grant saved in the
    /// watched lower-priority file.
    #[tokio::test]
    async fn merged_mode_grant_overridden_by_a_higher_priority_source_changes_nothing() {
        let root = temp_path("merged-grant");
        let (watched, high) = (root.join("watched"), root.join("high"));
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&high).unwrap();
        std::fs::write(watched.join("pancetta.toml"), STATION_TOML).unwrap();
        std::fs::write(high.join("pancetta.toml"), station_agent_toml(false)).unwrap();
        let (mut coordinator, load) = merged_coordinator(vec![watched.clone(), high]).await;
        assert!(!station_agent_consent(&coordinator), "setup: consent off");
        let rx = tui_rx(&coordinator).await;

        std::fs::write(watched.join("pancetta.toml"), station_agent_toml(true)).unwrap();
        coordinator.handle_config_reload(load()).await;

        assert!(
            !station_agent_consent(&coordinator),
            "the higher-priority false wins"
        );
        assert!(
            !coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .remote_tx_enabled
        );
        let lines = diagnostics(&rx);
        assert!(
            !texts(&lines).contains(&MSG_STATION_AGENT_CONSENT_ON),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Merged mode measures edits on the merged result, not on the watched
    /// file: a revocation in a higher-priority source (which is not itself
    /// watched) is applied by the next reload, and a key the watched file
    /// leaves at its default never hides one (Codex P1 on #436, round 2).
    #[tokio::test]
    async fn merged_mode_applies_a_revocation_from_a_higher_priority_source() {
        let root = temp_path("merged-revoke");
        let (watched, high) = (root.join("watched"), root.join("high"));
        std::fs::create_dir_all(&watched).unwrap();
        std::fs::create_dir_all(&high).unwrap();
        std::fs::write(watched.join("pancetta.toml"), STATION_TOML).unwrap();
        std::fs::write(high.join("pancetta.toml"), station_agent_toml(true)).unwrap();
        let (mut coordinator, load) = merged_coordinator(vec![watched.clone(), high.clone()]).await;
        assert!(station_agent_consent(&coordinator), "setup: consent on");
        arm_client(&coordinator, "A");
        assert!(permitted(&coordinator, "A"), "setup: armed");
        let rx = tui_rx(&coordinator).await;

        std::fs::write(high.join("pancetta.toml"), station_agent_toml(false)).unwrap();
        // Any save of the watched file triggers the reload.
        std::fs::write(
            watched.join("pancetta.toml"),
            format!("{STATION_TOML}# touched\n"),
        )
        .unwrap();
        coordinator.handle_config_reload(load()).await;

        assert!(!station_agent_consent(&coordinator), "consent revoked");
        assert!(!permitted(&coordinator, "A"), "remote TX refused");
        assert!(
            texts(&diagnostics(&rx)).contains(&MSG_STATION_AGENT_CONSENT_OFF),
            "the revocation is reported"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Codex P2 on #436: a revocation saved after the startup load but
    /// before the watcher's first read is applied when the watcher starts.
    #[tokio::test]
    async fn an_edit_saved_before_the_watcher_started_is_reconciled() {
        let dir = temp_path("startup-window");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        let mut config = base();
        config.station.callsign = "K1TEST".to_string();
        config.station.grid_square = "FN42".to_string();
        let mut coordinator = test_coordinator(config.clone()).await;
        coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .set_channel_consent(ConsentChannel::StationAgent, true, 0);
        let rx = tui_rx(&coordinator).await;
        // Saved during startup: consent off.
        let mut saved = coordinator.config.read().await.clone();
        saved.network.station_agent.remote_tx_enabled = false;
        std::fs::write(&path, toml::to_string(&saved).unwrap()).unwrap();

        coordinator.set_config_watch_path(path.clone());
        coordinator
            .start_config_reload_watcher_with_settle(Duration::from_millis(100))
            .await
            .unwrap();

        assert!(coordinator.config_reload_rx.is_some(), "watcher started");
        assert!(
            !station_agent_consent(&coordinator),
            "the startup-window edit applied"
        );
        assert!(
            !coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .remote_tx_enabled
        );
        let lines = diagnostics(&rx);
        assert!(
            texts(&lines).contains(&MSG_CHANGED_DURING_STARTUP),
            "{lines:?}"
        );
        assert!(
            texts(&lines).contains(&MSG_STATION_AGENT_CONSENT_OFF),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The common case: the file has not changed since startup, so starting
    /// the watcher applies and reports nothing.
    #[tokio::test]
    async fn starting_the_watcher_on_an_unchanged_file_reports_nothing() {
        let dir = temp_path("startup-unchanged");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        let mut config = base();
        config.station.callsign = "K1TEST".to_string();
        config.station.grid_square = "FN42".to_string();
        let mut coordinator = test_coordinator(config).await;
        let rx = tui_rx(&coordinator).await;
        let running = coordinator.config.read().await.clone();
        std::fs::write(&path, toml::to_string(&running).unwrap()).unwrap();
        coordinator.set_config_watch_path(path.clone());
        coordinator
            .start_config_reload_watcher_with_settle(Duration::from_millis(100))
            .await
            .unwrap();
        assert!(coordinator.config_reload_rx.is_some());
        assert!(diagnostics(&rx).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reload_with_identical_content_emits_nothing() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let same = coordinator.config.read().await.clone();
        coordinator
            .handle_config_reload(Ok((same, Vec::new())))
            .await;
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
    async fn latched_change_is_never_applied_but_a_revocation_in_the_same_save_is() {
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
        coordinator
            .handle_config_reload(Ok((new.clone(), Vec::new())))
            .await;

        {
            let cfg = coordinator.config.read().await;
            assert_eq!(
                cfg.station.callsign, running_call,
                "C19: latched field untouched"
            );
            assert!(!cfg.network.station_agent.remote_tx_enabled);
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
        assert!(
            texts(&lines).contains(&"Saved changes take effect after a restart: station.callsign"),
            "{lines:?}"
        );

        // Nor once the QSO ends: the callsign needs a restart.
        coordinator.active_tx_qsos.write().unwrap().clear();
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
        assert_eq!(
            coordinator.config.read().await.station.callsign,
            running_call
        );
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
        coordinator
            .handle_config_reload(Ok((off.clone(), Vec::new())))
            .await;
        assert!(
            !permitted(&coordinator, "A"),
            "queued/in-flight remote TX refused"
        );
        let lines = diagnostics(&rx);
        assert!(
            texts(&lines).contains(&MSG_STATION_AGENT_CONSENT_OFF),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|(_, t)| t.starts_with(MSG_RESTART_REQUIRED_PREFIX)),
            "{lines:?}"
        );
        let audit = std::fs::read_to_string(coordinator.audit_log().path()).unwrap();
        assert!(
            audit.contains("local consent station_agent=false"),
            "consent change audited: {audit}"
        );

        let mut on = off;
        on.network.station_agent.remote_tx_enabled = true;
        coordinator.handle_config_reload(Ok((on, Vec::new()))).await;
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
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
        assert!(!coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::WsjtxUdp));
        assert!(texts(&diagnostics(&rx)).contains(&MSG_WSJTX_CONSENT_OFF));

        let mut on = coordinator.config.read().await.clone();
        on.network.wsjtx_udp.allow_tx_initiation = true;
        coordinator.handle_config_reload(Ok((on, Vec::new()))).await;
        assert!(coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::WsjtxUdp));
        assert!(texts(&diagnostics(&rx)).contains(&MSG_WSJTX_CONSENT_ON));
        let audit = std::fs::read_to_string(coordinator.audit_log().path()).unwrap();
        assert!(audit.contains("local consent wsjtx_udp=false"), "{audit}");
    }

    #[tokio::test]
    async fn allow_list_removal_on_an_inert_agent_updates_the_snapshot_only() {
        let mut coordinator = test_coordinator(base()).await;
        assert!(coordinator.station_agent_live.is_none());
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.tx_allow_list = vec!["B".to_string()];
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
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
            stop_requested: Default::default(),
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
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;

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
        let live = crate::coordinator::station_agent::StationAgentLiveHandles {
            tx_allow_list: Arc::new(std::sync::RwLock::new(
                ["A".to_string(), "B".to_string()].into_iter().collect(),
            )),
            tx_arm_allow_list: Arc::new(std::sync::RwLock::new(
                ["A".to_string(), "B".to_string()].into_iter().collect(),
            )),
            client_keys: Default::default(),
            revocation_pending: Default::default(),
            authz_generation: Default::default(),
            stop_requested: Default::default(),
        };
        coordinator.station_agent_live = Some(live.clone());
        let rx = tui_rx(&coordinator).await;
        let mut new = coordinator.config.read().await.clone();
        new.network
            .station_agent
            .tx_allow_list
            .push("C".to_string());
        new.network.station_agent.relay_url = Some("wss://relay.example".to_string());
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
        let lines = diagnostics(&rx);
        let t = texts(&lines);
        assert!(t.contains(&MSG_ALLOW_LIST_ADDED), "{lines:?}");
        assert!(
            t.contains(&"station_agent transport settings changed (relay_url) — restart required")
        );
        assert!(
            !live.tx_allow_list.read().unwrap().contains("C"),
            "an addition is not applied live"
        );
        assert_eq!(live.tx_allow_list.read().unwrap().len(), 2);
        assert_eq!(
            live.authz_generation
                .load(std::sync::atomic::Ordering::Acquire),
            0
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
        let stop_requested = Arc::new(AtomicBool::new(false));
        coordinator.station_agent_live =
            Some(crate::coordinator::station_agent::StationAgentLiveHandles {
                tx_allow_list: Default::default(),
                tx_arm_allow_list: Default::default(),
                client_keys: Default::default(),
                revocation_pending: Default::default(),
                authz_generation: Default::default(),
                stop_requested: stop_requested.clone(),
            });
        let rx = tui_rx(&coordinator).await;

        let mut new = coordinator.config.read().await.clone();
        new.network.station_agent.enabled = false;
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;

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
        let audit = std::fs::read_to_string(coordinator.audit_log().path()).unwrap();
        assert!(
            audit.contains("disarmed: operator-disarm"),
            "a config disable is audited as an operator disarm: {audit}"
        );
        assert!(
            !audit.contains("component-crash"),
            "a config disable is never audited as a crash: {audit}"
        );
        assert!(
            stop_requested.load(std::sync::atomic::Ordering::Acquire),
            "the session's disarm guard was told this is a deliberate stop"
        );

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
        coordinator
            .handle_config_reload(Ok((new, Vec::new())))
            .await;
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
        coordinator.set_config_watch_path(path.clone());
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

        // A late event for the initial write (macOS FSEvents can deliver it
        // after the sleep above) forwards the pre-edit content first; wait
        // for the outcome carrying the edit.
        let outcome = next_outcome_with_consent(&mut rx, false).await;
        let (reloaded, _) = outcome.expect("the saved file loads and validates");
        assert!(!reloaded.network.station_agent.remote_tx_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Wait for the next reload outcome whose `remote_tx_enabled` is `want`.
    async fn next_outcome_with_consent(
        rx: &mut mpsc::Receiver<ConfigReloadOutcome>,
        want: bool,
    ) -> ConfigReloadOutcome {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let outcome = rx.recv().await.expect("channel open");
                if matches!(&outcome, Ok((c, _)) if c.network.station_agent.remote_tx_enabled == want)
                {
                    return outcome;
                }
            }
        })
        .await
        .expect("a matching reload outcome within 15 s")
    }

    /// `--config` names a symlink that a dotfile manager repoints at a new
    /// file in another directory; the new file is then edited, once before
    /// the coordinator handled the repoint and once after.
    /// `config_path` is the resolved target, as `main.rs` passes it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_follows_a_symlinked_config_repointed_at_a_new_file() {
        let root = temp_path("symlink");
        let (link_dir, old_dir, new_dir) = (root.join("link"), root.join("v1"), root.join("v2"));
        for d in [&link_dir, &old_dir, &new_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut config = base();
        config.station.callsign = "K1TEST".to_string();
        config.station.grid_square = "FN42".to_string();
        let old_target = old_dir.join("pancetta.toml");
        std::fs::write(&old_target, toml::to_string(&config).unwrap()).unwrap();
        let link = link_dir.join("pancetta.toml");
        std::os::unix::fs::symlink(&old_target, &link).unwrap();

        let mut coordinator = test_coordinator(config.clone()).await;
        coordinator.config_path = std::fs::canonicalize(&link).unwrap();
        coordinator.set_config_watch_path(link.clone());
        coordinator
            .start_config_reload_watcher_with_settle(Duration::from_millis(100))
            .await
            .unwrap();
        let mut rx = coordinator
            .config_reload_rx
            .take()
            .expect("watcher started");
        assert_eq!(
            coordinator.config_target_dir,
            Some(std::fs::canonicalize(&old_dir).unwrap())
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Repoint: the old target never changes.
        let mut revoked = config.clone();
        revoked.network.station_agent.remote_tx_enabled = false;
        let new_target = new_dir.join("pancetta.toml");
        std::fs::write(&new_target, toml::to_string(&revoked).unwrap()).unwrap();
        let tmp_link = link_dir.join(".pancetta.toml.tmp");
        std::os::unix::fs::symlink(&new_target, &tmp_link).unwrap();
        std::fs::rename(&tmp_link, &link).unwrap();
        let outcome = next_outcome_with_consent(&mut rx, false).await;
        // Saved before the new directory is watched: no event fires for it.
        let mut pruned = revoked.clone();
        pruned.network.station_agent.tx_allow_list = vec!["B".to_string()];
        std::fs::write(&new_target, toml::to_string(&pruned).unwrap()).unwrap();
        coordinator.handle_config_reload(outcome).await;
        assert!(!coordinator
            .remote_tx_arm
            .lock()
            .unwrap()
            .channel_consent(ConsentChannel::StationAgent));
        assert_eq!(
            coordinator.config_target_dir,
            Some(std::fs::canonicalize(&new_dir).unwrap())
        );
        assert_eq!(
            coordinator
                .config
                .read()
                .await
                .network
                .station_agent
                .tx_allow_list,
            vec!["B".to_string()],
            "the file is re-read once its new directory is watched"
        );

        // An in-place edit of the new target is seen too. Wait out the
        // library's 500 ms reload debounce first (this test's settle is
        // shorter than the production 750 ms that covers it).
        tokio::time::sleep(Duration::from_millis(700)).await;
        std::fs::write(&new_target, toml::to_string(&config).unwrap()).unwrap();
        assert!(next_outcome_with_consent(&mut rx, true).await.is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reload_load_returns_the_loader_warnings() {
        let dir = temp_path("warn");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pancetta.toml");
        std::fs::write(
            &path,
            "[station]\ncallsign = \"K1TEST\"\ngrid_square = \"FN42\"\n\n[autonomous_operator]\nenabled = true\n",
        )
        .unwrap();
        let (_, warnings) = load_config_for_reload(&path).expect("loads");
        assert!(
            warnings.iter().any(|w| w.contains("autonomous_operator")),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_reload_warning_reaches_the_operator_once() {
        let mut coordinator = test_coordinator(base()).await;
        let rx = tui_rx(&coordinator).await;
        let first = "unknown config section [autonomous_operator]".to_string();
        let second = "unknown key [ui].themee".to_string();

        // A save adding an ignored key leaves the parsed config unchanged.
        let same = coordinator.config.read().await.clone();
        coordinator
            .handle_config_reload(Ok((same.clone(), vec![first.clone()])))
            .await;
        let lines = diagnostics(&rx);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, DiagnosticLevel::Warn);
        assert!(lines[0].1.contains(&first), "{lines:?}");

        // A reload caused by another file in the watched directory.
        coordinator
            .handle_config_reload(Ok((same.clone(), vec![first.clone()])))
            .await;
        assert!(diagnostics(&rx).is_empty());

        let mut changed = same;
        changed.ui.theme = format!("{}-x", changed.ui.theme);
        coordinator
            .handle_config_reload(Ok((changed, vec![first.clone(), second.clone()])))
            .await;
        let lines = diagnostics(&rx);
        assert!(texts(&lines).contains(&MSG_RELOADED), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|(l, t)| *l == DiagnosticLevel::Warn && t.contains(&second)),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|(_, t)| t.contains(&first)), "{lines:?}");
    }
}
