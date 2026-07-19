//! Human-comparable fingerprint-word rendering, per the locked
//! `contracts/identity/fingerprint-words.v1.json` (dispensa Q-0025, CONCUR
//! 2026-07-04). Both ends of a pairing/admission TOFU ceremony must render
//! IDENTICAL words for the same key bytes so an operator can compare them
//! across two screens — table order, spellings, bitstream direction, and
//! word count are locked in that contract; this is the pancetta side of it
//! (dispensa Q-0039).

use base64::Engine as _;

/// Errors from fingerprint-word rendering.
#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    /// The input did not decode as base64url.
    #[error("fingerprint input is not valid base64url: {0}")]
    Base64(String),
    /// Decoded input was shorter than the 8-byte floor the contract requires
    /// regardless of `count` (so short/malformed keys fail loudly rather
    /// than silently truncating).
    #[error("fingerprint input too short: got {got} bytes, need at least {need}")]
    TooShort { got: usize, need: usize },
}

const MIN_BYTES: usize = 8;

/// The locked 32-entry table: 26 ICAO phonetic letters (ICAO spellings —
/// ALFA/JULIETT/XRAY, not Alpha/Juliet/X-ray) followed by the 6 ICAO/FAA
/// spoken-figure words (ZERO/WUN/TOO/TREE/FOWER/FIFE), addressable by 5 bits.
/// Index 0 = ALFA … index 31 = FIFE. Order is part of the contract — do not
/// reorder.
const WORDS: [&str; 32] = [
    "ALFA", "BRAVO", "CHARLIE", "DELTA", "ECHO", "FOXTROT", "GOLF", "HOTEL", "INDIA", "JULIETT",
    "KILO", "LIMA", "MIKE", "NOVEMBER", "OSCAR", "PAPA", "QUEBEC", "ROMEO", "SIERRA", "TANGO",
    "UNIFORM", "VICTOR", "WHISKEY", "XRAY", "YANKEE", "ZULU", "ZERO", "WUN", "TOO", "TREE",
    "FOWER", "FIFE",
];

/// Render `count` words from a keyId (unpadded base64url, decoded directly
/// to bytes — no hashing). This is the ceremony's primary comparison: the
/// same rendering both ends of a pairing must independently reproduce for
/// the same `agentKeyId`.
pub fn fingerprint_words(
    key_id_b64url: &str,
    count: usize,
) -> Result<Vec<&'static str>, FingerprintError> {
    let bytes = decode_b64url(key_id_b64url)?;
    words_from_bytes(&bytes, count)
}

/// Render `count` words from raw key material (unpadded base64url, decoded
/// then SHA-256'd to 32 bytes) rather than a keyId. Used when comparing two
/// keys that could share one `agentKeyId` under a TOFU mismatch (dispensa
/// Q-0020) — hashing the raw key first means the rendering changes even if
/// the wrapping keyId didn't.
pub fn key_material_words(
    key_b64url: &str,
    count: usize,
) -> Result<Vec<&'static str>, FingerprintError> {
    use sha2::{Digest, Sha256};
    let bytes = decode_b64url(key_b64url)?;
    let digest = Sha256::digest(&bytes);
    words_from_bytes(&digest, count)
}

/// Decode unpadded base64url into bytes.
fn decode_b64url(input: &str) -> Result<Vec<u8>, FingerprintError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|e| FingerprintError::Base64(e.to_string()))
}

/// Slice `count` 5-bit fields out of `bytes` as a big-endian bitstream
/// (MSB-first, NOT byte-aligned — words straddle byte boundaries except
/// when `i*5` is a multiple of 8), each indexing into `WORDS`. Matches the
/// contract's reference bit-slicing implementation exactly: for word `i`,
/// `byteIdx = (i*5) >> 3`, `off = (i*5) & 7`, combine `bytes[byteIdx]` and
/// `bytes[byteIdx+1]` (or 0 past the end) into a 16-bit big-endian value,
/// then `(combined >> (11 - off)) & 31`.
fn words_from_bytes(bytes: &[u8], count: usize) -> Result<Vec<&'static str>, FingerprintError> {
    if bytes.len() < MIN_BYTES {
        return Err(FingerprintError::TooShort {
            got: bytes.len(),
            need: MIN_BYTES,
        });
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit_offset = i * 5;
        let byte_idx = bit_offset >> 3;
        let off = bit_offset & 7;
        let hi = bytes.get(byte_idx).copied().unwrap_or(0) as u16;
        let lo = bytes.get(byte_idx + 1).copied().unwrap_or(0) as u16;
        let combined = (hi << 8) | lo;
        let idx = ((combined >> (11 - off)) & 31) as usize;
        out.push(WORDS[idx]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors vendored verbatim from `contracts/identity/fingerprint-words.v1.json`
    // (dispensa), which itself vendors them from panino's drift-guarded test
    // suite (`fingerprint-words.test.ts`, 2026-07-03) — byte-verified against
    // the reference TypeScript implementation.

    #[test]
    fn fingerprint_words_vector_1() {
        let words = fingerprint_words("YW3jpdl--AnmUR207kFW_kQCgLd4GCWws1SQNnMDsiw", 12).unwrap();
        assert_eq!(
            words,
            vec![
                "MIKE", "FOXTROT", "WHISKEY", "FOWER", "HOTEL", "JULIETT", "OSCAR", "ZULU", "PAPA",
                "WUN", "TOO", "ALFA"
            ]
        );
    }

    #[test]
    fn fingerprint_words_vector_2() {
        let words = fingerprint_words("YidrPvxMK2x4hUXj1K3WQeB0Dqkhj1YcQm9dX_e1NhU", 12).unwrap();
        assert_eq!(
            words,
            vec![
                "MIKE", "INDIA", "TANGO", "WHISKEY", "WHISKEY", "PAPA", "XRAY", "TOO", "JULIETT",
                "QUEBEC", "VICTOR", "WHISKEY"
            ]
        );
    }

    #[test]
    fn fingerprint_words_all_zero_bytes_is_all_alfa() {
        let words = fingerprint_words("AAAAAAAAAAA", 12).unwrap();
        assert_eq!(words, vec!["ALFA"; 12]);
    }

    #[test]
    fn fingerprint_words_all_one_bytes_is_all_fife() {
        let words = fingerprint_words("__________8", 12).unwrap();
        assert_eq!(words, vec!["FIFE"; 12]);
    }

    #[test]
    fn key_material_words_vector_1() {
        let words = key_material_words("ERERERERERERERERERERERERERERERERERERERERERE", 12).unwrap();
        assert_eq!(
            words,
            vec![
                "ALFA", "LIMA", "KILO", "ECHO", "TANGO", "INDIA", "YANKEE", "FIFE", "XRAY", "MIKE",
                "TANGO", "HOTEL"
            ]
        );
    }

    #[test]
    fn key_material_words_vector_2() {
        let words = key_material_words("IiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiI", 12).unwrap();
        assert_eq!(
            words,
            vec![
                "TANGO", "TREE", "ZULU", "OSCAR", "UNIFORM", "DELTA", "HOTEL", "UNIFORM", "SIERRA",
                "UNIFORM", "WUN", "OSCAR"
            ]
        );
    }

    #[test]
    fn fewer_words_is_a_prefix_of_more_words() {
        let full = fingerprint_words("YW3jpdl--AnmUR207kFW_kQCgLd4GCWws1SQNnMDsiw", 12).unwrap();
        let short = fingerprint_words("YW3jpdl--AnmUR207kFW_kQCgLd4GCWws1SQNnMDsiw", 8).unwrap();
        assert_eq!(short, &full[..8]);
    }

    #[test]
    fn too_short_input_fails_loudly() {
        // "AAAAAAA" (7 'A's) decodes to fewer than 8 bytes.
        let err = fingerprint_words("AAAAAAA", 12).unwrap_err();
        assert!(matches!(err, FingerprintError::TooShort { .. }));
    }

    #[test]
    fn invalid_base64_fails_loudly() {
        let err = fingerprint_words("not valid base64url!!", 12).unwrap_err();
        assert!(matches!(err, FingerprintError::Base64(_)));
    }
}
