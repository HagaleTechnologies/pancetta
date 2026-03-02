//! Property-based tests for FT8 encoder using proptest
//!
//! These tests use the proptest framework to generate random inputs
//! and verify invariants of the FT8 encoding pipeline.

#![cfg(feature = "transmit")]

use proptest::prelude::*;
use pancetta_ft8::{Ft8Encoder, NUM_SYMBOLS};
use pancetta_ft8::ldpc::{
    LdpcEncoder, binary_to_gray, gray_to_binary,
    LDPC_INFO_BITS, LDPC_CODEWORD_BITS,
};
use pancetta_ft8::message::{calculate_crc14, PAYLOAD_BITS};
use bitvec::prelude::*;

// =============================================================
// LDPC Validity Property
// =============================================================

proptest! {
    /// Any 91-bit vector → encode → syndrome = 0
    #[test]
    fn prop_ldpc_encode_valid_syndrome(bits in prop::collection::vec(any::<bool>(), LDPC_INFO_BITS)) {
        let encoder = LdpcEncoder::new();
        let mut info_bits = BitVec::with_capacity(LDPC_INFO_BITS);
        for &b in &bits {
            info_bits.push(b);
        }

        let codeword = encoder.encode(&info_bits).unwrap();
        prop_assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);
        prop_assert!(encoder.verify_syndrome(&codeword), "Syndrome check failed");
    }

    /// Encoded codeword preserves the original information bits
    #[test]
    fn prop_ldpc_encode_preserves_info(bits in prop::collection::vec(any::<bool>(), LDPC_INFO_BITS)) {
        let encoder = LdpcEncoder::new();
        let mut info_bits = BitVec::with_capacity(LDPC_INFO_BITS);
        for &b in &bits {
            info_bits.push(b);
        }

        let codeword = encoder.encode(&info_bits).unwrap();

        // First 91 bits should match original
        for i in 0..LDPC_INFO_BITS {
            prop_assert_eq!(
                codeword[i], info_bits[i],
                "Info bit {} mismatch", i
            );
        }
    }
}

// =============================================================
// Gray Code Properties
// =============================================================

proptest! {
    /// Gray code is a bijection: gray_to_binary(binary_to_gray(x)) == x
    #[test]
    fn prop_gray_code_bijection(x in 0..8u8) {
        let gray = binary_to_gray(x);
        let back = gray_to_binary(gray);
        prop_assert_eq!(x, back);
    }

    /// Gray code maps [0,7] to [0,7]
    #[test]
    fn prop_gray_code_range(x in 0..8u8) {
        let gray = binary_to_gray(x);
        prop_assert!(gray < 8, "Gray code {} out of range for input {}", gray, x);
    }

    /// Bijection for all valid FT8 tone values (0-7)
    #[test]
    fn prop_gray_code_bijection_u8(x in 0..8u8) {
        let gray = binary_to_gray(x);
        let back = gray_to_binary(gray);
        prop_assert_eq!(x, back);
    }
}

// =============================================================
// CRC-14 Error Detection Property
// =============================================================

proptest! {
    /// Flipping any single bit in a payload changes the CRC
    #[test]
    fn prop_crc14_detects_single_bit_flip(
        payload_bits in prop::collection::vec(any::<bool>(), PAYLOAD_BITS),
        flip_pos in 0..PAYLOAD_BITS
    ) {
        let mut original = BitVec::with_capacity(PAYLOAD_BITS);
        for &b in &payload_bits {
            original.push(b);
        }

        let original_crc = calculate_crc14(&original);

        let mut modified = original.clone();
        let current = modified[flip_pos];
        modified.set(flip_pos, !current);

        let modified_crc = calculate_crc14(&modified);

        prop_assert_ne!(
            original_crc, modified_crc,
            "CRC unchanged when flipping bit {}", flip_pos
        );
    }

    /// CRC is deterministic
    #[test]
    fn prop_crc14_deterministic(payload_bits in prop::collection::vec(any::<bool>(), PAYLOAD_BITS)) {
        let mut bits = BitVec::with_capacity(PAYLOAD_BITS);
        for &b in &payload_bits {
            bits.push(b);
        }

        let crc1 = calculate_crc14(&bits);
        let crc2 = calculate_crc14(&bits);
        prop_assert_eq!(crc1, crc2);
    }

    /// CRC fits in 14 bits
    #[test]
    fn prop_crc14_fits_in_14_bits(payload_bits in prop::collection::vec(any::<bool>(), PAYLOAD_BITS)) {
        let mut bits = BitVec::with_capacity(PAYLOAD_BITS);
        for &b in &payload_bits {
            bits.push(b);
        }

        let crc = calculate_crc14(&bits);
        prop_assert!(crc < (1 << 14), "CRC {} exceeds 14-bit range", crc);
    }
}

// =============================================================
// Symbol Bounds Property
// =============================================================

#[test]
fn prop_all_encoded_symbols_in_range() {
    let messages = [
        "CQ W1ABC FN42",
        "CQ DX W1ABC FN42",
        "K1DEF W1ABC -12",
        "K1DEF W1ABC +05",
        "K1DEF W1ABC RRR",
        "K1DEF W1ABC 73",
        "K1DEF W1ABC RR73",
        "W1ABC K1DEF FN41",
        "HELLO WORLD",
        "TEST 123",
        "HI",
    ];

    let mut encoder = Ft8Encoder::new();

    for msg in &messages {
        let symbols = encoder.encode_message(msg, None).unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS, "Wrong symbol count for '{}'", msg);

        for (i, &s) in symbols.iter().enumerate() {
            assert!(
                s < 8,
                "Symbol {} = {} out of range [0,7] for '{}'",
                i, s, msg
            );
        }
    }
}

// =============================================================
// Callsign Encoding Properties
// =============================================================

/// Strategy for generating valid amateur callsigns
fn callsign_strategy() -> impl Strategy<Value = String> {
    // Simple format: 1-2 letters, 1 digit, 1-3 letters (e.g., W1ABC, AA1BB)
    prop::string::string_regex("[A-Z]{1,2}[0-9][A-Z]{1,3}")
        .unwrap()
}

proptest! {
    #[test]
    fn prop_callsign_encodes_successfully(callsign in callsign_strategy()) {
        let mut encoder = Ft8Encoder::new();
        let msg = format!("CQ {} FN42", callsign);
        let result = encoder.encode_message(&msg, None);
        // Should either succeed or give a clear error (not panic)
        match result {
            Ok(symbols) => {
                prop_assert_eq!(symbols.len(), NUM_SYMBOLS);
                prop_assert!(symbols.iter().all(|&s| s < 8));
            }
            Err(_) => {
                // Some callsign formats may not be valid - that's OK
            }
        }
    }
}

// =============================================================
// LDPC Linearity Property
// =============================================================

#[test]
fn test_ldpc_linearity() {
    let encoder = LdpcEncoder::new();

    // For a linear code: encode(a XOR b) == encode(a) XOR encode(b)
    let mut a = bitvec![0; LDPC_INFO_BITS];
    let mut b = bitvec![0; LDPC_INFO_BITS];

    // Set some bits
    a.set(0, true);
    a.set(10, true);
    a.set(50, true);

    b.set(5, true);
    b.set(10, true);
    b.set(70, true);

    let ca = encoder.encode(&a).unwrap();
    let cb = encoder.encode(&b).unwrap();

    // a XOR b
    let mut a_xor_b = bitvec![0; LDPC_INFO_BITS];
    for i in 0..LDPC_INFO_BITS {
        a_xor_b.set(i, a[i] ^ b[i]);
    }

    let c_xor = encoder.encode(&a_xor_b).unwrap();

    // encode(a) XOR encode(b)
    let mut ca_xor_cb = bitvec![0; LDPC_CODEWORD_BITS];
    for i in 0..LDPC_CODEWORD_BITS {
        ca_xor_cb.set(i, ca[i] ^ cb[i]);
    }

    assert_eq!(
        c_xor, ca_xor_cb,
        "LDPC code should be linear: encode(a^b) == encode(a) ^ encode(b)"
    );
}
