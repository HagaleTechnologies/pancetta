//! Sample rate conversion for audio processing
//!
//! Provides high-quality sample rate conversion to support different
//! audio hardware sample rates while maintaining FT8 compatibility.

use crate::error::{AudioError, AudioResult};
use std::collections::VecDeque;

/// Linear interpolation sample rate converter
/// 
/// Provides basic but efficient sample rate conversion suitable for
/// real-time audio processing with minimal CPU usage.
pub struct LinearResampler {
    /// Source sample rate
    source_rate: u32,
    /// Target sample rate
    target_rate: u32,
    /// Conversion ratio (target / source)
    ratio: f64,
    /// Number of channels
    channels: u16,
    /// Previous samples for interpolation (per channel)
    prev_samples: Vec<f32>,
    /// Fractional sample position
    position: f64,
    /// Input buffer for processing
    input_buffer: VecDeque<f32>,
}

impl LinearResampler {
    /// Create a new linear resampler
    pub fn new(source_rate: u32, target_rate: u32, channels: u16) -> AudioResult<Self> {
        if source_rate == 0 || target_rate == 0 {
            return Err(AudioError::sample_rate("Sample rates must be non-zero"));
        }
        
        if channels == 0 {
            return Err(AudioError::sample_rate("Channel count must be non-zero"));
        }

        let ratio = target_rate as f64 / source_rate as f64;
        
        Ok(Self {
            source_rate,
            target_rate,
            ratio,
            channels,
            prev_samples: vec![0.0; channels as usize],
            position: 0.0,
            input_buffer: VecDeque::new(),
        })
    }

    /// Process a batch of audio samples
    pub fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>> {
        if input.len() % self.channels as usize != 0 {
            return Err(AudioError::sample_rate(
                "Input length must be multiple of channel count"
            ));
        }

        // Add input to buffer
        for &sample in input {
            self.input_buffer.push_back(sample);
        }

        let mut output = Vec::new();
        let frame_size = self.channels as usize;

        // Process samples frame by frame
        while self.input_buffer.len() >= frame_size * 2 {
            // Generate output samples until we need more input
            while self.position < 1.0 && self.input_buffer.len() >= frame_size * 2 {
                // Interpolate each channel
                for ch in 0..self.channels as usize {
                    let prev_idx = ch;
                    let curr_idx = ch + frame_size;
                    
                    let prev_sample = if prev_idx < self.input_buffer.len() {
                        self.input_buffer[prev_idx]
                    } else {
                        self.prev_samples[ch]
                    };
                    
                    let curr_sample = if curr_idx < self.input_buffer.len() {
                        self.input_buffer[curr_idx]
                    } else {
                        0.0
                    };

                    // Linear interpolation
                    let interpolated = prev_sample + (curr_sample - prev_sample) * self.position as f32;
                    output.push(interpolated);
                }

                self.position += 1.0 / self.ratio;
            }

            // Move to next input frame
            if self.position >= 1.0 {
                // Save previous samples for next interpolation
                for ch in 0..self.channels as usize {
                    if ch < self.input_buffer.len() {
                        self.prev_samples[ch] = self.input_buffer[ch];
                    }
                }

                // Remove consumed frame
                for _ in 0..frame_size {
                    self.input_buffer.pop_front();
                }

                self.position -= 1.0;
            }
        }

        Ok(output)
    }

    /// Get the expected output length for a given input length
    pub fn output_length(&self, input_length: usize) -> usize {
        let input_frames = input_length / self.channels as usize;
        let output_frames = (input_frames as f64 * self.ratio).ceil() as usize;
        output_frames * self.channels as usize
    }

    /// Get the conversion ratio
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Reset the resampler state
    pub fn reset(&mut self) {
        self.prev_samples.fill(0.0);
        self.position = 0.0;
        self.input_buffer.clear();
    }

    /// Get source sample rate
    pub fn source_rate(&self) -> u32 {
        self.source_rate
    }

    /// Get target sample rate
    pub fn target_rate(&self) -> u32 {
        self.target_rate
    }

    /// Check if conversion is needed
    pub fn is_passthrough(&self) -> bool {
        self.source_rate == self.target_rate
    }
}

/// High-quality sample rate converter using sinc interpolation
/// 
/// Provides better quality conversion at the cost of higher CPU usage.
/// Suitable for offline processing or when quality is more important than latency.
pub struct SincResampler {
    /// Source sample rate
    source_rate: u32,
    /// Target sample rate
    target_rate: u32,
    /// Conversion ratio
    ratio: f64,
    /// Number of channels
    channels: u16,
    /// Sinc table for interpolation
    sinc_table: Vec<f32>,
    /// Table size
    table_size: usize,
    /// Input history buffer
    history: VecDeque<f32>,
    /// Position in input stream
    position: f64,
}

impl SincResampler {
    /// Create a new sinc resampler
    pub fn new(source_rate: u32, target_rate: u32, channels: u16) -> AudioResult<Self> {
        if source_rate == 0 || target_rate == 0 {
            return Err(AudioError::sample_rate("Sample rates must be non-zero"));
        }
        
        if channels == 0 {
            return Err(AudioError::sample_rate("Channel count must be non-zero"));
        }

        let ratio = target_rate as f64 / source_rate as f64;
        let table_size = 1024; // Size of sinc interpolation table
        
        // Generate sinc table
        let sinc_table = Self::generate_sinc_table(table_size);
        
        // History buffer size (enough for sinc kernel)
        let history_size = 32 * channels as usize;
        
        Ok(Self {
            source_rate,
            target_rate,
            ratio,
            channels,
            sinc_table,
            table_size,
            history: VecDeque::with_capacity(history_size),
            position: 0.0,
        })
    }

    /// Generate sinc interpolation table
    fn generate_sinc_table(size: usize) -> Vec<f32> {
        let mut table = Vec::with_capacity(size);
        let half_size = size / 2;
        
        for i in 0..size {
            let x = (i as f32 - half_size as f32) / half_size as f32 * 4.0;
            
            let sinc_val = if x.abs() < 1e-6 {
                1.0
            } else {
                let pi_x = std::f32::consts::PI * x;
                pi_x.sin() / pi_x
            };
            
            // Apply Hamming window
            let window = 0.54 + 0.46 * (std::f32::consts::PI * i as f32 / (size - 1) as f32).cos();
            
            table.push(sinc_val * window);
        }
        
        table
    }

    /// Process audio samples with sinc interpolation
    pub fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>> {
        if input.len() % self.channels as usize != 0 {
            return Err(AudioError::sample_rate(
                "Input length must be multiple of channel count"
            ));
        }

        // Add input to history
        for &sample in input {
            self.history.push_back(sample);
        }

        // Ensure we don't exceed history capacity
        let max_history = 32 * self.channels as usize;
        while self.history.len() > max_history {
            self.history.pop_front();
        }

        let mut output = Vec::new();
        let frame_size = self.channels as usize;
        let kernel_size = 16; // Half kernel size for sinc interpolation

        // Process while we have enough history
        while self.history.len() >= (kernel_size * 2 + 1) * frame_size {
            for ch in 0..self.channels as usize {
                let mut sum = 0.0;
                
                // Apply sinc kernel
                for k in 0..kernel_size * 2 + 1 {
                    let history_idx = (self.position as usize + k) * frame_size + ch;
                    
                    if history_idx < self.history.len() {
                        let sample = self.history[history_idx];
                        let table_idx = (k as f32 * self.table_size as f32 / (kernel_size * 2) as f32) as usize;
                        let table_idx = table_idx.min(self.table_size - 1);
                        
                        sum += sample * self.sinc_table[table_idx];
                    }
                }
                
                output.push(sum);
            }

            self.position += 1.0 / self.ratio;
            
            // Remove consumed samples
            while self.position >= 1.0 && self.history.len() > frame_size {
                for _ in 0..frame_size {
                    self.history.pop_front();
                }
                self.position -= 1.0;
            }
        }

        Ok(output)
    }

    /// Reset the resampler state
    pub fn reset(&mut self) {
        self.history.clear();
        self.position = 0.0;
    }
}

/// Sample rate converter factory
pub struct ResamplerFactory;

impl ResamplerFactory {
    /// Create the best resampler for the given rates and quality requirements
    pub fn create_resampler(
        source_rate: u32,
        target_rate: u32,
        channels: u16,
        high_quality: bool,
    ) -> AudioResult<Box<dyn SampleRateConverter + Send>> {
        if source_rate == target_rate {
            return Ok(Box::new(PassthroughResampler::new(channels)));
        }

        if high_quality {
            Ok(Box::new(SincResampler::new(source_rate, target_rate, channels)?))
        } else {
            Ok(Box::new(LinearResampler::new(source_rate, target_rate, channels)?))
        }
    }

    /// Check if conversion is needed
    pub fn needs_conversion(source_rate: u32, target_rate: u32) -> bool {
        source_rate != target_rate
    }

    /// Get recommended target rate for FT8
    pub fn recommended_ft8_rate() -> u32 {
        12000
    }

    /// Get common audio rates that work well for conversion
    pub fn common_rates() -> &'static [u32] {
        &[8000, 12000, 16000, 22050, 44100, 48000, 96000]
    }
}

/// Trait for sample rate converters
pub trait SampleRateConverter: Send + Sync {
    /// Process audio samples
    fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>>;
    
    /// Reset converter state
    fn reset(&mut self);
    
    /// Get expected output length
    fn output_length(&self, input_length: usize) -> usize;
    
    /// Check if this is a passthrough converter
    fn is_passthrough(&self) -> bool;
}

impl SampleRateConverter for LinearResampler {
    fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>> {
        LinearResampler::process(self, input)
    }
    
    fn reset(&mut self) {
        LinearResampler::reset(self)
    }
    
    fn output_length(&self, input_length: usize) -> usize {
        LinearResampler::output_length(self, input_length)
    }
    
    fn is_passthrough(&self) -> bool {
        LinearResampler::is_passthrough(self)
    }
}

impl SampleRateConverter for SincResampler {
    fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>> {
        SincResampler::process(self, input)
    }
    
    fn reset(&mut self) {
        SincResampler::reset(self)
    }
    
    fn output_length(&self, input_length: usize) -> usize {
        (input_length as f64 * self.ratio).ceil() as usize
    }
    
    fn is_passthrough(&self) -> bool {
        self.source_rate == self.target_rate
    }
}

/// Passthrough converter (no conversion needed)
pub struct PassthroughResampler {
    channels: u16,
}

impl PassthroughResampler {
    pub fn new(channels: u16) -> Self {
        Self { channels }
    }
}

impl SampleRateConverter for PassthroughResampler {
    fn process(&mut self, input: &[f32]) -> AudioResult<Vec<f32>> {
        Ok(input.to_vec())
    }
    
    fn reset(&mut self) {
        // Nothing to reset
    }
    
    fn output_length(&self, input_length: usize) -> usize {
        input_length
    }
    
    fn is_passthrough(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_resampler_creation() {
        let resampler = LinearResampler::new(48000, 12000, 1).unwrap();
        assert_eq!(resampler.source_rate(), 48000);
        assert_eq!(resampler.target_rate(), 12000);
        assert_eq!(resampler.ratio(), 0.25);
    }

    #[test]
    fn test_linear_resampler_passthrough() {
        let resampler = LinearResampler::new(48000, 48000, 1).unwrap();
        assert!(resampler.is_passthrough());
    }

    #[test]
    fn test_linear_resampler_downsampling() {
        let mut resampler = LinearResampler::new(48000, 12000, 1).unwrap();
        
        // Input: 48 samples at 48kHz should produce ~12 samples at 12kHz
        let input = vec![1.0; 48];
        let output = resampler.process(&input).unwrap();
        
        // Should be approximately 1/4 the length
        assert!(output.len() >= 8 && output.len() <= 16);
    }

    #[test]
    fn test_linear_resampler_upsampling() {
        let mut resampler = LinearResampler::new(12000, 48000, 1).unwrap();
        
        // Input: 12 samples at 12kHz should produce ~48 samples at 48kHz
        let input = vec![0.5; 12];
        let output = resampler.process(&input).unwrap();
        
        // Should be approximately 4x the length
        assert!(output.len() >= 32 && output.len() <= 64);
    }

    #[test]
    fn test_linear_resampler_stereo() {
        let mut resampler = LinearResampler::new(48000, 12000, 2).unwrap();
        
        // Input: 48 samples (24 frames) at 48kHz stereo
        let input = vec![0.5; 48];
        let output = resampler.process(&input).unwrap();
        
        // Output should be even length (stereo)
        assert_eq!(output.len() % 2, 0);
    }

    #[test]
    fn test_sinc_resampler_creation() {
        let resampler = SincResampler::new(48000, 12000, 1).unwrap();
        // Should create successfully
        assert!(!resampler.is_passthrough());
    }

    #[test]
    fn test_resampler_factory() {
        // Test passthrough
        let resampler = ResamplerFactory::create_resampler(48000, 48000, 1, false).unwrap();
        assert!(resampler.is_passthrough());
        
        // Test linear conversion
        let resampler = ResamplerFactory::create_resampler(48000, 12000, 1, false).unwrap();
        assert!(!resampler.is_passthrough());
        
        // Test sinc conversion
        let resampler = ResamplerFactory::create_resampler(48000, 12000, 1, true).unwrap();
        assert!(!resampler.is_passthrough());
    }

    #[test]
    fn test_passthrough_resampler() {
        let mut resampler = PassthroughResampler::new(2);
        let input = vec![0.1, 0.2, 0.3, 0.4];
        let output = resampler.process(&input).unwrap();
        
        assert_eq!(input, output);
        assert_eq!(resampler.output_length(100), 100);
        assert!(resampler.is_passthrough());
    }

    #[test]
    fn test_factory_utilities() {
        assert!(!ResamplerFactory::needs_conversion(48000, 48000));
        assert!(ResamplerFactory::needs_conversion(48000, 12000));
        assert_eq!(ResamplerFactory::recommended_ft8_rate(), 12000);
        
        let rates = ResamplerFactory::common_rates();
        assert!(rates.contains(&12000));
        assert!(rates.contains(&48000));
    }

    #[test]
    fn test_error_conditions() {
        // Zero sample rates
        assert!(LinearResampler::new(0, 48000, 1).is_err());
        assert!(LinearResampler::new(48000, 0, 1).is_err());
        
        // Zero channels
        assert!(LinearResampler::new(48000, 12000, 0).is_err());
        
        // Wrong input length
        let mut resampler = LinearResampler::new(48000, 12000, 2).unwrap();
        let result = resampler.process(&[0.1, 0.2, 0.3]); // Odd length for stereo
        assert!(result.is_err());
    }
}