//! Example demonstrating enhanced spectral analysis for FT8 weak signal detection
//!
//! This example shows how to use the enhanced spectral analysis features:
//! - Multi-resolution FFT analysis
//! - Statistical noise floor estimation
//! - Coherent symbol averaging
//! - Doppler shift compensation
//! - Waterfall display generation
//! - Automatic gain control

use pancetta_ft8::{Ft8Config, Ft8Decoder};
use std::f64::consts::PI;

const SAMPLE_RATE: u32 = 12000;
const WINDOW_SAMPLES: usize = 151680; // Exactly 12.64 seconds at 12 kHz

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Enhanced FT8 Spectral Analysis Demo");
    println!("====================================\n");

    // Configure decoder for weak signal detection. Start from the
    // default config and override only the fields this demo wants to
    // tweak — this is forward-compatible with new Ft8Config fields.
    let config = Ft8Config {
        min_snr_db: -24.0,
        ldpc_iterations: 150,
        frequency_range: 300.0,
        time_range: 3.0,
        osd_depth: Some(1),
        ..Ft8Config::default()
    };

    // Create decoder
    let mut decoder = Ft8Decoder::new(config)?;

    // Generate simulated weak FT8 signal with noise
    let audio = generate_weak_ft8_signal();

    println!(
        "Processing {} samples ({:.2} seconds)",
        audio.len(),
        audio.len() as f64 / SAMPLE_RATE as f64
    );

    // Process the signal
    let start = std::time::Instant::now();
    let decoded_messages = decoder.decode_window(&audio)?;
    let elapsed = start.elapsed();

    println!("\nDecoding Results:");
    println!("-----------------");
    println!("Processing time: {:.3} seconds", elapsed.as_secs_f64());
    println!("Messages decoded: {}", decoded_messages.len());

    // Generate waterfall display data
    println!("\nGenerating waterfall display data...");
    let waterfall =
        decoder.generate_waterfall_data(&audio.iter().map(|&x| x as f64).collect::<Vec<_>>())?;

    println!("Waterfall data:");
    println!("  Time bins: {}", waterfall.time_bins.len());
    println!("  Frequency bins: {}", waterfall.frequency_bins.len());
    println!(
        "  Power range: {:.1} to {:.1} dB",
        waterfall.min_power, waterfall.max_power
    );

    // Display frequency spectrum summary
    if !waterfall.power_matrix.is_empty() {
        println!("\nSpectrum Analysis:");
        println!("------------------");

        // Find peak frequencies in the waterfall
        let mut peak_freqs = Vec::new();
        for (i, freq) in waterfall.frequency_bins.iter().enumerate() {
            let mut total_power = 0.0;
            let mut count = 0;

            for time_slice in &waterfall.power_matrix {
                if i < time_slice.len() {
                    total_power += time_slice[i];
                    count += 1;
                }
            }

            if count > 0 {
                let avg_power = total_power / count as f64;
                if avg_power > waterfall.min_power + 10.0 {
                    // 10 dB above noise floor
                    peak_freqs.push((*freq, avg_power));
                }
            }
        }

        // Sort by power and display top frequencies
        peak_freqs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        println!("Top detected frequencies:");
        for (i, (freq, power)) in peak_freqs.iter().take(10).enumerate() {
            println!("  {}. {:.1} Hz at {:.1} dB", i + 1, freq, power);
        }
    }

    // Display metrics
    let metrics = decoder.get_last_metrics();
    println!("\nPerformance Metrics:");
    println!("--------------------");
    println!("Average SNR: {:.1} dB", metrics.average_snr);
    println!("Sync quality: {:.2}", metrics.sync_quality);
    println!("Peak memory: {} KB", metrics.peak_memory_bytes / 1024);

    // Demonstrate weak signal capabilities
    println!("\nWeak Signal Detection Capabilities:");
    println!("-----------------------------------");
    println!("✓ Multi-resolution FFT (8192 coarse, 2048 fine)");
    println!("✓ Statistical noise floor (MAD-based estimation)");
    println!("✓ Coherent symbol averaging (3+ observations)");
    println!("✓ Doppler compensation (±200 Hz search)");
    println!("✓ AGC for dynamic range handling");
    println!("✓ Waterfall display generation");

    Ok(())
}

/// Generate a simulated weak FT8 signal for testing
fn generate_weak_ft8_signal() -> Vec<f32> {
    let mut audio = vec![0.0f32; WINDOW_SAMPLES];

    // Add very weak FT8-like signal (-20 dB SNR)
    let signal_amplitude = 0.01; // Very weak signal
    let base_freq = 1500.0;
    let tone_spacing = 6.25;
    let symbol_duration = 0.16;
    let samples_per_symbol = (SAMPLE_RATE as f64 * symbol_duration) as usize;

    // Generate 79 symbols with pseudo-random tones
    for symbol_idx in 0..79 {
        let tone = ((symbol_idx * 7 + 3) % 8) as f64; // Pseudo-random tone sequence
        let freq = base_freq + tone * tone_spacing;

        let start_sample = symbol_idx * samples_per_symbol;
        let end_sample = (start_sample + samples_per_symbol).min(audio.len());

        for i in start_sample..end_sample {
            let t = i as f64 / SAMPLE_RATE as f64;
            audio[i] += (2.0 * PI * freq * t).sin() as f32 * signal_amplitude;
        }
    }

    // Add strong noise to simulate weak signal conditions
    let noise_amplitude = 0.1; // 20 dB stronger than signal
    for (i, sample) in audio.iter_mut().enumerate() {
        // Generate pseudo-random noise
        let noise1 = ((i as f64 * 0.12345).sin() * 43758.5453).fract() - 0.5;
        let noise2 = ((i as f64 * 0.98765).cos() * 22341.1234).fract() - 0.5;
        *sample += (noise1 + noise2) as f32 * noise_amplitude;
    }

    // Add a simulated Doppler shift (for EME/satellite simulation)
    let doppler_shift = 50.0; // 50 Hz Doppler shift
    for i in 0..audio.len() {
        let t = i as f64 / SAMPLE_RATE as f64;
        let phase_shift = 2.0 * PI * doppler_shift * t;
        let original = audio[i];
        audio[i] = original * phase_shift.cos() as f32;
    }

    // Apply fading to simulate ionospheric effects
    for i in 0..audio.len() {
        let t = i as f64 / SAMPLE_RATE as f64;
        let fade = (0.5 + 0.5 * (0.1 * t).sin()) as f32; // Slow QSB
        audio[i] *= fade;
    }

    audio
}
