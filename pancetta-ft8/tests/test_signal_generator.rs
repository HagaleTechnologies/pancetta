//! Test signal generator for FT8 testing
//!
//! When the `transmit` feature is enabled, generates real FT8 signals
//! using the encoder+modulator pipeline. Otherwise, provides utility
//! functions for basic signal generation.

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

/// Generate a real FT8 signal using the encoder+modulator pipeline
///
/// Encodes the message text, modulates to audio, adds noise for desired SNR.
#[cfg(feature = "transmit")]
pub fn generate_real_ft8_signal(
    message_text: &str,
    snr_db: f32,
    frequency_offset: f64,
) -> Vec<f32> {
    use pancetta_ft8::{Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES};

    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default().unwrap();

    // Encode message to symbols
    let symbols = encoder
        .encode_message(message_text, None)
        .expect("Failed to encode message");

    // Modulate symbols to audio
    let mut audio = modulator
        .modulate_symbols(&symbols, frequency_offset)
        .expect("Failed to modulate symbols");

    // Pad to full window size
    audio.resize(WINDOW_SAMPLES, 0.0);

    // Add noise for desired SNR
    if snr_db < 50.0 {
        add_gaussian_noise(&mut audio, snr_db);
    }

    audio
}

/// Generate a synthetic FT8-like signal for testing (no real encoding)
pub fn generate_ft8_test_signal(params: &Ft8SignalParams) -> Vec<f32> {
    let num_samples = (params.sample_rate as f32 * params.duration) as usize;
    let mut samples = Vec::with_capacity(num_samples);

    const TONE_SPACING: f32 = 6.25;
    const NUM_SYMBOLS: usize = 79;
    const SYMBOL_DURATION: f32 = 0.16;

    let test_tones = generate_test_tone_sequence();
    let signal_amplitude = calculate_amplitude_from_snr(params.snr_db);

    let mut phase: f32 = 0.0;
    let samples_per_symbol = (params.sample_rate as f32 * SYMBOL_DURATION) as usize;

    for sample_idx in 0..num_samples {
        let symbol_idx = sample_idx / samples_per_symbol;

        if symbol_idx < NUM_SYMBOLS {
            let tone = test_tones[symbol_idx];
            let frequency = params.base_frequency + (tone as f32 * TONE_SPACING);

            let sample = signal_amplitude * phase.sin();
            samples.push(sample);

            phase += 2.0 * PI * frequency / params.sample_rate as f32;
            if phase > 2.0 * PI {
                phase -= 2.0 * PI;
            }
        } else {
            samples.push(0.0);
        }
    }

    if params.snr_db < 50.0 {
        add_gaussian_noise(&mut samples, params.snr_db);
    }

    samples
}

/// Generate a test tone sequence (simplified, not a real FT8 message)
fn generate_test_tone_sequence() -> Vec<u8> {
    let mut tones = Vec::with_capacity(79);

    // Use the real Costas array at sync positions
    let costas = [3u8, 1, 4, 0, 6, 5, 2];

    for i in 0..79 {
        if i < 7 {
            tones.push(costas[i]);
        } else if i >= 36 && i < 43 {
            tones.push(costas[i - 36]);
        } else if i >= 72 {
            tones.push(costas[i - 72]);
        } else {
            tones.push((i % 8) as u8);
        }
    }

    tones
}

fn calculate_amplitude_from_snr(snr_db: f32) -> f32 {
    let snr_linear = 10.0_f32.powf(snr_db / 10.0);
    snr_linear.sqrt() * 0.1
}

/// Add Gaussian noise to audio samples to achieve desired SNR
pub fn add_gaussian_noise(samples: &mut [f32], snr_db: f32) {
    let signal_power: f32 = samples.iter().map(|&x| x * x).sum::<f32>() / samples.len() as f32;

    if signal_power < 1e-12 {
        // No signal — just add noise at a fixed level
        let noise_std = 0.01;
        let mut seed = 12345u32;
        for sample in samples.iter_mut() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let r1 = ((seed >> 16) % 32768) as f32 / 16384.0 - 1.0;
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let r2 = ((seed >> 16) % 32768) as f32 / 16384.0 - 1.0;
            *sample += (r1 + r2) * 0.5 * noise_std;
        }
        return;
    }

    let snr_linear = 10.0_f32.powf(snr_db / 10.0);
    let noise_power = signal_power / snr_linear;
    let noise_std = noise_power.sqrt();

    // Box-Muller-like approximation using LCG
    let mut seed = 12345u32;
    for sample in samples.iter_mut() {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let r1 = ((seed >> 16) % 32768) as f32 / 16384.0 - 1.0;
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let r2 = ((seed >> 16) % 32768) as f32 / 16384.0 - 1.0;
        // Central limit theorem: sum of 2 uniform ≈ Gaussian
        *sample += (r1 + r2) * 0.5 * noise_std;
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

        assert_eq!(signal.len(), 151680);

        let non_zero_count = signal.iter().filter(|&&x| x != 0.0).count();
        assert!(non_zero_count > 0, "Signal should contain non-zero samples");

        let max_amplitude = signal.iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        assert!(
            max_amplitude > 0.0 && max_amplitude < 2.0,
            "Signal amplitude should be reasonable"
        );
    }

    #[test]
    fn test_generate_carrier() {
        let signal = generate_carrier(1000.0, 12000, 1.0);

        assert_eq!(signal.len(), 12000);

        let max = signal.iter().fold(f32::MIN, |a, &b| a.max(b));
        let min = signal.iter().fold(f32::MAX, |a, &b| a.min(b));

        assert!((max - 0.1).abs() < 0.001, "Max should be ~0.1");
        assert!((min + 0.1).abs() < 0.001, "Min should be ~-0.1");
    }

    #[test]
    fn test_generate_silence() {
        let signal = generate_silence(12000, 1.0);

        assert_eq!(signal.len(), 12000);
        assert!(
            signal.iter().all(|&x| x == 0.0),
            "All samples should be zero"
        );
    }

    #[test]
    fn test_snr_levels() {
        for snr in [-10.0, 0.0, 10.0, 20.0] {
            let params = Ft8SignalParams {
                snr_db: snr,
                ..Default::default()
            };

            let signal = generate_ft8_test_signal(&params);
            assert_eq!(signal.len(), 151680);

            let variance: f32 = signal.iter().map(|&x| x * x).sum::<f32>() / signal.len() as f32;

            assert!(
                variance > 0.0,
                "Signal should have non-zero variance at SNR {}",
                snr
            );
        }
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_real_ft8_signal_generation() {
        let signal = generate_real_ft8_signal("CQ W1ABC FN42", 0.0, 0.0);

        assert_eq!(signal.len(), pancetta_ft8::WINDOW_SAMPLES);

        let rms = (signal.iter().map(|&x| x * x).sum::<f32>() / signal.len() as f32).sqrt();
        assert!(rms > 0.0, "Real FT8 signal should have non-zero RMS");
    }
}
