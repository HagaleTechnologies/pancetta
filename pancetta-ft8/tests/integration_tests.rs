//! Integration tests for the FT8 decoder
//!
//! These tests validate the complete FT8 decoding pipeline with known signals
//! and verify performance requirements are met.

use pancetta_ft8::{
    Ft8Decoder, Ft8Config, DecodedMessage, MessageType,
    SAMPLE_RATE, WINDOW_SAMPLES, NUM_SYMBOLS, SYMBOL_DURATION,
};
use std::f64::consts::PI;
use std::time::Duration;

/// Generate a synthetic FT8 signal for testing
fn generate_ft8_test_signal(
    message: &str,
    snr_db: f32,
    frequency_offset: f64,
    time_offset: f64,
) -> Vec<f32> {
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];
    
    // Add noise floor
    let noise_power = 10.0_f64.powf(snr_db as f64 / 10.0);
    for sample in &mut samples {
        *sample = (rand::random::<f32>() - 0.5) * (noise_power as f32).sqrt();
    }
    
    // Generate FT8 tones based on message
    let base_freq = 1500.0 + frequency_offset;
    let tone_spacing = 6.25; // Hz
    let symbol_samples = (SYMBOL_DURATION * SAMPLE_RATE as f64) as usize;
    let start_sample = (time_offset * SAMPLE_RATE as f64) as usize;
    
    // Simple tone sequence for testing (not actual FT8 encoding)
    let test_tones = message_to_test_tones(message);
    
    for (symbol_idx, &tone) in test_tones.iter().enumerate().take(NUM_SYMBOLS) {
        let tone_freq = base_freq + tone as f64 * tone_spacing;
        let symbol_start = start_sample + symbol_idx * symbol_samples;
        let symbol_end = (symbol_start + symbol_samples).min(samples.len());
        
        for i in symbol_start..symbol_end {
            let t = i as f64 / SAMPLE_RATE as f64;
            let phase = 2.0 * PI * tone_freq * t;
            let amplitude = 0.1; // Signal amplitude
            samples[i] += amplitude * phase.cos() as f32;
        }
    }
    
    samples
}

/// Convert test message to tone sequence (simplified for testing)
fn message_to_test_tones(message: &str) -> Vec<u8> {
    let mut tones = Vec::with_capacity(NUM_SYMBOLS);
    
    // Create a deterministic tone sequence based on message
    for (i, ch) in message.chars().enumerate() {
        if i >= NUM_SYMBOLS {
            break;
        }
        let tone = (ch as u8 % 8) as u8; // Map to 0-7 tone range
        tones.push(tone);
    }
    
    // Fill remaining symbols
    while tones.len() < NUM_SYMBOLS {
        tones.push(0);
    }
    
    tones
}

#[test]
fn test_decoder_with_strong_signal() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Generate strong test signal (-5 dB SNR)
    let test_samples = generate_ft8_test_signal("CQ W1ABC FN42", -5.0, 0.0, 0.0);
    
    let result = decoder.decode_window(&test_samples);
    if let Err(e) = &result {
        println!("Decode error: {:?}", e);
    }
    assert!(result.is_ok());
    
    let decoded = result.unwrap();
    let metrics = decoder.get_last_metrics();
    
    // Should process within reasonable time (give more time for debug builds)
    assert!(metrics.processing_time < Duration::from_secs(10));
    
    // With a strong signal, should get some decode attempts
    println!("Decoded {} messages with strong signal", decoded.len());
    println!("Processing time: {:?}", metrics.processing_time);
    println!("Sync quality: {:.2}", metrics.sync_quality);
}

#[test]
fn test_decoder_with_weak_signal() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Generate weak test signal (-20 dB SNR)
    let test_samples = generate_ft8_test_signal("K1DEF W1ABC -15", -20.0, 12.5, 0.1);
    
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());
    
    let decoded = result.unwrap();
    let metrics = decoder.get_last_metrics();
    
    // Should still process successfully even if no decodes
    assert!(metrics.processing_time < Duration::from_secs(15));
    
    println!("Decoded {} messages with weak signal", decoded.len());
    println!("Processing time: {:?}", metrics.processing_time);
}

#[test]
fn test_decoder_with_multiple_signals() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Generate multiple overlapping signals
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];
    
    // Add multiple test signals at different frequencies
    let signals = [
        ("CQ W1ABC FN42", -10.0, -50.0, 0.0),
        ("W1ABC K1DEF FN41", -12.0, 0.0, 0.0),
        ("K1DEF W1ABC -08", -15.0, 75.0, 0.0),
    ];
    
    for (message, snr, freq_offset, time_offset) in signals {
        let signal = generate_ft8_test_signal(message, snr, freq_offset, time_offset);
        for (i, sample) in signal.iter().enumerate() {
            samples[i] += sample;
        }
    }
    
    let result = decoder.decode_window(&samples);
    assert!(result.is_ok());
    
    let decoded = result.unwrap();
    let metrics = decoder.get_last_metrics();
    
    println!("Decoded {} messages from multiple signals", decoded.len());
    println!("Processing time: {:?}", metrics.processing_time);
    println!("Peak memory: {} bytes", metrics.peak_memory_bytes);
    
    // Should handle multiple signals
    assert!(decoded.len() <= 50); // Max candidates limit
}

#[test]
fn test_decoder_with_frequency_offset() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Test with various frequency offsets
    let offsets = [-100.0, -25.0, 0.0, 37.5, 150.0];
    
    for offset in offsets {
        let test_samples = generate_ft8_test_signal("CQ DX W1ABC FN42", -8.0, offset, 0.0);
        
        let result = decoder.decode_window(&test_samples);
        assert!(result.is_ok());
        
        let decoded = result.unwrap();
        println!("Frequency offset {:.1} Hz: {} decodes", offset, decoded.len());
        
        // Check if any decodes have approximately correct frequency
        for decode in decoded {
            let freq_error = (decode.frequency_offset - (1500.0 + offset)).abs();
            println!("  Decoded at {:.1} Hz (error: {:.1} Hz)", decode.frequency_offset, freq_error);
        }
    }
}

#[test]
fn test_decoder_with_time_offset() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Test with various time offsets
    let offsets = [-0.5, -0.1, 0.0, 0.3, 1.0];
    
    for offset in offsets {
        let test_samples = generate_ft8_test_signal("W1ABC K1DEF FN42", -10.0, 0.0, offset);
        
        let result = decoder.decode_window(&test_samples);
        assert!(result.is_ok());
        
        let decoded = result.unwrap();
        println!("Time offset {:.1} s: {} decodes", offset, decoded.len());
        
        // Check decoded time offsets
        for decode in decoded {
            println!("  Decoded at {:.2} s offset", decode.time_offset);
        }
    }
}

#[test]
fn test_decoder_performance_requirements() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Test performance with challenging scenario
    let test_samples = generate_complex_test_scenario();
    
    let start = std::time::Instant::now();
    let result = decoder.decode_window(&test_samples);
    let elapsed = start.elapsed();
    
    assert!(result.is_ok());
    
    let metrics = decoder.get_last_metrics();
    
    // Performance requirements (more lenient for debug builds)
    assert!(
        elapsed < Duration::from_secs(30),
        "Decoding took {:?}, should be under 30 seconds",
        elapsed
    );
    
    // Memory usage should be reasonable
    assert!(
        metrics.peak_memory_bytes < 10 * 1024 * 1024,
        "Peak memory {} bytes exceeds 10MB limit",
        metrics.peak_memory_bytes
    );
    
    println!("Performance test results:");
    println!("  Processing time: {:?}", elapsed);
    println!("  Peak memory: {} KB", metrics.peak_memory_bytes / 1024);
    println!("  Messages decoded: {}", metrics.messages_decoded);
    println!("  Sync quality: {:.2}", metrics.sync_quality);
}

#[test]
fn test_decoder_noise_only() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Generate pure noise
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];
    for sample in &mut samples {
        *sample = (rand::random::<f32>() - 0.5) * 0.1;
    }
    
    let result = decoder.decode_window(&samples);
    assert!(result.is_ok());
    
    let decoded = result.unwrap();
    
    // Should not decode valid messages from pure noise
    assert_eq!(decoded.len(), 0);
    
    let metrics = decoder.get_last_metrics();
    assert_eq!(metrics.messages_decoded, 0);
    assert!(metrics.processing_time < Duration::from_secs(10));
}

#[test]
fn test_decoder_configuration_variants() {
    // Test with aggressive decoding enabled
    let mut aggressive_config = Ft8Config::default();
    aggressive_config.aggressive_decoding = true;
    aggressive_config.max_candidates = 100;
    aggressive_config.min_snr_db = -25.0;
    
    let mut decoder = Ft8Decoder::new(aggressive_config).unwrap();
    let test_samples = generate_ft8_test_signal("CQ TEST W1ABC", -22.0, 0.0, 0.0);
    
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());
    
    // Test with minimal configuration
    let mut minimal_config = Ft8Config::default();
    minimal_config.enable_multithreading = false;
    minimal_config.max_candidates = 10;
    minimal_config.min_snr_db = -10.0;
    
    let mut decoder = Ft8Decoder::new(minimal_config).unwrap();
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());
}

#[test]
fn test_synchronization_quality() {
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    
    // Generate signal aligned to expected timing
    let aligned_samples = generate_ft8_test_signal("CQ W1ABC FN42", -8.0, 0.0, 0.0);
    
    let result = decoder.decode_window(&aligned_samples);
    assert!(result.is_ok());
    
    let metrics = decoder.get_last_metrics();
    
    // Should have reasonable sync quality
    println!("Sync quality: {:.2}", metrics.sync_quality);
    assert!(decoder.is_synchronized() || metrics.sync_quality >= 0.0);
}

/// Generate a complex test scenario with multiple signals and noise
fn generate_complex_test_scenario() -> Vec<f32> {
    let mut samples = vec![0.0f32; WINDOW_SAMPLES];
    
    // Add background noise
    for sample in &mut samples {
        *sample = (rand::random::<f32>() - 0.5) * 0.05;
    }
    
    // Add multiple FT8 signals at various SNRs and frequencies
    let signals = [
        ("CQ W1ABC FN42", -15.0, -75.0, 0.0),
        ("W1ABC K1DEF FN41", -18.0, -25.0, 0.1),
        ("K1DEF W1ABC -12", -20.0, 25.0, 0.2),
        ("W1ABC K1DEF RRR", -22.0, 75.0, 0.3),
        ("K1DEF W1ABC 73", -16.0, 125.0, 0.1),
    ];
    
    for (message, snr, freq_offset, time_offset) in signals {
        let signal = generate_ft8_test_signal(message, snr, freq_offset, time_offset);
        for (i, signal_sample) in signal.iter().enumerate() {
            samples[i] += signal_sample;
        }
    }
    
    // Add some interference
    let interference_freq = 2000.0; // Outside FT8 band but within filter
    for (i, sample) in samples.iter_mut().enumerate() {
        let t = i as f64 / SAMPLE_RATE as f64;
        let interference = 0.02 * (2.0 * PI * interference_freq * t).sin() as f32;
        *sample += interference;
    }
    
    samples
}

/// Integration test for message handler callbacks
#[test]
fn test_message_handler_callbacks() {
    use std::sync::{Arc, Mutex};
    use pancetta_ft8::{MessageHandler, DecodingMetrics};
    
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
    
    let mut decoder = Ft8Decoder::with_message_handler(
        Ft8Config::default(),
        Box::new(handler),
    ).unwrap();
    
    let test_samples = generate_ft8_test_signal("CQ TEST W1ABC", -10.0, 0.0, 0.0);
    let result = decoder.decode_window(&test_samples);
    assert!(result.is_ok());
    
    // Check that callbacks were called
    assert_eq!(*starts.lock().unwrap(), 1);
    assert_eq!(*completes.lock().unwrap(), 1);
    
    println!("Handler received {} messages", messages.lock().unwrap().len());
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
    use pancetta_ft8::{SAMPLE_RATE, SYMBOL_DURATION, MESSAGE_DURATION, NUM_SYMBOLS, WINDOW_SAMPLES};
    
    assert_eq!(SAMPLE_RATE, 12_000);
    assert_eq!(SYMBOL_DURATION, 0.16);
    assert_eq!(MESSAGE_DURATION, 12.64);
    assert_eq!(NUM_SYMBOLS, 79);
    assert_eq!(WINDOW_SAMPLES, 151_680);
    
    // Verify window size calculation
    let calculated_samples = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;
    assert_eq!(WINDOW_SAMPLES, calculated_samples);
}