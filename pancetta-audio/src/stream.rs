//! Audio stream management for real-time processing
//!
//! Provides high-level audio stream creation and management with
//! proper input/output handling for FT8 signal processing.

use crate::{
    device::AudioDeviceManager,
    error::{AudioError, AudioResult},
    ringbuffer_comm::{AudioComm, AudioSample, DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE},
    latency::CallbackTimer,
};
use cpal::{
    traits::{DeviceTrait, StreamTrait},
    Stream, InputCallbackInfo, OutputCallbackInfo,
};
use std::sync::Arc;

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
            sample_rate: 48000,      // Use 48kHz and convert to 12kHz
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
    comm: Arc<AudioComm>,
    is_running: bool,
}

impl AudioStreamManager {
    /// Create a new audio stream manager
    pub fn new(config: StreamConfig) -> AudioResult<Self> {
        let device_manager = AudioDeviceManager::new()?;
        let comm = Arc::new(AudioComm::new(
            DEFAULT_AUDIO_BUFFER_SIZE,
            DEFAULT_LATENCY_BUFFER_SIZE,
        ));

        Ok(Self {
            device_manager,
            config,
            input_stream: None,
            output_stream: None,
            comm,
            is_running: false,
        })
    }

    /// Get the communication channel for audio data
    pub fn get_comm(&self) -> Arc<AudioComm> {
        self.comm.clone()
    }

    /// Get the current configuration
    pub fn get_config(&self) -> &StreamConfig {
        &self.config
    }

    /// Update the configuration (requires restart if running)
    pub fn set_config(&mut self, config: StreamConfig) -> AudioResult<()> {
        if self.is_running {
            return Err(AudioError::configuration(
                "Cannot change configuration while streams are running"
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

        // Create input stream
        self.create_input_stream()?;

        // Create output stream if monitoring is enabled
        if self.config.enable_monitoring {
            self.create_output_stream()?;
        }

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
        self.comm.stop();

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

    /// Get current stream statistics
    pub fn get_statistics(&self) -> StreamStatistics {
        let buffer_stats = self.comm.get_buffer_stats();
        
        StreamStatistics {
            is_running: self.is_running,
            config: self.config.clone(),
            theoretical_latency_ms: self.config.theoretical_latency_ms(),
            audio_samples_buffered: buffer_stats.audio_buffer_used,
            buffer_usage_percent: buffer_stats.audio_buffer_usage_percent,
            samples_dropped: buffer_stats.dropped_samples,
            samples_processed: buffer_stats.processed_samples,
            drop_rate_percent: buffer_stats.drop_rate_percent(),
            has_buffer_overruns: buffer_stats.has_buffer_overruns(),
        }
    }

    /// Create the input stream for audio capture
    fn create_input_stream(&mut self) -> AudioResult<()> {
        // Get input device
        let input_device = if let Some(ref device_name) = self.config.input_device_name {
            // Find specific device by name
            let devices = self.device_manager.list_devices();
            devices
                .iter()
                .find(|(_, info)| info.name == *device_name && info.supports_input)
                .map(|(device, _)| device)
                .ok_or_else(|| {
                    AudioError::device(format!("Input device '{}' not found", device_name))
                })?
        } else {
            // Use best available input device
            self.device_manager.get_best_ft8_input_device()?
        };

        // Get optimal configuration
        let stream_config = self.device_manager.find_optimal_config(
            input_device,
            self.config.sample_rate,
            self.config.input_channels,
            true, // is_input
        )?;

        // Create the input stream
        let comm = self.comm.clone();
        let config = self.config.clone();
        let actual_sample_rate = stream_config.sample_rate().0;
        let actual_channels = stream_config.channels();

        let stream = input_device.build_input_stream(
            &stream_config.into(),
            move |data: &[f32], _info: &InputCallbackInfo| {
                Self::input_callback(data, &comm, actual_sample_rate, actual_channels, &config);
            },
            |err| {
                eprintln!("Input stream error: {}", err);
            },
            None,
        )?;

        self.input_stream = Some(stream);
        Ok(())
    }

    /// Create the output stream for audio monitoring
    fn create_output_stream(&mut self) -> AudioResult<()> {
        // Get output device
        let output_device = if let Some(ref device_name) = self.config.output_device_name {
            // Find specific device by name
            let devices = self.device_manager.list_devices();
            devices
                .iter()
                .find(|(_, info)| info.name == *device_name && info.supports_output)
                .map(|(device, _)| device)
                .ok_or_else(|| {
                    AudioError::device(format!("Output device '{}' not found", device_name))
                })?
        } else {
            // Use best available output device
            self.device_manager.get_best_output_device()?
        };

        // Get optimal configuration
        let stream_config = self.device_manager.find_optimal_config(
            output_device,
            self.config.sample_rate,
            self.config.output_channels,
            false, // is_input
        )?;

        // Create the output stream
        let comm = self.comm.clone();

        let stream = output_device.build_output_stream(
            &stream_config.into(),
            move |data: &mut [f32], _info: &OutputCallbackInfo| {
                Self::output_callback(data, &comm);
            },
            |err| {
                eprintln!("Output stream error: {}", err);
            },
            None,
        )?;

        self.output_stream = Some(stream);
        Ok(())
    }

    /// Real-time input callback - processes incoming audio
    fn input_callback(
        data: &[f32],
        comm: &Arc<AudioComm>,
        sample_rate: u32,
        channels: u16,
        _config: &StreamConfig,
    ) {
        let timer = CallbackTimer::start();

        // Create audio sample from input data
        let audio_sample = AudioSample::new(data.to_vec(), sample_rate, channels);

        // Try to push to the ring buffer (non-blocking)
        if let Err(_) = comm.push_audio_sample(audio_sample) {
            // Buffer is full - this indicates the processing chain is too slow
            // The dropped sample counter will be incremented automatically
        }

        // Record callback latency
        let latency_ns = timer.elapsed_ns();
        let _ = comm.push_latency(latency_ns);
    }

    /// Real-time output callback - generates monitoring audio
    fn output_callback(data: &mut [f32], _comm: &Arc<AudioComm>) {
        // For now, just generate silence for monitoring
        // In a full implementation, this would play back received audio
        for sample in data.iter_mut() {
            *sample = 0.0;
        }

        // Could also mix in tone or received audio for monitoring
        // This is where you'd implement sidetone or audio monitoring
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
            && self.buffer_usage_percent < 80.0  // Not near buffer capacity
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