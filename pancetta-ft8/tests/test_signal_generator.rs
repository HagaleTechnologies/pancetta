//! Test signal generator for FT8 testing
//! 
//! Generates synthetic FT8 signals for testing the decoder

use std::f32::consts::PI;

/// FT8 signal parameters
pub struct Ft8SignalParams {
    /// Base frequency in Hz (typically 1500 Hz for FT8)
    pub base_frequency: f32,
    /// Signal-to-noise ratio in dB
    pub snr_db: f32,
    /// Sample rate (must be 12000 for FT8)
    pub sample_rate: u32,
    /// Duration in seconds (must be 12.64 for FT8)
    pub duration: f32,
}

impl Default for Ft8SignalParams {
    fn default() -> Self {
        Self {
            base_frequency: 1500.0,
            snr_db: 0.0,
            sample_rate: 12000,
            duration: 12.64,
        }
    }
}

/// Generate a synthetic FT8 signal for testing
pub fn generate_ft8_test_signal(params: &Ft8SignalParams) -> Vec<f32> {
    let num_samples = (params.sample_rate as f32 * params.duration) as usize;
    let mut samples = Vec::with_capacity(num_samples);
    
    // FT8 uses 8-FSK modulation with tones spaced 6.25 Hz apart
    const TONE_SPACING: f32 = 6.25;
    const NUM_SYMBOLS: usize = 79; // FT8 has 79 symbols
    const SYMBOL_DURATION: f32 = 0.16; // 160ms per symbol
    
    // Generate a simple test pattern (not a real FT8 message)
    let test_tones = generate_test_tone_sequence();
    
    // Calculate signal amplitude from SNR
    let signal_amplitude = calculate_amplitude_from_snr(params.snr_db);
    
    let mut phase: f32 = 0.0;
    let samples_per_symbol = (params.sample_rate as f32 * SYMBOL_DURATION) as usize;
    
    for sample_idx in 0..num_samples {
        // Determine which symbol we're in
        let symbol_idx = sample_idx / samples_per_symbol;
        
        if symbol_idx < NUM_SYMBOLS {
            // Get the tone for this symbol (0-7 for 8-FSK)
            let tone = test_tones[symbol_idx];
            let frequency = params.base_frequency + (tone as f32 * TONE_SPACING);
            
            // Generate the sample
            let sample = signal_amplitude * phase.sin();
            samples.push(sample);
            
            // Update phase
            phase += 2.0 * PI * frequency / params.sample_rate as f32;
            if phase > 2.0 * PI {
                phase -= 2.0 * PI;
            }
        } else {
            // Silence after the FT8 transmission
            samples.push(0.0);
        }
    }
    
    // Add noise if SNR is not infinite
    if params.snr_db < 50.0 {
        add_gaussian_noise(&mut samples, params.snr_db);
    }
    
    samples
}

/// Generate a test tone sequence (simplified, not a real FT8 message)
fn generate_test_tone_sequence() -> Vec<u8> {
    // Generate a pattern that resembles FT8 sync and data
    let mut tones = Vec::with_capacity(79);
    
    // FT8 sync pattern positions (simplified)
    let sync_positions = vec![0, 6, 12, 18, 24, 30, 36, 42, 48, 54, 60, 66, 72];
    
    for i in 0..79 {
        if sync_positions.contains(&i) {
            // Sync tone (use tone 0 for simplicity)
            tones.push(0);
        } else {
            // Data tone (random for testing)
            tones.push((i % 8) as u8);
        }
    }
    
    tones
}

/// Calculate signal amplitude from desired SNR
fn calculate_amplitude_from_snr(snr_db: f32) -> f32 {
    // Assuming noise power of 1.0, calculate signal amplitude
    let snr_linear = 10.0_f32.powf(snr_db / 10.0);
    snr_linear.sqrt() * 0.1 // Scale down to reasonable amplitude
}

/// Add Gaussian noise to achieve desired SNR
fn add_gaussian_noise(samples: &mut [f32], snr_db: f32) {
    // Calculate signal power
    let signal_power: f32 = samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32;
    
    // Calculate required noise power
    let snr_linear = 10.0_f32.powf(snr_db / 10.0);
    let noise_power = signal_power / snr_linear;
    let noise_std = noise_power.sqrt();
    
    // Add pseudo-random noise (simplified - not true Gaussian)
    use std::num::Wrapping;
    let mut seed = Wrapping(12345u32);
    
    for sample in samples.iter_mut() {
        // Simple linear congruential generator for pseudo-random numbers
        seed = Wrapping(seed.0.wrapping_mul(1103515245).wrapping_add(12345));
        let random = (seed.0 / 65536) % 32768;
        let normalized = (random as f32 / 16384.0) - 1.0; // Range [-1, 1]
        
        *sample += normalized * noise_std;
    }
}

/// Generate a clean carrier wave for testing
pub fn generate_carrier(frequency: f32, sample_rate: u32, duration: f32) -> Vec<f32> {
    let num_samples = (sample_rate as f32 * duration) as usize;
    let mut samples = Vec::with_capacity(num_samples);
    let mut phase: f32 = 0.0;
    
    for _ in 0..num_samples {
        samples.push(0.1 * phase.sin());
        phase += 2.0 * PI * frequency / sample_rate as f32;
        if phase > 2.0 * PI {
            phase -= 2.0 * PI;
        }
    }
    
    samples
}

/// Generate silence
pub fn generate_silence(sample_rate: u32, duration: f32) -> Vec<f32> {
    let num_samples = (sample_rate as f32 * duration) as usize;
    vec![0.0; num_samples]
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_generate_ft8_signal() {
        let params = Ft8SignalParams::default();
        let signal = generate_ft8_test_signal(&params);
        
        // Check correct number of samples
        assert_eq!(signal.len(), 151680); // 12.64s * 12000Hz
        
        // Check signal is not all zeros
        let non_zero_count = signal.iter().filter(|&&x| x != 0.0).count();
        assert!(non_zero_count > 0, "Signal should contain non-zero samples");
        
        // Check signal amplitude is reasonable
        let max_amplitude = signal.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        assert!(max_amplitude > 0.0 && max_amplitude < 1.0, "Signal amplitude should be reasonable");
    }
    
    #[test]
    fn test_generate_carrier() {
        let signal = generate_carrier(1000.0, 12000, 1.0);
        
        assert_eq!(signal.len(), 12000);
        
        // Check for proper sinusoidal pattern
        let max = signal.iter().fold(f32::MIN, |a, &b| a.max(b));
        let min = signal.iter().fold(f32::MAX, |a, &b| a.min(b));
        
        assert!((max - 0.1).abs() < 0.001, "Max should be ~0.1");
        assert!((min + 0.1).abs() < 0.001, "Min should be ~-0.1");
    }
    
    #[test]
    fn test_generate_silence() {
        let signal = generate_silence(12000, 1.0);
        
        assert_eq!(signal.len(), 12000);
        assert!(signal.iter().all(|&x| x == 0.0), "All samples should be zero");
    }
    
    #[test]
    fn test_snr_levels() {
        // Test different SNR levels
        for snr in [-10.0, 0.0, 10.0, 20.0] {
            let params = Ft8SignalParams {
                snr_db: snr,
                ..Default::default()
            };
            
            let signal = generate_ft8_test_signal(&params);
            assert_eq!(signal.len(), 151680);
            
            // Higher SNR should generally have more consistent amplitude
            // (This is a simplified test)
            let variance: f32 = signal.iter()
                .map(|&x| x * x)
                .sum::<f32>() / signal.len() as f32;
            
            assert!(variance > 0.0, "Signal should have non-zero variance at SNR {}", snr);
        }
    }
}