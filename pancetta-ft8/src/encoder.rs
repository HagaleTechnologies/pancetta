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

// rationale: encode loops index the 77-bit payload / 174-bit codeword / 79-symbol
// arrays by position; the index is load-bearing for the protocol layout.
#![allow(clippy::needless_range_loop)]

use crate::ldpc::{binary_to_gray, binary_to_gray_4fsk, LdpcEncoder};
use crate::message::{calculate_crc14, hash12, pack58, CRC_BITS, PAYLOAD_BITS};
use crate::protocol::ProtocolParams;
use crate::{Ft8Error, Ft8Result, NUM_SYMBOLS};
use bitvec::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::debug;

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

/// Message encoder for FT8/FT4/FT2 protocols.
///
/// The payload encoding (77-bit message packing, CRC-14, LDPC) is shared
/// across all protocols. Only the final symbol mapping (Gray code + sync
/// array insertion) differs per protocol.
pub struct Ft8Encoder {
    /// LDPC encoder for error correction
    ldpc_encoder: LdpcEncoder,
    /// Protocol parameters (defaults to FT8)
    protocol: ProtocolParams,
}

impl Ft8Encoder {
    /// Create a new encoder with FT8 protocol (default)
    pub fn new() -> Self {
        Self {
            ldpc_encoder: LdpcEncoder::new(),
            protocol: ProtocolParams::ft8(),
        }
    }

    /// Create a new encoder for a specific protocol
    pub fn with_protocol(protocol: ProtocolParams) -> Self {
        Self {
            ldpc_encoder: LdpcEncoder::new(),
            protocol,
        }
    }

    /// Set the protocol for subsequent encoding operations
    pub fn set_protocol(&mut self, protocol: ProtocolParams) {
        self.protocol = protocol;
    }

    /// Get the current protocol
    pub fn protocol(&self) -> &ProtocolParams {
        &self.protocol
    }

    /// Encode a text message into FT8 transmission symbols
    ///
    /// # Arguments
    /// * `message_text` - Text message to encode (e.g., "CQ W1ABC FN42")
    /// * `_transmit_power` - Transmit power for contest exchanges (unused, reserved)
    ///
    /// # Returns
    /// Array of 79 symbol values (0-7) ready for transmission
    pub fn encode_message(
        &mut self,
        message_text: &str,
        _transmit_power: Option<u8>,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        // Normalize: uppercase, collapse whitespace
        let text = message_text.to_uppercase();
        let text = text.trim();

        // Try standard message encoding first
        if let Ok(payload) = self.try_encode_standard(text) {
            return self.payload_to_symbols(&payload);
        }

        // PAN-17: fall back to the i3=4 nonstandard-callsign path for
        // compound prefix/homecall forms (e.g. "YS/WE9G") that `pack28`
        // can't represent, before giving up to free text (which has a
        // 13-char cap most "<call> <call> <grid>" exchanges blow past).
        if let Ok(payload) = self.try_encode_nonstandard(text) {
            return self.payload_to_symbols(&payload);
        }

        // Fall back to free text encoding
        if let Ok(payload) = self.encode_free_text(text) {
            return self.payload_to_symbols(&payload);
        }

        Err(Ft8Error::MessageDecodingError(format!(
            "Cannot encode message: '{}'",
            message_text
        )))
    }

    /// Encode standard CQ message: "CQ [DX] <callsign> <grid>"
    pub fn encode_cq(
        &mut self,
        callsign: &str,
        grid_square: &str,
        dx_call: bool,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = if dx_call {
            format!("CQ DX {} {}", callsign, grid_square)
        } else {
            format!("CQ {} {}", callsign, grid_square)
        };
        self.encode_message(&message_text, None)
    }

    /// Encode response message: "<to_call> <from_call> <grid>"
    pub fn encode_response(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        grid_square: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} {}", to_callsign, from_callsign, grid_square);
        self.encode_message(&message_text, None)
    }

    /// Encode signal report: "<to_call> <from_call> <report>"
    pub fn encode_signal_report(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        report_db: i8,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if !(MIN_SIGNAL_REPORT..=MAX_SIGNAL_REPORT).contains(&report_db) {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Signal report {} dB out of range ({} to {})",
                report_db, MIN_SIGNAL_REPORT, MAX_SIGNAL_REPORT
            )));
        }
        let message_text = format!("{} {} {:+03}", to_callsign, from_callsign, report_db);
        self.encode_message(&message_text, None)
    }

    /// Encode acknowledgment: "<to_call> <from_call> RRR"
    pub fn encode_rrr(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} RRR", to_callsign, from_callsign);
        self.encode_message(&message_text, None)
    }

    /// Encode final 73: "<to_call> <from_call> 73"
    pub fn encode_73(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let message_text = format!("{} {} 73", to_callsign, from_callsign);
        self.encode_message(&message_text, None)
    }

    /// Encode free text message (max 13 characters)
    pub fn encode_freetext(&mut self, text: &str) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if text.len() > MAX_FREETEXT_LENGTH {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Free text message too long: {} characters (max {})",
                text.len(),
                MAX_FREETEXT_LENGTH
            )));
        }
        let text = text.to_uppercase();
        let payload = self.encode_free_text(&text)?;
        self.payload_to_symbols(&payload)
    }

    // ========================================================================
    // Core encoding pipeline
    // ========================================================================

    /// Encode a message into symbols using the current protocol.
    ///
    /// Returns a `Vec<u8>` with length matching `protocol.num_symbols`.
    /// For FT8: 79 symbols (values 0-7). For FT4: 105 symbols (values 0-3).
    pub fn encode_message_protocol(
        &mut self,
        message_text: &str,
        _transmit_power: Option<u8>,
    ) -> Ft8Result<Vec<u8>> {
        let text = message_text.to_uppercase();
        let text = text.trim();

        if let Ok(payload) = self.try_encode_standard(text) {
            return self.payload_to_symbols_protocol(&payload);
        }
        // PAN-17: see `encode_message`'s matching fallback for rationale.
        if let Ok(payload) = self.try_encode_nonstandard(text) {
            return self.payload_to_symbols_protocol(&payload);
        }
        if let Ok(payload) = self.encode_free_text(text) {
            return self.payload_to_symbols_protocol(&payload);
        }

        Err(Ft8Error::MessageDecodingError(format!(
            "Cannot encode message: '{}'",
            message_text
        )))
    }

    /// Convert 77-bit payload to transmission symbols (protocol-aware, returns Vec)
    fn payload_to_symbols_protocol(&self, payload: &[u8; 10]) -> Ft8Result<Vec<u8>> {
        // FT4 applies XOR scrambling to the payload before CRC computation
        let effective_payload = if let Some(xor_seq) = self.protocol.xor_sequence {
            let mut scrambled = *payload;
            for i in 0..10 {
                scrambled[i] ^= xor_seq[i];
            }
            scrambled
        } else {
            *payload
        };
        let ldpc_codeword = self.payload_to_ldpc(&effective_payload)?;
        self.generate_symbols_protocol(&ldpc_codeword)
    }

    /// Convert 77-bit payload to 79 FT8 transmission symbols (backward compatible)
    fn payload_to_symbols(&self, payload: &[u8; 10]) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let ldpc_codeword = self.payload_to_ldpc(payload)?;
        self.generate_symbols(&ldpc_codeword)
    }

    /// Shared: payload → LDPC codeword (174 bits)
    fn payload_to_ldpc(&self, payload: &[u8; 10]) -> Ft8Result<BitVec> {
        let mut payload_bitvec = BitVec::with_capacity(PAYLOAD_BITS);
        for i in 0..PAYLOAD_BITS {
            payload_bitvec.push(payload[i / 8] & (0x80u8 >> (i % 8)) != 0);
        }

        let crc = calculate_crc14(&payload_bitvec);

        let mut message_bits = BitVec::with_capacity(PAYLOAD_BITS + CRC_BITS);
        message_bits.extend_from_bitslice(&payload_bitvec);
        for i in (0..CRC_BITS).rev() {
            message_bits.push((crc >> i) & 1 != 0);
        }

        self.ldpc_encoder.encode(&message_bits)
    }

    /// Generate symbol sequence from LDPC codeword using current protocol params.
    ///
    /// Inserts Costas sync arrays at the protocol-defined positions and maps
    /// data bits to tones via the appropriate Gray code (3-bit for 8-FSK, 2-bit for 4-FSK).
    fn generate_symbols_protocol(&self, ldpc_codeword: &BitSlice) -> Ft8Result<Vec<u8>> {
        if ldpc_codeword.len() != 174 {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Invalid LDPC codeword length: {}",
                ldpc_codeword.len()
            )));
        }

        let params = &self.protocol;
        let mut symbols = vec![0u8; params.num_symbols];
        let mut bit_idx = 0usize;

        for i in 0..params.num_symbols {
            if let Some(costas_val) = params.costas_value(i) {
                symbols[i] = costas_val;
            } else if params.is_data_symbol(i) {
                match params.bits_per_symbol {
                    3 => {
                        // 8-FSK: extract 3 bits, apply Gray code
                        let mut bits3 = 0u8;
                        if ldpc_codeword[bit_idx] {
                            bits3 |= 4;
                        }
                        if ldpc_codeword[bit_idx + 1] {
                            bits3 |= 2;
                        }
                        if ldpc_codeword[bit_idx + 2] {
                            bits3 |= 1;
                        }
                        bit_idx += 3;
                        symbols[i] = binary_to_gray(bits3);
                    }
                    2 => {
                        // 4-FSK: extract 2 bits, apply Gray code
                        let mut bits2 = 0u8;
                        if ldpc_codeword[bit_idx] {
                            bits2 |= 2;
                        }
                        if ldpc_codeword[bit_idx + 1] {
                            bits2 |= 1;
                        }
                        bit_idx += 2;
                        symbols[i] = binary_to_gray_4fsk(bits2);
                    }
                    _ => {
                        return Err(Ft8Error::ConfigError(format!(
                            "Unsupported bits_per_symbol: {}",
                            params.bits_per_symbol
                        )));
                    }
                }
            }
            // else: ramp or unused symbol, stays 0
        }

        Ok(symbols)
    }

    /// Generate 79-symbol FT8 sequence from LDPC codeword (backward compatible)
    ///
    /// FT8 symbol layout: S7 D29 S7 D29 S7
    fn generate_symbols(&self, ldpc_codeword: &BitSlice) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if ldpc_codeword.len() != 174 {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Invalid LDPC codeword length: {}",
                ldpc_codeword.len()
            )));
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
                let mut bits3 = 0u8;
                if ldpc_codeword[bit_idx] {
                    bits3 |= 4;
                }
                if ldpc_codeword[bit_idx + 1] {
                    bits3 |= 2;
                }
                if ldpc_codeword[bit_idx + 2] {
                    bits3 |= 1;
                }
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
    fn parse_standard_message(&self, parts: &[&str]) -> Ft8Result<(String, String, String)> {
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
                return Err(Ft8Error::MessageDecodingError(
                    "CQ message missing callsign".to_string(),
                ));
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
                    "Standard message needs at least 2 callsigns".to_string(),
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
    // Nonstandard callsign encoding (i3=4)
    // ========================================================================

    /// Try to encode as a nonstandard-callsign message (Type 4, i3=4).
    ///
    /// `pack28` (the i3=1 standard path) can only represent callsigns that
    /// fit its fixed 6-character mixed-radix scheme plus a bare `/R` or `/P`
    /// suffix flag — it has no path for an arbitrary compound prefix/homecall
    /// form (e.g. `YS/WE9G`, `3E40CDW`, `8G81PA`). Those need i3=4: one
    /// callsign is packed exactly into a 58-bit base-38 field (up to 11
    /// characters, charset `HASH_CALL_CHARSET`), the other is represented by
    /// a lossy 12-bit hash (recoverable by the receiver only if it has
    /// previously seen that callsign's plain text — true in practice, since
    /// a station's own callsign appears in every standard-format frame it
    /// sends). Mirrors the decode side, `MessageParser::parse_nonstd_call`
    /// (message.rs) — same bit layout: n12(12) + n58(58) + iflip(1) +
    /// nrpt(2) + icq(1) + i3(3) = 77 bits.
    ///
    /// PAN-17: the 2-bit `nrpt` field only has room for four values — blank,
    /// RRR, RR73, 73 — there is no bit budget left for a grid square or a
    /// numeric dB report once one callsign already needs the compound-call
    /// slot. A grid-shaped `extra` (the CqResponse reply-to-CQ step) is
    /// degraded to blank rather than failing the whole message: dropping a
    /// grid still produces a valid, distinct "bare pairing" frame, and an
    /// FT8 QSO can complete without ever exchanging a grid with a
    /// compound-call station. A *report*-shaped `extra` (a numeric dB value,
    /// with or without an `R` prefix — the SignalReport/ReportAck steps) is
    /// NOT degraded: blanking it would produce a DIFFERENT, misleading
    /// message (indistinguishable on the wire from a bare pairing frame),
    /// which is worse than an honest encode failure — PAN-17 round 2 (Codex
    /// review). Callers must not schedule a report/ack TX to a compound-call
    /// partner expecting it to silently degrade; see
    /// `pancetta-qso::qso_manager::callsign_is_wire_representable`'s
    /// report-aware watchdog check, which retires such a QSO instead of
    /// looping on this Err.
    fn try_encode_nonstandard(&self, text: &str) -> Ft8Result<[u8; 10]> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.is_empty() {
            return Err(Ft8Error::MessageDecodingError("Empty message".to_string()));
        }

        // "CQ <compound-call> [grid]" — icq mode has no field at all for a
        // CQ modifier ("CQ DX ...") or a report, but a trailing grid token
        // (the shape `Ft8Encoder::encode_cq`/`MessageExchange::generate_message`
        // render for a non-DX CQ) is accepted syntactically and dropped —
        // same "can't fit it, drop gracefully" pattern as the directed
        // branch below — so a compound-callsign OPERATOR can still call CQ
        // (PAN-17 round 2: previously rejected outright, `parts.len() != 2`).
        if parts[0] == "CQ" {
            if parts.len() < 2 || parts.len() > 3 {
                return Err(Ft8Error::MessageDecodingError(
                    "Nonstandard CQ must be 'CQ <callsign>' or 'CQ <callsign> <grid>'".to_string(),
                ));
            }
            let call = parts[1];
            if !looks_like_callsign(call) {
                // Not callsign-shaped at all (e.g. a two-word free-text
                // message like "CQ SOMETHING") — this path is only for
                // actual compound callsigns; let free text handle the rest.
                return Err(Ft8Error::MessageDecodingError(format!(
                    "'{}' is not callsign-shaped",
                    call
                )));
            }
            if pack28(call).is_ok() {
                // Standard-packable — try_encode_standard already covers it.
                return Err(Ft8Error::MessageDecodingError(
                    "Callsign is standard-encodable".to_string(),
                ));
            }
            let n58 = pack58(call).ok_or_else(|| {
                Ft8Error::MessageDecodingError(format!(
                    "Callsign '{}' does not fit the 58-bit hash-callsign field \
                     (>11 chars or invalid character)",
                    call
                ))
            })?;
            if let Some(&third) = parts.get(2) {
                // PAN-26: only a genuine grid locator is safe to drop (the
                // icq shape has no field for it, but a distinct "bare CQ"
                // frame is still an honest degrade). Anything else (a
                // mistyped grid, or an unrelated exchange field like a
                // numeric report) must fail loudly instead of silently
                // discarding it — dropping it here would transmit an
                // unrelated, valid-looking bare CQ instead of the requested
                // message.
                //
                // PAN-22 (Codex round-1 review of PR #305, finding 1): a CQ
                // message's optional third token is ALWAYS the sender's
                // grid — the underlying FT8 CQ grammar has no field at all
                // for a close/report token (those only ever appear in a
                // directed reply, never in a CQ). So "RR73" is not a
                // "reserved close token" in this context; it's simply the
                // (extremely rare, but real) Maidenhead square RR73, and
                // must be accepted like any other shape-valid grid — same
                // "drop it, the icq shape has no room" degrade as
                // everything else here. Only reject when the token does NOT
                // parse as a locator at all (mistyped grid, stray free
                // text, etc.); do not special-case specific literal strings.
                if !extra_is_grid_locator(third) {
                    return Err(Ft8Error::MessageDecodingError(format!(
                        "'{}' is not a valid grid locator for a compound-callsign CQ",
                        third
                    )));
                }
                debug!(
                    "i3=4 CQ encode: dropping grid '{}' (icq shape has no room \
                     for it) for '{}'",
                    third, text
                );
            }
            return Ok(pack_nonstandard(0, n58, 0, 0, 1));
        }

        if parts.len() < 2 {
            return Err(Ft8Error::MessageDecodingError(
                "Nonstandard message needs at least 2 callsigns".to_string(),
            ));
        }
        let call_to = parts[0];
        let call_de = parts[1];
        let extra = parts.get(2).copied().unwrap_or("");

        // This path is only for actual compound callsigns, not arbitrary
        // two-word text (e.g. "HELLO WORLD" would otherwise pack28-fail on
        // both "words" and get mistaken for a nonstandard-callsign
        // exchange) — a real callsign always has both a digit and a letter.
        // This also correctly rejects the decode-side hash-miss placeholder
        // `<...>` (no alphanumeric characters at all), so a QSO whose
        // partner callsign somehow ended up as that literal string fails
        // cleanly here rather than transmitting a hash of it.
        if !looks_like_callsign(call_to) || !looks_like_callsign(call_de) {
            return Err(Ft8Error::MessageDecodingError(format!(
                "'{}' / '{}' is not callsign-shaped",
                call_to, call_de
            )));
        }

        let to_std = pack28(call_to).is_ok();
        let de_std = pack28(call_de).is_ok();

        // The callsign that FAILED pack28 is the one that actually needs
        // exact (58-bit) representation — the whole point of this path.
        // Deliberately does NOT fall back to putting the pack28-successful
        // callsign in the exact slot when the nonstandard one can't fit
        // pack58 either: that would silently address the frame with a hash
        // of garbage input (e.g. the decode-side hash-miss placeholder
        // `<...>`) rather than the intended station, which is worse than
        // failing loudly and letting the QSO-layer watchdog retire it.
        let (exact_call, hashed_call, iflip): (&str, &str, u8) = match (to_std, de_std) {
            (true, true) => {
                // Nothing for this path to add — try_encode_standard
                // already covers a message where both callsigns pack28 fine.
                return Err(Ft8Error::MessageDecodingError(
                    "Both callsigns are standard-encodable".to_string(),
                ));
            }
            (false, true) => (call_to, call_de, 1),
            (true, false) => (call_de, call_to, 0),
            (false, false) => {
                // Both compound (e.g. two DXpedition-style calls working
                // each other) — pick whichever fits the 58-bit field,
                // preferring call_to for determinism if both do.
                if pack58(call_to).is_some() {
                    (call_to, call_de, 1)
                } else if pack58(call_de).is_some() {
                    (call_de, call_to, 0)
                } else {
                    return Err(Ft8Error::MessageDecodingError(format!(
                        "Neither '{}' nor '{}' fits the 58-bit hash-callsign field",
                        call_to, call_de
                    )));
                }
            }
        };

        let n58 = pack58(exact_call).ok_or_else(|| {
            Ft8Error::MessageDecodingError(format!(
                "Callsign '{}' does not fit the 58-bit hash-callsign field \
                 (>11 chars or invalid character)",
                exact_call
            ))
        })?;
        let n12 = hash12(hashed_call) & 0xFFF;

        let nrpt: u8 = match extra {
            "" => 0,
            "RRR" => 1,
            "RR73" => 2,
            "73" => 3,
            _ if extra_is_grid_locator(extra) => {
                // Grid square (the CqResponse reply-to-CQ step): dropping it
                // still produces a valid, distinct "bare pairing" frame — an
                // honest degrade, not a different message in disguise.
                debug!(
                    "i3=4 encode: dropping grid '{}' (nonstandard-call report \
                     field has no room for it) for '{}'",
                    extra, text
                );
                0
            }
            _ if extra_is_numeric_report(extra) => {
                // PAN-17 round 2 (Codex review): a numeric dB report or
                // R+report ack CANNOT be represented — there is no bit
                // budget left once one callsign needs the compound slot.
                // Blanking it would silently produce a DIFFERENT, valid-
                // looking message (indistinguishable from a bare pairing
                // frame) instead of the report the QSO actually needs to
                // advance. Fail loudly instead of transmitting the wrong
                // thing; the caller (the QSO layer) must not schedule this.
                return Err(Ft8Error::MessageDecodingError(format!(
                    "Cannot represent report '{}' for a compound-callsign \
                     message: i3=4 has no numeric-report field, and dropping \
                     it would silently send a different, misleading message",
                    extra
                )));
            }
            _ => {
                // Anything else unrecognized: same reasoning as the report
                // case — do not guess. Fail rather than silently drop
                // unknown content.
                return Err(Ft8Error::MessageDecodingError(format!(
                    "Cannot represent exchange field '{}' for a \
                     compound-callsign message",
                    extra
                )));
            }
        };

        Ok(pack_nonstandard(n12, n58, iflip, nrpt, 0))
    }

    // ========================================================================
    // Contest message encoding (i3=0, n3=1..4)
    // ========================================================================

    /// Encode an ARRL Field Day message: "<to_call> <from_call> [R] <class> <section>"
    ///
    /// i3=0, n3=3 (or n3=4 for alternate order).
    /// Class format: nL where n=transmitters (1-32), L=A-F
    /// Section: one of 84 ARRL/RAC sections
    pub fn encode_field_day(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        r_prefix: bool,
        n_transmitters: u8,
        class_letter: char,
        section: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if !(1..=32).contains(&n_transmitters) {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Field Day transmitter count must be 1-32, got {}",
                n_transmitters
            )));
        }
        let letter_idx = match class_letter.to_ascii_uppercase() {
            'A' => 0u32,
            'B' => 1,
            'C' => 2,
            'D' => 3,
            'E' => 4,
            'F' => 5,
            _ => {
                return Err(Ft8Error::MessageDecodingError(format!(
                    "Invalid Field Day class letter: {}",
                    class_letter
                )))
            }
        };
        let section_code = encode_arrl_section(section)?;

        let (n28a, _) = pack28(to_callsign)?;
        let (n28b, _) = pack28(from_callsign)?;

        const NSEC: u32 = 84;
        let class_code = (n_transmitters as u32 - 1) * 6 + letter_idx;
        let n_class_section = class_code * NSEC + section_code as u32;

        let ir: u8 = if r_prefix { 1 } else { 0 };
        let n3: u8 = 3;

        let payload = pack_type0(n28a, n28b, ir, n_class_section as u16, n3);
        self.payload_to_symbols(&payload)
    }

    /// Encode a DXpedition message: "<to_call> <from_call> [R] <grid_or_report>"
    ///
    /// i3=0, n3=1. Same field layout as standard type 1 but uses
    /// the type-0 container with n3=1.
    pub fn encode_dxpedition(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        r_prefix: bool,
        grid_or_report: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let (n28a, _) = pack28(to_callsign)?;
        let (n28b, _) = pack28(from_callsign)?;

        let ir: u8 = if r_prefix { 1 } else { 0 };

        // Pack grid/report into 14-bit field (same logic as standard igrid4 but 14 bits)
        let igrid14 = pack_grid_14bit(grid_or_report);
        let n3: u8 = 1;

        let payload = pack_type0(n28a, n28b, ir, igrid14, n3);
        self.payload_to_symbols(&payload)
    }

    /// Encode an EU VHF Contest message: "<to_call> <from_call> [R] <grid_or_token>"
    ///
    /// i3=0, n3=2. Uses compressed grid encoding for 14-bit field.
    pub fn encode_eu_vhf(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        r_prefix: bool,
        exchange: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let (n28a, _) = pack28(to_callsign)?;
        let (n28b, _) = pack28(from_callsign)?;

        let ir: u8 = if r_prefix { 1 } else { 0 };

        // EU VHF uses compressed grid or tokens in 14-bit field
        let irpt = pack_eu_vhf_14bit(exchange);
        let n3: u8 = 2;

        let payload = pack_type0(n28a, n28b, ir, irpt, n3);
        self.payload_to_symbols(&payload)
    }

    /// Encode an RTTY Roundup message: "<to_call> <from_call> [R] <rst> <state>"
    ///
    /// i3=0, n3=5. Packs RST + state/province into 14-bit field.
    pub fn encode_rtty_roundup(
        &mut self,
        to_callsign: &str,
        from_callsign: &str,
        r_prefix: bool,
        rst: u16,
        state: &str,
    ) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        let (n28a, _) = pack28(to_callsign)?;
        let (n28b, _) = pack28(from_callsign)?;

        let ir: u8 = if r_prefix { 1 } else { 0 };

        let state_code = encode_state_code(state)?;
        // RTTY Roundup: 14-bit field = rst_code * 64 + state_code
        // RST is typically 559 or 599; encode as (rst / 10) - 52 = index
        let rst_code = match rst {
            529 => 0u16,
            539 => 1,
            549 => 2,
            559 => 3,
            569 => 4,
            579 => 5,
            589 => 6,
            599 => 7,
            _ => {
                return Err(Ft8Error::MessageDecodingError(format!(
                    "Invalid RST: {} (must be 529-599 in steps of 10)",
                    rst
                )))
            }
        };

        let irpt = rst_code * 64 + state_code as u16;
        let n3: u8 = 5;

        let payload = pack_type0(n28a, n28b, ir, irpt, n3);
        self.payload_to_symbols(&payload)
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
            return Err(Ft8Error::MessageDecodingError(format!(
                "Free text too long: {} (max {})",
                text.len(),
                MAX_FREETEXT_LENGTH
            )));
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
    if callsign == "DE" {
        return Ok((0, 0));
    }
    if callsign == "QRZ" {
        return Ok((1, 0));
    }
    if callsign == "CQ" {
        return Ok((2, 0));
    }

    // CQ with modifier
    if callsign.starts_with("CQ ") && callsign.len() < 8 {
        let modifier = &callsign[3..];
        if let Some(v) = parse_cq_modifier(modifier) {
            return Ok((3 + v, 0));
        }
        return Err(Ft8Error::MessageDecodingError(format!(
            "Invalid CQ modifier: {}",
            modifier
        )));
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

    Err(Ft8Error::MessageDecodingError(format!(
        "Cannot encode callsign: '{}'",
        callsign
    )))
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
    if !(3..=6).contains(&length) {
        return None;
    }

    let bytes = callsign.as_bytes();

    // Normalize to 6-character buffer (right-aligned if needed)
    let mut c6 = [b' '; 6];

    // Handle special prefixes
    if callsign.starts_with("3DA0") && length > 4 && length <= 7 {
        // Swaziland: 3DA0XYZ → 3D0XYZ
        c6[0] = b'3';
        c6[1] = b'D';
        c6[2] = b'0';
        for (i, &b) in bytes[4..].iter().enumerate() {
            if i + 3 < 6 {
                c6[i + 3] = b;
            }
        }
    } else if callsign.starts_with("3X")
        && length > 2
        && bytes[2].is_ascii_alphabetic()
        && length <= 7
    {
        // Guinea: 3XA0XYZ → QA0XYZ
        c6[0] = b'Q';
        for (i, &b) in bytes[2..].iter().enumerate() {
            if i + 1 < 6 {
                c6[i + 1] = b;
            }
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
    if token == "DX" {
        return true;
    }
    let bytes = token.as_bytes();
    if bytes.len() == 3 && bytes.iter().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if bytes.len() <= 4 && !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_uppercase()) {
        return true;
    }
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
    if extra == "RRR" {
        return MAXGRID4 + 2;
    }
    if extra == "RR73" {
        return MAXGRID4 + 3;
    }
    if extra == "73" {
        return MAXGRID4 + 4;
    }

    let bytes = extra.as_bytes();

    // Check for 4-character grid locator (AA00..RR99)
    if bytes.len() == 4
        && bytes[0] >= b'A'
        && bytes[0] <= b'R'
        && bytes[1] >= b'A'
        && bytes[1] <= b'R'
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
            if !(-35..=30).contains(&dd) {
                return MAXGRID4 + 1; // out of range
            }
            let irpt = (35 + dd) as u16;
            return (MAXGRID4 + irpt) | 0x8000; // ir=1
        }
    } else if let Some(dd) = parse_report(extra) {
        if !(-35..=30).contains(&dd) {
            return MAXGRID4 + 1; // out of range
        }
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
    Err(Ft8Error::MessageDecodingError(format!(
        "Invalid free text character: '{}'",
        c as char
    )))
}

// ============================================================================
// Type-0 message packing (i3=0, n3=1..5)
// ============================================================================

/// Pack a type-0 message into 10-byte payload.
///
/// Layout (77 bits): n28a(28) + n28b(28) + ir(1) + field14(14) + n3(3) + i3(3)
/// i3 is always 0 for type-0 messages.
fn pack_type0(n28a: u32, n28b: u32, ir: u8, field14: u16, n3: u8) -> [u8; 10] {
    let mut payload = [0u8; 10];

    // Bits 0-27: n28a (28 bits)
    payload[0] = (n28a >> 20) as u8;
    payload[1] = (n28a >> 12) as u8;
    payload[2] = (n28a >> 4) as u8;
    payload[3] = ((n28a & 0xF) << 4) as u8 | ((n28b >> 24) as u8);
    // Bits 28-55: n28b (28 bits)
    payload[4] = (n28b >> 16) as u8;
    payload[5] = (n28b >> 8) as u8;
    payload[6] = n28b as u8;
    // Bit 56: ir (1 bit)
    // Bits 57-70: field14 (14 bits)
    // Bits 71-73: n3 (3 bits)
    // Bits 74-76: i3 = 0 (3 bits)
    payload[7] = ((ir & 1) << 7) | ((field14 >> 8) as u8 & 0x7F);
    payload[8] = field14 as u8;
    payload[9] = n3 << 3; // i3=0, so lower 3 bits are 0

    payload
}

/// Pack a 14-bit grid/report field for DXpedition messages.
///
/// Grid: 4-char Maidenhead → standard encoding (0..32399)
/// Report: signal report → 35+dd (no R prefix in this field; R is the ir bit)
/// Tokens: RRR=32402, RR73=32403, 73=32404
fn pack_grid_14bit(exchange: &str) -> u16 {
    if exchange.is_empty() {
        return 0;
    }

    let bytes = exchange.as_bytes();

    // Tokens
    if exchange == "RRR" {
        return MAXGRID4 + 2;
    }
    if exchange == "RR73" {
        return MAXGRID4 + 3;
    }
    if exchange == "73" {
        return MAXGRID4 + 4;
    }

    // 4-character grid
    if bytes.len() == 4
        && bytes[0] >= b'A'
        && bytes[0] <= b'R'
        && bytes[1] >= b'A'
        && bytes[1] <= b'R'
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
    {
        let mut igrid: u16 = (bytes[0] - b'A') as u16;
        igrid = igrid * 18 + (bytes[1] - b'A') as u16;
        igrid = igrid * 10 + (bytes[2] - b'0') as u16;
        igrid = igrid * 10 + (bytes[3] - b'0') as u16;
        return igrid;
    }

    // Signal report
    if let Ok(dd) = exchange.parse::<i32>() {
        return (35 + dd) as u16;
    }

    0 // fallback
}

/// Pack EU VHF compressed grid/report into 14-bit field.
///
/// Grid: compressed 14-bit encoding (lon*900 + lat*50 + lon_digit*5 + lat_digit)
/// Tokens: RRR=16201, RR73=16202, 73=16203
/// Report: 16200 + 35 + dd
fn pack_eu_vhf_14bit(exchange: &str) -> u16 {
    if exchange.is_empty() {
        return 0;
    }

    // Tokens
    if exchange == "RRR" {
        return 16200 + 1;
    }
    if exchange == "RR73" {
        return 16200 + 2;
    }
    if exchange == "73" {
        return 16200 + 3;
    }

    let bytes = exchange.as_bytes();

    // 4-character grid (compressed encoding)
    if bytes.len() == 4
        && bytes[0] >= b'A'
        && bytes[0] <= b'R'
        && bytes[1] >= b'A'
        && bytes[1] <= b'R'
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
    {
        let lon = (bytes[0] - b'A') as u16;
        let lat = (bytes[1] - b'A') as u16;
        let lon_digit = (bytes[2] - b'0') as u16;
        let lat_digit = (bytes[3] - b'0') as u16 / 2; // Compress: only 0-4
        return lon * 900 + lat * 50 + lon_digit * 5 + lat_digit;
    }

    // Signal report
    if let Ok(dd) = exchange.parse::<i32>() {
        return (16200 + 35 + dd) as u16;
    }

    0 // fallback
}

/// "Is this token shaped like a real callsign" check, guarding the i3=4
/// nonstandard-callsign path against being triggered by arbitrary free text.
///
/// PAN-17 round 2 (Codex review): the original version of this check (≥3
/// chars, has a digit AND a letter, in any position) was too loose —
/// `"HELLO1 WORLD2"` is valid 13-char free text, but both tokens pass that
/// check and fit the 58-bit hash-callsign field, so `encode_message` would
/// silently transmit a WRONG type-4 frame instead of the requested free
/// text. Real callsigns (including compound forms) always have their digit
/// "block" with letters on BOTH sides — a prefix before it and a suffix
/// after it (`K1ABC`, `WE9G`, `8G81PA`, `3E40CDW`) — never a bare trailing
/// digit with no suffix letters (`HELLO1`, `WORLD2`). This mirrors the
/// decode side's own shape rule, `Ft8Message::looks_like_callsign`
/// (message.rs's suffix-letters-after-the-last-digit check), so the two
/// paths agree on what a plausible callsign looks like.
///
/// PAN-22 (round 3+ finding): a trailing `/<digits>` is the call-area
/// reassignment convention (`K1ABC/4`, `W1AW/8`) — `pack28` only special-
/// cases `/P`/`/R`, so this genuinely needs the i3=4 fallback, and
/// `pancetta-qso::exchange::validate_callsign` already treats it as a valid
/// compound form. Strip it and validate the base call underneath.
///
/// PAN-27 (round 4 finding): the suffix-after-digit rule alone still
/// misclassifies ordinary words like `"ABC1D"`/`"EFG2H"` as callsign-shaped
/// (they satisfy it too). Real callsign/DXCC prefixes are at most 2 letters
/// before the first digit (`K1`, `W1`, `AB1`), or a digit-led international
/// form with none at all (`8G8...`, `3E4...`) — a longer all-letter run
/// before the first digit is an ordinary word, not a prefix.
fn looks_like_callsign(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    if let Some(slash_pos) = s.rfind('/') {
        let base = &s[..slash_pos];
        let suffix = &s[slash_pos + 1..];
        if base.is_empty() {
            return false;
        }
        if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return looks_like_callsign(base);
        }
        // Compound prefix/homecall form (e.g. "YS/WE9G", "PJ4/KA1ABC"):
        // validate the shape of the final component; earlier components
        // (the DXCC prefix) just need to be present.
        return looks_like_callsign_shape(suffix.as_bytes());
    }
    looks_like_callsign_shape(s.as_bytes())
}

/// Digit/letter shape check shared by `looks_like_callsign`'s branches — see
/// that function's doc for the rationale of each rule.
fn looks_like_callsign_shape(bytes: &[u8]) -> bool {
    if bytes.len() < 2 {
        return false;
    }
    let Some(last_digit_pos) = bytes.iter().rposition(u8::is_ascii_digit) else {
        return false; // no digit at all
    };
    // At least one suffix letter after the last digit.
    if last_digit_pos + 1 >= bytes.len()
        || !bytes[last_digit_pos + 1..]
            .iter()
            .all(u8::is_ascii_alphabetic)
    {
        return false;
    }
    // The prefix before the FIRST digit must be a plausible callsign/DXCC
    // prefix: at most 2 letters (or none, for a digit-led form).
    let first_digit_pos = bytes.iter().position(u8::is_ascii_digit).unwrap();
    let prefix = &bytes[..first_digit_pos];
    prefix.len() <= 2 && prefix.iter().all(u8::is_ascii_alphabetic)
}

/// Does `s` look like a 4-character Maidenhead grid locator (`AA00`..`RR99`)?
/// Mirrors `packgrid`'s own grid-shape check — used by `try_encode_nonstandard`
/// to decide whether an unrepresentable `extra` field is safe to drop
/// (a grid) vs. must fail loudly (a report — see `extra_is_numeric_report`).
fn extra_is_grid_locator(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 4
        && (b'A'..=b'R').contains(&bytes[0])
        && (b'A'..=b'R').contains(&bytes[1])
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
}

/// Does `s` look like a numeric signal report, with or without the `R`
/// acknowledgment prefix (`"-12"`, `"+05"`, `"R-12"`)? Mirrors
/// `packgrid`/`parse_report`'s own report-shape parsing. `try_encode_nonstandard`
/// uses this to recognize a report/ack `extra` that i3=4 genuinely cannot
/// represent, so it can fail loudly instead of silently sending a blank
/// exchange that looks like a different, valid message on the wire.
fn extra_is_numeric_report(s: &str) -> bool {
    let body = s.strip_prefix('R').unwrap_or(s);
    !body.is_empty() && body.parse::<i32>().is_ok()
}

// ============================================================================
// Type-4 (nonstandard callsign) message packing
// ============================================================================

/// Pack an i3=4 nonstandard-callsign message into a 10-byte, 77-bit payload.
///
/// Layout (mirrors `MessageParser::parse_nonstd_call` in message.rs):
/// n12(12) + n58(58) + iflip(1) + nrpt(2) + icq(1) + i3(3) = 77 bits.
fn pack_nonstandard(n12: u32, n58: u64, iflip: u8, nrpt: u8, icq: u8) -> [u8; 10] {
    let mut bits: BitVec<u8, Msb0> = BitVec::with_capacity(80);
    for i in (0..12).rev() {
        bits.push((n12 >> i) & 1 != 0);
    }
    for i in (0..58).rev() {
        bits.push((n58 >> i) & 1 != 0);
    }
    bits.push(iflip != 0);
    for i in (0..2).rev() {
        bits.push((nrpt >> i) & 1 != 0);
    }
    bits.push(icq != 0);
    for i in (0..3).rev() {
        bits.push((4u8 >> i) & 1 != 0); // i3 = 4
    }
    while bits.len() < 80 {
        bits.push(false);
    }
    let mut payload = [0u8; 10];
    payload.copy_from_slice(bits.as_raw_slice());
    payload
}

/// Look up ARRL section name → code (0-83)
fn encode_arrl_section(section: &str) -> Ft8Result<u8> {
    const SECTIONS: [&str; 84] = [
        "CT", "EMA", "ME", "NH", "RI", "VT", "WMA", "ENY", "NLI", "NNJ", "NNY", "SNJ", "WNY", "DE",
        "EPA", "MDC", "WPA", "AL", "GA", "KY", "NC", "NFL", "SC", "SFL", "WCF", "TN", "VA", "PR",
        "MI", "OH", "WV", "IL", "IN", "WI", "AR", "LA", "MS", "NM", "OK", "NTX", "STX", "WTX",
        "CO", "IA", "KS", "MN", "MO", "NE", "ND", "SD", "OR", "EWA", "WWA", "ID", "MT", "WY", "AK",
        "HI", "PAC", "AZ", "EBay", "LAX", "ORG", "SB", "SDG", "SCV", "SF", "SJV", "SV", "NV", "UT",
        "AB", "BC", "GH", "MB", "NB", "NL", "NS", "NT", "ON", "PE", "QC", "SK", "YT",
    ];

    let section_upper = section.to_uppercase();
    for (i, &s) in SECTIONS.iter().enumerate() {
        if s.eq_ignore_ascii_case(&section_upper) {
            return Ok(i as u8);
        }
    }

    Err(Ft8Error::MessageDecodingError(format!(
        "Unknown ARRL section: '{}'",
        section
    )))
}

/// Look up state/province name → code (0-62)
fn encode_state_code(state: &str) -> Ft8Result<u8> {
    const STATES: [&str; 63] = [
        "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
        "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ",
        "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT",
        "VA", "WA", "WV", "WI", "WY", "DC", "AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON",
        "PE", "QC", "SK",
    ];

    let state_upper = state.to_uppercase();
    for (i, &s) in STATES.iter().enumerate() {
        if s == state_upper {
            return Ok(i as u8);
        }
    }

    Err(Ft8Error::MessageDecodingError(format!(
        "Unknown state/province: '{}'",
        state
    )))
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

    #[test]
    fn test_encode_field_day() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_field_day("K1DEF", "W1ABC", false, 2, 'A', "CT");
        assert!(
            result.is_ok(),
            "Field Day encoding failed: {:?}",
            result.err()
        );
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
        assert!(symbols.iter().all(|&s| s < 8));
    }

    #[test]
    fn test_encode_field_day_with_r() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_field_day("K1DEF", "W1ABC", true, 1, 'B', "WMA");
        assert!(result.is_ok());
    }

    #[test]
    fn test_encode_field_day_invalid_class() {
        let mut encoder = Ft8Encoder::new();
        assert!(encoder
            .encode_field_day("K1DEF", "W1ABC", false, 0, 'A', "CT")
            .is_err());
        assert!(encoder
            .encode_field_day("K1DEF", "W1ABC", false, 33, 'A', "CT")
            .is_err());
        assert!(encoder
            .encode_field_day("K1DEF", "W1ABC", false, 1, 'G', "CT")
            .is_err());
    }

    #[test]
    fn test_encode_field_day_invalid_section() {
        let mut encoder = Ft8Encoder::new();
        assert!(encoder
            .encode_field_day("K1DEF", "W1ABC", false, 1, 'A', "ZZZ")
            .is_err());
    }

    #[test]
    fn test_encode_dxpedition() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_dxpedition("K1DEF", "W1ABC", false, "FN42");
        assert!(result.is_ok());
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_encode_dxpedition_report() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_dxpedition("K1DEF", "W1ABC", true, "-12");
        assert!(result.is_ok());
    }

    #[test]
    fn test_encode_eu_vhf_grid() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_eu_vhf("K1DEF", "W1ABC", false, "JO65");
        assert!(result.is_ok());
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_encode_eu_vhf_tokens() {
        let mut encoder = Ft8Encoder::new();
        assert!(encoder
            .encode_eu_vhf("K1DEF", "W1ABC", false, "RRR")
            .is_ok());
        assert!(encoder
            .encode_eu_vhf("K1DEF", "W1ABC", false, "RR73")
            .is_ok());
        assert!(encoder.encode_eu_vhf("K1DEF", "W1ABC", false, "73").is_ok());
    }

    #[test]
    fn test_encode_rtty_roundup() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_rtty_roundup("K1DEF", "W1ABC", false, 599, "NY");
        assert!(result.is_ok());
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
    }

    #[test]
    fn test_encode_rtty_roundup_invalid_rst() {
        let mut encoder = Ft8Encoder::new();
        assert!(encoder
            .encode_rtty_roundup("K1DEF", "W1ABC", false, 500, "NY")
            .is_err());
    }

    #[test]
    fn test_encode_rtty_roundup_invalid_state() {
        let mut encoder = Ft8Encoder::new();
        assert!(encoder
            .encode_rtty_roundup("K1DEF", "W1ABC", false, 599, "ZZ")
            .is_err());
    }

    #[test]
    fn test_arrl_section_lookup() {
        assert_eq!(encode_arrl_section("CT").unwrap(), 0);
        assert_eq!(encode_arrl_section("WMA").unwrap(), 6);
        assert_eq!(encode_arrl_section("YT").unwrap(), 83);
        assert!(encode_arrl_section("ZZZ").is_err());
    }

    #[test]
    fn test_state_code_lookup() {
        assert_eq!(encode_state_code("AL").unwrap(), 0);
        assert_eq!(encode_state_code("NY").unwrap(), 31);
        assert_eq!(encode_state_code("DC").unwrap(), 50);
        assert_eq!(encode_state_code("SK").unwrap(), 62);
        assert!(encode_state_code("ZZ").is_err());
    }

    #[test]
    fn test_pack_type0_structure() {
        // Verify the type-0 packing produces correct bit layout
        let payload = pack_type0(0x0ABCDEF, 0x0123456, 1, 0x3FFF, 3);
        // i3 should be 0 (lower 3 bits of last byte)
        assert_eq!(payload[9] & 0x07, 0, "i3 must be 0 for type-0 messages");
        // n3 should be 3 (bits 71-73 = upper 3 bits of payload[9] >> 3)
        assert_eq!((payload[9] >> 3) & 0x07, 3, "n3 should be 3");
    }

    #[test]
    fn test_pack_grid_14bit() {
        assert_eq!(pack_grid_14bit("RRR"), MAXGRID4 + 2);
        assert_eq!(pack_grid_14bit("RR73"), MAXGRID4 + 3);
        assert_eq!(pack_grid_14bit("73"), MAXGRID4 + 4);
        // FN42 grid
        let g = pack_grid_14bit("FN42");
        assert!(g < MAXGRID4);
        assert_eq!(g, 10342); // Same as packgrid
                              // Report -12
        assert_eq!(pack_grid_14bit("-12"), 23);
    }

    // ====================================================================
    // FT4 encoder tests
    // ====================================================================

    #[test]
    fn test_ft4_encoder_creation() {
        let encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        assert_eq!(encoder.protocol().protocol, crate::Protocol::Ft4);
    }

    #[test]
    fn test_ft4_encode_cq() {
        let mut encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        let symbols = encoder
            .encode_message_protocol("CQ W1ABC FN42", None)
            .unwrap();

        // FT4: 105 symbols, values 0-3
        assert_eq!(symbols.len(), 105);
        assert!(
            symbols.iter().all(|&s| s < 4),
            "All FT4 symbols must be 0-3"
        );

        // Verify sync arrays at correct positions
        // Sync group 0 at positions 1-4: [0, 1, 3, 2]
        assert_eq!(&symbols[1..5], &[0, 1, 3, 2]);
        // Sync group 1 at positions 34-37: [1, 0, 2, 3]
        assert_eq!(&symbols[34..38], &[1, 0, 2, 3]);
        // Sync group 2 at positions 67-70: [2, 3, 1, 0]
        assert_eq!(&symbols[67..71], &[2, 3, 1, 0]);
        // Sync group 3 at positions 100-103: [3, 2, 0, 1]
        assert_eq!(&symbols[100..104], &[3, 2, 0, 1]);

        // Ramp symbols at 0 and 104 should be 0
        assert_eq!(symbols[0], 0);
        assert_eq!(symbols[104], 0);
    }

    #[test]
    fn test_ft4_encode_sample_count() {
        let mut encoder = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        let symbols = encoder
            .encode_message_protocol("CQ W1ABC FN42", None)
            .unwrap();

        // Modulate and verify sample count
        let mut modulator = crate::modulator::Ft8Modulator::with_pulse_shape(
            crate::SAMPLE_RATE,
            crate::BASE_FREQUENCY,
            0.5,
            crate::modulator::PulseShape::Gaussian { bt: 1.0 },
        )
        .unwrap();

        let params = ProtocolParams::ft4();
        let audio = modulator
            .modulate_symbols_protocol(&symbols, 0.0, &params)
            .unwrap();

        // FT4: 105 symbols × 576 samples/symbol = 60480 samples
        assert_eq!(audio.len(), 60480);
        assert!(audio.iter().all(|&s| s.abs() <= 1.0));
    }

    #[test]
    fn test_ft4_xor_symmetry() {
        // Verify that XOR scrambling → CRC → LDPC → decode → un-XOR → parse
        // gives back the original message for various message types.
        use crate::message::{calculate_crc14, CRC_BITS, PAYLOAD_BITS};
        use crate::protocol::{ProtocolParams, FT4_XOR_SEQUENCE};

        let encoder = Ft8Encoder::new(); // FT8 encoder to get raw payloads

        let messages = [
            "CQ W1ABC FN42",
            "CQ DX W1ABC FN42",
            "K1DEF W1ABC FN42",
            "K1DEF W1ABC -12",
            "K1DEF W1ABC RR73",
            "HELLO WORLD",
        ];

        for msg in &messages {
            // Get the raw 77-bit payload (as 10 bytes, without XOR)
            let payload = encoder
                .try_encode_standard(msg)
                .or_else(|_| encoder.encode_free_text(msg))
                .unwrap();

            // Apply XOR (as encoder does for FT4)
            let mut scrambled = payload;
            for i in 0..10 {
                scrambled[i] ^= FT4_XOR_SEQUENCE[i];
            }

            // Extract 77 bits from scrambled payload
            let mut scrambled_bits = BitVec::with_capacity(PAYLOAD_BITS);
            for i in 0..PAYLOAD_BITS {
                scrambled_bits.push(scrambled[i / 8] & (0x80u8 >> (i % 8)) != 0);
            }

            // Compute CRC on scrambled payload
            let _crc = calculate_crc14(&scrambled_bits);

            // Simulate decoder un-XOR: apply XOR to the scrambled bits
            let mut unscrambled_bits = scrambled_bits.clone();
            for byte_idx in 0..10 {
                let xor_byte = FT4_XOR_SEQUENCE[byte_idx];
                for bit_pos in 0..8 {
                    let global_bit = byte_idx * 8 + bit_pos;
                    if global_bit >= PAYLOAD_BITS {
                        break;
                    }
                    if (xor_byte >> (7 - bit_pos)) & 1 == 1 {
                        let cur = unscrambled_bits[global_bit];
                        unscrambled_bits.set(global_bit, !cur);
                    }
                }
            }

            // The unscrambled bits should match the original payload bits
            let mut original_bits: BitVec<u8, Msb0> = BitVec::with_capacity(PAYLOAD_BITS);
            for i in 0..PAYLOAD_BITS {
                original_bits.push(payload[i / 8] & (0x80u8 >> (i % 8)) != 0);
            }

            assert_eq!(
                unscrambled_bits, original_bits,
                "XOR round-trip failed for '{}'",
                msg
            );

            // Also verify the message parser can parse the unscrambled payload
            let parser = crate::message::MessageParser::new();
            let parsed = parser.parse_payload(&unscrambled_bits);
            assert!(
                parsed.is_ok(),
                "Failed to parse unscrambled payload for '{}': {:?}",
                msg,
                parsed
            );
        }
    }

    #[test]
    fn test_ft4_different_from_ft8() {
        // Same message encoded as FT8 and FT4 should produce different symbols
        let mut ft8_enc = Ft8Encoder::new();
        let mut ft4_enc = Ft8Encoder::with_protocol(ProtocolParams::ft4());

        let ft8_syms = ft8_enc.encode_message("CQ W1ABC FN42", None).unwrap();
        let ft4_syms = ft4_enc
            .encode_message_protocol("CQ W1ABC FN42", None)
            .unwrap();

        assert_eq!(ft8_syms.len(), 79);
        assert_eq!(ft4_syms.len(), 105);

        // Data content should differ due to XOR scrambling and different Gray code
    }

    // ========================================================================
    // PAN-17: nonstandard/compound-callsign encoding (i3=4)
    // ========================================================================

    /// Decode a raw 10-byte 77-bit payload back through `MessageParser`,
    /// exactly like `test_ft4_xor_scramble_unscramble_roundtrip` above does
    /// for the standard path — the round-trip check that actually proves
    /// the encoder and decoder agree on the wire format.
    fn decode_payload(payload: &[u8; 10]) -> crate::message::Ft8Message {
        decode_payload_with(payload, |_| {})
    }

    fn decode_payload_with(
        payload: &[u8; 10],
        setup: impl FnOnce(&mut crate::message::MessageParser),
    ) -> crate::message::Ft8Message {
        let mut bits = BitVec::with_capacity(PAYLOAD_BITS);
        for i in 0..PAYLOAD_BITS {
            bits.push(payload[i / 8] & (0x80u8 >> (i % 8)) != 0);
        }
        let mut parser = crate::message::MessageParser::new();
        setup(&mut parser);
        parser.parse_payload(&bits).expect("payload must parse")
    }

    #[test]
    fn test_encode_message_compound_callsign_cq_no_longer_errors() {
        // PAN-17 root cause: encode_message had zero path for a compound
        // prefix/homecall callsign (pack28 has no representation for
        // arbitrary "<prefix>/<homecall>" forms) and always returned Err.
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_message("CQ YS/WE9G", None);
        assert!(
            result.is_ok(),
            "compound-callsign CQ failed to encode: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_encode_decode_roundtrip_compound_callsign_cq() {
        let encoder = Ft8Encoder::new();
        let payload = encoder.try_encode_nonstandard("CQ YS/WE9G").unwrap();
        let msg = decode_payload(&payload);
        assert_eq!(msg.to_callsign, Some("CQ".to_string()));
        assert_eq!(msg.from_callsign, Some("YS/WE9G".to_string()));
    }

    #[test]
    fn test_encode_decode_roundtrip_compound_callsign_reply_no_report() {
        // "YS/WE9G K5ARH" — reply to a compound-call CQ carrying neither
        // grid nor report (see the grid-degradation test below for why).
        let encoder = Ft8Encoder::new();
        let payload = encoder.try_encode_nonstandard("YS/WE9G K5ARH").unwrap();
        let msg = decode_payload_with(&payload, |p| p.add_callsign("K5ARH"));
        assert_eq!(msg.to_callsign, Some("YS/WE9G".to_string()));
        assert_eq!(msg.from_callsign, Some("<K5ARH>".to_string()));
    }

    #[test]
    fn test_encode_decode_roundtrip_compound_callsign_rr73() {
        let encoder = Ft8Encoder::new();
        let payload = encoder
            .try_encode_nonstandard("YS/WE9G K5ARH RR73")
            .unwrap();
        let msg = decode_payload_with(&payload, |p| p.add_callsign("K5ARH"));
        assert_eq!(msg.to_callsign, Some("YS/WE9G".to_string()));
        assert_eq!(msg.from_callsign, Some("<K5ARH>".to_string()));
        assert_eq!(msg.contest_exchange, Some("RR73".to_string()));
    }

    #[test]
    fn test_encode_message_compound_callsign_with_grid_degrades_gracefully() {
        // PAN-17 live repro (YS/WE9G, 2026-08-13): pancetta tried to send
        // "YS/WE9G K5ARH EM10" (the grid-carrying reply-to-CQ message) and
        // encode_message always returned Err, so the QSO queued a TX every
        // slot for the full watchdog window but never actually keyed the
        // radio. The i3=4 wire format has no room for a grid alongside a
        // compound callsign (only 2 report bits: blank/RRR/RR73/73), so the
        // grid is dropped rather than failing the whole message — this is
        // what actually unblocks the QSO on air.
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_message("YS/WE9G K5ARH EM10", None);
        assert!(
            result.is_ok(),
            "compound-callsign grid reply failed to encode: {:?}",
            result.err()
        );

        let payload = encoder
            .try_encode_nonstandard("YS/WE9G K5ARH EM10")
            .unwrap();
        let msg = decode_payload_with(&payload, |p| p.add_callsign("K5ARH"));
        assert_eq!(msg.to_callsign, Some("YS/WE9G".to_string()));
        assert_eq!(msg.from_callsign, Some("<K5ARH>".to_string()));
        // The grid could not be carried — nrpt degrades to blank.
        assert_eq!(msg.contest_exchange, None);
    }

    #[test]
    fn test_encode_message_other_compound_forms_from_pan17_incident() {
        // Other compound-call classes cited in the PAN-17 live incident log
        // (all previously failed to encode for the same reason as YS/WE9G).
        let mut encoder = Ft8Encoder::new();
        for msg in [
            "CQ 8G81PA",
            "CQ 3E40CDW",
            "8G81PA K5ARH EM10",
            "3E40CDW K5ARH EM10",
        ] {
            assert!(
                encoder.encode_message(msg, None).is_ok(),
                "'{}' failed to encode",
                msg
            );
        }
    }

    #[test]
    fn test_try_encode_nonstandard_rejects_when_compound_call_exceeds_hash_field() {
        // A callsign that fails BOTH pack28 and pack58 (invalid characters —
        // e.g. the decode-side hash-miss placeholder "<...>", or >11 chars)
        // genuinely cannot be represented on the wire. This must stay a
        // clean Err (not silently address the frame using a hash of K5ARH
        // as if it were the DX), so the QSO-layer watchdog can retire the
        // QSO instead of retrying an unencodable message for minutes.
        let encoder = Ft8Encoder::new();
        let result = encoder.try_encode_nonstandard("<...> K5ARH EM10");
        assert!(result.is_err());

        // And the overall encode_message also fails cleanly (free text is
        // too long for this text, too, so this remains an honest Err).
        let mut encoder = Ft8Encoder::new();
        assert!(encoder.encode_message("<...> K5ARH EM10", None).is_err());
    }

    #[test]
    fn test_try_encode_nonstandard_both_compound() {
        // Two compound calls working each other (e.g. two DXpeditions) —
        // rare, but the picking rule must still be deterministic and
        // produce a valid payload rather than a panic.
        let encoder = Ft8Encoder::new();
        let payload = encoder
            .try_encode_nonstandard("YS/WE9G PJ4/KA1ABC RR73")
            .unwrap();
        let msg = decode_payload(&payload);
        // call_to ("YS/WE9G") wins the exact slot per the deterministic
        // preference; call_de is hashed (unresolvable without a prior
        // add_callsign, so it renders as the hash-miss placeholder).
        assert_eq!(msg.to_callsign, Some("YS/WE9G".to_string()));
        assert_eq!(msg.from_callsign, Some("<...>".to_string()));
        assert_eq!(msg.contest_exchange, Some("RR73".to_string()));
    }

    // ========================================================================
    // PAN-17 round 2 (Codex review #248): P1/P2 remediation
    // ========================================================================

    #[test]
    fn test_try_encode_nonstandard_rejects_numeric_report_instead_of_blanking() {
        // P1 #1: a numeric dB report to a compound-call partner cannot be
        // represented (no bit budget) — must fail loudly, NOT silently
        // degrade to a blank exchange (which would look like a different,
        // valid message: a bare CqResponse/pairing frame, not a report).
        let encoder = Ft8Encoder::new();
        for msg in ["YS/WE9G K5ARH -12", "YS/WE9G K5ARH +05"] {
            let result = encoder.try_encode_nonstandard(msg);
            assert!(
                result.is_err(),
                "'{}' (numeric report) must fail, not silently blank",
                msg
            );
        }
    }

    #[test]
    fn test_try_encode_nonstandard_rejects_report_ack_instead_of_blanking() {
        // P1 #1: an R+report acknowledgment is likewise unrepresentable.
        let encoder = Ft8Encoder::new();
        let result = encoder.try_encode_nonstandard("YS/WE9G K5ARH R-12");
        assert!(
            result.is_err(),
            "R+report ack must fail, not silently blank"
        );
    }

    #[test]
    fn test_encode_message_report_to_compound_call_fails_cleanly_overall() {
        // The full encode_message fallback chain: standard fails (compound
        // call), nonstandard now correctly refuses the report, and free
        // text also fails (too long / contains characters outside its
        // charset is irrelevant here — length is the blocker: "YS/WE9G
        // K5ARH -12" is 17 chars, over the 13-char free-text cap). The
        // overall result must be an honest Err, never a silently-wrong TX.
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_message("YS/WE9G K5ARH -12", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_cq_grid_bearing_from_compound_operator() {
        // P1 #3: a compound-callsign OPERATOR must still be able to call CQ
        // with their own grid — `encode_cq`/`generate_message` render this
        // as a 3-token "CQ <call> <grid>" message. Previously rejected
        // outright (`parts.len() != 2`); now accepted, dropping the grid.
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_cq("YS/WE9G", "EM10", false);
        assert!(
            result.is_ok(),
            "grid-bearing CQ from a compound-callsign operator must encode: {:?}",
            result.err()
        );

        let payload = encoder.try_encode_nonstandard("CQ YS/WE9G EM10").unwrap();
        let msg = decode_payload(&payload);
        assert_eq!(msg.to_callsign, Some("CQ".to_string()));
        assert_eq!(msg.from_callsign, Some("YS/WE9G".to_string()));
    }

    #[test]
    fn test_looks_like_callsign_rejects_trailing_digit_free_text() {
        // P2 #5: "HELLO1"/"WORLD2" have a digit and a letter but are NOT
        // callsign-shaped — real callsigns always have a suffix letter
        // after the digit block; a bare trailing digit is not a callsign.
        assert!(!looks_like_callsign("HELLO1"));
        assert!(!looks_like_callsign("WORLD2"));
        // Sanity: genuine callsign shapes still pass.
        assert!(looks_like_callsign("K1ABC"));
        assert!(looks_like_callsign("YS/WE9G"));
        assert!(looks_like_callsign("8G81PA"));
        assert!(looks_like_callsign("3E40CDW"));
    }

    #[test]
    fn test_encode_message_preserves_digit_bearing_free_text() {
        // P2 #5 regression (Codex review #248): "HELLO1 WORLD2" is valid
        // 13-char free text. Before the looks_like_callsign fix, both
        // tokens failed pack28, passed the old (loose) heuristic, and fit
        // pack58 — so encode_message silently transmitted a WRONG type-4
        // callsign-hash frame instead of the requested free text. It must
        // now round-trip as FreeText, not NonStdCall.
        let mut encoder = Ft8Encoder::new();
        let symbols = encoder
            .encode_message("HELLO1 WORLD2", None)
            .expect("valid 13-char free text must encode");

        // Decode it back and confirm it landed as FreeText, not a mangled
        // nonstandard-callsign frame.
        let payload = encoder.encode_free_text("HELLO1 WORLD2").unwrap();
        let expected_symbols = encoder.payload_to_symbols(&payload).unwrap();
        assert_eq!(
            symbols, expected_symbols,
            "encode_message must choose the free-text encoding, not i3=4"
        );

        let msg = decode_payload(&payload);
        assert_eq!(msg.message_type, crate::message::MessageType::FreeText);
    }

    #[test]
    fn test_try_encode_nonstandard_still_rejects_plain_free_text() {
        // Existing free-text safety net still holds for text with no digit
        // at all.
        let encoder = Ft8Encoder::new();
        assert!(encoder.try_encode_nonstandard("HELLO WORLD").is_err());
    }

    #[test]
    fn test_looks_like_callsign_accepts_numeral_call_area_suffix() {
        // PAN-22 finding 1 (round 3+ review): "K1ABC/4"/"W1AW/8" are the
        // call-area reassignment convention -- `pack28` only special-cases
        // /P and /R, so these need the i3=4 fallback, and
        // `pancetta-qso::exchange::validate_callsign` already treats them
        // as valid compound forms. Must not regress to unencodable.
        assert!(looks_like_callsign("K1ABC/4"));
        assert!(looks_like_callsign("W1AW/8"));
        // A digit suffix with no plausible base underneath must still fail.
        assert!(!looks_like_callsign("AB/4"));
    }

    #[test]
    fn test_encode_message_numeral_call_area_suffix_compound_call() {
        // PAN-22 finding 1 live scenario: a compound call using the
        // call-area reassignment suffix must still encode via i3=4, not
        // regress to an Err (pack28 can't represent it either).
        let mut encoder = Ft8Encoder::new();
        for msg in ["CQ K1ABC/4", "K1ABC/4 K5ARH EM10", "CQ W1AW/8"] {
            assert!(
                encoder.encode_message(msg, None).is_ok(),
                "'{}' (numeral call-area suffix) failed to encode",
                msg
            );
        }

        let payload = encoder.try_encode_nonstandard("CQ K1ABC/4").unwrap();
        let msg = decode_payload(&payload);
        assert_eq!(msg.to_callsign, Some("CQ".to_string()));
        assert_eq!(msg.from_callsign, Some("K1ABC/4".to_string()));
    }

    #[test]
    fn test_try_encode_nonstandard_cq_rejects_non_grid_third_token() {
        // PAN-26 finding 2: a three-token compound CQ whose third token is
        // NOT a valid grid must fail loudly, not silently drop it and
        // transmit the unrelated bare "CQ <call>" message. Previously any
        // third token was unconditionally dropped.
        //
        // Note: "RR73" is deliberately NOT in this list — PAN-22 finding 1
        // (Codex round-1 review of PR #305) established that a CQ's third
        // token is always meant as a grid, and "RR73" is shape-valid
        // Maidenhead (see `test_try_encode_nonstandard_cq_accepts_rr73_shaped_grid`
        // below), so it must be accepted/dropped like any other grid, not
        // rejected as a bogus close token.
        let encoder = Ft8Encoder::new();
        for msg in ["CQ YS/WE9G HELLO", "CQ YS/WE9G AA9"] {
            assert!(
                encoder.try_encode_nonstandard(msg).is_err(),
                "'{}' has a non-grid third token and must fail, not silently drop it",
                msg
            );
        }
        // A genuine grid is still accepted and dropped (existing behavior).
        assert!(encoder.try_encode_nonstandard("CQ YS/WE9G EM10").is_ok());
    }

    #[test]
    fn test_encode_message_report_to_compound_call_via_cq_fails_cleanly_overall() {
        // The full encode_message fallback chain for the PAN-26 finding 2
        // scenario: standard fails (compound call), nonstandard correctly
        // refuses a bogus (non-grid-shaped) third token, and free text also
        // fails (too long). The overall result must be an honest Err.
        let mut encoder = Ft8Encoder::new();
        assert!(encoder.encode_message("CQ YS/WE9G HELLO", None).is_err());
    }

    #[test]
    fn test_try_encode_nonstandard_cq_accepts_rr73_shaped_grid() {
        // PAN-22 finding 1 (Codex round-1 review of PR #305): a
        // compound-callsign operator whose configured grid is the
        // (extremely rare but syntactically valid) Maidenhead square
        // "RR73" must still be able to encode a CQ. Previously this
        // literal string was special-cased as a "reserved close token" and
        // rejected outright before the locator shape check even ran, even
        // though "R"/"R" are valid Maidenhead field letters and "7"/"3" are
        // valid square digits — i.e. "RR73" legitimately parses as a grid.
        // The icq shape still has no room to carry any third token, so
        // (like every other grid) it is accepted and dropped, producing
        // the same honest "bare CQ" degrade as any other grid-bearing
        // compound CQ.
        let encoder = Ft8Encoder::new();
        let result = encoder.try_encode_nonstandard("CQ YS/WE9G RR73");
        assert!(
            result.is_ok(),
            "'RR73' is a shape-valid grid and must not be rejected as a close token: {:?}",
            result.err()
        );
        let payload = result.unwrap();
        let msg = decode_payload(&payload);
        assert_eq!(msg.to_callsign, Some("CQ".to_string()));
        assert_eq!(msg.from_callsign, Some("YS/WE9G".to_string()));

        // And via the actual public entry point a compound-callsign
        // operator would use: `encode_cq` with grid_square == "RR73".
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_cq("YS/WE9G", "RR73", false);
        assert!(
            result.is_ok(),
            "encode_cq with grid 'RR73' from a compound-callsign operator must encode: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_looks_like_callsign_rejects_ambiguous_free_text_words() {
        // PAN-27 finding 1 (round 4 review): "ABC1D"/"EFG2H" satisfy the
        // suffix-after-digit rule (like "HELLO1"/"WORLD2" did before that
        // fix) but are ordinary words, not callsigns -- a real
        // callsign/DXCC prefix is at most 2 letters before the first digit.
        assert!(!looks_like_callsign("ABC1D"));
        assert!(!looks_like_callsign("EFG2H"));
        // Sanity: genuine callsign shapes (short prefix, or digit-led
        // international forms) still pass.
        assert!(looks_like_callsign("K1ABC"));
        assert!(looks_like_callsign("8G81PA"));
        assert!(looks_like_callsign("3E40CDW"));
        assert!(looks_like_callsign("YS/WE9G"));
    }

    #[test]
    fn test_encode_message_ambiguous_free_text_routes_to_free_text_not_nonstdcall() {
        // PAN-27 finding 1 live scenario: "ABC1D EFG2H" is valid 11-char
        // free text. Before this fix, both tokens satisfied
        // looks_like_callsign, fit pack58, and neither fit pack28 -- so
        // encode_message transmitted an unrelated type-4 callsign frame
        // instead of the requested free text.
        let mut encoder = Ft8Encoder::new();
        let symbols = encoder
            .encode_message("ABC1D EFG2H", None)
            .expect("valid free text must encode");

        let payload = encoder.encode_free_text("ABC1D EFG2H").unwrap();
        let expected_symbols = encoder.payload_to_symbols(&payload).unwrap();
        assert_eq!(
            symbols, expected_symbols,
            "encode_message must choose the free-text encoding, not i3=4"
        );

        let msg = decode_payload(&payload);
        assert_eq!(msg.message_type, crate::message::MessageType::FreeText);

        // And compound calls (the OTHER direction PAN-27 requires still
        // hold) must still route to i3=4, not free text.
        assert!(encoder.try_encode_nonstandard("CQ YS/WE9G").is_ok());
        let cq_symbols = encoder.encode_message("CQ YS/WE9G", None).unwrap();
        let cq_payload = encoder.try_encode_nonstandard("CQ YS/WE9G").unwrap();
        let expected_cq_symbols = encoder.payload_to_symbols(&cq_payload).unwrap();
        assert_eq!(
            cq_symbols, expected_cq_symbols,
            "a genuine compound-callsign CQ must still route to i3=4, not free text"
        );
    }
}
