# Pancetta Developer API Documentation

## Table of Contents

1. [Overview](#overview)
2. [Architecture](#architecture)
3. [Core APIs](#core-apis)
4. [Audio Engine API](#audio-engine-api)
5. [FT8 Processing API](#ft8-processing-api)
6. [DSP Engine API](#dsp-engine-api)
7. [Configuration API](#configuration-api)
8. [TUI Framework API](#tui-framework-api)
9. [Plugin Development](#plugin-development)
10. [Integration Examples](#integration-examples)
11. [Performance Guidelines](#performance-guidelines)

## Overview

Pancetta provides a comprehensive Rust API for real-time audio processing and FT8 digital mode operations. The API is designed for maximum performance with sub-millisecond latency requirements while maintaining memory safety and ease of use.

### Key Design Principles

- **Zero-Cost Abstractions**: No runtime overhead for high-level APIs
- **Real-Time Safe**: Lock-free data structures for audio thread safety
- **Memory Safe**: Rust's ownership model prevents data races and memory leaks
- **Modular Architecture**: Composable components for flexible integration
- **Cross-Platform**: Unified API across Linux, macOS, and Windows

### Minimum Supported Rust Version (MSRV)

- **Rust**: 1.70.0 or later
- **Edition**: 2021

## Architecture

### Crate Structure

```
pancetta/
├── pancetta-audio/     # Real-time audio processing engine
├── pancetta-dsp/       # Digital signal processing primitives
├── pancetta-ft8/       # FT8 protocol implementation
├── pancetta-tui/       # Terminal user interface framework
├── pancetta-config/    # Configuration management
└── pancetta/           # Main application and orchestration
```

### Data Flow Architecture

```
┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│    Audio    │───▶│     DSP     │───▶│     FT8     │
│   Engine    │    │  Processing │    │  Decoding   │
└─────────────┘    └─────────────┘    └─────────────┘
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│   Config    │    │     TUI     │    │   Metrics   │
│ Management  │    │ Interface   │    │ Collection  │
└─────────────┘    └─────────────┘    └─────────────┘
```

## Core APIs

### Pancetta Core

```rust
use pancetta::prelude::*;

// Main application builder
#[tokio::main]
async fn main() -> Result<()> {
    let app = PancettaApp::builder()
        .with_config_path("./config.toml")?
        .with_audio_latency_target(Duration::from_millis(1))?
        .with_ft8_enabled(true)
        .build()?;
    
    app.run().await
}
```

### Error Handling

```rust
use pancetta::error::{PancettaError, Result};

// Unified error type across all crates
#[derive(Debug, thiserror::Error)]
pub enum PancettaError {
    #[error("Audio system error: {0}")]
    Audio(#[from] AudioError),
    
    #[error("FT8 processing error: {0}")]
    Ft8(#[from] Ft8Error),
    
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),
    
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// Helper type alias
pub type Result<T> = std::result::Result<T, PancettaError>;
```

### Logging and Metrics

```rust
use pancetta::telemetry::{init_tracing, Metrics};
use tracing::{info, warn, error};

// Initialize structured logging
init_tracing(LogLevel::Info, Some("./logs"))?;

// Emit metrics
let metrics = Metrics::global();
metrics.record_latency("audio.callback", latency_ns);
metrics.increment_counter("ft8.decodes");
```

## Audio Engine API

### Basic Audio Setup

```rust
use pancetta_audio::{AudioEngine, AudioConfig, AudioCallback};
use std::sync::Arc;

// Configure audio system
let config = AudioConfig {
    sample_rate: 48000,
    buffer_size: 64,
    channels_in: 2,
    channels_out: 2,
    latency_target: Duration::from_millis(1),
};

// Create audio engine
let engine = AudioEngine::new(config)?;

// Define audio callback (real-time safe)
struct MyCallback {
    // State must be Send + Sync
    dsp_processor: Arc<DspProcessor>,
}

impl AudioCallback for MyCallback {
    fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        info: &AudioCallbackInfo,
    ) -> AudioCallbackResult {
        // CRITICAL: No allocations allowed here!
        // CRITICAL: Must complete in <1ms
        
        // Process audio samples
        self.dsp_processor.process_samples(input, output);
        
        AudioCallbackResult::Continue
    }
}

// Start audio processing
let callback = MyCallback {
    dsp_processor: Arc::new(DspProcessor::new()),
};
engine.start(Box::new(callback))?;
```

### Advanced Audio Configuration

```rust
use pancetta_audio::{AudioDeviceInfo, AudioBackend};

// List available audio devices
let devices = AudioEngine::list_devices()?;
for device in devices {
    println!("Device: {} ({})", device.name, device.backend);
    println!("  Channels: {} in, {} out", device.max_input_channels, device.max_output_channels);
    println!("  Sample rates: {:?}", device.supported_sample_rates);
}

// Select specific device
let config = AudioConfig::builder()
    .sample_rate(48000)
    .buffer_size(64)
    .input_device("USB Audio Interface")
    .output_device("USB Audio Interface")
    .backend(AudioBackend::Jack) // Linux JACK backend
    .build()?;
```

### Real-Time Communication

```rust
use pancetta_audio::ringbuffer::{RingBuffer, Producer, Consumer};

// Lock-free communication between threads
let (mut producer, mut consumer) = RingBuffer::<f32>::new(8192);

// In audio callback (real-time thread)
impl AudioCallback for MyCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        // Send audio data to main thread (non-blocking)
        if let Err(_) = self.producer.push_slice(input) {
            // Handle buffer full condition
            tracing::warn!("Audio buffer overflow");
        }
        
        AudioCallbackResult::Continue
    }
}

// In main thread
while let Ok(samples) = consumer.pop_slice(&mut buffer) {
    // Process samples from audio thread
    process_audio_data(&samples);
}
```

### Latency Monitoring

```rust
use pancetta_audio::LatencyMonitor;

let mut monitor = LatencyMonitor::new();

impl AudioCallback for MyCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], info: &AudioCallbackInfo) -> AudioCallbackResult {
        let start = Instant::now();
        
        // Your audio processing here
        self.process_audio(input, output);
        
        // Record latency
        let latency = start.elapsed();
        self.monitor.record_latency(latency);
        
        AudioCallbackResult::Continue
    }
}

// Get latency statistics
let stats = monitor.get_statistics();
println!("Average latency: {:.2}ms", stats.average_ms());
println!("Max latency: {:.2}ms", stats.max_ms());
println!("Dropouts: {}", stats.dropout_count());
```

## FT8 Processing API

### FT8 Decoder Setup

```rust
use pancetta_ft8::{Ft8Decoder, Ft8Config, Ft8Message};

// Configure FT8 decoder
let config = Ft8Config {
    sample_rate: 48000,
    center_frequency: 1500.0, // Hz offset from USB carrier
    decode_depth: 3,           // Processing depth (1-3)
    snr_threshold: -25.0,      // Minimum SNR in dB
    frequency_tolerance: 10.0, // Hz frequency tolerance
    time_tolerance_ms: 500,    // Time sync tolerance
};

let mut decoder = Ft8Decoder::new(config)?;
```

### Real-Time Decoding

```rust
use pancetta_ft8::{Ft8Frame, Ft8Result};

// Process 15-second audio frames for FT8
let frame_samples = 48000 * 15; // 15 seconds at 48kHz
let mut audio_buffer = vec![0.0f32; frame_samples];

// Collect audio data from real-time stream
// ... fill audio_buffer with samples ...

// Create FT8 frame
let frame = Ft8Frame::new(audio_buffer, 48000)?;

// Decode FT8 messages
let results: Vec<Ft8Result> = decoder.decode_frame(&frame)?;

for result in results {
    match result.message {
        Ft8Message::Cq { call_sign, grid_square } => {
            println!("CQ from {} at {}", call_sign, grid_square);
        },
        Ft8Message::Reply { to_call, from_call, signal_report } => {
            println!("{} calling {} ({})", from_call, to_call, signal_report);
        },
        Ft8Message::Report { to_call, from_call, report } => {
            println!("{} -> {}: {}", from_call, to_call, report);
        },
        Ft8Message::RRR73 { to_call, from_call } => {
            println!("{} -> {}: RR73", from_call, to_call);
        },
    }
    
    println!("  SNR: {:.1} dB, Freq: {:.1} Hz, Time: {:.2}s", 
             result.snr, result.frequency, result.time_offset);
}
```

### FT8 Encoder

```rust
use pancetta_ft8::{Ft8Encoder, Ft8MessageBuilder};

let encoder = Ft8Encoder::new(48000)?;

// Build FT8 message
let message = Ft8MessageBuilder::new()
    .cq("W1ABC", "FN42")
    .build()?;

// Generate audio samples
let audio_samples: Vec<f32> = encoder.encode_message(&message, 1500.0)?;
println!("Generated {} samples ({:.2}s)", 
         audio_samples.len(), 
         audio_samples.len() as f32 / 48000.0);

// Transmit through audio system
// ... send audio_samples to output ...
```

### Message Parsing and Validation

```rust
use pancetta_ft8::{Ft8Parser, CallSign, GridSquare};

// Parse raw FT8 message text
let raw_message = "CQ DX W1ABC FN42";
let parsed = Ft8Parser::parse(raw_message)?;

match parsed {
    Ft8Message::Cq { call_sign, grid_square } => {
        // Validate call sign format
        if !call_sign.is_valid() {
            return Err(Ft8Error::InvalidCallSign(call_sign.to_string()));
        }
        
        // Validate grid square
        if !grid_square.is_valid() {
            return Err(Ft8Error::InvalidGridSquare(grid_square.to_string()));
        }
        
        println!("Valid CQ from {} at {}", call_sign, grid_square);
    },
    _ => {
        println!("Other message type: {:?}", parsed);
    }
}
```

## DSP Engine API

### FFT Processing

```rust
use pancetta_dsp::{Fft, Window, WindowType};

// Create FFT processor
let mut fft = Fft::new(2048)?; // 2048-point FFT

// Create windowing function
let window = Window::new(WindowType::Hann, 2048);

// Process audio samples
let mut samples = vec![0.0f32; 2048];
// ... fill samples with audio data ...

// Apply window function
window.apply(&mut samples);

// Forward FFT
let spectrum = fft.forward(&samples)?;

// Process frequency domain data
for (bin, &value) in spectrum.iter().enumerate() {
    let frequency = bin as f32 * 48000.0 / 2048.0;
    let magnitude = value.norm();
    let phase = value.arg();
    
    println!("Bin {}: {:.1} Hz, Mag: {:.3}, Phase: {:.3}", 
             bin, frequency, magnitude, phase);
}

// Inverse FFT (if needed)
let reconstructed = fft.inverse(&spectrum)?;
```

### Digital Filtering

```rust
use pancetta_dsp::{Filter, FilterType, FilterConfig};

// Design a bandpass filter for FT8 (200-3000 Hz)
let filter_config = FilterConfig {
    filter_type: FilterType::BandPass,
    sample_rate: 48000.0,
    low_cutoff: Some(200.0),
    high_cutoff: Some(3000.0),
    order: 4,
};

let mut filter = Filter::new(filter_config)?;

// Process audio samples
let mut samples = vec![0.0f32; 1024];
// ... fill with audio data ...

// Apply filter in-place
filter.process(&mut samples);

// Or process with separate output
let filtered = filter.process_copy(&samples);
```

### Spectrum Analysis

```rust
use pancetta_dsp::{SpectrumAnalyzer, SpectrumConfig};

let config = SpectrumConfig {
    fft_size: 2048,
    overlap_factor: 0.5,
    window_type: WindowType::Hann,
    sample_rate: 48000.0,
};

let mut analyzer = SpectrumAnalyzer::new(config)?;

// Add audio samples
analyzer.add_samples(&audio_samples);

// Get spectrum data
if let Some(spectrum) = analyzer.get_spectrum() {
    for (frequency, magnitude_db) in spectrum.iter() {
        println!("{:.1} Hz: {:.1} dB", frequency, magnitude_db);
    }
}

// Get waterfall data for display
let waterfall_line = analyzer.get_waterfall_line();
```

### Signal Detection

```rust
use pancetta_dsp::{SignalDetector, DetectionConfig, Signal};

let config = DetectionConfig {
    threshold_db: -30.0,
    min_duration_ms: 100.0,
    frequency_range: (200.0, 3000.0),
};

let mut detector = SignalDetector::new(config)?;

// Process spectrum data
let signals: Vec<Signal> = detector.detect(&spectrum_data);

for signal in signals {
    println!("Signal detected:");
    println!("  Frequency: {:.1} Hz", signal.center_frequency);
    println!("  Bandwidth: {:.1} Hz", signal.bandwidth);
    println!("  SNR: {:.1} dB", signal.snr);
    println!("  Duration: {:.1} ms", signal.duration_ms);
}
```

## Configuration API

### Configuration Management

```rust
use pancetta_config::{Config, ConfigBuilder, ConfigPath};
use serde::{Deserialize, Serialize};

// Define custom configuration structure
#[derive(Debug, Serialize, Deserialize)]
struct MyAppConfig {
    audio: AudioSettings,
    ft8: Ft8Settings,
    ui: UiSettings,
}

#[derive(Debug, Serialize, Deserialize)]
struct AudioSettings {
    sample_rate: u32,
    buffer_size: usize,
    input_device: Option<String>,
    output_device: Option<String>,
}

// Load configuration
let config: MyAppConfig = Config::load_from_file("config.toml")?;

// Or use builder pattern
let config = ConfigBuilder::new()
    .with_file("config.toml")
    .with_env_prefix("PANCETTA")
    .with_defaults(MyAppConfig::default())
    .build()?;
```

### Hot Reload Configuration

```rust
use pancetta_config::{ConfigWatcher, ConfigEvent};
use tokio::sync::mpsc;

// Watch configuration file for changes
let (tx, mut rx) = mpsc::channel(100);
let _watcher = ConfigWatcher::new("config.toml", tx)?;

// Handle configuration changes
while let Some(event) = rx.recv().await {
    match event {
        ConfigEvent::Changed(new_config) => {
            println!("Configuration updated: {:?}", new_config);
            // Apply new configuration
            apply_config_changes(&new_config)?;
        },
        ConfigEvent::Error(err) => {
            eprintln!("Configuration error: {}", err);
        },
    }
}
```

### Profile Management

```rust
use pancetta_config::{Profile, ProfileManager};

// Create profile manager
let mut profiles = ProfileManager::new()?;

// Define different profiles
let contest_profile = Profile::builder()
    .name("contest")
    .audio_buffer_size(32)    // Lower latency for contest
    .ft8_decode_depth(1)      // Faster decoding
    .ui_update_rate(30)       // Lower UI update rate
    .build();

let dx_profile = Profile::builder()
    .name("dx")
    .audio_buffer_size(128)   // Higher latency OK for DX
    .ft8_decode_depth(3)      // Deep decoding for weak signals
    .ui_update_rate(60)       // Smoother UI
    .build();

// Register profiles
profiles.register(contest_profile)?;
profiles.register(dx_profile)?;

// Switch profiles
profiles.activate("contest")?;
let active_config = profiles.get_active_config();
```

## TUI Framework API

### Basic TUI Application

```rust
use pancetta_tui::{TuiApp, TuiEvent, TuiResult};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

struct MyTuiApp {
    status_message: String,
}

impl TuiApp for MyTuiApp {
    fn handle_event(&mut self, event: TuiEvent) -> TuiResult<bool> {
        match event {
            TuiEvent::Key(key) => match key.code {
                KeyCode::Char('q') => return Ok(true), // Quit
                KeyCode::Char('r') => {
                    self.status_message = "Refreshed!".to_string();
                },
                _ => {}
            },
            TuiEvent::Tick => {
                // Handle periodic updates
            },
            _ => {}
        }
        Ok(false)
    }
    
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.size();
        
        let paragraph = Paragraph::new(self.status_message.as_str())
            .block(Block::default().title("Status").borders(Borders::ALL));
        
        frame.render_widget(paragraph, area);
    }
}

// Run TUI application
#[tokio::main]
async fn main() -> TuiResult<()> {
    let app = MyTuiApp {
        status_message: "Ready".to_string(),
    };
    
    pancetta_tui::run(app).await
}
```

### Custom Widgets

```rust
use pancetta_tui::widgets::{Waterfall, WaterfallState, SpectrumData};
use ratatui::{buffer::Buffer, layout::Rect, widgets::{Widget, StatefulWidget}};

// Custom waterfall widget
let mut waterfall_state = WaterfallState::new();

// Add spectrum data
let spectrum_data = SpectrumData {
    frequencies: (0..1024).map(|i| i as f32 * 46.875).collect(), // 48kHz / 1024
    magnitudes: magnitude_data,
    timestamp: Instant::now(),
};

waterfall_state.add_spectrum_line(spectrum_data);

// Render waterfall in TUI
impl TuiApp for MyApp {
    fn draw(&mut self, frame: &mut Frame) {
        let waterfall = Waterfall::new()
            .frequency_range(0.0, 3000.0)
            .time_span_seconds(30.0)
            .color_map(ColorMap::Viridis);
        
        frame.render_stateful_widget(waterfall, area, &mut self.waterfall_state);
    }
}
```

### Audio Level Meters

```rust
use pancetta_tui::widgets::{LevelMeter, LevelMeterState};

let mut level_state = LevelMeterState::new();

// Update audio levels
level_state.set_input_level(-12.0);   // dB
level_state.set_output_level(-18.0);  // dB
level_state.set_peak_hold(true);

// Render level meters
let level_meter = LevelMeter::new()
    .range(-60.0, 0.0)
    .show_peak_indicators(true)
    .show_numeric_values(true);

frame.render_stateful_widget(level_meter, area, &mut level_state);
```

## Plugin Development

### Plugin API

```rust
use pancetta::plugin::{Plugin, PluginContext, PluginResult};
use async_trait::async_trait;

// Define a custom plugin
pub struct MyPlugin {
    name: String,
    enabled: bool,
}

#[async_trait]
impl Plugin for MyPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn version(&self) -> &str {
        "1.0.0"
    }
    
    async fn initialize(&mut self, context: &PluginContext) -> PluginResult<()> {
        // Plugin initialization logic
        tracing::info!("Initializing plugin: {}", self.name);
        
        // Access Pancetta services through context
        let audio_engine = context.audio_engine();
        let config = context.config();
        
        Ok(())
    }
    
    async fn process_audio(&mut self, samples: &[f32]) -> PluginResult<Vec<f32>> {
        if !self.enabled {
            return Ok(samples.to_vec());
        }
        
        // Custom audio processing
        let processed = samples.iter()
            .map(|&sample| sample * 0.8) // Example: reduce volume
            .collect();
        
        Ok(processed)
    }
    
    async fn process_ft8_message(&mut self, message: &Ft8Message) -> PluginResult<()> {
        // Handle decoded FT8 messages
        match message {
            Ft8Message::Cq { call_sign, grid_square } => {
                println!("Plugin: CQ from {} at {}", call_sign, grid_square);
                // Custom logic (logging, alerting, etc.)
            },
            _ => {}
        }
        
        Ok(())
    }
    
    async fn shutdown(&mut self) -> PluginResult<()> {
        tracing::info!("Shutting down plugin: {}", self.name);
        Ok(())
    }
}

// Plugin factory function
#[no_mangle]
pub extern "C" fn create_plugin() -> Box<dyn Plugin> {
    Box::new(MyPlugin {
        name: "My Custom Plugin".to_string(),
        enabled: true,
    })
}
```

### Plugin Registration

```rust
use pancetta::plugin::{PluginManager, PluginLoader};

#[tokio::main]
async fn main() -> Result<()> {
    let mut plugin_manager = PluginManager::new();
    
    // Load plugins from directory
    let loader = PluginLoader::new("./plugins")?;
    let plugins = loader.load_all().await?;
    
    // Register plugins
    for plugin in plugins {
        plugin_manager.register(plugin).await?;
    }
    
    // Initialize all plugins
    plugin_manager.initialize_all().await?;
    
    // Use plugins in audio processing pipeline
    let processed_audio = plugin_manager.process_audio(&audio_samples).await?;
    
    Ok(())
}
```

## Integration Examples

### Embedding Pancetta in Your Application

```rust
use pancetta::{PancettaEngine, EngineConfig, EngineEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    // Create event channel for communication
    let (tx, mut rx) = mpsc::channel(1000);
    
    // Configure Pancetta engine
    let config = EngineConfig {
        audio: AudioConfig {
            sample_rate: 48000,
            buffer_size: 64,
            ..Default::default()
        },
        ft8: Ft8Config {
            decode_depth: 2,
            ..Default::default()
        },
        enable_tui: false, // Headless mode
    };
    
    // Start Pancetta engine
    let mut engine = PancettaEngine::new(config, tx)?;
    engine.start().await?;
    
    // Handle events from Pancetta
    while let Some(event) = rx.recv().await {
        match event {
            EngineEvent::Ft8Decoded { message, snr, frequency } => {
                println!("FT8: {} (SNR: {:.1} dB, Freq: {:.1} Hz)", 
                         message, snr, frequency);
                
                // Integrate with your application logic
                handle_ft8_message(&message).await?;
            },
            EngineEvent::AudioLatencyWarning { latency_ms } => {
                eprintln!("Warning: High audio latency: {:.2} ms", latency_ms);
            },
            EngineEvent::Error { error } => {
                eprintln!("Engine error: {}", error);
            },
        }
    }
    
    Ok(())
}

async fn handle_ft8_message(message: &str) -> Result<()> {
    // Your application logic here
    // Examples:
    // - Store in database
    // - Send to web API
    // - Trigger notifications
    // - Log to file
    Ok(())
}
```

### Custom Audio Source Integration

```rust
use pancetta_audio::{AudioSource, AudioSink, AudioFrame};
use std::sync::Arc;

// Implement custom audio source (e.g., network stream, file)
struct NetworkAudioSource {
    receiver: tokio::sync::mpsc::Receiver<AudioFrame>,
}

impl AudioSource for NetworkAudioSource {
    fn read_frame(&mut self) -> Result<AudioFrame> {
        // Non-blocking read from network stream
        match self.receiver.try_recv() {
            Ok(frame) => Ok(frame),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                // Return silence if no data available
                Ok(AudioFrame::silence(1024, 48000))
            },
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                Err(AudioError::SourceDisconnected)
            }
        }
    }
}

// Integrate with Pancetta
let (tx, rx) = tokio::sync::mpsc::channel(100);
let audio_source = NetworkAudioSource { receiver: rx };

let mut engine = PancettaEngine::builder()
    .with_custom_audio_source(Box::new(audio_source))
    .build()?;
```

### Web API Integration

```rust
use warp::Filter;
use serde_json::json;

// Create REST API for Pancetta control
#[tokio::main]
async fn main() {
    let pancetta_engine = Arc::new(Mutex::new(
        PancettaEngine::new(EngineConfig::default(), tx)?
    ));
    
    // GET /status - Get current status
    let status = warp::path("status")
        .and(warp::get())
        .and(with_engine(pancetta_engine.clone()))
        .and_then(get_status);
    
    // POST /start - Start audio processing
    let start = warp::path("start")
        .and(warp::post())
        .and(with_engine(pancetta_engine.clone()))
        .and_then(start_engine);
    
    // GET /metrics - Get performance metrics
    let metrics = warp::path("metrics")
        .and(warp::get())
        .and(with_engine(pancetta_engine.clone()))
        .and_then(get_metrics);
    
    let routes = status.or(start).or(metrics);
    
    warp::serve(routes)
        .run(([127, 0, 0, 1], 8080))
        .await;
}

async fn get_status(engine: Arc<Mutex<PancettaEngine>>) -> Result<impl warp::Reply, warp::Rejection> {
    let engine = engine.lock().await;
    let status = engine.get_status().await;
    
    Ok(warp::reply::json(&json!({
        "running": status.is_running,
        "audio_latency_ms": status.audio_latency_ms,
        "ft8_decodes_count": status.ft8_decodes_count,
        "uptime_seconds": status.uptime.as_secs(),
    })))
}
```

## Performance Guidelines

### Real-Time Programming Best Practices

#### Audio Callback Constraints

```rust
// ❌ NEVER do this in audio callback
impl AudioCallback for BadCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        // These operations are forbidden in real-time context:
        
        // 1. Memory allocation
        let mut buffer = Vec::new(); // ❌ Heap allocation
        
        // 2. Blocking operations
        std::thread::sleep(Duration::from_millis(1)); // ❌ Blocking
        
        // 3. Mutex/locks
        let _guard = self.mutex.lock().unwrap(); // ❌ Can block
        
        // 4. System calls
        println!("Processing audio"); // ❌ I/O operation
        
        // 5. Network operations
        let response = reqwest::get("http://example.com"); // ❌ Network I/O
        
        AudioCallbackResult::Continue
    }
}

// ✅ Correct real-time audio callback
impl AudioCallback for GoodCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        // Pre-allocated buffers (no heap allocation)
        let temp_buffer = &mut self.pre_allocated_buffer[..input.len()];
        
        // Lock-free communication with main thread
        if let Err(_) = self.ringbuffer.push_slice(input) {
            // Handle overflow gracefully (no panic)
            self.metrics.increment_overflow_count();
        }
        
        // Fast, bounded processing only
        for (i, (&inp, out)) in input.iter().zip(output.iter_mut()).enumerate() {
            *out = inp * self.gain; // Simple, predictable operation
        }
        
        AudioCallbackResult::Continue
    }
}
```

#### Memory Management

```rust
// Pre-allocate all buffers during initialization
struct RealTimeProcessor {
    // Pre-allocated working buffers
    temp_buffer: Vec<f32>,
    fft_buffer: Vec<Complex<f32>>,
    window_buffer: Vec<f32>,
    
    // Lock-free communication
    ringbuffer: ringbuf::Producer<f32>,
    
    // Atomic metrics (lock-free)
    processed_samples: AtomicU64,
}

impl RealTimeProcessor {
    fn new(max_buffer_size: usize) -> Self {
        Self {
            // Allocate maximum possible size upfront
            temp_buffer: vec![0.0; max_buffer_size],
            fft_buffer: vec![Complex::new(0.0, 0.0); max_buffer_size],
            window_buffer: vec![0.0; max_buffer_size],
            ringbuffer: create_ringbuffer(),
            processed_samples: AtomicU64::new(0),
        }
    }
}
```

#### Latency Optimization

```rust
// Measure and optimize critical paths
use std::time::Instant;

struct LatencyOptimizedProcessor {
    // Metrics for optimization
    max_process_time: AtomicU64, // nanoseconds
    process_count: AtomicU64,
}

impl AudioCallback for LatencyOptimizedProcessor {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        let start = Instant::now();
        
        // Your processing here
        self.do_processing(input, output);
        
        // Track performance
        let elapsed_ns = start.elapsed().as_nanos() as u64;
        
        // Update metrics atomically
        self.max_process_time.fetch_max(elapsed_ns, Ordering::Relaxed);
        self.process_count.fetch_add(1, Ordering::Relaxed);
        
        // Warn if processing takes too long
        if elapsed_ns > 500_000 { // 0.5ms warning threshold
            // Can't print in real-time thread, use atomic flag
            self.latency_warning.store(true, Ordering::Relaxed);
        }
        
        AudioCallbackResult::Continue
    }
}
```

### CPU Optimization

#### SIMD Processing

```rust
// Use SIMD instructions for bulk operations
use std::simd::{f32x8, Simd};

fn process_samples_simd(input: &[f32], output: &mut [f32], gain: f32) {
    let gain_vec = Simd::splat(gain);
    
    // Process 8 samples at a time
    for (chunk_in, chunk_out) in input.chunks_exact(8).zip(output.chunks_exact_mut(8)) {
        let input_vec = Simd::from_slice(chunk_in);
        let result = input_vec * gain_vec;
        result.copy_to_slice(chunk_out);
    }
    
    // Handle remaining samples
    let remainder_start = (input.len() / 8) * 8;
    for i in remainder_start..input.len() {
        output[i] = input[i] * gain;
    }
}
```

#### Thread Affinity

```rust
use core_affinity;

// Pin real-time threads to specific CPU cores
fn setup_real_time_thread() -> Result<()> {
    // Get available CPU cores
    let core_ids = core_affinity::get_core_ids().unwrap();
    
    // Pin to first core (avoid sharing with other threads)
    if let Some(core_id) = core_ids.first() {
        core_affinity::set_for_current(*core_id);
    }
    
    // Set real-time priority (Linux)
    #[cfg(target_os = "linux")]
    {
        use libc::{sched_setscheduler, sched_param, SCHED_FIFO};
        
        let param = sched_param {
            sched_priority: 80, // High priority
        };
        
        unsafe {
            sched_setscheduler(0, SCHED_FIFO, &param);
        }
    }
    
    Ok(())
}
```

### Memory Usage Optimization

```rust
// Pool allocator for temporary objects
use object_pool::Pool;

struct ProcessorPool {
    buffer_pool: Pool<Vec<f32>>,
    complex_pool: Pool<Vec<Complex<f32>>>,
}

impl ProcessorPool {
    fn new() -> Self {
        Self {
            buffer_pool: Pool::new(32, || vec![0.0f32; 2048]),
            complex_pool: Pool::new(16, || vec![Complex::new(0.0, 0.0); 1024]),
        }
    }
    
    fn process_with_pooled_memory(&self, input: &[f32]) -> Result<Vec<f32>> {
        // Get pre-allocated buffer from pool
        let mut buffer = self.buffer_pool.try_pull()
            .ok_or(ProcessingError::PoolExhausted)?;
        
        // Use buffer for processing
        buffer.clear();
        buffer.extend_from_slice(input);
        
        // Process data...
        self.apply_processing(&mut buffer);
        
        // Return result (buffer automatically returns to pool when dropped)
        Ok(buffer.clone())
    }
}
```

---

*This API documentation covers Pancetta v0.1.0. For the latest updates and examples, visit the [GitHub repository](https://github.com/pancetta-team/pancetta) or check the [online documentation](https://docs.pancetta.dev).*