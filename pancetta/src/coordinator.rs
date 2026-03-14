//! # Application Coordinator
//!
//! The Application Coordinator is the central orchestrator for the Pancetta application.
//! It manages the lifecycle of all components and coordinates communication between them.
//!
//! ## Architecture
//!
//! The coordinator uses point-to-point crossbeam channels for the core data path:
//!   Audio → DSP → FT8 Decoder → TUI
//!
//! The message bus is retained for control messages and health monitoring.
//!
//! ## WAV Playback Mode
//!
//! When started with `--wav <file>`, the coordinator reads a WAV file, resamples to
//! 12 kHz mono, feeds the samples through the DSP/FT8 pipeline, prints decoded messages,
//! and exits.

use anyhow::Result;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use pancetta_config::Config;
use pancetta_dsp::DspPipeline;
use pancetta_ft8::{Ft8Decoder, Ft8Config, Ft8Encoder, Ft8Modulator};
use std::path::PathBuf;
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

    /// WAV file playback path (if set, runs in playback mode)
    wav_path: Option<PathBuf>,

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
        wav_path: Option<PathBuf>,
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
            wav_path,
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
        self.start_qso_component().await?;
        self.start_transmitter_component().await?;
        self.start_autonomous_component().await?;
        self.start_dx_cluster_component().await?;

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

    // =========================================================================
    // WAV playback mode
    // =========================================================================

    /// Run WAV playback mode: read file, decode, print results, exit.
    async fn run_wav_playback(&self, wav_path: PathBuf) -> Result<()> {
        info!("WAV playback mode: {}", wav_path.display());

        // Read WAV file
        let reader = hound::WavReader::open(&wav_path)
            .map_err(|e| anyhow::anyhow!("Failed to open WAV file {}: {}", wav_path.display(), e))?;

        let spec = reader.spec();
        info!(
            "WAV: {} channels, {} Hz, {:?}, {} bits",
            spec.channels, spec.sample_rate, spec.sample_format, spec.bits_per_sample
        );

        // Read all samples as f32
        let raw_samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .into_samples::<i32>()
                    .filter_map(|s| s.ok())
                    .map(|s| s as f32 / max_val)
                    .collect()
            }
            hound::SampleFormat::Float => reader
                .into_samples::<f32>()
                .filter_map(|s| s.ok())
                .collect(),
        };

        info!("Read {} raw samples", raw_samples.len());

        // Mix down to mono if stereo
        let mono_samples: Vec<f32> = if spec.channels > 1 {
            let ch = spec.channels as usize;
            raw_samples
                .chunks(ch)
                .map(|frame| frame.iter().sum::<f32>() / ch as f32)
                .collect()
        } else {
            raw_samples
        };

        // Resample to 12 kHz if needed
        let target_rate = pancetta_ft8::SAMPLE_RATE;
        let samples_12k: Vec<f32> = if spec.sample_rate != target_rate {
            info!(
                "Resampling from {} Hz to {} Hz",
                spec.sample_rate, target_rate
            );
            resample_linear(&mono_samples, spec.sample_rate, target_rate)
        } else {
            mono_samples
        };

        let total_samples = samples_12k.len();
        let duration_s = total_samples as f64 / target_rate as f64;
        info!(
            "Audio ready: {} samples ({:.2}s) at {} Hz",
            total_samples, duration_s, target_rate
        );

        // Create FT8 decoder
        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let window_size = pancetta_ft8::WINDOW_SAMPLES; // 151680 (12.64s @ 12 kHz)

        // Decode each 15-second slot worth of samples
        // FT8 windows overlap — try decoding from multiple offsets
        let mut all_decoded = Vec::new();
        let mut offset = 0usize;

        // Step by half a window (6.32s) to catch messages at slot boundaries
        let step = window_size / 2;

        while offset + window_size <= total_samples {
            let window = &samples_12k[offset..offset + window_size];
            match decoder.decode_window(window) {
                Ok(messages) => {
                    for msg in &messages {
                        let freq_hz = msg.frequency_offset;
                        let snr = msg.snr_db;
                        let dt = msg.time_offset;
                        let text = &msg.text;

                        // Print in WSJT-X style format
                        let slot_time = offset as f64 / target_rate as f64;
                        let mins = (slot_time / 60.0) as u32;
                        let secs = (slot_time % 60.0) as u32;
                        println!(
                            "{:02}:{:02}  {:>+4.0} {:>6.1} {:>+5.1}  {}",
                            mins, secs, snr, freq_hz, dt, text
                        );
                    }
                    all_decoded.extend(messages);
                }
                Err(e) => {
                    debug!("Decode error at offset {}: {}", offset, e);
                }
            }
            offset += step;
        }

        // Also try from offset 0 if we haven't covered it
        if total_samples >= window_size && step > 0 {
            // Already covered above
        }

        println!(
            "\n--- Decoded {} messages from {} ---",
            all_decoded.len(),
            wav_path.display()
        );

        Ok(())
    }

    // =========================================================================
    // Core pipeline: Audio → DSP → FT8 → TUI
    // =========================================================================

    /// Start the core pipeline with proper point-to-point channels.
    ///
    /// Creates direct crossbeam channels between components:
    ///   audio_tx → dsp_rx  (raw audio)
    ///   dsp_tx   → ft8_rx  (processed windows)
    ///   ft8_tx   → tui_rx  (decoded messages)
    async fn start_pipeline(&mut self) -> Result<()> {
        // Point-to-point channels for the data path
        let (audio_to_dsp_tx, audio_to_dsp_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (dsp_to_ft8_tx, dsp_to_ft8_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (ft8_to_tui_tx, ft8_to_tui_rx) =
            crossbeam_channel::unbounded::<pancetta_ft8::DecodedMessage>();

        // Also create message bus channels for control messages (hamlib, autonomous, etc.)
        let (_audio_bus_tx, _audio_bus_rx) =
            self.message_bus.create_channel(ComponentId::Audio).await?;
        let (_dsp_bus_tx, _dsp_bus_rx) =
            self.message_bus.create_channel(ComponentId::Dsp).await?;
        let (_ft8_bus_tx, _ft8_bus_rx) =
            self.message_bus.create_channel(ComponentId::Ft8Decoder).await?;
        let (_tui_bus_tx, tui_bus_rx) =
            self.message_bus.create_channel(ComponentId::Tui).await?;

        // --- Audio component ---
        self.start_audio_pipeline(audio_to_dsp_tx).await?;

        // --- DSP component ---
        self.start_dsp_pipeline(audio_to_dsp_rx, dsp_to_ft8_tx)
            .await?;

        // --- FT8 decoder component ---
        self.start_ft8_pipeline(dsp_to_ft8_rx, ft8_to_tui_tx)
            .await?;

        // --- TUI component ---
        if !self.headless {
            self.start_tui_pipeline(ft8_to_tui_rx, tui_bus_rx)
                .await?;
        } else {
            // In headless mode, just drain decoded messages and log them
            let shutdown = self.shutdown_signal.clone();
            let handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Relaxed) {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(msg) => {
                            info!(
                                "Decoded: {} (SNR: {:.0}, freq: {:.1} Hz)",
                                msg.text, msg.snr_db, msg.frequency_offset
                            );
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }
                Ok(())
            });
            self.task_handles.push(handle);
        }

        Ok(())
    }

    /// Start audio component with point-to-point output channel
    async fn start_audio_pipeline(
        &mut self,
        audio_to_dsp_tx: crossbeam_channel::Sender<Vec<f32>>,
    ) -> Result<()> {
        if self.no_audio {
            info!("Audio processing disabled");
            return Ok(());
        }

        let span = span!(Level::INFO, "start_audio");
        let _enter = span.enter();

        let use_stub = std::env::var("PANCETTA_STUB_AUDIO").is_ok();

        if use_stub {
            info!("Starting audio component in STUB mode");

            let config = self.config.read().await;
            let sample_rate = config.audio.sample_rate;
            let buffer_size = config.audio.buffer_size as usize;
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            let handle = tokio::spawn(async move {
                let mut phase = 0.0f32;
                let frequency = 1500.0;
                let buffer_duration_ms =
                    (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
                let mut process_interval =
                    interval(Duration::from_millis(buffer_duration_ms.max(5)));

                while !shutdown.load(Ordering::Relaxed) {
                    process_interval.tick().await;

                    let mut samples = Vec::with_capacity(buffer_size);
                    for _ in 0..buffer_size {
                        let sample = 0.1 * phase.sin();
                        samples.push(sample);
                        phase += 2.0 * std::f32::consts::PI * frequency / sample_rate as f32;
                        if phase > 2.0 * std::f32::consts::PI {
                            phase -= 2.0 * std::f32::consts::PI;
                        }
                    }

                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }

                    if audio_to_dsp_tx.send(samples).is_err() {
                        break;
                    }
                }

                info!("Audio stub stopped");
                Ok(())
            });

            self.task_handles.push(handle);
        } else {
            info!("Starting audio component with real AudioManager");

            let config = self.config.read().await;
            let audio_config = AudioManagerConfig {
                input_device: Some(config.audio.input_device.clone()),
                output_device: Some(config.audio.output_device.clone()),
                sample_rate: config.audio.sample_rate,
                buffer_size: config.audio.buffer_size as usize,
                channels: config.audio.input_channels as u16,
                enable_monitoring: false,
                target_latency_ms: 1.0,
                input_gain_db: config.audio.levels.input_gain_db,
            };
            drop(config);

            let shutdown = self.shutdown_signal.clone();
            let last_timestamp = self.last_audio_timestamp.clone();

            // Audio thread sends samples via a tokio mpsc to an async relay
            let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(100);

            std::thread::spawn(move || {
                let mut audio_manager = match AudioManager::with_config(audio_config) {
                    Ok(manager) => manager,
                    Err(e) => {
                        error!("Failed to create AudioManager: {}", e);
                        return;
                    }
                };

                if let Err(e) = audio_manager.start() {
                    error!("Failed to start audio stream: {}", e);
                    return;
                }

                info!("AudioManager started in dedicated thread");

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    match audio_manager.process_audio() {
                        Ok(Some(samples)) => {
                            if result_tx.blocking_send(samples).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(e) => {
                            error!("Audio processing error: {}", e);
                        }
                    }
                }

                let _ = audio_manager.stop();
                info!("Audio manager thread stopped");
            });

            // Async relay: tokio mpsc → crossbeam point-to-point
            let handle = tokio::spawn(async move {
                while let Some(samples) = result_rx.recv().await {
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }

                    if audio_to_dsp_tx.send(samples).is_err() {
                        break;
                    }
                }

                info!("Audio relay task stopped");
                Ok(())
            });

            self.task_handles.push(handle);
        }

        info!("Audio component started");
        Ok(())
    }

    /// Start DSP pipeline with point-to-point channels
    async fn start_dsp_pipeline(
        &mut self,
        audio_rx: crossbeam_channel::Receiver<Vec<f32>>,
        dsp_to_ft8_tx: crossbeam_channel::Sender<Vec<f32>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();

        info!("Starting DSP component");

        // Create FT8-optimized DSP pipeline
        let (mut dsp_pipeline, dsp_input_tx, dsp_output_rx) =
            pancetta_dsp::factory::create_ft8_pipeline()?;

        let shutdown_input = self.shutdown_signal.clone();
        let shutdown_output = self.shutdown_signal.clone();
        let message_count = self.message_count.clone();

        let handle = tokio::spawn(async move {
            // Start the DSP pipeline
            let pipeline_task = tokio::spawn(async move {
                if let Err(e) = dsp_pipeline.start().await {
                    error!("DSP pipeline error: {}", e);
                }
            });

            // Input: read from audio point-to-point channel, feed DSP
            let input_task = tokio::spawn(async move {
                while !shutdown_input.load(Ordering::Relaxed) {
                    match audio_rx.try_recv() {
                        Ok(samples) => {
                            message_count.fetch_add(1, Ordering::Relaxed);
                            if let Err(e) = dsp_input_tx.send(samples) {
                                warn!("Failed to send samples to DSP: {}", e);
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }
            });

            // Output: accumulate DSP output into FT8-sized windows, send to FT8
            let output_task = tokio::spawn(async move {
                const FT8_WINDOW_SIZE: usize = 151680; // 12.64s at 12 kHz
                let mut ft8_buffer = Vec::with_capacity(FT8_WINDOW_SIZE);

                while !shutdown_output.load(Ordering::Relaxed) {
                    if let Ok(processed_samples) = dsp_output_rx.recv() {
                        ft8_buffer.extend_from_slice(&processed_samples);

                        while ft8_buffer.len() >= FT8_WINDOW_SIZE {
                            let window: Vec<f32> =
                                ft8_buffer.drain(..FT8_WINDOW_SIZE).collect();
                            if dsp_to_ft8_tx.send(window).is_err() {
                                return;
                            }
                            debug!("Sent FT8 window ({} samples) to decoder", FT8_WINDOW_SIZE);
                        }
                    }
                }
            });

            tokio::select! {
                _ = pipeline_task => {},
                _ = input_task => {},
                _ = output_task => {},
            }

            info!("DSP component stopped");
            Ok(())
        });

        self.task_handles.push(handle);
        info!("DSP component started");
        Ok(())
    }

    /// Start FT8 decoder with point-to-point channels
    async fn start_ft8_pipeline(
        &mut self,
        ft8_rx: crossbeam_channel::Receiver<Vec<f32>>,
        ft8_to_tui_tx: crossbeam_channel::Sender<pancetta_ft8::DecodedMessage>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();

        info!("Starting FT8 component");

        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let shutdown = self.shutdown_signal.clone();
        let last_decode_timestamp = self.last_decode_timestamp.clone();
        let message_bus = self.message_bus.clone();

        let handle = tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                match ft8_rx.try_recv() {
                    Ok(window) => {
                        match decoder.decode_window(&window) {
                            Ok(decoded_messages) => {
                                {
                                    let mut timestamp = last_decode_timestamp.write().await;
                                    *timestamp = Some(Instant::now());
                                }

                                for decoded_msg in decoded_messages {
                                    info!(
                                        "FT8 decoded: {} (SNR: {:.0}, freq: {:.1})",
                                        decoded_msg.text,
                                        decoded_msg.snr_db,
                                        decoded_msg.frequency_offset
                                    );

                                    // Send to TUI via point-to-point channel
                                    if ft8_to_tui_tx.send(decoded_msg.clone()).is_err() {
                                        warn!("TUI channel disconnected");
                                    }

                                    // Also send to Autonomous via message bus
                                    let auto_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Autonomous,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(auto_msg).await {
                                        debug!("Failed to send to Autonomous: {}", e);
                                    }

                                    // Send to QSO manager for state tracking and logging
                                    let qso_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Qso,
                                        MessageType::DecodedMessage(decoded_msg),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(qso_msg).await {
                                        debug!("Failed to send to QSO: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("FT8 decode error: {}", e);
                            }
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }

            info!("FT8 component stopped");
            Ok(())
        });

        self.task_handles.push(handle);
        info!("FT8 component started");
        Ok(())
    }

    /// Start TUI component with point-to-point decoded message channel
    async fn start_tui_pipeline(
        &mut self,
        ft8_to_tui_rx: crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>,
        tui_bus_rx: crossbeam_channel::Receiver<ComponentMessage>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_tui");
        let _enter = span.enter();

        info!("Starting TUI component");

        let config = self.config.clone();
        let shutdown = self.shutdown_signal.clone();

        // Create TUI message/command channels for the TuiRunner
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiMessage>();
        let (tui_cmd_tx, tui_cmd_rx) = crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiCommand>();

        // Task: relay decoded messages from point-to-point channel into TuiMessage channel
        let relay_shutdown = shutdown.clone();
        let tui_msg_tx_relay = tui_msg_tx.clone();
        let relay_handle = tokio::spawn(async move {
            while !relay_shutdown.load(Ordering::Relaxed) {
                match ft8_to_tui_rx.try_recv() {
                    Ok(decoded_msg) => {
                        let call_sign = decoded_msg.message.from_callsign.clone();
                        let grid_square = decoded_msg.message.grid_square.clone();

                        let tui_decoded = pancetta_tui::DecodedMessage {
                            timestamp: chrono::Utc::now(),
                            frequency: 14.074,
                            mode: "FT8".to_string(),
                            snr: decoded_msg.snr_db as i32,
                            delta_time: decoded_msg.time_offset as f32,
                            delta_freq: decoded_msg.frequency_offset as f32,
                            call_sign,
                            grid_square,
                            message: decoded_msg.text.clone(),
                            distance: None,
                            bearing: None,
                        };

                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::DecodedMessage(tui_decoded),
                        );
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }

                // Also drain control messages from the message bus
                match tui_bus_rx.try_recv() {
                    Ok(bus_msg) => {
                        match bus_msg.message_type {
                            MessageType::AutonomousStatus(ref status) => {
                                // Forward as status update for now
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: status.state.clone(),
                                    },
                                );
                            }
                            MessageType::RigControl(
                                crate::message_bus::RigControlMessage::FrequencyResponse {
                                    vfo,
                                    frequency,
                                },
                            ) => {
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::FrequencyUpdate {
                                        vfo,
                                        frequency,
                                    },
                                );
                            }
                            MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                callsign,
                                frequency,
                                spotter,
                                ..
                            }) => {
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::DxSpot {
                                        callsign,
                                        frequency,
                                        spotter,
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {}
                }
            }
            Ok(())
        });
        self.task_handles.push(relay_handle);

        // Task: relay TUI commands (e.g. SendMessage) to message bus as TransmitRequests
        let cmd_shutdown = self.shutdown_signal.clone();
        let cmd_message_bus = self.message_bus.clone();
        let cmd_handle = tokio::spawn(async move {
            while !cmd_shutdown.load(Ordering::Relaxed) {
                match tui_cmd_rx.try_recv() {
                    Ok(cmd) => {
                        match cmd {
                            pancetta_tui::tui_runner::TuiCommand::SendMessage { text } => {
                                info!("TUI SendMessage: '{}'", text);
                                let msg = ComponentMessage::new(
                                    ComponentId::Tui,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TransmitRequest {
                                        message_text: text,
                                        frequency_offset: 1500.0,
                                        qso_id: None,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = cmd_message_bus.send_message(msg).await {
                                    warn!("Failed to forward TUI command: {}", e);
                                }
                            }
                            pancetta_tui::tui_runner::TuiCommand::CallStation { callsign, frequency } => {
                                info!("TUI CallStation: {} at {} Hz", callsign, frequency);
                                let msg = ComponentMessage::new(
                                    ComponentId::Tui,
                                    ComponentId::Qso,
                                    MessageType::QsoMessage(crate::message_bus::QsoMessage::StartQso {
                                        callsign,
                                        frequency,
                                    }),
                                    Instant::now(),
                                );
                                if let Err(e) = cmd_message_bus.send_message(msg).await {
                                    warn!("Failed to forward CallStation command: {}", e);
                                }
                            }
                            _ => {
                                debug!("Unhandled TUI command: {:?}", cmd);
                            }
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
            Ok(())
        });
        self.task_handles.push(cmd_handle);

        // Run the TUI on a blocking task (it takes over the terminal)
        let tui_config_lock = config.read().await;
        let tui_config = pancetta_tui::Config {
            station: pancetta_tui::config::StationConfig {
                call_sign: tui_config_lock.station.callsign.clone(),
                grid_square: tui_config_lock.station.grid_square.clone(),
                power: tui_config_lock.station.power_watts,
                antenna: "Vertical".to_string(),
                rig: tui_config_lock.rig.model.clone(),
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
                device: Some(tui_config_lock.audio.input_device.clone()),
                sample_rate: tui_config_lock.audio.sample_rate,
                buffer_size: tui_config_lock.audio.buffer_size as usize,
                auto_gain: false,
                gain_level: tui_config_lock.audio.levels.input_gain_db,
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
        drop(tui_config_lock);

        // Start TUI runner in a blocking task so it can own the terminal
        let tui_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                pancetta_tui::tui_runner::run_tui_with_message_bus(
                    tui_config,
                    tui_msg_rx,
                    tui_cmd_tx,
                    shutdown,
                )
                .await
            })
        });

        // Wrap the JoinHandle<Result<()>> to match our task_handles type
        let tui_wrapper = tokio::spawn(async move {
            match tui_handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("TUI task panicked: {}", e)),
            }
        });
        self.task_handles.push(tui_wrapper);

        info!("TUI component started");
        Ok(())
    }

    // =========================================================================
    // Auxiliary components (unchanged architecture, but messages routed via bus)
    // =========================================================================

    /// Initialize metrics collection
    async fn init_metrics(&self) -> Result<()> {
        info!("Initializing metrics on port {}", self.metrics_port);

        #[cfg(feature = "prometheus")]
        {
            use metrics_exporter_prometheus::PrometheusBuilder;

            let builder = PrometheusBuilder::new()
                .with_http_listener(([0, 0, 0, 0], self.metrics_port));

            builder
                .install()
                .context("Failed to install Prometheus metrics exporter")?;

            info!("Metrics server started on port {}", self.metrics_port);
        }

        Ok(())
    }

    /// Start Hamlib component for rig control
    #[cfg(feature = "pancetta-hamlib")]
    async fn start_hamlib_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_hamlib");
        let _enter = span.enter();

        info!("Starting Hamlib component");

        let (hamlib_tx, hamlib_rx) = self.message_bus.create_channel(ComponentId::Hamlib).await?;
        let message_bus = self.message_bus.clone();

        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                let use_mock = std::env::var("PANCETTA_MOCK_RIG")
                    .map(|v| v.to_lowercase() == "true" || v == "1")
                    .unwrap_or(true);

                let rig: Box<dyn pancetta_hamlib::RigControl + Send + Sync> = if use_mock {
                    info!("Using mock rig");
                    Box::new(pancetta_hamlib::MockRig::default())
                } else {
                    info!("Using rigctld client");
                    let host = std::env::var("RIGCTLD_HOST")
                        .unwrap_or_else(|_| "127.0.0.1".to_string());
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

                match rig.connect().await {
                    Ok(_) => info!("Rig connected successfully"),
                    Err(e) => {
                        error!("Failed to connect to rig: {}. Continuing without.", e);
                    }
                }

                // Polling task
                let rig_poll = Arc::new(rig);
                let rig_for_polling = Arc::clone(&rig_poll);
                let shutdown_for_polling = shutdown.clone();

                tokio::spawn(async move {
                    let mut poll_interval = interval(Duration::from_millis(500));

                    while !shutdown_for_polling.load(Ordering::Relaxed) {
                        poll_interval.tick().await;

                        if let Ok(status) = rig_for_polling.get_status().await {
                            if status.connection_state
                                == pancetta_hamlib::ConnectionState::Connected
                            {
                                if let Ok(freq) = rig_for_polling
                                    .get_frequency(pancetta_hamlib::Vfo::Current)
                                    .await
                                {
                                    let message = ComponentMessage::new(
                                        ComponentId::Hamlib,
                                        ComponentId::Tui,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                                vfo: 0,
                                                frequency: freq,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    let _ = message_bus.send_message(message).await;
                                }
                            }
                        }
                    }
                });

                // Process messages
                while !shutdown.load(Ordering::Relaxed) {
                    match hamlib_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::RigControl(ref rig_msg) = message.message_type {
                                match rig_msg {
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    } => {
                                        let vfo_enum = if *vfo == 0 {
                                            pancetta_hamlib::Vfo::A
                                        } else {
                                            pancetta_hamlib::Vfo::B
                                        };
                                        if let Err(e) =
                                            rig_poll.set_frequency(vfo_enum, *frequency).await
                                        {
                                            error!("Failed to set frequency: {}", e);
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::SetPtt { state } => {
                                        let ptt = if *state {
                                            pancetta_hamlib::PttState::On
                                        } else {
                                            pancetta_hamlib::PttState::Off
                                        };
                                        if let Err(e) = rig_poll
                                            .set_ptt(pancetta_hamlib::Vfo::Current, ptt)
                                            .await
                                        {
                                            error!("Failed to set PTT: {}", e);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
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
    ///
    /// Wires decoded FT8 messages into the QSO manager for state tracking,
    /// auto-logging to SQLite at `~/.pancetta/qso.db`, and duplicate detection.
    async fn start_qso_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_qso");
        let _enter = span.enter();

        info!("Starting QSO component");

        let (_qso_tx, qso_rx) = self.message_bus.create_channel(ComponentId::Qso).await?;
        let message_bus = self.message_bus.clone();

        // Read station config for callsign/grid
        let config = self.config.read().await;
        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };
        drop(config);

        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{QsoManager, QsoManagerConfig, QsoLogger, LoggerConfig};

                let qso_config = QsoManagerConfig {
                    our_callsign: our_callsign.clone(),
                    our_grid: our_grid.clone(),
                    ..Default::default()
                };

                let qso_manager = QsoManager::new(qso_config);
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }

                // Initialize QSO logger with SQLite database at ~/.pancetta/qso.db
                let db_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qso.db");
                let logger_config = LoggerConfig {
                    database_path: db_path.clone(),
                    ..Default::default()
                };

                let logger = match QsoLogger::new(logger_config, qso_manager.clone()).await {
                    Ok(l) => {
                        info!("QSO logger initialized with database at {:?}", db_path);
                        if let Err(e) = l.start().await {
                            warn!("QSO logger background tasks failed to start: {}", e);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        warn!("Failed to initialize QSO logger (continuing without): {}", e);
                        None
                    }
                };

                info!("QSO component ready (callsign={}, grid={:?})", our_callsign, our_grid);

                while !shutdown.load(Ordering::Relaxed) {
                    match qso_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                // Decoded FT8 messages forwarded from the decoder
                                MessageType::DecodedMessage(ref decoded_msg) => {
                                    let raw_text = decoded_msg.text.clone();
                                    let frequency = decoded_msg.frequency_offset as f64;
                                    let snr = decoded_msg.snr_db as f32;

                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(&raw_text, &our_callsign) {
                                        Ok(msg_type) => {
                                            if let Err(e) = qso_manager.process_message(
                                                msg_type,
                                                raw_text.clone(),
                                                frequency,
                                                Some(snr),
                                            ).await {
                                                debug!("QSO process_message error: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            debug!("Could not parse FT8 message '{}': {}", raw_text, e);
                                        }
                                    }
                                }

                                // QSO control messages (start QSO, log, etc.)
                                MessageType::QsoMessage(qso_msg) => {
                                    match qso_msg {
                                        crate::message_bus::QsoMessage::StartQso {
                                            callsign,
                                            frequency,
                                        } => {
                                            info!("Starting QSO with {} on {} Hz", callsign, frequency);
                                            match qso_manager.respond_to_cq(
                                                callsign.clone(),
                                                frequency as f64,
                                            ).await {
                                                Ok(qso_id) => {
                                                    info!("QSO started with {}: {}", callsign, qso_id);
                                                    // Send grid reply as TX request
                                                    let grid = our_grid.as_deref().unwrap_or("AA00");
                                                    let reply = format!("{} {} {}", callsign, our_callsign, grid);
                                                    let tx_msg = ComponentMessage::new(
                                                        ComponentId::Qso,
                                                        ComponentId::Ft8Transmitter,
                                                        MessageType::TransmitRequest {
                                                            message_text: reply,
                                                            frequency_offset: frequency as f64,
                                                            qso_id: Some(qso_id.to_string()),
                                                        },
                                                        Instant::now(),
                                                    );
                                                    if let Err(e) = message_bus.send_message(tx_msg).await {
                                                        warn!("Failed to send QSO TX request: {}", e);
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!("Failed to start QSO with {}: {}", callsign, e);
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::LogQso { qso_data } => {
                                            debug!("Manual log QSO: {}", qso_data);
                                        }
                                        _ => {}
                                    }
                                }

                                _ => {}
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
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

    /// Start DX cluster component for real-time spot monitoring
    async fn start_dx_cluster_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.dx_cluster.enabled {
            info!("DX cluster disabled in configuration");
            drop(config);
            // Still create channel so message bus doesn't complain
            let _ = self.message_bus.create_channel(ComponentId::DxCluster).await?;
            return Ok(());
        }

        let cluster_hostname = config.network.dx_cluster.servers
            .first()
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string());
        let cluster_port = config.network.dx_cluster.servers
            .first()
            .map(|s| s.port)
            .unwrap_or(23);
        let our_callsign = config.station.callsign.clone();
        drop(config);

        info!("Starting DX cluster component ({}:{})", cluster_hostname, cluster_port);

        let (_dx_tx, _dx_rx) = self.message_bus.create_channel(ComponentId::DxCluster).await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::cluster::DxClusterClient;

                let mut client = DxClusterClient::new();

                match client.connect().await {
                    Ok(_) => {
                        info!("Connected to DX cluster");

                        // Login with our callsign
                        if let Err(e) = client.login().await {
                            warn!("DX cluster login failed: {}. Continuing without.", e);
                        }

                        // Monitor spots and forward to TUI
                        while !shutdown.load(Ordering::Relaxed) {
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                client.receive_spot(),
                            ).await {
                                Ok(Some(spot)) => {
                                    debug!("DX spot: {} on {} Hz by {}", spot.callsign, spot.frequency, spot.spotter);

                                    let msg = ComponentMessage::new(
                                        ComponentId::DxCluster,
                                        ComponentId::Tui,
                                        MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                            callsign: spot.callsign,
                                            frequency: spot.frequency,
                                            spotter: spot.spotter,
                                            comment: spot.comment.unwrap_or_default(),
                                        }),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        debug!("Failed to forward DX spot: {}", e);
                                    }
                                }
                                Ok(None) => {
                                    // No spot available, yield
                                    tokio::task::yield_now().await;
                                }
                                Err(_) => {
                                    // Timeout — normal, just loop
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to DX cluster: {}. Feature disabled.", e);
                    }
                }

                info!("DX cluster component stopped");
                Ok(())
            })
        };

        self.task_handles.push(dx_handle);
        info!("DX cluster component started");
        Ok(())
    }

    /// Start FT8 transmitter component
    async fn start_transmitter_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_transmitter");
        let _enter = span.enter();

        info!("Starting FT8 transmitter component");

        let (_tx_sender, tx_rx) = self.message_bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await?;
        let message_bus = self.message_bus.clone();

        let tx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                info!("FT8 transmitter component ready");

                let mut encoder = Ft8Encoder::new();
                let mut modulator = match Ft8Modulator::new_default() {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Failed to create modulator: {}", e);
                        return Err(anyhow::anyhow!("Modulator init failed: {}", e));
                    }
                };

                while !shutdown.load(Ordering::Relaxed) {
                    match tx_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::TransmitRequest {
                                message_text,
                                frequency_offset,
                                qso_id,
                            } = message.message_type
                            {
                                info!(
                                    "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                    message_text, frequency_offset, qso_id
                                );

                                // Encode the message to FT8 symbols
                                let encode_result = encoder.encode_message(&message_text, None);
                                let (success, duration_ms) = match encode_result {
                                    Ok(symbols) => {
                                        // Modulate symbols to audio samples
                                        match modulator.modulate_symbols(&symbols, frequency_offset) {
                                            Ok(samples) => {
                                                let duration = (samples.len() as f64 / 12000.0 * 1000.0) as u64;
                                                info!(
                                                    "Encoded '{}' → {} symbols → {} audio samples ({} ms)",
                                                    message_text, symbols.len(), samples.len(), duration
                                                );
                                                // TODO: Route samples to audio output device
                                                // For now, samples are generated but not played
                                                (true, duration)
                                            }
                                            Err(e) => {
                                                warn!("Modulation failed for '{}': {}", message_text, e);
                                                (false, 0)
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Encoding failed for '{}': {}", message_text, e);
                                        (false, 0)
                                    }
                                };

                                let complete_msg = ComponentMessage::new(
                                    ComponentId::Ft8Transmitter,
                                    ComponentId::Autonomous,
                                    MessageType::TransmitComplete {
                                        success,
                                        message_text,
                                        duration_ms,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(complete_msg).await {
                                    warn!("Failed to send TransmitComplete: {}", e);
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::task::yield_now().await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                info!("FT8 transmitter component stopped");
                Ok(())
            })
        };

        self.task_handles.push(tx_handle);
        info!("FT8 transmitter component started");
        Ok(())
    }

    /// Start autonomous operator component
    async fn start_autonomous_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_autonomous");
        let _enter = span.enter();

        let config = self.config.read().await;
        let auto_config_enabled = config.autonomous.enabled;

        if !auto_config_enabled {
            info!("Autonomous operator disabled in configuration");
            drop(config);
            let _ = self.message_bus
                .create_channel(ComponentId::Autonomous)
                .await?;
            return Ok(());
        }

        info!("Starting autonomous operator component");

        let qso_auto_config = pancetta_qso::AutonomousConfig {
            enabled: config.autonomous.enabled,
            slot_parity: match config.autonomous.slot_parity {
                pancetta_config::autonomous::SlotParitySetting::Even => {
                    pancetta_qso::SlotParityConfig::Even
                }
                pancetta_config::autonomous::SlotParitySetting::Odd => {
                    pancetta_qso::SlotParityConfig::Odd
                }
                pancetta_config::autonomous::SlotParitySetting::Auto => {
                    pancetta_qso::SlotParityConfig::Auto
                }
            },
            cq_after_idle_cycles: config.autonomous.cq_after_idle_cycles,
            max_concurrent_qsos: config.autonomous.max_concurrent_qsos,
            tx_offset_hz: config.autonomous.tx_offset_hz,
            min_dx_score: config.autonomous.min_dx_score,
            cq_direction: config.autonomous.cq_direction.clone(),
            listen_cycle: pancetta_qso::autonomous::ListenCycleConfig {
                initial_interval: config.autonomous.listen_cycle.initial_interval,
                backoff_interval: config.autonomous.listen_cycle.backoff_interval,
                collision_interval: config.autonomous.listen_cycle.collision_interval,
                backoff_threshold: config.autonomous.listen_cycle.backoff_threshold,
            },
            band_hopping: pancetta_qso::autonomous::BandHoppingConfig {
                enabled: config.autonomous.band_hopping.enabled,
                hop_threshold: config.autonomous.band_hopping.hop_threshold,
                bands: config
                    .autonomous
                    .band_hopping
                    .bands
                    .iter()
                    .map(|b| pancetta_qso::autonomous::BandEntry {
                        dial_frequency: b.dial_frequency,
                        band_name: b.band_name.clone(),
                        priority: b.priority,
                    })
                    .collect(),
            },
        };

        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };
        drop(config);

        let operator = std::sync::Arc::new(tokio::sync::Mutex::new(
            pancetta_qso::AutonomousOperator::new(qso_auto_config, our_callsign, our_grid),
        ));

        let evaluator: std::sync::Arc<dyn pancetta_qso::DxEvaluator> =
            std::sync::Arc::new(pancetta_qso::NullDxEvaluator);

        let (_auto_tx, auto_rx) = self.message_bus
            .create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();

        let auto_handle = {
            let shutdown = self.shutdown_signal.clone();
            let operator = operator.clone();
            let evaluator = evaluator.clone();

            tokio::spawn(async move {
                info!("Autonomous operator started");

                let mut slot_messages: Vec<pancetta_qso::DecodedMessageInfo> = Vec::new();
                let mut slot_interval = tokio::time::interval(Duration::from_secs(15));

                loop {
                    tokio::select! {
                        _ = slot_interval.tick() => {
                            let mut op = operator.lock().await;
                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());
                            slot_messages.clear();
                            let actions = op.decide();
                            drop(op);

                            for action in actions {
                                match action {
                                    pancetta_qso::OperatorAction::Transmit {
                                        message_text,
                                        frequency_offset,
                                        qso_id,
                                    } => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text,
                                                frequency_offset,
                                                qso_id,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send TransmitRequest: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::ChangeBand { dial_frequency } => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Hamlib,
                                            MessageType::RigControl(
                                                crate::message_bus::RigControlMessage::SetFrequency {
                                                    vfo: 0,
                                                    frequency: dial_frequency,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send ChangeBand: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::StatusUpdate(status) => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Tui,
                                            MessageType::AutonomousStatus(
                                                crate::message_bus::AutonomousStatusData {
                                                    enabled: status.enabled,
                                                    state: status.state,
                                                    slot_parity: status.slot_parity,
                                                    listen_counter: status.listen_counter,
                                                    active_qsos: status.active_qsos,
                                                    max_qsos: status.max_qsos,
                                                    idle_cycles: status.idle_cycles,
                                                    band_name: status.band_name,
                                                    tx_offset_hz: status.tx_offset_hz,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send AutonomousStatus: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::Listen
                                    | pancetta_qso::OperatorAction::CollisionListen => {}
                                    pancetta_qso::OperatorAction::FrequencyShift { new_offset_hz } => {
                                        info!("Autonomous: TX offset shifted to {:.0} Hz", new_offset_hz);
                                    }
                                }
                            }
                        }

                        _ = async {
                            loop {
                                match auto_rx.try_recv() {
                                    Ok(message) => {
                                        if let MessageType::DecodedMessage(decoded_msg) = message.message_type {
                                            slot_messages.push(pancetta_qso::DecodedMessageInfo {
                                                callsign: decoded_msg.message.from_callsign.clone(),
                                                frequency_hz: decoded_msg.frequency_offset,
                                                snr: decoded_msg.snr_db as i32,
                                                message_text: decoded_msg.text.clone(),
                                            });
                                        }
                                    }
                                    Err(crossbeam_channel::TryRecvError::Empty) => {
                                        tokio::task::yield_now().await;
                                        break;
                                    }
                                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                                }
                            }
                        } => {}
                    }

                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                }

                info!("Autonomous operator stopped");
                Ok(())
            })
        };

        self.task_handles.push(auto_handle);
        info!("Autonomous operator component started");
        Ok(())
    }

    // =========================================================================
    // Coordinator management
    // =========================================================================

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

                    let health_status = message_bus.get_component_health().await;
                    for health in health_status {
                        if !health.is_healthy {
                            warn!(
                                "Component {:?} is unhealthy: {} errors",
                                health.component_id, health.error_count
                            );
                        }
                    }
                }

                Ok(())
            })
        };

        // Configuration hot-reload task
        let config_handle = {
            let _config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
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
    async fn log_performance_stats(&self) {
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
        self.shutdown_signal.store(true, Ordering::Relaxed);

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
        info!(
            "Graceful shutdown completed in {:.2}s",
            shutdown_duration.as_secs_f64()
        );

        Ok(())
    }
}

/// Simple linear resampler for WAV playback mode.
///
/// This is a basic interpolation resampler. For real-time use, the DSP pipeline's
/// high-quality SINC resampler is preferred.
fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return input.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = (input.len() as f64 / ratio) as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_idx = i as f64 * ratio;
        let idx0 = src_idx as usize;
        let frac = src_idx - idx0 as f64;

        let sample = if idx0 + 1 < input.len() {
            input[idx0] * (1.0 - frac as f32) + input[idx0 + 1] * frac as f32
        } else if idx0 < input.len() {
            input[idx0]
        } else {
            0.0
        };

        output.push(sample);
    }

    output
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
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            shutdown,
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
            eprintln!("Skipping WAV playback test: fixture not found at {:?}", wav_path);
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
            shutdown,
        )
        .await
        .expect("coordinator creation should succeed");

        // run_wav_playback exits after decoding — should not error
        let result = coordinator.run().await;
        assert!(result.is_ok(), "WAV playback should succeed: {:?}", result.err());
    }
}
