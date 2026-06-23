//! WSJT-X compatibility tests
//!
//! These tests validate that Pancetta's FT8 encoder produces bit-exact output
//! matching the WSJT-X / ft8_lib reference implementation.
//!
//! Reference vectors were generated using kgoba/ft8_lib compiled from source.

// rationale: test/bench loops index buffers by position; the index is
// load-bearing and an iterator rewrite would obscure intent.
#![allow(clippy::needless_range_loop)]
#![cfg(feature = "transmit")]

use bitvec::prelude::*;
use pancetta_ft8::ldpc::{gray_to_binary, LdpcEncoder, LDPC_CODEWORD_BITS, LDPC_INFO_BITS};
use pancetta_ft8::message::{calculate_crc14, PAYLOAD_BITS};
use pancetta_ft8::{Ft8Encoder, NUM_SYMBOLS};

// ============================================================================
// pack28 reference values
// ============================================================================

#[test]
fn test_pack28_cq_matches_wsjtx() {
    use pancetta_ft8::encoder::pack28;
    let (n28, ip) = pack28("CQ").unwrap();
    assert_eq!(n28, 2, "pack28('CQ') should be 2");
    assert_eq!(ip, 0);
}

#[test]
fn test_pack28_k1abc_matches_wsjtx() {
    use pancetta_ft8::encoder::pack28;
    let (n28, ip) = pack28("K1ABC").unwrap();
    // Reference: NTOKENS + MAX22 + basecall = 2063592 + 4194304 + 3957069 = 10214965
    assert_eq!(n28, 10_214_965, "pack28('K1ABC') should be 10214965");
    assert_eq!(ip, 0);
}

#[test]
fn test_packgrid_fn42_matches_wsjtx() {
    use pancetta_ft8::encoder::packgrid;
    let igrid = packgrid("FN42");
    assert_eq!(igrid, 10342, "packgrid('FN42') should be 10342");
}

// ============================================================================
// Payload bit-exactness tests
// ============================================================================

/// Reference payload for "CQ K1ABC FN42" from ft8_lib
const CQ_K1ABC_FN42_PAYLOAD: [u8; 10] =
    [0x00, 0x00, 0x00, 0x20, 0x4d, 0xef, 0x1a, 0x8a, 0x19, 0x88];

/// Reference payload for "K1DEF W1ABC -12" from ft8_lib
const K1DEF_W1ABC_M12_PAYLOAD: [u8; 10] =
    [0x09, 0xbe, 0x71, 0x40, 0x5f, 0xf4, 0x4e, 0x9f, 0xa9, 0xc8];

/// Reference payload for "HELLO WORLD" (free text) from ft8_lib
const HELLO_WORLD_PAYLOAD: [u8; 10] = [0x3c, 0x02, 0x0b, 0x01, 0xe3, 0x89, 0xcc, 0x38, 0x10, 0x00];

#[test]
fn test_cq_k1abc_fn42_payload_matches_wsjtx() {
    let mut encoder = Ft8Encoder::new();
    // Encode and extract the 77-bit payload from the symbols
    let symbols = encoder.encode_message("CQ K1ABC FN42", None).unwrap();

    // Extract payload by reversing: symbols → codeword → info bits → payload + CRC
    let payload = extract_payload_from_symbols(&symbols);

    assert_eq!(
        payload, CQ_K1ABC_FN42_PAYLOAD,
        "Payload mismatch for 'CQ K1ABC FN42'.\nGot:      {:02x?}\nExpected: {:02x?}",
        payload, CQ_K1ABC_FN42_PAYLOAD
    );
}

#[test]
fn test_k1def_w1abc_m12_payload_matches_wsjtx() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("K1DEF W1ABC -12", None).unwrap();
    let payload = extract_payload_from_symbols(&symbols);

    assert_eq!(
        payload, K1DEF_W1ABC_M12_PAYLOAD,
        "Payload mismatch for 'K1DEF W1ABC -12'.\nGot:      {:02x?}\nExpected: {:02x?}",
        payload, K1DEF_W1ABC_M12_PAYLOAD
    );
}

#[test]
fn test_hello_world_payload_matches_wsjtx() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("HELLO WORLD", None).unwrap();
    let payload = extract_payload_from_symbols(&symbols);

    assert_eq!(
        payload, HELLO_WORLD_PAYLOAD,
        "Payload mismatch for 'HELLO WORLD'.\nGot:      {:02x?}\nExpected: {:02x?}",
        payload, HELLO_WORLD_PAYLOAD
    );
}

// ============================================================================
// Symbol-level reference tests
// ============================================================================

/// Reference symbols for "CQ K1ABC FN42" from ft8_lib
const CQ_K1ABC_FN42_SYMBOLS: [u8; 79] = [
    3, 1, 4, 0, 6, 5, 2, // Costas 1
    0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 5, 4, 7, 6, 7, 0, 4, 6, 0, 6, 0, // Data 1 (29)
    2, 1, 5, 3, 3, 4, 3, // ← wait, that's only 7, need to include in data block
    3, 1, 4, 0, 6, 5, 2, // Costas 2
    7, 3, 6, 0, 1, 1, 0, 4, 7, 5, 1, 7, 0, 0, 7, 3, 3, 4, 7, 4, 5, 4, 5, 5, 1, 3, 3, 5,
    4, // Data 2 (29)
    3, 1, 4, 0, 6, 5, 2, // Costas 3
];

/// Reference symbols for "K1DEF W1ABC -12" from ft8_lib
const K1DEF_W1ABC_M12_SYMBOLS: [u8; 79] = [
    3, 1, 4, 0, 6, 5, 2, // Costas 1
    0, 3, 2, 2, 7, 1, 4, 1, 3, 0, 0, 6, 7, 7, 4, 5, 3, 2, 6, 1, 7, 4, // Data 1 (29)
    6, 1, 4, 3, 0, 3, 5, 3, 1, 4, 0, 6, 5, 2, // Costas 2
    3, 2, 3, 2, 7, 7, 6, 2, 4, 2, 3, 4, 1, 1, 0, 5, 0, 6, 0, 7, 1, 7, 1, 2, 3, 5, 3, 7,
    5, // Data 2 (29)
    3, 1, 4, 0, 6, 5, 2, // Costas 3
];

#[test]
fn test_cq_k1abc_fn42_symbols_match_wsjtx() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("CQ K1ABC FN42", None).unwrap();

    assert_eq!(
        symbols, CQ_K1ABC_FN42_SYMBOLS,
        "Symbol mismatch for 'CQ K1ABC FN42'"
    );
}

#[test]
fn test_k1def_w1abc_m12_symbols_match_wsjtx() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("K1DEF W1ABC -12", None).unwrap();

    assert_eq!(
        symbols, K1DEF_W1ABC_M12_SYMBOLS,
        "Symbol mismatch for 'K1DEF W1ABC -12'"
    );
}

// ============================================================================
// Additional WSJT-X payload compatibility tests
// ============================================================================

#[test]
fn test_special_tokens_payload() {
    let mut encoder = Ft8Encoder::new();

    // RRR, RR73, 73 all share the same callsigns, differ only in grid field
    let messages = [
        (
            "K1ABC W9XYZ RRR",
            [0x09u8, 0xbd, 0xe3, 0x50, 0x61, 0x49, 0xdc, 0x1f, 0xa4, 0x88],
        ),
        (
            "K1ABC W9XYZ 73",
            [0x09, 0xbd, 0xe3, 0x50, 0x61, 0x49, 0xdc, 0x1f, 0xa5, 0x08],
        ),
        (
            "K1ABC W9XYZ RR73",
            [0x09, 0xbd, 0xe3, 0x50, 0x61, 0x49, 0xdc, 0x1f, 0xa4, 0xc8],
        ),
    ];

    for (msg, expected_payload) in &messages {
        let symbols = encoder.encode_message(msg, None).unwrap();
        let payload = extract_payload_from_symbols(&symbols);
        assert_eq!(
            payload, *expected_payload,
            "Payload mismatch for '{}'.\nGot:      {:02x?}\nExpected: {:02x?}",
            msg, payload, expected_payload
        );
    }
}

#[test]
fn test_cq_dx_payload() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("CQ DX K1ABC FN42", None).unwrap();
    let payload = extract_payload_from_symbols(&symbols);

    let expected: [u8; 10] = [0x00, 0x00, 0x46, 0xf0, 0x4d, 0xef, 0x1a, 0x8a, 0x19, 0x88];
    assert_eq!(
        payload, expected,
        "Payload mismatch for 'CQ DX K1ABC FN42'.\nGot:      {:02x?}\nExpected: {:02x?}",
        payload, expected
    );
}

// ============================================================================
// CRC-14 verification
// ============================================================================

#[test]
fn test_crc14_for_reference_payloads() {
    // The CRC should be consistent and produce valid codewords
    for (name, payload_bytes) in [
        ("CQ K1ABC FN42", CQ_K1ABC_FN42_PAYLOAD),
        ("K1DEF W1ABC -12", K1DEF_W1ABC_M12_PAYLOAD),
        ("HELLO WORLD", HELLO_WORLD_PAYLOAD),
    ] {
        let mut payload_bits = BitVec::with_capacity(PAYLOAD_BITS);
        for i in 0..PAYLOAD_BITS {
            payload_bits.push(payload_bytes[i / 8] & (0x80u8 >> (i % 8)) != 0);
        }

        let crc = calculate_crc14(&payload_bits);
        assert!(
            crc < (1 << 14),
            "CRC for '{}' exceeds 14 bits: {}",
            name,
            crc
        );

        // Build 91-bit message and verify LDPC encoding works
        let mut msg_bits = BitVec::with_capacity(LDPC_INFO_BITS);
        msg_bits.extend_from_bitslice(&payload_bits);
        for i in (0..14).rev() {
            msg_bits.push((crc >> i) & 1 != 0);
        }

        let encoder = LdpcEncoder::new();
        let codeword = encoder.encode(&msg_bits).unwrap();
        assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);
        assert!(
            encoder.verify_syndrome(&codeword),
            "Syndrome check failed for '{}'",
            name
        );
    }
}

// ============================================================================
// LDPC codeword extraction from symbols (reverse direction)
// ============================================================================

#[test]
fn test_ldpc_codeword_validity_from_encoded_symbols() {
    let encoder = LdpcEncoder::new();
    let mut ft8_encoder = Ft8Encoder::new();

    let messages = [
        "CQ K1ABC FN42",
        "K1DEF W1ABC -12",
        "K1ABC W9XYZ RRR",
        "K1ABC W9XYZ 73",
        "K1ABC W9XYZ RR73",
    ];

    for msg in &messages {
        let symbols = ft8_encoder.encode_message(msg, None).unwrap();

        // Extract codeword from data symbols (reverse Gray code)
        let mut codeword = BitVec::with_capacity(LDPC_CODEWORD_BITS);
        for i_tone in 0..NUM_SYMBOLS {
            let is_data = (7..36).contains(&i_tone) || (43..72).contains(&i_tone);
            if !is_data {
                continue;
            }

            let binary_value = gray_to_binary(symbols[i_tone]);
            codeword.push((binary_value & 4) != 0);
            codeword.push((binary_value & 2) != 0);
            codeword.push((binary_value & 1) != 0);
        }

        assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);
        assert!(
            encoder.verify_syndrome(&codeword),
            "Syndrome check failed for '{}' — codeword extracted from symbols is invalid",
            msg
        );
    }
}

// ============================================================================
// /R and /P suffix encoding tests
// ============================================================================

#[test]
fn test_pack28_suffix_flags() {
    use pancetta_ft8::encoder::pack28;

    // /R suffix sets ip=1, base callsign packed normally
    let (n28_r, ip_r) = pack28("W1ABC/R").unwrap();
    assert_eq!(ip_r, 1, "W1ABC/R should have ip=1");

    // /P suffix sets ip=1, base callsign packed normally
    let (n28_p, ip_p) = pack28("W1ABC/P").unwrap();
    assert_eq!(ip_p, 1, "W1ABC/P should have ip=1");

    // Base callsign should be the same for /R and /P (same call)
    assert_eq!(n28_r, n28_p, "W1ABC/R and W1ABC/P should have the same n28");

    // Bare callsign should have ip=0
    let (n28_bare, ip_bare) = pack28("W1ABC").unwrap();
    assert_eq!(ip_bare, 0, "W1ABC should have ip=0");
    assert_eq!(
        n28_bare, n28_r,
        "Base callsign value should match with or without suffix"
    );
}

#[test]
fn test_suffix_messages_use_i3_1() {
    let mut encoder = Ft8Encoder::new();

    // Both /R and /P messages should use i3=1 (standard message type)
    for msg in &["K1DEF W1ABC/R FN42", "K1DEF W1ABC/P FN42"] {
        let symbols = encoder.encode_message(msg, None).unwrap();
        let payload = extract_payload_from_symbols(&symbols);

        // i3 is in the last 3 bits of the 77-bit payload (bits 74-76)
        // payload[9] has bits 72-79: xxxxxi3i3i3
        let i3 = (payload[9] >> 3) & 0x07;
        assert_eq!(i3, 1, "Message '{}' should have i3=1, got i3={}", msg, i3);
    }
}

#[test]
fn test_suffix_round_trip() {
    // Batch 35 (hb-219): both /R and /P encode with ip=1 (protocol-
    // identical). The renderer now defaults to /P (matching jt9 and
    // recovering 16 /P truths on hard-200). /R-only operators (contest
    // rovers, rare in the autonomous-personal-station profile) are
    // displayed as /P.
    //
    // /P round-trip: input → ip=1 → render as /P → exact match.
    let decoded = encode_and_decode("K1DEF W1ABC/P FN42");
    assert_eq!(decoded, "K1DEF W1ABC/P FN42");

    // /R round-trip: input → ip=1 → render as /P (protocol limitation).
    // Contest /R operators decode as /P; pancetta's autonomous-station
    // profile does not run contests, so this is an acceptable loss.
    let decoded = encode_and_decode("K1DEF W1ABC/R FN42");
    assert_eq!(
        decoded, "K1DEF W1ABC/P FN42",
        "/R decodes as /P (Batch 35 renderer change)"
    );
}

fn encode_and_decode(message: &str) -> String {
    use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Modulator, WINDOW_SAMPLES};

    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default().unwrap();

    let symbols = encoder.encode_message(message, None).unwrap();
    let mut audio = modulator.modulate_symbols(&symbols, 0.0).unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);

    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    let decoded = decoder.decode_window(&audio).unwrap();

    assert!(!decoded.is_empty(), "Failed to decode '{}'", message);
    decoded[0].text.clone()
}

// ============================================================================
// Helpers
// ============================================================================

/// Extract the 77-bit payload (as 10 bytes) from encoded symbols
///
/// Reverses: symbols → Gray decode → codeword bits → info bits[0..77] = payload
fn extract_payload_from_symbols(symbols: &[u8; NUM_SYMBOLS]) -> [u8; 10] {
    // Extract 174-bit codeword from data symbols
    let mut codeword_bits = Vec::with_capacity(LDPC_CODEWORD_BITS);
    for i_tone in 0..NUM_SYMBOLS {
        let is_data = (7..36).contains(&i_tone) || (43..72).contains(&i_tone);
        if !is_data {
            continue;
        }

        let binary_value = gray_to_binary(symbols[i_tone]);
        codeword_bits.push((binary_value & 4) != 0);
        codeword_bits.push((binary_value & 2) != 0);
        codeword_bits.push((binary_value & 1) != 0);
    }

    assert_eq!(codeword_bits.len(), LDPC_CODEWORD_BITS);

    // First 91 bits = info bits. First 77 of those = payload.
    let mut payload = [0u8; 10];
    for i in 0..PAYLOAD_BITS {
        if codeword_bits[i] {
            payload[i / 8] |= 0x80u8 >> (i % 8);
        }
    }

    payload
}
