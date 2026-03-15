//! FT8 audio modulation for transmission
//!
//! This module handles generation of audio signals for FT8 transmission:
//! - 8-CPFSK (continuous-phase frequency shift keying) modulation
//! - Costas array synchronization sequences
//! - Configurable frequency offset and power levels
//! - Real-time audio generation with precise timing

use crate::{
    protocol::ProtocolParams,
    Ft8Error, Ft8Result, BASE_FREQUENCY, MESSAGE_DURATION, NUM_SYMBOLS, NUM_TONES, SAMPLE_RATE,
    SYMBOL_DURATION, TONE_SPACING,
};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Number of samples per FT8 symbol
pub const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;

/// Total samples in complete FT8 transmission
pub const TOTAL_TRANSMISSION_SAMPLES: usize = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;

/// Default transmission power level (0.0 to 1.0)
pub const DEFAULT_TX_POWER: f64 = 0.5;

/// Maximum frequency deviation (Hz)
pub const MAX_FREQUENCY_DEVIATION: f64 = 2500.0;

/// Default GFSK bandwidth-time product for FT8
pub const DEFAULT_BT: f64 = 2.0;

/// Pulse shaping mode for FT8 modulation
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PulseShape {
    /// Rectangular pulse (pure CPFSK, no smoothing)
    Rectangular,
    /// Gaussian pulse shaping with configurable BT product
    /// BT=2.0 is the FT8 standard (close to rectangular but with smooth transitions)
    Gaussian { bt: f64 },
}

impl Default for PulseShape {
    fn default() -> Self {
        // Rectangular is the default for decoder compatibility.
        // GFSK produces cleaner spectral output but requires a
        // matched decoder (not yet implemented).
        PulseShape::Rectangular
    }
}

/// FT8 audio modulator for generating transmission signals
pub struct Ft8Modulator {
    /// Sample rate for audio generation
    sample_rate: u32,
    /// Base frequency offset (typically 1500 Hz)
    base_frequency: f64,
    /// Tone spacing (6.25 Hz for FT8)
    tone_spacing: f64,
    /// Transmission power level (0.0 - 1.0)
    tx_power: f64,
    /// Phase accumulator for continuous phase modulation
    phase_accumulator: f64,
    /// Random number generator for dithering
    dither_state: u32,
    /// Pulse shaping mode
    pulse_shape: PulseShape,
}

impl Ft8Modulator {
    /// Create a new FT8 modulator
    ///
    /// # Arguments
    /// * `sample_rate` - Audio sample rate (typically 12000 Hz)
    /// * `base_frequency` - Base frequency offset in Hz (typically 1500 Hz)
    /// * `tx_power` - Transmission power level (0.0 - 1.0)
    pub fn new(sample_rate: u32, base_frequency: f64, tx_power: f64) -> Ft8Result<Self> {
        Self::with_pulse_shape(sample_rate, base_frequency, tx_power, PulseShape::default())
    }

    /// Create a new FT8 modulator with specific pulse shaping
    pub fn with_pulse_shape(
        sample_rate: u32,
        base_frequency: f64,
        tx_power: f64,
        pulse_shape: PulseShape,
    ) -> Ft8Result<Self> {
        if sample_rate == 0 || sample_rate > 192_000 {
            return Err(Ft8Error::ConfigError(format!(
                "Invalid sample rate: {} Hz",
                sample_rate
            )));
        }

        if base_frequency < 200.0 || base_frequency > 4000.0 {
            return Err(Ft8Error::ConfigError(format!(
                "Base frequency {} Hz out of range (200-4000 Hz)",
                base_frequency
            )));
        }

        if tx_power < 0.0 || tx_power > 1.0 {
            return Err(Ft8Error::ConfigError(format!(
                "TX power {} out of range (0.0-1.0)",
                tx_power
            )));
        }

        if let PulseShape::Gaussian { bt } = pulse_shape {
            if bt <= 0.0 || bt > 10.0 {
                return Err(Ft8Error::ConfigError(format!(
                    "BT product {} out of range (0.0-10.0)",
                    bt
                )));
            }
        }

        Ok(Self {
            sample_rate,
            base_frequency,
            tone_spacing: TONE_SPACING,
            tx_power,
            phase_accumulator: 0.0,
            dither_state: 12345, // Simple PRNG seed
            pulse_shape,
        })
    }

    /// Create modulator with default settings for FT8 (GFSK, BT=2.0)
    pub fn new_default() -> Ft8Result<Self> {
        Self::new(SAMPLE_RATE, BASE_FREQUENCY, DEFAULT_TX_POWER)
    }

    /// Create modulator with rectangular pulse shaping (pure CPFSK)
    pub fn new_rectangular() -> Ft8Result<Self> {
        Self::with_pulse_shape(
            SAMPLE_RATE,
            BASE_FREQUENCY,
            DEFAULT_TX_POWER,
            PulseShape::Rectangular,
        )
    }

    /// Generate transmission audio for any protocol (FT8, FT4, FT2).
    ///
    /// # Arguments
    /// * `symbols` - Symbol values (length must match `params.num_symbols`)
    /// * `frequency_offset` - Additional frequency offset in Hz
    /// * `params` - Protocol parameters defining tone count, spacing, timing
    ///
    /// # Returns
    /// Vector of audio samples ready for transmission
    pub fn modulate_symbols_protocol(
        &mut self,
        symbols: &[u8],
        frequency_offset: f64,
        params: &ProtocolParams,
    ) -> Ft8Result<Vec<f32>> {
        if symbols.len() != params.num_symbols {
            return Err(Ft8Error::InvalidDataSize {
                expected: params.num_symbols,
                actual: symbols.len(),
            });
        }

        if symbols.iter().any(|&s| s >= params.num_tones as u8) {
            return Err(Ft8Error::SignalProcessingError(format!(
                "Invalid symbol value (must be 0-{})",
                params.num_tones - 1
            )));
        }

        let tone_spacing = params.tone_spacing;
        let total_frequency = self.base_frequency + frequency_offset;
        if total_frequency < 200.0
            || total_frequency + (params.num_tones as f64 - 1.0) * tone_spacing
                > MAX_FREQUENCY_DEVIATION
        {
            return Err(Ft8Error::SignalProcessingError(format!(
                "Frequency {} Hz would exceed deviation limits",
                total_frequency
            )));
        }

        let sps = params.samples_per_symbol(self.sample_rate);
        let total_samples = params.total_samples(self.sample_rate);

        // Build per-sample frequency trajectory
        let freq_trajectory =
            self.build_frequency_trajectory_generic(symbols, total_frequency, tone_spacing, sps);

        // Generate audio from frequency trajectory via phase accumulation
        let mut audio_samples = Vec::with_capacity(total_samples);
        self.phase_accumulator = 0.0;

        let ramp_samples = (self.sample_rate as f64 * 0.005) as usize;

        for (i, &freq) in freq_trajectory.iter().enumerate() {
            let angular_freq = 2.0 * PI * freq / self.sample_rate as f64;
            self.phase_accumulator += angular_freq;
            if self.phase_accumulator > 2.0 * PI {
                self.phase_accumulator -= 2.0 * PI;
            }

            let mut sample = (self.tx_power * self.phase_accumulator.sin()) as f32;

            if i < ramp_samples {
                let factor = (i as f64 / ramp_samples as f64).sin().powi(2) as f32;
                sample *= factor;
            }
            let total = freq_trajectory.len();
            if i >= total - ramp_samples {
                let remaining = total - i;
                let factor = (remaining as f64 / ramp_samples as f64).sin().powi(2) as f32;
                sample *= factor;
            }

            audio_samples.push(sample);
        }

        self.apply_final_processing(&mut audio_samples)?;
        Ok(audio_samples)
    }

    /// Generate complete FT8 transmission audio samples
    ///
    /// # Arguments
    /// * `symbols` - Array of 79 FT8 symbols (0-7)
    /// * `frequency_offset` - Additional frequency offset in Hz
    ///
    /// # Returns
    /// Vector of audio samples ready for transmission
    pub fn modulate_symbols(
        &mut self,
        symbols: &[u8; NUM_SYMBOLS],
        frequency_offset: f64,
    ) -> Ft8Result<Vec<f32>> {
        if symbols.iter().any(|&s| s >= NUM_TONES as u8) {
            return Err(Ft8Error::SignalProcessingError(
                "Invalid symbol value (must be 0-7)".to_string(),
            ));
        }

        let total_frequency = self.base_frequency + frequency_offset;
        if total_frequency < 200.0
            || total_frequency + (NUM_TONES as f64 - 1.0) * self.tone_spacing
                > MAX_FREQUENCY_DEVIATION
        {
            return Err(Ft8Error::SignalProcessingError(format!(
                "Frequency {} Hz would exceed deviation limits",
                total_frequency
            )));
        }

        // Build per-sample frequency trajectory
        let freq_trajectory = self.build_frequency_trajectory(symbols, total_frequency);

        // Generate audio from frequency trajectory via phase accumulation
        let mut audio_samples = Vec::with_capacity(TOTAL_TRANSMISSION_SAMPLES);
        self.phase_accumulator = 0.0;

        let ramp_samples = (self.sample_rate as f64 * 0.005) as usize; // 5ms ramp

        for (i, &freq) in freq_trajectory.iter().enumerate() {
            let angular_freq = 2.0 * PI * freq / self.sample_rate as f64;
            self.phase_accumulator += angular_freq;
            if self.phase_accumulator > 2.0 * PI {
                self.phase_accumulator -= 2.0 * PI;
            }

            let mut sample = (self.tx_power * self.phase_accumulator.sin()) as f32;

            // Ramp up at start
            if i < ramp_samples {
                let factor = (i as f64 / ramp_samples as f64).sin().powi(2) as f32;
                sample *= factor;
            }
            // Ramp down at end
            let total = freq_trajectory.len();
            if i >= total - ramp_samples {
                let remaining = total - i;
                let factor = (remaining as f64 / ramp_samples as f64).sin().powi(2) as f32;
                sample *= factor;
            }

            audio_samples.push(sample);
        }

        // Apply final amplitude scaling and clipping protection
        self.apply_final_processing(&mut audio_samples)?;

        Ok(audio_samples)
    }

    /// Build per-sample frequency trajectory with pulse shaping.
    ///
    /// For `Rectangular`: each sample gets the frequency of its current symbol (pure CPFSK).
    /// For `Gaussian { bt }`: symbol transitions are smoothed by a Gaussian filter,
    /// producing GFSK (Gaussian Frequency Shift Keying).
    fn build_frequency_trajectory(&self, symbols: &[u8; NUM_SYMBOLS], base_freq: f64) -> Vec<f64> {
        let n_sym = NUM_SYMBOLS;
        let sps = SAMPLES_PER_SYMBOL;
        let total = n_sym * sps;

        match self.pulse_shape {
            PulseShape::Rectangular => {
                // Pure CPFSK: abrupt frequency transitions
                let mut trajectory = Vec::with_capacity(total);
                for &sym in symbols.iter() {
                    let freq = base_freq + (sym as f64) * self.tone_spacing;
                    for _ in 0..sps {
                        trajectory.push(freq);
                    }
                }
                trajectory
            }
            PulseShape::Gaussian { bt } => {
                // GFSK: smooth the rectangular frequency waveform with a Gaussian filter.
                //
                // 1. Build rectangular trajectory (same as CPFSK)
                // 2. Convolve with a Gaussian kernel to smooth symbol transitions
                //
                // The Gaussian kernel has standard deviation:
                //   σ = √(ln2) / (2π·BT) symbol periods
                // For BT=2.0, σ ≈ 0.059 symbols — very narrow, close to rectangular.

                // Step 1: rectangular trajectory
                let mut trajectory = Vec::with_capacity(total);
                for &sym in symbols.iter() {
                    let freq = base_freq + (sym as f64) * self.tone_spacing;
                    for _ in 0..sps {
                        trajectory.push(freq);
                    }
                }

                // Step 2: build Gaussian smoothing kernel
                let sigma_symbols = (2.0_f64.ln()).sqrt() / (2.0 * PI * bt);
                let sigma_samples = sigma_symbols * sps as f64;

                // Kernel spans ±3σ (captures 99.7% of energy)
                let half_len = (3.0 * sigma_samples).ceil() as usize;
                if half_len < 1 {
                    // BT so high that filter is sub-sample — skip filtering
                    return trajectory;
                }

                let kernel_len = 2 * half_len + 1;
                let mut kernel = vec![0.0f64; kernel_len];
                let mut kernel_sum = 0.0;

                for i in 0..kernel_len {
                    let t = (i as f64 - half_len as f64) / sigma_samples;
                    kernel[i] = (-0.5 * t * t).exp();
                    kernel_sum += kernel[i];
                }

                // Normalize kernel
                for k in kernel.iter_mut() {
                    *k /= kernel_sum;
                }

                // Step 3: convolve trajectory with kernel (same-size output)
                let mut smoothed = vec![0.0f64; total];
                for i in 0..total {
                    let mut acc = 0.0;
                    for j in 0..kernel_len {
                        let src_idx = i as isize + j as isize - half_len as isize;
                        // Clamp to boundary (extend edge values)
                        let src_idx = src_idx.max(0).min(total as isize - 1) as usize;
                        acc += trajectory[src_idx] * kernel[j];
                    }
                    smoothed[i] = acc;
                }

                smoothed
            }
        }
    }

    /// Build per-sample frequency trajectory for any protocol.
    fn build_frequency_trajectory_generic(
        &self,
        symbols: &[u8],
        base_freq: f64,
        tone_spacing: f64,
        sps: usize,
    ) -> Vec<f64> {
        let total = symbols.len() * sps;

        match self.pulse_shape {
            PulseShape::Rectangular => {
                let mut trajectory = Vec::with_capacity(total);
                for &sym in symbols.iter() {
                    let freq = base_freq + (sym as f64) * tone_spacing;
                    for _ in 0..sps {
                        trajectory.push(freq);
                    }
                }
                trajectory
            }
            PulseShape::Gaussian { bt } => {
                let mut trajectory = Vec::with_capacity(total);
                for &sym in symbols.iter() {
                    let freq = base_freq + (sym as f64) * tone_spacing;
                    for _ in 0..sps {
                        trajectory.push(freq);
                    }
                }

                let sigma_symbols = (2.0_f64.ln()).sqrt() / (2.0 * PI * bt);
                let sigma_samples = sigma_symbols * sps as f64;

                let half_len = (3.0 * sigma_samples).ceil() as usize;
                if half_len < 1 {
                    return trajectory;
                }

                let kernel_len = 2 * half_len + 1;
                let mut kernel = vec![0.0f64; kernel_len];
                let mut kernel_sum = 0.0;

                for i in 0..kernel_len {
                    let t = (i as f64 - half_len as f64) / sigma_samples;
                    kernel[i] = (-0.5 * t * t).exp();
                    kernel_sum += kernel[i];
                }
                for k in kernel.iter_mut() {
                    *k /= kernel_sum;
                }

                let mut smoothed = vec![0.0f64; total];
                for i in 0..total {
                    let mut acc = 0.0;
                    for j in 0..kernel_len {
                        let src_idx = i as isize + j as isize - half_len as isize;
                        let src_idx = src_idx.max(0).min(total as isize - 1) as usize;
                        acc += trajectory[src_idx] * kernel[j];
                    }
                    smoothed[i] = acc;
                }

                smoothed
            }
        }
    }

    /// Apply final processing: normalize amplitude with headroom and add dither
    fn apply_final_processing(&mut self, samples: &mut [f32]) -> Ft8Result<()> {
        let peak = samples.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);

        if peak > 0.0 {
            let headroom = 0.95; // 5% headroom
            let scale_factor = headroom / peak;

            for sample in samples.iter_mut() {
                *sample *= scale_factor;

                // Add dithering to reduce quantization noise
                let dither = self.generate_dither() * 1e-6;
                *sample += dither;
            }
        }

        Ok(())
    }

    /// Generate dither noise for quantization noise reduction
    fn generate_dither(&mut self) -> f32 {
        // Simple linear congruential generator for dither
        self.dither_state = self
            .dither_state
            .wrapping_mul(1103515245)
            .wrapping_add(12345);
        let normalized = (self.dither_state >> 16) as f32 / 32768.0;
        normalized - 1.0 // Range: -1.0 to 1.0
    }

    /// Set transmission power level
    pub fn set_tx_power(&mut self, power: f64) -> Ft8Result<()> {
        if power < 0.0 || power > 1.0 {
            return Err(Ft8Error::ConfigError(format!(
                "TX power {} out of range (0.0-1.0)",
                power
            )));
        }
        self.tx_power = power;
        Ok(())
    }

    /// Set base frequency offset
    pub fn set_base_frequency(&mut self, frequency: f64) -> Ft8Result<()> {
        if frequency < 200.0 || frequency > 4000.0 {
            return Err(Ft8Error::ConfigError(format!(
                "Base frequency {} Hz out of range (200-4000 Hz)",
                frequency
            )));
        }
        self.base_frequency = frequency;
        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> ModulatorConfig {
        ModulatorConfig {
            sample_rate: self.sample_rate,
            base_frequency: self.base_frequency,
            tone_spacing: self.tone_spacing,
            tx_power: self.tx_power,
            pulse_shape: self.pulse_shape,
        }
    }

    /// Set pulse shaping mode
    pub fn set_pulse_shape(&mut self, pulse_shape: PulseShape) -> Ft8Result<()> {
        if let PulseShape::Gaussian { bt } = pulse_shape {
            if bt <= 0.0 || bt > 10.0 {
                return Err(Ft8Error::ConfigError(format!(
                    "BT product {} out of range (0.0-10.0)",
                    bt
                )));
            }
        }
        self.pulse_shape = pulse_shape;
        Ok(())
    }

    /// Generate test tone for audio system verification
    pub fn generate_test_tone(&self, frequency: f64, duration_seconds: f64) -> Ft8Result<Vec<f32>> {
        let num_samples = (duration_seconds * self.sample_rate as f64) as usize;
        let mut samples = Vec::with_capacity(num_samples);
        let angular_frequency = 2.0 * PI * frequency / self.sample_rate as f64;

        for i in 0..num_samples {
            let phase = angular_frequency * i as f64;
            let sample = (self.tx_power * phase.sin()) as f32;
            samples.push(sample);
        }

        Ok(samples)
    }

    /// Calculate symbol timing for precise transmission scheduling
    pub fn calculate_symbol_timing(&self) -> SymbolTiming {
        SymbolTiming {
            samples_per_symbol: SAMPLES_PER_SYMBOL,
            symbol_duration_ms: (SYMBOL_DURATION * 1000.0) as u32,
            total_duration_ms: (MESSAGE_DURATION * 1000.0) as u32,
            sample_rate: self.sample_rate,
        }
    }
}

/// A single transmit request for multi-TX summation
pub struct MultiTxItem<'a> {
    /// Symbol values for this message
    pub symbols: &'a [u8],
    /// Frequency offset in Hz for this message
    pub frequency_offset: f64,
    /// Protocol parameters for this message
    pub params: &'a ProtocolParams,
}

/// Combine multiple transmission signals into a single audio buffer.
///
/// Each signal is modulated independently at its frequency offset, then all
/// are summed sample-by-sample and normalized to prevent clipping.
///
/// # Arguments
/// * `items` - Slice of transmit requests (message + frequency + protocol)
/// * `sample_rate` - Audio sample rate
/// * `base_frequency` - Base audio frequency (typically 1500 Hz)
/// * `tx_power` - Transmission power level (0.0-1.0)
///
/// # Returns
/// Combined audio samples with 0.95 headroom, or error if frequencies overlap.
pub fn modulate_multi_tx(
    items: &[MultiTxItem],
    sample_rate: u32,
    base_frequency: f64,
    tx_power: f64,
) -> Ft8Result<Vec<f32>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // Verify minimum frequency separation between all signal pairs
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let sep = (items[i].frequency_offset - items[j].frequency_offset).abs();
            // Minimum separation: wider signal's bandwidth + 25 Hz guard
            let bw_i = items[i].params.signal_bandwidth();
            let bw_j = items[j].params.signal_bandwidth();
            let min_sep = bw_i.max(bw_j) + 25.0;
            if sep < min_sep {
                return Err(Ft8Error::ConfigError(format!(
                    "Frequency separation {:.1} Hz too small (minimum {:.1} Hz) \
                     between signals at {:.1} and {:.1} Hz",
                    sep, min_sep, items[i].frequency_offset, items[j].frequency_offset
                )));
            }
        }
    }

    // Find the longest signal to determine output buffer size
    let max_samples = items
        .iter()
        .map(|item| item.params.total_samples(sample_rate))
        .max()
        .unwrap_or(0);

    // Modulate each signal independently
    let mut signals: Vec<Vec<f32>> = Vec::with_capacity(items.len());
    for item in items {
        let mut modulator =
            Ft8Modulator::new(sample_rate, base_frequency, tx_power)?;
        let audio = modulator.modulate_symbols_protocol(
            item.symbols,
            item.frequency_offset,
            item.params,
        )?;
        signals.push(audio);
    }

    // Sum all signals sample-by-sample
    let mut combined = vec![0.0f32; max_samples];
    for signal in &signals {
        for (i, &s) in signal.iter().enumerate() {
            if i < combined.len() {
                combined[i] += s;
            }
        }
    }

    // Normalize: divide by signal count and apply headroom
    let signal_count = signals.len() as f32;
    let peak = combined.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);
    if peak > 0.0 {
        let headroom = 0.95;
        let scale = headroom / peak;
        for sample in &mut combined {
            *sample *= scale;
        }
    }

    Ok(combined)
}

impl Default for Ft8Modulator {
    fn default() -> Self {
        Self::new_default().expect("Default modulator creation should not fail")
    }
}

/// Complementary error function approximation (Abramowitz & Stegun 7.1.26)
fn erfc(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let poly = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let result = poly * (-x * x).exp();
    if x >= 0.0 {
        result
    } else {
        2.0 - result
    }
}

/// Modulator configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModulatorConfig {
    /// Audio sample rate
    pub sample_rate: u32,
    /// Base frequency offset in Hz
    pub base_frequency: f64,
    /// Tone spacing in Hz
    pub tone_spacing: f64,
    /// Transmission power level (0.0 - 1.0)
    pub tx_power: f64,
    /// Pulse shaping mode
    pub pulse_shape: PulseShape,
}

impl Default for ModulatorConfig {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            base_frequency: BASE_FREQUENCY,
            tone_spacing: TONE_SPACING,
            tx_power: DEFAULT_TX_POWER,
            pulse_shape: PulseShape::default(),
        }
    }
}

/// Symbol timing information
#[derive(Debug, Clone)]
pub struct SymbolTiming {
    /// Number of samples per symbol
    pub samples_per_symbol: usize,
    /// Symbol duration in milliseconds
    pub symbol_duration_ms: u32,
    /// Total transmission duration in milliseconds
    pub total_duration_ms: u32,
    /// Sample rate
    pub sample_rate: u32,
}

/// Audio format specifications for FT8 transmission
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Sample rate (Hz)
    pub sample_rate: u32,
    /// Bits per sample
    pub bits_per_sample: u16,
    /// Number of channels (1 for mono)
    pub channels: u16,
    /// Sample format (f32, i16, etc.)
    pub sample_type: SampleType,
}

/// Audio sample format types
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SampleType {
    /// 32-bit floating point
    F32,
    /// 16-bit signed integer
    I16,
    /// 24-bit signed integer (packed)
    I24,
    /// 32-bit signed integer
    I32,
}

impl AudioFormat {
    /// Standard FT8 audio format (12 kHz, 16-bit, mono)
    pub fn ft8_standard() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            channels: 1,
            sample_type: SampleType::I16,
        }
    }

    /// High quality FT8 format (12 kHz, 32-bit float, mono)
    pub fn ft8_high_quality() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 32,
            channels: 1,
            sample_type: SampleType::F32,
        }
    }

    /// Calculate bytes per sample
    pub fn bytes_per_sample(&self) -> usize {
        (self.bits_per_sample as usize / 8) * self.channels as usize
    }

    /// Calculate frame size in bytes
    pub fn frame_size(&self) -> usize {
        self.bytes_per_sample()
    }
}

/// Convert f32 samples to specified audio format
pub fn convert_samples(samples: &[f32], format: AudioFormat) -> Vec<u8> {
    let mut output = Vec::with_capacity(samples.len() * format.bytes_per_sample());

    match format.sample_type {
        SampleType::F32 => {
            for &sample in samples {
                output.extend_from_slice(&sample.to_le_bytes());
            }
        }
        SampleType::I16 => {
            for &sample in samples {
                let scaled = (sample * 32767.0).round().max(-32768.0).min(32767.0) as i16;
                output.extend_from_slice(&scaled.to_le_bytes());
            }
        }
        SampleType::I24 => {
            for &sample in samples {
                let scaled = (sample * 8388607.0).round().max(-8388608.0).min(8388607.0) as i32;
                let bytes = scaled.to_le_bytes();
                output.extend_from_slice(&bytes[0..3]); // 24-bit = 3 bytes
            }
        }
        SampleType::I32 => {
            for &sample in samples {
                let scaled = (sample * 2147483647.0)
                    .round()
                    .max(-2147483648.0)
                    .min(2147483647.0) as i32;
                output.extend_from_slice(&scaled.to_le_bytes());
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modulator_creation() {
        let modulator = Ft8Modulator::new(12000, 1500.0, 0.5);
        assert!(modulator.is_ok());

        let mod_invalid_rate = Ft8Modulator::new(0, 1500.0, 0.5);
        assert!(mod_invalid_rate.is_err());

        let mod_invalid_freq = Ft8Modulator::new(12000, 5000.0, 0.5);
        assert!(mod_invalid_freq.is_err());

        let mod_invalid_power = Ft8Modulator::new(12000, 1500.0, 2.0);
        assert!(mod_invalid_power.is_err());
    }

    #[test]
    fn test_default_modulator() {
        let modulator = Ft8Modulator::new_default();
        assert!(modulator.is_ok());

        let config = modulator.unwrap().get_config();
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.base_frequency, BASE_FREQUENCY);
    }

    #[test]
    fn test_symbol_modulation() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let symbols = [
            0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4,
            5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1,
            2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6,
        ]; // 79 symbols

        let result = modulator.modulate_symbols(&symbols, 0.0);
        assert!(result.is_ok());

        let audio = result.unwrap();
        assert_eq!(audio.len(), TOTAL_TRANSMISSION_SAMPLES);

        // Check that samples are properly bounded
        assert!(audio.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn test_invalid_symbols() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let mut symbols = [0u8; 79];
        symbols[0] = 8; // Invalid symbol (> 7)

        let result = modulator.modulate_symbols(&symbols, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_frequency_limits() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let symbols = [0u8; 79];

        // Test excessive frequency offset
        let result = modulator.modulate_symbols(&symbols, 3000.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_test_tone_generation() {
        let modulator = Ft8Modulator::new_default().unwrap();
        let result = modulator.generate_test_tone(1000.0, 1.0);
        assert!(result.is_ok());

        let tone = result.unwrap();
        assert_eq!(tone.len(), 12000); // 1 second at 12 kHz
    }

    #[test]
    fn test_power_setting() {
        let mut modulator = Ft8Modulator::new_default().unwrap();

        assert!(modulator.set_tx_power(0.8).is_ok());
        assert_eq!(modulator.get_config().tx_power, 0.8);

        assert!(modulator.set_tx_power(1.5).is_err());
        assert!(modulator.set_tx_power(-0.1).is_err());
    }

    #[test]
    fn test_symbol_timing() {
        let modulator = Ft8Modulator::new_default().unwrap();
        let timing = modulator.calculate_symbol_timing();

        assert_eq!(timing.samples_per_symbol, SAMPLES_PER_SYMBOL);
        assert_eq!(timing.symbol_duration_ms, (SYMBOL_DURATION * 1000.0) as u32);
        assert_eq!(timing.total_duration_ms, (MESSAGE_DURATION * 1000.0) as u32);
    }

    #[test]
    fn test_audio_format_conversion() {
        let samples = vec![0.5, -0.5, 0.0, 1.0, -1.0];

        let format_i16 = AudioFormat::ft8_standard();
        let converted_i16 = convert_samples(&samples, format_i16);
        assert_eq!(converted_i16.len(), samples.len() * 2); // 2 bytes per sample

        let format_f32 = AudioFormat::ft8_high_quality();
        let converted_f32 = convert_samples(&samples, format_f32);
        assert_eq!(converted_f32.len(), samples.len() * 4); // 4 bytes per sample
    }

    #[test]
    fn test_modulator_config() {
        let config = ModulatorConfig::default();
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.base_frequency, BASE_FREQUENCY);
        assert_eq!(config.tone_spacing, TONE_SPACING);
        assert_eq!(config.tx_power, DEFAULT_TX_POWER);
        assert_eq!(config.pulse_shape, PulseShape::Rectangular);
    }

    #[test]
    fn test_gfsk_modulation_produces_valid_audio() {
        let mut modulator = Ft8Modulator::with_pulse_shape(
            SAMPLE_RATE,
            BASE_FREQUENCY,
            DEFAULT_TX_POWER,
            PulseShape::Gaussian { bt: 2.0 },
        )
        .unwrap();

        let symbols = [
            0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4,
            5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1,
            2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6,
        ]; // 79 symbols

        let result = modulator.modulate_symbols(&symbols, 0.0);
        assert!(result.is_ok());

        let audio = result.unwrap();
        assert_eq!(audio.len(), TOTAL_TRANSMISSION_SAMPLES);
        assert!(audio.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn test_gfsk_smoother_than_rectangular() {
        // GFSK should have smaller max frequency derivative (smoother transitions)
        let symbols = [
            0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0,
            7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7,
            0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0, 7, 0,
        ]; // worst-case transitions

        let rect_mod = Ft8Modulator::with_pulse_shape(
            SAMPLE_RATE,
            BASE_FREQUENCY,
            DEFAULT_TX_POWER,
            PulseShape::Rectangular,
        )
        .unwrap();
        let gfsk_mod = Ft8Modulator::with_pulse_shape(
            SAMPLE_RATE,
            BASE_FREQUENCY,
            DEFAULT_TX_POWER,
            PulseShape::Gaussian { bt: 2.0 },
        )
        .unwrap();

        let rect_traj = rect_mod.build_frequency_trajectory(&symbols, 1500.0);
        let gfsk_traj = gfsk_mod.build_frequency_trajectory(&symbols, 1500.0);

        // Max derivative (frequency change between adjacent samples)
        let rect_max_df: f64 = rect_traj
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);
        let gfsk_max_df: f64 = gfsk_traj
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max);

        // Rectangular has abrupt jumps, GFSK should be smoother
        assert!(
            gfsk_max_df < rect_max_df,
            "GFSK max dF ({:.4}) should be less than rectangular ({:.4})",
            gfsk_max_df,
            rect_max_df,
        );
    }

    #[test]
    fn test_pulse_shape_setting() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        assert_eq!(modulator.get_config().pulse_shape, PulseShape::Rectangular);

        assert!(modulator
            .set_pulse_shape(PulseShape::Gaussian { bt: 2.0 })
            .is_ok());
        assert_eq!(
            modulator.get_config().pulse_shape,
            PulseShape::Gaussian { bt: 2.0 }
        );

        assert!(modulator
            .set_pulse_shape(PulseShape::Gaussian { bt: 0.0 })
            .is_err());
        assert!(modulator
            .set_pulse_shape(PulseShape::Gaussian { bt: 11.0 })
            .is_err());
    }

    #[test]
    fn test_erfc_approximation() {
        // erfc(0) = 1.0
        assert!((erfc(0.0) - 1.0).abs() < 1e-6);
        // erfc(∞) → 0
        assert!(erfc(5.0) < 1e-10);
        // erfc(-∞) → 2
        assert!((erfc(-5.0) - 2.0).abs() < 1e-10);
        // erfc(1) ≈ 0.1573
        assert!((erfc(1.0) - 0.1573).abs() < 0.001);
    }

    #[test]
    fn test_rectangular_modulator_creation() {
        let modulator = Ft8Modulator::new_rectangular();
        assert!(modulator.is_ok());
        assert_eq!(
            modulator.unwrap().get_config().pulse_shape,
            PulseShape::Rectangular
        );
    }

    // ================================================================
    // Multi-TX tests
    // ================================================================

    #[test]
    fn test_multi_tx_two_ft8_signals() {
        use crate::protocol::ProtocolParams;

        let params = ProtocolParams::ft8();
        let symbols1 = [0u8; 79];
        let symbols2 = [3u8; 79];

        let items = vec![
            MultiTxItem {
                symbols: &symbols1,
                frequency_offset: -100.0,
                params: &params,
            },
            MultiTxItem {
                symbols: &symbols2,
                frequency_offset: 100.0,
                params: &params,
            },
        ];

        let combined = modulate_multi_tx(&items, SAMPLE_RATE, BASE_FREQUENCY, 0.5).unwrap();

        // Output length = max of the two signals
        assert_eq!(combined.len(), TOTAL_TRANSMISSION_SAMPLES);
        // No clipping
        assert!(combined.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn test_multi_tx_frequency_guard() {
        use crate::protocol::ProtocolParams;

        let params = ProtocolParams::ft8();
        let symbols = [0u8; 79];

        // FT8 bandwidth = 50 Hz, guard = 25 Hz → minimum separation = 75 Hz
        let items = vec![
            MultiTxItem {
                symbols: &symbols,
                frequency_offset: 0.0,
                params: &params,
            },
            MultiTxItem {
                symbols: &symbols,
                frequency_offset: 50.0, // Only 50 Hz apart — too close
                params: &params,
            },
        ];

        let result = modulate_multi_tx(&items, SAMPLE_RATE, BASE_FREQUENCY, 0.5);
        assert!(result.is_err(), "Should reject signals too close together");
    }

    #[test]
    fn test_multi_tx_sufficient_separation() {
        use crate::protocol::ProtocolParams;

        let params = ProtocolParams::ft8();
        let symbols = [0u8; 79];

        // 200 Hz separation is plenty for FT8 (min 75 Hz)
        let items = vec![
            MultiTxItem {
                symbols: &symbols,
                frequency_offset: -100.0,
                params: &params,
            },
            MultiTxItem {
                symbols: &symbols,
                frequency_offset: 100.0,
                params: &params,
            },
        ];

        let result = modulate_multi_tx(&items, SAMPLE_RATE, BASE_FREQUENCY, 0.5);
        assert!(result.is_ok());
    }

    #[test]
    fn test_multi_tx_mixed_protocols() {
        use crate::protocol::ProtocolParams;

        let ft8_params = ProtocolParams::ft8();
        let ft4_params = ProtocolParams::ft4();
        let ft8_symbols = [0u8; 79];
        let ft4_symbols = [0u8; 105];

        let items = vec![
            MultiTxItem {
                symbols: &ft8_symbols,
                frequency_offset: -200.0,
                params: &ft8_params,
            },
            MultiTxItem {
                symbols: &ft4_symbols,
                frequency_offset: 200.0,
                params: &ft4_params,
            },
        ];

        let combined = modulate_multi_tx(&items, SAMPLE_RATE, BASE_FREQUENCY, 0.5).unwrap();
        // Length should be max of FT8 (151680) and FT4 (60480)
        assert_eq!(combined.len(), TOTAL_TRANSMISSION_SAMPLES);
        assert!(combined.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn test_multi_tx_empty() {
        let items: Vec<MultiTxItem> = vec![];
        let result = modulate_multi_tx(&items, SAMPLE_RATE, BASE_FREQUENCY, 0.5).unwrap();
        assert!(result.is_empty());
    }
}
