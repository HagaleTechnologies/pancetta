use biquad::{Biquad, Coefficients, DirectForm1, ToHertz, Type, Q_BUTTERWORTH_F32};
use num_complex::Complex;
use realfft::RealFftPlanner;
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

        match config.filter_type {
            FilterType::BandPass => {
                // A true Butterworth bandpass is built from a highpass at f_low
                // cascaded with a lowpass at f_high, each of order N/2 per
                // biquad section. For a 4th-order bandpass (2 biquad sections),
                // we get one 2nd-order HP and one 2nd-order LP.
                let f_low = config.cutoff_low.unwrap();
                let f_high = config.cutoff_high.unwrap();
                let fs = config.sample_rate.hz();

                // Highpass sections at f_low
                for section in 0..num_sections {
                    let hp_order = num_sections * 2; // order of the HP sub-filter
                    let theta_k =
                        std::f32::consts::PI * (2 * section + 1) as f32 / (2 * hp_order) as f32;
                    let section_q = 1.0 / (2.0 * theta_k.cos());
                    let coeffs =
                        Coefficients::<f32>::from_params(Type::HighPass, fs, f_low.hz(), section_q)
                            .map_err(|e| FilterError::DesignFailed {
                                message: format!("Bandpass HP design failed: {:?}", e),
                            })?;
                    sections.push(DirectForm1::<f32>::new(coeffs));
                }

                // Lowpass sections at f_high
                for section in 0..num_sections {
                    let lp_order = num_sections * 2; // order of the LP sub-filter
                    let theta_k =
                        std::f32::consts::PI * (2 * section + 1) as f32 / (2 * lp_order) as f32;
                    let section_q = 1.0 / (2.0 * theta_k.cos());
                    let coeffs =
                        Coefficients::<f32>::from_params(Type::LowPass, fs, f_high.hz(), section_q)
                            .map_err(|e| FilterError::DesignFailed {
                                message: format!("Bandpass LP design failed: {:?}", e),
                            })?;
                    sections.push(DirectForm1::<f32>::new(coeffs));
                }
            }
            FilterType::BandStop => {
                // A Butterworth bandstop is built from a lowpass at f_low
                // cascaded with a highpass at f_high.
                let f_low = config.cutoff_low.unwrap();
                let f_high = config.cutoff_high.unwrap();
                let fs = config.sample_rate.hz();

                // Lowpass sections at f_low
                for section in 0..num_sections {
                    let lp_order = num_sections * 2;
                    let theta_k =
                        std::f32::consts::PI * (2 * section + 1) as f32 / (2 * lp_order) as f32;
                    let section_q = 1.0 / (2.0 * theta_k.cos());
                    let coeffs =
                        Coefficients::<f32>::from_params(Type::LowPass, fs, f_low.hz(), section_q)
                            .map_err(|e| FilterError::DesignFailed {
                                message: format!("Bandstop LP design failed: {:?}", e),
                            })?;
                    sections.push(DirectForm1::<f32>::new(coeffs));
                }

                // Highpass sections at f_high
                for section in 0..num_sections {
                    let hp_order = num_sections * 2;
                    let theta_k =
                        std::f32::consts::PI * (2 * section + 1) as f32 / (2 * hp_order) as f32;
                    let section_q = 1.0 / (2.0 * theta_k.cos());
                    let coeffs = Coefficients::<f32>::from_params(
                        Type::HighPass,
                        fs,
                        f_high.hz(),
                        section_q,
                    )
                    .map_err(|e| FilterError::DesignFailed {
                        message: format!("Bandstop HP design failed: {:?}", e),
                    })?;
                    sections.push(DirectForm1::<f32>::new(coeffs));
                }
            }
            _ => {
                // LowPass, HighPass, AllPass: standard cascaded biquad design
                for section in 0..num_sections {
                    let coeffs = Self::design_biquad_section(&config, section)?;
                    sections.push(DirectForm1::<f32>::new(coeffs));
                }
            }
        }

        debug!(
            "Created IIR filter: {:?}, order={}, sections={}, fs={}Hz",
            config.filter_type,
            config.order,
            sections.len(),
            config.sample_rate
        );

        Ok(Self {
            sections,
            sample_rate: config.sample_rate,
            config,
            stats: FilterStats::default(),
        })
    }

    /// Design biquad coefficients for a filter section
    ///
    /// For cascaded Butterworth filters, each section k (0-indexed) uses a different
    /// pole angle: theta_k = PI * (2*k + 1) / (2 * order), giving Q = 1 / (2 * cos(theta_k)).
    /// This ensures the overall cascade produces the correct Butterworth response.
    fn design_biquad_section(config: &FilterConfig, section: usize) -> Result<Coefficients<f32>> {
        let fs = config.sample_rate.hz();

        // Compute the per-section Q factor for Butterworth cascaded design.
        // For an Nth-order Butterworth split into N/2 biquad sections,
        // section k uses pole angle theta_k = PI * (2*k + 1) / (2 * N),
        // and Q_k = 1 / (2 * cos(theta_k)).
        let section_q = {
            let order = config.order;
            let theta_k = std::f32::consts::PI * (2 * section + 1) as f32 / (2 * order) as f32;
            1.0 / (2.0 * theta_k.cos())
        };

        match config.filter_type {
            FilterType::LowPass => {
                let cutoff = config.cutoff_high.unwrap().hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::LowPass, fs, cutoff, section_q)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Lowpass design failed: {:?}", e),
                        })?,
                )
            }
            FilterType::HighPass => {
                let cutoff = config.cutoff_high.unwrap().hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::HighPass, fs, cutoff, section_q)
                        .map_err(|e| FilterError::DesignFailed {
                            message: format!("Highpass design failed: {:?}", e),
                        })?,
                )
            }
            FilterType::BandPass | FilterType::BandStop => {
                // Handled directly in IirFilter::new() via HP+LP cascade;
                // this path should never be reached.
                unreachable!("BandPass/BandStop sections are built in IirFilter::new()")
            }
            FilterType::AllPass => {
                let cutoff = config.cutoff_high.unwrap_or(1000.0).hz();
                Ok(
                    Coefficients::<f32>::from_params(Type::AllPass, fs, cutoff, section_q)
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
    /// Noise estimate (power spectrum, frame_size/2 + 1 bins)
    noise_estimate: Vec<f32>,
    /// Signal estimate
    signal_estimate: Vec<f32>,
    /// Spectral subtraction factor
    alpha: f32,
    /// Spectral floor (minimum gain)
    beta: f32,
    /// Input buffer
    input_buffer: VecDeque<f32>,
    /// Output buffer
    output_buffer: VecDeque<f32>,
    /// Window function
    window: Vec<f32>,
    /// FFT workspace (real-valued, frame_size samples)
    fft_workspace: Vec<f32>,
    /// Complex FFT output workspace (frame_size/2 + 1 bins)
    fft_complex: Vec<Complex<f32>>,
    /// Forward FFT plan
    fft_forward: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
    /// Inverse FFT plan
    fft_inverse: std::sync::Arc<dyn realfft::ComplexToReal<f32>>,
    /// Number of frames processed (first CALIBRATION_FRAMES are noise-only)
    frames_processed: usize,
}

/// Number of initial frames used for noise-only calibration
const CALIBRATION_FRAMES: usize = 10;

impl NoiseReductionFilter {
    /// Create a new noise reduction filter
    pub fn new(sample_rate: f32, frame_size: usize, overlap_factor: f32) -> Self {
        let window = Self::create_hann_window(frame_size);
        let num_bins = frame_size / 2 + 1;

        let mut planner = RealFftPlanner::<f32>::new();
        let fft_forward = planner.plan_fft_forward(frame_size);
        let fft_inverse = planner.plan_fft_inverse(frame_size);
        let fft_complex = vec![Complex::new(0.0f32, 0.0); num_bins];

        debug!(
            "Created noise reduction filter: fs={}Hz, frame_size={}, overlap={}, bins={}",
            sample_rate, frame_size, overlap_factor, num_bins
        );

        Self {
            sample_rate,
            frame_size,
            overlap_factor,
            noise_estimate: vec![0.0; num_bins],
            signal_estimate: vec![0.0; num_bins],
            alpha: 2.0, // Spectral subtraction factor
            beta: 0.01, // Spectral floor (minimum gain)
            input_buffer: VecDeque::new(),
            output_buffer: VecDeque::new(),
            window,
            fft_workspace: vec![0.0; frame_size],
            fft_complex,
            fft_forward,
            fft_inverse,
            frames_processed: 0,
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

    /// Process a single frame with FFT-based spectral subtraction
    ///
    /// Steps:
    /// 1. Copy windowed frame into FFT workspace
    /// 2. Forward FFT (real-to-complex)
    /// 3. Compute power spectrum |X[k]|^2
    /// 4. Update noise estimate (calibration or slow tracking)
    /// 5. Spectral subtraction: gain[k] = max(1 - alpha * noise[k] / power[k], beta)
    /// 6. Apply gain to complex spectrum
    /// 7. Inverse FFT
    /// 8. Apply synthesis window and scale
    fn process_frame(&mut self, frame: &mut [f32]) -> Result<()> {
        let num_bins = self.frame_size / 2 + 1;
        let fft_size = self.frame_size;

        // (a) Copy input frame into FFT workspace (already windowed by caller)
        self.fft_workspace[..fft_size].copy_from_slice(&frame[..fft_size]);

        // (b) Forward FFT: real -> complex
        self.fft_forward
            .process(&mut self.fft_workspace, &mut self.fft_complex)
            .map_err(|e| FilterError::ProcessingFailed {
                message: format!("Forward FFT failed: {}", e),
            })?;

        // (c) Compute power spectrum: |X[k]|^2
        let mut power_spectrum = vec![0.0f32; num_bins];
        for k in 0..num_bins {
            power_spectrum[k] = self.fft_complex[k].norm_sqr();
        }

        // (d) Update noise estimate
        if self.frames_processed < CALIBRATION_FRAMES {
            // Calibration phase: accumulate average power spectrum
            for k in 0..num_bins {
                self.noise_estimate[k] += power_spectrum[k] / CALIBRATION_FRAMES as f32;
            }
        } else {
            // Slow tracking: exponential moving average with small alpha
            let tracking_alpha = 0.02f32;
            for k in 0..num_bins {
                self.noise_estimate[k] = (1.0 - tracking_alpha) * self.noise_estimate[k]
                    + tracking_alpha * power_spectrum[k].min(self.noise_estimate[k] * 4.0);
            }
        }

        // (e-f) Spectral subtraction and gain application
        // During calibration, pass through with unity gain (we're still learning noise)
        if self.frames_processed >= CALIBRATION_FRAMES {
            for k in 0..num_bins {
                let gain = if power_spectrum[k] > 1e-30 {
                    (1.0 - self.alpha * self.noise_estimate[k] / power_spectrum[k]).max(self.beta)
                } else {
                    self.beta
                };
                self.fft_complex[k] *= gain;
            }
        }

        // (g) Inverse FFT: complex -> real
        self.fft_inverse
            .process(&mut self.fft_complex, &mut self.fft_workspace)
            .map_err(|e| FilterError::ProcessingFailed {
                message: format!("Inverse FFT failed: {}", e),
            })?;

        // (h) Apply synthesis window and scale by 1/fft_size
        let scale = 1.0 / fft_size as f32;
        for i in 0..fft_size {
            frame[i] = self.fft_workspace[i] * self.window[i] * scale;
        }

        self.frames_processed += 1;
        trace!(
            "Processed frame {}, calibration={}",
            self.frames_processed,
            self.frames_processed <= CALIBRATION_FRAMES
        );

        Ok(())
    }

    /// Reset filter state
    pub fn reset(&mut self) {
        self.input_buffer.clear();
        self.output_buffer.clear();
        self.noise_estimate.fill(0.0);
        self.signal_estimate.fill(0.0);
        self.frames_processed = 0;
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
