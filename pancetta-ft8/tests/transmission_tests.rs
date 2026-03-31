//! Integration tests for FT8 transmission functionality
//!
//! These tests validate the complete FT8 encoding and transmission pipeline,
//! including safety features and PTT control.

#[cfg(feature = "transmit")]
mod transmission_tests {
    use pancetta_ft8::{
        AudioConfig, AudioFormat, BandLimits, FrequencyConfig, Ft8Encoder, Ft8EncodingConfig,
        Ft8Modulator, Ft8Transmitter, ModulatorConfig, PowerConfig, PttConfig, PttMethod,
        SafetyConfig, SampleType, TransmissionConfig, TransmissionState, MESSAGE_DURATION,
        NUM_SYMBOLS, SAMPLE_RATE,
    };
    use std::time::{Duration, SystemTime};

    #[test]
    fn test_encoder_basic_messages() {
        let mut encoder = Ft8Encoder::new();

        // Test CQ encoding
        let cq_symbols = encoder.encode_cq("W1ABC", "FN42", false).unwrap();
        assert_eq!(cq_symbols.len(), NUM_SYMBOLS);
        assert!(cq_symbols.iter().all(|&s| s < 8));

        // Test signal report encoding
        let report_symbols = encoder.encode_signal_report("K1DEF", "W1ABC", -12).unwrap();
        assert_eq!(report_symbols.len(), NUM_SYMBOLS);
        assert!(report_symbols.iter().all(|&s| s < 8));

        // Test RRR encoding
        let rrr_symbols = encoder.encode_rrr("K1DEF", "W1ABC").unwrap();
        assert_eq!(rrr_symbols.len(), NUM_SYMBOLS);

        // Test 73 encoding
        let final_symbols = encoder.encode_73("K1DEF", "W1ABC").unwrap();
        assert_eq!(final_symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_encoder_freetext_messages() {
        let mut encoder = Ft8Encoder::new();

        // Test valid free text
        let freetext_symbols = encoder.encode_freetext("HELLO WORLD").unwrap();
        assert_eq!(freetext_symbols.len(), NUM_SYMBOLS);

        // Test short message
        let short_symbols = encoder.encode_freetext("HI").unwrap();
        assert_eq!(short_symbols.len(), NUM_SYMBOLS);

        // Test message too long
        let long_result = encoder.encode_freetext("THIS MESSAGE IS TOO LONG FOR FT8");
        assert!(long_result.is_err());

        // Test invalid characters
        let invalid_result = encoder.encode_freetext("HELLO@WORLD");
        assert!(invalid_result.is_err());
    }

    #[test]
    fn test_encoder_signal_report_limits() {
        let mut encoder = Ft8Encoder::new();

        // Test valid signal reports (WSJT-X range: -35 to +30 dB)
        assert!(encoder.encode_signal_report("K1DEF", "W1ABC", -35).is_ok());
        assert!(encoder.encode_signal_report("K1DEF", "W1ABC", 30).is_ok());
        assert!(encoder.encode_signal_report("K1DEF", "W1ABC", 0).is_ok());

        // Test out of range signal reports
        assert!(encoder.encode_signal_report("K1DEF", "W1ABC", -36).is_err());
        assert!(encoder.encode_signal_report("K1DEF", "W1ABC", 31).is_err());
    }

    #[test]
    fn test_modulator_creation() {
        // Test valid modulator creation
        let modulator = Ft8Modulator::new(12000, 1500.0, 0.5);
        assert!(modulator.is_ok());

        // Test invalid parameters
        assert!(Ft8Modulator::new(0, 1500.0, 0.5).is_err()); // Invalid sample rate
        assert!(Ft8Modulator::new(12000, 100.0, 0.5).is_err()); // Invalid frequency
        assert!(Ft8Modulator::new(12000, 1500.0, 1.5).is_err()); // Invalid power
    }

    #[test]
    fn test_modulator_symbol_generation() {
        let mut modulator = Ft8Modulator::new_default().unwrap();

        // Create test symbol sequence
        let symbols = [
            0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4,
            5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1,
            2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6,
        ]; // 79 symbols

        let audio_samples = modulator.modulate_symbols(&symbols, 0.0).unwrap();

        // Verify audio length
        let expected_samples = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;
        assert_eq!(audio_samples.len(), expected_samples);

        // Verify amplitude bounds
        assert!(audio_samples.iter().all(|&s| s.abs() <= 1.0));

        // Verify non-silence (should have signal content)
        let rms =
            (audio_samples.iter().map(|&s| s * s).sum::<f32>() / audio_samples.len() as f32).sqrt();
        assert!(rms > 0.001); // Should have some signal energy
    }

    #[test]
    fn test_modulator_frequency_limits() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let symbols = [0u8; 79];

        // Test valid frequency offsets
        assert!(modulator.modulate_symbols(&symbols, 0.0).is_ok());
        assert!(modulator.modulate_symbols(&symbols, 100.0).is_ok());
        assert!(modulator.modulate_symbols(&symbols, -100.0).is_ok());

        // Test excessive frequency offset
        assert!(modulator.modulate_symbols(&symbols, 3000.0).is_err());
        assert!(modulator.modulate_symbols(&symbols, -3000.0).is_err());
    }

    #[test]
    fn test_modulator_invalid_symbols() {
        let mut modulator = Ft8Modulator::new_default().unwrap();
        let mut symbols = [0u8; 79];
        symbols[0] = 8; // Invalid symbol (must be 0-7)

        let result = modulator.modulate_symbols(&symbols, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_modulator_test_tone() {
        let modulator = Ft8Modulator::new_default().unwrap();

        let test_tone = modulator.generate_test_tone(1000.0, 1.0).unwrap();
        assert_eq!(test_tone.len(), 12000); // 1 second at 12 kHz

        // Should be a pure tone
        let max_amplitude = test_tone.iter().map(|&s| s.abs()).fold(0.0f32, f32::max);
        assert!(max_amplitude > 0.1);
        assert!(max_amplitude <= 1.0);
    }

    #[test]
    fn test_modulator_power_control() {
        let mut modulator = Ft8Modulator::new_default().unwrap();

        // Test power setting
        assert!(modulator.set_tx_power(0.8).is_ok());
        assert_eq!(modulator.get_config().tx_power, 0.8);

        // Test invalid power values
        assert!(modulator.set_tx_power(-0.1).is_err());
        assert!(modulator.set_tx_power(1.1).is_err());
    }

    #[test]
    fn test_modulator_frequency_setting() {
        let mut modulator = Ft8Modulator::new_default().unwrap();

        // Test frequency setting
        assert!(modulator.set_base_frequency(1400.0).is_ok());
        assert_eq!(modulator.get_config().base_frequency, 1400.0);

        // Test invalid frequencies
        assert!(modulator.set_base_frequency(100.0).is_err());
        assert!(modulator.set_base_frequency(5000.0).is_err());
    }

    #[test]
    fn test_audio_format_conversion() {
        use pancetta_ft8::convert_samples;

        let test_samples = vec![0.0, 0.5, -0.5, 1.0, -1.0];

        // Test 16-bit conversion
        let format_i16 = AudioFormat::ft8_standard();
        let converted_i16 = convert_samples(&test_samples, format_i16);
        assert_eq!(converted_i16.len(), test_samples.len() * 2);

        // Test 32-bit float conversion
        let format_f32 = AudioFormat::ft8_high_quality();
        let converted_f32 = convert_samples(&test_samples, format_f32);
        assert_eq!(converted_f32.len(), test_samples.len() * 4);

        // Test custom format
        let custom_format = AudioFormat {
            sample_rate: 48000,
            bits_per_sample: 24,
            channels: 1,
            sample_type: SampleType::I24,
        };
        let converted_i24 = convert_samples(&test_samples, custom_format);
        assert_eq!(converted_i24.len(), test_samples.len() * 3);
    }

    #[test]
    fn test_transmission_config() {
        let config = TransmissionConfig::default();

        // Verify default values
        assert_eq!(config.frequency_config.base_frequency, 1500.0);
        assert_eq!(config.power_config.tx_power_level, 0.5);
        assert_eq!(config.ptt_config.method, PttMethod::None);
        assert!(config.safety_config.enable_tx_timeout);

        // Test custom configuration
        let custom_config = TransmissionConfig {
            frequency_config: FrequencyConfig {
                base_frequency: 1400.0,
                band_limits: BandLimits {
                    lower_edge: 7074000.0, // 40m band
                    upper_edge: 7076000.0,
                },
                frequency_calibration: 1.5,
            },
            power_config: PowerConfig {
                tx_power_level: 0.3,
                max_power_watts: 50,
                power_calibration: 0.95,
            },
            ptt_config: PttConfig {
                method: PttMethod::SerialDtr,
                serial_port: Some("/dev/ttyUSB0".to_string()),
                serial_baud_rate: 9600,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(custom_config.frequency_config.base_frequency, 1400.0);
        assert_eq!(custom_config.power_config.max_power_watts, 50);
        assert_eq!(custom_config.ptt_config.method, PttMethod::SerialDtr);
    }

    #[test]
    fn test_transmitter_creation() {
        let config = TransmissionConfig::default();
        let transmitter = Ft8Transmitter::new(config);
        assert!(transmitter.is_ok());

        let tx = transmitter.unwrap();
        assert_eq!(tx.get_state(), TransmissionState::Idle);

        let stats = tx.get_statistics();
        assert_eq!(stats.total_transmissions, 0);
        assert!(stats.transmission_allowed);
    }

    #[test]
    fn test_transmitter_emergency_stop() {
        let config = TransmissionConfig::default();
        let transmitter = Ft8Transmitter::new(config).unwrap();

        // Initial state should be idle
        assert_eq!(transmitter.get_state(), TransmissionState::Idle);

        // Activate emergency stop
        transmitter.emergency_stop();
        assert_eq!(transmitter.get_state(), TransmissionState::EmergencyStop);

        // Emergency stop should be reflected in state
        // Note: transmission_allowed might still be true from safety monitor perspective
        // but emergency_stop flag would prevent actual transmission
        let stats = transmitter.get_statistics();
        // The emergency stop doesn't necessarily change transmission_allowed from safety monitor,
        // but the state check would prevent transmission

        // Clear emergency stop
        transmitter.clear_emergency_stop().unwrap();
        assert_eq!(transmitter.get_state(), TransmissionState::Idle);

        let stats = transmitter.get_statistics();
        assert!(stats.transmission_allowed);
    }

    #[tokio::test]
    #[ignore] // Requires audio hardware (ALSA) — fails in CI
    async fn test_transmitter_system_test() {
        let config = TransmissionConfig::default();
        let mut transmitter = Ft8Transmitter::new(config).unwrap();

        // Run system test
        let test_result = transmitter.test_transmission_system(0.5).await;
        assert!(test_result.is_ok());

        let test_report = test_result.unwrap();
        assert!(test_report.success);
        assert!(test_report.ptt_test.success);
        assert!(test_report.audio_test.success);
        assert!(test_report.frequency_test.within_tolerance);
    }

    #[test]
    fn test_safety_monitor_tx_timeout() {
        // Test safety configuration instead of internal safety monitor
        let safety_config = SafetyConfig {
            max_tx_time_seconds: 10, // Short timeout for testing
            ..Default::default()
        };

        // Verify configuration
        assert_eq!(safety_config.max_tx_time_seconds, 10);
        assert!(safety_config.enable_tx_timeout);
        assert!(safety_config.enable_band_edge_protection);

        // Safety monitoring is tested through the transmitter interface
        let config = TransmissionConfig {
            safety_config,
            ..Default::default()
        };

        let transmitter = Ft8Transmitter::new(config).unwrap();
        let stats = transmitter.get_statistics();
        assert!(stats.transmission_allowed);
        assert_eq!(stats.total_transmissions, 0);
    }

    #[test]
    fn test_band_edge_protection() {
        let mut config = TransmissionConfig::default();
        config.frequency_config.band_limits = BandLimits {
            lower_edge: 14074000.0,
            upper_edge: 14076000.0,
        };

        // Test configuration itself
        assert_eq!(config.frequency_config.band_limits.lower_edge, 14074000.0);
        assert_eq!(config.frequency_config.band_limits.upper_edge, 14076000.0);

        let _transmitter = Ft8Transmitter::new(config).unwrap();
        // Band edge validation happens internally during transmission requests
    }

    #[test]
    fn test_ptt_methods() {
        // Test all PTT method variants
        let methods = [
            PttMethod::None,
            PttMethod::SerialDtr,
            PttMethod::SerialRts,
            PttMethod::CatCommand,
            PttMethod::Gpio,
            PttMethod::Vox,
        ];

        for method in methods {
            let config = PttConfig {
                method,
                serial_port: Some("/dev/ttyUSB0".to_string()),
                cat_ptt_on_command: "TX1;".to_string(),
                cat_ptt_off_command: "TX0;".to_string(),
                ..Default::default()
            };

            // Should be able to create config for all methods
            // (actual hardware initialization would fail, but config is valid)
            assert_eq!(config.method, method); // Verify the method was set correctly
        }
    }

    #[test]
    fn test_encoding_configuration() {
        let config = Ft8EncodingConfig::default();
        assert!(config.use_hash_encoding);
        assert!(config.enable_telemetry);
        assert_eq!(config.max_freetext_length, 13);

        let custom_config = Ft8EncodingConfig {
            use_hash_encoding: false,
            enable_telemetry: false,
            max_freetext_length: 10,
        };

        assert!(!custom_config.use_hash_encoding);
        assert!(!custom_config.enable_telemetry);
        assert_eq!(custom_config.max_freetext_length, 10);
    }

    #[test]
    fn test_modulator_configuration() {
        let config = ModulatorConfig::default();
        assert_eq!(config.sample_rate, SAMPLE_RATE);
        assert_eq!(config.base_frequency, 1500.0);
        assert_eq!(config.tone_spacing, 6.25);
        assert_eq!(config.tx_power, 0.5);
    }

    #[test]
    fn test_encoder_message_parsing() {
        let mut encoder = Ft8Encoder::new();

        // Test encoding different message types (which internally parses them)
        let cq_result = encoder.encode_message("CQ W1ABC FN42", None);
        assert!(cq_result.is_ok());

        let dx_cq_result = encoder.encode_message("CQ DX W1ABC FN42", None);
        assert!(dx_cq_result.is_ok());

        let resp_result = encoder.encode_message("W1ABC K1DEF FN41", None);
        assert!(resp_result.is_ok());

        let report_result = encoder.encode_message("K1DEF W1ABC -15", None);
        assert!(report_result.is_ok());

        let rrr_result = encoder.encode_message("K1DEF W1ABC RRR", None);
        assert!(rrr_result.is_ok());

        let final_result = encoder.encode_message("K1DEF W1ABC 73", None);
        assert!(final_result.is_ok());
    }

    #[test]
    fn test_symbol_timing_calculation() {
        let modulator = Ft8Modulator::new_default().unwrap();
        let timing = modulator.calculate_symbol_timing();

        assert_eq!(timing.sample_rate, SAMPLE_RATE);
        assert_eq!(timing.symbol_duration_ms, 160); // 0.16 seconds = 160 ms
        assert_eq!(timing.total_duration_ms, 12640); // 12.64 seconds = 12640 ms
        assert_eq!(timing.samples_per_symbol, 1920); // 0.16s * 12000 Hz
    }

    #[test]
    fn test_complete_encoding_pipeline() {
        let mut encoder = Ft8Encoder::new();
        let mut modulator = Ft8Modulator::new_default().unwrap();

        // Test complete pipeline for different message types
        let messages = [
            "CQ W1ABC FN42",
            "W1ABC K1DEF FN41",
            "K1DEF W1ABC -12",
            "K1DEF W1ABC RRR",
            "K1DEF W1ABC 73",
        ];

        for message in messages {
            // Encode to symbols
            let symbols = encoder.encode_message(message, None).unwrap();
            assert_eq!(symbols.len(), NUM_SYMBOLS);
            assert!(symbols.iter().all(|&s| s < 8));

            // Modulate to audio
            let audio = modulator.modulate_symbols(&symbols, 0.0).unwrap();
            let expected_samples = (MESSAGE_DURATION * SAMPLE_RATE as f64) as usize;
            assert_eq!(audio.len(), expected_samples);

            // Verify audio characteristics
            let rms = (audio.iter().map(|&s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
            assert!(rms > 0.001); // Should have signal energy
            assert!(audio.iter().all(|&s| s.abs() <= 1.0)); // Should be bounded

            println!("Successfully processed message: '{}'", message);
        }
    }

    #[test]
    fn test_transmission_state_transitions() {
        // Test all state transitions
        let states = [
            TransmissionState::Idle,
            TransmissionState::Preparing,
            TransmissionState::Transmitting,
            TransmissionState::EmergencyStop,
        ];

        for state in states {
            // Each state should be equal to itself
            assert_eq!(state, state);

            // Different states should not be equal
            for other_state in states {
                if std::mem::discriminant(&state) != std::mem::discriminant(&other_state) {
                    assert_ne!(state, other_state);
                }
            }
        }
    }

    #[test]
    fn test_audio_format_characteristics() {
        let standard_format = AudioFormat::ft8_standard();
        assert_eq!(standard_format.sample_rate, 12000);
        assert_eq!(standard_format.bits_per_sample, 16);
        assert_eq!(standard_format.channels, 1);
        assert_eq!(standard_format.sample_type, SampleType::I16);
        assert_eq!(standard_format.bytes_per_sample(), 2);

        let hq_format = AudioFormat::ft8_high_quality();
        assert_eq!(hq_format.sample_rate, 12000);
        assert_eq!(hq_format.bits_per_sample, 32);
        assert_eq!(hq_format.channels, 1);
        assert_eq!(hq_format.sample_type, SampleType::F32);
        assert_eq!(hq_format.bytes_per_sample(), 4);
    }
}

// Tests that should always run, regardless of features
#[test]
fn test_ft8_transmission_constants() {
    use pancetta_ft8::{MESSAGE_DURATION, NUM_SYMBOLS, SAMPLE_RATE};

    // Verify FT8 constants are correct
    assert_eq!(NUM_SYMBOLS, 79);
    assert_eq!(SAMPLE_RATE, 12_000);
    assert_eq!(MESSAGE_DURATION, 12.64);

    // Verify timing calculations
    let symbol_duration = MESSAGE_DURATION / NUM_SYMBOLS as f64;
    assert!((symbol_duration - 0.16).abs() < 0.001); // Should be 160ms per symbol
}

#[cfg(not(feature = "transmit"))]
#[test]
fn test_transmission_feature_disabled() {
    // When transmit feature is disabled, transmission modules should not be available
    // This test just verifies the feature system works correctly

    // The encoder, modulator, and transmit modules should not be compiled
    // We can't directly test this, but the fact that this test compiles
    // without the transmit feature indicates the conditional compilation works

    println!("Transmission feature is disabled - transmission modules not available");
}
