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
        decode_at_offset_with_config(freq_offset, Ft8Config::default())
    }

    /// Same as `decode_at_offset` but with a caller-supplied `Ft8Config`,
    /// so tests can exercise non-default flags (e.g.
    /// `fine_fft_rect_window`, Task W1.3) end-to-end through
    /// `decode_window`.
    fn decode_at_offset_with_config(freq_offset: f64, config: Ft8Config) -> (usize, f64) {
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

    /// Task W1.3 (decoder-TP-sensitivity plan): `Ft8Config::default()`
    /// must keep `fine_fft_rect_window` off until the hard-200 A/B gate
    /// passes — a regression guard against an accidental default flip.
    #[test]
    fn test_fine_fft_rect_window_default_is_off() {
        assert!(
            !Ft8Config::default().fine_fft_rect_window,
            "fine_fft_rect_window must default to false pending the A/B gate"
        );
    }

    /// Task W1.3: sanity end-to-end check that enabling the rectangular
    /// fine-FFT window doesn't break ordinary decoding of a clean,
    /// bin-center signal (the flag only changes the Hann-vs-rect window
    /// used on the `sync_score >= 3.5` fine-FFT fallback path; a strong
    /// clean signal typically decodes on the coarse spectrogram path
    /// before that fallback is even reached, so this is a coarse
    /// regression guard, not a sensitivity claim — the sensitivity claim
    /// is validated by the hard-200 A/B gate, see
    /// `research/experiments/`).
    #[test]
    fn test_fine_fft_rect_window_flag_does_not_break_clean_decode() {
        let mut config = Ft8Config::default();
        config.fine_fft_rect_window = true;
        let (count, _) = decode_at_offset_with_config(0.0, config);
        assert!(
            count >= 1,
            "Should still decode a clean bin-center signal with fine_fft_rect_window enabled"
        );
    }
}

// ============================================================================
// Task W1.4 (decoder-TP-sensitivity plan, spec Section 7): `whiten_llrs`
// dB-vs-linear-|y| gain-invariance property test.
//
// Unconditional (not gated behind `transmit`, unlike `mod refinement` above)
// because it only needs a real off-air fixture WAV + `decode_window`, the
// same minimal surface `wav_decode_tests.rs` uses without any feature gate.
// ============================================================================

mod gain_invariance {
    use super::*;
    use std::collections::BTreeSet;

    fn fixture_path(subpath: &str) -> String {
        format!(
            "{}/tests/fixtures/wav/{}",
            env!("CARGO_MANIFEST_DIR"),
            subpath
        )
    }

    fn read_wav_samples(path: &str) -> Vec<f32> {
        let reader = hound::WavReader::open(path)
            .unwrap_or_else(|e| panic!("Failed to open {}: {}", path, e));
        let spec = reader.spec();
        assert_eq!(spec.channels, 1, "fixture must be mono");
        assert_eq!(
            spec.sample_rate, SAMPLE_RATE,
            "fixture must match SAMPLE_RATE"
        );
        reader
            .into_samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect()
    }

    /// Decode `samples` after uniformly scaling every sample by `gain`,
    /// returning the set of decoded message texts. A `BTreeSet` (rather
    /// than the raw `Vec`) deliberately ignores decode ORDER — only the
    /// set of messages recovered matters for a gain-invariance claim.
    ///
    /// `llr_whitening_enabled` is forced ON here (it defaults to OFF as
    /// of Task W1.4 — see the docstring below): this test exists
    /// specifically to guard `whiten_llrs`/`maybe_whiten_llrs` against a
    /// future gain-dependence regression, so it must exercise that code
    /// path regardless of the crate's current default.
    fn decode_set_at_gain(samples: &[f32], gain: f32) -> BTreeSet<String> {
        let mut buffer: Vec<f32> = samples.iter().map(|&s| s * gain).collect();
        if buffer.len() < WINDOW_SAMPLES {
            buffer.resize(WINDOW_SAMPLES, 0.0);
        }
        let mut config = Ft8Config::default();
        config.llr_whitening_enabled = true;
        let mut decoder = Ft8Decoder::new(config).unwrap();
        decoder
            .decode_window(&buffer)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    /// `whiten_llrs` (`Ft8Config::llr_whitening_enabled`, OFF by default
    /// as of Task W1.4 — explicitly forced ON in `decode_set_at_gain`
    /// above so this test still exercises the whitening path it's
    /// meant to guard) divides LLRs by per-tone/per-symbol MEDIAN tone
    /// magnitudes with a
    /// `NOISE_FLOOR = 1e-6` floor. That floor is calibrated for the
    /// LINEAR-magnitude domain (the fine-FFT path's native units, `|y|`),
    /// but several spectrogram-path callers hand it dB LOG-POWER values
    /// instead (`10*log10(power)`, commonly negative). Scaling the whole
    /// recording by a gain factor shifts every dB value by a CONSTANT
    /// (`20*log10(gain)` in magnitude terms), which changes how often the
    /// `1e-6` floor clamps the per-tone/per-symbol medians — a real input
    /// recording and a uniformly quieter recording of the IDENTICAL
    /// signal should decode to the identical message set; if the floor
    /// clamps differently at the two gains, the whitening divisor changes
    /// shape between the two runs and the decode set can differ.
    ///
    /// This decodes a real off-air recording (`wsjt/210703_133430.wav`,
    /// 9 messages at gain 1.0 per `wav_decode_tests.rs`) at gain 1.0 and
    /// at gain 0.01 (pre-scaled samples — a ~-40 dB quieter recording of
    /// the exact same signal) and asserts the decoded message sets match.
    #[test]
    fn test_whiten_llrs_gain_invariance() {
        let samples = read_wav_samples(&fixture_path("wsjt/210703_133430.wav"));

        let set_full = decode_set_at_gain(&samples, 1.0);
        let set_quiet = decode_set_at_gain(&samples, 0.01);

        assert!(
            !set_full.is_empty(),
            "sanity check: gain=1.0 baseline decode must find at least one \
             message on this real off-air recording (documented at 9 \
             messages in wav_decode_tests.rs)"
        );

        assert_eq!(
            set_full, set_quiet,
            "decode set must be gain-invariant — LDPC/CRC-verified messages \
             don't depend on absolute recording level. gain=1.0 decoded \
             {set_full:?}, gain=0.01 (~-40dB quieter, identical signal) \
             decoded {set_quiet:?}"
        );
    }

    /// Wider gain sweep than the primary two-point test above, spanning
    /// -80 dB to +40 dB relative to the fixture's native level (six
    /// orders of magnitude). Added during this task's investigation to
    /// probe for a gain-dependent flip beyond the single 0.01 data
    /// point; found none on this fixture (see the W1.4 experiment log
    /// for the full investigation, including instrumented floor-hit
    /// counts confirming the underlying dB/linear-magnitude unit
    /// mismatch is real but happens not to cross the `NOISE_FLOOR`
    /// threshold anywhere in this recording's dynamic range). Kept as a
    /// standing regression guard broader than the primary test.
    #[test]
    fn test_whiten_llrs_gain_invariance_wide_sweep() {
        let samples = read_wav_samples(&fixture_path("wsjt/210703_133430.wav"));
        let baseline = decode_set_at_gain(&samples, 1.0);
        assert!(
            !baseline.is_empty(),
            "sanity: gain=1.0 baseline must decode"
        );

        for gain in [0.0001f32, 0.001, 0.01, 0.1, 5.0, 20.0, 100.0] {
            let set = decode_set_at_gain(&samples, gain);
            assert_eq!(
                set, baseline,
                "decode set at gain={gain} must match the gain=1.0 baseline \
                 {baseline:?}; got {set:?}"
            );
        }
    }
}
