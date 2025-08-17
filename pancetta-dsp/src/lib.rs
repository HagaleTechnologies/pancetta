//! # Pancetta DSP - High-Performance Digital Signal Processing for Amateur Radio
//!
//! This crate provides a comprehensive signal processing pipeline optimized for amateur radio
//! applications, particularly FT8 digital mode processing. It connects real-time audio streaming
//! with FT8 decoders through a sophisticated multi-stage processing chain.
//!
//! ## Features
//!
//! - **High-Quality Resampling**: SINC-based resampling with configurable quality parameters
//! - **Automatic Gain Control**: Sophisticated AGC with hang time, compression, and noise gating
//! - **Advanced Filtering**: Cascaded biquad IIR filters with multiple design types
//! - **Noise Reduction**: Spectral subtraction and adaptive filtering
//! - **Ring Buffer Management**: Lock-free audio buffering for real-time streaming
//! - **FT8 Optimization**: Pre-configured processing chains for FT8 digital mode
//! - **Pipeline Architecture**: Modular, async-friendly processing stages
//! - **Performance Monitoring**: Detailed statistics and performance metrics
//!
//! ## Architecture
//!
//! The DSP pipeline consists of several configurable stages:
//!
//! 1. **Audio Ring Buffer** - Continuous audio streaming with overflow/underflow handling
//! 2. **Resampling** - High-quality sample rate conversion (48kHz → 12kHz for FT8)
//! 3. **Bandpass Filtering** - Remove out-of-band noise and interference
//! 4. **Noise Reduction** - Adaptive spectral subtraction
//! 5. **AGC** - Automatic gain control with compression and hang time
//! 6. **Window Extraction** - Extract 12.64-second windows for FT8 processing
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use pancetta_dsp::{DspPipeline, PipelineBuilder};
//! use tokio;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create FT8-optimized pipeline
//!     let (mut pipeline, input_tx, output_rx) = PipelineBuilder::new()
//!         .input_sample_rate(48000.0)
//!         .output_sample_rate(12000.0)
//!         .enable_agc(true)
//!         .enable_noise_reduction(true)
//!         .enable_bandpass(true)
//!         .build()?;
//!
//!     // Start pipeline processing
//!     tokio::spawn(async move {
//!         if let Err(e) = pipeline.start().await {
//!             eprintln!("Pipeline error: {}", e);
//!         }
//!     });
//!
//!     // Send audio samples
//!     let audio_samples = vec![0.1; 1024];
//!     input_tx.send(audio_samples)?;
//!
//!     // Receive processed windows
//!     if let Ok(ft8_window) = output_rx.recv() {
//!         println!("Received FT8 window with {} samples", ft8_window.len());
//!         // Send to FT8 decoder...
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## FT8 Integration
//!
//! This crate is designed to integrate seamlessly with the `pancetta-ft8` decoder:
//!
//! ```rust,no_run
//! use pancetta_dsp::PipelineBuilder;
//! // use pancetta_ft8::Ft8Decoder;
//!
//! async fn ft8_processing_example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create DSP pipeline
//!     let (mut pipeline, input_tx, output_rx) = PipelineBuilder::new()
//!         .input_sample_rate(48000.0)
//!         .output_sample_rate(12000.0)
//!         .build()?;
//!
//!     // Create FT8 decoder
//!     // let mut decoder = Ft8Decoder::new(12000.0)?;
//!
//!     // Start pipeline
//!     tokio::spawn(async move {
//!         pipeline.start().await.unwrap();
//!     });
//!
//!     // Process FT8 windows
//!     while let Ok(window) = output_rx.recv() {
//!         // decoder.decode_window(&window)?;
//!         println!("Processing FT8 window...");
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Performance Considerations
//!
//! - The pipeline is designed for real-time processing with minimal latency
//! - SIMD optimizations are available when the `simd` feature is enabled
//! - Ring buffers use lock-free algorithms for high-performance audio streaming
//! - Configurable block sizes allow balancing latency vs. CPU efficiency
//! - Processing statistics help monitor performance and tune parameters
//!
//! ## Configuration
//!
//! All components are highly configurable for different amateur radio applications:
//!
//! - **FT8**: 12kHz sample rate, 200-4000Hz bandpass, medium AGC
//! - **PSK31**: 8kHz sample rate, narrow bandpass, fast AGC
//! - **SSB**: 24kHz sample rate, wide bandwidth, slow AGC
//! - **CW**: High sample rate, narrow filters, very fast AGC

pub mod agc;
pub mod buffer;
pub mod filter;
pub mod pipeline;
pub mod resampler;

// Re-export main types for convenience
pub use agc::{AgcConfig, AgcMode, AutomaticGainControl};
pub use buffer::{AudioRingBuffer, WindowExtractor};
pub use filter::{FilterConfig, FilterType, IirFilter, NoiseReductionFilter};
pub use pipeline::{
    AudioFrame, DspPipeline, PipelineBuilder, PipelineConfig, PipelineStage, PipelineStats,
};
pub use resampler::AudioResampler;

// Re-export error types
pub use agc::AgcError;
pub use buffer::BufferError;
pub use filter::FilterError;
pub use pipeline::PipelineError;
pub use resampler::ResamplerError;

/// Result type for the entire crate
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// High-level API for common use cases
pub mod prelude {
    //! Convenient re-exports for common use cases
    
    pub use crate::{
        AgcConfig, AgcMode, AudioFrame, AudioResampler, AudioRingBuffer, AutomaticGainControl,
        DspPipeline, PipelineBuilder, PipelineConfig,
    };
    
    pub use crossbeam_channel::{Receiver, Sender};
    pub use tokio;
}

/// Factory functions for creating common configurations
pub mod factory {
    //! Factory functions for creating pre-configured components
    
    use crate::*;

    /// Create an FT8-optimized DSP pipeline
    /// 
    /// This creates a complete pipeline optimized for FT8 digital mode processing:
    /// - 48kHz input → 12kHz output resampling
    /// - 200-4000Hz bandpass filter
    /// - AGC optimized for digital modes
    /// - Noise reduction enabled
    /// - 12.64-second window extraction
    pub fn create_ft8_pipeline() -> pipeline::Result<(
        DspPipeline,
        crossbeam_channel::Sender<Vec<f32>>,
        crossbeam_channel::Receiver<Vec<f32>>,
    )> {
        PipelineBuilder::new()
            .input_sample_rate(48000.0)
            .output_sample_rate(12000.0)
            .enable_agc(true)
            .enable_noise_reduction(true)
            .enable_bandpass(true)
            .max_latency(0.5)
            .block_size(1024)
            .build()
    }

    /// Create a general amateur radio DSP pipeline
    /// 
    /// This creates a pipeline suitable for various amateur radio modes:
    /// - 48kHz input → 24kHz output resampling
    /// - Wide bandwidth (no bandpass filtering)
    /// - Medium AGC settings
    /// - Noise reduction enabled
    pub fn create_amateur_radio_pipeline() -> pipeline::Result<(
        DspPipeline,
        crossbeam_channel::Sender<Vec<f32>>,
        crossbeam_channel::Receiver<Vec<f32>>,
    )> {
        let config = PipelineConfig::new_amateur_radio();
        DspPipeline::new(config)
    }

    /// Create a high-quality audio resampler
    /// 
    /// Uses optimal settings for amateur radio applications with
    /// excellent anti-aliasing and minimal artifacts.
    pub fn create_ft8_resampler() -> resampler::Result<AudioResampler> {
        AudioResampler::new_ft8_optimized()
    }

    /// Create an FT8-optimized AGC
    /// 
    /// Configured for digital mode processing with appropriate
    /// attack/decay times and compression settings.
    pub fn create_ft8_agc(sample_rate: f32) -> agc::Result<AutomaticGainControl> {
        AutomaticGainControl::new_ft8_optimized(sample_rate)
    }

    /// Create an FT8 bandpass filter
    /// 
    /// Filters to 200-4000Hz range typical for FT8 operations.
    pub fn create_ft8_filter(sample_rate: f32) -> filter::Result<IirFilter> {
        let config = FilterConfig::new_ft8_bandpass(sample_rate);
        IirFilter::new(config)
    }

    /// Create an audio ring buffer for real-time streaming
    /// 
    /// Optimized for low-latency audio processing with overflow handling.
    pub fn create_audio_buffer(sample_rate: f32, max_latency: f32) -> buffer::Result<AudioRingBuffer> {
        AudioRingBuffer::new(sample_rate, max_latency)
    }

    /// Create an FT8 window extractor
    /// 
    /// Extracts 12.64-second windows with 50% overlap for FT8 processing.
    pub fn create_ft8_window_extractor(sample_rate: f32) -> WindowExtractor {
        WindowExtractor::new_ft8(sample_rate)
    }
}

/// Utility functions for signal processing
pub mod utils {
    //! Utility functions for common signal processing tasks
    
    /// Convert dB to linear scale
    pub fn db_to_linear(db: f32) -> f32 {
        10.0_f32.powf(db / 20.0)
    }

    /// Convert linear scale to dB
    pub fn linear_to_db(linear: f32) -> f32 {
        20.0 * linear.log10()
    }

    /// Calculate RMS value of a signal
    pub fn calculate_rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        
        let sum_squares: f32 = samples.iter().map(|&x| x * x).sum();
        (sum_squares / samples.len() as f32).sqrt()
    }

    /// Calculate peak value of a signal
    pub fn calculate_peak(samples: &[f32]) -> f32 {
        samples.iter().map(|&x| x.abs()).fold(0.0, f32::max)
    }

    /// Calculate signal-to-noise ratio in dB
    pub fn calculate_snr_db(signal_rms: f32, noise_rms: f32) -> f32 {
        if noise_rms > 0.0 {
            linear_to_db(signal_rms / noise_rms)
        } else {
            f32::INFINITY
        }
    }

    /// Apply Hann window to signal
    pub fn apply_hann_window(samples: &mut [f32]) {
        let len = samples.len();
        for (i, sample) in samples.iter_mut().enumerate() {
            let window_value = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (len - 1) as f32).cos());
            *sample *= window_value;
        }
    }

    /// Check if resampling is needed between two sample rates
    pub fn is_resampling_needed(input_rate: f32, output_rate: f32) -> bool {
        (input_rate - output_rate).abs() > f32::EPSILON
    }

    /// Calculate optimal block size for given sample rate and target latency
    pub fn calculate_optimal_block_size(sample_rate: f32, target_latency_ms: f32) -> usize {
        let samples = (sample_rate * target_latency_ms / 1000.0) as usize;
        // Round to nearest power of 2 for optimal FFT performance
        samples.next_power_of_two()
    }

    /// Validate sample rate for amateur radio applications
    pub fn validate_sample_rate(sample_rate: f32) -> bool {
        // Common amateur radio sample rates
        matches!(sample_rate as u32, 8000 | 12000 | 16000 | 24000 | 44100 | 48000 | 96000 | 192000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn test_factory_functions() {
        let pipeline_result = factory::create_ft8_pipeline();
        assert!(pipeline_result.is_ok());

        let resampler_result = factory::create_ft8_resampler();
        assert!(resampler_result.is_ok());

        let buffer_result = factory::create_audio_buffer(48000.0, 0.5);
        assert!(buffer_result.is_ok());
    }

    #[test]
    fn test_utils() {
        assert_eq!(utils::db_to_linear(0.0), 1.0);
        assert_eq!(utils::linear_to_db(1.0), 0.0);
        
        let samples = vec![0.1, 0.2, 0.3, 0.4, 0.5];
        let rms = utils::calculate_rms(&samples);
        assert!(rms > 0.0);
        
        let peak = utils::calculate_peak(&samples);
        assert_eq!(peak, 0.5);
        
        assert!(utils::validate_sample_rate(48000.0));
        assert!(!utils::validate_sample_rate(47999.0));
    }

    #[tokio::test]
    async fn test_pipeline_integration() {
        let (pipeline, input_tx, _output_rx) = factory::create_ft8_pipeline().unwrap();
        
        // Test that we can create the pipeline and channels
        assert!(!pipeline.is_running());
        
        // Test basic buffer operations
        let buffer = factory::create_audio_buffer(48000.0, 0.1).unwrap();
        let test_samples = vec![0.1; 100];
        let write_result = buffer.write(&test_samples);
        assert!(write_result.is_ok());
        
        // Test that we can send data through channel
        let test_samples = vec![0.1; 1024];
        let send_result = input_tx.try_send(test_samples);
        assert!(send_result.is_ok());
    }
}