use crate::{
    agc::{AgcConfig, AutomaticGainControl},
    buffer::{AudioRingBuffer, WindowExtractor},
    filter::{FilterConfig, IirFilter, NoiseReductionFilter},
    resampler::AudioResampler,
};
use async_trait::async_trait;
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::sync::Arc;
use thiserror::Error;
use tokio::time::{sleep, Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("Pipeline initialization failed: {message}")]
    InitializationFailed { message: String },
    #[error("Processing failed: {message}")]
    ProcessingFailed { message: String },
    #[error("Buffer error: {source}")]
    BufferError {
        #[from]
        source: crate::buffer::BufferError,
    },
    #[error("Resampler error: {source}")]
    ResamplerError {
        #[from]
        source: crate::resampler::ResamplerError,
    },
    #[error("AGC error: {source}")]
    AgcError {
        #[from]
        source: crate::agc::AgcError,
    },
    #[error("Filter error: {source}")]
    FilterError {
        #[from]
        source: crate::filter::FilterError,
    },
    #[error("Channel error: {message}")]
    ChannelError { message: String },
}

pub type Result<T> = std::result::Result<T, PipelineError>;

/// Configuration for the DSP pipeline
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Input sample rate (from audio system)
    pub input_sample_rate: f32,
    /// Output sample rate (for FT8 decoder)
    pub output_sample_rate: f32,
    /// Maximum processing latency in seconds
    pub max_latency: f32,
    /// Enable AGC
    pub enable_agc: bool,
    /// AGC configuration
    pub agc_config: AgcConfig,
    /// Enable noise reduction
    pub enable_noise_reduction: bool,
    /// Enable bandpass filtering
    pub enable_bandpass: bool,
    /// Bandpass filter configuration
    pub bandpass_config: FilterConfig,
    /// Processing block size
    pub block_size: usize,
    /// Number of processing threads
    pub num_threads: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self::new_ft8_optimized()
    }
}

impl PipelineConfig {
    /// Create configuration optimized for FT8 processing
    pub fn new_ft8_optimized() -> Self {
        Self {
            input_sample_rate: 48000.0,
            output_sample_rate: 12000.0,
            max_latency: 0.5, // 500ms max latency
            enable_agc: true,
            agc_config: AgcConfig::new_ft8_optimized(),
            enable_noise_reduction: true,
            enable_bandpass: true,
            bandpass_config: FilterConfig::new_ft8_bandpass(12000.0),
            block_size: 1024,
            num_threads: 2,
        }
    }

    /// Create configuration for general amateur radio use
    pub fn new_amateur_radio() -> Self {
        Self {
            input_sample_rate: 48000.0,
            output_sample_rate: 24000.0,
            max_latency: 0.2, // 200ms max latency
            enable_agc: true,
            agc_config: AgcConfig::new_for_mode(crate::agc::AgcMode::Medium),
            enable_noise_reduction: true,
            enable_bandpass: false, // Wide bandwidth for general use
            bandpass_config: FilterConfig::new_ft8_bandpass(24000.0),
            block_size: 512,
            num_threads: 1,
        }
    }

    /// Validate pipeline configuration
    pub fn validate(&self) -> Result<()> {
        if self.input_sample_rate <= 0.0 || self.output_sample_rate <= 0.0 {
            return Err(PipelineError::InitializationFailed {
                message: "Invalid sample rates".to_string(),
            });
        }

        if self.max_latency <= 0.0 {
            return Err(PipelineError::InitializationFailed {
                message: "Invalid latency setting".to_string(),
            });
        }

        if self.block_size == 0 || self.num_threads == 0 {
            return Err(PipelineError::InitializationFailed {
                message: "Invalid processing parameters".to_string(),
            });
        }

        self.agc_config
            .validate()
            .map_err(|e| PipelineError::InitializationFailed {
                message: format!("AGC config validation failed: {}", e),
            })?;

        self.bandpass_config
            .validate()
            .map_err(|e| PipelineError::InitializationFailed {
                message: format!("Filter config validation failed: {}", e),
            })?;

        Ok(())
    }
}

/// Audio frame for pipeline processing
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Audio samples
    pub samples: Vec<f32>,
    /// Sample rate
    pub sample_rate: f32,
    /// Timestamp
    pub timestamp: Instant,
    /// Frame number for tracking
    pub frame_number: u64,
}

impl AudioFrame {
    pub fn new(samples: Vec<f32>, sample_rate: f32, frame_number: u64) -> Self {
        Self {
            samples,
            sample_rate,
            timestamp: Instant::now(),
            frame_number,
        }
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f32(self.samples.len() as f32 / self.sample_rate)
    }
}

/// Trait for pipeline stages
#[async_trait]
pub trait PipelineStage: Send {
    /// Process an audio frame
    async fn process(&mut self, input: AudioFrame) -> Result<AudioFrame>;

    /// Get stage name for debugging
    fn name(&self) -> &str;

    /// Reset stage state
    fn reset(&mut self);
}

/// Resampling stage
pub struct ResamplingStage {
    resampler: AudioResampler,
    output_sample_rate: f32,
}

impl ResamplingStage {
    pub fn new(input_rate: f32, output_rate: f32) -> Result<Self> {
        let resampler = AudioResampler::new(input_rate, output_rate, 1024)?;
        Ok(Self {
            resampler,
            output_sample_rate: output_rate,
        })
    }
}

#[async_trait]
impl PipelineStage for ResamplingStage {
    async fn process(&mut self, input: AudioFrame) -> Result<AudioFrame> {
        let mut output_samples = Vec::new();
        self.resampler
            .process(&input.samples, &mut output_samples)?;

        Ok(AudioFrame {
            samples: output_samples,
            sample_rate: self.output_sample_rate,
            timestamp: input.timestamp,
            frame_number: input.frame_number,
        })
    }

    fn name(&self) -> &str {
        "Resampler"
    }

    fn reset(&mut self) {
        self.resampler.reset();
    }
}

/// AGC stage
pub struct AgcStage {
    agc: AutomaticGainControl,
}

impl AgcStage {
    pub fn new(config: AgcConfig, sample_rate: f32) -> Result<Self> {
        let agc = AutomaticGainControl::new(config, sample_rate)?;
        Ok(Self { agc })
    }
}

#[async_trait]
impl PipelineStage for AgcStage {
    async fn process(&mut self, input: AudioFrame) -> Result<AudioFrame> {
        let mut output_samples = vec![0.0; input.samples.len()];
        self.agc.process(&input.samples, &mut output_samples)?;

        Ok(AudioFrame {
            samples: output_samples,
            sample_rate: input.sample_rate,
            timestamp: input.timestamp,
            frame_number: input.frame_number,
        })
    }

    fn name(&self) -> &str {
        "AGC"
    }

    fn reset(&mut self) {
        self.agc.reset();
    }
}

/// Filtering stage
pub struct FilterStage {
    filter: IirFilter,
}

impl FilterStage {
    pub fn new(config: FilterConfig) -> Result<Self> {
        let filter = IirFilter::new(config)?;
        Ok(Self { filter })
    }
}

#[async_trait]
impl PipelineStage for FilterStage {
    async fn process(&mut self, input: AudioFrame) -> Result<AudioFrame> {
        let mut output_samples = vec![0.0; input.samples.len()];
        self.filter.process(&input.samples, &mut output_samples)?;

        Ok(AudioFrame {
            samples: output_samples,
            sample_rate: input.sample_rate,
            timestamp: input.timestamp,
            frame_number: input.frame_number,
        })
    }

    fn name(&self) -> &str {
        "Filter"
    }

    fn reset(&mut self) {
        self.filter.reset();
    }
}

/// Noise reduction stage
pub struct NoiseReductionStage {
    nr_filter: NoiseReductionFilter,
}

impl NoiseReductionStage {
    pub fn new(sample_rate: f32, frame_size: usize) -> Self {
        let nr_filter = NoiseReductionFilter::new(sample_rate, frame_size, 0.5);
        Self { nr_filter }
    }
}

#[async_trait]
impl PipelineStage for NoiseReductionStage {
    async fn process(&mut self, input: AudioFrame) -> Result<AudioFrame> {
        let mut output_samples = Vec::new();
        self.nr_filter
            .process(&input.samples, &mut output_samples)
            .map_err(|e| PipelineError::ProcessingFailed {
                message: format!("Noise reduction failed: {}", e),
            })?;

        // Pad output if needed
        if output_samples.len() < input.samples.len() {
            output_samples.resize(input.samples.len(), 0.0);
        }

        Ok(AudioFrame {
            samples: output_samples,
            sample_rate: input.sample_rate,
            timestamp: input.timestamp,
            frame_number: input.frame_number,
        })
    }

    fn name(&self) -> &str {
        "NoiseReduction"
    }

    fn reset(&mut self) {
        self.nr_filter.reset();
    }
}

/// Main DSP pipeline
/// Orchestrates all signal processing stages for real-time audio processing
pub struct DspPipeline {
    /// Pipeline configuration
    config: PipelineConfig,
    /// Processing stages
    stages: Vec<Box<dyn PipelineStage>>,
    /// Input ring buffer
    input_buffer: AudioRingBuffer,
    /// Window extractor for FT8
    window_extractor: WindowExtractor,
    /// Output channel for processed windows
    output_tx: Sender<Vec<f32>>,
    /// Input channel for raw audio
    input_rx: Receiver<Vec<f32>>,
    /// Processing statistics
    stats: Arc<Mutex<PipelineStats>>,
    /// Running flag
    is_running: Arc<Mutex<bool>>,
    /// Frame counter
    frame_counter: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    pub frames_processed: u64,
    pub samples_processed: u64,
    pub processing_time_ms: f64,
    pub average_latency_ms: f64,
    pub buffer_overruns: u64,
    pub buffer_underruns: u64,
    pub current_load: f32,
}

impl DspPipeline {
    /// Create a new DSP pipeline
    pub fn new(config: PipelineConfig) -> Result<(Self, Sender<Vec<f32>>, Receiver<Vec<f32>>)> {
        config.validate()?;

        // Create channels for audio data
        let (input_tx, input_rx) = bounded(config.num_threads * 4);
        let (output_tx, output_rx) = bounded(config.num_threads * 4);

        // Create input buffer
        let input_buffer = AudioRingBuffer::new(config.input_sample_rate, config.max_latency)?;

        // Create window extractor for FT8
        let window_extractor = WindowExtractor::new_ft8(config.output_sample_rate);

        // Build processing stages
        let mut stages: Vec<Box<dyn PipelineStage>> = Vec::new();

        // Resampling stage (if needed)
        if AudioResampler::is_resampling_needed(config.input_sample_rate, config.output_sample_rate)
        {
            let resampling_stage =
                ResamplingStage::new(config.input_sample_rate, config.output_sample_rate)?;
            stages.push(Box::new(resampling_stage));
            info!(
                "Added resampling stage: {}Hz -> {}Hz",
                config.input_sample_rate, config.output_sample_rate
            );
        }

        // Bandpass filter stage
        if config.enable_bandpass {
            let mut filter_config = config.bandpass_config.clone();
            filter_config.sample_rate = config.output_sample_rate;
            let filter_stage = FilterStage::new(filter_config)?;
            stages.push(Box::new(filter_stage));
            info!("Added bandpass filter stage");
        }

        // Noise reduction stage
        if config.enable_noise_reduction {
            let nr_stage = NoiseReductionStage::new(config.output_sample_rate, 1024);
            stages.push(Box::new(nr_stage));
            info!("Added noise reduction stage");
        }

        // AGC stage
        if config.enable_agc {
            let agc_stage = AgcStage::new(config.agc_config.clone(), config.output_sample_rate)?;
            stages.push(Box::new(agc_stage));
            info!("Added AGC stage");
        }

        info!("Created DSP pipeline with {} stages", stages.len());

        let pipeline = Self {
            config,
            stages,
            input_buffer,
            window_extractor,
            output_tx,
            input_rx,
            stats: Arc::new(Mutex::new(PipelineStats::default())),
            is_running: Arc::new(Mutex::new(false)),
            frame_counter: 0,
        };

        Ok((pipeline, input_tx, output_rx))
    }

    /// Start the pipeline processing
    pub async fn start(&mut self) -> Result<()> {
        {
            let mut running = self.is_running.lock();
            if *running {
                return Ok(());
            }
            *running = true;
        }

        info!("Starting DSP pipeline");

        // Main processing loop
        while *self.is_running.lock() {
            let start_time = Instant::now();

            // Check for new input data
            if let Ok(input_samples) = self.input_rx.try_recv() {
                self.process_input_samples(input_samples).await?;
            }

            // Process buffered audio
            self.process_buffered_audio().await?;

            // Update processing statistics
            let processing_time = start_time.elapsed();
            self.update_stats(processing_time);

            // Small delay to prevent CPU spinning
            sleep(Duration::from_micros(100)).await;
        }

        info!("DSP pipeline stopped");
        Ok(())
    }

    /// Stop the pipeline
    pub fn stop(&self) {
        let mut running = self.is_running.lock();
        *running = false;
        info!("DSP pipeline stop requested");
    }

    /// Process new input samples
    async fn process_input_samples(&mut self, samples: Vec<f32>) -> Result<()> {
        // Write to ring buffer
        match self.input_buffer.write(&samples) {
            Ok(_) => {
                self.stats.lock().samples_processed += samples.len() as u64;
            }
            Err(crate::buffer::BufferError::Overflow { count }) => {
                self.stats.lock().buffer_overruns += 1;
                warn!("Input buffer overflow: dropped {} samples", count);
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    /// Process buffered audio data
    async fn process_buffered_audio(&mut self) -> Result<()> {
        let block_size = self.config.block_size;

        // Check if we have enough samples to process
        if self.input_buffer.len() < block_size {
            return Ok(());
        }

        // Read samples from buffer
        let mut samples = vec![0.0; block_size];
        match self.input_buffer.read(&mut samples) {
            Ok(_) => {}
            Err(crate::buffer::BufferError::Underflow) => {
                self.stats.lock().buffer_underruns += 1;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }

        // Create audio frame
        let mut frame = AudioFrame::new(samples, self.config.input_sample_rate, self.frame_counter);
        self.frame_counter += 1;

        // Process through all stages
        for stage in &mut self.stages {
            frame = stage.process(frame).await?;
        }

        // Extract windows for FT8
        self.window_extractor.process(&frame.samples, |window| {
            // Send window to FT8 decoder
            if let Err(e) = self.output_tx.try_send(window.to_vec()) {
                warn!("Failed to send window to decoder: {}", e);
            }
        });

        self.stats.lock().frames_processed += 1;
        Ok(())
    }

    /// Update processing statistics
    fn update_stats(&self, processing_time: Duration) {
        let mut stats = self.stats.lock();
        stats.processing_time_ms = processing_time.as_secs_f64() * 1000.0;

        // Calculate current CPU load
        let block_duration = self.config.block_size as f64 / self.config.input_sample_rate as f64;
        stats.current_load = (stats.processing_time_ms / 1000.0 / block_duration) as f32;

        // Update average latency
        let buffer_latency = self.input_buffer.latency() * 1000.0; // Convert to ms
        stats.average_latency_ms = buffer_latency as f64;
    }

    /// Get pipeline statistics
    pub fn stats(&self) -> PipelineStats {
        self.stats.lock().clone()
    }

    /// Reset pipeline statistics
    pub fn reset_stats(&self) {
        let mut stats = self.stats.lock();
        *stats = PipelineStats::default();
    }

    /// Reset all pipeline stages
    pub fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
        self.input_buffer.clear();
        self.frame_counter = 0;
        self.reset_stats();
        info!("Pipeline reset complete");
    }

    /// Get current buffer levels
    pub fn buffer_status(&self) -> (usize, usize, f32) {
        let len = self.input_buffer.len();
        let capacity = self.input_buffer.capacity();
        let latency = self.input_buffer.latency();
        (len, capacity, latency)
    }

    /// Check if pipeline is running
    pub fn is_running(&self) -> bool {
        *self.is_running.lock()
    }
}

/// Builder for creating DSP pipelines with custom configurations
pub struct PipelineBuilder {
    config: PipelineConfig,
}

impl PipelineBuilder {
    pub fn new() -> Self {
        Self {
            config: PipelineConfig::default(),
        }
    }

    pub fn input_sample_rate(mut self, rate: f32) -> Self {
        self.config.input_sample_rate = rate;
        self
    }

    pub fn output_sample_rate(mut self, rate: f32) -> Self {
        self.config.output_sample_rate = rate;
        self
    }

    pub fn max_latency(mut self, latency: f32) -> Self {
        self.config.max_latency = latency;
        self
    }

    pub fn enable_agc(mut self, enable: bool) -> Self {
        self.config.enable_agc = enable;
        self
    }

    pub fn enable_noise_reduction(mut self, enable: bool) -> Self {
        self.config.enable_noise_reduction = enable;
        self
    }

    pub fn enable_bandpass(mut self, enable: bool) -> Self {
        self.config.enable_bandpass = enable;
        self
    }

    pub fn block_size(mut self, size: usize) -> Self {
        self.config.block_size = size;
        self
    }

    pub fn build(self) -> Result<(DspPipeline, Sender<Vec<f32>>, Receiver<Vec<f32>>)> {
        DspPipeline::new(self.config)
    }
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_builder() {
        let result = PipelineBuilder::new()
            .input_sample_rate(48000.0)
            .output_sample_rate(12000.0)
            .enable_agc(true)
            .build();

        assert!(result.is_ok());
    }

    #[test]
    fn test_ft8_config() {
        let config = PipelineConfig::new_ft8_optimized();
        assert!(config.validate().is_ok());
        assert_eq!(config.input_sample_rate, 48000.0);
        assert_eq!(config.output_sample_rate, 12000.0);
    }

    #[tokio::test]
    async fn test_pipeline_stages() {
        let config = AgcConfig::new_ft8_optimized();
        let mut agc_stage = AgcStage::new(config, 12000.0).unwrap();

        let frame = AudioFrame::new(vec![0.1; 1000], 12000.0, 0);
        let result = agc_stage.process(frame).await;
        assert!(result.is_ok());
    }
}
