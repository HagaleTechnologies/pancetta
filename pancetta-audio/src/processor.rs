//! High-level audio processor for FT8 signal processing
//!
//! Provides a complete audio processing pipeline with device management,
//! sample rate conversion, and message bus integration.

use crate::{
    converter::{ResamplerFactory, SampleRateConverter},
    device::AudioDeviceManager,
    error::{AudioError, AudioResult},
    ringbuffer_comm::{AudioComm, AudioSample},
    stream::{AudioStreamManager, StreamConfig, StreamStatistics},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Audio processor configuration
#[derive(Debug, Clone)]
pub struct AudioProcessorConfig {
    /// Stream configuration
    pub stream_config: StreamConfig,
    /// Target sample rate for processing (typically 12kHz for FT8)
    pub target_sample_rate: u32,
    /// Enable high-quality sample rate conversion
    pub high_quality_conversion: bool,
    /// Maximum buffer latency in milliseconds
    pub max_buffer_latency_ms: f64,
    /// Enable audio statistics collection
    pub enable_statistics: bool,
    /// Statistics update interval
    pub stats_update_interval: Duration,
}

impl Default for AudioProcessorConfig {
    fn default() -> Self {
        Self {
            stream_config: StreamConfig::for_ft8(),
            target_sample_rate: 12000,
            high_quality_conversion: false,
            max_buffer_latency_ms: 10.0,
            enable_statistics: true,
            stats_update_interval: Duration::from_secs(1),
        }
    }
}

impl AudioProcessorConfig {
    /// Create configuration optimized for FT8
    pub fn for_ft8() -> Self {
        Self {
            stream_config: StreamConfig::for_ft8(),
            target_sample_rate: 12000,
            high_quality_conversion: false,
            max_buffer_latency_ms: 5.0,
            enable_statistics: true,
            stats_update_interval: Duration::from_secs(1),
        }
    }

    /// Create configuration for high-quality processing
    pub fn for_high_quality() -> Self {
        Self {
            stream_config: StreamConfig::for_high_quality(),
            target_sample_rate: 12000,
            high_quality_conversion: true,
            max_buffer_latency_ms: 20.0,
            enable_statistics: true,
            stats_update_interval: Duration::from_secs(1),
        }
    }
}

/// Audio processing statistics
#[derive(Debug, Clone)]
pub struct AudioProcessingStats {
    /// Stream statistics
    pub stream_stats: StreamStatistics,
    /// Sample rate conversion statistics
    pub conversion_stats: ConversionStats,
    /// Overall processing health
    pub is_healthy: bool,
    /// Last update timestamp
    pub last_update: Instant,
    /// Total runtime
    pub uptime: Duration,
}

/// Sample rate conversion statistics
#[derive(Debug, Clone)]
pub struct ConversionStats {
    /// Whether conversion is active
    pub is_active: bool,
    /// Source sample rate
    pub source_rate: u32,
    /// Target sample rate
    pub target_rate: u32,
    /// Conversion ratio
    pub conversion_ratio: f64,
    /// Total samples converted
    pub samples_converted: u64,
    /// Conversion processing time (microseconds)
    pub conversion_time_us: u64,
}

impl Default for ConversionStats {
    fn default() -> Self {
        Self {
            is_active: false,
            source_rate: 0,
            target_rate: 0,
            conversion_ratio: 1.0,
            samples_converted: 0,
            conversion_time_us: 0,
        }
    }
}

/// Main audio processor for FT8 signal processing
pub struct AudioProcessor {
    config: AudioProcessorConfig,
    stream_manager: AudioStreamManager,
    resampler: Option<Box<dyn SampleRateConverter + Send>>,
    device_manager: AudioDeviceManager,
    comm: Arc<AudioComm>,
    statistics: Arc<RwLock<AudioProcessingStats>>,
    start_time: Option<Instant>,
}

impl AudioProcessor {
    /// Create a new audio processor
    pub async fn new(config: AudioProcessorConfig) -> AudioResult<Self> {
        let device_manager = AudioDeviceManager::new()?;
        let stream_manager = AudioStreamManager::new(config.stream_config.clone())?;
        let comm = stream_manager.get_comm();

        // Create sample rate converter if needed
        let resampler = if ResamplerFactory::needs_conversion(
            config.stream_config.sample_rate,
            config.target_sample_rate,
        ) {
            Some(ResamplerFactory::create_resampler(
                config.stream_config.sample_rate,
                config.target_sample_rate,
                config.stream_config.input_channels,
                config.high_quality_conversion,
            )?)
        } else {
            None
        };

        let statistics = Arc::new(RwLock::new(AudioProcessingStats {
            stream_stats: StreamStatistics {
                is_running: false,
                config: config.stream_config.clone(),
                theoretical_latency_ms: 0.0,
                audio_samples_buffered: 0,
                buffer_usage_percent: 0.0,
                samples_dropped: 0,
                samples_processed: 0,
                drop_rate_percent: 0.0,
                has_buffer_overruns: false,
            },
            conversion_stats: ConversionStats::default(),
            is_healthy: false,
            last_update: Instant::now(),
            uptime: Duration::ZERO,
        }));

        Ok(Self {
            config,
            stream_manager,
            resampler,
            device_manager,
            comm,
            statistics,
            start_time: None,
        })
    }

    /// Start audio processing
    pub async fn start(&mut self) -> AudioResult<()> {
        if self.is_running().await {
            return Err(AudioError::stream("Audio processor is already running"));
        }

        info!("Starting audio processor");

        // Start audio streams
        self.stream_manager.start()?;
        self.start_time = Some(Instant::now());

        // Start statistics update task if enabled
        if self.config.enable_statistics {
            self.start_statistics_task().await;
        }

        info!("Audio processor started successfully");
        Ok(())
    }

    /// Stop audio processing
    pub async fn stop(&mut self) -> AudioResult<()> {
        if !self.is_running().await {
            return Ok(());
        }

        info!("Stopping audio processor");

        // Stop audio streams
        self.stream_manager.stop()?;
        self.start_time = None;

        // Reset resampler state
        if let Some(ref mut resampler) = self.resampler {
            resampler.reset();
        }

        info!("Audio processor stopped");
        Ok(())
    }

    /// Check if the processor is running
    pub async fn is_running(&self) -> bool {
        self.stream_manager.is_running()
    }

    /// Get processed audio samples
    ///
    /// Returns samples converted to the target sample rate and ready for DSP processing
    pub async fn get_processed_samples(&mut self) -> AudioResult<Vec<AudioSample>> {
        let mut processed_samples = Vec::new();

        // Get raw samples from stream
        while let Some(raw_sample) = self.comm.pop_audio_sample() {
            // Apply sample rate conversion if needed
            let converted_sample = if let Some(ref mut resampler) = self.resampler {
                let start_time = Instant::now();
                let converted_data = resampler.process(&raw_sample.data)?;
                let conversion_time = start_time.elapsed();

                // Update conversion statistics
                self.update_conversion_stats(
                    raw_sample.data.len() as u64,
                    conversion_time.as_micros() as u64,
                )
                .await;

                AudioSample {
                    data: converted_data,
                    timestamp: raw_sample.timestamp,
                    sample_rate: self.config.target_sample_rate,
                    channels: raw_sample.channels,
                }
            } else {
                raw_sample
            };

            // Check buffer latency
            let sample_latency_ms = converted_sample.timestamp.elapsed().as_millis() as f64;
            if sample_latency_ms > self.config.max_buffer_latency_ms {
                warn!(
                    "High buffer latency: {:.2}ms > {:.2}ms",
                    sample_latency_ms, self.config.max_buffer_latency_ms
                );
            }

            processed_samples.push(converted_sample);
        }

        Ok(processed_samples)
    }

    /// Get current processing statistics
    pub async fn get_statistics(&self) -> AudioProcessingStats {
        self.statistics.read().await.clone()
    }

    /// Get available audio devices
    pub fn list_devices(&self) -> Vec<crate::device::AudioDeviceInfo> {
        self.device_manager
            .list_device_info()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get FT8-compatible devices
    pub fn list_ft8_devices(&self) -> Vec<crate::device::AudioDeviceInfo> {
        self.device_manager
            .find_ft8_compatible_devices()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Update configuration (requires restart if running)
    pub async fn update_config(&mut self, config: AudioProcessorConfig) -> AudioResult<()> {
        if self.is_running().await {
            return Err(AudioError::configuration(
                "Cannot update configuration while processor is running",
            ));
        }

        self.config = config;

        // Update stream configuration
        self.stream_manager
            .set_config(self.config.stream_config.clone())?;

        // Update resampler if needed
        self.resampler = if ResamplerFactory::needs_conversion(
            self.config.stream_config.sample_rate,
            self.config.target_sample_rate,
        ) {
            let resampler = ResamplerFactory::create_resampler(
                self.config.stream_config.sample_rate,
                self.config.target_sample_rate,
                self.config.stream_config.input_channels,
                self.config.high_quality_conversion,
            )?;
            Some(resampler)
        } else {
            None
        };

        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> &AudioProcessorConfig {
        &self.config
    }

    /// Start the statistics update task
    async fn start_statistics_task(&self) {
        let statistics = self.statistics.clone();
        let _stream_manager_stats = self.stream_manager.get_statistics();
        let interval_duration = self.config.stats_update_interval;

        tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval_duration);

            loop {
                interval_timer.tick().await;

                // This is a simplified version - in a real implementation,
                // you'd get updated stats from the stream manager
                let mut stats = statistics.write().await;
                stats.last_update = Instant::now();
                // Update other statistics here
            }
        });
    }

    /// Update sample rate conversion statistics
    async fn update_conversion_stats(&self, samples_processed: u64, processing_time_us: u64) {
        let mut stats = self.statistics.write().await;

        if let Some(ref resampler) = self.resampler {
            stats.conversion_stats.is_active = true;
            stats.conversion_stats.samples_converted += samples_processed;
            stats.conversion_stats.conversion_time_us += processing_time_us;

            if !resampler.is_passthrough() {
                stats.conversion_stats.source_rate = self.config.stream_config.sample_rate;
                stats.conversion_stats.target_rate = self.config.target_sample_rate;
                stats.conversion_stats.conversion_ratio = self.config.target_sample_rate as f64
                    / self.config.stream_config.sample_rate as f64;
            }
        }
    }
}

impl Drop for AudioProcessor {
    fn drop(&mut self) {
        // Best effort to stop streams in drop
        // Note: This is simplified to avoid async in Drop
        let _ = self.stream_manager.stop();
    }
}

/// Audio processor builder for easier configuration
pub struct AudioProcessorBuilder {
    config: AudioProcessorConfig,
}

impl AudioProcessorBuilder {
    /// Create a new builder with default configuration
    pub fn new() -> Self {
        Self {
            config: AudioProcessorConfig::default(),
        }
    }

    /// Set the sample rate
    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.config.stream_config.sample_rate = rate;
        self
    }

    /// Set the target processing sample rate
    pub fn target_sample_rate(mut self, rate: u32) -> Self {
        self.config.target_sample_rate = rate;
        self
    }

    /// Set the number of input channels
    pub fn input_channels(mut self, channels: u16) -> Self {
        self.config.stream_config.input_channels = channels;
        self
    }

    /// Set the buffer size
    pub fn buffer_size(mut self, size: u32) -> Self {
        self.config.stream_config.buffer_size = size;
        self
    }

    /// Enable high-quality sample rate conversion
    pub fn high_quality_conversion(mut self, enable: bool) -> Self {
        self.config.high_quality_conversion = enable;
        self
    }

    /// Enable audio monitoring
    pub fn enable_monitoring(mut self, enable: bool) -> Self {
        self.config.stream_config.enable_monitoring = enable;
        self
    }

    /// Set input device by name
    pub fn input_device(mut self, name: String) -> Self {
        self.config.stream_config.input_device_name = Some(name);
        self
    }

    /// Set output device by name
    pub fn output_device(mut self, name: String) -> Self {
        self.config.stream_config.output_device_name = Some(name);
        self
    }

    /// Build the audio processor
    pub async fn build(self) -> AudioResult<AudioProcessor> {
        AudioProcessor::new(self.config).await
    }
}

impl Default for AudioProcessorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_audio_processor_creation() {
        let config = AudioProcessorConfig::for_ft8();
        let processor = AudioProcessor::new(config).await;

        // Should succeed even without audio devices in test environment
        if let Ok(processor) = processor {
            assert!(!processor.is_running().await);
        }
    }

    #[tokio::test]
    async fn test_audio_processor_builder() {
        let builder = AudioProcessorBuilder::new()
            .sample_rate(48000)
            .target_sample_rate(12000)
            .input_channels(1)
            .buffer_size(128)
            .high_quality_conversion(true);

        let processor = builder.build().await;

        if let Ok(processor) = processor {
            let config = processor.get_config();
            assert_eq!(config.stream_config.sample_rate, 48000);
            assert_eq!(config.target_sample_rate, 12000);
            assert_eq!(config.stream_config.input_channels, 1);
            assert_eq!(config.stream_config.buffer_size, 128);
            assert!(config.high_quality_conversion);
        }
    }

    #[test]
    fn test_config_defaults() {
        let config = AudioProcessorConfig::default();
        assert_eq!(config.target_sample_rate, 12000);
        assert!(!config.high_quality_conversion);
        assert!(config.enable_statistics);
    }

    #[test]
    fn test_ft8_config() {
        let config = AudioProcessorConfig::for_ft8();
        assert_eq!(config.target_sample_rate, 12000);
        assert_eq!(config.stream_config.sample_rate, 48000);
        assert!(config.max_buffer_latency_ms < 10.0);
    }

    #[test]
    fn test_high_quality_config() {
        let config = AudioProcessorConfig::for_high_quality();
        assert!(config.high_quality_conversion);
        assert_eq!(config.stream_config.sample_rate, 48000);
        assert!(config.max_buffer_latency_ms >= 10.0);
    }

    #[tokio::test]
    async fn test_statistics_structure() {
        let stats = AudioProcessingStats {
            stream_stats: StreamStatistics {
                is_running: true,
                config: StreamConfig::for_ft8(),
                theoretical_latency_ms: 5.3,
                audio_samples_buffered: 10,
                buffer_usage_percent: 50.0,
                samples_dropped: 0,
                samples_processed: 1000,
                drop_rate_percent: 0.0,
                has_buffer_overruns: false,
            },
            conversion_stats: ConversionStats {
                is_active: true,
                source_rate: 48000,
                target_rate: 12000,
                conversion_ratio: 0.25,
                samples_converted: 5000,
                conversion_time_us: 100,
            },
            is_healthy: true,
            last_update: Instant::now(),
            uptime: Duration::from_secs(30),
        };

        assert!(stats.is_healthy);
        assert!(stats.conversion_stats.is_active);
        assert_eq!(stats.conversion_stats.conversion_ratio, 0.25);
    }
}
