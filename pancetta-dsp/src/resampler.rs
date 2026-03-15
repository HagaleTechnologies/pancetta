use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::collections::VecDeque;
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum ResamplerError {
    #[error("Resampler initialization failed: {message}")]
    InitializationFailed { message: String },
    #[error("Invalid sample rate: input={input_rate}, output={output_rate}")]
    InvalidSampleRate { input_rate: f32, output_rate: f32 },
    #[error("Resampling failed: {message}")]
    ProcessingFailed { message: String },
    #[error("Buffer size mismatch: expected {expected}, got {actual}")]
    BufferSizeMismatch { expected: usize, actual: usize },
}

pub type Result<T> = std::result::Result<T, ResamplerError>;

/// High-quality audio resampler optimized for amateur radio applications
/// Uses Rubato's high-quality SINC resampling with configurable parameters
pub struct AudioResampler {
    /// The underlying resampler engine (using SincFixedIn for simplicity)
    resampler: SincFixedIn<f32>,
    /// Input sample rate
    input_rate: f32,
    /// Output sample rate
    output_rate: f32,
    /// Resampling ratio
    ratio: f64,
    /// Input buffer for batching
    input_buffer: VecDeque<f32>,
    /// Output buffer for remainder samples
    output_buffer: VecDeque<f32>,
    /// Expected input chunk size
    input_chunk_size: usize,
    /// Expected output chunk size
    output_chunk_size: usize,
}

impl AudioResampler {
    /// Create a new high-quality resampler
    ///
    /// # Arguments
    /// * `input_rate` - Input sample rate in Hz
    /// * `output_rate` - Output sample rate in Hz
    /// * `chunk_size` - Input chunk size for processing
    ///
    /// # Returns
    /// A new AudioResampler instance optimized for the given rates
    pub fn new(input_rate: f32, output_rate: f32, chunk_size: usize) -> Result<Self> {
        if input_rate <= 0.0 || output_rate <= 0.0 {
            return Err(ResamplerError::InvalidSampleRate {
                input_rate,
                output_rate,
            });
        }

        let ratio = output_rate as f64 / input_rate as f64;

        debug!(
            "Creating resampler: {}Hz -> {}Hz (ratio: {:.6}), chunk_size: {}",
            input_rate, output_rate, ratio, chunk_size
        );

        // Configure SINC interpolation parameters for high quality
        let params = SincInterpolationParameters {
            sinc_len: 256,  // High quality, more taps
            f_cutoff: 0.95, // Preserve most of the frequency content
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256, // High oversampling for quality
            window: WindowFunction::BlackmanHarris2, // Excellent stopband attenuation
        };

        // Use SincFixedIn for all resampling scenarios
        let resampler = SincFixedIn::<f32>::new(
            output_rate as f64 / input_rate as f64,
            2.0, // Max relative deviation
            params,
            chunk_size,
            1, // Mono channel
        )
        .map_err(|e| ResamplerError::InitializationFailed {
            message: format!("SincFixedIn creation failed: {}", e),
        })?;

        let input_chunk_size = resampler.input_frames_next();
        let output_chunk_size = resampler.output_frames_next();

        debug!(
            "Resampler configured: input_chunk={}, output_chunk={}, delay={}",
            input_chunk_size,
            output_chunk_size,
            resampler.output_delay()
        );

        Ok(Self {
            resampler,
            input_rate,
            output_rate,
            ratio,
            input_buffer: VecDeque::new(),
            output_buffer: VecDeque::new(),
            input_chunk_size,
            output_chunk_size,
        })
    }

    /// Create a resampler optimized for FT8 (48kHz -> 12kHz)
    pub fn new_ft8_optimized() -> Result<Self> {
        // FT8 typically uses 12kHz sample rate, but we often receive 48kHz audio
        Self::new(48000.0, 12000.0, 4096)
    }

    /// Create a resampler for common audio rates
    pub fn new_audio_optimized(input_rate: f32, output_rate: f32) -> Result<Self> {
        // Choose chunk size based on sample rates for optimal performance
        let chunk_size = if input_rate >= 44100.0 { 4096 } else { 1024 };
        Self::new(input_rate, output_rate, chunk_size)
    }

    /// Process audio samples through the resampler
    ///
    /// # Arguments
    /// * `input` - Input audio samples
    /// * `output` - Output buffer for resampled audio
    ///
    /// # Returns
    /// Number of output samples produced
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<usize> {
        // Add input samples to buffer
        self.input_buffer.extend(input.iter());

        let mut total_output = 0;

        // Process complete chunks
        while self.input_buffer.len() >= self.input_chunk_size {
            // Extract input chunk
            let mut input_chunk = vec![Vec::new(); 1]; // Single channel
            for _ in 0..self.input_chunk_size {
                if let Some(sample) = self.input_buffer.pop_front() {
                    input_chunk[0].push(sample);
                }
            }

            // Resample the chunk
            let output_chunk = self.resampler.process(&input_chunk, None).map_err(|e| {
                ResamplerError::ProcessingFailed {
                    message: format!("Resampling failed: {}", e),
                }
            })?;

            // Add output samples to buffer
            self.output_buffer.extend(output_chunk[0].iter());
        }

        // Extract available output samples
        let available_output = self.output_buffer.len();

        for _ in 0..available_output {
            if let Some(sample) = self.output_buffer.pop_front() {
                output.push(sample);
                total_output += 1;
            }
        }

        debug!(
            "Resampled {} input samples -> {} output samples (buffer: in={}, out={})",
            input.len(),
            total_output,
            self.input_buffer.len(),
            self.output_buffer.len()
        );

        Ok(total_output)
    }

    /// Process samples and return exactly the requested number of output samples
    /// May require multiple input buffers to produce enough output
    pub fn process_exact(&mut self, input: &[f32], output_size: usize) -> Result<Vec<f32>> {
        let mut output = Vec::new();
        self.process(input, &mut output)?;

        // If we don't have enough output samples, return what we have
        if output.len() >= output_size {
            // Put remainder back in buffer before truncating
            if output.len() > output_size {
                let remainder: Vec<f32> = output.drain(output_size..).collect();
                // Insert at front of output buffer
                for sample in remainder.into_iter().rev() {
                    self.output_buffer.push_front(sample);
                }
            }
            output.truncate(output_size);
        }

        Ok(output)
    }

    /// Flush any remaining samples from the resampler
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let mut output = Vec::new();

        // Process any remaining input samples
        if !self.input_buffer.is_empty() {
            // Pad with zeros to complete the chunk if needed
            while self.input_buffer.len() < self.input_chunk_size {
                self.input_buffer.push_back(0.0);
            }

            // Process the final chunk
            let mut input_chunk = vec![Vec::new(); 1];
            for _ in 0..self.input_chunk_size {
                if let Some(sample) = self.input_buffer.pop_front() {
                    input_chunk[0].push(sample);
                }
            }

            let output_chunk = self.resampler.process(&input_chunk, None).map_err(|e| {
                ResamplerError::ProcessingFailed {
                    message: format!("Final resampling failed: {}", e),
                }
            })?;

            self.output_buffer.extend(output_chunk[0].iter());
        }

        // Return all buffered output samples
        while let Some(sample) = self.output_buffer.pop_front() {
            output.push(sample);
        }

        debug!("Flushed {} samples from resampler", output.len());
        Ok(output)
    }

    /// Get the resampling ratio
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Get input sample rate
    pub fn input_rate(&self) -> f32 {
        self.input_rate
    }

    /// Get output sample rate
    pub fn output_rate(&self) -> f32 {
        self.output_rate
    }

    /// Get the group delay introduced by the resampler
    pub fn delay_samples(&self) -> usize {
        self.resampler.output_delay()
    }

    /// Get the group delay in seconds
    pub fn delay_seconds(&self) -> f32 {
        self.delay_samples() as f32 / self.output_rate
    }

    /// Get expected input chunk size
    pub fn input_chunk_size(&self) -> usize {
        self.input_chunk_size
    }

    /// Get expected output chunk size  
    pub fn output_chunk_size(&self) -> usize {
        self.output_chunk_size
    }

    /// Calculate expected output length for given input length
    pub fn calculate_output_length(&self, input_length: usize) -> usize {
        (input_length as f64 * self.ratio) as usize
    }

    /// Get current buffer levels
    pub fn buffer_levels(&self) -> (usize, usize) {
        (self.input_buffer.len(), self.output_buffer.len())
    }

    /// Clear internal buffers
    pub fn reset(&mut self) {
        self.input_buffer.clear();
        self.output_buffer.clear();
        // Note: Rubato doesn't provide a reset method, so internal state remains
        warn!("Resampler buffers cleared, but internal filter state remains");
    }
}

/// Utility functions for common resampling scenarios
impl AudioResampler {
    /// Create a resampler for decimation (downsampling by integer factor)
    pub fn new_decimator(input_rate: f32, decimation_factor: usize) -> Result<Self> {
        let output_rate = input_rate / decimation_factor as f32;
        Self::new(input_rate, output_rate, 4096)
    }

    /// Create a resampler for interpolation (upsampling by integer factor)
    pub fn new_interpolator(input_rate: f32, interpolation_factor: usize) -> Result<Self> {
        let output_rate = input_rate * interpolation_factor as f32;
        Self::new(input_rate, output_rate, 1024)
    }

    /// Check if resampling is needed for given rates
    pub fn is_resampling_needed(input_rate: f32, output_rate: f32) -> bool {
        (input_rate - output_rate).abs() > f32::EPSILON
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ft8_resampler_creation() {
        let resampler = AudioResampler::new_ft8_optimized();
        assert!(resampler.is_ok());

        let resampler = resampler.unwrap();
        assert_eq!(resampler.input_rate(), 48000.0);
        assert_eq!(resampler.output_rate(), 12000.0);
        assert_eq!(resampler.ratio(), 0.25);
    }

    #[test]
    fn test_resampling_ratio_calculation() {
        let resampler = AudioResampler::new(48000.0, 12000.0, 1024).unwrap();
        assert_eq!(resampler.calculate_output_length(4800), 1200);
    }

    #[test]
    fn test_decimator_creation() {
        let resampler = AudioResampler::new_decimator(48000.0, 4).unwrap();
        assert_eq!(resampler.output_rate(), 12000.0);
    }
}
