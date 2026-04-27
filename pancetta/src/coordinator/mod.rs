//! # Application Coordinator
//!
//! The Application Coordinator is the central orchestrator for the Pancetta application.
//! It manages the lifecycle of all components and coordinates communication between them.
//!
//! ## Architecture
//!
//! The coordinator uses point-to-point crossbeam channels for the core data path:
//!   Audio -> DSP -> FT8 Decoder -> TUI
//!
//! The message bus is retained for control messages and health monitoring.
//!
//! ## WAV Playback Mode
//!
//! When started with `--wav <file>`, the coordinator reads a WAV file, resamples to
//! 12 kHz mono, feeds the samples through the DSP/FT8 pipeline, prints decoded messages,
//! and exits.

mod audio;
mod components;
mod dsp;
mod ft8;
mod hamlib;
mod health;
mod pipeline;
mod shutdown;
mod tui_relay;
mod util;
mod wav_playback;

use anyhow::Result;
use pancetta_config::Config;
use pancetta_ft8::Ft8Decoder;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

use util::resample_linear;

/// Application coordinator that manages all Pancetta components
pub struct ApplicationCoordinator {
    /// Unique instance identifier
    id: uuid::Uuid,

    /// Application configuration (hot-reloadable)
    config: Arc<RwLock<Config>>,

    /// Central message bus for inter-component communication
    message_bus: MessageBus,

    /// Component managers
    ft8_decoder: Option<Ft8Decoder>,

    /// Named component task handles for health monitoring
    named_task_handles: Vec<(ComponentId, JoinHandle<Result<()>>)>,

    /// Component health status map (shared with health monitor task)
    component_status: Arc<RwLock<HashMap<ComponentId, ComponentStatus>>>,

    /// Managed rigctld child process (killed on shutdown)
    #[cfg(feature = "pancetta-hamlib")]
    rigctld_process: Option<std::process::Child>,

    /// Application state
    is_running: Arc<AtomicBool>,
    shutdown_signal: Arc<AtomicBool>,
    startup_time: Instant,

    /// Configuration
    audio_device: Option<String>,
    no_audio: bool,
    headless: bool,
    enable_metrics: bool,
    metrics_port: u16,

    /// WAV file playback path (if set, runs in playback mode)
    wav_path: Option<PathBuf>,

    /// One-shot test transmission. If Some, after startup the coordinator
    /// injects a single TransmitRequest with this message text and shuts
    /// down on TransmitComplete. Used for hardware bench validation.
    test_tx: Option<String>,
    test_tx_offset: f64,

    /// Cached station lookup for priority scoring (shared between QSO and autonomous components).
    cached_lookup: std::sync::Arc<crate::priority_evaluator::CachedStationLookup>,

    /// cqdx.io integration bridge (None = degraded mode).
    cqdx_bridge: Option<std::sync::Arc<crate::cqdx_bridge::CqdxBridge>>,

    /// Sender for waterfall data to the autonomous operator.
    waterfall_to_auto_tx: Option<crossbeam_channel::Sender<Vec<Vec<f32>>>>,

    /// Shared active QSO AP state for FT8 AP3/AP4 decoding.
    /// Updated by the QSO component, read by the FT8 decoder thread.
    active_qso_ap: std::sync::Arc<std::sync::RwLock<Option<pancetta_ft8::QsoAp>>>,

    /// TUI relay OS thread handle (joined on shutdown)
    tui_relay_handle: Option<std::thread::JoinHandle<()>>,

    /// Current operating frequency in Hz, shared across components.
    /// Updated by the hamlib polling task; read by cqdx.io and PSKReporter
    /// to compute absolute RF frequency from audio offsets.
    operating_frequency_hz: Arc<std::sync::atomic::AtomicU64>,

    /// Performance metrics
    message_count: Arc<std::sync::atomic::AtomicU64>,
    last_audio_timestamp: Arc<RwLock<Option<Instant>>>,
    last_decode_timestamp: Arc<RwLock<Option<Instant>>>,
}

#[cfg(feature = "pancetta-hamlib")]
impl Drop for ApplicationCoordinator {
    fn drop(&mut self) {
        if let Some(mut child) = self.rigctld_process.take() {
            eprintln!("Pancetta: killing managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Coordinator configuration
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Component startup timeout
    pub startup_timeout: Duration,

    /// Component shutdown timeout
    pub shutdown_timeout: Duration,

    /// Health check interval
    pub health_check_interval: Duration,

    /// Message bus buffer size
    pub message_buffer_size: usize,

    /// Maximum concurrent tasks
    pub max_concurrent_tasks: usize,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
            health_check_interval: Duration::from_secs(5),
            message_buffer_size: 10000,
            max_concurrent_tasks: 100,
        }
    }
}

/// Component health status (coordinator-level)
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub component_id: ComponentId,
    pub is_healthy: bool,
    pub last_heartbeat: Instant,
    pub error_count: u32,
    pub message_count: u64,
    pub avg_latency_ms: f64,
}

/// State of a component as tracked by the health monitor
#[derive(Debug, Clone, PartialEq)]
pub enum ComponentState {
    /// Component is running normally
    Running,
    /// Component has failed (with error description)
    Failed(String),
    /// Component was never started or is disabled
    NotStarted,
}

impl std::fmt::Display for ComponentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentState::Running => write!(f, "Running"),
            ComponentState::Failed(msg) => write!(f, "Failed: {}", msg),
            ComponentState::NotStarted => write!(f, "NotStarted"),
        }
    }
}

/// Per-component status tracked by the coordinator health monitor
#[derive(Debug, Clone)]
pub struct ComponentStatus {
    pub state: ComponentState,
    pub last_seen: Instant,
    pub error_count: u32,
}

impl ComponentStatus {
    fn new_running() -> Self {
        Self {
            state: ComponentState::Running,
            last_seen: Instant::now(),
            error_count: 0,
        }
    }
}

/// Criticality level of a component -- determines shutdown behavior on failure
#[derive(Debug, Clone, Copy, PartialEq)]
enum ComponentCriticality {
    /// Application can continue without this component
    NonCritical,
    /// Component failure should be logged prominently but app continues
    Important,
}

fn component_criticality(id: ComponentId) -> ComponentCriticality {
    match id {
        ComponentId::Ft8Decoder => ComponentCriticality::Important,
        ComponentId::Audio => ComponentCriticality::NonCritical,
        ComponentId::Dsp => ComponentCriticality::Important,
        _ => ComponentCriticality::NonCritical,
    }
}

/// Human-readable degradation message for a failed component
fn degradation_message(id: ComponentId) -> &'static str {
    match id {
        ComponentId::Audio => "Audio disconnected -- no RX/TX until reconnected",
        ComponentId::Hamlib => "Rig control lost -- PTT safety defaulting to OFF",
        ComponentId::DxCluster => "DX cluster disconnected -- continuing without spots",
        ComponentId::Ft8Decoder => "FT8 decoder crashed -- no decoding until restart",
        ComponentId::Dsp => "DSP pipeline failed -- audio processing halted",
        ComponentId::PskReporter => "PSKReporter upload failed -- spots not being reported",
        ComponentId::Qso => "QSO manager failed -- contact logging unavailable",
        ComponentId::Ft8Transmitter => "FT8 transmitter failed -- TX disabled",
        ComponentId::Autonomous => "Autonomous operator failed -- manual operation only",
        _ => "Component failed",
    }
}

impl ApplicationCoordinator {
    /// Create a new application coordinator
    pub async fn new(
        config: Config,
        audio_device: Option<String>,
        no_audio: bool,
        headless: bool,
        enable_metrics: bool,
        metrics_port: u16,
        wav_path: Option<PathBuf>,
        test_tx: Option<String>,
        test_tx_offset: f64,
        shutdown_signal: Arc<AtomicBool>,
    ) -> Result<Self> {
        let span = span!(Level::INFO, "coordinator_init");
        let _enter = span.enter();

        info!("Initializing Application Coordinator");

        let id = uuid::Uuid::new_v4();
        let startup_time = Instant::now();

        // Create message bus with high-performance configuration
        let coordinator_config = CoordinatorConfig::default();
        let message_bus = MessageBus::new(coordinator_config.message_buffer_size)?;

        // Wrap config in Arc<RwLock> for hot-reloading
        let config = Arc::new(RwLock::new(config));

        let coordinator = Self {
            id,
            config,
            message_bus,
            ft8_decoder: None,
            named_task_handles: Vec::new(),
            component_status: Arc::new(RwLock::new(HashMap::new())),
            is_running: Arc::new(AtomicBool::new(false)),
            shutdown_signal,
            startup_time,
            audio_device,
            no_audio,
            headless,
            enable_metrics,
            metrics_port,
            wav_path,
            test_tx,
            test_tx_offset,
            cached_lookup: std::sync::Arc::new(
                crate::priority_evaluator::CachedStationLookup::new(),
            ),
            cqdx_bridge: None,
            waterfall_to_auto_tx: None,
            active_qso_ap: std::sync::Arc::new(std::sync::RwLock::new(None)),
            tui_relay_handle: None,
            // Initialize to 0 — hamlib will read the actual rig frequency on startup.
            // If hamlib isn't available, the TUI default (14.074) takes over.
            operating_frequency_hz: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "pancetta-hamlib")]
            rigctld_process: None,
            message_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_audio_timestamp: Arc::new(RwLock::new(None)),
            last_decode_timestamp: Arc::new(RwLock::new(None)),
        };

        info!("Application Coordinator initialized with ID: {}", id);
        Ok(coordinator)
    }

    /// Start the application and all components
    pub async fn run(mut self) -> Result<()> {
        let span = span!(Level::INFO, "coordinator_run");
        let _enter = span.enter();

        info!("Starting Pancetta application");
        self.is_running.store(true, Ordering::Release);

        // If WAV playback mode, run the short-circuit pipeline and exit
        if let Some(ref wav_path) = self.wav_path {
            let path = wav_path.clone();
            return self.run_wav_playback(path).await;
        }

        // Initialize metrics if enabled
        if self.enable_metrics {
            self.init_metrics().await?;
        }

        // Start all components in dependency order using point-to-point channels
        self.start_pipeline().await?;

        // Start auxiliary components
        #[cfg(feature = "pancetta-hamlib")]
        self.start_hamlib_component().await?;
        #[cfg(not(feature = "pancetta-hamlib"))]
        warn!("Hamlib feature is disabled -- PTT safety watchdog is not active. Transmit at your own risk.");
        self.start_qso_component().await?;

        // Initialize cqdx.io integration (before autonomous, so rarity/needed data is available)
        {
            let config = self.config.read().await;
            if let Some(bridge) = crate::cqdx_bridge::CqdxBridge::from_config(
                &config.network.cqdx,
                self.cached_lookup.clone(),
            ) {
                drop(config);
                match bridge.startup().await {
                    Ok(()) => {
                        info!("cqdx.io integration initialized");
                        let poller_handle = bridge.spawn_spot_poller(
                            self.shutdown_signal.clone(),
                            self.last_decode_timestamp.clone(),
                            None,
                            None, // TUI tx — set up later in pipeline if available
                        );
                        // Wrap the JoinHandle<()> into JoinHandle<Result<()>> for named_task_handles
                        let wrapped = tokio::spawn(async move {
                            poller_handle
                                .await
                                .map_err(|e| anyhow::anyhow!("cqdx poller join error: {}", e))?;
                            Ok(())
                        });
                        self.named_task_handles
                            .push((ComponentId::DxCluster, wrapped));
                        self.cqdx_bridge = Some(std::sync::Arc::new(bridge));
                    }
                    Err(e) => {
                        warn!("cqdx.io startup failed, running in degraded mode: {}", e);
                    }
                }
            } else {
                drop(config);
                info!("cqdx.io integration not configured, running in degraded mode");
            }
        }

        self.start_transmitter_component().await?;

        // If --test-tx was passed, inject a single TransmitRequest after a
        // brief settle period, then trigger shutdown after a generous window
        // covering the worst-case TX cycle (slot wait + 12.64s TX + tail).
        if let Some(test_tx_text) = self.test_tx.clone() {
            let bus = self.message_bus.clone();
            let shutdown = self.shutdown_signal.clone();
            let offset = self.test_tx_offset;
            tokio::spawn(async move {
                // Settle: let hamlib spawn rigctld and connect.
                tokio::time::sleep(Duration::from_secs(3)).await;

                info!(
                    "TEST-TX: injecting TransmitRequest '{}' at offset {:.0} Hz",
                    test_tx_text, offset
                );

                let req = crate::message_bus::ComponentMessage::new(
                    crate::message_bus::ComponentId::Coordinator,
                    crate::message_bus::ComponentId::Ft8Transmitter,
                    crate::message_bus::MessageType::TransmitRequest {
                        message_text: test_tx_text.clone(),
                        frequency_offset: offset,
                        qso_id: None,
                    },
                    Instant::now(),
                );
                if let Err(e) = bus.send_message(req).await {
                    error!("TEST-TX: send TransmitRequest failed: {}", e);
                    shutdown.store(true, Ordering::Release);
                    return;
                }

                // Worst case: ≤16s slot wait + 12.64s TX + tail/settle = ~30s.
                tokio::time::sleep(Duration::from_secs(35)).await;
                info!("TEST-TX: cycle window elapsed — shutting down");
                shutdown.store(true, Ordering::Release);
            });
        }

        self.start_autonomous_component().await?;
        self.start_dx_cluster_component().await?;
        self.start_pskreporter_component().await?;

        // Start coordinator tasks
        self.start_coordinator_tasks().await?;

        let startup_duration = self.startup_time.elapsed();
        info!(
            "Application startup completed in {:.2}s",
            startup_duration.as_secs_f64()
        );

        // Main application loop
        self.run_main_loop().await?;

        // Graceful shutdown
        self.shutdown().await?;

        Ok(())
    }

    /// Initialize metrics collection
    async fn init_metrics(&self) -> Result<()> {
        info!("Initializing metrics on port {}", self.metrics_port);

        #[cfg(feature = "prometheus")]
        {
            use metrics_exporter_prometheus::PrometheusBuilder;

            let builder =
                PrometheusBuilder::new().with_http_listener(([0, 0, 0, 0], self.metrics_port));

            builder
                .install()
                .context("Failed to install Prometheus metrics exporter")?;

            info!("Metrics server started on port {}", self.metrics_port);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_config::Config;

    #[tokio::test]
    async fn test_coordinator_creation() {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));

        let coordinator = ApplicationCoordinator::new(
            config, None, true,  // no_audio
            true,  // headless
            false, // metrics
            9090, None, // no WAV
            None, // no test-tx
            1500.0, shutdown,
        )
        .await;

        assert!(coordinator.is_ok());
    }

    #[tokio::test]
    async fn test_coordinator_config() {
        let config = CoordinatorConfig::default();

        assert_eq!(config.startup_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
        assert!(config.message_buffer_size > 0);
    }

    #[test]
    fn test_resample_identity() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let output = resample_linear(&input, 48000, 48000);
        assert_eq!(output.len(), 4);
    }

    #[test]
    fn test_resample_downsample() {
        let input: Vec<f32> = (0..48000).map(|i| (i as f32 / 48000.0).sin()).collect();
        let output = resample_linear(&input, 48000, 12000);
        // Should be approximately 12000 samples
        assert!((output.len() as i64 - 12000).abs() <= 1);
    }

    #[tokio::test]
    async fn test_wav_playback_decodes_messages() {
        // Use a known WAV fixture from the FT8 test suite
        let wav_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav");

        if !wav_path.exists() {
            eprintln!(
                "Skipping WAV playback test: fixture not found at {:?}",
                wav_path
            );
            return;
        }

        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));

        let coordinator = ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // no metrics
            9090,
            Some(wav_path),
            None, // no test-tx
            1500.0,
            shutdown,
        )
        .await
        .expect("coordinator creation should succeed");

        // run_wav_playback exits after decoding -- should not error
        let result = coordinator.run().await;
        assert!(
            result.is_ok(),
            "WAV playback should succeed: {:?}",
            result.err()
        );
    }
}
