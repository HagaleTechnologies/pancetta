//! Configuration loader module
//!
//! This module handles hierarchical configuration loading, hot-reload capability,
//! file watching, and configuration merging from multiple sources.

use crate::{Config, ConfigError, ConfigResult};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;
use tracing::{debug, error, info, warn};

/// Configuration loader with hierarchical loading and hot-reload support
pub struct ConfigLoader {
    /// Search paths for configuration files
    search_paths: Vec<PathBuf>,

    /// Current configuration
    current_config: Arc<RwLock<Config>>,

    /// File watcher for hot-reload
    watcher: Option<RecommendedWatcher>,

    /// Watched files and their last modification times
    watched_files: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,

    /// Configuration sources in priority order
    sources: Vec<ConfigSource>,

    /// Hot-reload callback
    // rationale: a boxed `Fn` behind Arc<Mutex<Option<..>>> is the natural shape for
    // a shared, optional, hot-swappable callback; a type alias adds indirection.
    #[allow(clippy::type_complexity)]
    reload_callback: Arc<Mutex<Option<Box<dyn Fn(&Config) + Send + Sync>>>>,

    /// Cache for parsed configurations
    config_cache: Arc<Mutex<HashMap<PathBuf, (SystemTime, Config)>>>,

    /// Human-readable warnings accumulated during the last `load()` (e.g. a
    /// config file that failed to parse and was skipped). Surfaced to the
    /// operator (console + TUI) so a silent revert-to-defaults is visible.
    load_warnings: Arc<Mutex<Vec<String>>>,
}

/// Configuration source definition
#[derive(Debug, Clone)]
pub struct ConfigSource {
    /// Source name for debugging
    pub name: String,

    /// Source path
    pub path: PathBuf,

    /// Source priority (higher = more important)
    pub priority: u8,

    /// Whether this source is required
    pub required: bool,

    /// Source type
    pub source_type: SourceType,
}

/// Configuration source types
#[derive(Debug, Clone)]
pub enum SourceType {
    /// TOML configuration file
    Toml,

    /// JSON configuration file
    Json,

    /// Environment variables
    Environment,

    /// Command line arguments
    CommandLine,

    /// Remote configuration (URL)
    Remote(String),
}

/// Configuration manager for hot-reload and watch functionality
pub struct ConfigManager {
    /// Configuration loader
    loader: ConfigLoader,

    /// Watch thread handle
    _watch_handle: Option<std::thread::JoinHandle<()>>,

    /// Manager state
    state: Arc<RwLock<ManagerState>>,
}

/// Manager state
#[derive(Debug)]
pub struct ManagerState {
    /// Whether watching is active
    watching: bool,

    /// Last reload time
    last_reload: SystemTime,

    /// Number of successful reloads
    reload_count: u64,

    /// Last error
    last_error: Option<String>,
}

impl ConfigLoader {
    /// Create a new configuration loader with default search paths
    pub fn new() -> ConfigResult<Self> {
        let mut search_paths = Vec::new();

        // Add standard search paths
        search_paths.extend(Self::get_default_search_paths()?);

        Ok(Self {
            search_paths,
            current_config: Arc::new(RwLock::new(Config::default())),
            watcher: None,
            watched_files: Arc::new(Mutex::new(HashMap::new())),
            sources: Vec::new(),
            reload_callback: Arc::new(Mutex::new(None)),
            config_cache: Arc::new(Mutex::new(HashMap::new())),
            load_warnings: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Create a configuration loader with custom search paths
    pub fn with_search_paths(paths: Vec<PathBuf>) -> ConfigResult<Self> {
        Ok(Self {
            search_paths: paths,
            current_config: Arc::new(RwLock::new(Config::default())),
            watcher: None,
            watched_files: Arc::new(Mutex::new(HashMap::new())),
            sources: Vec::new(),
            reload_callback: Arc::new(Mutex::new(None)),
            config_cache: Arc::new(Mutex::new(HashMap::new())),
            load_warnings: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Get default search paths for configuration files
    fn get_default_search_paths() -> ConfigResult<Vec<PathBuf>> {
        let mut paths = Vec::new();

        // Current directory
        if let Ok(current_dir) = std::env::current_dir() {
            paths.push(current_dir);
        }

        // User configuration directory
        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("pancetta"));
        }

        // System configuration directory
        #[cfg(unix)]
        {
            paths.push(PathBuf::from("/etc/pancetta"));
            paths.push(PathBuf::from("/usr/local/etc/pancetta"));
        }

        #[cfg(windows)]
        {
            if let Some(program_data) = dirs::data_dir() {
                paths.push(program_data.join("Pancetta"));
            }
        }

        // Home directory
        if let Some(home_dir) = dirs::home_dir() {
            paths.push(home_dir.join(".pancetta"));
            paths.push(home_dir.join(".config").join("pancetta"));
        }

        Ok(paths)
    }

    /// Add a configuration source
    pub fn add_source(&mut self, source: ConfigSource) {
        self.sources.push(source);
        // Sort sources by priority (highest first)
        self.sources.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    /// Load configuration from all sources using hierarchical merging
    pub fn load(&self) -> ConfigResult<Config> {
        debug!("Loading configuration from all sources");

        // Fresh warning slate for this load.
        if let Ok(mut w) = self.load_warnings.lock() {
            w.clear();
        }

        // Start with default configuration
        let mut config = Config::default();

        // If no sources are configured, use default search
        let sources = if self.sources.is_empty() {
            self.discover_sources()?
        } else {
            self.sources.clone()
        };

        // Load and merge configurations in priority order (lowest to highest)
        let mut sorted_sources = sources.clone();
        sorted_sources.sort_by(|a, b| a.priority.cmp(&b.priority));

        for source in &sorted_sources {
            match self.load_source(source) {
                Ok(source_config) => {
                    debug!("Loaded configuration from source: {}", source.name);
                    config.merge_with(source_config);
                }
                Err(e) => {
                    if source.required {
                        error!(
                            "Failed to load required configuration source '{}': {}",
                            source.name, e
                        );
                        return Err(e);
                    } else if matches!(
                        e,
                        ConfigError::FileNotFound(_) | ConfigError::SourceSkipped(_)
                    ) {
                        // A missing optional file, or an intentionally-inactive
                        // source (CLI handled in main.rs; no PANCETTA_* env vars),
                        // is normal — stay quiet. These are NOT parse failures and
                        // must not raise the operator-facing "failed to parse"
                        // warning below.
                        debug!(
                            "Skipped optional configuration source '{}': {}",
                            source.name, e
                        );
                    } else {
                        // The file EXISTS but failed to parse/validate. This is the
                        // silent-revert-to-defaults trap: warn loudly AND record a
                        // human-readable warning the caller can surface (console + TUI).
                        warn!(
                            "Config source '{}' failed to load — IGNORING it and using \
                             defaults for its settings: {}",
                            source.name, e
                        );
                        if let Ok(mut wlist) = self.load_warnings.lock() {
                            wlist.push(format!(
                                "Config '{}' failed to parse — using defaults for it ({})",
                                source.name, e
                            ));
                        }
                    }
                }
            }
        }

        // Validate the final configuration
        config.validate()?;

        // Update metadata
        if let Some(ref mut metadata) = config.metadata {
            metadata.last_modified = Some(chrono::Utc::now());
            metadata.sources = sources.iter().map(|s| s.path.clone()).collect();
        }

        // Store current configuration
        {
            let mut current = self.current_config.write().unwrap();
            *current = config.clone();
        }

        info!(
            "Configuration loaded successfully from {} sources",
            sources.len()
        );
        Ok(config)
    }

    /// Warnings accumulated during the last [`load`](Self::load) call (e.g. a
    /// config file that existed but failed to parse and was skipped, so its
    /// settings silently reverted to defaults). Empty on a clean load.
    pub fn load_warnings(&self) -> Vec<String> {
        self.load_warnings
            .lock()
            .map(|w| w.clone())
            .unwrap_or_default()
    }

    /// Load configuration from a specific file
    pub fn load_from_file<P: AsRef<Path>>(&self, path: P) -> ConfigResult<Config> {
        let path = path.as_ref();
        debug!("Loading configuration from file: {}", path.display());

        // Check cache first
        if let Some((cached_time, cached_config)) = self.get_cached_config(path)? {
            if let Ok(modified) = fs::metadata(path)?.modified() {
                if modified < cached_time {
                    debug!("Using cached configuration for: {}", path.display());
                    return Ok(cached_config);
                }
            }
        }

        // Load and parse file
        let content = fs::read_to_string(path).map_err(ConfigError::Io)?;

        let config = match path.extension().and_then(|ext| ext.to_str()) {
            Some("toml") => self.parse_toml(&content)?,
            Some("json") => self.parse_json(&content)?,
            _ => {
                // Try to determine format from content
                if content.trim_start().starts_with('{') {
                    self.parse_json(&content)?
                } else {
                    self.parse_toml(&content)?
                }
            }
        };

        // Cache the result
        self.cache_config(path, config.clone())?;

        info!("Configuration loaded from file: {}", path.display());
        Ok(config)
    }

    /// Discover configuration sources in search paths
    fn discover_sources(&self) -> ConfigResult<Vec<ConfigSource>> {
        let mut sources = Vec::new();

        // Default configuration (embedded)
        sources.push(ConfigSource {
            name: "defaults".to_string(),
            path: PathBuf::from("defaults.toml"),
            priority: 0,
            required: false,
            source_type: SourceType::Toml,
        });

        // Search for configuration files in search paths
        for (index, search_path) in self.search_paths.iter().enumerate() {
            let config_files = [
                "pancetta.toml",
                "config.toml",
                "pancetta.json",
                "config.json",
            ];

            for config_file in &config_files {
                let config_path = search_path.join(config_file);
                if config_path.exists() {
                    let source_type = if config_file.ends_with(".json") {
                        SourceType::Json
                    } else {
                        SourceType::Toml
                    };

                    sources.push(ConfigSource {
                        name: format!("file:{}", config_path.display()),
                        path: config_path,
                        priority: (index + 1) as u8,
                        required: false,
                        source_type,
                    });

                    break; // Use first config file found in each path
                }
            }
        }

        // Environment variables
        sources.push(ConfigSource {
            name: "environment".to_string(),
            path: PathBuf::from("env"),
            priority: 100,
            required: false,
            source_type: SourceType::Environment,
        });

        // Command line arguments (if clap feature is enabled)
        #[cfg(feature = "cli")]
        {
            sources.push(ConfigSource {
                name: "command_line".to_string(),
                path: PathBuf::from("cli"),
                priority: 200,
                required: false,
                source_type: SourceType::CommandLine,
            });
        }

        debug!("Discovered {} configuration sources", sources.len());
        Ok(sources)
    }

    /// Load configuration from a specific source
    fn load_source(&self, source: &ConfigSource) -> ConfigResult<Config> {
        match &source.source_type {
            SourceType::Toml => {
                if source.path.exists() {
                    self.load_from_file(&source.path)
                } else if source.path.file_name() == Some(std::ffi::OsStr::new("defaults.toml")) {
                    // Load embedded defaults
                    self.load_embedded_defaults()
                } else {
                    Err(ConfigError::FileNotFound(source.path.clone()))
                }
            }
            SourceType::Json => {
                if source.path.exists() {
                    self.load_from_file(&source.path)
                } else {
                    Err(ConfigError::FileNotFound(source.path.clone()))
                }
            }
            SourceType::Environment => self.load_from_environment(),
            SourceType::CommandLine => self.load_from_command_line(),
            SourceType::Remote(url) => self.load_from_remote(url),
        }
    }

    /// Load embedded default configuration
    fn load_embedded_defaults(&self) -> ConfigResult<Config> {
        // This would typically load from an embedded resource
        // For now, return the default configuration
        debug!("Loading embedded default configuration");
        Ok(Config::default())
    }

    /// Load configuration from environment variables
    fn load_from_environment(&self) -> ConfigResult<Config> {
        debug!("Loading configuration from environment variables");

        // Start with empty-ish defaults so unset env vars don't overwrite
        // file-loaded values during merge. Only fields explicitly set via
        // PANCETTA_* env vars should appear in the returned config.
        let mut config = Config::default();
        let mut any_set = false;

        // Use empty strings / zeros for fields not set via env vars so
        // merge_with's "skip empty" logic leaves file values intact.
        config.station.callsign = String::new();
        config.station.grid_square = String::new();
        config.station.power_watts = 0;
        config.station.qth = String::new();
        config.station.dxcc_entity = 0;
        config.station.itu_zone = 0;
        config.station.cq_zone = 0;
        config.rig.interface.port = String::new();
        config.rig.interface.baud_rate = 0;
        config.audio.input_device = String::new();
        config.audio.output_device = String::new();
        config.audio.sample_rate = 0;
        config.ui.theme = String::new();

        // Map environment variables to configuration fields
        if let Ok(callsign) = std::env::var("PANCETTA_CALLSIGN") {
            config.station.callsign = callsign;
            any_set = true;
        }

        if let Ok(grid) = std::env::var("PANCETTA_GRID_SQUARE") {
            config.station.grid_square = grid;
            any_set = true;
        }

        if let Ok(power) = std::env::var("PANCETTA_POWER_WATTS") {
            if let Ok(power_val) = power.parse::<u32>() {
                config.station.power_watts = power_val;
                any_set = true;
            }
        }

        if let Ok(cat_port) = std::env::var("PANCETTA_CAT_PORT") {
            config.rig.interface.port = cat_port;
            any_set = true;
        }

        if let Ok(cat_baud) = std::env::var("PANCETTA_CAT_BAUD") {
            if let Ok(baud_val) = cat_baud.parse::<u32>() {
                config.rig.interface.baud_rate = baud_val;
                any_set = true;
            }
        }

        if let Ok(audio_input) = std::env::var("PANCETTA_AUDIO_INPUT") {
            config.audio.input_device = audio_input;
            any_set = true;
        }

        if let Ok(audio_output) = std::env::var("PANCETTA_AUDIO_OUTPUT") {
            config.audio.output_device = audio_output;
            any_set = true;
        }

        if let Ok(sample_rate) = std::env::var("PANCETTA_SAMPLE_RATE") {
            if let Ok(rate_val) = sample_rate.parse::<u32>() {
                config.audio.sample_rate = rate_val;
                any_set = true;
            }
        }

        if let Ok(theme) = std::env::var("PANCETTA_THEME") {
            config.ui.theme = theme;
            any_set = true;
        }

        if any_set {
            debug!("Loaded configuration from environment variables");
            Ok(config)
        } else {
            // No PANCETTA_* env vars set — skip this source so defaults
            // don't overwrite file-loaded values during merge. This is a normal
            // intentional skip, not a parse failure (see SourceSkipped handling
            // in `load`).
            Err(ConfigError::SourceSkipped(
                "no PANCETTA_* environment variables set".to_string(),
            ))
        }
    }

    /// Load configuration from command line arguments
    #[cfg(feature = "cli")]
    fn load_from_command_line(&self) -> ConfigResult<Config> {
        // CLI argument parsing is handled by main.rs via clap, not here.
        // Skip this source so defaults don't overwrite file-loaded values.
        // Intentional skip, not a parse failure (see SourceSkipped in `load`).
        Err(ConfigError::SourceSkipped(
            "CLI overrides are applied in main.rs, not the config loader".to_string(),
        ))
    }

    #[cfg(not(feature = "cli"))]
    fn load_from_command_line(&self) -> ConfigResult<Config> {
        // No CLI overrides available — skip this source so defaults
        // don't overwrite file-loaded values during merge. Intentional skip,
        // not a parse failure (see SourceSkipped in `load`).
        Err(ConfigError::SourceSkipped(
            "CLI config source not enabled".to_string(),
        ))
    }

    /// Load configuration from remote URL
    fn load_from_remote(&self, url: &str) -> ConfigResult<Config> {
        // Validate URL format (basic check: must start with http:// or https://)
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ConfigError::Validation(format!(
                "Invalid remote URL format: '{}'. URL must start with http:// or https://. \
                 As a workaround, download the config file manually and place it at ~/.pancetta/pancetta.toml",
                url
            )));
        }

        Err(ConfigError::Validation(format!(
            "Remote configuration loading is not yet implemented for: {}. \
             Download the config file manually and place it at ~/.pancetta/pancetta.toml",
            url
        )))
    }

    /// Parse TOML configuration
    fn parse_toml(&self, content: &str) -> ConfigResult<Config> {
        // Tilde-only (`~`) expansion — deliberately NOT `shellexpand::full`,
        // which would also expand `$VAR` env references in the raw config text
        // and risk leaking secrets (e.g. tokens) into the parsed config and
        // downstream logs/errors. See security fix I-7.
        let expanded_content = shellexpand::tilde(content);

        toml::from_str(&expanded_content).map_err(ConfigError::Toml)
    }

    /// Parse JSON configuration
    fn parse_json(&self, content: &str) -> ConfigResult<Config> {
        // Tilde-only (`~`) expansion — see `parse_toml` and security fix I-7.
        let expanded_content = shellexpand::tilde(content);

        serde_json::from_str(&expanded_content).map_err(ConfigError::Json)
    }

    /// Get cached configuration for a file
    fn get_cached_config(&self, path: &Path) -> ConfigResult<Option<(SystemTime, Config)>> {
        let cache = self.config_cache.lock().unwrap();
        Ok(cache.get(path).cloned())
    }

    /// Cache configuration for a file
    fn cache_config(&self, path: &Path, config: Config) -> ConfigResult<()> {
        let modified = fs::metadata(path)?.modified()?;
        let mut cache = self.config_cache.lock().unwrap();
        cache.insert(path.to_path_buf(), (modified, config));
        Ok(())
    }

    /// Set up file watching for hot-reload
    pub fn setup_watching(&mut self) -> ConfigResult<()> {
        let (tx, rx) = std::sync::mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| match res {
                Ok(event) => {
                    if let Err(e) = tx.send(event) {
                        error!("Failed to send file watch event: {}", e);
                    }
                }
                Err(e) => {
                    error!("File watch error: {}", e);
                }
            },
            notify::Config::default(),
        )?;

        // Watch all configuration files
        for source in &self.sources {
            if matches!(source.source_type, SourceType::Toml | SourceType::Json)
                && source.path.exists()
            {
                if let Some(parent) = source.path.parent() {
                    watcher.watch(parent, RecursiveMode::NonRecursive)?;
                    debug!("Watching directory: {}", parent.display());
                }
            }
        }

        // Also watch search paths
        for path in &self.search_paths {
            if path.exists() {
                watcher.watch(path, RecursiveMode::NonRecursive)?;
                debug!("Watching directory: {}", path.display());
            }
        }

        self.watcher = Some(watcher);

        // Start watching thread
        let current_config = Arc::clone(&self.current_config);
        let reload_callback = Arc::clone(&self.reload_callback);
        let watched_files = Arc::clone(&self.watched_files);
        let config_cache = Arc::clone(&self.config_cache);

        std::thread::spawn(move || {
            debug!("File watcher thread started");

            while let Ok(event) = rx.recv() {
                if let EventKind::Modify(_) = event.kind {
                    for path in event.paths {
                        if Self::is_config_file(&path) {
                            debug!("Configuration file changed: {}", path.display());

                            // Check if file was actually modified
                            if let Ok(metadata) = fs::metadata(&path) {
                                if let Ok(modified) = metadata.modified() {
                                    let mut files = watched_files.lock().unwrap();
                                    if let Some(&last_modified) = files.get(&path) {
                                        if modified <= last_modified {
                                            continue; // File not actually changed
                                        }
                                    }
                                    files.insert(path.clone(), modified);
                                }
                            }

                            // Clear cache for this file
                            {
                                let mut cache = config_cache.lock().unwrap();
                                cache.remove(&path);
                            }

                            // Reload config from disk BEFORE invoking the callback.
                            // The old code passed the pre-reload config to listeners.
                            let new_config = match fs::read_to_string(&path) {
                                Ok(content) => {
                                    let parsed = if path.extension().and_then(|e| e.to_str())
                                        == Some("json")
                                    {
                                        serde_json::from_str::<Config>(&content).ok()
                                    } else {
                                        toml::from_str::<Config>(&content).ok()
                                    };
                                    parsed
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to re-read config file {}: {}",
                                        path.display(),
                                        e
                                    );
                                    None
                                }
                            };

                            if let Some(config) = new_config {
                                // Update current_config with the freshly-loaded value
                                if let Ok(mut config_guard) = current_config.write() {
                                    *config_guard = config;
                                }
                            }

                            // Trigger reload callback with the now-updated config
                            if let Ok(callback_guard) = reload_callback.lock() {
                                if let Some(ref callback) = *callback_guard {
                                    if let Ok(config_guard) = current_config.read() {
                                        callback(&config_guard);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            debug!("File watcher thread ended");
        });

        info!("File watching enabled for configuration hot-reload");
        Ok(())
    }

    /// Check if a file is a configuration file
    fn is_config_file(path: &Path) -> bool {
        if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
            matches!(extension, "toml" | "json")
        } else {
            false
        }
    }

    /// Set callback for configuration reload events
    pub fn set_reload_callback<F>(&self, callback: F)
    where
        F: Fn(&Config) + Send + Sync + 'static,
    {
        let mut cb = self.reload_callback.lock().unwrap();
        *cb = Some(Box::new(callback));
    }

    /// Get current configuration
    pub fn get_current_config(&self) -> Config {
        let config = self.current_config.read().unwrap();
        config.clone()
    }

    /// Reload configuration manually
    pub fn reload(&self) -> ConfigResult<Config> {
        debug!("Manually reloading configuration");

        // Clear cache
        {
            let mut cache = self.config_cache.lock().unwrap();
            cache.clear();
        }

        // Reload configuration
        let config = self.load()?;

        // Trigger callback
        if let Ok(callback_guard) = self.reload_callback.lock() {
            if let Some(ref callback) = *callback_guard {
                callback(&config);
            }
        }

        Ok(config)
    }
}

impl ConfigManager {
    /// Create a new configuration manager
    pub fn new() -> ConfigResult<Self> {
        let loader = ConfigLoader::new()?;

        let state = Arc::new(RwLock::new(ManagerState {
            watching: false,
            last_reload: SystemTime::now(),
            reload_count: 0,
            last_error: None,
        }));

        Ok(Self {
            loader,
            _watch_handle: None,
            state,
        })
    }

    /// Start watching for configuration changes
    pub fn start_watching(&mut self) -> ConfigResult<()> {
        info!("Starting configuration watching");

        self.loader.setup_watching()?;
        let loader = &mut self.loader;

        // Set up reload callback
        let state = Arc::clone(&self.state);
        loader.set_reload_callback(move |_config| {
            let mut s = state.write().unwrap();
            s.last_reload = SystemTime::now();
            s.reload_count += 1;
            info!("Configuration reloaded (count: {})", s.reload_count);
        });

        {
            let mut state = self.state.write().unwrap();
            state.watching = true;
        }

        Ok(())
    }

    /// Stop watching for configuration changes
    pub fn stop_watching(&mut self) {
        info!("Stopping configuration watching");

        {
            let mut state = self.state.write().unwrap();
            state.watching = false;
        }

        // The watcher will be dropped when the loader is replaced
        self.loader = ConfigLoader::new().unwrap();
    }

    /// Get current manager state
    pub fn get_state(&self) -> ManagerState {
        let state = self.state.read().unwrap();
        ManagerState {
            watching: state.watching,
            last_reload: state.last_reload,
            reload_count: state.reload_count,
            last_error: state.last_error.clone(),
        }
    }

    /// Load configuration
    pub fn load_config(&self) -> ConfigResult<Config> {
        match self.loader.load() {
            Ok(config) => {
                let mut state = self.state.write().unwrap();
                state.last_error = None;
                Ok(config)
            }
            Err(e) => {
                let mut state = self.state.write().unwrap();
                state.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Get current configuration
    pub fn get_config(&self) -> Config {
        self.loader.get_current_config()
    }

    /// Reload configuration manually
    pub fn reload_config(&self) -> ConfigResult<Config> {
        match self.loader.reload() {
            Ok(config) => {
                let mut state = self.state.write().unwrap();
                state.last_reload = SystemTime::now();
                state.reload_count += 1;
                state.last_error = None;
                Ok(config)
            }
            Err(e) => {
                let mut state = self.state.write().unwrap();
                state.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_config_loader_new() {
        let loader = ConfigLoader::new();
        assert!(loader.is_ok());
    }

    #[test]
    fn test_config_source_creation() {
        let source = ConfigSource {
            name: "test".to_string(),
            path: PathBuf::from("test.toml"),
            priority: 10,
            required: false,
            source_type: SourceType::Toml,
        };

        assert_eq!(source.name, "test");
        assert_eq!(source.priority, 10);
        assert!(!source.required);
    }

    #[test]
    fn test_parse_toml() {
        let loader = ConfigLoader::new().unwrap();
        let mut config = Config::default();
        config.station.callsign = "K1ABC".to_string();
        config.station.grid_square = "FN31pr".to_string();
        config.station.power_watts = 100;
        let toml_content = toml::to_string_pretty(&config).unwrap();

        let parsed = loader.parse_toml(&toml_content).unwrap();
        assert_eq!(parsed.station.callsign, "K1ABC");
        assert_eq!(parsed.station.grid_square, "FN31pr");
        assert_eq!(parsed.station.power_watts, 100);
    }

    #[test]
    fn test_parse_json() {
        let loader = ConfigLoader::new().unwrap();
        let mut config = Config::default();
        config.station.callsign = "K1ABC".to_string();
        config.station.grid_square = "FN31pr".to_string();
        config.station.power_watts = 100;
        let json_content = serde_json::to_string_pretty(&config).unwrap();

        let parsed = loader.parse_json(&json_content).unwrap();
        assert_eq!(parsed.station.callsign, "K1ABC");
        assert_eq!(parsed.station.grid_square, "FN31pr");
        assert_eq!(parsed.station.power_watts, 100);
    }

    #[test]
    fn test_load_from_file() {
        let loader = ConfigLoader::new().unwrap();

        // Create temporary config file with a complete valid config
        let mut config = Config::default();
        config.station.callsign = "K1TEST".to_string();
        config.station.grid_square = "FN42aa".to_string();
        config.station.power_watts = 50;
        let toml_content = toml::to_string_pretty(&config).unwrap();

        let mut temp_file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp_file, toml_content.as_bytes()).unwrap();

        let loaded = loader.load_from_file(temp_file.path()).unwrap();
        assert_eq!(loaded.station.callsign, "K1TEST");
        assert_eq!(loaded.station.grid_square, "FN42aa");
        assert_eq!(loaded.station.power_watts, 50);
    }

    #[test]
    fn test_environment_variables() {
        let loader = ConfigLoader::new().unwrap();

        // Set environment variables
        std::env::set_var("PANCETTA_CALLSIGN", "K1ENV");
        std::env::set_var("PANCETTA_POWER_WATTS", "75");

        let config = loader.load_from_environment().unwrap();
        assert_eq!(config.station.callsign, "K1ENV");
        assert_eq!(config.station.power_watts, 75);

        // Clean up
        std::env::remove_var("PANCETTA_CALLSIGN");
        std::env::remove_var("PANCETTA_POWER_WATTS");
    }

    #[test]
    fn test_config_manager() {
        let manager = ConfigManager::new().unwrap();
        let state = manager.get_state();
        assert!(!state.watching);
        assert_eq!(state.reload_count, 0);
    }

    /// The intentionally-inactive sources (command-line — handled in main.rs;
    /// environment — when no PANCETTA_* vars are set) must be skipped QUIETLY.
    /// They previously returned `Validation` and tripped the loader's
    /// "failed to parse — using defaults" warning, which surfaced to the
    /// console + TUI on every launch as a spurious `command_line` error.
    #[test]
    fn skipped_sources_do_not_raise_parse_warnings() {
        let loader = ConfigLoader::new().unwrap();
        // load() may Ok (defaults validate) or surface real file warnings, but
        // it must never warn about the intentionally-skipped sources.
        let _ = loader.load();
        let warnings = loader.load_warnings();
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("command_line") || w.contains("environment")),
            "intentional source skips must not surface as parse warnings: {warnings:?}"
        );
    }

    /// `SourceSkipped` is distinct from a real parse/validation failure so the
    /// loader can branch on it.
    #[test]
    fn source_skipped_is_not_file_not_found_or_validation() {
        let skipped = ConfigError::SourceSkipped("x".into());
        assert!(matches!(skipped, ConfigError::SourceSkipped(_)));
        assert!(!matches!(skipped, ConfigError::Validation(_)));
    }

    #[test]
    fn test_source_priority_sorting() {
        let mut loader = ConfigLoader::new().unwrap();

        // Add sources with different priorities
        loader.add_source(ConfigSource {
            name: "low".to_string(),
            path: PathBuf::from("low.toml"),
            priority: 1,
            required: false,
            source_type: SourceType::Toml,
        });

        loader.add_source(ConfigSource {
            name: "high".to_string(),
            path: PathBuf::from("high.toml"),
            priority: 10,
            required: false,
            source_type: SourceType::Toml,
        });

        loader.add_source(ConfigSource {
            name: "medium".to_string(),
            path: PathBuf::from("medium.toml"),
            priority: 5,
            required: false,
            source_type: SourceType::Toml,
        });

        // Verify sources are sorted by priority (highest first)
        assert_eq!(loader.sources[0].name, "high");
        assert_eq!(loader.sources[1].name, "medium");
        assert_eq!(loader.sources[2].name, "low");
    }

    #[test]
    fn test_shell_expansion() {
        let loader = ConfigLoader::new().unwrap();

        // Start with a valid default config and verify that parse_toml
        // successfully processes path expansion on the TOML content.
        // Note: parse_toml uses tilde-only (`~`) expansion via
        // `shellexpand::tilde`, which operates on the raw TOML text. It
        // expands ONLY a `~` at the very start of the input (never `$VAR`
        // env references — see security fix I-7), so `~` inside quoted
        // string values is left untouched.
        let mut config = Config::default();
        config.audio.recording.directory = "~/Documents/Recordings".to_string();
        let toml_content = toml::to_string_pretty(&config).unwrap();

        let parsed = loader.parse_toml(&toml_content);
        assert!(parsed.is_ok(), "parse_toml failed: {:?}", parsed.err());

        // Verify parse_toml ran path expansion without errors and
        // produced a valid config.
        let parsed = parsed.unwrap();
        assert_eq!(parsed.station.callsign, config.station.callsign);
    }

    #[test]
    fn test_no_env_var_expansion_in_config_values() {
        // Security fix I-7: `$VAR` env references in config values must be
        // passed through LITERALLY and never expanded (the old
        // `shellexpand::full` would have substituted the env value,
        // leaking secrets into the parsed config and downstream logs).
        let loader = ConfigLoader::new().unwrap();

        // Set a distinctive env var that, if expanded, would corrupt the
        // value (and in the real leak, would inject a secret).
        let var_name = "PANCETTA_I7_TEST_SECRET";
        std::env::set_var(var_name, "leaked-secret-value");

        let mut config = Config::default();
        // A config value that textually references the env var.
        config.audio.recording.directory = format!("/recordings/${}", var_name);
        let toml_content = toml::to_string_pretty(&config).unwrap();

        let parsed = loader
            .parse_toml(&toml_content)
            .expect("parse_toml should succeed");

        // The `$VAR` reference must survive verbatim — NOT be expanded to
        // the env value.
        assert_eq!(
            parsed.audio.recording.directory,
            format!("/recordings/${}", var_name),
            "$VAR in config value must be passed through literally (no env-var expansion)"
        );
        assert!(
            !parsed
                .audio
                .recording
                .directory
                .contains("leaked-secret-value"),
            "env-var value leaked into parsed config — secret-leak regression"
        );

        std::env::remove_var(var_name);
    }

    #[test]
    fn test_leading_tilde_still_expands() {
        // The legitimate use case (the original intent of the call) — a
        // leading `~` in the raw input expands to the home directory.
        let loader = ConfigLoader::new().unwrap();
        let home = dirs::home_dir().expect("home dir available in test env");

        // `shellexpand::tilde` only expands a `~` at the very start of the
        // input, so build a raw document that begins with `~`.
        let raw = format!(
            "~/.pancetta\n{}",
            toml::to_string_pretty(&Config::default()).unwrap()
        );
        let expanded = shellexpand::tilde(&raw);
        assert!(
            expanded.starts_with(&home.to_string_lossy().to_string()),
            "leading ~ should expand to home dir, got: {}",
            &expanded[..expanded.len().min(80)]
        );

        // Sanity: parse_toml on a normal default config still succeeds.
        let toml_content = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(loader.parse_toml(&toml_content).is_ok());
    }

    #[test]
    fn test_config_caching() {
        let loader = ConfigLoader::new().unwrap();

        // Create temporary config file with a complete valid config
        let mut config = Config::default();
        config.station.callsign = "K1CACHE".to_string();
        let toml_content = toml::to_string_pretty(&config).unwrap();

        let mut temp_file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp_file, toml_content.as_bytes()).unwrap();

        // First load should cache the result
        let config1 = loader.load_from_file(temp_file.path()).unwrap();
        assert_eq!(config1.station.callsign, "K1CACHE");

        // Second load should use cache (verify by checking cache)
        let cache_result = loader.get_cached_config(temp_file.path()).unwrap();
        assert!(cache_result.is_some());

        let config2 = loader.load_from_file(temp_file.path()).unwrap();
        assert_eq!(config2.station.callsign, "K1CACHE");
    }

    #[test]
    fn test_is_config_file() {
        assert!(ConfigLoader::is_config_file(&PathBuf::from("config.toml")));
        assert!(ConfigLoader::is_config_file(&PathBuf::from(
            "settings.json"
        )));
        assert!(!ConfigLoader::is_config_file(&PathBuf::from("readme.txt")));
        assert!(!ConfigLoader::is_config_file(&PathBuf::from("script.sh")));
    }
}
