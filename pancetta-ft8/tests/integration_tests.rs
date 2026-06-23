//! Integration tests for the FT8 decoder
//!
//! These tests validate the complete FT8 decoding pipeline using real
//! encoder → modulator → decoder round-trip signals.

// rationale: plain-data config structs built field-by-field in test/bench
// setup; sequential assignment reads clearer than a struct-update splat.
#![allow(clippy::field_reassign_with_default)]
#![cfg(feature = "transmit")]

use pancetta_ft8::{
    DecodedMessage, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, MESSAGE_DURATION, NUM_SYMBOLS,
    SAMPLE_RATE, SYMBOL_DURATION, WINDOW_SAMPLES,
};
use std::time::Duration;

/// Generate a real FT8 signal using the encoder + modulator pipeline,
/// then add Gaussian noise at the given SNR.
fn generate_ft8_test_signal(message: &str, snr_db: f32, frequency_offset: f64) -> Vec<f32> {
    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default().unwrap();

    let symbols = encoder
        .encode_message(message, None)
        .unwrap_or_else(|e| panic!("Failed to encode '{}': {}", message, e));

    let mut audio = modulator
        .modulate_symbols(&symbols, frequency_offset)
        .unwrap_or_else(|e| panic!("Failed to modulate '{}': {}", message, e));

    // Pad/trim to WINDOW_SAMPLES
    audio.resize(WINDOW_SAMPLES, 0.0);

    // Add Gaussian noise at calibrated SNR
    if snr_db < 100.0 {
        let signal_power: f32 = audio.iter().map(|&s| s * s).sum::<f32>() / audio.len() as f32;
        let noise_power = signal_power / 10.0_f32.powf(snr_db / 10.0);
        let noise_std = noise_power.sqrt();

        // Box-Muller transform for Gaussian noise (deterministic seed via index)
        for i in (0..audio.len()).step_by(2) {
            let u1 = (((i * 1103515245 + 12345) % (1 << 31)) as f32) / (1u32 << 31) as f32;
            let u2 = ((((i + 1) * 1103515245 + 12345) % (1 << 31)) as f32) / (1u32 << 31) as f32;
            let u1 = u1.max(1e-10);
            let r = (-2.0 * u1.ln()).sqrt();
            let theta = 2.0 * std::f32::consts::PI * u2;
            audio[i] += r * theta.cos() * noise_std;
            if i + 1 < audio.len() {
                audio[i + 1] += r * theta.sin() * noise_std;
            }
        }
    }

    audio
}

// =============================================================
// Decoder tests with real FT8 signals
// =============================================================

#[test]
fn test_decoder_with_strong_signal() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    let test_samples = generate_ft8_test_signal("CQ W1ABC FN42", 20.0, 0.0);

    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());

    let decoded = result.unwrap();
    let metrics = decoder.get_last_metrics();

    assert!(metrics.processing_time < Duration::from_secs(10));
    assert!(
        decoded.iter().any(|m| m.text == "CQ W1ABC FN42"),
        "Should decode strong signal: got {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_decoder_with_weak_signal() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    // Weak signal — may or may not decode, but should not crash
    let test_samples = generate_ft8_test_signal("K1DEF W1ABC -15", 0.0, 12.5);

    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());

    let metrics = decoder.get_last_metrics();
    assert!(metrics.processing_time < Duration::from_secs(15));
}

#[test]
fn test_decoder_with_multiple_signals() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    // Combine two signals at different frequencies
    let signal1 = generate_ft8_test_signal("CQ W1ABC FN42", 20.0, -50.0);
    let signal2 = generate_ft8_test_signal("K1DEF W1ABC -12", 20.0, 50.0);

    let mut combined = vec![0.0f32; WINDOW_SAMPLES];
    for i in 0..WINDOW_SAMPLES {
        combined[i] = signal1[i] + signal2[i];
    }

    let result = decoder.decode_window(&combined);
    assert!(result.is_ok());

    let decoded = result.unwrap();
    // Should decode at least one signal
    assert!(
        !decoded.is_empty(),
        "Should decode at least one of the combined signals"
    );
}

#[test]
fn test_decoder_with_frequency_offset() {
    let offsets = [0.0, 50.0, -50.0, 100.0];

    for offset in offsets {
        let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let test_samples = generate_ft8_test_signal("CQ DX W1ABC FN42", 20.0, offset);

        let result = decoder.decode_window(&test_samples);
        assert!(result.is_ok(), "Should not error at offset {}", offset);

        let decoded = result.unwrap();
        assert!(
            decoded.iter().any(|m| m.text == "CQ DX W1ABC FN42"),
            "Should decode at offset {}: got {:?}",
            offset,
            decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }
}

#[test]
fn test_decoder_performance_requirements() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    // Generate signal with moderate noise
    let test_samples = generate_ft8_test_signal("CQ W1ABC FN42", 10.0, 0.0);

    let start = std::time::Instant::now();
    let result = decoder.decode_window(&test_samples);
    let elapsed = start.elapsed();

    assert!(result.is_ok());

    let metrics = decoder.get_last_metrics();

    assert!(
        elapsed < Duration::from_secs(30),
        "Decoding took {:?}, should be under 30 seconds",
        elapsed
    );

    assert!(
        metrics.peak_memory_bytes < 10 * 1024 * 1024,
        "Peak memory {} bytes exceeds 10MB limit",
        metrics.peak_memory_bytes
    );
}

#[test]
fn test_decoder_noise_only() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    // Generate pure noise (no signal)
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];
    for (i, sample) in samples.iter_mut().enumerate() {
        // Simple PRNG-based noise
        let u = (((i * 1103515245 + 12345) % (1 << 31)) as f32) / (1u32 << 31) as f32;
        *sample = (u - 0.5) * 0.1;
    }

    let result = decoder.decode_window(&samples);
    assert!(result.is_ok());

    let decoded = result.unwrap();
    // FT8 decoders may occasionally produce false positives from noise
    // (the CRC-14 has a 1/16384 false positive rate per candidate).
    // Allow at most 1 false positive.
    assert!(
        decoded.len() <= 1,
        "Should decode at most 1 false positive from noise, got {}",
        decoded.len()
    );
}

#[test]
fn test_decoder_configuration_variants() {
    // Test with high-sensitivity configuration (more candidates)
    let mut high_sensitivity_config = Ft8Config::default();
    high_sensitivity_config.max_candidates = 100;

    let mut decoder = Ft8Decoder::new(high_sensitivity_config).unwrap();
    let test_samples = generate_ft8_test_signal("CQ W1ABC FN42", 20.0, 0.0);

    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());

    // Test with minimal configuration
    let mut minimal_config = Ft8Config::default();
    minimal_config.max_candidates = 10;

    let mut decoder = Ft8Decoder::new(minimal_config).unwrap();
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());
}

#[test]
fn test_synchronization_quality() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();

    let aligned_samples = generate_ft8_test_signal("CQ W1ABC FN42", 20.0, 0.0);

    let result = decoder.decode_window(&aligned_samples);
    assert!(result.is_ok());

    let decoded = result.unwrap();
    let metrics = decoder.get_last_metrics();
    eprintln!(
        "sync_quality={}, decoded={}",
        metrics.sync_quality,
        decoded.len()
    );
    // The signal should decode successfully; sync_quality depends on
    // spectrogram search range and may vary with padding.
    assert!(
        !decoded.is_empty() || metrics.sync_quality > 0.3,
        "Clean signal should decode or have reasonable sync quality (got {})",
        metrics.sync_quality
    );
}

/// Integration test for message handler callbacks
#[test]
fn test_message_handler_callbacks() {
    use pancetta_ft8::{DecodingMetrics, MessageHandler};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestMessageHandler {
        messages: Arc<Mutex<Vec<DecodedMessage>>>,
        window_starts: Arc<Mutex<usize>>,
        window_completes: Arc<Mutex<usize>>,
    }

    impl MessageHandler for TestMessageHandler {
        fn on_message_decoded(&mut self, message: &DecodedMessage, _metrics: &DecodingMetrics) {
            self.messages.lock().unwrap().push(message.clone());
        }

        fn on_window_start(&mut self, _timestamp: std::time::SystemTime) {
            *self.window_starts.lock().unwrap() += 1;
        }

        fn on_window_complete(&mut self, _metrics: &DecodingMetrics) {
            *self.window_completes.lock().unwrap() += 1;
        }
    }

    let handler = TestMessageHandler::default();
    let messages = handler.messages.clone();
    let starts = handler.window_starts.clone();
    let completes = handler.window_completes.clone();

    let mut decoder =
        Ft8Decoder::with_message_handler(Ft8Config::default(), Box::new(handler)).unwrap();

    let test_samples = generate_ft8_test_signal("CQ W1ABC FN42", 20.0, 0.0);
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());

    assert_eq!(*starts.lock().unwrap(), 1);
    assert_eq!(*completes.lock().unwrap(), 1);

    // With a strong clean signal, the handler should receive the decoded message
    assert!(
        !messages.lock().unwrap().is_empty(),
        "Handler should receive decoded message"
    );
}

#[test]
fn test_error_conditions() {
    // Test with invalid sample rate
    let mut config = Ft8Config::default();
    config.sample_rate = 48000;

    let result = Ft8Decoder::new(config);
    assert!(result.is_err());

    // Test with wrong window size
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();

    let wrong_size_samples = vec![0.0f32; 1000];
    let result = decoder.decode_window(&wrong_size_samples);
    assert!(result.is_err());
}

/// Validate that constants are correct
#[test]
fn test_ft8_constants() {
    assert_eq!(SAMPLE_RATE, 12_000);
    assert_eq!(SYMBOL_DURATION, 0.16);
    assert_eq!(MESSAGE_DURATION, 12.64);
    assert_eq!(NUM_SYMBOLS, 79);
    assert_eq!(WINDOW_SAMPLES, 151_680);

    // Verify window size calculation
    let calculated_samples = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;
    assert_eq!(WINDOW_SAMPLES, calculated_samples);
}
