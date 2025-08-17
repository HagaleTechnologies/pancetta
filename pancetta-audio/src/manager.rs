//! Audio Manager - Coordinates all audio processing components
//! 
//! This module provides the main AudioManager that replaces the stub
//! implementation in the coordinator. It manages device selection,
//! stream initialization, and real-time audio processing.

use crate::{
    AudioDeviceManager, AudioStreamManager, StreamConfig, AudioError,
    AudioComm, LatencyMeasurer, LinearResampler, AudioDeviceInfo,
    AudioSample
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Main audio manager that coordinates all audio components
pub struct AudioManager {
    /// Device manager for audio device enumeration and selection
    device_manager: AudioDeviceManager,
    
    /// Active audio stream (if running)
    stream: Option<AudioStreamManager>,
    
    /// Shared ring buffer for audio communication
    audio_comm: Arc<AudioComm>,
    
    /// Latency monitor for tracking performance
    latency_monitor: LatencyMeasurer,
    
    /// Sample rate converter for resampling
    converter: Option<LinearResampler>,
    
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
        let stream = AudioStreamManager::new(stream_config)?;
        
        // Get the shared AudioComm from the stream
        let audio_comm = stream.get_comm();
        
        // Create latency monitor (max 1000 measurements, target 1ms = 1_000_000ns)
        let latency_monitor = LatencyMeasurer::new(1000, 1_000_000);
        
        // Initialize sample rate converter if needed
        let converter = if config.sample_rate != 12000 {
            Some(LinearResampler::new(
                config.sample_rate,
                12000, // FT8 requires 12kHz
                config.channels,
            )?)
        } else {
            None
        };
        
        Ok(Self {
            device_manager,
            stream: Some(stream),
            audio_comm,
            latency_monitor,
            converter,
            shutdown: Arc::new(AtomicBool::new(false)),
            config,
            stats: AudioManagerStats::default(),
            process_handle: None,
        })
    }
    
    /// List available audio devices
    pub fn list_devices(&self) -> Vec<AudioDeviceInfo> {
        self.device_manager.list_devices()
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
            
            // Start processing task to apply gain and convert samples
            let audio_comm = Arc::clone(&self.audio_comm);
            let gain_db = self.config.input_gain_db;
            let shutdown = Arc::clone(&self.shutdown);
            
            let handle = tokio::spawn(async move {
                let gain = 10.0_f32.powf(gain_db / 20.0);
                
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    
                    // Check for audio samples from the stream
                    if let Some(sample) = audio_comm.pop_audio_sample() {
                        // Apply gain to the samples
                        let processed = AudioSample::new(
                            sample.data.iter()
                                .map(|&s| s * gain)
                                .collect(),
                            sample.sample_rate,
                            sample.channels,
                        );
                        
                        // Try to push back processed sample
                        if let Err(_) = audio_comm.push_audio_sample(processed) {
                            // Buffer full, sample dropped
                        }
                    } else {
                        // No samples available, yield
                        tokio::time::sleep(Duration::from_micros(100)).await;
                    }
                }
            });
            
            self.process_handle = Some(handle);
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
                // Don't wait forever, just abort if it doesn't stop
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    handle
                );
            }
            
            stream.stop()?;
            info!("Audio stream stopped");
        }
        Ok(())
    }
    
    /// Process audio data from ring buffer
    pub fn process_audio(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
        // Read samples from the shared AudioComm
        let mut output_samples = Vec::new();
        
        // Pop available samples (up to buffer_size)
        for _ in 0..self.config.buffer_size {
            if let Some(sample) = self.audio_comm.pop_audio_sample() {
                output_samples.extend_from_slice(&sample.data);
                self.stats.samples_processed += sample.data.len() as u64;
            } else {
                break;
            }
        }
        
        if output_samples.is_empty() {
            return Ok(None);
        }
        
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
        self.stats.current_latency_us = stats.average_ns / 1000; // Convert ns to us
        self.stats.avg_latency_us = stats.average_ms * 1000.0; // Convert ms to us
        self.stats.peak_latency_us = stats.max_ns / 1000; // Convert ns to us
        
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
        if gain_db < -60.0 || gain_db > 20.0 {
            return Err(AudioError::Configuration {
                message: format!("Gain must be between -60 and +20 dB, got {}", gain_db)
            });
        }
        self.config.input_gain_db = gain_db;
        Ok(())
    }
    
    /// Get current configuration
    pub fn get_config(&self) -> &AudioManagerConfig {
        &self.config
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
    
    let sum_squares: f32 = samples.iter()
        .map(|&x| x * x)
        .sum();
    
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
    LevelUpdate {
        rms: f32,
        peak: f32,
    },
    
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
    fn test_rms_calculation() {
        let samples = vec![0.5, -0.5, 0.5, -0.5];
        let rms = calculate_rms(&samples);
        assert!((rms - 0.5).abs() < 0.001);
        
        let silence = vec![0.0; 100];
        assert_eq!(calculate_rms(&silence), 0.0);
    }
}