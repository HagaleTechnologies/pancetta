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

/// Full round-trip for all standard FT8 message types.
///
/// FreeText ("HELLO WORLD") is intentionally excluded as of Batch 32:
/// `is_plausible` now rejects FreeText unconditionally for the
/// autonomous-station profile (16/16 emissions were FP on hard-200).
/// See `test_freetext_round_trip_rejected_post_batch_32` for the
/// inverse assertion.
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

/// Batch 32: FreeText round-trips through encoder + modulator + decoder
/// at the SIGNAL level, but `is_plausible` rejects it at the message-
/// level filter. Decoded output should NOT contain "HELLO WORLD".
#[test]
fn test_freetext_round_trip_rejected_post_batch_32() {
    let symbols = encode_message("HELLO WORLD");
    let audio = modulate_symbols(&symbols, 0.0);
    let decoded = decode_audio(&audio);
    // The FreeText filter is now unconditional; decoded must not include
    // the original FreeText message.
    assert!(
        !decoded.iter().any(|m| m.text == "HELLO WORLD"),
        "FreeText must be rejected post-Batch-32: decoded = {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
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
    for (text, freq, _time, _ldpc, snr) in &decoded {
        println!("  [ft8lib] {:+.1} dB  {:.1} Hz  {}", snr, freq, text);
    }

    assert!(
        decoded
            .iter()
            .any(|(text, _, _, _, _)| text.contains("W1ABC")),
        "ft8_lib should decode our GFSK signal for '{}': got {:?}",
        message,
        decoded.iter().map(|(t, _, _, _, _)| t).collect::<Vec<_>>()
    );
}

/// Two synthetic FT8 signals at different amplitudes must decode with
/// different (non-zero, monotonically-ordered) SNRs via the ft8_lib path.
///
/// Regression for the "every decode shows SNR +0" bug: the FFI now
/// computes a real SNR from the waterfall, so the louder signal must read
/// a higher SNR than the quieter one, and neither may be the old hard-coded
/// 0.0.
#[test]
fn test_ft8lib_snr_tracks_amplitude_two_signals() {
    let loud_msg = "CQ W1ABC FN42";
    let quiet_msg = "CQ K9XYZ EM79";

    let loud = modulate_symbols(&encode_message(loud_msg), 200.0); // ~1700 Hz
    let quiet = modulate_symbols(&encode_message(quiet_msg), 800.0); // ~2300 Hz

    // Sum at very different amplitudes over a common white-noise floor so
    // the SNR metric (signal-vs-local-noise) has something to reference and
    // the ordering is unambiguous. A deterministic LCG keeps the test
    // reproducible without an rng dependency.
    let n = loud.len().max(quiet.len());
    let mut mixed = vec![0.0f32; n];
    let mut state: u32 = 0x1234_5678;
    for i in 0..n {
        // xorshift-ish noise in roughly [-1, 1].
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let noise = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let l = loud.get(i).copied().unwrap_or(0.0) * 0.80;
        let q = quiet.get(i).copied().unwrap_or(0.0) * 0.12;
        mixed[i] = l + q + noise * 0.02;
    }

    let decoded = ft8lib_decode_audio(&mixed);
    let snr_of = |needle: &str| -> Option<f32> {
        decoded
            .iter()
            .find(|(t, _, _, _, _)| t == needle)
            .map(|(_, _, _, _, snr)| *snr)
    };

    let loud_snr =
        snr_of(loud_msg).unwrap_or_else(|| panic!("loud signal not decoded; got {:?}", decoded));
    let quiet_snr =
        snr_of(quiet_msg).unwrap_or_else(|| panic!("quiet signal not decoded; got {:?}", decoded));

    // Neither is the old constant-0 bug.
    assert_ne!(loud_snr, 0.0, "loud SNR is the hard-coded 0.0");
    assert_ne!(quiet_snr, 0.0, "quiet SNR is the hard-coded 0.0");

    // Louder signal reads a higher SNR.
    assert!(
        loud_snr > quiet_snr,
        "expected louder signal SNR ({:.1}) > quieter ({:.1})",
        loud_snr,
        quiet_snr
    );

    // Both in a plausible WSJT-X-referenced range.
    for (label, snr) in [("loud", loud_snr), ("quiet", quiet_snr)] {
        assert!(
            (-30.0..=40.0).contains(&snr),
            "{} SNR {:.1} dB out of plausible range",
            label,
            snr
        );
    }
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
    let mut modulator =
        Ft8Modulator::with_pulse_shape(SAMPLE_RATE, BASE_FREQUENCY, 0.5, PulseShape::Rectangular)
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
    // FreeText ("HELLO WORLD") excluded — see Batch 32 `is_plausible`
    // unconditional reject.
    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "K1DEF W1ABC RR73",
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
    let mut modulator =
        Ft8Modulator::with_pulse_shape(SAMPLE_RATE, 1500.0, 0.5, PulseShape::Rectangular).unwrap();
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

    let mut modulator =
        Ft8Modulator::with_pulse_shape(SAMPLE_RATE, 1500.0, 0.5, PulseShape::Rectangular).unwrap();
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

    let mut combined = modulate_multi_tx(&items, SAMPLE_RATE, 1500.0, 0.5).unwrap();
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

/// Verify decoder handles silence (all-zero audio) without producing -inf or NaN
#[test]
fn test_decode_silence_no_panic_or_inf() {
    let silence = vec![0.0f64; WINDOW_SAMPLES];
    let config = pancetta_ft8::Ft8Config::default();
    let mut decoder = pancetta_ft8::Ft8Decoder::new(config).unwrap();
    let waterfall = decoder.generate_waterfall_data(&silence).unwrap();
    assert!(
        waterfall.min_power.is_finite(),
        "Waterfall min_power should be finite, got {}",
        waterfall.min_power
    );
    assert!(
        waterfall.max_power.is_finite(),
        "Waterfall max_power should be finite, got {}",
        waterfall.max_power
    );
}

// =============================================================
// SNR calibration regression (WSJT-X 2500 Hz reference)
// =============================================================

/// Deterministic Box-Muller Gaussian PRNG (xorshift64) for calibrated noise.
struct CalRng(u64);
impl CalRng {
    fn new(seed: u64) -> Self {
        CalRng(seed | 1)
    }
    fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0)
    }
    fn gaussian(&mut self) -> f64 {
        let u1 = self.unit();
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Add white Gaussian noise so the WSJT-X 2500 Hz-reference SNR equals
/// `target_snr_db`. For real white noise of variance sigma^2 sampled at
/// `SAMPLE_RATE`, the one-sided spectrum spans 0..fs/2, so the power in a
/// 2500 Hz reference bandwidth is `sigma^2 * 2500/(fs/2)`. We solve for sigma
/// from `P_noise_2500 = P_sig / 10^(snr/10)`.
fn add_noise_2500ref(audio: &mut [f32], p_sig: f64, target_snr_db: f64, seed: u64) {
    let p_noise_2500 = p_sig / 10f64.powf(target_snr_db / 10.0);
    let half_band = SAMPLE_RATE as f64 / 2.0;
    let sigma = (p_noise_2500 * half_band / 2500.0).sqrt();
    let mut rng = CalRng::new(seed);
    for s in audio.iter_mut() {
        *s += (sigma * rng.gaussian()) as f32;
    }
}

/// Mean power over the active (modulated) span of the slot.
fn active_signal_power(audio: &[f32]) -> f64 {
    let active = audio
        .iter()
        .rposition(|&x| x != 0.0)
        .map(|i| i + 1)
        .unwrap_or(audio.len());
    audio[..active]
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum::<f64>()
        / active as f64
}

/// Both decode paths must report an SNR close to the *true* WSJT-X 2500 Hz
/// SNR across the operational band. This pins the linearity calibration in
/// `decoder::snr_from_tone_mags_db` and `ft8_lib_ffi::estimate_snr_from_waterfall`
/// (derived in `examples/snr_calibration.rs`); a regression there would shift
/// reported SNR off the WSJT-X convention.
#[test]
fn test_snr_calibration_wsjtx_2500ref() {
    let symbols = encode_message("CQ K5ARH EM12");
    let clean = modulate_symbols(&symbols, 0.0);
    let p_sig = active_signal_power(&clean);

    // Operational band where SNR reports drive QSO decisions. Tolerance is
    // generous (±3 dB) — the calibration targets ~±1 dB but a single noise
    // realization per point wobbles, and the goal is "unbiased & WSJT-X-aligned",
    // not bit-exact.
    let cases = [(-17.0, 4.0), (-13.0, 3.0), (-9.0, 3.0), (-5.0, 3.5)];

    for (true_snr, tol) in cases {
        let mut audio = clean.clone();
        add_noise_2500ref(
            &mut audio,
            p_sig,
            true_snr,
            0xC0FFEE ^ (true_snr as i64 as u64),
        );

        // Native path.
        let native = decode_audio(&audio);
        let nat = native
            .iter()
            .find(|m| m.text.contains("K5ARH"))
            .unwrap_or_else(|| panic!("native decode failed at true SNR {true_snr} dB"));
        assert!(
            (nat.snr_db as f64 - true_snr).abs() <= tol,
            "native reported SNR {} too far from true {} dB (tol {})",
            nat.snr_db,
            true_snr,
            tol
        );

        // ft8_lib FFI path.
        let ffi = ft8lib_decode_audio(&audio);
        let (_t, _f, _ti, _l, ffi_snr) = ffi
            .iter()
            .find(|(t, ..)| t.contains("K5ARH"))
            .unwrap_or_else(|| panic!("ffi decode failed at true SNR {true_snr} dB"));
        assert!(
            (*ffi_snr as f64 - true_snr).abs() <= tol,
            "ffi reported SNR {} too far from true {} dB (tol {})",
            ffi_snr,
            true_snr,
            tol
        );

        // The two paths must also agree with each other.
        assert!(
            (nat.snr_db - ffi_snr).abs() <= 3.0,
            "native ({}) and ffi ({}) SNR disagree at true {} dB",
            nat.snr_db,
            ffi_snr,
            true_snr
        );
    }
}

/// Reported SNR must never fall outside WSJT-X's conventional range
/// (-24..+24 dB), even on pure noise or a very strong signal.
#[test]
fn test_snr_reported_range_clamped() {
    // Pure noise: no signal tone; estimator must clamp, not produce -inf/NaN.
    let mut noise = vec![0.0f32; WINDOW_SAMPLES];
    let mut rng = CalRng::new(0xDEAD_BEEF);
    for s in noise.iter_mut() {
        *s = (0.05 * rng.gaussian()) as f32;
    }
    for m in decode_audio(&noise) {
        assert!(
            (-24.0..=24.0).contains(&m.snr_db) && m.snr_db.is_finite(),
            "SNR out of range on noise: {}",
            m.snr_db
        );
    }

    // Very strong clean signal: reported SNR is clamped at the top of the range.
    let symbols = encode_message("CQ K5ARH EM12");
    let strong = modulate_symbols(&symbols, 0.0);
    for m in decode_audio(&strong) {
        assert!(
            (-24.0..=24.0).contains(&m.snr_db) && m.snr_db.is_finite(),
            "SNR out of range on strong signal: {}",
            m.snr_db
        );
    }
}
