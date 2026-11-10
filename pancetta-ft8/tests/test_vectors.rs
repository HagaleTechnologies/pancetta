//! Reference test vectors for FT8 encoder
//!
//! Deterministic regression tests to catch encoder output changes.
//! Uses self-referential golden tests since WSJT-X reference vectors
//! are not yet available.

#![cfg(feature = "transmit")]

use bitvec::prelude::*;
use pancetta_ft8::ldpc::{binary_to_gray, gray_to_binary, LdpcEncoder, LDPC_CODEWORD_BITS};
use pancetta_ft8::message::{calculate_crc14, PAYLOAD_BITS};
use pancetta_ft8::{Ft8Encoder, NUM_SYMBOLS};

/// FT8 Costas array that must appear at positions 0-6, 36-42, 72-78
const COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

// =============================================================
// Encoder Determinism Tests
// =============================================================

#[test]
fn test_encoder_determinism() {
    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "HELLO WORLD",
        "CQ DX W1ABC FN42",
        "W1ABC K1DEF FN41",
    ];

    for msg in &messages {
        let mut encoder1 = Ft8Encoder::new();
        let mut encoder2 = Ft8Encoder::new();

        let symbols1 = encoder1.encode_message(msg, None).unwrap();
        let symbols2 = encoder2.encode_message(msg, None).unwrap();

        assert_eq!(
            symbols1, symbols2,
            "Encoding of '{}' is not deterministic across encoder instances",
            msg
        );

        // Also verify repeated encoding on the same instance
        let symbols3 = encoder1.encode_message(msg, None).unwrap();
        assert_eq!(
            symbols1, symbols3,
            "Encoding of '{}' not stable across calls",
            msg
        );
    }
}

// =============================================================
// CRC-14 Known Values
// =============================================================

#[test]
fn test_crc14_known_values() {
    // All-zeros payload → deterministic CRC
    let zeros = bitvec![0; PAYLOAD_BITS];
    let crc_zeros = calculate_crc14(&zeros);
    assert!(
        crc_zeros < (1 << 14),
        "CRC should be 14-bit: got {}",
        crc_zeros
    );

    // All-ones payload → deterministic (and different) CRC
    let ones = bitvec![1; PAYLOAD_BITS];
    let crc_ones = calculate_crc14(&ones);
    assert!(
        crc_ones < (1 << 14),
        "CRC should be 14-bit: got {}",
        crc_ones
    );

    // Different payloads → different CRCs
    assert_ne!(
        crc_zeros, crc_ones,
        "Different payloads should have different CRCs"
    );

    // CRC is deterministic
    assert_eq!(
        calculate_crc14(&zeros),
        crc_zeros,
        "CRC should be deterministic"
    );
    assert_eq!(
        calculate_crc14(&ones),
        crc_ones,
        "CRC should be deterministic"
    );

    // Record values for regression (if these change, the CRC implementation changed)
    println!("CRC(all-zeros) = {:#06x}", crc_zeros);
    println!("CRC(all-ones)  = {:#06x}", crc_ones);
}

#[test]
fn test_crc14_single_bit_sensitivity() {
    // Flipping any single bit in the payload should change the CRC
    let base = bitvec![0; PAYLOAD_BITS];
    let base_crc = calculate_crc14(&base);

    for bit_idx in 0..PAYLOAD_BITS {
        let mut modified = base.clone();
        modified.set(bit_idx, true);
        let modified_crc = calculate_crc14(&modified);

        assert_ne!(
            base_crc, modified_crc,
            "CRC unchanged when flipping bit {}",
            bit_idx
        );
    }
}

// =============================================================
// LDPC Codeword Validity Tests
// =============================================================

#[test]
fn test_encoder_produces_valid_codewords() {
    let encoder = LdpcEncoder::new();
    let mut ft8_encoder = Ft8Encoder::new();

    let messages = [
        "CQ W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "W1ABC K1DEF FN41",
        "HELLO WORLD",
        "CQ DX W1ABC FN42",
    ];

    for msg in &messages {
        let symbols = ft8_encoder.encode_message(msg, None).unwrap();

        // Extract codeword bits from symbols:
        // Reverse Gray code, then extract 3 bits per data symbol
        // Data symbols are at positions 7..36 and 43..72
        let mut codeword = BitVec::with_capacity(LDPC_CODEWORD_BITS);

        let data_positions: Vec<usize> = (7..36).chain(43..72).collect();
        assert_eq!(data_positions.len(), 58);

        for &pos in &data_positions {
            let gray_value = symbols[pos];
            let binary_value = gray_to_binary(gray_value);
            codeword.push((binary_value & 4) != 0);
            codeword.push((binary_value & 2) != 0);
            codeword.push((binary_value & 1) != 0);
        }

        assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);

        assert!(
            encoder.verify_syndrome(&codeword),
            "Syndrome check failed for message '{}'",
            msg
        );
    }
}

// =============================================================
// Costas Array Position Tests
// =============================================================

#[test]
fn test_costas_array_positions() {
    let mut encoder = Ft8Encoder::new();

    let messages = ["CQ W1ABC FN42", "K1DEF W1ABC -12", "HELLO WORLD"];

    for msg in &messages {
        let symbols = encoder.encode_message(msg, None).unwrap();

        // Verify Costas arrays at all three positions
        assert_eq!(
            &symbols[0..7],
            &COSTAS,
            "First Costas array wrong for '{}'",
            msg
        );
        assert_eq!(
            &symbols[36..43],
            &COSTAS,
            "Second Costas array wrong for '{}'",
            msg
        );
        assert_eq!(
            &symbols[72..79],
            &COSTAS,
            "Third Costas array wrong for '{}'",
            msg
        );
    }
}

#[test]
fn test_symbol_layout_structure() {
    let mut encoder = Ft8Encoder::new();
    let symbols = encoder.encode_message("CQ W1ABC FN42", None).unwrap();

    assert_eq!(symbols.len(), NUM_SYMBOLS);
    assert_eq!(NUM_SYMBOLS, 79);

    // Verify total structure: 7 + 29 + 7 + 29 + 7 = 79
    let sync1 = &symbols[0..7];
    let data1 = &symbols[7..36];
    let sync2 = &symbols[36..43];
    let data2 = &symbols[43..72];
    let sync3 = &symbols[72..79];

    assert_eq!(sync1.len(), 7);
    assert_eq!(data1.len(), 29);
    assert_eq!(sync2.len(), 7);
    assert_eq!(data2.len(), 29);
    assert_eq!(sync3.len(), 7);

    // Total data symbols × 3 bits = 174 = LDPC codeword length
    assert_eq!((data1.len() + data2.len()) * 3, LDPC_CODEWORD_BITS);
}

// =============================================================
// Gray Code Consistency Tests
// =============================================================

#[test]
fn test_gray_code_round_trip_all_values() {
    // FT8 Gray code only covers values 0-7
    for b in 0..8u8 {
        let gray = binary_to_gray(b);
        let back = gray_to_binary(gray);
        assert_eq!(b, back, "Round-trip failed for {}", b);
    }
}

#[test]
fn test_gray_code_3bit_sequence() {
    // FT8 uses kFT8_Gray_map from ft8_lib (NOT standard binary-reflected Gray code)
    let expected = [0u8, 1, 3, 2, 5, 6, 4, 7];

    for (binary, &gray) in expected.iter().enumerate() {
        assert_eq!(
            binary_to_gray(binary as u8),
            gray,
            "binary_to_gray({}) expected {:03b} got {:03b}",
            binary,
            gray,
            binary_to_gray(binary as u8)
        );
    }
}
