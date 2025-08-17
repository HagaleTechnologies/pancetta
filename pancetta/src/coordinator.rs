//! # Application Coordinator
//!
//! The Application Coordinator is the central orchestrator for the Pancetta application.
//! It manages the lifecycle of all components and coordinates communication between them.
//!
//! ## Architecture
//!
//! The coordinator uses a message-driven architecture with dedicated components:
//! - **Audio Manager**: Real-time audio input and buffering
//! - **DSP Pipeline**: Signal processing and filtering  
//! - **FT8 Decoder**: Digital mode decoding
//! - **TUI Manager**: User interface and display
//! - **Configuration Manager**: Hot-reload configuration management
//!
//! ## Performance Goals
//!
//! - Component startup: <100ms
//! - Message latency: <1ms between components
//! - Graceful shutdown: <5s
//! - Memory usage: <50MB for coordination layer

use anyhow::{Context, Result};
// TODO: Import when AudioManager is available
// use pancetta_audio::{AudioManager, AudioConfig as AudioSettings, AudioDevice};
use pancetta_config::Config;
use pancetta_dsp::{DspPipeline, PipelineBuilder};
use pancetta_ft8::{Ft8Decoder, Ft8Config, DecodedMessage};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, span, warn, Level};
use uuid::Uuid;

use crate::message_bus::{MessageBus, ComponentMessage, ComponentId, MessageType};

/// Application coordinator that manages all Pancetta components
pub struct ApplicationCoordinator {
    /// Unique instance identifier
    id: Uuid,
    
    /// Application configuration (hot-reloadable)
    config: Arc<RwLock<Config>>,
    
    /// Central message bus for inter-component communication
    message_bus: MessageBus,
    
    /// Component managers
    // audio_manager: Option<AudioManager>,
    dsp_pipeline: Option<DspPipeline>,
    ft8_decoder: Option<Ft8Decoder>,
    
    /// Component task handles
    task_handles: Vec<JoinHandle<Result<()>>>,
    
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
    
    /// Performance metrics
    message_count: Arc<std::sync::atomic::AtomicU64>,
    last_audio_timestamp: Arc<RwLock<Option<Instant>>>,
    last_decode_timestamp: Arc<RwLock<Option<Instant>>>,
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

/// Component health status
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    pub component_id: ComponentId,
    pub is_healthy: bool,
    pub last_heartbeat: Instant,
    pub error_count: u32,
    pub message_count: u64,
    pub avg_latency_ms: f64,
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
        shutdown_signal: Arc<AtomicBool>,
    ) -> Result<Self> {
        let span = span!(Level::INFO, "coordinator_init");
        let _enter = span.enter();
        
        info!("Initializing Application Coordinator");
        
        let id = Uuid::new_v4();
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
            // audio_manager: None,
            dsp_pipeline: None,
            ft8_decoder: None,
            task_handles: Vec::new(),
            is_running: Arc::new(AtomicBool::new(false)),
            shutdown_signal,
            startup_time,
            audio_device,
            no_audio,
            headless,
            enable_metrics,
            metrics_port,
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
        self.is_running.store(true, Ordering::Relaxed);
        
        // Initialize metrics if enabled
        if self.enable_metrics {
            self.init_metrics().await?;
        }
        
        // Start all components in dependency order
        self.start_audio_component().await?;
        self.start_dsp_component().await?;
        self.start_ft8_component().await?;
        self.start_hamlib_component().await?;
        self.start_qso_component().await?;
        
        if !self.headless {
            self.start_tui_component().await?;
        }
        
        // Start coordinator tasks
        self.start_coordinator_tasks().await?;
        
        let startup_duration = self.startup_time.elapsed();
        info!("Application startup completed in {:.2}s", startup_duration.as_secs_f64());
        
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
            
            let builder = PrometheusBuilder::new()
                .with_http_listener(([0, 0, 0, 0], self.metrics_port));
                
            builder.install()
                .context("Failed to install Prometheus metrics exporter")?;
                
            info!("Metrics server started on port {}", self.metrics_port);
        }
        
        Ok(())
    }
    
    /// Start audio processing component
    async fn start_audio_component(&mut self) -> Result<()> {
        if self.no_audio {
            info!("Audio processing disabled");
            return Ok(());
        }
        
        let span = span!(Level::INFO, "start_audio");
        let _enter = span.enter();
        
        info!("Starting audio component (stubbed)");
        
        // TODO: Implement when AudioManager is available
        // For now, just create a dummy task that generates test audio
        let (audio_tx, _audio_rx) = self.message_bus.create_channel(ComponentId::Audio).await?;
        
        let audio_handle = {
            let last_timestamp = self.last_audio_timestamp.clone();
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    // Generate test audio samples
                    let test_samples: Vec<f32> = (0..1024)
                        .map(|i| 0.1 * (2.0 * std::f32::consts::PI * 1500.0 * i as f32 / 48000.0).sin())
                        .collect();
                    
                    // Update timestamp
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }
                    
                    // Send test audio data through message bus
                    let message = ComponentMessage::new(
                        ComponentId::Audio,
                        ComponentId::Dsp,
                        MessageType::AudioData(test_samples),
                        Instant::now(),
                    );
                    
                    if let Err(e) = audio_tx.try_send(message) {
                        warn!("Failed to send audio data: {}", e);
                    }
                    
                    // Simulate audio buffer timing
                    sleep(Duration::from_millis(20)).await;
                }
                
                info!("Audio component stopped");
                Ok(())
            })
        };
        
        self.task_handles.push(audio_handle);
        
        info!("Audio component started (stubbed)");
        Ok(())
    }
    
    /// Start DSP processing component
    async fn start_dsp_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();
        
        info!("Starting DSP component");
        
        // Create FT8-optimized DSP pipeline
        let (mut dsp_pipeline, dsp_input_tx, dsp_output_rx) = 
            pancetta_dsp::factory::create_ft8_pipeline()?;
        
        // Get message channels
        let (dsp_tx, dsp_rx) = self.message_bus.create_channel(ComponentId::Dsp).await?;
        
        // Start DSP processing task
        let dsp_handle = {
            let shutdown_for_input = self.shutdown_signal.clone();
            let shutdown_for_output = self.shutdown_signal.clone();
            let message_count = self.message_count.clone();
            
            tokio::spawn(async move {
                // Start the pipeline
                let pipeline_task = tokio::spawn(async move {
                    if let Err(e) = dsp_pipeline.start().await {
                        error!("DSP pipeline error: {}", e);
                    }
                });
                
                // Process incoming audio data
                let input_task = tokio::spawn(async move {
                    while !shutdown_for_input.load(Ordering::Relaxed) {
                        match dsp_rx.try_recv() {
                            Ok(message) => {
                                message_count.fetch_add(1, Ordering::Relaxed);
                                
                                if let MessageType::AudioData(samples) = message.message_type {
                                    if let Err(e) = dsp_input_tx.send(samples) {
                                        warn!("Failed to send samples to DSP: {}", e);
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => {
                                // No message available, yield and continue
                                tokio::task::yield_now().await;
                            }
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                break;
                            }
                        }
                    }
                });
                
                // Process DSP output
                let output_task = tokio::spawn(async move {
                    while !shutdown_for_output.load(Ordering::Relaxed) {
                        if let Ok(processed_window) = dsp_output_rx.recv() {
                            // Send to FT8 decoder
                            let message = ComponentMessage::new(
                                ComponentId::Dsp,
                                ComponentId::Ft8Decoder,
                                MessageType::DspData(processed_window),
                                Instant::now(),
                            );
                            
                            if let Err(e) = dsp_tx.try_send(message) {
                                warn!("Failed to send DSP data: {}", e);
                            }
                        }
                    }
                });
                
                // Wait for any task to complete
                tokio::select! {
                    _ = pipeline_task => {},
                    _ = input_task => {},
                    _ = output_task => {},
                }
                
                info!("DSP component stopped");
                Ok(())
            })
        };
        
        // DSP pipeline is moved into the task, so we set to None
        self.task_handles.push(dsp_handle);
        
        info!("DSP component started");
        Ok(())
    }
    
    /// Start FT8 decoding component
    async fn start_ft8_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();
        
        info!("Starting FT8 component");
        
        // Create FT8 decoder
        let ft8_config = Ft8Config::default();
        let ft8_decoder = Ft8Decoder::new(ft8_config)?;
        
        // Get message channels
        let (ft8_tx, ft8_rx) = self.message_bus.create_channel(ComponentId::Ft8Decoder).await?;
        
        // Start FT8 processing task
        let ft8_handle = {
            let shutdown = self.shutdown_signal.clone();
            let last_decode_timestamp = self.last_decode_timestamp.clone();
            let mut decoder = ft8_decoder; // Move decoder into the task (needs mut for decode_window)
            
            tokio::spawn(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    match ft8_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::DspData(window) = message.message_type {
                                match decoder.decode_window(&window) {
                                    Ok(decoded_messages) => {
                                        // Update timestamp
                                        {
                                            let mut timestamp = last_decode_timestamp.write().await;
                                            *timestamp = Some(Instant::now());
                                        }
                                        
                                        // Send decoded messages to TUI
                                        for decoded_msg in decoded_messages {
                                            let tui_message = ComponentMessage::new(
                                                ComponentId::Ft8Decoder,
                                                ComponentId::Tui,
                                                MessageType::DecodedMessage(decoded_msg),
                                                Instant::now(),
                                            );
                                            
                                            if let Err(e) = ft8_tx.try_send(tui_message) {
                                                warn!("Failed to send decoded message: {}", e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        debug!("FT8 decode error: {}", e);
                                    }
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            // No message available, yield and continue
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }
                
                info!("FT8 component stopped");
                Ok(())
            })
        };
        
        // Decoder is moved into the task, so we set to None
        self.task_handles.push(ft8_handle);
        
        info!("FT8 component started");
        Ok(())
    }
    
    /// Start Hamlib component for rig control
    async fn start_hamlib_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_hamlib");
        let _enter = span.enter();
        
        info!("Starting Hamlib component");
        
        // Get message channels
        let (hamlib_tx, hamlib_rx) = self.message_bus.create_channel(ComponentId::Hamlib).await?;
        
        // Start Hamlib task
        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                // Initialize with mock rig for POC
                #[cfg(feature = "mock-rig")]
                {
                    use pancetta_hamlib::{MockRig, RigControl};
                    
                    let rig = MockRig::default();
                    if let Err(e) = rig.connect().await {
                        error!("Failed to connect mock rig: {}", e);
                        return Err(anyhow::anyhow!("Hamlib connection failed"));
                    }
                    
                    info!("Mock rig connected successfully");
                }
                
                // Process messages
                while !shutdown.load(Ordering::Relaxed) {
                    match hamlib_rx.try_recv() {
                        Ok(mut message) => {
                            // Mark message as received
                            message.latency_tracking.received_at = Some(Instant::now());
                            message.latency_tracking.processing_started_at = Some(Instant::now());
                            
                            if let MessageType::RigControl(rig_msg) = message.message_type {
                                match rig_msg {
                                    crate::message_bus::RigControlMessage::SetFrequency { vfo, frequency } => {
                                        debug!("Setting frequency VFO {} to {}", vfo, frequency);
                                    }
                                    crate::message_bus::RigControlMessage::SetPtt { state } => {
                                        debug!("Setting PTT to {}", state);
                                    }
                                    _ => {}
                                }
                            }
                            
                            // Mark processing complete
                            message.latency_tracking.processing_completed_at = Some(Instant::now());
                            
                            // Log latency if significant
                            if let Some(latency) = message.total_latency_us() {
                                if latency > 1000 {
                                    debug!("Hamlib message latency: {}μs", latency);
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }
                
                info!("Hamlib component stopped");
                Ok(())
            })
        };
        
        self.task_handles.push(hamlib_handle);
        
        info!("Hamlib component started");
        Ok(())
    }
    
    /// Start QSO management component
    async fn start_qso_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_qso");
        let _enter = span.enter();
        
        info!("Starting QSO component");
        
        // Get message channels
        let (qso_tx, qso_rx) = self.message_bus.create_channel(ComponentId::Qso).await?;
        
        // Start QSO task
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                use pancetta_qso::{QsoManager, QsoManagerConfig};
                
                // Create QSO manager
                let config = QsoManagerConfig {
                    our_callsign: "NOCALL".to_string(),
                    our_grid: Some("FN42".to_string()),
                    ..Default::default()
                };
                
                let qso_manager = QsoManager::new(config);
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }
                
                info!("QSO manager started");
                
                // Process messages
                while !shutdown.load(Ordering::Relaxed) {
                    match qso_rx.try_recv() {
                        Ok(mut message) => {
                            // Track latency
                            message.latency_tracking.received_at = Some(Instant::now());
                            message.latency_tracking.processing_started_at = Some(Instant::now());
                            
                            if let MessageType::QsoMessage(qso_msg) = message.message_type {
                                match qso_msg {
                                    crate::message_bus::QsoMessage::StartQso { callsign, frequency } => {
                                        debug!("Starting QSO with {} on {}", callsign, frequency);
                                    }
                                    crate::message_bus::QsoMessage::LogQso { qso_data } => {
                                        debug!("Logging QSO: {}", qso_data);
                                    }
                                    _ => {}
                                }
                            }
                            
                            message.latency_tracking.processing_completed_at = Some(Instant::now());
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }
                
                info!("QSO component stopped");
                Ok(())
            })
        };
        
        self.task_handles.push(qso_handle);
        
        info!("QSO component started");
        Ok(())
    }
    
    /// Start TUI component (if not headless)
    async fn start_tui_component(&mut self) -> Result<()> {
        if self.headless {
            return Ok(());
        }
        
        let span = span!(Level::INFO, "start_tui");
        let _enter = span.enter();
        
        info!("Starting TUI component");
        
        // Get message channels
        let (tui_tx, tui_rx) = self.message_bus.create_channel(ComponentId::Tui).await?;
        
        // Start TUI task
        let tui_handle = {
            let _config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                // This will be implemented when we have the TUI interface ready
                // For now, just handle incoming messages
                while !shutdown.load(Ordering::Relaxed) {
                    match tui_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                MessageType::DecodedMessage(decoded_msg) => {
                                    // TODO: Update TUI with decoded message
                                    info!("Decoded: {}", decoded_msg.text);
                                }
                                _ => {}
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            // No message available, yield and continue
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            break;
                        }
                    }
                }
                
                info!("TUI component stopped");
                Ok(())
            })
        };
        
        self.task_handles.push(tui_handle);
        
        info!("TUI component started");
        Ok(())
    }
    
    /// Start coordinator management tasks
    async fn start_coordinator_tasks(&mut self) -> Result<()> {
        // Health monitoring task
        let health_handle = {
            let message_bus = self.message_bus.clone();
            let shutdown = self.shutdown_signal.clone();
            let mut health_interval = interval(Duration::from_secs(5));
            
            tokio::spawn(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    health_interval.tick().await;
                    
                    // Check component health
                    let health_status = message_bus.get_component_health().await;
                    for health in health_status {
                        if !health.is_healthy {
                            warn!("Component {:?} is unhealthy: {} errors", 
                                  health.component_id, health.error_count);
                        }
                    }
                }
                
                Ok(())
            })
        };
        
        // Configuration hot-reload task
        let config_handle = {
            let config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                // TODO: Implement configuration file watching
                while !shutdown.load(Ordering::Relaxed) {
                    sleep(Duration::from_secs(1)).await;
                }
                Ok(())
            })
        };
        
        self.task_handles.push(health_handle);
        self.task_handles.push(config_handle);
        
        Ok(())
    }
    
    /// Main application loop
    async fn run_main_loop(&self) -> Result<()> {
        info!("Entering main application loop");
        
        let mut stats_interval = interval(Duration::from_secs(30));
        
        while !self.shutdown_signal.load(Ordering::Relaxed) {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.log_performance_stats().await;
                }
                _ = sleep(Duration::from_millis(100)) => {
                    // Main loop iteration
                }
            }
        }
        
        info!("Main application loop completed");
        Ok(())
    }
    
    /// Log performance statistics
    async fn log_performance_stats(&self) -> () {
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
    
    /// Graceful shutdown of all components
    async fn shutdown(self) -> Result<()> {
        let span = span!(Level::INFO, "coordinator_shutdown");
        let _enter = span.enter();
        
        info!("Starting graceful shutdown");
        self.is_running.store(false, Ordering::Relaxed);
        
        // Signal all components to stop
        self.shutdown_signal.store(true, Ordering::Relaxed);
        
        // Wait for all tasks to complete with timeout
        let shutdown_timeout = Duration::from_secs(10);
        let start_time = Instant::now();
        
        for (index, handle) in self.task_handles.into_iter().enumerate() {
            let remaining_time = shutdown_timeout.saturating_sub(start_time.elapsed());
            
            if remaining_time.is_zero() {
                warn!("Shutdown timeout reached, aborting remaining tasks");
                handle.abort();
                continue;
            }
            
            match tokio::time::timeout(remaining_time, handle).await {
                Ok(Ok(_)) => {
                    debug!("Task {} completed successfully", index);
                }
                Ok(Err(e)) => {
                    warn!("Task {} completed with error: {}", index, e);
                }
                Err(_) => {
                    warn!("Task {} timed out during shutdown", index);
                }
            }
        }
        
        let shutdown_duration = start_time.elapsed();
        info!("Graceful shutdown completed in {:.2}s", shutdown_duration.as_secs_f64());
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_coordinator_creation() {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        
        let coordinator = ApplicationCoordinator::new(
            config,
            None,
            true, // no_audio
            true, // headless
            false, // metrics
            9090,
            shutdown,
        ).await;
        
        assert!(coordinator.is_ok());
    }
    
    #[tokio::test]
    async fn test_coordinator_config() {
        let config = CoordinatorConfig::default();
        
        assert_eq!(config.startup_timeout, Duration::from_secs(30));
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
        assert!(config.message_buffer_size > 0);
    }
}