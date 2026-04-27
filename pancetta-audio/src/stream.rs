//! Audio stream management for real-time processing
//!
//! Provides high-level audio stream creation and management with
//! proper input/output handling for FT8 signal processing.

use crate::{
    device::AudioDeviceManager,
    error::{AudioError, AudioResult},
    latency::CallbackTimer,
    ringbuffer_comm::{
        audio_comm_pair, AudioCommShared, AudioConsumer, AudioProducer, DEFAULT_AUDIO_BUFFER_SIZE,
        DEFAULT_LATENCY_BUFFER_SIZE, OUTPUT_AUDIO_BUFFER_SIZE,
    },
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    InputCallbackInfo, OutputCallbackInfo, Stream,
};

/// Audio stream configuration for FT8 processing
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Target sample rate (12kHz for FT8, or higher for conversion)
    pub sample_rate: u32,
    /// Input channels (typically 1 or 2)
    pub input_channels: u16,
    /// Output channels (typically 2 for stereo monitoring)
    pub output_channels: u16,
    /// Buffer size in frames (affects latency)
    pub buffer_size: u32,
    /// Enable audio monitoring output
    pub enable_monitoring: bool,
    /// Input device name (None for default)
    pub input_device_name: Option<String>,
    /// Output device name (None for default)
    pub output_device_name: Option<String>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,       // Common sample rate, convert to FT8's 12kHz
            input_channels: 1,        // Mono input for FT8
            output_channels: 2,       // Stereo output for monitoring
            buffer_size: 64,          // Low latency
            enable_monitoring: true,  // Enable audio monitoring
            input_device_name: None,  // Use default input
            output_device_name: None, // Use default output
        }
    }
}

impl StreamConfig {
    /// Create FT8-optimized configuration
    pub fn for_ft8() -> Self {
        Self {
            sample_rate: 48000, // Use 48kHz and convert to 12kHz
            input_channels: 1,
            output_channels: 2,
            buffer_size: 64,
            enable_monitoring: true,
            input_device_name: None,
            output_device_name: None,
        }
    }

    /// Create high-quality configuration (48kHz with conversion)
    pub fn for_high_quality() -> Self {
        Self {
            sample_rate: 48000,
            input_channels: 2,
            output_channels: 2,
            buffer_size: 128,
            enable_monitoring: true,
            input_device_name: None,
            output_device_name: None,
        }
    }

    /// Calculate theoretical minimum latency
    pub fn theoretical_latency_ms(&self) -> f64 {
        (self.buffer_size as f64 / self.sample_rate as f64) * 1000.0
    }
}

/// Audio stream manager handling input and output streams
pub struct AudioStreamManager {
    device_manager: AudioDeviceManager,
    config: StreamConfig,
    input_stream: Option<Stream>,
    output_stream: Option<Stream>,
    /// Consumer half — owned by the processing thread
    consumer: Option<AudioConsumer>,
    /// Producer half for output audio — caller pushes TX samples here
    output_producer: Option<AudioProducer>,
    /// Consumer half for output audio — moved into the output stream callback
    output_consumer: Option<AudioConsumer>,
    /// Shared atomic state (stop flag, counters)
    shared: AudioCommShared,
    is_running: bool,
}

impl AudioStreamManager {
    /// Create a new audio stream manager
    pub fn new(config: StreamConfig) -> AudioResult<Self> {
        let device_manager = AudioDeviceManager::new()?;
        let (_producer, consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        let shared = consumer.shared.clone();

        // Create output ring buffer pair for TX audio. Sized to hold a full
        // FT8 transmission (12.64s) so queue_output can push the entire
        // waveform in one call without overrun.
        let (output_producer, output_consumer) =
            audio_comm_pair(OUTPUT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        Ok(Self {
            device_manager,
            config,
            input_stream: None,
            output_stream: None,
            consumer: Some(consumer),
            output_producer: Some(output_producer),
            output_consumer: Some(output_consumer),
            shared,
            is_running: false,
        })
    }

    /// Take the consumer half out of this manager.
    ///
    /// This is intended to be called once after construction so the processing
    /// thread can own the consumer. Returns `None` if already taken.
    pub fn take_consumer(&mut self) -> Option<AudioConsumer> {
        self.consumer.take()
    }

    /// Take the output producer half so the AudioManager can push TX samples.
    ///
    /// Returns `None` if already taken.
    pub fn take_output_producer(&mut self) -> Option<AudioProducer> {
        self.output_producer.take()
    }

    /// Get the shared atomic state (stop flag, counters).
    pub fn get_shared(&self) -> AudioCommShared {
        self.shared.clone()
    }

    /// Get the current configuration
    pub fn get_config(&self) -> &StreamConfig {
        &self.config
    }

    /// Update the configuration (requires restart if running)
    pub fn set_config(&mut self, config: StreamConfig) -> AudioResult<()> {
        if self.is_running {
            return Err(AudioError::configuration(
                "Cannot change configuration while streams are running",
            ));
        }
        self.config = config;
        Ok(())
    }

    /// Start audio streams
    pub fn start(&mut self) -> AudioResult<()> {
        if self.is_running {
            return Err(AudioError::stream("Streams are already running"));
        }

        // Refresh device list
        self.device_manager.refresh_devices()?;

        // Create input stream (moves the producer into the callback)
        self.create_input_stream()?;

        // Recreate output ring buffer pair for this session (TX-sized).
        let (output_producer, output_consumer) =
            audio_comm_pair(OUTPUT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        self.output_producer = Some(output_producer);
        self.output_consumer = Some(output_consumer);

        // Always create output stream — needed for TX audio playback.
        // When not transmitting, the callback simply outputs silence (zero-fill).
        self.create_output_stream()?;

        // Start streams
        if let Some(ref stream) = self.input_stream {
            stream.play().map_err(AudioError::from)?;
        }

        if let Some(ref stream) = self.output_stream {
            stream.play().map_err(AudioError::from)?;
        }

        self.is_running = true;
        Ok(())
    }

    /// Stop audio streams
    pub fn stop(&mut self) -> AudioResult<()> {
        if !self.is_running {
            return Ok(());
        }

        // Signal stop to communication channel
        self.shared.stop();

        // Drop streams (this automatically stops them)
        self.input_stream = None;
        self.output_stream = None;

        self.is_running = false;
        Ok(())
    }

    /// Check if streams are running
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Check if the audio stream has reported an error (e.g. device disconnect)
    pub fn is_error(&self) -> bool {
        self.shared.has_stream_error()
    }

    /// Get current stream statistics
    pub fn get_statistics(&self) -> StreamStatistics {
        // If the consumer is still held here we can query it; otherwise use shared counters.
        let (audio_buffer_used, audio_buffer_capacity) = if let Some(ref consumer) = self.consumer {
            (
                consumer.audio_samples_available(),
                consumer.audio_buffer_capacity(),
            )
        } else {
            (0, 0)
        };

        let dropped = self.shared.dropped_samples();
        let processed = self.shared.processed_samples();
        let total = dropped + processed;
        let drop_rate = if total > 0 {
            (dropped as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let usage_pct = if audio_buffer_capacity > 0 {
            (audio_buffer_used as f64 / audio_buffer_capacity as f64) * 100.0
        } else {
            0.0
        };

        StreamStatistics {
            is_running: self.is_running,
            config: self.config.clone(),
            theoretical_latency_ms: self.config.theoretical_latency_ms(),
            audio_samples_buffered: audio_buffer_used,
            buffer_usage_percent: usage_pct,
            samples_dropped: dropped,
            samples_processed: processed,
            drop_rate_percent: drop_rate,
            has_buffer_overruns: dropped > 0,
        }
    }

    /// Create the input stream for audio capture
    fn create_input_stream(&mut self) -> AudioResult<()> {
        // Log available input devices for diagnostics
        let all_devices = self.device_manager.list_devices();
        for (_, info) in all_devices.iter() {
            if info.supports_input {
                tracing::info!(
                    "Available input: \"{}\" (ch={:?}, rates={:?}, default={})",
                    info.name,
                    info.input_channels,
                    info.input_sample_rates,
                    info.is_default_input,
                );
            }
        }

        // Get input device
        let input_device = if let Some(ref device_name) = self.config.input_device_name {
            if device_name.eq_ignore_ascii_case("default") {
                self.device_manager.get_best_ft8_input_device()?
            } else {
                // Find device by name substring match (case-insensitive).
                // Falls back to best available if no match found.
                match self.device_manager.find_input_device_by_name(device_name) {
                    Ok(device) => device,
                    Err(_) => {
                        tracing::warn!(
                            "Input device matching '{}' not found, falling back to best available",
                            device_name
                        );
                        self.device_manager.get_best_ft8_input_device()?
                    }
                }
            }
        } else {
            self.device_manager.get_best_ft8_input_device()?
        };

        // Get optimal configuration
        let stream_config = self.device_manager.find_optimal_config(
            input_device,
            self.config.sample_rate,
            self.config.input_channels,
            true, // is_input
        )?;

        // Create a fresh producer/consumer pair for this stream session.
        let (mut producer, consumer) =
            audio_comm_pair(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        // Copy the shared state so external code can observe stop/counters.
        self.shared = producer.shared.clone();
        self.consumer = Some(consumer);

        // Build the input stream — the producer is *moved* into the closure so
        // no Arc/Mutex is needed: the ringbuf producer is already Send.
        let sample_format = stream_config.sample_format();
        let config: cpal::StreamConfig = stream_config.into();

        // Clone shared state for the error callbacks so they can set the error flag.
        let err_shared_f32 = self.shared.clone();
        let err_shared_i16 = self.shared.clone();

        // Clone shared state for the I32 error callback
        let err_shared_i32 = self.shared.clone();

        let stream = match sample_format {
            cpal::SampleFormat::F32 => input_device.build_input_stream(
                &config,
                move |data: &[f32], _info: &InputCallbackInfo| {
                    producer.push_audio_slice(data);
                    let timer = CallbackTimer::start();
                    let _ = producer.push_latency(timer.elapsed_ns());
                },
                move |err| {
                    eprintln!("Input stream error: {}", err);
                    err_shared_f32.set_stream_error();
                },
                None,
            )?,
            cpal::SampleFormat::I16 => input_device.build_input_stream(
                &config,
                move |data: &[i16], _info: &InputCallbackInfo| {
                    let float_data: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    producer.push_audio_slice(&float_data);
                    let timer = CallbackTimer::start();
                    let _ = producer.push_latency(timer.elapsed_ns());
                },
                move |err| {
                    eprintln!("Input stream error: {}", err);
                    err_shared_i16.set_stream_error();
                },
                None,
            )?,
            cpal::SampleFormat::I32 => input_device.build_input_stream(
                &config,
                move |data: &[i32], _info: &InputCallbackInfo| {
                    let float_data: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i32::MAX as f32).collect();
                    producer.push_audio_slice(&float_data);
                    let timer = CallbackTimer::start();
                    let _ = producer.push_latency(timer.elapsed_ns());
                },
                move |err| {
                    eprintln!("Input stream error: {}", err);
                    err_shared_i32.set_stream_error();
                },
                None,
            )?,
            format => {
                return Err(crate::AudioError::Stream {
                    message: format!("Unsupported sample format: {:?}", format),
                });
            }
        };

        self.input_stream = Some(stream);
        Ok(())
    }
}

/// Direct cpal enumeration to find an output-capable device by name pattern.
///
/// Matches against the OS-reported device name with case-insensitive,
/// whitespace-normalized substring comparison (handles macOS reporting
/// "USB AUDIO  CODEC" with double-space). When multiple devices match,
/// prefers the one with the most enumerated output configs; ties go to
/// first-encountered, which mirrors tx_test's validated behavior of
/// picking the output-side handle when both halves of a bidirectional
/// codec report zero enumerated output configs.
fn find_output_device_by_cpal(pattern: &str) -> AudioResult<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let normalize = |s: &str| -> String {
        s.split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let needle = normalize(pattern);

    let host = cpal::default_host();
    let mut matches: Vec<cpal::Device> = host
        .devices()
        .map_err(|e| AudioError::device(format!("Failed to enumerate cpal devices: {}", e)))?
        .filter(|d| {
            d.name()
                .map(|n| normalize(&n).contains(&needle))
                .unwrap_or(false)
        })
        .collect();

    if matches.is_empty() {
        return Err(AudioError::device_not_found(pattern.to_string()));
    }

    // Sort by output-config count descending; stable sort means tie-broken
    // by enumeration order (first match wins on ties — this picks the
    // output-side handle of bidirectional CODECs that report 0 configs).
    matches.sort_by_key(|d| {
        let out_count = d.supported_output_configs().map(|c| c.count()).unwrap_or(0);
        std::cmp::Reverse(out_count)
    });

    let chosen = matches.remove(0);
    let chosen_name = chosen.name().unwrap_or_else(|_| "unknown".into());
    tracing::info!(
        "find_output_device_by_cpal: matched '{}' for pattern '{}'",
        chosen_name,
        pattern
    );
    Ok(chosen)
}

#[allow(dead_code)]
impl AudioStreamManager {
    /// Create the output stream for audio monitoring
    fn create_output_stream(&mut self) -> AudioResult<()> {
        // Output device discovery: bypass AudioDeviceManager and enumerate
        // cpal directly, mirroring tx_test's validated pattern. The manager's
        // cached `Device` references can refer to the input-side handle of a
        // bidirectional USB CODEC (e.g., the FTdx10 BurrBrown chip), which
        // accepts output stream creation on macOS but fails with "device no
        // longer available". Re-enumerating per-call returns a fresh handle
        // and lets us pick by output capability deterministically.
        let cpal_output_device = if let Some(ref device_name) = self.config.output_device_name {
            if device_name.eq_ignore_ascii_case("default") {
                cpal::default_host()
                    .default_output_device()
                    .ok_or_else(|| AudioError::stream("No default output device available"))?
            } else {
                find_output_device_by_cpal(device_name).or_else(|e| {
                    tracing::warn!(
                        "Output device matching '{}' not found via cpal ({}), falling back to default",
                        device_name,
                        e
                    );
                    cpal::default_host().default_output_device().ok_or_else(|| {
                        AudioError::stream("No default output device available")
                    })
                })?
            }
        } else {
            cpal::default_host()
                .default_output_device()
                .ok_or_else(|| AudioError::stream("No default output device available"))?
        };
        let output_device = &cpal_output_device;

        // FT8 audio is mono. Force mono regardless of what
        // find_optimal_output_config returned — many USB CODECs report
        // stereo capability but the TX path produces single-channel
        // samples, and we don't want cpal to interpret our mono buffer
        // as interleaved stereo. tx_test validated this pattern.
        let target_rate = self.config.sample_rate;
        let stream_config = cpal::StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(target_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        // Probe the device for log visibility — failures here are non-fatal
        // because we're force-opening with a known-good config.
        match self.device_manager.find_optimal_config(
            output_device,
            target_rate,
            1, // mono
            false,
        ) {
            Ok(probed) => tracing::info!(
                "Output device probe: {}Hz/{}ch (forcing mono/{}Hz for TX stream)",
                probed.sample_rate().0,
                probed.channels(),
                target_rate
            ),
            Err(e) => tracing::warn!(
                "Output device probe failed ({}); force-opening mono/{}Hz",
                e,
                target_rate
            ),
        }

        // Take the output consumer — move into the callback closure
        let mut output_consumer = self
            .output_consumer
            .take()
            .ok_or_else(|| AudioError::stream("Output consumer already taken"))?;

        // Create the output stream — drain TX samples from the ring buffer
        let stream = output_device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _info: &OutputCallbackInfo| {
                let read = output_consumer.pop_audio_slice(data);
                // Fill any remaining samples with silence (underrun is normal when not transmitting)
                for sample in data[read..].iter_mut() {
                    *sample = 0.0;
                }
            },
            |err| {
                eprintln!("Output stream error: {}", err);
            },
            None,
        )?;

        self.output_stream = Some(stream);
        Ok(())
    }
}

impl Drop for AudioStreamManager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Stream performance statistics
#[derive(Debug, Clone)]
pub struct StreamStatistics {
    /// Whether streams are currently running
    pub is_running: bool,
    /// Current stream configuration
    pub config: StreamConfig,
    /// Theoretical minimum latency in milliseconds
    pub theoretical_latency_ms: f64,
    /// Number of audio samples currently buffered
    pub audio_samples_buffered: usize,
    /// Buffer usage as percentage
    pub buffer_usage_percent: f64,
    /// Total samples dropped due to buffer overruns
    pub samples_dropped: u64,
    /// Total samples successfully processed
    pub samples_processed: u64,
    /// Sample drop rate as percentage
    pub drop_rate_percent: f64,
    /// Whether buffer overruns are occurring
    pub has_buffer_overruns: bool,
}

impl StreamStatistics {
    /// Check if the stream is healthy (low drop rate, not overrunning)
    pub fn is_healthy(&self) -> bool {
        self.is_running
            && self.drop_rate_percent < 1.0  // Less than 1% drops
            && self.buffer_usage_percent < 80.0 // Not near buffer capacity
    }

    /// Get a human-readable status description
    pub fn status_description(&self) -> String {
        if !self.is_running {
            "Stopped".to_string()
        } else if self.is_healthy() {
            "Healthy".to_string()
        } else if self.has_buffer_overruns {
            "Buffer Overruns".to_string()
        } else if self.buffer_usage_percent > 90.0 {
            "High Buffer Usage".to_string()
        } else {
            "Degraded".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_config_defaults() {
        let config = StreamConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.input_channels, 1);
        assert_eq!(config.output_channels, 2);
        assert!(config.enable_monitoring);
    }

    #[test]
    fn test_ft8_config() {
        let config = StreamConfig::for_ft8();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.input_channels, 1);

        // Should have low latency
        assert!(config.theoretical_latency_ms() < 10.0);
    }

    #[test]
    fn test_high_quality_config() {
        let config = StreamConfig::for_high_quality();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.input_channels, 2);
    }

    #[test]
    fn test_latency_calculation() {
        let config = StreamConfig {
            sample_rate: 48000,
            buffer_size: 64,
            ..Default::default()
        };

        // 64 samples at 48kHz = 1.333ms
        let expected_latency = 64.0 / 48000.0 * 1000.0;
        assert!((config.theoretical_latency_ms() - expected_latency).abs() < 0.001);
    }

    #[test]
    fn test_stream_manager_creation() {
        let config = StreamConfig::for_ft8();
        let manager = AudioStreamManager::new(config);

        // Should succeed even without audio devices in test environment
        if let Ok(manager) = manager {
            assert!(!manager.is_running());
            assert_eq!(manager.get_config().sample_rate, 48000);
        }
    }

    #[test]
    fn test_stream_statistics() {
        let stats = StreamStatistics {
            is_running: true,
            config: StreamConfig::for_ft8(),
            theoretical_latency_ms: 5.3,
            audio_samples_buffered: 10,
            buffer_usage_percent: 50.0,
            samples_dropped: 0,
            samples_processed: 1000,
            drop_rate_percent: 0.0,
            has_buffer_overruns: false,
        };

        assert!(stats.is_healthy());
        assert_eq!(stats.status_description(), "Healthy");
    }

    #[test]
    fn test_unhealthy_stream_statistics() {
        let stats = StreamStatistics {
            is_running: true,
            config: StreamConfig::for_ft8(),
            theoretical_latency_ms: 5.3,
            audio_samples_buffered: 95,
            buffer_usage_percent: 95.0,
            samples_dropped: 50,
            samples_processed: 1000,
            drop_rate_percent: 5.0,
            has_buffer_overruns: true,
        };

        assert!(!stats.is_healthy());
        assert_eq!(stats.status_description(), "Buffer Overruns");
    }
}
