//! Signal processing functions for FT8 decoding
//!
//! This module provides high-performance DSP functions optimized for FT8:
//! - FFT/IFFT operations with zero-allocation hot paths
//! - Window functions for spectral analysis
//! - Bandpass filtering for noise reduction
//! - Power spectral density estimation
//! - Symbol synchronization and timing recovery

// rationale: DSP loops index FFT/window/sample buffers by position; the index is
// load-bearing for the signal-processing math.
#![allow(clippy::needless_range_loop)]

use crate::{
    Ft8Error, Ft8Result, BASE_FREQUENCY, NUM_TONES, SAMPLE_RATE, SYMBOL_DURATION, TONE_SPACING,
};
use num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

/// Window function types for spectral analysis
#[derive(Debug, Default, Clone, Copy)]
pub enum WindowFunction {
    /// Rectangular window (no tapering)
    Rectangle,
    /// Hamming window (good side-lobe suppression)
    Hamming,
    /// Hann window (better spectral resolution)
    #[default]
    Hann,
    /// Blackman window (excellent side-lobe suppression)
    Blackman,
    /// Kaiser window with beta parameter
    Kaiser(f64),
}

/// High-performance FFT processor with reusable buffers
pub struct FftProcessor {
    /// FFT plan for forward transforms
    fft_forward: Arc<dyn Fft<f64>>,
    /// FFT plan for inverse transforms
    fft_inverse: Arc<dyn Fft<f64>>,
    /// FFT size
    fft_size: usize,
    /// Reusable input buffer
    input_buffer: Vec<Complex<f64>>,
    /// Reusable output buffer
    output_buffer: Vec<Complex<f64>>,
    /// Window function coefficients
    window_coeffs: Vec<f64>,
    /// Window function type
    window_type: WindowFunction,
}

impl FftProcessor {
    /// Create a new FFT processor with specified size and window function
    pub fn new(fft_size: usize, window_type: WindowFunction) -> Ft8Result<Self> {
        let mut planner = FftPlanner::new();
        let fft_forward = planner.plan_fft_forward(fft_size);
        let fft_inverse = planner.plan_fft_inverse(fft_size);

        let window_coeffs = generate_window(fft_size, window_type)?;

        Ok(Self {
            fft_forward,
            fft_inverse,
            fft_size,
            input_buffer: vec![Complex::new(0.0, 0.0); fft_size],
            output_buffer: vec![Complex::new(0.0, 0.0); fft_size],
            window_coeffs,
            window_type,
        })
    }

    /// Perform forward FFT on real-valued input
    pub fn fft_real(&mut self, input: &[f64]) -> Ft8Result<&[Complex<f64>]> {
        if input.len() > self.fft_size {
            return Err(Ft8Error::FftError(format!(
                "Input size {} exceeds FFT size {}",
                input.len(),
                self.fft_size
            )));
        }

        // Clear the input buffer first
        for i in 0..self.fft_size {
            self.input_buffer[i] = Complex::new(0.0, 0.0);
        }

        // Apply window function and convert to complex (with zero padding if needed)
        for (i, &sample) in input.iter().enumerate() {
            if i < self.fft_size {
                self.input_buffer[i] = Complex::new(sample * self.window_coeffs[i], 0.0);
            }
        }

        // Perform FFT
        self.output_buffer.copy_from_slice(&self.input_buffer);
        self.fft_forward.process(&mut self.output_buffer);

        Ok(&self.output_buffer)
    }

    /// Perform forward FFT on complex input
    pub fn fft_complex(&mut self, input: &[Complex<f64>]) -> Ft8Result<&[Complex<f64>]> {
        if input.len() != self.fft_size {
            return Err(Ft8Error::FftError(format!(
                "Input size {} doesn't match FFT size {}",
                input.len(),
                self.fft_size
            )));
        }

        // Apply window function
        for (i, &sample) in input.iter().enumerate() {
            self.input_buffer[i] = sample * self.window_coeffs[i];
        }

        // Perform FFT
        self.output_buffer.copy_from_slice(&self.input_buffer);
        self.fft_forward.process(&mut self.output_buffer);

        Ok(&self.output_buffer)
    }

    /// Perform inverse FFT
    pub fn ifft(&mut self, input: &[Complex<f64>]) -> Ft8Result<&[Complex<f64>]> {
        if input.len() != self.fft_size {
            return Err(Ft8Error::FftError(format!(
                "Input size {} doesn't match FFT size {}",
                input.len(),
                self.fft_size
            )));
        }

        self.input_buffer.copy_from_slice(input);
        self.fft_inverse.process(&mut self.input_buffer);

        // Normalize
        let scale = 1.0 / self.fft_size as f64;
        for sample in &mut self.input_buffer {
            *sample *= scale;
        }

        Ok(&self.input_buffer)
    }

    /// Compute power spectral density
    pub fn power_spectral_density(&mut self, input: &[f64]) -> Ft8Result<Vec<f64>> {
        let fft_size = self.fft_size; // Capture before borrowing
        let fft_result = self.fft_real(input)?;

        let mut psd = Vec::with_capacity(fft_size / 2 + 1);

        // DC component
        psd.push(fft_result[0].norm_sqr());

        // Positive frequencies (multiply by 2 for single-sided PSD)
        for i in 1..fft_size / 2 {
            psd.push(2.0 * fft_result[i].norm_sqr());
        }

        // Nyquist frequency
        if fft_size.is_multiple_of(2) {
            psd.push(fft_result[fft_size / 2].norm_sqr());
        }

        Ok(psd)
    }

    /// Get frequency bins for the FFT
    pub fn frequency_bins(&self) -> Vec<f64> {
        let df = SAMPLE_RATE as f64 / self.fft_size as f64;
        (0..=self.fft_size / 2).map(|i| i as f64 * df).collect()
    }

    /// Get the FFT size
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }
}

/// Generate window function coefficients
fn generate_window(size: usize, window_type: WindowFunction) -> Ft8Result<Vec<f64>> {
    let mut coeffs = vec![0.0; size];
    let n = size as f64;

    match window_type {
        WindowFunction::Rectangle => {
            coeffs.fill(1.0);
        }
        WindowFunction::Hamming => {
            for (i, coeff) in coeffs.iter_mut().enumerate() {
                let i = i as f64;
                *coeff = 0.54 - 0.46 * (2.0 * PI * i / (n - 1.0)).cos();
            }
        }
        WindowFunction::Hann => {
            for (i, coeff) in coeffs.iter_mut().enumerate() {
                let i = i as f64;
                *coeff = 0.5 * (1.0 - (2.0 * PI * i / (n - 1.0)).cos());
            }
        }
        WindowFunction::Blackman => {
            for (i, coeff) in coeffs.iter_mut().enumerate() {
                let i = i as f64;
                let arg = 2.0 * PI * i / (n - 1.0);
                *coeff = 0.42 - 0.5 * arg.cos() + 0.08 * (2.0 * arg).cos();
            }
        }
        WindowFunction::Kaiser(beta) => {
            let i0_beta = bessel_i0(beta);
            for (i, coeff) in coeffs.iter_mut().enumerate() {
                let i = i as f64;
                let arg = beta * (1.0 - ((2.0 * i / (n - 1.0)) - 1.0).powi(2)).sqrt();
                *coeff = bessel_i0(arg) / i0_beta;
            }
        }
    }

    Ok(coeffs)
}

/// Zero-order modified Bessel function of the first kind (for Kaiser window)
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let x_half_sq = (x / 2.0).powi(2);

    for k in 1..50 {
        term *= x_half_sq / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }

    sum
}

/// Bandpass filter for FT8 frequency range
pub struct BandpassFilter {
    /// Filter coefficients
    coefficients: Vec<f64>,
    /// Delay line for filtering
    delay_line: Vec<f64>,
    /// Current delay line index
    delay_index: usize,
    /// Center frequency
    center_freq: f64,
    /// Bandwidth
    bandwidth: f64,
}

impl BandpassFilter {
    /// Create a new bandpass filter
    pub fn new(center_freq: f64, bandwidth: f64, filter_order: usize) -> Ft8Result<Self> {
        let coefficients = design_bandpass_filter(center_freq, bandwidth, filter_order)?;
        let delay_line = vec![0.0; coefficients.len()];

        Ok(Self {
            coefficients,
            delay_line,
            delay_index: 0,
            center_freq,
            bandwidth,
        })
    }

    /// Filter a single sample
    pub fn filter_sample(&mut self, input: f64) -> f64 {
        // Add new sample to delay line
        self.delay_line[self.delay_index] = input;

        // Compute filter output
        let mut output = 0.0;
        for (i, &coeff) in self.coefficients.iter().enumerate() {
            let delay_idx = (self.delay_index + self.delay_line.len() - i) % self.delay_line.len();
            output += coeff * self.delay_line[delay_idx];
        }

        // Update delay line index
        self.delay_index = (self.delay_index + 1) % self.delay_line.len();

        output
    }

    /// Filter a batch of samples
    pub fn filter_batch(&mut self, input: &[f64], output: &mut [f64]) -> Ft8Result<()> {
        if input.len() != output.len() {
            return Err(Ft8Error::SignalProcessingError(
                "Input and output slices must have the same length".to_string(),
            ));
        }

        for (in_sample, out_sample) in input.iter().zip(output.iter_mut()) {
            *out_sample = self.filter_sample(*in_sample);
        }

        Ok(())
    }
}

/// Design a simple bandpass filter using windowed sinc method
fn design_bandpass_filter(center_freq: f64, bandwidth: f64, order: usize) -> Ft8Result<Vec<f64>> {
    if order.is_multiple_of(2) {
        return Err(Ft8Error::SignalProcessingError(
            "Filter order must be odd".to_string(),
        ));
    }

    let mut coeffs = vec![0.0; order];
    let m = (order - 1) / 2;
    let fs = SAMPLE_RATE as f64;

    let f1 = (center_freq - bandwidth / 2.0) / fs;
    let f2 = (center_freq + bandwidth / 2.0) / fs;

    for i in 0..order {
        let n = i as i32 - m as i32;

        if n == 0 {
            coeffs[i] = 2.0 * (f2 - f1);
        } else {
            let n_f = n as f64;
            coeffs[i] =
                (2.0 * PI * f2 * n_f).sin() / (PI * n_f) - (2.0 * PI * f1 * n_f).sin() / (PI * n_f);
        }

        // Apply Hamming window
        coeffs[i] *= 0.54 - 0.46 * (2.0 * PI * i as f64 / (order - 1) as f64).cos();
    }

    Ok(coeffs)
}

/// FT8 symbol correlator for timing and frequency detection
pub struct SymbolCorrelator {
    /// Symbol duration in samples
    symbol_samples: usize,
    /// FFT processor for correlation
    fft_processor: FftProcessor,
    /// Reference symbols for correlation
    reference_symbols: Vec<Vec<Complex<f64>>>,
}

impl SymbolCorrelator {
    /// Create a new symbol correlator
    pub fn new() -> Ft8Result<Self> {
        let symbol_samples = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
        // Use a fixed FFT size that's a power of 2 and fits the symbol
        let fft_size = symbol_samples.next_power_of_two();
        let fft_processor = FftProcessor::new(fft_size, WindowFunction::Hann)?;
        let reference_symbols = generate_ft8_reference_symbols(symbol_samples)?;

        Ok(Self {
            symbol_samples,
            fft_processor,
            reference_symbols,
        })
    }

    /// Correlate input signal with FT8 reference symbols
    pub fn correlate(&mut self, signal: &[f64]) -> Ft8Result<Vec<f64>> {
        if signal.len() < self.symbol_samples {
            return Err(Ft8Error::InsufficientData {
                needed: self.symbol_samples,
                available: signal.len(),
            });
        }

        let mut correlations = Vec::new();

        // Use smaller correlation windows to avoid FFT size issues
        let correlation_window = self.fft_processor.fft_size().min(self.symbol_samples);

        // Sliding window correlation
        for start in 0..=(signal.len() - correlation_window) {
            let window = &signal[start..start + correlation_window];
            let fft_result = self.fft_processor.fft_real(window)?;

            // Correlate with each reference symbol
            let mut max_correlation: f64 = 0.0;
            for reference in &self.reference_symbols {
                let correlation = compute_correlation(fft_result, reference);
                max_correlation = max_correlation.max(correlation);
            }

            correlations.push(max_correlation);
        }

        Ok(correlations)
    }
}

/// Generate reference symbols for FT8 correlation
fn generate_ft8_reference_symbols(symbol_samples: usize) -> Ft8Result<Vec<Vec<Complex<f64>>>> {
    let mut symbols = Vec::new();
    let dt = 1.0 / SAMPLE_RATE as f64;

    // Generate reference symbols for each of the 8 tones
    for tone in 0..NUM_TONES {
        let mut symbol = Vec::with_capacity(symbol_samples);
        let freq = BASE_FREQUENCY + tone as f64 * TONE_SPACING;

        for i in 0..symbol_samples {
            let t = i as f64 * dt;
            let phase = 2.0 * PI * freq * t;
            symbol.push(Complex::new(phase.cos(), phase.sin()));
        }

        symbols.push(symbol);
    }

    Ok(symbols)
}

/// Compute correlation between two complex signals
fn compute_correlation(signal1: &[Complex<f64>], signal2: &[Complex<f64>]) -> f64 {
    if signal1.len() != signal2.len() {
        return 0.0;
    }

    let mut correlation = Complex::new(0.0, 0.0);
    for (s1, s2) in signal1.iter().zip(signal2.iter()) {
        correlation += s1 * s2.conj();
    }

    correlation.norm()
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn test_fft_processor_creation() {
        let processor = FftProcessor::new(1024, WindowFunction::Hann);
        assert!(processor.is_ok());
    }

    #[test]
    fn test_window_functions() {
        let size = 64;

        // Test different window functions
        let windows = [
            WindowFunction::Rectangle,
            WindowFunction::Hamming,
            WindowFunction::Hann,
            WindowFunction::Blackman,
            WindowFunction::Kaiser(5.0),
        ];

        for window in windows {
            let coeffs = generate_window(size, window).unwrap();
            assert_eq!(coeffs.len(), size);

            // Check that all coefficients are finite
            for coeff in coeffs {
                assert!(coeff.is_finite());
            }
        }
    }

    #[test]
    fn test_bandpass_filter() {
        let mut filter = BandpassFilter::new(1500.0, 50.0, 65).unwrap();

        // Test filtering a simple signal
        let input = vec![1.0, 0.0, -1.0, 0.0];
        let mut output = vec![0.0; input.len()];

        filter.filter_batch(&input, &mut output).unwrap();

        // Output should be finite
        for sample in output {
            assert!(sample.is_finite());
        }
    }

    #[test]
    fn test_symbol_correlator() {
        let correlator = SymbolCorrelator::new();
        assert!(correlator.is_ok());
    }

    #[test]
    fn test_bessel_i0() {
        // Test known values
        assert_relative_eq!(bessel_i0(0.0), 1.0, epsilon = 1e-10);
        assert_relative_eq!(bessel_i0(1.0), 1.2660658777520084, epsilon = 1e-10);
    }

    #[test]
    fn test_correlation() {
        let signal1 = vec![
            Complex::new(1.0, 0.0),
            Complex::new(0.0, 1.0),
            Complex::new(-1.0, 0.0),
            Complex::new(0.0, -1.0),
        ];

        let signal2 = signal1.clone();
        let correlation = compute_correlation(&signal1, &signal2);

        // Self-correlation should be positive
        assert!(correlation > 0.0);
    }
}
