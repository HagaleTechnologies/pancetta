// Configuration Hot Reload
//
// This module provides file watching and hot reload capabilities for the Pancetta
// configuration system. It monitors configuration files for changes and automatically
// reloads them without requiring application restart.

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info};

use crate::Config;

/// Start of [`ConfigReloadEvent::error`] when the watched file no longer
/// exists. The current and last good configuration are kept.
pub const CONFIG_FILE_REMOVED: &str = "Configuration file removed";

/// Configuration reload event
#[derive(Debug, Clone)]
pub struct ConfigReloadEvent {
    /// Path to the changed configuration file
    pub path: PathBuf,
    /// New configuration (if successfully loaded)
    pub config: Option<Config>,
    /// Error message (if reload failed)
    pub error: Option<String>,
    /// Non-fatal load warnings (e.g. an unrecognized top-level config
    /// section) from this reload. Empty on a clean load or a failed reload
    /// (`error.is_some()`) — a failed load has no config to have warned
    /// about beyond the fatal error itself.
    pub warnings: Vec<String>,
    /// Timestamp of the event
    pub timestamp: Instant,
}

/// Configuration hot reload manager
pub struct ConfigHotReload {
    /// Current configuration
    config: Arc<RwLock<Config>>,
    /// Configuration file path
    config_path: PathBuf,
    /// File watcher
    watcher: Option<RecommendedWatcher>,
    /// Event broadcaster
    event_tx: broadcast::Sender<ConfigReloadEvent>,
    /// Debounce duration to avoid multiple rapid reloads
    debounce_duration: Duration,
    /// Last reload timestamp
    last_reload: Arc<RwLock<Option<Instant>>>,
    /// Validation before reload
    validate_before_reload: bool,
    /// Backup of last known good configuration
    last_good_config: Arc<RwLock<Option<Config>>>,
}

impl ConfigHotReload {
    /// Create new hot reload manager
    pub fn new(
        config: Config,
        config_path: impl AsRef<Path>,
    ) -> Result<(Self, broadcast::Receiver<ConfigReloadEvent>)> {
        // Verify the config file exists
        if !config_path.as_ref().exists() {
            return Err(anyhow::anyhow!(
                "Configuration file does not exist: {}",
                config_path.as_ref().display()
            ));
        }
        Self::new_allow_missing(config, config_path)
    }

    /// Like [`new`](Self::new), but the file itself may be absent for now
    /// (e.g. mid-way through a non-atomic replacement): only its parent
    /// directory must exist. [`start_watching`](Self::start_watching) then
    /// watches the parent, so the file's re-creation triggers a reload.
    pub fn new_allow_missing(
        config: Config,
        config_path: impl AsRef<Path>,
    ) -> Result<(Self, broadcast::Receiver<ConfigReloadEvent>)> {
        let config_path = config_path.as_ref().to_path_buf();
        let (event_tx, event_rx) = broadcast::channel(100);

        if !config_path.parent().is_some_and(Path::is_dir) {
            return Err(anyhow::anyhow!(
                "Configuration directory does not exist: {}",
                config_path.display()
            ));
        }

        let manager = Self {
            config: Arc::new(RwLock::new(config.clone())),
            config_path,
            watcher: None,
            event_tx,
            debounce_duration: Duration::from_millis(500),
            last_reload: Arc::new(RwLock::new(None)),
            validate_before_reload: true,
            last_good_config: Arc::new(RwLock::new(Some(config))),
        };

        Ok((manager, event_rx))
    }

    /// Start watching for configuration changes
    pub async fn start_watching(&mut self) -> Result<()> {
        info!(
            "Starting configuration hot reload for: {}",
            self.config_path.display()
        );

        let config_path = self.config_path.clone();
        let event_tx = self.event_tx.clone();
        let last_reload = self.last_reload.clone();
        let debounce_duration = self.debounce_duration;
        let validate = self.validate_before_reload;
        let config_arc = self.config.clone();
        let last_good = self.last_good_config.clone();

        // Capture the tokio runtime handle before entering the non-async callback
        let runtime_handle = tokio::runtime::Handle::current();

        // Create file watcher
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                match result {
                    Ok(event) => {
                        if Self::should_reload(&event) {
                            // Use tokio runtime to handle async operations
                            let config_path = config_path.clone();
                            let event_tx = event_tx.clone();
                            let last_reload = last_reload.clone();
                            let config_arc = config_arc.clone();
                            let last_good = last_good.clone();

                            runtime_handle.spawn(async move {
                                // The file is gone (deleted, or mid-way
                                // through an editor's remove-then-create
                                // save). There is nothing to load: report
                                // the removal and keep the current and last
                                // good configuration. A caller that can load
                                // without this file (e.g. a merged loader)
                                // reloads on this event itself.
                                if !config_path.exists() {
                                    info!("Configuration file removed: {}", config_path.display());
                                    let event = ConfigReloadEvent {
                                        path: config_path.clone(),
                                        config: None,
                                        error: Some(format!(
                                            "{CONFIG_FILE_REMOVED}: {}",
                                            config_path.display()
                                        )),
                                        warnings: Vec::new(),
                                        timestamp: Instant::now(),
                                    };
                                    if let Err(e) = event_tx.send(event) {
                                        debug!("No receivers for config removal event: {}", e);
                                    }
                                    return;
                                }

                                // Debounce check
                                {
                                    let last = last_reload.read().await;
                                    if let Some(last_time) = *last {
                                        if last_time.elapsed() < debounce_duration {
                                            debug!("Skipping reload due to debounce");
                                            return;
                                        }
                                    }
                                }

                                // Perform reload
                                match Self::reload_config(&config_path, validate).await {
                                    Ok((new_config, warnings)) => {
                                        info!("Configuration reloaded successfully");
                                        if !warnings.is_empty() {
                                            for w in &warnings {
                                                tracing::warn!("Config reload warning: {}", w);
                                            }
                                        }

                                        // Update configuration
                                        {
                                            let mut config = config_arc.write().await;
                                            *config = new_config.clone();
                                        }

                                        // Update last good config
                                        {
                                            let mut last = last_good.write().await;
                                            *last = Some(new_config.clone());
                                        }

                                        // Update last reload time
                                        {
                                            let mut last = last_reload.write().await;
                                            *last = Some(Instant::now());
                                        }

                                        // Send reload event
                                        let event = ConfigReloadEvent {
                                            path: config_path.clone(),
                                            config: Some(new_config),
                                            error: None,
                                            warnings,
                                            timestamp: Instant::now(),
                                        };

                                        if let Err(e) = event_tx.send(event) {
                                            debug!("No receivers for config reload event: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to reload configuration: {}", e);

                                        // Send error event
                                        let event = ConfigReloadEvent {
                                            path: config_path.clone(),
                                            config: None,
                                            error: Some(e.to_string()),
                                            warnings: Vec::new(),
                                            timestamp: Instant::now(),
                                        };

                                        if let Err(e) = event_tx.send(event) {
                                            debug!("No receivers for config error event: {}", e);
                                        }
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => {
                        error!("File watch error: {}", e);
                    }
                }
            },
            notify::Config::default().with_poll_interval(Duration::from_secs(2)),
        )?;

        // Watch the configuration file (when it exists right now; the
        // parent watch below sees it being created).
        if self.config_path.exists() {
            watcher.watch(&self.config_path, RecursiveMode::NonRecursive)?;
        }

        // Also watch parent directory for file replacements (common with editors)
        if let Some(parent) = self.config_path.parent() {
            watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .context("Failed to watch parent directory")?;
        }

        self.watcher = Some(watcher);

        info!("Configuration hot reload started");
        Ok(())
    }

    /// Also watch `path` (non-recursively) with the started watcher. A change
    /// there triggers the same reload of the configured path. Used for the
    /// directory holding a symlinked config file's current target.
    pub fn watch_also(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let watcher = self
            .watcher
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("configuration watcher not started"))?;
        watcher
            .watch(path.as_ref(), RecursiveMode::NonRecursive)
            .with_context(|| format!("Failed to watch {}", path.as_ref().display()))?;
        Ok(())
    }

    /// Stop watching a path added with [`watch_also`](Self::watch_also).
    pub fn unwatch_also(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let watcher = self
            .watcher
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("configuration watcher not started"))?;
        watcher
            .unwatch(path.as_ref())
            .with_context(|| format!("Failed to unwatch {}", path.as_ref().display()))?;
        Ok(())
    }

    /// Stop watching for configuration changes
    pub fn stop_watching(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            drop(watcher);
            info!("Configuration hot reload stopped");
        }
    }

    /// Get current configuration
    pub async fn get_config(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Get last known good configuration
    pub async fn get_last_good_config(&self) -> Option<Config> {
        self.last_good_config.read().await.clone()
    }

    /// Restore last known good configuration
    pub async fn restore_last_good(&self) -> Result<()> {
        let last_good = self.last_good_config.read().await.clone();

        if let Some(good_config) = last_good {
            let mut config = self.config.write().await;
            *config = good_config.clone();

            info!("Restored last known good configuration");

            // Send restore event
            let event = ConfigReloadEvent {
                path: self.config_path.clone(),
                config: Some(good_config),
                error: None,
                warnings: Vec::new(),
                timestamp: Instant::now(),
            };

            if let Err(e) = self.event_tx.send(event) {
                debug!("No receivers for config restore event: {}", e);
            }

            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "No last known good configuration available"
            ))
        }
    }

    /// Set debounce duration
    pub fn set_debounce_duration(&mut self, duration: Duration) {
        self.debounce_duration = duration;
    }

    /// Enable/disable validation before reload
    pub fn set_validation(&mut self, enabled: bool) {
        self.validate_before_reload = enabled;
    }

    /// Check if an event should trigger reload. A removal counts: deleting
    /// the file is a configuration change too (PAN-103). The handler then
    /// reports the removal instead of loading (see `start_watching`).
    fn should_reload(event: &Event) -> bool {
        matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        )
    }

    /// Reload configuration from file, along with any non-fatal load
    /// warnings (e.g. an unrecognized top-level config section) — the same
    /// warnings the startup path (`Config::load_default_with_warnings`)
    /// surfaces, so a live reload doesn't silently drop them.
    async fn reload_config(path: &Path, validate: bool) -> Result<(Config, Vec<String>)> {
        debug!("Reloading configuration from: {}", path.display());

        // Load new configuration
        let (new_config, warnings) = Config::load_from_file_with_warnings(path)
            .context("Failed to load configuration file")?;

        // Validate if requested
        if validate {
            new_config
                .validate()
                .context("Configuration validation failed")?;
        }

        Ok((new_config, warnings))
    }
}

impl Drop for ConfigHotReload {
    fn drop(&mut self) {
        self.stop_watching();
    }
}

/// Configuration change handler trait
pub trait ConfigChangeHandler: Send + Sync {
    /// Handle configuration change
    fn handle_config_change(&mut self, event: ConfigReloadEvent);
}

/// Configuration hot reload with handlers
pub struct ConfigHotReloadWithHandlers {
    /// Base hot reload manager
    manager: ConfigHotReload,
    /// Change handlers
    handlers: Vec<Box<dyn ConfigChangeHandler>>,
    /// Event receiver
    event_rx: broadcast::Receiver<ConfigReloadEvent>,
}

impl ConfigHotReloadWithHandlers {
    /// Create new hot reload manager with handlers
    pub fn new(config: Config, config_path: impl AsRef<Path>) -> Result<Self> {
        let (manager, event_rx) = ConfigHotReload::new(config, config_path)?;

        Ok(Self {
            manager,
            handlers: Vec::new(),
            event_rx,
        })
    }

    /// Add a change handler
    pub fn add_handler(&mut self, handler: Box<dyn ConfigChangeHandler>) {
        self.handlers.push(handler);
    }

    /// Start watching with handler processing
    pub async fn start(&mut self) -> Result<()> {
        self.manager.start_watching().await?;

        // Start handler processing task
        let mut event_rx = self.manager.event_tx.subscribe();
        let handlers = std::mem::take(&mut self.handlers);
        let handlers = Arc::new(RwLock::new(handlers));

        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                let mut handlers = handlers.write().await;
                for handler in handlers.iter_mut() {
                    handler.handle_config_change(event.clone());
                }
            }
        });

        Ok(())
    }

    /// Stop watching
    pub fn stop(&mut self) {
        self.manager.stop_watching();
    }

    /// Get current configuration
    pub async fn get_config(&self) -> Config {
        self.manager.get_config().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_hot_reload_creation() {
        let config = Config::default();
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        // Save config to temp file
        config.save_to_file(path).unwrap();

        let result = ConfigHotReload::new(config, path);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_config_reload() {
        let mut config = Config::default();
        config.station.callsign = "TEST1".to_string();

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_path_buf();

        // Save initial config
        config.save_to_file(&path).unwrap();

        let (mut manager, mut event_rx) = ConfigHotReload::new(config, &path).unwrap();
        manager.set_debounce_duration(Duration::from_millis(100));
        manager.start_watching().await.unwrap();

        // Modify config
        let mut new_config = Config::default();
        new_config.station.callsign = "TEST2".to_string();
        new_config.save_to_file(&path).unwrap();

        // Wait for reload
        sleep(Duration::from_millis(200)).await;

        // Check if event was received
        if let Ok(event) = event_rx.try_recv() {
            assert!(event.config.is_some());
            assert_eq!(event.config.unwrap().station.callsign, "TEST2");
        }

        // Check current config
        let current = manager.get_config().await;
        assert_eq!(current.station.callsign, "TEST2");
    }

    #[tokio::test]
    async fn reload_config_surfaces_unknown_section_warnings() {
        // Regression test: reload_config used to call Config::load_from_file
        // (discarding the loader's warnings) instead of the _with_warnings
        // variant, so an unrecognized section on a live reload never
        // reached the caller — only a startup load's warnings did.
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();
        std::fs::write(
            path,
            r#"
[station]
callsign = "N0CALL"

[autonomous_operator]
enabled = true
"#,
        )
        .unwrap();

        let (config, warnings) = ConfigHotReload::reload_config(path, false).await.unwrap();
        assert_eq!(config.station.callsign, "N0CALL");
        assert_eq!(
            warnings.len(),
            1,
            "unknown [autonomous_operator] section must warn: {warnings:?}"
        );
        assert!(warnings[0].contains("autonomous_operator"));
    }

    /// PAN-103 (Codex P1 on #436): `should_reload` discarded every `Remove`,
    /// so a backend that reports a deletion only as a removal (Windows'
    /// `ReadDirectoryChangesW`, or inotify's parent-directory watch) never
    /// told the consumer the file was deleted. Synthetic events: macOS
    /// FSEvents attaches sticky modify flags to a deletion and Linux sends
    /// `IN_ATTRIB` to the file watch, so a real deletion there does not
    /// isolate the removal kind.
    #[test]
    fn should_reload_counts_a_removal() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        let path = PathBuf::from("/x/pancetta.toml");
        for kind in [
            EventKind::Remove(RemoveKind::File),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let event = Event::new(kind).add_path(path.clone());
            assert!(ConfigHotReload::should_reload(&event), "{kind:?}");
        }
        assert!(!ConfigHotReload::should_reload(&Event::new(
            EventKind::Access(notify::event::AccessKind::Any)
        )));
    }

    /// PAN-103 (Codex P1 on #436): deleting the watched file used to produce
    /// no event at all (`should_reload` discarded every `Remove`), so a
    /// consumer never learned the configuration changed. It now gets a
    /// removal event, and the last good configuration is kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_the_watched_file_emits_a_removal_event_and_keeps_last_good() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pancetta.toml");
        let mut config = Config::default();
        config.station.callsign = "TEST1".to_string();
        config.save_to_file(&path).unwrap();

        let (mut manager, mut event_rx) = ConfigHotReload::new(config, &path).unwrap();
        manager.start_watching().await.unwrap();
        // macOS FSEvents registers the watch asynchronously and can deliver a
        // late event for the initial write: let it settle, then drain.
        sleep(Duration::from_millis(500)).await;
        while event_rx.try_recv().is_ok() {}

        std::fs::remove_file(&path).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = event_rx.recv().await.expect("channel open");
                if event.config.is_none() {
                    return event;
                }
            }
        })
        .await
        .expect("a removal event within 10 s");
        let error = event.error.expect("removal reported as an error");
        assert!(error.starts_with(CONFIG_FILE_REMOVED), "{error}");
        assert_eq!(manager.get_config().await.station.callsign, "TEST1");
        assert_eq!(
            manager
                .get_last_good_config()
                .await
                .unwrap()
                .station
                .callsign,
            "TEST1"
        );
    }

    #[tokio::test]
    async fn test_last_good_config() {
        let config = Config::default();
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        config.save_to_file(path).unwrap();

        let (manager, _) = ConfigHotReload::new(config.clone(), path).unwrap();

        let last_good = manager.get_last_good_config().await;
        assert!(last_good.is_some());
        assert_eq!(last_good.unwrap().station.callsign, config.station.callsign);
    }
}
