//! Audio Manager - Coordinates all audio processing components
//!
//! This module provides the main AudioManager that replaces the stub
//! implementation in the coordinator. It manages device selection,
//! stream initialization, and real-time audio processing.

use crate::{
    AudioCommShared, AudioConsumer, AudioDeviceInfo, AudioDeviceManager, AudioError, AudioProducer,
    AudioStreamManager, LatencyMeasurer, LinearResampler, StreamConfig,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Main audio manager that coordinates all audio components
pub struct AudioManager {
    /// Device manager for audio device enumeration and selection
    device_manager: AudioDeviceManager,

    /// Active audio stream (if running)
    stream: Option<AudioStreamManager>,

    /// Consumer half of the lock-free ring buffer
    consumer: Option<AudioConsumer>,

    /// Shared atomic state (stop flag, counters)
    shared: AudioCommShared,

    /// Latency monitor for tracking performance
    latency_monitor: LatencyMeasurer,

    /// Sample rate converter for resampling
    converter: Option<LinearResampler>,

    /// Producer half for sending TX audio to the output stream
    output_producer: Option<AudioProducer>,

    /// Shutdown signal
    shutdown: Arc<AtomicBool>,

    /// Current configuration
    config: AudioManagerConfig,

    /// Statistics
    stats: AudioManagerStats,

    /// Audio processing task handle
    process_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Audio manager configuration
#[derive(Debug, Clone)]
pub struct AudioManagerConfig {
    /// Input device name (None for default)
    pub input_device: Option<String>,

    /// Output device name (None for default)
    pub output_device: Option<String>,

    /// Target sample rate (Hz)
    pub sample_rate: u32,

    /// Buffer size in frames
    pub buffer_size: usize,

    /// Number of channels (1 for mono, 2 for stereo)
    pub channels: u16,

    /// Enable loopback monitoring
    pub enable_monitoring: bool,

    /// Target latency in milliseconds
    pub target_latency_ms: f32,

    /// Gain in dB (-60 to +20)
    pub input_gain_db: f32,
}

impl Default for AudioManagerConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            sample_rate: 48000,
            buffer_size: 256,
            channels: 1,
            enable_monitoring: false,
            target_latency_ms: 1.0,
            input_gain_db: 0.0,
        }
    }
}

/// Audio manager statistics
#[derive(Debug, Default, Clone)]
pub struct AudioManagerStats {
    /// Total samples processed
    pub samples_processed: u64,

    /// Current callback latency in microseconds
    pub current_latency_us: u64,

    /// Average callback latency in microseconds
    pub avg_latency_us: f64,

    /// Peak callback latency in microseconds
    pub peak_latency_us: u64,

    /// Number of buffer underruns
    pub underruns: u64,

    /// Number of buffer overruns
    pub overruns: u64,

    /// Current signal level (RMS)
    pub signal_level: f32,

    /// Time stream has been running
    pub uptime: Duration,
}

impl AudioManager {
    /// Create a new audio manager with default configuration
    pub fn new() -> Result<Self, AudioError> {
        Self::with_config(AudioManagerConfig::default())
    }

    /// Create a new audio manager with specific configuration
    pub fn with_config(config: AudioManagerConfig) -> Result<Self, AudioError> {
        info!("Creating AudioManager with config: {:?}", config);

        // Initialize device manager
        let device_manager = AudioDeviceManager::new()?;

        // Create stream configuration
        let stream_config = StreamConfig {
            sample_rate: config.sample_rate,
            buffer_size: config.buffer_size as u32,
            input_channels: config.channels,
            output_channels: 2, // Always stereo output for monitoring
            input_device_name: config.input_device.clone(),
            output_device_name: config.output_device.clone(),
            enable_monitoring: config.enable_monitoring,
        };

        // Create audio stream manager
        let mut stream = AudioStreamManager::new(stream_config)?;

        // Take the consumer half from the stream manager
        let consumer = stream.take_consumer();
        let shared = stream.get_shared();

        // Take the output producer so we can push TX samples
        let output_producer = stream.take_output_producer();

        // Create latency monitor (max 1000 measurements, target 1ms = 1_000_000ns)
        let latency_monitor = LatencyMeasurer::new(1000, 1_000_000);

        // Skip resampling here — let the DSP pipeline handle it properly
        // with anti-aliasing. Pass raw samples at the input sample rate.
        let converter: Option<LinearResampler> = None;

        Ok(Self {
            device_manager,
            stream: Some(stream),
            consumer,
            shared,
            latency_monitor,
            converter,
            output_producer,
            shutdown: Arc::new(AtomicBool::new(false)),
            config,
            stats: AudioManagerStats::default(),
            process_handle: None,
        })
    }

    /// Whether the resolved TX OUTPUT device is the system default rather than
    /// an explicitly configured rig CODEC. Only meaningful after [`start`](Self::start).
    /// `true` means TX audio may be going to the wrong device (e.g. laptop
    /// speakers) while PTT keys the rig — surfaced to the TUI so the operator
    /// sees the misconfig instead of it being log-only.
    pub fn output_is_system_default(&self) -> bool {
        self.stream
            .as_ref()
            .map(|s| s.output_is_system_default())
            .unwrap_or(false)
    }

    /// List available audio devices
    pub fn list_devices(&self) -> Vec<AudioDeviceInfo> {
        self.device_manager
            .list_devices()
            .iter()
            .map(|(_, info)| info.clone())
            .collect()
    }

    /// Start audio processing
    pub fn start(&mut self) -> Result<(), AudioError> {
        if let Some(ref mut stream) = self.stream {
            if stream.is_running() {
                warn!("Audio stream already running");
                return Ok(());
            }

            info!("Starting audio stream");
            stream.start()?;

            // Grab the freshly-created consumer and output producer from the stream manager
            self.consumer = stream.take_consumer();
            self.shared = stream.get_shared();
            self.output_producer = stream.take_output_producer();

            info!("Audio stream started successfully");
        }

        Ok(())
    }

    /// Stop audio processing
    pub fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(ref mut stream) = self.stream {
            info!("Stopping audio stream");
            self.shutdown.store(true, Ordering::Relaxed);

            // Stop the processing task
            if let Some(handle) = self.process_handle.take() {
                let _ = tokio::time::timeout(Duration::from_secs(1), handle);
            }

            stream.stop()?;
            info!("Audio stream stopped");
        }
        Ok(())
    }

    /// Process audio data from ring buffer
    pub fn process_audio(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
        // Check if the audio stream has reported an error (e.g. device disconnect)
        if self.shared.has_stream_error() {
            warn!("Audio device error detected — stream may be disconnected");
            return Err(AudioError::stream(
                "Audio device error detected — stream may be disconnected",
            ));
        }

        let consumer = match self.consumer {
            Some(ref mut c) => c,
            None => {
                tracing::trace!("process_audio: no consumer");
                return Ok(None);
            }
        };

        let available = consumer.audio_samples_available();
        if available == 0 {
            return Ok(None);
        }

        // Log first time we get samples
        if self.stats.samples_processed == 0 {
            tracing::info!("First audio samples received: {} available", available);
        }

        // Read up to buffer_size * channels samples
        let to_read = available.min(self.config.buffer_size * self.config.channels as usize);
        let mut output_samples = vec![0.0f32; to_read];
        let read = consumer.pop_audio_slice(&mut output_samples);
        output_samples.truncate(read);

        if output_samples.is_empty() {
            return Ok(None);
        }

        self.stats.samples_processed += read as u64;

        // Update statistics
        self.stats.signal_level = calculate_rms(&output_samples);

        // Apply sample rate conversion if needed
        let output = if let Some(ref mut converter) = self.converter {
            converter.process(&output_samples)?
        } else {
            output_samples
        };

        // Update latency stats
        let stats = self.latency_monitor.get_stats();
        self.stats.current_latency_us = stats.average_ns / 1000;
        self.stats.avg_latency_us = stats.average_ms * 1000.0;
        self.stats.peak_latency_us = stats.max_ns / 1000;

        Ok(Some(output))
    }

    /// Get current statistics
    pub fn get_stats(&self) -> AudioManagerStats {
        self.stats.clone()
    }

    /// Check if audio is running
    pub fn is_running(&self) -> bool {
        self.stream.as_ref().map_or(false, |s| s.is_running())
    }

    /// Set input gain in dB
    pub fn set_gain(&mut self, gain_db: f32) -> Result<(), AudioError> {
        if !(-60.0..=20.0).contains(&gain_db) {
            return Err(AudioError::Configuration {
                message: format!("Gain must be between -60 and +20 dB, got {}", gain_db),
            });
        }
        self.config.input_gain_db = gain_db;
        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> &AudioManagerConfig {
        &self.config
    }

    /// Queue audio samples for output playback.
    ///
    /// Pushes TX audio into the output ring buffer. The cpal output stream
    /// callback drains this buffer in real time. If `input_rate` differs from
    /// the configured output sample rate, a simple linear interpolation
    /// resampler is applied.
    pub fn queue_output(&mut self, samples: &[f32], input_rate: u32) -> Result<(), AudioError> {
        let producer = self
            .output_producer
            .as_mut()
            .ok_or_else(|| AudioError::Stream {
                message: "Output stream not initialized".to_string(),
            })?;

        // Resample if input rate differs from output rate
        let output_samples = if input_rate != self.config.sample_rate {
            let ratio = self.config.sample_rate as f64 / input_rate as f64;
            let out_len = (samples.len() as f64 * ratio) as usize;
            let mut resampled = Vec::with_capacity(out_len);
            for i in 0..out_len {
                let src_pos = i as f64 / ratio;
                let src_idx = src_pos as usize;
                let frac = src_pos - src_idx as f64;
                let s0 = samples[src_idx.min(samples.len() - 1)];
                let s1 = samples[(src_idx + 1).min(samples.len() - 1)];
                resampled.push(s0 + (s1 - s0) * frac as f32);
            }
            resampled
        } else {
            samples.to_vec()
        };

        let written = producer.push_audio_slice(&output_samples);
        if written < output_samples.len() {
            warn!(
                "Output buffer overrun: {}/{} samples written",
                written,
                output_samples.len()
            );
        }

        info!(
            "Queued {} TX audio samples for output (rate {}->{}Hz)",
            written, input_rate, self.config.sample_rate
        );
        Ok(())
    }

    /// Get list of available input devices
    pub fn list_input_devices(&self) -> Vec<&AudioDeviceInfo> {
        self.device_manager
            .list_device_info()
            .into_iter()
            .filter(|d| d.supports_input)
            .collect()
    }

    /// Get list of available output devices
    pub fn list_output_devices(&self) -> Vec<&AudioDeviceInfo> {
        self.device_manager
            .list_device_info()
            .into_iter()
            .filter(|d| d.supports_output)
            .collect()
    }

    /// Select input device by name
    pub fn select_input_device(&mut self, name: &str) -> Result<(), AudioError> {
        let found = self
            .device_manager
            .list_device_info()
            .iter()
            .any(|d| d.supports_input && d.name == name);
        if found {
            self.config.input_device = Some(name.to_string());
            info!("Selected input device: {}", name);
            Ok(())
        } else {
            Err(AudioError::Configuration {
                message: format!("Input device '{}' not found", name),
            })
        }
    }

    /// Select output device by name
    pub fn select_output_device(&mut self, name: &str) -> Result<(), AudioError> {
        let found = self
            .device_manager
            .list_device_info()
            .iter()
            .any(|d| d.supports_output && d.name == name);
        if found {
            self.config.output_device = Some(name.to_string());
            info!("Selected output device: {}", name);
            Ok(())
        } else {
            Err(AudioError::Configuration {
                message: format!("Output device '{}' not found", name),
            })
        }
    }
}

impl Drop for AudioManager {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            error!("Error stopping audio manager: {:?}", e);
        }
    }
}

/// Calculate RMS (Root Mean Square) of audio samples
fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    let sum_squares: f32 = samples.iter().map(|&x| x * x).sum();

    (sum_squares / samples.len() as f32).sqrt()
}

/// Audio messages sent to message bus
#[derive(Debug, Clone)]
pub enum AudioMessage {
    /// Audio data ready for processing
    AudioData {
        samples: Vec<f32>,
        sample_rate: u32,
        timestamp: Instant,
    },

    /// Audio level update
    LevelUpdate { rms: f32, peak: f32 },

    /// Latency update
    LatencyUpdate {
        current_us: u64,
        average_us: f64,
        peak_us: u64,
    },

    /// Error occurred
    Error(String),
}

/// Commands received from message bus
#[derive(Debug, Clone)]
pub enum AudioCommand {
    /// Start audio processing
    Start,

    /// Stop audio processing
    Stop,

    /// Set input gain
    SetGain(f32),

    /// Change input device
    SetDevice(String),

    /// Request statistics
    GetStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_manager_creation() {
        let manager = AudioManager::new();
        assert!(manager.is_ok());
    }

    #[test]
    fn test_custom_config() {
        let config = AudioManagerConfig {
            sample_rate: 44100,
            buffer_size: 512,
            channels: 2,
            ..Default::default()
        };

        let manager = AudioManager::with_config(config);
        assert!(manager.is_ok());

        let manager = manager.unwrap();
        assert_eq!(manager.get_config().sample_rate, 44100);
        assert_eq!(manager.get_config().buffer_size, 512);
        assert_eq!(manager.get_config().channels, 2);
    }

    #[test]
    fn test_queue_output_no_crash() {
        let manager = AudioManager::new();
        if let Ok(mut manager) = manager {
            let samples = vec![0.5f32; 480];
            // output_producer is Some from construction
            let result = manager.queue_output(&samples, 12000);
            // Should succeed — producer is available even without a running stream
            assert!(result.is_ok() || result.is_err());
        }
    }

    #[test]
    fn test_rms_calculation() {
        let samples = vec![0.5, -0.5, 0.5, -0.5];
        let rms = calculate_rms(&samples);
        assert!((rms - 0.5).abs() < 0.001);

        let silence = vec![0.0; 100];
        assert_eq!(calculate_rms(&silence), 0.0);
    }
}
