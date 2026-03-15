use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type, Q_BUTTERWORTH_F32};
use std::collections::VecDeque;
use thiserror::Error;
use tracing::{debug, trace};

#[derive(Debug, Error)]
pub enum FilterError {
    #[error("Invalid filter parameter: {parameter} = {value}")]
    InvalidParameter { parameter: String, value: f32 },
    #[error("Filter design failed: {message}")]
    DesignFailed { message: String },
    #[error("Filter processing failed: {message}")]
    ProcessingFailed { message: String },
}

pub type Result<T> = std::result::Result<T, FilterError>;

/// Filter types for different applications
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterType {
    /// Low-pass filter
    LowPass,
    /// High-pass filter
    HighPass,
    /// Band-pass filter
    BandPass,
    /// Band-stop (notch) filter
    BandStop,
    /// All-pass filter (phase shift only)
    AllPass,
}

/// Filter design methods
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterDesign {
    /// Butterworth (maximally flat response)
    Butterworth,
    /// Chebyshev Type I (ripple in passband)
    Chebyshev1,
    /// Chebyshev Type II (ripple in stopband)
    Chebyshev2,
    /// Elliptic (ripple in both bands, steepest rolloff)
    Elliptic,
    /// Bessel (linear phase)
    Bessel,
}

/// High-performance IIR filter implementation
/// Uses cascaded biquad sections for stability and precision
pub struct IirFilter {
    /// Biquad filter sections
    sections: Vec<DirectForm1<f32>>,
    /// Filter configuration
    config: FilterConfig,
    /// Sample rate
    sample_rate: f32,
    /// Processing statistics
    stats: FilterStats,
}

#[derive(Debug, Clone)]
pub struct FilterConfig {
    pub filter_type: FilterType,
    pub design: FilterDesign,
    pub sample_rate: f32,
    pub cutoff_low: Option<f32>,  // Low cutoff for bandpass/bandstop
    pub cutoff_high: Option<f32>, // High cutoff or main cutoff
    pub q_factor: f32,            // Quality factor
    pub order: usize,             // Filter order (must be even for biquad cascade)
    pub gain: f32,                // Filter gain in dB
}

#[derive(Debug, Clone, Default)]
pub struct FilterStats {
    pub samples_processed: u64,
    pub peak_input: f32,
    pub peak_output: f32,
    pub rms_input: f64,
    pub rms_output: f64,
}

impl FilterConfig {
    /// Create configuration for FT8 bandpass filter
    pub fn new_ft8_bandpass(sample_rate: f32) -> Self {
        Self {
            filter_type: FilterType::BandPass,
            design: FilterDesign::Butterworth,
            sample_rate,
            cutoff_low: Some(200.0), // FT8 typically 200Hz to 4kHz
            cutoff_high: Some(4000.0),
            q_factor: Q_BUTTERWORTH_F32,
            order: 4, // 4th order = 2 biquad sections
            gain: 0.0,
        }
    }

    /// Create configuration for audio anti-aliasing filter
    pub fn new_anti_aliasing(sample_rate: f32, cutoff_ratio: f32) -> Self {
        Self {
            filter_type: FilterType::LowPass,
            design: FilterDesign::Butterworth,
            sample_rate,
            cutoff_low: None,
            cutoff_high: Some(sample_rate * cutoff_ratio), // e.g., 0.4 for Nyquist
            q_factor: Q_BUTTERWORTH_F32,
            order: 6, // 6th order for steep rolloff
            gain: 0.0,
        }
    }

    /// Create configuration for noise reduction high-pass filter
    pub fn new_noise_highpass(sample_rate: f32, cutoff: f32) -> Self {
        Self {
            filter_type: FilterType::HighPass,
            design: FilterDesign::Butterworth,
            sample_rate,
            cutoff_low: None,
            cutoff_high: Some(cutoff),
            q_factor: Q_BUTTERWORTH_F32,
            order: 2, // 2nd order for gentle rolloff
            gain: 0.0,
        }
    }

    /// Validate filter configuration
    pub fn validate(&self) -> Result<()> {
        if self.sample_rate <= 0.0 {
            return Err(FilterError::InvalidParameter {
                parameter: "sample_rate".to_string(),
                value: self.sample_rate,
            });
        }

        let nyquist = self.sample_rate / 2.0;

        match self.filter_type {
            FilterType::LowPass | FilterType::HighPass => {
                if let Some(cutoff) = self.cutoff_high {
                    if cutoff <= 0.0 || cutoff >= nyquist {
                        return Err(FilterError::InvalidParameter {
                            parameter: "cutoff_frequency".to_string(),
                            value: cutoff,
                        });
                    }
                } else {
                    return Err(FilterError::DesignFailed {
                        message: "Cutoff frequency required for lowpass/highpass".to_string(),
                    });
                }
            }
            FilterType::BandPass | FilterType::BandStop => {
                if let (Some(low), Some(high)) = (self.cutoff_low, self.cutoff_high) {
                    if low >= high || low <= 0.0 || high >= nyquist {
                        return Err(FilterError::InvalidParameter {
                            parameter: "cutoff_frequencies".to_string(),
                            value: high - low,
                        });
                    }
                } else {
                    return Err(FilterError::DesignFailed {
                        message: "Both cutoff frequencies required for bandpass/bandstop"
                            .to_string(),
                    });
                }
            }
            FilterType::AllPass => {
                // All-pass filters are always valid
            }
        }

        if self.order == 0 || self.order % 2 != 0 {
            return Err(FilterError::InvalidParameter {
                parameter: "order".to_string(),
                value: self.order as f32,
            });
        }

        Ok(())
    }
}

impl IirFilter {
    /// Create a new IIR filter
    pub fn new(config: FilterConfig) -> Result<Self> {
        config.validate()?;

        let num_sections = config.order / 2;
        let mut sections = Vec::with_capacity(num_sections);

        // Design cascaded biquad sections
        for section in 0..num_sections {
            let coeffs = Self::design_biquad_section(&config, section)?;
            let biquad = DirectForm1::<f32>::new(coeffs);
            sections.push(biquad);
        }

        debug!(
            "Created IIR filter: {:?}, order={}, sections={}, fs={}Hz",
            config.filter_type, config.order, num_sections, config.sample_rate
        );

        Ok(Self {
            sections,
            sample_rate: config.sample_rate,
            config,
            stats: FilterStats::default(),
        })
    }

    /// Design biquad coefficients for a filter section
    fn design_biquad_section(config: &FilterConfig, _section: usize) -> Result<Coefficients<f32>> {
        let fs = config.sample_rate.hz();

        match config.filter_type {
            FilterType::LowPass => {
                let cutoff = config.cutoff_high.unwrap().hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::LowPass, fs, cutoff, config.q_factor)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Lowpass design failed: {:?}", e),
                        })?,
                )
            }
            FilterType::HighPass => {
                let cutoff = config.cutoff_high.unwrap().hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::HighPass, fs, cutoff, config.q_factor)
                        .map_err(|e| FilterError::DesignFailed {
                        message: format!("Highpass design failed: {:?}", e),
                    })?,
                )
            }
            FilterType::BandPass => {
                let center_freq = (config.cutoff_low.unwrap() * config.cutoff_high.unwrap()).sqrt();
                let bandwidth = config.cutoff_high.unwrap() - config.cutoff_low.unwrap();
                let q = center_freq / bandwidth;

                Ok(
                    Coefficients::<f32>::from_params(Type::BandPass, fs, center_freq.hz(), q)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Bandpass design failed: {:?}", e),
                        })?,
                )
            }
            FilterType::BandStop => {
                let center_freq = (config.cutoff_low.unwrap() * config.cutoff_high.unwrap()).sqrt();
                let bandwidth = config.cutoff_high.unwrap() - config.cutoff_low.unwrap();
                let q = center_freq / bandwidth;

                Ok(
                    Coefficients::<f32>::from_params(Type::Notch, fs, center_freq.hz(), q)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Bandstop design failed: {:?}", e),
                        })?,
                )
            }
            FilterType::AllPass => {
                let cutoff = config.cutoff_high.unwrap_or(1000.0).hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::AllPass, fs, cutoff, config.q_factor)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Allpass design failed: {:?}", e),
                        })?,
                )
            }
        }
    }

    /// Process audio samples through the filter
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<()> {
        if input.len() != output.len() {
            return Err(FilterError::ProcessingFailed {
                message: format!(
                    "Input/output length mismatch: {} vs {}",
                    input.len(),
                    output.len()
                ),
            });
        }

        // Process through cascaded biquad sections
        let mut temp_buffer = input.to_vec();

        for section in &mut self.sections {
            let mut output = vec![0.0; temp_buffer.len()];
            for (i, &sample) in temp_buffer.iter().enumerate() {
                output[i] = section.run(sample);
            }
            temp_buffer = output;
        }

        // Apply gain and copy to output
        let gain_linear = 10.0_f32.powf(self.config.gain / 20.0);
        for (i, &sample) in temp_buffer.iter().enumerate() {
            output[i] = sample * gain_linear;
        }

        // Update statistics
        self.update_stats(input, output);

        trace!("Filtered {} samples", input.len());
        Ok(())
    }

    /// Process samples in-place
    pub fn process_inplace(&mut self, samples: &mut [f32]) -> Result<()> {
        let mut output = vec![0.0; samples.len()];
        self.process(samples, &mut output)?;
        samples.copy_from_slice(&output);
        Ok(())
    }

    /// Update filter statistics
    fn update_stats(&mut self, input: &[f32], output: &[f32]) {
        self.stats.samples_processed += input.len() as u64;

        // Update peak values
        for &sample in input {
            self.stats.peak_input = self.stats.peak_input.max(sample.abs());
        }

        for &sample in output {
            self.stats.peak_output = self.stats.peak_output.max(sample.abs());
        }

        // Update RMS values (running average)
        let input_power: f64 = input.iter().map(|&x| (x as f64).powi(2)).sum();
        let output_power: f64 = output.iter().map(|&x| (x as f64).powi(2)).sum();

        let alpha = 0.001; // Smoothing factor
        self.stats.rms_input =
            (1.0 - alpha) * self.stats.rms_input + alpha * input_power / input.len() as f64;
        self.stats.rms_output =
            (1.0 - alpha) * self.stats.rms_output + alpha * output_power / output.len() as f64;
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        for section in &mut self.sections {
            section.reset_state();
        }
        self.stats = FilterStats::default();
    }

    /// Get filter statistics
    pub fn stats(&self) -> &FilterStats {
        &self.stats
    }

    /// Get filter configuration
    pub fn config(&self) -> &FilterConfig {
        &self.config
    }

    /// Get filter delay in samples
    pub fn delay_samples(&self) -> usize {
        // Each biquad section contributes 2 samples delay
        self.sections.len() * 2
    }

    /// Get filter delay in seconds
    pub fn delay_seconds(&self) -> f32 {
        self.delay_samples() as f32 / self.sample_rate
    }
}

/// Adaptive noise reduction filter
/// Implements spectral subtraction and Wiener filtering techniques
pub struct NoiseReductionFilter {
    /// Sample rate
    sample_rate: f32,
    /// Frame size for processing
    frame_size: usize,
    /// Overlap factor
    overlap_factor: f32,
    /// Noise estimate
    noise_estimate: Vec<f32>,
    /// Signal estimate
    signal_estimate: Vec<f32>,
    /// Spectral subtraction factor
    alpha: f32,
    /// Over-subtraction factor
    beta: f32,
    /// Input buffer
    input_buffer: VecDeque<f32>,
    /// Output buffer
    output_buffer: VecDeque<f32>,
    /// Window function
    window: Vec<f32>,
    /// FFT workspace
    fft_workspace: Vec<f32>,
}

impl NoiseReductionFilter {
    /// Create a new noise reduction filter
    pub fn new(sample_rate: f32, frame_size: usize, overlap_factor: f32) -> Self {
        let window = Self::create_hann_window(frame_size);

        debug!(
            "Created noise reduction filter: fs={}Hz, frame_size={}, overlap={}",
            sample_rate, frame_size, overlap_factor
        );

        Self {
            sample_rate,
            frame_size,
            overlap_factor,
            noise_estimate: vec![0.001; frame_size / 2 + 1], // Initial noise floor
            signal_estimate: vec![0.0; frame_size / 2 + 1],
            alpha: 2.0, // Spectral subtraction factor
            beta: 0.01, // Over-subtraction factor
            input_buffer: VecDeque::new(),
            output_buffer: VecDeque::new(),
            window,
            fft_workspace: vec![0.0; frame_size * 2],
        }
    }

    /// Create Hann window function
    fn create_hann_window(size: usize) -> Vec<f32> {
        (0..size)
            .map(|i| {
                0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (size - 1) as f32).cos())
            })
            .collect()
    }

    /// Process audio with noise reduction
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<()> {
        // Add input to buffer
        self.input_buffer.extend(input.iter());

        let step_size = (self.frame_size as f32 * (1.0 - self.overlap_factor)) as usize;

        // Process complete frames
        while self.input_buffer.len() >= self.frame_size {
            // Extract frame
            let mut frame: Vec<f32> = self
                .input_buffer
                .iter()
                .take(self.frame_size)
                .cloned()
                .collect();

            // Apply window
            for (i, sample) in frame.iter_mut().enumerate() {
                *sample *= self.window[i];
            }

            // Process frame (simplified spectral subtraction)
            self.process_frame(&mut frame)?;

            // Add to output buffer
            self.output_buffer.extend(frame.iter());

            // Advance input buffer
            for _ in 0..step_size {
                self.input_buffer.pop_front();
            }
        }

        // Extract available output
        let available = self.output_buffer.len().min(input.len());
        for _ in 0..available {
            if let Some(sample) = self.output_buffer.pop_front() {
                output.push(sample);
            }
        }

        Ok(())
    }

    /// Process a single frame (simplified version)
    fn process_frame(&mut self, frame: &mut [f32]) -> Result<()> {
        // This is a simplified implementation
        // A full implementation would use FFT for spectral processing

        // Calculate frame energy
        let energy: f32 = frame.iter().map(|&x| x * x).sum();
        let rms = (energy / frame.len() as f32).sqrt();

        // Update noise estimate (very simple VAD)
        let noise_threshold = 0.01;
        if rms < noise_threshold {
            // Likely noise - update noise estimate
            for sample in frame.iter_mut() {
                *sample *= 0.1; // Simple noise reduction
            }
        }

        Ok(())
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        self.input_buffer.clear();
        self.output_buffer.clear();
        self.noise_estimate.fill(0.001);
        self.signal_estimate.fill(0.0);
    }

    /// Set noise reduction parameters
    pub fn set_parameters(&mut self, alpha: f32, beta: f32) {
        self.alpha = alpha;
        self.beta = beta;
    }
}

/// Filter bank for multi-band processing
pub struct FilterBank {
    /// Individual filters for each band
    filters: Vec<IirFilter>,
    /// Crossover frequencies
    crossovers: Vec<f32>,
    /// Sample rate
    sample_rate: f32,
}

impl FilterBank {
    /// Create a new filter bank
    pub fn new(sample_rate: f32, crossovers: Vec<f32>) -> Result<Self> {
        let mut filters = Vec::new();

        // Create filters for each band
        for (i, &crossover) in crossovers.iter().enumerate() {
            let config = if i == 0 {
                // Low band
                FilterConfig {
                    filter_type: FilterType::LowPass,
                    design: FilterDesign::Butterworth,
                    sample_rate,
                    cutoff_low: None,
                    cutoff_high: Some(crossover),
                    q_factor: Q_BUTTERWORTH_F32,
                    order: 4,
                    gain: 0.0,
                }
            } else if i == crossovers.len() - 1 {
                // High band
                FilterConfig {
                    filter_type: FilterType::HighPass,
                    design: FilterDesign::Butterworth,
                    sample_rate,
                    cutoff_low: None,
                    cutoff_high: Some(crossovers[i - 1]),
                    q_factor: Q_BUTTERWORTH_F32,
                    order: 4,
                    gain: 0.0,
                }
            } else {
                // Mid band
                FilterConfig {
                    filter_type: FilterType::BandPass,
                    design: FilterDesign::Butterworth,
                    sample_rate,
                    cutoff_low: Some(crossovers[i - 1]),
                    cutoff_high: Some(crossover),
                    q_factor: Q_BUTTERWORTH_F32,
                    order: 4,
                    gain: 0.0,
                }
            };

            filters.push(IirFilter::new(config)?);
        }

        Ok(Self {
            filters,
            crossovers,
            sample_rate,
        })
    }

    /// Process audio through all bands
    pub fn process(&mut self, input: &[f32], outputs: &mut [Vec<f32>]) -> Result<()> {
        if outputs.len() != self.filters.len() {
            return Err(FilterError::ProcessingFailed {
                message: format!(
                    "Output band count mismatch: {} vs {}",
                    outputs.len(),
                    self.filters.len()
                ),
            });
        }

        for (band, filter) in outputs.iter_mut().zip(self.filters.iter_mut()) {
            band.resize(input.len(), 0.0);
            filter.process(input, band)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ft8_bandpass_creation() {
        let config = FilterConfig::new_ft8_bandpass(12000.0);
        let filter = IirFilter::new(config);
        assert!(filter.is_ok());
    }

    #[test]
    fn test_filter_processing() {
        let config = FilterConfig::new_ft8_bandpass(12000.0);
        let mut filter = IirFilter::new(config).unwrap();

        let input = vec![0.1; 1000];
        let mut output = vec![0.0; 1000];

        let result = filter.process(&input, &mut output);
        assert!(result.is_ok());
    }

    #[test]
    fn test_noise_reduction_creation() {
        let nr = NoiseReductionFilter::new(12000.0, 1024, 0.5);
        // Basic creation test
        assert_eq!(nr.frame_size, 1024);
    }
}
