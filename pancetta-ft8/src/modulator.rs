//! FT8 audio modulation for transmission
//!
//! This module handles generation of audio signals for FT8 transmission:
//! - 8-CPFSK (continuous-phase frequency shift keying) modulation
//! - Costas array synchronization sequences
//! - Configurable frequency offset and power levels
//! - Real-time audio generation with precise timing

use crate::{
    Ft8Error, Ft8Result, SAMPLE_RATE, SYMBOL_DURATION, MESSAGE_DURATION, 
    BASE_FREQUENCY, TONE_SPACING, NUM_SYMBOLS, NUM_TONES
};
use std::f64::consts::PI;
use serde::{Deserialize, Serialize};

/// Number of samples per FT8 symbol
pub const SAMPLES_PER_SYMBOL: usize = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;

/// Total samples in complete FT8 transmission
pub const TOTAL_TRANSMISSION_SAMPLES: usize = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;

/// Default transmission power level (0.0 to 1.0)
pub const DEFAULT_TX_POWER: f64 = 0.5;

/// Maximum frequency deviation (Hz)
pub const MAX_FREQUENCY_DEVIATION: f64 = 2500.0;

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
}

impl Ft8Modulator {
    /// Create a new FT8 modulator
    ///
    /// # Arguments
    /// * `sample_rate` - Audio sample rate (typically 12000 Hz)
    /// * `base_frequency` - Base frequency offset in Hz (typically 1500 Hz)
    /// * `tx_power` - Transmission power level (0.0 - 1.0)
    pub fn new(sample_rate: u32, base_frequency: f64, tx_power: f64) -> Ft8Result<Self> {
        if sample_rate == 0 || sample_rate > 192_000 {
            return Err(Ft8Error::ConfigError(
                format!("Invalid sample rate: {} Hz", sample_rate)
            ));
        }
        
        if base_frequency < 200.0 || base_frequency > 4000.0 {
            return Err(Ft8Error::ConfigError(
                format!("Base frequency {} Hz out of range (200-4000 Hz)", base_frequency)
            ));
        }
        
        if tx_power < 0.0 || tx_power > 1.0 {
            return Err(Ft8Error::ConfigError(
                format!("TX power {} out of range (0.0-1.0)", tx_power)
            ));
        }
        
        Ok(Self {
            sample_rate,
            base_frequency,
            tone_spacing: TONE_SPACING,
            tx_power,
            phase_accumulator: 0.0,
            dither_state: 12345, // Simple PRNG seed
        })
    }

    /// Create modulator with default settings for FT8
    pub fn new_default() -> Ft8Result<Self> {
        Self::new(SAMPLE_RATE, BASE_FREQUENCY, DEFAULT_TX_POWER)
    }

    /// Generate complete FT8 transmission audio samples
    ///
    /// # Arguments
    /// * `symbols` - Array of 79 FT8 symbols (0-7)
    /// * `frequency_offset` - Additional frequency offset in Hz
    ///
    /// # Returns
    /// Vector of audio samples ready for transmission
    pub fn modulate_symbols(&mut self, symbols: &[u8; NUM_SYMBOLS], frequency_offset: f64) -> Ft8Result<Vec<f32>> {
        if symbols.iter().any(|&s| s >= NUM_TONES as u8) {
            return Err(Ft8Error::SignalProcessingError(
                "Invalid symbol value (must be 0-7)".to_string()
            ));
        }
        
        let total_frequency = self.base_frequency + frequency_offset;
        if total_frequency < 200.0 || total_frequency + (NUM_TONES as f64 - 1.0) * self.tone_spacing > MAX_FREQUENCY_DEVIATION {
            return Err(Ft8Error::SignalProcessingError(
                format!("Frequency {} Hz would exceed deviation limits", total_frequency)
            ));
        }
        
        let mut audio_samples = Vec::with_capacity(TOTAL_TRANSMISSION_SAMPLES);
        
        // Reset phase accumulator for new transmission
        self.phase_accumulator = 0.0;
        
        // Generate audio for each symbol
        for (symbol_idx, &symbol) in symbols.iter().enumerate() {
            let symbol_frequency = total_frequency + (symbol as f64) * self.tone_spacing;
            let symbol_samples = self.generate_symbol_audio(symbol_frequency, symbol_idx)?;
            audio_samples.extend_from_slice(&symbol_samples);
        }
        
        // Apply final amplitude scaling and clipping protection
        self.apply_final_processing(&mut audio_samples)?;
        
        Ok(audio_samples)
    }

    /// Generate audio samples for a single CPFSK symbol
    ///
    /// FT8 uses continuous-phase FSK: abrupt frequency transitions at symbol
    /// boundaries with phase continuity maintained via the phase accumulator.
    fn generate_symbol_audio(&mut self, frequency: f64, symbol_idx: usize) -> Ft8Result<Vec<f32>> {
        let mut samples = Vec::with_capacity(SAMPLES_PER_SYMBOL);
        let angular_frequency = 2.0 * PI * frequency / self.sample_rate as f64;

        for _sample_idx in 0..SAMPLES_PER_SYMBOL {
            self.phase_accumulator += angular_frequency;

            // Keep phase in reasonable range to prevent numerical issues
            if self.phase_accumulator > 2.0 * PI {
                self.phase_accumulator -= 2.0 * PI;
            }

            let amplitude = self.tx_power as f32;
            let sample = amplitude * self.phase_accumulator.sin() as f32;
            samples.push(sample);
        }

        // Apply ramp-up/ramp-down at transmission boundaries
        self.apply_symbol_shaping(&mut samples, symbol_idx)?;

        Ok(samples)
    }

    /// Apply symbol transition shaping to reduce spectral splatter
    fn apply_symbol_shaping(&self, samples: &mut [f32], symbol_idx: usize) -> Ft8Result<()> {
        let ramp_samples = (self.sample_rate as f64 * 0.01) as usize; // 10ms ramp
        let ramp_samples = ramp_samples.min(samples.len() / 4);
        
        // Apply smooth transitions at symbol boundaries
        if symbol_idx == 0 {
            // Ramp up at start of transmission
            for i in 0..ramp_samples {
                let factor = (i as f64 / ramp_samples as f64).sin().powi(2) as f32;
                samples[i] *= factor;
            }
        }
        
        if symbol_idx == NUM_SYMBOLS - 1 {
            // Ramp down at end of transmission
            let start_idx = samples.len() - ramp_samples;
            for i in 0..ramp_samples {
                let factor = ((ramp_samples - i) as f64 / ramp_samples as f64).sin().powi(2) as f32;
                samples[start_idx + i] *= factor;
            }
        }
        
        Ok(())
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
        self.dither_state = self.dither_state.wrapping_mul(1103515245).wrapping_add(12345);
        let normalized = (self.dither_state >> 16) as f32 / 32768.0;
        normalized - 1.0 // Range: -1.0 to 1.0
    }

    /// Set transmission power level
    pub fn set_tx_power(&mut self, power: f64) -> Ft8Result<()> {
        if power < 0.0 || power > 1.0 {
            return Err(Ft8Error::ConfigError(
                format!("TX power {} out of range (0.0-1.0)", power)
            ));
        }
        self.tx_power = power;
        Ok(())
    }

    /// Set base frequency offset
    pub fn set_base_frequency(&mut self, frequency: f64) -> Ft8Result<()> {
        if frequency < 200.0 || frequency > 4000.0 {
            return Err(Ft8Error::ConfigError(
                format!("Base frequency {} Hz out of range (200-4000 Hz)", frequency)
            ));
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
        }
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

impl Default for Ft8Modulator {
    fn default() -> Self {
        Self::new_default().expect("Default modulator creation should not fail")
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
}

impl Default for ModulatorConfig {
    fn default() -> Self {
        Self {
            sample_rate: SAMPLE_RATE,
            base_frequency: BASE_FREQUENCY,
            tone_spacing: TONE_SPACING,
            tx_power: DEFAULT_TX_POWER,
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
                let scaled = (sample * 2147483647.0).round().max(-2147483648.0).min(2147483647.0) as i32;
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
        let symbols = [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 
                       0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7,
                       0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7,
                       0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7,
                       0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6]; // 79 symbols
        
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
    }
}