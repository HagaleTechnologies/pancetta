//! FT8 message encoding implementation (WSJT-X compatible)
//!
//! This module handles encoding of text messages into FT8 protocol format,
//! producing output bit-compatible with WSJT-X / ft8_lib.
//!
//! Encoding pipeline:
//! 1. Parse message text → structured fields
//! 2. Pack fields into 77-bit payload (i3 at bits 74-76)
//! 3. Calculate CRC-14 checksum → 91-bit message
//! 4. LDPC encode → 174-bit codeword
//! 5. Map to 79 symbols via Gray code + Costas sync arrays

use crate::message::{PAYLOAD_BITS, CRC_BITS, calculate_crc14, NUM_SYMBOLS};
use crate::ldpc::{LdpcEncoder, binary_to_gray};
use crate::{Ft8Error, Ft8Result};
use bitvec::prelude::*;
use serde::{Deserialize, Serialize};

/// Maximum length for free text messages
pub const MAX_FREETEXT_LENGTH: usize = 13;

/// Maximum signal report value in dB (WSJT-X limit: MAXGRID4 + 35 + dd < 2^15)
pub const MAX_SIGNAL_REPORT: i8 = 30;

/// Minimum signal report value in dB (must satisfy 35 + dd >= 0)
pub const MIN_SIGNAL_REPORT: i8 = -35;

/// WSJT-X constants for callsign encoding
const NTOKENS: u32 = 2_063_592;
const MAX22: u32 = 4_194_304;
const MAXGRID4: u16 = 32400;

/// FT8 Costas synchronization array (same at all three positions)
const COSTAS_ARRAY: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

/// Free text character table (42 chars): " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?"
const FREETEXT_CHARS: &[u8; 42] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";

/// FT8 message encoder for generating transmission-ready symbols
pub struct Ft8Encoder {
    /// LDPC encoder for error correction
    ldpc_encoder: LdpcEncoder,
}

impl Ft8Encoder {
    /// Create a new FT8 encoder
    pub fn new() -> Self {
        Self {
            ldpc_encoder: LdpcEncoder::new(),
        }
    }

    /// Encode a text message into FT8 transmission symbols
    ///
    /// # Arguments
    /// * `message_text` - Text message to encode (e.g., "CQ W1ABC FN42")
    /// * `_transmit_power` - Transmit power for contest exchanges (unused, reserved)
    ///
    /// # Returns
    /// Array of 79 symbol values (0-7) ready for transmission
    pub fn encode_message(&mut self, message_text: &str, _transmit_power: Option<u8>) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        // Normalize: uppercase, collapse whitespace
        let text = message_text.to_uppercase();
        let text = text.trim();

        // Try standard message encoding first
        if let Ok(payload) = self.try_encode_standard(text) {
            return self.payload_to_symbols(&payload);
        }

        // Fall back to free text encoding
        if let Ok(payload) = self.encode_free_text(text) {
            return self.payload_to_symbols(&payload);
        }

        Err(Ft8Error::MessageDecodingError(
            format!("Cannot encode message: '{}'", message_text)
        ))
    }

    /// Encode standard CQ message: "CQ [DX] <callsign> <grid>"
    pub fn encode_cq(&mut self, callsign: &str, grid_square: &str, dx_call: bool) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = if dx_call {
            format!("CQ DX {} {}", callsign, grid_square)
        } else {
            format!("CQ {} {}", callsign, grid_square)
        };
        self.encode_message(&message_text, None)
    }

    /// Encode response message: "<to_call> <from_call> <grid>"
    pub fn encode_response(&mut self, to_callsign: &str, from_callsign: &str, grid_square: &str) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} {}", to_callsign, from_callsign, grid_square);
        self.encode_message(&message_text, None)
    }

    /// Encode signal report: "<to_call> <from_call> <report>"
    pub fn encode_signal_report(&mut self, to_callsign: &str, from_callsign: &str, report_db: i8) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if report_db < MIN_SIGNAL_REPORT || report_db > MAX_SIGNAL_REPORT {
            return Err(Ft8Error::MessageDecodingError(
                format!("Signal report {} dB out of range ({} to {})", report_db, MIN_SIGNAL_REPORT, MAX_SIGNAL_REPORT)
            ));
        }
        let message_text = format!("{} {} {:+03}", to_callsign, from_callsign, report_db);
        self.encode_message(&message_text, None)
    }

    /// Encode acknowledgment: "<to_call> <from_call> RRR"
    pub fn encode_rrr(&mut self, to_callsign: &str, from_callsign: &str) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} RRR", to_callsign, from_callsign);
        self.encode_message(&message_text, None)
    }

    /// Encode final 73: "<to_call> <from_call> 73"
    pub fn encode_73(&mut self, to_callsign: &str, from_callsign: &str) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} 73", to_callsign, from_callsign);
        self.encode_message(&message_text, None)
    }

    /// Encode free text message (max 13 characters)
    pub fn encode_freetext(&mut self, text: &str) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if text.len() > MAX_FREETEXT_LENGTH {
            return Err(Ft8Error::MessageDecodingError(
                format!("Free text message too long: {} characters (max {})", text.len(), MAX_FREETEXT_LENGTH)
            ));
        }
        let text = text.to_uppercase();
        let payload = self.encode_free_text(&text)?;
        self.payload_to_symbols(&payload)
    }

    // ========================================================================
    // Core encoding pipeline
    // ========================================================================

    /// Convert 77-bit payload to 79 transmission symbols
    fn payload_to_symbols(&self, payload: &[u8; 10]) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        // Add CRC-14 to get 91 bits
        let mut payload_bitvec = BitVec::with_capacity(PAYLOAD_BITS);
        for i in 0..PAYLOAD_BITS {
            payload_bitvec.push(payload[i / 8] & (0x80u8 >> (i % 8)) != 0);
        }

        let crc = calculate_crc14(&payload_bitvec);

        // Build 91-bit message: 77 payload + 14 CRC
        let mut message_bits = BitVec::with_capacity(PAYLOAD_BITS + CRC_BITS);
        message_bits.extend_from_bitslice(&payload_bitvec);
        for i in (0..CRC_BITS).rev() {
            message_bits.push((crc >> i) & 1 != 0);
        }

        // LDPC encode (91 → 174 bits)
        let ldpc_codeword = self.ldpc_encoder.encode(&message_bits)?;

        // Generate symbols
        self.generate_symbols(&ldpc_codeword)
    }

    /// Generate 79-symbol sequence from LDPC codeword
    ///
    /// FT8 symbol layout: S7 D29 S7 D29 S7
    fn generate_symbols(&self, ldpc_codeword: &BitSlice) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if ldpc_codeword.len() != 174 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid LDPC codeword length: {}", ldpc_codeword.len())
            ));
        }

        let mut symbols = [0u8; NUM_SYMBOLS];
        let mut bit_idx = 0usize;

        for i_tone in 0..NUM_SYMBOLS {
            if i_tone < 7 {
                symbols[i_tone] = COSTAS_ARRAY[i_tone];
            } else if (36..43).contains(&i_tone) {
                symbols[i_tone] = COSTAS_ARRAY[i_tone - 36];
            } else if i_tone >= 72 {
                symbols[i_tone] = COSTAS_ARRAY[i_tone - 72];
            } else {
                // Extract 3 bits, apply Gray code mapping
                let mut bits3 = 0u8;
                if ldpc_codeword[bit_idx] { bits3 |= 4; }
                if ldpc_codeword[bit_idx + 1] { bits3 |= 2; }
                if ldpc_codeword[bit_idx + 2] { bits3 |= 1; }
                bit_idx += 3;

                symbols[i_tone] = binary_to_gray(bits3);
            }
        }

        Ok(symbols)
    }

    // ========================================================================
    // Standard message encoding (i3=1)
    // ========================================================================

    /// Try to encode as a standard FT8 message (Type 1)
    ///
    /// Standard message layout (77 bits):
    ///   n29a (28+1) + n29b (28+1) + R1 (1) + igrid4 (15) + i3 (3)
    fn try_encode_standard(&self, text: &str) -> Ft8Result<[u8; 10]> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.is_empty() {
            return Err(Ft8Error::MessageDecodingError("Empty message".to_string()));
        }

        // Parse: call_to, call_de, extra
        let (call_to, call_de, extra) = self.parse_standard_message(&parts)?;

        // Pack callsigns
        let (n28a, ipa) = pack28(&call_to)?;
        let (n28b, ipb) = pack28(&call_de)?;

        // Pack grid/report/token
        let igrid4 = packgrid(&extra);

        // i3=1 for all standard messages (including /R and /P suffixes)
        let i3: u8 = 1;

        // Build n29a and n29b (28-bit callsign + 1-bit suffix flag)
        let n29a: u32 = (n28a << 1) | (ipa as u32);
        let n29b: u32 = (n28b << 1) | (ipb as u32);

        // Extract ir bit from igrid4 (bit 15 = R prefix indicator)
        let ir: u8 = if igrid4 & 0x8000 != 0 { 1 } else { 0 };
        let igrid4_val: u16 = igrid4 & 0x7FFF;

        // Pack into 10 bytes: n29a(29) + n29b(29) + ir(1) + igrid4(15) + i3(3) = 77 bits
        let mut payload = [0u8; 10];
        payload[0] = (n29a >> 21) as u8;
        payload[1] = (n29a >> 13) as u8;
        payload[2] = (n29a >> 5) as u8;
        payload[3] = ((n29a << 3) as u8) | ((n29b >> 26) as u8);
        payload[4] = (n29b >> 18) as u8;
        payload[5] = (n29b >> 10) as u8;
        payload[6] = (n29b >> 2) as u8;
        payload[7] = ((n29b << 6) as u8) | (ir << 5) | ((igrid4_val >> 10) as u8);
        payload[8] = (igrid4_val >> 2) as u8;
        payload[9] = ((igrid4_val << 6) as u8) | (i3 << 3);

        Ok(payload)
    }

    /// Parse standard message text into (call_to, call_de, extra) fields
    fn parse_standard_message<'a>(&self, parts: &[&'a str]) -> Ft8Result<(String, String, String)> {
        if parts.is_empty() {
            return Err(Ft8Error::MessageDecodingError("Empty message".to_string()));
        }

        let is_cq = parts[0] == "CQ";

        if is_cq {
            // CQ [modifier] <callsign> [grid]
            let mut idx = 1;
            let mut call_to = String::from("CQ");

            // Check for CQ modifier (DX, nnn, or letter sequence)
            if parts.len() > idx {
                let next = parts[idx];
                if is_cq_modifier(next) {
                    call_to = format!("CQ {}", next);
                    idx += 1;
                }
            }

            let call_de = if parts.len() > idx {
                parts[idx].to_string()
            } else {
                return Err(Ft8Error::MessageDecodingError("CQ message missing callsign".to_string()));
            };
            idx += 1;

            let extra = if parts.len() > idx {
                parts[idx].to_string()
            } else {
                String::new()
            };

            Ok((call_to, call_de, extra))
        } else {
            // <to_call> <from_call> [grid/report/token]
            if parts.len() < 2 {
                return Err(Ft8Error::MessageDecodingError(
                    "Standard message needs at least 2 callsigns".to_string()
                ));
            }

            let call_to = parts[0].to_string();
            let call_de = parts[1].to_string();
            let extra = if parts.len() > 2 {
                parts[2].to_string()
            } else {
                String::new()
            };

            Ok((call_to, call_de, extra))
        }
    }

    // ========================================================================
    // Free text encoding (i3=0, n3=0)
    // ========================================================================

    /// Encode free text message using base-42 multi-precision encoding
    ///
    /// WSJT-X compatible: 13 characters × base-42 → 71 bits,
    /// shifted left by 1, then i3=0/n3=0 in bits 71-76.
    fn encode_free_text(&self, text: &str) -> Ft8Result<[u8; 10]> {
        if text.len() > MAX_FREETEXT_LENGTH {
            return Err(Ft8Error::MessageDecodingError(
                format!("Free text too long: {} (max {})", text.len(), MAX_FREETEXT_LENGTH)
            ));
        }

        // Encode 13 characters into 9-byte big integer using base-42
        let mut b71 = [0u8; 9];

        for idx in 0..13 {
            let ch = if idx < text.len() {
                text.as_bytes()[idx]
            } else {
                b' '
            };

            let cid = freetext_char_index(ch)?;

            // Multiply b71 by 42 and add cid (multi-precision arithmetic)
            let mut rem = cid as u16;
            for i in (0..9).rev() {
                rem += (b71[i] as u16) * 42;
                b71[i] = (rem & 0xFF) as u8;
                rem >>= 8;
            }
        }

        // Shift b71 left by 1 bit (telemetry encoding format)
        let mut payload = [0u8; 10];
        let mut carry: u8 = 0;
        for i in (0..9).rev() {
            payload[i] = (b71[i] << 1) | carry;
            carry = b71[i] >> 7;
        }
        // payload[9] = 0 — i3=0, n3=0 for free text

        Ok(payload)
    }
}

impl Default for Ft8Encoder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// WSJT-X pack28: callsign → 28-bit integer
// ============================================================================

/// Pack a callsign (or special token) into a 28-bit integer.
///
/// Returns (n28, ip) where ip is the suffix flag (1 for /R or /P).
///
/// Encoding scheme (from WSJT-X):
/// - DE → 0, QRZ → 1, CQ → 2
/// - CQ nnn → 3 + nnn
/// - CQ ABCD → 3 + 1000 + base-27 value
/// - Standard callsign → NTOKENS + MAX22 + basecall_value
/// - Non-standard → error (hash not supported without table)
pub fn pack28(callsign: &str) -> Ft8Result<(u32, u8)> {
    let mut ip: u8 = 0;

    // Special tokens
    if callsign == "DE" { return Ok((0, 0)); }
    if callsign == "QRZ" { return Ok((1, 0)); }
    if callsign == "CQ" { return Ok((2, 0)); }

    // CQ with modifier
    if callsign.starts_with("CQ ") && callsign.len() < 8 {
        let modifier = &callsign[3..];
        if let Some(v) = parse_cq_modifier(modifier) {
            return Ok((3 + v, 0));
        }
        return Err(Ft8Error::MessageDecodingError(
            format!("Invalid CQ modifier: {}", modifier)
        ));
    }

    // Detect /R or /P suffix
    let base_callsign = if callsign.ends_with("/P") || callsign.ends_with("/R") {
        ip = 1;
        &callsign[..callsign.len() - 2]
    } else {
        callsign
    };

    // Try standard basecall encoding
    if let Some(n28) = pack_basecall(base_callsign) {
        return Ok((NTOKENS + MAX22 + n28, ip));
    }

    Err(Ft8Error::MessageDecodingError(
        format!("Cannot encode callsign: '{}'", callsign)
    ))
}

/// Pack a standard base callsign into a 28-bit value.
///
/// Normalizes to 6 characters, right-aligned, then encodes with
/// mixed-radix: 37 × 36 × 10 × 27 × 27 × 27
///
/// Character tables:
/// - Position 0: " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ" (37)
/// - Position 1: "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ" (36)
/// - Position 2: "0123456789" (10)
/// - Positions 3-5: " ABCDEFGHIJKLMNOPQRSTUVWXYZ" (27)
fn pack_basecall(callsign: &str) -> Option<u32> {
    let length = callsign.len();
    if length < 3 || length > 6 {
        return None;
    }

    let bytes = callsign.as_bytes();

    // Normalize to 6-character buffer (right-aligned if needed)
    let mut c6 = [b' '; 6];

    // Handle special prefixes
    if callsign.starts_with("3DA0") && length > 4 && length <= 7 {
        // Swaziland: 3DA0XYZ → 3D0XYZ
        c6[0] = b'3'; c6[1] = b'D'; c6[2] = b'0';
        for (i, &b) in bytes[4..].iter().enumerate() {
            if i + 3 < 6 { c6[i + 3] = b; }
        }
    } else if callsign.starts_with("3X") && length > 2 && bytes[2].is_ascii_alphabetic() && length <= 7 {
        // Guinea: 3XA0XYZ → QA0XYZ
        c6[0] = b'Q';
        for (i, &b) in bytes[2..].iter().enumerate() {
            if i + 1 < 6 { c6[i + 1] = b; }
        }
    } else if length >= 3 && bytes[2].is_ascii_digit() && length <= 6 {
        // AB0XYZ format
        c6[..length].copy_from_slice(&bytes[..length]);
    } else if length >= 2 && bytes[1].is_ascii_digit() && length <= 5 {
        // A0XYZ → " A0XYZ" (right-aligned)
        c6[1..1 + length].copy_from_slice(&bytes[..length]);
    } else {
        return None;
    }

    // Encode each position
    let i0 = nchar_alphanum_space(c6[0])?;
    let i1 = nchar_alphanum(c6[1])?;
    let i2 = nchar_numeric(c6[2])?;
    let i3 = nchar_letters_space(c6[3])?;
    let i4 = nchar_letters_space(c6[4])?;
    let i5 = nchar_letters_space(c6[5])?;

    let mut n: u32 = i0;
    n = n * 36 + i1;
    n = n * 10 + i2;
    n = n * 27 + i3;
    n = n * 27 + i4;
    n = n * 27 + i5;

    Some(n)
}

/// Parse CQ modifier: returns value for "CQ nnn" or "CQ ABCD" patterns
fn parse_cq_modifier(modifier: &str) -> Option<u32> {
    if modifier.is_empty() || modifier.len() > 4 {
        return None;
    }

    let bytes = modifier.as_bytes();
    let all_digits = bytes.iter().all(|b| b.is_ascii_digit());
    let all_letters = bytes.iter().all(|b| b.is_ascii_uppercase());

    if all_digits && modifier.len() == 3 {
        // CQ nnn
        let nnn: u32 = modifier.parse().ok()?;
        Some(nnn)
    } else if all_letters && modifier.len() <= 4 {
        // CQ ABCD → base-27 encoding
        let mut m: u32 = 0;
        for &b in bytes {
            m = 27 * m + ((b - b'A') as u32 + 1);
        }
        Some(1000 + m)
    } else {
        None
    }
}

/// Check if a token is a CQ modifier (DX, 3-digit number, or 1-4 letter code)
fn is_cq_modifier(token: &str) -> bool {
    if token == "DX" { return true; }
    let bytes = token.as_bytes();
    if bytes.len() == 3 && bytes.iter().all(|b| b.is_ascii_digit()) { return true; }
    if bytes.len() <= 4 && !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_uppercase()) { return true; }
    false
}

// ============================================================================
// WSJT-X packgrid: grid/report/token → 16-bit value
// ============================================================================

/// Pack a grid locator, signal report, or special token into a 16-bit value.
///
/// Returns value with bit 15 set if ir=1 (R prefix on report).
pub fn packgrid(extra: &str) -> u16 {
    if extra.is_empty() {
        return MAXGRID4 + 1; // no grid/report
    }

    // Special tokens
    if extra == "RRR" { return MAXGRID4 + 2; }
    if extra == "RR73" { return MAXGRID4 + 3; }
    if extra == "73" { return MAXGRID4 + 4; }

    let bytes = extra.as_bytes();

    // Check for 4-character grid locator (AA00..RR99)
    if bytes.len() == 4
        && bytes[0] >= b'A' && bytes[0] <= b'R'
        && bytes[1] >= b'A' && bytes[1] <= b'R'
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
    {
        let mut igrid4: u16 = (bytes[0] - b'A') as u16;
        igrid4 = igrid4 * 18 + (bytes[1] - b'A') as u16;
        igrid4 = igrid4 * 10 + (bytes[2] - b'0') as u16;
        igrid4 = igrid4 * 10 + (bytes[3] - b'0') as u16;
        return igrid4;
    }

    // Parse signal report: +dd / -dd / R+dd / R-dd
    if bytes[0] == b'R' && bytes.len() >= 2 {
        // R prefix → ir=1
        if let Some(dd) = parse_report(&extra[1..]) {
            let irpt = (35 + dd) as u16;
            return (MAXGRID4 + irpt) | 0x8000; // ir=1
        }
    } else if let Some(dd) = parse_report(extra) {
        let irpt = (35 + dd) as u16;
        return MAXGRID4 + irpt; // ir=0
    }

    MAXGRID4 + 1 // fallback: no grid
}

/// Parse a signal report string like "+05" or "-12" into an integer
fn parse_report(s: &str) -> Option<i32> {
    s.parse::<i32>().ok()
}

// ============================================================================
// Character encoding helpers (matching WSJT-X text.h tables)
// ============================================================================

/// " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ" (37 chars)
fn nchar_alphanum_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'0'..=b'9' => Some((c - b'0') as u32 + 1),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 11),
        _ => None,
    }
}

/// "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ" (36 chars)
fn nchar_alphanum(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 10),
        _ => None,
    }
}

/// "0123456789" (10 chars)
fn nchar_numeric(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        _ => None,
    }
}

/// " ABCDEFGHIJKLMNOPQRSTUVWXYZ" (27 chars)
fn nchar_letters_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 1),
        _ => None,
    }
}

/// Look up character index in the 42-char free text table
fn freetext_char_index(c: u8) -> Ft8Result<u8> {
    let c_upper = c.to_ascii_uppercase();
    for (i, &ch) in FREETEXT_CHARS.iter().enumerate() {
        if ch == c_upper {
            return Ok(i as u8);
        }
    }
    Err(Ft8Error::MessageDecodingError(
        format!("Invalid free text character: '{}'", c as char)
    ))
}

/// Configuration for FT8 encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ft8EncodingConfig {
    /// Use hash encoding for non-standard callsigns
    pub use_hash_encoding: bool,
    /// Enable telemetry message support
    pub enable_telemetry: bool,
    /// Maximum free text length (1-13)
    pub max_freetext_length: usize,
}

impl Default for Ft8EncodingConfig {
    fn default() -> Self {
        Self {
            use_hash_encoding: true,
            enable_telemetry: true,
            max_freetext_length: MAX_FREETEXT_LENGTH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_creation() {
        let _encoder = Ft8Encoder::new();
    }

    #[test]
    fn test_pack28_special_tokens() {
        assert_eq!(pack28("DE").unwrap(), (0, 0));
        assert_eq!(pack28("QRZ").unwrap(), (1, 0));
        assert_eq!(pack28("CQ").unwrap(), (2, 0));
    }

    #[test]
    fn test_pack28_cq_modifiers() {
        // CQ 000
        let (n28, ip) = pack28("CQ 000").unwrap();
        assert_eq!(n28, 3);
        assert_eq!(ip, 0);

        // CQ 999
        let (n28, _) = pack28("CQ 999").unwrap();
        assert_eq!(n28, 3 + 999);

        // CQ DX
        let (n28, _) = pack28("CQ DX").unwrap();
        assert_eq!(n28, 3 + 1000 + (4 * 27 + 24)); // D=4, X=24
    }

    #[test]
    fn test_pack28_standard_callsign() {
        // K1ABC should encode as a standard callsign
        let (n28, ip) = pack28("K1ABC").unwrap();
        assert!(n28 >= NTOKENS + MAX22);
        assert_eq!(ip, 0);

        // W1ABC
        let (n28_w, _) = pack28("W1ABC").unwrap();
        assert!(n28_w >= NTOKENS + MAX22);
        assert_ne!(n28, n28_w); // different callsigns should give different values

        // With /R suffix
        let (n28_r, ip_r) = pack28("K1ABC/R").unwrap();
        assert_eq!(n28_r, n28); // same base value
        assert_eq!(ip_r, 1); // suffix flag set
    }

    #[test]
    fn test_pack_basecall_k1abc() {
        // K1ABC → " K1ABC" (right-aligned)
        // i0 = nchar_alphanum_space(' ') = 0
        // i1 = nchar_alphanum('K') = 10 + 10 = 20
        // i2 = nchar_numeric('1') = 1
        // i3 = nchar_letters_space('A') = 1
        // i4 = nchar_letters_space('B') = 2
        // i5 = nchar_letters_space('C') = 3
        // n = 0*36*10*27*27*27 + 20*10*27*27*27 + 1*27*27*27 + 1*27*27 + 2*27 + 3
        //   = 0 + 3,936,600 + 19,683 + 729 + 54 + 3 = 3,957,069
        let n = pack_basecall("K1ABC").unwrap();
        assert_eq!(n, 3_957_069);
    }

    #[test]
    fn test_packgrid() {
        // Empty
        assert_eq!(packgrid(""), MAXGRID4 + 1);

        // Special tokens
        assert_eq!(packgrid("RRR"), MAXGRID4 + 2);
        assert_eq!(packgrid("RR73"), MAXGRID4 + 3);
        assert_eq!(packgrid("73"), MAXGRID4 + 4);

        // Grid locator FN42
        let igrid = packgrid("FN42");
        assert!(igrid <= MAXGRID4);
        // F=5, N=13, 4, 2 → 5*18*10*10 + 13*10*10 + 4*10 + 2 = 9000+1300+40+2 = 10342
        assert_eq!(igrid, 10342);

        // Signal report -12 (no R prefix, ir=0)
        let igrid = packgrid("-12");
        assert_eq!(igrid, MAXGRID4 + 35 - 12); // 32400 + 23 = 32423

        // Signal report R-12 (R prefix, ir=1)
        let igrid = packgrid("R-12");
        assert_eq!(igrid, (MAXGRID4 + 35 - 12) | 0x8000);
    }

    #[test]
    fn test_encode_cq_message() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_cq("K1ABC", "FN42", false);
        assert!(result.is_ok());

        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
        assert!(symbols.iter().all(|&s| s < 8));

        // Verify Costas arrays
        assert_eq!(&symbols[0..7], &COSTAS_ARRAY);
        assert_eq!(&symbols[36..43], &COSTAS_ARRAY);
        assert_eq!(&symbols[72..79], &COSTAS_ARRAY);
    }

    #[test]
    fn test_encode_signal_report() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_signal_report("K1DEF", "W1ABC", -12);
        assert!(result.is_ok());

        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_encode_freetext() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_freetext("HELLO WORLD");
        assert!(result.is_ok());

        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_invalid_signal_report() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_signal_report("K1DEF", "W1ABC", 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_freetext_too_long() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_freetext("THIS MESSAGE IS TOO LONG");
        assert!(result.is_err());
    }

    #[test]
    fn test_freetext_char_encoding() {
        assert_eq!(freetext_char_index(b' ').unwrap(), 0);
        assert_eq!(freetext_char_index(b'0').unwrap(), 1);
        assert_eq!(freetext_char_index(b'9').unwrap(), 10);
        assert_eq!(freetext_char_index(b'A').unwrap(), 11);
        assert_eq!(freetext_char_index(b'Z').unwrap(), 36);
        assert_eq!(freetext_char_index(b'+').unwrap(), 37);
        assert_eq!(freetext_char_index(b'-').unwrap(), 38);
        assert_eq!(freetext_char_index(b'.').unwrap(), 39);
        assert_eq!(freetext_char_index(b'/').unwrap(), 40);
        assert_eq!(freetext_char_index(b'?').unwrap(), 41);
    }

    #[test]
    fn test_costas_arrays() {
        assert_eq!(COSTAS_ARRAY.len(), 7);
        assert!(COSTAS_ARRAY.iter().all(|&s| s < 8));
        assert_eq!(COSTAS_ARRAY, [3, 1, 4, 0, 6, 5, 2]);
    }

    #[test]
    fn test_encode_deterministic() {
        let mut encoder = Ft8Encoder::new();

        let symbols1 = encoder.encode_message("CQ K1ABC FN42", None).unwrap();
        let symbols2 = encoder.encode_message("CQ K1ABC FN42", None).unwrap();
        assert_eq!(symbols1, symbols2);
    }

    #[test]
    fn test_message_parsing_standard() {
        let encoder = Ft8Encoder::new();
        let parts: Vec<&str> = "CQ K1ABC FN42".split_whitespace().collect();
        let (call_to, call_de, extra) = encoder.parse_standard_message(&parts).unwrap();
        assert_eq!(call_to, "CQ");
        assert_eq!(call_de, "K1ABC");
        assert_eq!(extra, "FN42");
    }

    #[test]
    fn test_message_parsing_cq_dx() {
        let encoder = Ft8Encoder::new();
        let parts: Vec<&str> = "CQ DX K1ABC FN42".split_whitespace().collect();
        let (call_to, call_de, extra) = encoder.parse_standard_message(&parts).unwrap();
        assert_eq!(call_to, "CQ DX");
        assert_eq!(call_de, "K1ABC");
        assert_eq!(extra, "FN42");
    }

    #[test]
    fn test_message_parsing_report() {
        let encoder = Ft8Encoder::new();
        let parts: Vec<&str> = "K1DEF W1ABC -12".split_whitespace().collect();
        let (call_to, call_de, extra) = encoder.parse_standard_message(&parts).unwrap();
        assert_eq!(call_to, "K1DEF");
        assert_eq!(call_de, "W1ABC");
        assert_eq!(extra, "-12");
    }

    #[test]
    fn test_payload_cq_k1abc_fn42() {
        // Verify the packed payload for "CQ K1ABC FN42"
        let encoder = Ft8Encoder::new();
        let payload = encoder.try_encode_standard("CQ K1ABC FN42").unwrap();

        // n28a = pack28("CQ") = 2, ipa = 0 → n29a = 4
        // n28b = pack28("K1ABC") = NTOKENS + MAX22 + 3957069 = 10214965, ipb = 0 → n29b = 20429930
        // igrid4 = packgrid("FN42") = 10342
        // ir = 0
        // i3 = 1

        let n29a: u32 = 4; // CQ=2, shifted left 1
        let n29b: u32 = 20_429_930; // K1ABC encoded, shifted left 1
        let igrid4: u16 = 10342;
        let i3: u8 = 1;

        let mut expected = [0u8; 10];
        expected[0] = (n29a >> 21) as u8;
        expected[1] = (n29a >> 13) as u8;
        expected[2] = (n29a >> 5) as u8;
        expected[3] = ((n29a << 3) as u8) | ((n29b >> 26) as u8);
        expected[4] = (n29b >> 18) as u8;
        expected[5] = (n29b >> 10) as u8;
        expected[6] = (n29b >> 2) as u8;
        expected[7] = ((n29b << 6) as u8) | ((igrid4 >> 10) as u8);
        expected[8] = (igrid4 >> 2) as u8;
        expected[9] = ((igrid4 << 6) as u8) | (i3 << 3);

        assert_eq!(payload, expected, "Payload mismatch for CQ K1ABC FN42");
    }
}
