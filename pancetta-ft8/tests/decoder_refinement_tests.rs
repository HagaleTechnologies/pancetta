//! Tests for decoder frequency/time refinement improvements

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
}
