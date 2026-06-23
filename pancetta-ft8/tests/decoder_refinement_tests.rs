//! Tests for decoder frequency/time refinement improvements

// rationale: plain-data config structs built field-by-field in test/bench
// setup; sequential assignment reads clearer than a struct-update splat.
#![allow(clippy::field_reassign_with_default)]

use pancetta_ft8::{Ft8Config, Ft8Decoder, SAMPLE_RATE, WINDOW_SAMPLES};

#[cfg(feature = "transmit")]
mod refinement {
    use super::*;

    /// Helper: generate a known FT8 signal at a specific frequency offset
    /// and verify the decoder finds it. Returns (decoded_count, frequency_error_hz).
    fn decode_at_offset(freq_offset: f64) -> (usize, f64) {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let symbols = encoder.encode_message("CQ W1ABC FN42", None).unwrap();

        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();
        let signal = modulator.modulate_symbols(&symbols, freq_offset).unwrap();

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal.iter().enumerate() {
            if i < samples.len() {
                samples[i] = s;
            }
        }

        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();

        let count = decoded.len();
        let freq_error = if let Some(msg) = decoded.first() {
            (msg.frequency_offset - (pancetta_ft8::BASE_FREQUENCY + freq_offset)).abs()
        } else {
            f64::MAX
        };

        (count, freq_error)
    }

    #[test]
    fn test_decode_at_exact_bin_center() {
        let (count, _) = decode_at_offset(0.0);
        assert!(count >= 1, "Should decode signal at bin center");
    }

    #[test]
    fn test_decode_at_quarter_bin_offset() {
        let (count, _) = decode_at_offset(1.5625);
        assert!(count >= 1, "Should decode signal at quarter-bin offset");
    }

    #[test]
    fn test_decode_at_half_bin_offset() {
        let (count, _) = decode_at_offset(3.125);
        assert!(count >= 1, "Should decode signal at half-bin offset");
    }

    #[test]
    fn test_frequency_estimate_accuracy() {
        let (count, freq_error) = decode_at_offset(1.0);
        assert!(count >= 1, "Should decode signal");
        assert!(
            freq_error < 2.0,
            "Frequency error {:.2} Hz should be < 2 Hz",
            freq_error
        );
    }

    #[test]
    fn test_decode_with_time_offset() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let symbols = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();
        let signal = modulator.modulate_symbols(&symbols, 0.0).unwrap();

        // Place signal with a 100-sample offset (8.3ms) from the start
        // This tests that the time refinement can find signals not aligned to symbol boundaries
        let offset = 100;
        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal.iter().enumerate() {
            if i + offset < samples.len() {
                samples[i + offset] = s;
            }
        }

        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();
        assert!(
            !decoded.is_empty(),
            "Should decode signal with 100-sample time offset"
        );
    }

    #[test]
    fn test_multipass_decodes_overlapping_signals() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        // Create two signals at different frequencies
        let mut encoder = Ft8Encoder::new();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();

        // Signal 1: strong, at 0 Hz offset
        let symbols1 = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
        let signal1 = modulator.modulate_symbols(&symbols1, 0.0).unwrap();

        // Signal 2: weaker, at +100 Hz offset
        let symbols2 = encoder.encode_message("CQ K2DEF EM73", None).unwrap();
        let signal2 = modulator.modulate_symbols(&symbols2, 100.0).unwrap();

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal1.iter().enumerate() {
            if i < samples.len() {
                samples[i] += s;
            }
        }
        // Add signal 2 at half amplitude (6 dB weaker)
        for (i, &s) in signal2.iter().enumerate() {
            if i < samples.len() {
                samples[i] += s * 0.5;
            }
        }

        // With multi-pass (default 3), should decode both
        let mut config = Ft8Config::default();
        config.max_decode_passes = 3;
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();

        let messages: Vec<&str> = decoded.iter().map(|m| m.text.as_str()).collect();
        assert!(
            decoded.len() >= 2,
            "Multi-pass should decode both signals, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_single_pass_vs_multipass() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();

        // Three signals at different frequencies
        let msgs = ["CQ W1ABC FN42", "CQ K2DEF EM73", "CQ N3GHI DM65"];
        let offsets = [0.0, 75.0, 150.0];
        let amplitudes = [1.0f32, 0.5, 0.25];

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (idx, msg) in msgs.iter().enumerate() {
            let symbols = encoder.encode_message(msg, None).unwrap();
            let signal = modulator.modulate_symbols(&symbols, offsets[idx]).unwrap();
            for (i, &s) in signal.iter().enumerate() {
                if i < samples.len() {
                    samples[i] += s * amplitudes[idx];
                }
            }
        }

        // Single pass
        let mut config1 = Ft8Config::default();
        config1.max_decode_passes = 1;
        let mut decoder1 = Ft8Decoder::new(config1).unwrap();
        let decoded1 = decoder1.decode_window(&samples.clone()).unwrap();

        // Multi-pass
        let mut config3 = Ft8Config::default();
        config3.max_decode_passes = 3;
        let mut decoder3 = Ft8Decoder::new(config3).unwrap();
        let decoded3 = decoder3.decode_window(&samples).unwrap();

        println!("Single pass: {} decodes", decoded1.len());
        println!("Multi-pass:  {} decodes", decoded3.len());
        assert!(
            decoded3.len() >= decoded1.len(),
            "Multi-pass ({}) should decode at least as many as single-pass ({})",
            decoded3.len(),
            decoded1.len()
        );
    }
}
