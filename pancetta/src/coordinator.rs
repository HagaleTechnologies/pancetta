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

use anyhow::Result;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use pancetta_config::Config;
use pancetta_dsp::DspPipeline;
use pancetta_ft8::{Ft8Decoder, Ft8Config};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
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
        
        // Check if we should use stub audio (for testing without real audio device)
        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();
        
        if use_stub {
            info!("Starting audio component with STUB (set PANCETTA_STUB_AUDIO to disable)");
        } else {
            info!("Starting audio component with real AudioManager");
        }
        
        // Create message bus channel
        let (audio_tx, _audio_rx) = self.message_bus.create_channel(ComponentId::Audio).await?;
        
        let audio_handle = if !use_stub {
            // Use real AudioManager
            let config = self.config.read().await;
            let audio_config = AudioManagerConfig {
                input_device: Some(config.audio.input_device.clone()),
                output_device: Some(config.audio.output_device.clone()),
                sample_rate: config.audio.sample_rate,
                buffer_size: config.audio.buffer_size as usize,
                channels: config.audio.input_channels as u16,
                enable_monitoring: false, // Use default for now
                target_latency_ms: 1.0, // Use 1ms default target
                input_gain_db: config.audio.levels.input_gain_db,
            };
            drop(config);
            
            // Create task to process audio from AudioManager
            let shutdown = self.shutdown_signal.clone();
            
            // Run AudioManager in a dedicated thread (non-Send)
            // Create it inside the thread to avoid Send requirements
            let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(100);
            
            std::thread::spawn(move || {
                // Create AudioManager inside the thread
                let mut audio_manager = match AudioManager::with_config(audio_config) {
                    Ok(manager) => manager,
                    Err(e) => {
                        error!("Failed to create AudioManager: {}", e);
                        return;
                    }
                };
                
                // List available devices for debugging
                let devices = audio_manager.list_devices();
                for device in devices {
                    debug!("Available audio device: {}", device.name);
                }
                
                // Start audio stream
                if let Err(e) = audio_manager.start() {
                    error!("Failed to start audio stream: {}", e);
                    return;
                }
                
                info!("AudioManager started in dedicated thread");
                
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    
                    // Process audio from AudioManager
                    match audio_manager.process_audio() {
                        Ok(Some(samples)) => {
                            // Send samples through channel to async task
                            if let Err(_) = result_tx.blocking_send(samples) {
                                break; // Channel closed
                            }
                            
                            // Log statistics periodically
                            let stats = audio_manager.get_stats();
                            if stats.samples_processed % 48000 == 0 {
                                debug!(
                                    "Audio stats - Latency: {}μs, Signal: {:.3}, Samples: {}",
                                    stats.current_latency_us,
                                    stats.signal_level,
                                    stats.samples_processed
                                );
                            }
                        }
                        Ok(None) => {
                            // No audio data available yet
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(e) => {
                            error!("Audio processing error: {}", e);
                        }
                    }
                }
                
                // Stop audio manager on shutdown
                if let Err(e) = audio_manager.stop() {
                    error!("Error stopping audio manager: {}", e);
                }
                
                info!("Audio manager thread stopped");
            });
            
            // Create async task to relay audio samples to message bus
            let last_timestamp = self.last_audio_timestamp.clone();
            
            tokio::spawn(async move {
                while let Some(samples) = result_rx.recv().await {
                    // Update timestamp
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }
                    
                    // Send audio data through message bus
                    let message = ComponentMessage::new(
                        ComponentId::Audio,
                        ComponentId::Dsp,
                        MessageType::AudioData(samples),
                        Instant::now(),
                    );
                    
                    if let Err(e) = audio_tx.try_send(message) {
                        warn!("Failed to send audio data: {}", e);
                    }
                }
                
                info!("Audio relay task stopped");
                Ok(())
            })
        } else {
            // Use stub audio for testing
            let last_timestamp = self.last_audio_timestamp.clone();
            let shutdown = self.shutdown_signal.clone();
            
            // Get audio config from locked config
            let config = self.config.read().await;
            let sample_rate = config.audio.sample_rate;
            let buffer_size = config.audio.buffer_size as usize;
            drop(config); // Release lock early
            
            tokio::spawn(async move {
                let mut phase = 0.0f32;
                let frequency = 1500.0; // FT8 center frequency
                let mut sample_count = 0u64;
                
                // Calculate buffer timing based on real config
                let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
                let mut process_interval = interval(Duration::from_millis(buffer_duration_ms.max(5)));
                
                info!("Audio stub: {}Hz sample rate, {} buffer size, {}ms interval", 
                     sample_rate, buffer_size, buffer_duration_ms);
                
                while !shutdown.load(Ordering::Relaxed) {
                    process_interval.tick().await;
                    
                    // Generate more realistic test audio
                    let mut samples = Vec::with_capacity(buffer_size);
                    for _ in 0..buffer_size {
                        // Generate sine wave (add noise later when rand is available)
                        let sample = 0.1 * phase.sin();
                        samples.push(sample);
                        
                        phase += 2.0 * std::f32::consts::PI * frequency / sample_rate as f32;
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                        sample_count += 1;
                    }
                    
                    // Update timestamp
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }
                    
                    // Send audio data through message bus
                    let message = ComponentMessage::new(
                        ComponentId::Audio,
                        ComponentId::Dsp,
                        MessageType::AudioData(samples),
                        Instant::now(),
                    );
                    
                    if let Err(e) = audio_tx.try_send(message) {
                        warn!("Failed to send audio data: {}", e);
                    }
                    
                    // Log statistics periodically (every second)
                    if sample_count % sample_rate as u64 == 0 {
                        debug!(
                            "Audio stub stats - Samples: {}, Buffer duration: {}ms",
                            sample_count,
                            buffer_duration_ms
                        );
                    }
                }
                
                info!("Audio component stopped");
                Ok(())
            })
        };
        
        self.task_handles.push(audio_handle);
        
        if use_stub {
            info!("Audio component started (stub mode)");
        } else {
            info!("Audio component started (real AudioManager)");
        }
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
                
                // Process DSP output with windowing for FT8
                let output_task = tokio::spawn(async move {
                    // Buffer for accumulating samples for FT8 window
                    let mut ft8_buffer = Vec::with_capacity(151680); // 12.64s at 12kHz
                    const FT8_WINDOW_SIZE: usize = 151680; // 12.64 seconds at 12kHz
                    
                    while !shutdown_for_output.load(Ordering::Relaxed) {
                        if let Ok(processed_samples) = dsp_output_rx.recv() {
                            // Accumulate samples
                            ft8_buffer.extend_from_slice(&processed_samples);
                            
                            // Check if we have enough samples for FT8 window
                            while ft8_buffer.len() >= FT8_WINDOW_SIZE {
                                // Extract exactly one window
                                let window: Vec<f32> = ft8_buffer.drain(..FT8_WINDOW_SIZE).collect();
                                
                                // Send to FT8 decoder
                                let message = ComponentMessage::new(
                                    ComponentId::Dsp,
                                    ComponentId::Ft8Decoder,
                                    MessageType::DspData(window),
                                    Instant::now(),
                                );
                                
                                if let Err(e) = dsp_tx.try_send(message) {
                                    warn!("Failed to send DSP data to FT8 decoder: {}", e);
                                }
                                
                                debug!("Sent FT8 window ({}  samples) to decoder", FT8_WINDOW_SIZE);
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
                // Check if we should use mock or real rigctld
                let use_mock = std::env::var("PANCETTA_MOCK_RIG")
                    .map(|v| v.to_lowercase() == "true" || v == "1")
                    .unwrap_or(true); // Default to mock for safety
                
                // Create appropriate rig controller
                let rig: Box<dyn pancetta_hamlib::RigControl + Send + Sync> = if use_mock {
                    info!("Using mock rig (set PANCETTA_MOCK_RIG to disable)");
                    Box::new(pancetta_hamlib::MockRig::default())
                } else {
                    info!("Using rigctld client");
                    
                    // Check for custom rigctld config from environment
                    let host = std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
                    let port = std::env::var("RIGCTLD_PORT")
                        .ok()
                        .and_then(|p| p.parse::<u16>().ok())
                        .unwrap_or(4532);
                    
                    let config = pancetta_hamlib::RigctldConfig {
                        host,
                        port,
                        ..Default::default()
                    };
                    
                    Box::new(pancetta_hamlib::RigctldClient::new(config))
                };
                
                // Try to connect
                match rig.connect().await {
                    Ok(_) => {
                        info!("Rig connected successfully");
                    }
                    Err(e) => {
                        error!("Failed to connect to rig: {}. Continuing without rig control.", e);
                        // Continue anyway - rig control is optional
                    }
                }
                
                // Start polling task for status updates
                let rig_poll = Arc::new(rig);
                let rig_for_polling = Arc::clone(&rig_poll);
                let hamlib_tx_for_polling = hamlib_tx.clone();
                let shutdown_for_polling = shutdown.clone();
                
                tokio::spawn(async move {
                    let mut poll_interval = interval(Duration::from_millis(500));
                    
                    while !shutdown_for_polling.load(Ordering::Relaxed) {
                        poll_interval.tick().await;
                        
                        if let Ok(status) = rig_for_polling.get_status().await {
                            if status.connection_state == pancetta_hamlib::ConnectionState::Connected {
                                // Get current frequency
                                if let Ok(freq) = rig_for_polling.get_frequency(pancetta_hamlib::Vfo::Current).await {
                                let message = ComponentMessage::new(
                                    ComponentId::Hamlib,
                                    ComponentId::Tui,
                                    MessageType::RigControl(crate::message_bus::RigControlMessage::FrequencyResponse {
                                        vfo: 0,
                                        frequency: freq,
                                    }),
                                    Instant::now(),
                                );
                                let _ = hamlib_tx_for_polling.try_send(message);
                            }
                            
                                // Get signal strength
                                if let Ok(strength) = rig_for_polling.get_s_meter().await {
                                    let message = ComponentMessage::new(
                                        ComponentId::Hamlib,
                                        ComponentId::Tui,
                                        MessageType::RigControl(crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                            dbm: strength,
                                        }),
                                        Instant::now(),
                                    );
                                    let _ = hamlib_tx_for_polling.try_send(message);
                                }
                            }
                        }
                    }
                });
                
                // Process messages
                while !shutdown.load(Ordering::Relaxed) {
                    match hamlib_rx.try_recv() {
                        Ok(mut message) => {
                            // Mark message as received
                            message.latency_tracking.received_at = Some(Instant::now());
                            message.latency_tracking.processing_started_at = Some(Instant::now());
                            
                            if let MessageType::RigControl(ref rig_msg) = message.message_type {
                                match rig_msg {
                                    crate::message_bus::RigControlMessage::SetFrequency { vfo, frequency } => {
                                        debug!("Setting frequency VFO {} to {}", vfo, frequency);
                                        let vfo_enum = if *vfo == 0 {
                                            pancetta_hamlib::Vfo::A
                                        } else {
                                            pancetta_hamlib::Vfo::B
                                        };
                                        
                                        if let Err(e) = rig_poll.set_frequency(vfo_enum, *frequency).await {
                                            error!("Failed to set frequency: {}", e);
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::SetPtt { state } => {
                                        debug!("Setting PTT to {}", state);
                                        let ptt = if *state {
                                            pancetta_hamlib::PttState::On
                                        } else {
                                            pancetta_hamlib::PttState::Off
                                        };
                                        
                                        if let Err(e) = rig_poll.set_ptt(pancetta_hamlib::Vfo::Current, ptt).await {
                                            error!("Failed to set PTT: {}", e);
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::GetFrequency { vfo } => {
                                        let vfo_enum = if *vfo == 0 {
                                            pancetta_hamlib::Vfo::A
                                        } else {
                                            pancetta_hamlib::Vfo::B
                                        };
                                        
                                        if let Ok(freq) = rig_poll.get_frequency(vfo_enum).await {
                                            let response = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                message.source,
                                                MessageType::RigControl(crate::message_bus::RigControlMessage::FrequencyResponse {
                                                    vfo: *vfo,
                                                    frequency: freq,
                                                }),
                                                Instant::now(),
                                            );
                                            let _ = hamlib_tx.try_send(response);
                                        }
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
            let config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();
            
            tokio::spawn(async move {
                // Create TUI configuration
                let config_lock = config.read().await;
                let tui_config = pancetta_tui::Config {
                    station: pancetta_tui::config::StationConfig {
                        call_sign: config_lock.station.callsign.clone(),
                        grid_square: config_lock.station.grid_square.clone(),
                        power: config_lock.station.power_watts,
                        antenna: "Vertical".to_string(), // TODO: Add antenna field to config
                        rig: config_lock.rig.model.clone(),
                        default_frequency: 14.074,
                    },
                    ui: pancetta_tui::config::UiConfig {
                        theme: pancetta_tui::Theme::Dark,
                        refresh_rate: 30,
                        max_messages: 100,
                        show_waterfall: true,
                        show_coordinates: true,
                        time_format: pancetta_tui::config::TimeFormat::UTC24,
                        frequency_format: pancetta_tui::config::FrequencyFormat::MHz,
                    },
                    audio: pancetta_tui::config::AudioConfig {
                        device: Some(config_lock.audio.input_device.clone()),
                        sample_rate: config_lock.audio.sample_rate,
                        buffer_size: config_lock.audio.buffer_size as usize,
                        auto_gain: false,
                        gain_level: config_lock.audio.levels.input_gain_db,
                    },
                    decoder: pancetta_tui::config::DecoderConfig {
                        enabled_modes: vec!["FT8".to_string()],
                        minimum_snr: -20,
                        decode_depth: 3,
                        aggressive_decode: true,
                        enable_averaging: false,
                    },
                    bands: pancetta_tui::config::BandConfig {
                        bands: vec![],
                        default_band: "20m".to_string(),
                    },
                };
                drop(config_lock);
                
                // Create TUI app
                let app = match pancetta_tui::create_app(tui_config, None).await {
                    Ok(app) => Arc::new(RwLock::new(app)),
                    Err(e) => {
                        error!("Failed to create TUI app: {}", e);
                        return Err(anyhow::anyhow!("TUI creation failed"));
                    }
                };
                
                // Process messages and update TUI state
                while !shutdown.load(Ordering::Relaxed) {
                    match tui_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                MessageType::DecodedMessage(decoded_msg) => {
                                    // Update TUI with decoded message
                                    let mut app_lock = app.write().await;
                                    
                                    // Extract callsign and grid from the message
                                    let call_sign = decoded_msg.message.from_callsign.clone();
                                    let grid_square = decoded_msg.message.grid_square.clone();
                                    
                                    app_lock.add_decoded_message(pancetta_tui::DecodedMessage {
                                        timestamp: chrono::Utc::now(),
                                        frequency: 14.074, // TODO: Get actual frequency in MHz
                                        mode: "FT8".to_string(),
                                        snr: decoded_msg.snr_db as i32,
                                        delta_time: decoded_msg.time_offset as f32,
                                        delta_freq: decoded_msg.frequency_offset as f32,
                                        call_sign,
                                        grid_square,
                                        message: decoded_msg.text.clone(),
                                        distance: None, // TODO: Calculate distance from grid
                                        bearing: None,  // TODO: Calculate bearing from grid
                                    }).await;
                                    drop(app_lock);
                                    
                                    info!("TUI: Decoded message added - {}", decoded_msg.text);
                                }
                                MessageType::StatusUpdate(status) => {
                                    debug!("TUI: Status update - {}", status);
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