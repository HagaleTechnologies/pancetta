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
use geographiclib_rs::InverseGeodesic;
use pancetta_audio::{AudioManager, AudioManagerConfig};
use pancetta_config::Config;
use pancetta_dsp::DspPipeline;
use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, span, warn, Level};
use uuid::Uuid;

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

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

    /// Cached station lookup for priority scoring (shared between QSO and autonomous components).
    cached_lookup: std::sync::Arc<crate::priority_evaluator::CachedStationLookup>,

    /// cqdx.io integration bridge (None = degraded mode).
    cqdx_bridge: Option<std::sync::Arc<crate::cqdx_bridge::CqdxBridge>>,

    /// Sender for waterfall data to the autonomous operator.
    waterfall_to_auto_tx: Option<crossbeam_channel::Sender<Vec<Vec<f32>>>>,

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

/// Criticality level of a component — determines shutdown behavior on failure
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
        ComponentId::Audio => "Audio disconnected — no RX/TX until reconnected",
        ComponentId::Hamlib => "Rig control lost — PTT safety defaulting to OFF",
        ComponentId::DxCluster => "DX cluster disconnected — continuing without spots",
        ComponentId::Ft8Decoder => "FT8 decoder crashed — no decoding until restart",
        ComponentId::Dsp => "DSP pipeline failed — audio processing halted",
        ComponentId::PskReporter => "PSKReporter upload failed — spots not being reported",
        ComponentId::Qso => "QSO manager failed — contact logging unavailable",
        ComponentId::Ft8Transmitter => "FT8 transmitter failed — TX disabled",
        ComponentId::Autonomous => "Autonomous operator failed — manual operation only",
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
            cached_lookup: std::sync::Arc::new(crate::priority_evaluator::CachedStationLookup::new()),
            cqdx_bridge: None,
            waterfall_to_auto_tx: None,
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
        warn!("Hamlib feature is disabled — PTT safety watchdog is not active. Transmit at your own risk.");
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
                        let _poller_handle = bridge.spawn_spot_poller(
                            self.shutdown_signal.clone(),
                            self.last_decode_timestamp.clone(),
                            None,
                            None,
                        );
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

    // =========================================================================
    // WAV playback mode
    // =========================================================================

    /// Run WAV playback mode: read file, decode, print results, exit.
    async fn run_wav_playback(&self, wav_path: PathBuf) -> Result<()> {
        info!("WAV playback mode: {}", wav_path.display());

        // Read WAV file
        let reader = hound::WavReader::open(&wav_path).map_err(|e| {
            anyhow::anyhow!("Failed to open WAV file {}: {}", wav_path.display(), e)
        })?;

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
        let (waterfall_tx, waterfall_rx) = crossbeam_channel::unbounded::<Vec<Vec<f32>>>();

        // Also create message bus channels for control messages (hamlib, autonomous, etc.)
        let (_audio_bus_tx, _audio_bus_rx) =
            self.message_bus.create_channel(ComponentId::Audio).await?;
        let (_dsp_bus_tx, _dsp_bus_rx) = self.message_bus.create_channel(ComponentId::Dsp).await?;
        let (_ft8_bus_tx, _ft8_bus_rx) = self
            .message_bus
            .create_channel(ComponentId::Ft8Decoder)
            .await?;
        let (_tui_bus_tx, tui_bus_rx) = self.message_bus.create_channel(ComponentId::Tui).await?;

        // --- Audio component ---
        self.start_audio_pipeline(audio_to_dsp_tx).await?;

        // --- DSP component ---
        self.start_dsp_pipeline(audio_to_dsp_rx, dsp_to_ft8_tx, waterfall_tx.clone())
            .await?;

        // --- FT8 decoder component ---
        self.start_ft8_pipeline(dsp_to_ft8_rx, ft8_to_tui_tx, waterfall_tx)
            .await?;

        // --- TUI component ---
        if !self.headless {
            self.start_tui_pipeline(ft8_to_tui_rx, tui_bus_rx, waterfall_rx.clone())
                .await?;
        } else {
            // In headless mode, just drain decoded messages and log them
            let shutdown = self.shutdown_signal.clone();
            let handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
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
            self.named_task_handles.push((ComponentId::Tui, handle));
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
                let buffer_duration_ms = (buffer_size as f64 * 1000.0 / sample_rate as f64) as u64;
                let mut process_interval =
                    interval(Duration::from_millis(buffer_duration_ms.max(5)));

                while !shutdown.load(Ordering::Acquire) {
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

            self.named_task_handles.push((ComponentId::Audio, handle));
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
                    if shutdown.load(Ordering::Acquire) {
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
                let mut relay_count: u64 = 0;
                while let Some(samples) = result_rx.recv().await {
                    {
                        let mut timestamp = last_timestamp.write().await;
                        *timestamp = Some(Instant::now());
                    }

                    let len = samples.len();
                    if audio_to_dsp_tx.send(samples).is_err() {
                        info!("Audio relay: DSP channel closed after {} sends", relay_count);
                        break;
                    }
                    relay_count += 1;
                    if relay_count == 1 {
                        info!("Audio relay: first batch sent ({} samples)", len);
                    } else if relay_count % 1000 == 0 {
                        info!("Audio relay: {} batches sent so far", relay_count);
                    }
                }

                info!("Audio relay task stopped (total: {} batches)", relay_count);
                Ok(())
            });

            self.named_task_handles.push((ComponentId::Audio, handle));
        }

        info!("Audio component started");
        Ok(())
    }

    /// Start DSP pipeline with point-to-point channels
    ///
    /// Simple direct pipeline: resample 48kHz→12kHz on a dedicated thread,
    /// accumulate FT8-sized windows, and send to the decoder.
    async fn start_dsp_pipeline(
        &mut self,
        audio_rx: crossbeam_channel::Receiver<Vec<f32>>,
        dsp_to_ft8_tx: crossbeam_channel::Sender<Vec<f32>>,
        live_waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_dsp");
        let _enter = span.enter();

        info!("Starting DSP component");

        let shutdown = self.shutdown_signal.clone();
        let message_count = self.message_count.clone();

        let config = self.config.read().await;
        let input_rate = config.audio.sample_rate;
        let input_channels = config.audio.input_channels as u16;
        drop(config);

        let handle = tokio::task::spawn_blocking(move || {
            // FT8 timing: transmissions start at 0/15/30/45 second marks.
            // We need 12.64 seconds of audio at 12kHz = 151,680 samples per window.
            // We align window capture to UTC 15-second boundaries for best decode.
            let decimation_factor = (input_rate / 12000) as usize;
            const FT8_SAMPLE_RATE: usize = 12000;
            const FT8_WINDOW_SECONDS: f64 = 12.64;
            const FT8_WINDOW_SAMPLES: usize = (FT8_SAMPLE_RATE as f64 * FT8_WINDOW_SECONDS) as usize; // 151,680

            // FIR low-pass filter for anti-aliased decimation.
            // 65-tap Kaiser-windowed sinc (beta=8, ~80dB stopband attenuation).
            // Cutoff at 0.125 * Nyquist = 6kHz (= 12kHz/2, the decimated Nyquist).
            let fir_len = decimation_factor * 16 + 1; // 65 taps for factor=4
            let beta = 8.0f32; // Kaiser beta for ~80dB stopband
            let fir_coeffs: Vec<f32> = (0..fir_len)
                .map(|i| {
                    let n = i as f32 - (fir_len - 1) as f32 / 2.0;
                    let cutoff = 1.0 / (2.0 * decimation_factor as f32);
                    // Windowed sinc
                    let sinc = if n.abs() < 1e-6 {
                        2.0 * cutoff
                    } else {
                        (2.0 * std::f32::consts::PI * cutoff * n).sin() / (std::f32::consts::PI * n)
                    };
                    // Kaiser window: I0(beta * sqrt(1 - (2i/(N-1) - 1)^2)) / I0(beta)
                    let m = (fir_len - 1) as f32;
                    let x = 2.0 * i as f32 / m - 1.0;
                    let arg = beta * (1.0 - x * x).max(0.0).sqrt();
                    // Approximate I0 (modified Bessel) with series expansion
                    let i0 = |v: f32| -> f32 {
                        let mut sum = 1.0f32;
                        let mut term = 1.0f32;
                        for k in 1..20 {
                            term *= (v / (2.0 * k as f32)) * (v / (2.0 * k as f32));
                            sum += term;
                            if term < 1e-10 { break; }
                        }
                        sum
                    };
                    let window = i0(arg) / i0(beta);
                    sinc * window
                })
                .collect();
            // Normalize filter
            let fir_sum: f32 = fir_coeffs.iter().sum();
            let fir_coeffs: Vec<f32> = fir_coeffs.iter().map(|c| c / fir_sum).collect();

            let mut fir_buffer: Vec<f32> = vec![0.0; fir_len];
            let mut fir_pos: usize = 0;
            let mut decimate_counter: usize = 0;

            let mut ft8_buffer: Vec<f32> = Vec::with_capacity(FT8_WINDOW_SAMPLES * 2);
            let mut window_count: u64 = 0;
            let mut batch_count: u64 = 0;
            let _waiting_for_boundary = true;

            // Live waterfall state
            let mut last_live_wf_samples: usize = 0;
            let mut live_wf_planner = rustfft::FftPlanner::<f32>::new();
            let live_wf_fft = live_wf_planner.plan_fft_forward(2048);

            info!(
                "DSP: {}Hz/{}ch → {}Hz mono (decimate {}:1, {}-tap FIR), window={}",
                input_rate, input_channels, FT8_SAMPLE_RATE, decimation_factor, fir_len, FT8_WINDOW_SAMPLES
            );

            // Continuously capture audio — don't wait for boundaries.
            // FT8 has both even (0/30s) and odd (15/45s) time slots.
            // We send overlapping windows: one at each 15-second mark.
            // The decoder handles time alignment internally via Costas sync.
            let mut next_window_time = {
                let now = chrono::Utc::now();
                let secs = now.timestamp() % 15;
                // Next 15-second boundary
                let wait_secs = if secs == 0 { 0 } else { 15 - secs };
                now + chrono::Duration::seconds(wait_secs)
            };
            info!("DSP: first window at {}", next_window_time.format("%H:%M:%S"));

            while !shutdown.load(Ordering::Acquire) {

                match audio_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(samples) => {
                        message_count.fetch_add(1, Ordering::Relaxed);
                        batch_count += 1;

                        // Extract mono from interleaved multi-channel.
                        // Use left channel only (channel 0) to avoid phase cancellation
                        // that can occur when averaging L+R from USB audio codecs.
                        let mono: Vec<f32> = if input_channels > 1 {
                            samples
                                .chunks(input_channels as usize)
                                .map(|ch| ch[0])
                                .collect()
                        } else {
                            samples
                        };

                        // Anti-aliased decimation: FIR low-pass + downsample
                        for &sample in &mono {
                            fir_buffer[fir_pos] = sample;
                            fir_pos = (fir_pos + 1) % fir_len;
                            decimate_counter += 1;

                            if decimate_counter >= decimation_factor {
                                decimate_counter = 0;
                                // Apply FIR filter (convolution)
                                let mut sum = 0.0f32;
                                for (j, &coeff) in fir_coeffs.iter().enumerate() {
                                    let idx = (fir_pos + j) % fir_len;
                                    sum += fir_buffer[idx] * coeff;
                                }
                                ft8_buffer.push(sum);
                            }
                        }

                        // Live waterfall: emit one spectrum row per second using rustfft.
                        // We keep a simple sample counter to trigger every ~1 second.
                        const LIVE_WF_INTERVAL: usize = 12000; // 1 second at 12kHz
                        const LIVE_WF_FFT_SIZE: usize = 2048;
                        if ft8_buffer.len() >= LIVE_WF_FFT_SIZE {
                            let samples_since_last = ft8_buffer.len() - last_live_wf_samples;
                            if samples_since_last >= LIVE_WF_INTERVAL {
                                last_live_wf_samples = ft8_buffer.len();

                                let wf_start = ft8_buffer.len() - LIVE_WF_FFT_SIZE;
                                let wf_slice = &ft8_buffer[wf_start..];

                                // Use rustfft for a quick spectrum
                                let mut input: Vec<rustfft::num_complex::Complex<f32>> = wf_slice
                                    .iter()
                                    .enumerate()
                                    .map(|(i, &s)| {
                                        // Apply Hann window
                                        let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / LIVE_WF_FFT_SIZE as f32).cos());
                                        rustfft::num_complex::Complex::new(s * w, 0.0)
                                    })
                                    .collect();
                                live_wf_fft.process(&mut input);

                                // Extract 0–3000 Hz bins and convert to dB
                                let freq_res = FT8_SAMPLE_RATE as f32 / LIVE_WF_FFT_SIZE as f32;
                                let bin_end = (3000.0 / freq_res) as usize;
                                let bin_end = bin_end.min(LIVE_WF_FFT_SIZE / 2);

                                let powers: Vec<f32> = (0..=bin_end)
                                    .map(|i| 10.0 * (input[i].norm_sqr() / LIVE_WF_FFT_SIZE as f32 + 1e-12).log10())
                                    .collect();

                                let min_p = powers.iter().cloned().fold(f32::MAX, f32::min);
                                let max_p = powers.iter().cloned().fold(f32::MIN, f32::max);
                                let range = (max_p - min_p).max(1.0);
                                let row: Vec<f32> = powers.iter().map(|&p| (p - min_p) / range).collect();
                                let _ = live_waterfall_tx.try_send(vec![row]);
                            }
                        }

                        // Send FT8 window when we have enough samples AND
                        // we've reached the next 15-second boundary
                        let now = chrono::Utc::now();
                        if ft8_buffer.len() >= FT8_WINDOW_SAMPLES && now >= next_window_time {
                            // Take the most recent FT8_WINDOW_SAMPLES from the buffer
                            let start = ft8_buffer.len() - FT8_WINDOW_SAMPLES;
                            let window: Vec<f32> = ft8_buffer[start..].to_vec();
                            // Keep some overlap for the next window (retain last 1s worth)
                            let keep = FT8_SAMPLE_RATE; // 12000 samples = 1 second
                            if ft8_buffer.len() > keep {
                                ft8_buffer.drain(..ft8_buffer.len() - keep);
                                last_live_wf_samples = ft8_buffer.len();
                            }
                            window_count += 1;
                            let rms = (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt();
                            info!("DSP: FT8 window #{} (RMS={:.4}) at {}",
                                window_count, rms, now.format("%H:%M:%S.%3f"));
                            if dsp_to_ft8_tx.send(window).is_err() {
                                info!("DSP: FT8 channel closed");
                                return Ok(());
                            }
                            // Schedule next window at the next 15-second boundary
                            next_window_time = next_window_time + chrono::Duration::seconds(15);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!("DSP: audio channel disconnected after {} batches", batch_count);
                        break;
                    }
                }
            }

            info!("DSP stopped ({} batches, {} windows sent)", batch_count, window_count);
            Ok(())
        });

        self.named_task_handles.push((ComponentId::Dsp, handle));
        info!("DSP component started");
        Ok(())
    }

    /// Start FT8 decoder with point-to-point channels
    async fn start_ft8_pipeline(
        &mut self,
        ft8_rx: crossbeam_channel::Receiver<Vec<f32>>,
        ft8_to_tui_tx: crossbeam_channel::Sender<pancetta_ft8::DecodedMessage>,
        waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();

        info!("Starting FT8 component");

        let ft8_config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(ft8_config)?;

        let shutdown = self.shutdown_signal.clone();
        let last_decode_timestamp = self.last_decode_timestamp.clone();
        let message_bus = self.message_bus.clone();
        let self_waterfall_to_auto_tx = self.waterfall_to_auto_tx.clone();

        // Run FT8 decoder on a dedicated thread to avoid tokio starvation
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            info!("FT8 decoder thread started");

            while !shutdown.load(Ordering::Acquire) {
                match ft8_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(window) => {
                        info!("FT8 decoder: received window ({} samples)", window.len());

                        // Generate waterfall data
                        let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();
                        match decoder.generate_waterfall_data(&audio_f64) {
                            Ok(wf) => {
                                let range = wf.max_power - wf.min_power;
                                info!(
                                    "Waterfall: {}x{} matrix, power range {:.1}..{:.1} dB",
                                    wf.power_matrix.len(),
                                    wf.power_matrix.first().map(|r| r.len()).unwrap_or(0),
                                    wf.min_power,
                                    wf.max_power,
                                );
                                let rows: Vec<Vec<f32>> = if range > 0.0 {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| {
                                            row.iter()
                                                .map(|&p| ((p - wf.min_power) / range) as f32)
                                                .collect()
                                        })
                                        .collect()
                                } else {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| vec![0.0f32; row.len()])
                                        .collect()
                                };
                                let _ = waterfall_tx.send(rows.clone());
                                if let Some(ref auto_wf_tx) = self_waterfall_to_auto_tx {
                                    let _ = auto_wf_tx.try_send(rows);
                                }
                            }
                            Err(e) => {
                                warn!("Waterfall generation error: {}", e);
                            }
                        }

                        // Decode FT8 signals
                        match decoder.decode_window(&window) {
                            Ok(decoded_messages) => {
                                // Update decode timestamp
                                rt.block_on(async {
                                    let mut timestamp = last_decode_timestamp.write().await;
                                    *timestamp = Some(Instant::now());
                                });

                                info!("FT8 decoder: {} messages decoded", decoded_messages.len());

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

                                    // Forward to other components via message bus
                                    let auto_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Autonomous,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    rt.block_on(async {
                                        let _ = message_bus.send_message(auto_msg).await;
                                    });

                                    let qso_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::Qso,
                                        MessageType::DecodedMessage(decoded_msg.clone()),
                                        Instant::now(),
                                    );
                                    rt.block_on(async {
                                        let _ = message_bus.send_message(qso_msg).await;
                                    });

                                    let psk_msg = ComponentMessage::new(
                                        ComponentId::Ft8Decoder,
                                        ComponentId::PskReporter,
                                        MessageType::DecodedMessage(decoded_msg),
                                        Instant::now(),
                                    );
                                    rt.block_on(async {
                                        let _ = message_bus.send_message(psk_msg).await;
                                    });
                                }
                            }
                            Err(e) => {
                                warn!("FT8 decode error: {}", e);
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!("FT8 decoder: input channel disconnected");
                        break;
                    }
                }
            }

            info!("FT8 component stopped");
            Ok(())
        });

        self.named_task_handles
            .push((ComponentId::Ft8Decoder, handle));
        info!("FT8 component started");
        Ok(())
    }

    /// Start TUI component with point-to-point decoded message channel
    async fn start_tui_pipeline(
        &mut self,
        ft8_to_tui_rx: crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>,
        tui_bus_rx: crossbeam_channel::Receiver<ComponentMessage>,
        waterfall_rx: crossbeam_channel::Receiver<Vec<Vec<f32>>>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_tui");
        let _enter = span.enter();

        info!("Starting TUI component");

        let config = self.config.clone();
        let shutdown = self.shutdown_signal.clone();

        // Create TUI message/command channels for the TuiRunner
        let (tui_msg_tx, tui_msg_rx) =
            crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiMessage>();
        let (tui_cmd_tx, tui_cmd_rx) =
            crossbeam_channel::unbounded::<pancetta_tui::tui_runner::TuiCommand>();

        // Read initial operating frequency from config (no frequency_mhz field on StationConfig,
        // so default to 20m FT8 = 14.074 MHz; will be updated by FrequencyResponse messages)
        let operating_freq_mhz = 14.074_f64;
        let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
            operating_freq_mhz.to_bits(),
        ));
        let operating_freq_relay = operating_freq.clone();

        // Set up station coordinates for distance/bearing calculation
        let station_coords = {
            let config = self.config.read().await;
            pancetta_dx::gridsquare::grid_to_coordinates(&config.station.grid_square).ok()
        };

        // Relay decoded messages from FT8 → TUI on a dedicated thread
        // (tokio::spawn was causing starvation — same pattern as DSP/FT8 fixes)
        let relay_shutdown = shutdown.clone();
        let tui_msg_tx_relay = tui_msg_tx.clone();
        std::thread::Builder::new()
            .name("tui-relay".to_string())
            .spawn(move || {
            let mut ft8_disconnected = false;
            while !relay_shutdown.load(Ordering::Acquire) {
                if !ft8_disconnected {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(decoded_msg) => {
                            let call_sign = decoded_msg.message.from_callsign.clone();
                            let grid_square = decoded_msg.message.grid_square.clone();

                            // Compute distance and bearing if both grids are available
                            let (distance, bearing) = match (&grid_square, &station_coords) {
                                (Some(remote_grid), Some((home_lat, home_lon))) => {
                                    match pancetta_dx::gridsquare::grid_to_coordinates(remote_grid)
                                    {
                                        Ok((remote_lat, remote_lon)) => {
                                            let geod = geographiclib_rs::Geodesic::wgs84();
                                            let (dist_m, azi1, _azi2, _arc) = geod.inverse(
                                                *home_lat, *home_lon, remote_lat, remote_lon,
                                            );
                                            let bearing_deg =
                                                if azi1 < 0.0 { azi1 + 360.0 } else { azi1 };
                                            (Some(dist_m / 1000.0), Some(bearing_deg))
                                        }
                                        Err(_) => (None, None),
                                    }
                                }
                                _ => (None, None),
                            };

                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                timestamp: chrono::Utc::now(),
                                frequency: f64::from_bits(
                                    operating_freq_relay.load(Ordering::Relaxed),
                                ),
                                mode: "FT8".to_string(),
                                snr: decoded_msg.snr_db as i32,
                                delta_time: decoded_msg.time_offset as f32,
                                delta_freq: decoded_msg.frequency_offset as f32,
                                call_sign,
                                grid_square,
                                message: decoded_msg.text.clone(),
                                distance,
                                bearing,
                            };

                            match tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodedMessage(tui_decoded),
                            ) {
                                Ok(()) => info!("TUI relay: forwarded decoded message to TUI channel"),
                                Err(e) => warn!("TUI relay: failed to send to TUI: {}", e),
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            warn!("FT8 decoder channel disconnected, TUI relay continuing without decode data");
                            ft8_disconnected = true;
                        }
                    }
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
                                // Update operating frequency for decoded message enrichment
                                let freq_mhz = frequency as f64 / 1_000_000.0;
                                // Relaxed ordering is fine — this is a best-effort display value for the TUI
                                operating_freq_relay.store(freq_mhz.to_bits(), Ordering::Relaxed);
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

                // Relay waterfall data from FT8 decoder to TUI
                match waterfall_rx.try_recv() {
                    Ok(rows) => {
                        let _ = tui_msg_tx_relay
                            .send(pancetta_tui::tui_runner::TuiMessage::WaterfallUpdate { rows });
                    }
                    Err(_) => {}
                }

                // Sleep to prevent busy-spinning
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            info!("TUI relay thread stopped");
        }).expect("Failed to spawn TUI relay thread");

        // Task: relay TUI commands (e.g. SendMessage) to message bus as TransmitRequests
        let cmd_shutdown = self.shutdown_signal.clone();
        let cmd_message_bus = self.message_bus.clone();
        let cmd_operating_freq = operating_freq.clone();
        let cmd_handle = tokio::spawn(async move {
            while !cmd_shutdown.load(Ordering::Acquire) {
                match tui_cmd_rx.try_recv() {
                    Ok(cmd) => match cmd {
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
                        pancetta_tui::tui_runner::TuiCommand::CallStation {
                            callsign,
                            frequency,
                        } => {
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
                        pancetta_tui::tui_runner::TuiCommand::SetFrequency { vfo, frequency } => {
                            info!("TUI SetFrequency: VFO {} → {} Hz", vfo, frequency);
                            let freq_mhz = frequency as f64 / 1_000_000.0;
                            cmd_operating_freq.store(freq_mhz.to_bits(), Ordering::Relaxed);
                            // Forward to hamlib if available
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                debug!("Failed to forward SetFrequency to hamlib: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::Quit => {
                            info!("TUI requested application quit");
                            cmd_shutdown.store(true, Ordering::Release);
                            break;
                        }
                        _ => {
                            debug!("Unhandled TUI command: {:?}", cmd);
                        }
                    },
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
            Ok(())
        });
        self.named_task_handles.push((ComponentId::Tui, cmd_handle));

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
            bands: pancetta_tui::Config::default().bands,
        };
        drop(tui_config_lock);

        // Start TUI runner in a blocking task so it can own the terminal
        let tui_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                pancetta_tui::tui_runner::run_tui_with_message_bus(
                    tui_config, tui_msg_rx, tui_cmd_tx, shutdown,
                )
                .await
            })
        });

        // Wrap the JoinHandle and ensure shutdown is triggered when TUI exits
        let tui_shutdown = self.shutdown_signal.clone();
        let tui_wrapper = tokio::spawn(async move {
            let result = match tui_handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("TUI task panicked: {}", e)),
            };
            // Always trigger shutdown when TUI exits (user quit, crash, etc.)
            tui_shutdown.store(true, Ordering::Release);
            result
        });
        self.named_task_handles
            .push((ComponentId::Tui, tui_wrapper));

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

            let builder =
                PrometheusBuilder::new().with_http_listener(([0, 0, 0, 0], self.metrics_port));

            builder
                .install()
                .context("Failed to install Prometheus metrics exporter")?;

            info!("Metrics server started on port {}", self.metrics_port);
        }

        Ok(())
    }

    /// Start Hamlib component for rig control
    /// Map rig model name to hamlib model number.
    /// See: https://github.com/Hamlib/Hamlib/wiki/Supported-Radios
    #[cfg(feature = "pancetta-hamlib")]
    fn hamlib_model_id(model: &str) -> Option<u32> {
        match model.to_lowercase().replace(['-', ' '], "").as_str() {
            "ftdx10" => Some(1042),
            "ftdx101d" | "ftdx101mp" => Some(1040),
            "ft991" | "ft991a" => Some(1036),
            "ft710" => Some(1046),
            "ft891" => Some(1038),
            "ft857" | "ft857d" => Some(1022),
            "ft817" | "ft817nd" => Some(1020),
            "ic7300" => Some(3073),
            "ic7610" => Some(3078),
            "ic7851" => Some(3075),
            "ic705" => Some(3085),
            "ic9700" => Some(3081),
            "ts890" | "ts890s" => Some(2029),
            "ts590" | "ts590s" | "ts590sg" => Some(2026),
            _ => None,
        }
    }

    #[cfg(feature = "pancetta-hamlib")]
    async fn start_hamlib_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_hamlib");
        let _enter = span.enter();

        info!("Starting Hamlib component");

        let (_hamlib_tx, hamlib_rx) = self.message_bus.create_channel(ComponentId::Hamlib).await?;
        let message_bus = self.message_bus.clone();

        // Read rig config before spawning
        let rig_config = {
            let config = self.config.read().await;
            config.rig.clone()
        };

        // Use mock rig only if explicitly requested via env var
        let use_mock = std::env::var("PANCETTA_MOCK_RIG")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);
        let rig_enabled = rig_config.interface.enabled && !use_mock;

        // Spawn rigctld as a managed child process if rig is enabled
        // and no external rigctld is already running
        let rigctld_port: u16 = std::env::var("RIGCTLD_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(4532);
        let rigctld_host =
            std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        if rig_enabled {
            // Check if rigctld is already running
            let already_running = tokio::net::TcpStream::connect(
                format!("{}:{}", rigctld_host, rigctld_port),
            )
            .await
            .is_ok();

            if already_running {
                info!("rigctld already running on {}:{}", rigctld_host, rigctld_port);
            } else if let Some(model_id) = Self::hamlib_model_id(&rig_config.model) {
                // rigctld knows the correct serial parameters (stop bits, parity,
                // flow control) for each rig model — we only need to specify
                // model, port, and baud rate.
                info!(
                    "Spawning rigctld: model={} (hamlib {}), port={}, baud={}",
                    rig_config.model, model_id,
                    rig_config.interface.port, rig_config.interface.baud_rate
                );

                match std::process::Command::new("rigctld")
                    .args([
                        "-m", &model_id.to_string(),
                        "-r", &rig_config.interface.port,
                        "-s", &rig_config.interface.baud_rate.to_string(),
                        "-t", &rigctld_port.to_string(),
                        "-T", &rigctld_host,
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        info!("rigctld spawned (PID {})", child.id());
                        self.rigctld_process = Some(child);
                        // Give rigctld time to bind the port and open the serial device
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to spawn rigctld: {}. Install hamlib: brew install hamlib",
                            e
                        );
                    }
                }
            } else {
                warn!(
                    "Unknown rig model '{}' — cannot determine hamlib ID. \
                     Set RIGCTLD_HOST/RIGCTLD_PORT to use an external rigctld.",
                    rig_config.model
                );
            }
        }

        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                let rig: Box<dyn pancetta_hamlib::RigControl + Send + Sync> = if !rig_enabled {
                    info!("Rig control disabled, using mock rig");
                    Box::new(pancetta_hamlib::MockRig::default())
                } else {
                    info!("Connecting to rigctld at {}:{}", rigctld_host, rigctld_port);

                    let config = pancetta_hamlib::RigctldConfig {
                        host: rigctld_host,
                        port: rigctld_port,
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

                    while !shutdown_for_polling.load(Ordering::Acquire) {
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

                // PTT safety watchdog: track when PTT was turned on
                // If PTT stays on for longer than PTT_SAFETY_TIMEOUT_SECS,
                // force it off to prevent accidental continuous transmission
                // (e.g. if the TX pipeline crashes mid-transmission).
                const PTT_SAFETY_TIMEOUT_SECS: u64 = 30;
                let ptt_on_since: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));

                // Spawn the PTT watchdog as a background task
                let rig_for_watchdog = Arc::clone(&rig_poll);
                let ptt_watchdog_tracker = ptt_on_since.clone();
                let shutdown_for_watchdog = shutdown.clone();
                tokio::spawn(async move {
                    let mut watchdog_interval = interval(Duration::from_secs(1));
                    loop {
                        watchdog_interval.tick().await;
                        if shutdown_for_watchdog.load(Ordering::Acquire) {
                            break;
                        }

                        let ptt_time = {
                            let guard = ptt_watchdog_tracker.read().await;
                            *guard
                        };

                        if let Some(on_since) = ptt_time {
                            if on_since.elapsed() > Duration::from_secs(PTT_SAFETY_TIMEOUT_SECS) {
                                error!(
                                    "PTT SAFETY WATCHDOG: PTT has been on for >{} seconds — forcing OFF",
                                    PTT_SAFETY_TIMEOUT_SECS
                                );
                                match rig_for_watchdog
                                    .set_ptt(
                                        pancetta_hamlib::Vfo::Current,
                                        pancetta_hamlib::PttState::Off,
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        warn!("PTT SAFETY WATCHDOG: PTT forced off successfully");
                                        // Only clear timer on success — retry on next tick if it fails
                                        let mut guard = ptt_watchdog_tracker.write().await;
                                        *guard = None;
                                    }
                                    Err(e) => {
                                        error!(
                                            "PTT SAFETY WATCHDOG: failed to force PTT off: {} — will retry in 1s",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                });

                // Process messages
                while !shutdown.load(Ordering::Acquire) {
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
                                        // Update PTT watchdog tracker
                                        {
                                            let mut guard = ptt_on_since.write().await;
                                            if *state {
                                                // PTT going on — record the time
                                                if guard.is_none() {
                                                    *guard = Some(Instant::now());
                                                    debug!("PTT watchdog: PTT ON, timer started");
                                                }
                                            } else {
                                                // PTT going off — clear the timer
                                                *guard = None;
                                                debug!("PTT watchdog: PTT OFF, timer cleared");
                                            }
                                        }

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

        self.named_task_handles
            .push((ComponentId::Hamlib, hamlib_handle));
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

        let qso_lookup = self.cached_lookup.clone();
        let cqdx_bridge = self.cqdx_bridge.clone();
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{LoggerConfig, QsoLogger, QsoManager, QsoManagerConfig};

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

                let _logger = match QsoLogger::new(logger_config, qso_manager.clone()).await {
                    Ok(l) => {
                        info!("QSO logger initialized with database at {:?}", db_path);
                        if let Err(e) = l.start().await {
                            warn!("QSO logger background tasks failed to start: {}", e);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize QSO logger (continuing without): {}",
                            e
                        );
                        None
                    }
                };

                info!(
                    "QSO component ready (callsign={}, grid={:?})",
                    our_callsign, our_grid
                );

                // Spawn a task to forward QSO auto-sequence TX requests to the transmitter
                let mut qso_events = qso_manager.subscribe();
                let tx_bus = message_bus.clone();
                let tx_shutdown = shutdown.clone();
                let tx_callsign = our_callsign.clone();
                tokio::spawn(async move {
                    while !tx_shutdown.load(Ordering::Acquire) {
                        match qso_events.recv().await {
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                qso_id,
                                message,
                                frequency,
                            }) => {
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={})",
                                            text, frequency, qso_id
                                        );
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: text,
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = tx_bus.send_message(tx_msg).await {
                                            warn!("Failed to send auto-sequence TX: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to generate FT8 message for QSO {}: {}",
                                            qso_id, e
                                        );
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO completed with {}, marking as worked", their_call);
                                    qso_lookup.record_worked(their_call);

                                    // Report QSO to cqdx.io
                                    if let Some(ref bridge) = cqdx_bridge {
                                        bridge.report_qso(pancetta_cqdx::QsoRecord {
                                            callsign: their_call.clone(),
                                            remote_grid: metadata.grids.theirs.clone(),
                                            local_grid: metadata.grids.ours.clone(),
                                            frequency: metadata.frequency as u64,
                                            mode: metadata.mode.clone(),
                                            rst_sent: metadata.reports.sent.map(|r| r.to_string()),
                                            rst_received: metadata.reports.received.map(|r| r.to_string()),
                                            start_time: metadata.start_time,
                                            end_time: metadata.end_time.unwrap_or_else(chrono::Utc::now),
                                        });
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoFailed { metadata, .. }) => {
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO failed with {}, adding backoff", their_call);
                                    qso_lookup.record_failure(their_call);
                                }
                            }
                            Ok(_) => {} // Other events (StateChanged, etc.)
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("QSO event subscriber lagged by {} events", n);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                });

                while !shutdown.load(Ordering::Acquire) {
                    match qso_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                // Decoded FT8 messages forwarded from the decoder
                                MessageType::DecodedMessage(ref decoded_msg) => {
                                    let raw_text = decoded_msg.text.clone();
                                    let frequency = decoded_msg.frequency_offset as f64;
                                    let snr = decoded_msg.snr_db as f32;

                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(
                                        &raw_text,
                                        &our_callsign,
                                    ) {
                                        Ok(msg_type) => {
                                            if let Err(e) = qso_manager
                                                .process_message(
                                                    msg_type,
                                                    raw_text.clone(),
                                                    frequency,
                                                    Some(snr),
                                                )
                                                .await
                                            {
                                                debug!("QSO process_message error: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            debug!(
                                                "Could not parse FT8 message '{}': {}",
                                                raw_text, e
                                            );
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
                                            info!(
                                                "Starting QSO with {} on {} Hz",
                                                callsign, frequency
                                            );
                                            match qso_manager
                                                .respond_to_cq(callsign.clone(), frequency as f64)
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "QSO started with {}: {}",
                                                        callsign, qso_id
                                                    );
                                                    // Send grid reply as TX request
                                                    let grid =
                                                        our_grid.as_deref().unwrap_or("AA00");
                                                    let reply = format!(
                                                        "{} {} {}",
                                                        callsign, our_callsign, grid
                                                    );
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
                                                    if let Err(e) =
                                                        message_bus.send_message(tx_msg).await
                                                    {
                                                        warn!(
                                                            "Failed to send QSO TX request: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to start QSO with {}: {}",
                                                        callsign, e
                                                    );
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

        self.named_task_handles.push((ComponentId::Qso, qso_handle));
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
            let _ = self
                .message_bus
                .create_channel(ComponentId::DxCluster)
                .await?;
            return Ok(());
        }

        let cluster_hostname = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string());
        let cluster_port = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.port)
            .unwrap_or(23);
        let our_callsign = config.station.callsign.clone();
        drop(config);

        info!(
            "Starting DX cluster component ({}:{})",
            cluster_hostname, cluster_port
        );

        let (_dx_tx, _dx_rx) = self
            .message_bus
            .create_channel(ComponentId::DxCluster)
            .await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::cluster::{ClusterConfig, DxClusterClient};

                let mut client = DxClusterClient::with_config(ClusterConfig {
                    hostname: cluster_hostname.clone(),
                    port: cluster_port,
                    callsign: our_callsign.clone(),
                    timeout_seconds: 30,
                    reconnect_delay_seconds: 30,
                    auto_reconnect: true,
                    filter_settings: Default::default(),
                    use_websocket: false,
                    websocket_url: None,
                });

                match client.connect().await {
                    Ok(_) => {
                        info!("Connected to DX cluster");

                        // Login with our callsign
                        if let Err(e) = client.login().await {
                            warn!("DX cluster login failed: {}. Continuing without.", e);
                        }

                        // Monitor spots and forward to TUI
                        while !shutdown.load(Ordering::Acquire) {
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                client.receive_spot(),
                            )
                            .await
                            {
                                Ok(Some(spot)) => {
                                    debug!(
                                        "DX spot: {} on {} Hz by {}",
                                        spot.callsign, spot.frequency, spot.spotter
                                    );

                                    let msg = ComponentMessage::new(
                                        ComponentId::DxCluster,
                                        ComponentId::Tui,
                                        MessageType::DxMessage(
                                            crate::message_bus::DxMessage::Spot {
                                                callsign: spot.callsign,
                                                frequency: spot.frequency,
                                                spotter: spot.spotter,
                                                comment: spot.comment.unwrap_or_default(),
                                            },
                                        ),
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

        self.named_task_handles
            .push((ComponentId::DxCluster, dx_handle));
        info!("DX cluster component started");
        Ok(())
    }

    /// Start PSKReporter upload component
    ///
    /// Receives decoded FT8 messages, batches them, and uploads to PSKReporter
    /// at the configured interval (default: 5 minutes).
    async fn start_pskreporter_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.psk_reporter.enabled {
            info!("PSKReporter upload disabled in configuration");
            drop(config);
            let _ = self
                .message_bus
                .create_channel(ComponentId::PskReporter)
                .await?;
            return Ok(());
        }

        let our_callsign = config.station.callsign.clone();
        let our_grid = config.station.grid_square.clone();
        let upload_interval = config.network.psk_reporter.upload_interval_seconds;
        let antenna = config
            .network
            .psk_reporter
            .reporter_info
            .antenna_info
            .clone()
            .unwrap_or_default();
        let software = format!(
            "{}/{}",
            config.network.psk_reporter.reporter_info.software_name,
            config.network.psk_reporter.reporter_info.software_version
        );
        drop(config);

        info!(
            "Starting PSKReporter upload component (interval: {}s)",
            upload_interval
        );

        let (_psk_tx, psk_rx) = self
            .message_bus
            .create_channel(ComponentId::PskReporter)
            .await?;

        let psk_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::pskreporter::{
                    PskReporterUploadConfig, PskReporterUploader, ReceptionReport,
                };

                let upload_config = PskReporterUploadConfig {
                    reporter_callsign: our_callsign,
                    reporter_grid: our_grid,
                    antenna,
                    software,
                    upload_interval_secs: upload_interval,
                    ..Default::default()
                };

                let mut uploader = PskReporterUploader::new(upload_config);
                let mut upload_timer = interval(Duration::from_secs(upload_interval));

                while !shutdown.load(Ordering::Acquire) {
                    // Drain incoming decoded messages
                    loop {
                        match psk_rx.try_recv() {
                            Ok(message) => {
                                if let MessageType::DecodedMessage(ref decoded_msg) =
                                    message.message_type
                                {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64;

                                    if let Some(ref callsign) = decoded_msg.message.from_callsign {
                                        uploader.add_report(ReceptionReport {
                                            tx_callsign: callsign.clone(),
                                            frequency: decoded_msg.frequency_offset as u64,
                                            snr: Some(decoded_msg.snr_db as i32),
                                            mode: "FT8".to_string(),
                                            tx_grid: decoded_msg.message.grid_square.clone(),
                                            timestamp,
                                        });
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                info!("PSKReporter channel disconnected");
                                return Ok(());
                            }
                        }
                    }

                    // Check if it's time to upload
                    tokio::select! {
                        _ = upload_timer.tick() => {
                            if uploader.pending_count() > 0 {
                                match uploader.flush().await {
                                    Ok(count) => {
                                        info!("PSKReporter: uploaded {} spots", count);
                                    }
                                    Err(e) => {
                                        warn!("PSKReporter upload failed: {}", e);
                                    }
                                }
                            }
                        }
                        _ = sleep(Duration::from_millis(100)) => {
                            // Short sleep to avoid busy-looping
                        }
                    }
                }

                // Flush remaining on shutdown
                if uploader.pending_count() > 0 {
                    if let Err(e) = uploader.flush().await {
                        warn!("PSKReporter final flush failed: {}", e);
                    }
                }

                info!("PSKReporter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::PskReporter, psk_handle));
        info!("PSKReporter component started");
        Ok(())
    }

    /// Start FT8 transmitter component
    async fn start_transmitter_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_transmitter");
        let _enter = span.enter();

        info!("Starting FT8 transmitter component");

        let (_tx_sender, tx_rx) = self
            .message_bus
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

                while !shutdown.load(Ordering::Acquire) {
                    match tx_rx.try_recv() {
                        Ok(message) => {
                            // Helper: wait for slot boundary, assert PTT, TX audio, de-assert PTT.
                            let wait_for_slot = || {
                                use std::time::{SystemTime, UNIX_EPOCH};
                                let now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default();
                                let secs = now.as_secs();
                                let slot_pos = secs % 15;
                                let wait_secs = if slot_pos == 0 { 0 } else { 15 - slot_pos };
                                let sub_ms = now.subsec_millis() as u64;
                                let wait_ms = wait_secs
                                    .saturating_mul(1000)
                                    .saturating_add(1000u64.saturating_sub(sub_ms))
                                    .saturating_sub(200); // 200ms guard for PTT latency
                                Duration::from_millis(wait_ms.min(15000))
                            };

                            match message.message_type {
                                MessageType::TransmitRequest {
                                    message_text,
                                    frequency_offset,
                                    qso_id,
                                } => {
                                    info!(
                                        "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                        message_text, frequency_offset, qso_id
                                    );

                                    // --- Step 1: Wait for next FT8 slot boundary ---
                                    let slot_wait = wait_for_slot();

                                    if slot_wait.as_millis() > 100 {
                                        info!(
                                            "Waiting {:.1}s for next TX slot boundary",
                                            slot_wait.as_secs_f64()
                                        );
                                        sleep(slot_wait).await;
                                    }

                                    // --- Step 2: Assert PTT ---
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        debug!("PTT on failed (no rig?): {}", e);
                                    }
                                    sleep(Duration::from_millis(50)).await;

                                    // --- Step 3: Encode and modulate ---
                                    let encode_result = encoder.encode_message(&message_text, None);
                                    let (success, duration_ms) = match encode_result {
                                        Ok(symbols) => {
                                            match modulator
                                                .modulate_symbols(&symbols, frequency_offset)
                                            {
                                                Ok(samples) => {
                                                    let duration = (samples.len() as f64 / 12000.0
                                                        * 1000.0)
                                                        as u64;
                                                    info!(
                                                        "TX: '{}' → {} samples ({:.1}s)",
                                                        message_text,
                                                        samples.len(),
                                                        duration as f64 / 1000.0
                                                    );

                                                    // --- Step 4: Route audio to output ---
                                                    let audio_msg = ComponentMessage::new(
                                                        ComponentId::Ft8Transmitter,
                                                        ComponentId::Audio,
                                                        MessageType::AudioOutput {
                                                            samples: samples.clone(),
                                                            sample_rate: 12000,
                                                        },
                                                        Instant::now(),
                                                    );
                                                    if let Err(e) =
                                                        message_bus.send_message(audio_msg).await
                                                    {
                                                        debug!("Audio output routing: {}", e);
                                                    }

                                                    sleep(Duration::from_millis(duration)).await;
                                                    (true, duration)
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Modulation failed for '{}': {}",
                                                        message_text, e
                                                    );
                                                    (false, 0)
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Encoding failed for '{}': {}", message_text, e);
                                            (false, 0)
                                        }
                                    };

                                    // --- Step 5: De-assert PTT ---
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        debug!("PTT off failed (no rig?): {}", e);
                                    }

                                    // --- Step 6: Send TransmitComplete ---
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

                                MessageType::MultiTransmitRequest { items } => {
                                    info!("Multi-TX request: {} messages", items.len());

                                    // --- Step 1: Wait for slot boundary ---
                                    let slot_wait = wait_for_slot();
                                    if slot_wait.as_millis() > 100 {
                                        info!(
                                            "Waiting {:.1}s for next TX slot boundary",
                                            slot_wait.as_secs_f64()
                                        );
                                        sleep(slot_wait).await;
                                    }

                                    // --- Step 2: Assert PTT ---
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        debug!("PTT on failed (no rig?): {}", e);
                                    }
                                    sleep(Duration::from_millis(50)).await;

                                    // --- Step 3: Encode each message, build multi-TX items ---
                                    let ft8_params = pancetta_ft8::ProtocolParams::ft8();
                                    let mut multi_items = Vec::new();
                                    let mut symbol_sets: Vec<Vec<u8>> = Vec::new();
                                    let mut item_texts: Vec<String> = Vec::new();

                                    for item in &items {
                                        match encoder.encode_message(&item.message_text, None) {
                                            Ok(symbols) => {
                                                item_texts.push(item.message_text.clone());
                                                symbol_sets.push(symbols.to_vec());
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Encoding failed for '{}': {}",
                                                    item.message_text, e
                                                );
                                            }
                                        }
                                    }

                                    // Build MultiTxItem references
                                    for (i, symbols) in symbol_sets.iter().enumerate() {
                                        multi_items.push(pancetta_ft8::MultiTxItem {
                                            symbols: symbols.as_slice(),
                                            frequency_offset: items[i].frequency_offset,
                                            params: &ft8_params,
                                        });
                                    }

                                    let (success, duration_ms) = if !multi_items.is_empty() {
                                        match pancetta_ft8::modulate_multi_tx(
                                            &multi_items,
                                            12000,
                                            1500.0,
                                            0.5,
                                        ) {
                                            Ok(samples) => {
                                                let duration = (samples.len() as f64 / 12000.0
                                                    * 1000.0)
                                                    as u64;
                                                info!(
                                                    "Multi-TX: {} messages → {} samples ({:.1}s)",
                                                    multi_items.len(),
                                                    samples.len(),
                                                    duration as f64 / 1000.0
                                                );

                                                let audio_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Audio,
                                                    MessageType::AudioOutput {
                                                        samples: samples.clone(),
                                                        sample_rate: 12000,
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(audio_msg).await
                                                {
                                                    debug!("Audio output routing: {}", e);
                                                }

                                                sleep(Duration::from_millis(duration)).await;
                                                (true, duration)
                                            }
                                            Err(e) => {
                                                warn!("Multi-TX modulation failed: {}", e);
                                                (false, 0)
                                            }
                                        }
                                    } else {
                                        (false, 0)
                                    };

                                    // --- Step 5: De-assert PTT ---
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        debug!("PTT off failed (no rig?): {}", e);
                                    }

                                    // --- Step 6: Send TransmitComplete for each item ---
                                    for text in item_texts {
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success,
                                                message_text: text,
                                                duration_ms,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(complete_msg).await
                                        {
                                            warn!("Failed to send TransmitComplete: {}", e);
                                        }
                                    }
                                }

                                _ => {} // Ignore other message types
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

        self.named_task_handles
            .push((ComponentId::Ft8Transmitter, tx_handle));
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
            let _ = self
                .message_bus
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
            min_multi_slot_score: config.autonomous.min_multi_slot_score,
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
            frequency: pancetta_qso::frequency::FrequencyAllocatorConfig {
                decode_history_cycles: config.autonomous.frequency.decode_history_cycles,
                center_bias_hz: config.autonomous.frequency.center_bias_hz,
                dx_proximity_min_hz: config.autonomous.frequency.dx_proximity_min_hz,
                dx_proximity_max_hz: config.autonomous.frequency.dx_proximity_max_hz,
                min_separation_hz: config.autonomous.frequency.min_separation_hz,
                neighbor_guard_hz: config.autonomous.frequency.neighbor_guard_hz,
                ..Default::default()
            },
        };

        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };

        // Read priority weights before dropping config
        let priority_weights = pancetta_qso::priority::PriorityWeights {
            needed_dxcc: config.autonomous.priorities.needed_dxcc,
            needed_grid: config.autonomous.priorities.needed_grid,
            pota_sota: config.autonomous.priorities.pota_sota,
            rarity: config.autonomous.priorities.rarity,
            signal_strength: config.autonomous.priorities.signal_strength,
            duplicate_penalty: config.autonomous.priorities.duplicate_penalty,
            recent_failure_penalty: config.autonomous.priorities.recent_failure_penalty,
        };
        drop(config);

        let cached_lookup = self.cached_lookup.clone();

        let spot_reporter_callsign = our_callsign.clone();
        let spot_reporter_grid = our_grid.clone();
        let operator = std::sync::Arc::new(tokio::sync::Mutex::new(
            pancetta_qso::AutonomousOperator::new(qso_auto_config, our_callsign, our_grid),
        ));

        let (waterfall_to_auto_tx, waterfall_to_auto_rx) =
            crossbeam_channel::bounded::<Vec<Vec<f32>>>(2);
        self.waterfall_to_auto_tx = Some(waterfall_to_auto_tx);

        let evaluator: std::sync::Arc<dyn pancetta_qso::DxEvaluator> =
            std::sync::Arc::new(pancetta_qso::PriorityScorer::new(
                priority_weights,
                Box::new((*cached_lookup).clone()),
            ));

        let (_auto_tx, auto_rx) = self
            .message_bus
            .create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();

        let cqdx_bridge_for_auto = self.cqdx_bridge.clone();
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
                            // Report decoded spots to cqdx.io
                            if let Some(ref bridge) = cqdx_bridge_for_auto {
                                let spot_reports: Vec<pancetta_cqdx::SpotReport> = slot_messages
                                    .iter()
                                    .filter_map(|msg| {
                                        msg.callsign.as_ref().map(|call| pancetta_cqdx::SpotReport {
                                            callsign: call.clone(),
                                            grid: None,
                                            frequency: msg.frequency_hz as u64,
                                            mode: "FT8".to_string(),
                                            snr: msg.snr,
                                            timestamp: chrono::Utc::now(),
                                            reporter: spot_reporter_callsign.clone(),
                                            reporter_grid: spot_reporter_grid.clone(),
                                        })
                                    })
                                    .collect();
                                bridge.report_spots(spot_reports);
                            }

                            let mut op = operator.lock().await;

                            // Update spectral data from waterfall
                            if let Ok(rows) = waterfall_to_auto_rx.try_recv() {
                                if let Some(first_row) = rows.first() {
                                    let num_bins = first_row.len();
                                    let mut avg = vec![0.0f32; num_bins];
                                    for row in &rows {
                                        for (i, &v) in row.iter().enumerate().take(num_bins) {
                                            avg[i] += v;
                                        }
                                    }
                                    let n = rows.len() as f32;
                                    for v in &mut avg {
                                        *v /= n;
                                    }
                                    op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                                        power_bins: avg,
                                        freq_min_hz: 200.0,
                                        freq_max_hz: 4000.0,
                                    });
                                }
                            }

                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());
                            slot_messages.clear();
                            let actions = op.decide();
                            drop(op);

                            // Collect Transmit actions, then bundle into a
                            // single MultiTransmitRequest (or single TransmitRequest).
                            let mut tx_items: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();

                            for action in actions {
                                match action {
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                        }
                                        tx_items.push(crate::message_bus::TransmitRequestItem {
                                            message_text: message_text.clone(),
                                            frequency_offset,
                                            qso_id: qso_id.clone(),
                                        });
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

                            // Bundle collected TX items into a single message.
                            if tx_items.len() == 1 {
                                let item = tx_items.remove(0);
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TransmitRequest {
                                        message_text: item.message_text,
                                        frequency_offset: item.frequency_offset,
                                        qso_id: item.qso_id,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send TransmitRequest: {}", e);
                                }
                            } else if tx_items.len() > 1 {
                                info!("Bundling {} TX items into MultiTransmitRequest", tx_items.len());
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::MultiTransmitRequest { items: tx_items },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send MultiTransmitRequest: {}", e);
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

                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }

                info!("Autonomous operator stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Autonomous, auto_handle));
        info!("Autonomous operator component started");
        Ok(())
    }

    // =========================================================================
    // Coordinator management
    // =========================================================================

    /// Start coordinator management tasks
    async fn start_coordinator_tasks(&mut self) -> Result<()> {
        // Initialize component status for all registered task handles
        {
            let mut status_map = self.component_status.write().await;
            for (id, _) in &self.named_task_handles {
                status_map
                    .entry(*id)
                    .or_insert_with(ComponentStatus::new_running);
            }
        }

        // Health monitoring task — checks task handles and message bus health
        let health_handle = self.start_health_monitor().await;

        // Configuration hot-reload task
        let config_handle = {
            let _config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    sleep(Duration::from_secs(1)).await;
                }
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Coordinator, health_handle));
        self.named_task_handles
            .push((ComponentId::Coordinator, config_handle));

        Ok(())
    }

    /// Start the health monitor task.
    ///
    /// Runs every `health_check_interval` (5s) and:
    /// 1. Checks each named task handle with `is_finished()`
    /// 2. If a task finished unexpectedly, logs the appropriate degradation message
    /// 3. Sends a health status summary to the TUI via the message bus
    ///
    /// No component failure crashes the whole application — the coordinator
    /// continues running in degraded mode.
    async fn start_health_monitor(&self) -> JoinHandle<Result<()>> {
        let message_bus = self.message_bus.clone();
        let shutdown = self.shutdown_signal.clone();
        let component_status = self.component_status.clone();
        let mut health_interval = interval(Duration::from_secs(5));

        tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                health_interval.tick().await;

                // Check message bus level health (heartbeats, error counts)
                let bus_health = message_bus.get_component_health().await;
                for health in &bus_health {
                    if !health.is_healthy {
                        warn!(
                            "Component {} is unhealthy: {} errors",
                            health.component_id, health.error_count
                        );
                    }
                }

                // Build a status summary from the component_status map
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
                            // Not started / disabled — don't report
                        }
                    }
                }

                // Send health summary to TUI
                if any_failed {
                    let summary = format!("Degraded — {}", summary_parts.join("; "));
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
    async fn run_main_loop(&mut self) -> Result<()> {
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
                _ = sleep(Duration::from_millis(100)) => {
                    // Main loop iteration
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
    async fn check_task_handles(&mut self) {
        for (component_id, handle) in &self.named_task_handles {
            // Skip coordinator's own tasks and already-known failures
            if *component_id == ComponentId::Coordinator {
                continue;
            }

            if !handle.is_finished() {
                // Task is still running — update last_seen
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(component_id) {
                    if status.state == ComponentState::Running {
                        status.last_seen = Instant::now();
                    }
                }
                continue;
            }

            // Task has finished — check if we already know about it
            let mut status_map = self.component_status.write().await;
            let status = status_map
                .entry(*component_id)
                .or_insert_with(ComponentStatus::new_running);

            if status.state != ComponentState::Running {
                // Already recorded this failure
                continue;
            }

            // First time seeing this component as finished
            let degradation = degradation_message(*component_id);
            let criticality = component_criticality(*component_id);

            status.error_count += 1;
            status.state = ComponentState::Failed(degradation.to_string());

            match criticality {
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

            // For Hamlib failure: ensure PTT defaults to off for safety
            if *component_id == ComponentId::Hamlib {
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

            // Notify TUI of the component failure
            let error_msg = ComponentMessage::new(
                ComponentId::Coordinator,
                ComponentId::Tui,
                MessageType::StatusUpdate(format!("{}: {}", component_id, degradation)),
                Instant::now(),
            );
            let _ = self.message_bus.send_message(error_msg).await;
        }
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
    async fn shutdown(mut self) -> Result<()> {
        let span = span!(Level::INFO, "coordinator_shutdown");
        let _enter = span.enter();

        info!("Starting graceful shutdown");
        self.is_running.store(false, Ordering::Release);
        self.shutdown_signal.store(true, Ordering::Release);

        let per_task_timeout = Duration::from_secs(1);

        for (index, (component_id, handle)) in std::mem::take(&mut self.named_task_handles).into_iter().enumerate() {
            match tokio::time::timeout(per_task_timeout, handle).await {
                Ok(Ok(_)) => {
                    debug!("Task {} ({}) completed successfully", index, component_id);
                }
                Ok(Err(e)) => {
                    warn!(
                        "Task {} ({}) completed with error: {}",
                        index, component_id, e
                    );
                }
                Err(_) => {
                    debug!("Task {} ({}) timed out, aborting", index, component_id);
                }
            }
        }

        // Kill managed rigctld process
        #[cfg(feature = "pancetta-hamlib")]
        if let Some(mut child) = self.rigctld_process.take() {
            info!("Stopping managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }

        info!("Graceful shutdown completed");

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
            config, None, true,  // no_audio
            true,  // headless
            false, // metrics
            9090, None, // no WAV
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
            shutdown,
        )
        .await
        .expect("coordinator creation should succeed");

        // run_wav_playback exits after decoding — should not error
        let result = coordinator.run().await;
        assert!(
            result.is_ok(),
            "WAV playback should succeed: {:?}",
            result.err()
        );
    }
}
