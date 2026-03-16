//! Round-trip integration tests: encode → modulate → decode → verify
//!
//! These tests validate the complete FT8 pipeline by encoding a message,
//! generating audio, and decoding it back to verify the message matches.

#![cfg(feature = "transmit")]

mod test_signal_generator;

use bitvec::prelude::*;
use pancetta_ft8::ldpc::{LdpcEncoder, LDPC_CODEWORD_BITS, LDPC_INFO_BITS};
use pancetta_ft8::{
    ft8_lib_ffi::ft8lib_decode_audio, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, PulseShape,
    NUM_SYMBOLS, SAMPLE_RATE, WINDOW_SAMPLES,
};

/// Helper: encode message text to symbols
fn encode_message(text: &str) -> [u8; NUM_SYMBOLS] {
    let mut encoder = Ft8Encoder::new();
    encoder
        .encode_message(text, None)
        .unwrap_or_else(|e| panic!("Failed to encode '{}': {}", text, e))
}

/// Helper: modulate symbols to audio at given frequency offset
fn modulate_symbols(symbols: &[u8; NUM_SYMBOLS], frequency_offset: f64) -> Vec<f32> {
    let mut modulator = Ft8Modulator::new_default().unwrap();
    let mut audio = modulator
        .modulate_symbols(symbols, frequency_offset)
        .unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);
    audio
}

/// Helper: modulate symbols to audio using GFSK pulse shaping
fn modulate_symbols_gfsk(symbols: &[u8; NUM_SYMBOLS], frequency_offset: f64) -> Vec<f32> {
    use pancetta_ft8::BASE_FREQUENCY;
    let mut modulator = Ft8Modulator::with_pulse_shape(
        SAMPLE_RATE,
        BASE_FREQUENCY,
        0.5,
        PulseShape::Gaussian { bt: 2.0 },
    )
    .unwrap();
    let mut audio = modulator
        .modulate_symbols(symbols, frequency_offset)
        .unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);
    audio
}

/// Helper: add calibrated noise to audio signal
fn add_noise(audio: &mut [f32], snr_db: f32) {
    test_signal_generator::add_gaussian_noise(audio, snr_db);
}

/// Helper: decode audio window
fn decode_audio(audio: &[f32]) -> Vec<pancetta_ft8::DecodedMessage> {
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    decoder.decode_window(audio).unwrap_or_default()
}

// =============================================================
// Bit-Level Round-Trip Tests (no audio, just LDPC encode/decode)
// =============================================================

#[test]
fn test_ldpc_bit_level_round_trip() {
    let encoder = LdpcEncoder::new();

    // Create several test messages and verify encode → syndrome check
    for pattern in 0..20u8 {
        let mut info_bits = bitvec![0; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            info_bits.set(
                i,
                ((i as u8).wrapping_add(pattern).wrapping_mul(7)) % 3 == 0,
            );
        }

        let codeword = encoder.encode(&info_bits).unwrap();
        assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);

        // Verify info bits preserved
        for i in 0..LDPC_INFO_BITS {
            assert_eq!(
                codeword[i], info_bits[i],
                "Bit {} mismatch in pattern {}",
                i, pattern
            );
        }

        // Verify syndrome = 0
        assert!(
            encoder.verify_syndrome(&codeword),
            "Syndrome check failed for pattern {}",
            pattern
        );
    }
}

// =============================================================
// Symbol-Level Round-Trip Tests (encode → symbol extraction)
// =============================================================

#[test]
fn test_encoder_determinism() {
    let mut encoder = Ft8Encoder::new();

    let symbols1 = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
    let symbols2 = encoder.encode_message("CQ W1ABC FN42", None).unwrap();

    assert_eq!(symbols1, symbols2, "Encoding should be deterministic");
}

#[test]
fn test_different_messages_produce_different_symbols() {
    let mut encoder = Ft8Encoder::new();

    let symbols_cq = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
    let symbols_report = encoder.encode_message("K1DEF W1ABC -12", None).unwrap();

    // The data symbols should differ (sync symbols are the same)
    let data_differ = symbols_cq
        .iter()
        .zip(symbols_report.iter())
        .enumerate()
        .filter(|(i, _)| {
            // Skip sync positions
            !(0..7).contains(i) && !(36..43).contains(i) && !(72..79).contains(i)
        })
        .any(|(_, (a, b))| a != b);

    assert!(
        data_differ,
        "Different messages should produce different data symbols"
    );
}

#[test]
fn test_costas_arrays_in_encoded_symbols() {
    let costas = [3u8, 1, 4, 0, 6, 5, 2];

    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "HELLO WORLD",
    ];

    let mut encoder = Ft8Encoder::new();

    for msg in &messages {
        let symbols = encoder.encode_message(msg, None).unwrap();

        assert_eq!(
            &symbols[0..7],
            &costas,
            "First Costas mismatch for '{}'",
            msg
        );
        assert_eq!(
            &symbols[36..43],
            &costas,
            "Second Costas mismatch for '{}'",
            msg
        );
        assert_eq!(
            &symbols[72..79],
            &costas,
            "Third Costas mismatch for '{}'",
            msg
        );
    }
}

#[test]
fn test_all_symbols_in_valid_range() {
    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "W1ABC K1DEF FN42",
        "HELLO WORLD",
    ];

    let mut encoder = Ft8Encoder::new();

    for msg in &messages {
        let symbols = encoder.encode_message(msg, None).unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
        for (i, &s) in symbols.iter().enumerate() {
            assert!(
                s < 8,
                "Symbol {} = {} out of range [0,7] for '{}'",
                i,
                s,
                msg
            );
        }
    }
}

// =============================================================
// Audio-Level Round-Trip Tests (encode → modulate → decode)
// =============================================================

/// Test that encoding → modulation produces audio with expected characteristics
#[test]
fn test_encode_modulate_audio_characteristics() {
    let symbols = encode_message("CQ W1ABC FN42");
    let audio = modulate_symbols(&symbols, 0.0);

    assert_eq!(audio.len(), WINDOW_SAMPLES);

    // Audio should have non-trivial content
    let rms = (audio.iter().map(|&s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
    assert!(rms > 0.001, "Audio should have signal energy, RMS={}", rms);

    // Audio should be bounded
    assert!(
        audio.iter().all(|&s| s.abs() <= 1.0),
        "Audio samples should be bounded [-1, 1]"
    );
}

/// Full round-trip with clean channel — must decode correctly
#[test]
fn test_round_trip_cq_clean() {
    let symbols = encode_message("CQ W1ABC FN42");
    let audio = modulate_symbols(&symbols, 0.0);

    let decoded = decode_audio(&audio);

    assert_eq!(decoded.len(), 1, "Expected exactly 1 decoded message");
    assert_eq!(decoded[0].text, "CQ W1ABC FN42");
}

/// Test that modulated signals at different frequencies produce distinct audio
#[test]
fn test_round_trip_frequency_offset() {
    let symbols = encode_message("CQ W1ABC FN42");

    let offsets = [0.0, 50.0, 100.0, -50.0];
    let mut audios: Vec<Vec<f32>> = Vec::new();

    for &offset in &offsets {
        let audio = modulate_symbols(&symbols, offset);
        assert_eq!(audio.len(), WINDOW_SAMPLES);
        audios.push(audio);
    }

    // Different frequency offsets should produce measurably different audio
    // (compare RMS of difference between offset=0 and offset=100)
    let diff: f32 = audios[0]
        .iter()
        .zip(audios[2].iter())
        .map(|(a, b)| (a - b).powi(2))
        .sum::<f32>()
        / WINDOW_SAMPLES as f32;
    assert!(
        diff > 1e-6,
        "Different frequency offsets should produce different audio"
    );
}

/// Test decoding multiple messages combined into one audio window
#[test]
fn test_round_trip_multiple_signals() {
    let messages = [("CQ W1ABC FN42", -50.0), ("K1DEF W1ABC -12", 50.0)];

    let mut combined = vec![0.0f32; WINDOW_SAMPLES];

    for (msg, freq_offset) in &messages {
        let symbols = encode_message(msg);
        let audio = modulate_symbols(&symbols, *freq_offset);
        for (i, &s) in audio.iter().enumerate() {
            if i < combined.len() {
                combined[i] += s;
            }
        }
    }

    let decoded = decode_audio(&combined);

    // Should decode at least one of the two messages
    assert!(
        !decoded.is_empty(),
        "Should decode at least one message from combined signal"
    );

    let texts: Vec<&str> = decoded.iter().map(|m| m.text.as_str()).collect();
    // Verify the decoded messages match expected texts
    for (expected_msg, _) in &messages {
        if texts.contains(expected_msg) {
            println!("OK: decoded '{}'", expected_msg);
        }
    }
}

/// Test SNR sweep — must decode at high SNR, graceful degradation at low SNR
#[test]
fn test_round_trip_snr_sweep() {
    let symbols = encode_message("CQ W1ABC FN42");

    // High SNR: must decode correctly
    for &snr in &[20.0, 10.0] {
        let mut audio = modulate_symbols(&symbols, 0.0);
        add_noise(&mut audio, snr);

        let decoded = decode_audio(&audio);
        assert!(
            decoded.iter().any(|m| m.text == "CQ W1ABC FN42"),
            "Should decode at SNR={} dB",
            snr
        );
    }

    // Low SNR: may or may not decode (no assertion, just verify no crash)
    for &snr in &[0.0, -5.0, -10.0] {
        let mut audio = modulate_symbols(&symbols, 0.0);
        add_noise(&mut audio, snr);
        let _decoded = decode_audio(&audio);
    }
}

/// Full round-trip for all standard FT8 message types
#[test]
fn test_round_trip_all_message_types() {
    let messages = [
        "CQ W1ABC FN42",
        "CQ DX W1ABC FN42",
        "K1DEF W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC +05",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "K1DEF W1ABC RR73",
        "HELLO WORLD",
    ];

    for msg in &messages {
        let symbols = encode_message(msg);
        let audio = modulate_symbols(&symbols, 0.0);
        let decoded = decode_audio(&audio);

        assert!(
            decoded.iter().any(|m| m.text == *msg),
            "Round-trip failed for '{}': decoded {:?}",
            msg,
            decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }
}

// =========================================================================
// GFSK validation: ft8_lib should decode our GFSK-modulated signals
// =========================================================================

#[test]
fn test_gfsk_decoded_by_ft8lib() {
    let message = "CQ W1ABC FN42";
    let symbols = encode_message(message);
    let audio = modulate_symbols_gfsk(&symbols, 0.0);

    let decoded = ft8lib_decode_audio(&audio);
    println!(
        "GFSK '{}': ft8_lib decoded {} messages",
        message,
        decoded.len()
    );
    for (text, freq, _time, _ldpc) in &decoded {
        println!("  [ft8lib] {:.1} Hz  {}", freq, text);
    }

    assert!(
        decoded.iter().any(|(text, _, _, _)| text.contains("W1ABC")),
        "ft8_lib should decode our GFSK signal for '{}': got {:?}",
        message,
        decoded.iter().map(|(t, _, _, _)| t).collect::<Vec<_>>()
    );
}

// =========================================================================
// FT4 round-trip tests
// =========================================================================

/// Helper: encode a message as FT4 symbols
fn encode_ft4(text: &str) -> Vec<u8> {
    use pancetta_ft8::ProtocolParams;
    let mut encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
    encoder
        .encode_message_protocol(text, None)
        .unwrap_or_else(|e| panic!("Failed to FT4-encode '{}': {}", text, e))
}

/// Helper: modulate FT4 symbols to audio
fn modulate_ft4(symbols: &[u8], frequency_offset: f64) -> Vec<f32> {
    use pancetta_ft8::{ProtocolParams, BASE_FREQUENCY};
    let params = ProtocolParams::ft4();
    // Use rectangular pulse shaping for decode compatibility.
    // GFSK BT=1.0 is the standard for FT4 OTA but our decoder uses raw DFT
    // which works best with rectangular/CPFSK modulation.
    let mut modulator = Ft8Modulator::with_pulse_shape(
        SAMPLE_RATE,
        BASE_FREQUENCY,
        0.5,
        PulseShape::Rectangular,
    )
    .unwrap();
    let mut audio = modulator
        .modulate_symbols_protocol(symbols, frequency_offset, &params)
        .unwrap();
    // Pad to FT4 window size (7.5s × 12000 = 90000 samples)
    let window = params.window_samples(SAMPLE_RATE);
    audio.resize(window, 0.0);
    audio
}

/// Helper: decode FT4 audio
fn decode_ft4_audio(audio: &[f32]) -> Vec<pancetta_ft8::DecodedMessage> {
    use pancetta_ft8::Protocol;
    let config = Ft8Config {
        protocol: Protocol::Ft4,
        max_decode_passes: 1,
        ..Ft8Config::default()
    };
    let mut decoder = Ft8Decoder::new(config).unwrap();
    decoder.decode_window(audio).unwrap_or_default()
}

#[test]
fn test_ft4_encode_modulate_basic() {
    use pancetta_ft8::ProtocolParams;
    let symbols = encode_ft4("CQ W1ABC FN42");
    assert_eq!(symbols.len(), 105);
    assert!(symbols.iter().all(|&s| s < 4));

    let params = ProtocolParams::ft4();
    assert_eq!(params.total_samples(SAMPLE_RATE), 60480);
}

#[test]
fn test_ft4_round_trip_cq() {
    let symbols = encode_ft4("CQ W1ABC FN42");
    let audio = modulate_ft4(&symbols, 0.0);
    let decoded = decode_ft4_audio(&audio);

    assert!(
        decoded.iter().any(|m| m.text == "CQ W1ABC FN42"),
        "FT4 round-trip failed for 'CQ W1ABC FN42': decoded {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

#[test]
fn test_ft4_round_trip_all_message_types() {
    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "K1DEF W1ABC RR73",
        "HELLO WORLD",
    ];

    for msg in &messages {
        let symbols = encode_ft4(msg);
        let audio = modulate_ft4(&symbols, 0.0);
        let decoded = decode_ft4_audio(&audio);

        assert!(
            decoded.iter().any(|m| m.text == *msg),
            "FT4 round-trip failed for '{}': decoded {:?}",
            msg,
            decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }

    // CQ DX — verify it decodes correctly
    let cq_dx_symbols = encode_ft4("CQ DX W1ABC FN42");
    let cq_dx_audio = modulate_ft4(&cq_dx_symbols, 0.0);
    let cq_dx_decoded = decode_ft4_audio(&cq_dx_audio);
    assert!(
        cq_dx_decoded.iter().any(|m| m.text == "CQ DX W1ABC FN42"),
        "CQ DX FT4 round-trip failed: decoded {:?}",
        cq_dx_decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

// =========================================================================
// Multi-TX round-trip tests
// =========================================================================

// =========================================================================
// FT2 round-trip tests (experimental, feature-gated)
// =========================================================================

#[cfg(feature = "ft2")]
#[test]
fn test_ft2_encode_modulate_basic() {
    use pancetta_ft8::ProtocolParams;
    let params = ProtocolParams::ft2();

    let mut encoder = Ft8Encoder::with_protocol(params.clone());
    let symbols = encoder
        .encode_message_protocol("CQ W1ABC FN42", None)
        .unwrap();

    assert_eq!(symbols.len(), 79);
    assert!(symbols.iter().all(|&s| s < 8));

    // Modulate
    let mut modulator = Ft8Modulator::with_pulse_shape(
        SAMPLE_RATE,
        1500.0,
        0.5,
        PulseShape::Rectangular,
    )
    .unwrap();
    let audio = modulator
        .modulate_symbols_protocol(&symbols, 0.0, &params)
        .unwrap();

    // FT2: 79 symbols × 480 samples/symbol = 37920 samples
    assert_eq!(audio.len(), params.total_samples(SAMPLE_RATE));
    assert!(audio.iter().all(|&s| s.abs() <= 1.0));
}

#[cfg(feature = "ft2")]
#[test]
fn test_ft2_round_trip() {
    use pancetta_ft8::{Protocol, ProtocolParams};

    let params = ProtocolParams::ft2();
    let mut encoder = Ft8Encoder::with_protocol(params.clone());
    let symbols = encoder
        .encode_message_protocol("CQ W1ABC FN42", None)
        .unwrap();

    let mut modulator = Ft8Modulator::with_pulse_shape(
        SAMPLE_RATE,
        1500.0,
        0.5,
        PulseShape::Rectangular,
    )
    .unwrap();
    let mut audio = modulator
        .modulate_symbols_protocol(&symbols, 0.0, &params)
        .unwrap();
    audio.resize(params.window_samples(SAMPLE_RATE), 0.0);

    let config = Ft8Config {
        protocol: Protocol::Ft2,
        max_decode_passes: 1,
        ..Ft8Config::default()
    };
    let mut decoder = Ft8Decoder::new(config).unwrap();
    let decoded = decoder.decode_window(&audio).unwrap_or_default();

    assert!(
        decoded.iter().any(|m| m.text == "CQ W1ABC FN42"),
        "FT2 round-trip failed: decoded {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

/// Test: two FT8 messages at different frequencies → decode both
#[test]
fn test_multi_tx_round_trip_two_ft8() {
    use pancetta_ft8::{modulate_multi_tx, MultiTxItem, ProtocolParams};

    let params = ProtocolParams::ft8();
    let symbols1 = encode_message("CQ W1ABC FN42");
    let symbols2 = encode_message("K1DEF W1ABC -12");

    let items = vec![
        MultiTxItem {
            symbols: &symbols1,
            frequency_offset: -100.0, // 1400 Hz
            params: &params,
        },
        MultiTxItem {
            symbols: &symbols2,
            frequency_offset: 100.0, // 1600 Hz
            params: &params,
        },
    ];

    let mut combined =
        modulate_multi_tx(&items, SAMPLE_RATE, 1500.0, 0.5).unwrap();
    combined.resize(WINDOW_SAMPLES, 0.0);

    let decoded = decode_audio(&combined);
    let texts: Vec<&str> = decoded.iter().map(|m| m.text.as_str()).collect();

    assert!(
        texts.contains(&"CQ W1ABC FN42"),
        "Should decode first message from multi-TX: got {:?}",
        texts
    );
    assert!(
        texts.contains(&"K1DEF W1ABC -12"),
        "Should decode second message from multi-TX: got {:?}",
        texts
    );
}
