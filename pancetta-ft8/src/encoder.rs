//! FT8 message encoding implementation
//!
//! This module handles encoding of text messages into FT8 protocol format:
//! - 77-bit information payload encoding
//! - CRC-14 checksum calculation
//! - LDPC error correction coding
//! - Symbol generation for transmission
//! - Support for all standard FT8 message types

use crate::message::{
    MessageType, Ft8Message, PAYLOAD_BITS, CRC_BITS, calculate_crc14, NUM_SYMBOLS
};
use crate::{Ft8Error, Ft8Result, TONE_SPACING, NUM_TONES};
use bitvec::prelude::*;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Maximum length for free text messages
pub const MAX_FREETEXT_LENGTH: usize = 13;

/// Maximum signal report value (+40 dB)
pub const MAX_SIGNAL_REPORT: i8 = 40;

/// Minimum signal report value (-50 dB)
pub const MIN_SIGNAL_REPORT: i8 = -50;

/// FT8 message encoder for generating transmission-ready symbols
pub struct Ft8Encoder {
    /// Callsign encoding table
    callsign_table: CallsignEncodingTable,
    /// LDPC encoder for error correction
    ldpc_encoder: LdpcEncoder,
    /// Costas sync arrays for symbol generation
    costas_arrays: CostasArrays,
}

impl Ft8Encoder {
    /// Create a new FT8 encoder
    pub fn new() -> Self {
        Self {
            callsign_table: CallsignEncodingTable::new(),
            ldpc_encoder: LdpcEncoder::new(),
            costas_arrays: CostasArrays::new(),
        }
    }

    /// Encode a text message into FT8 transmission symbols
    ///
    /// # Arguments
    /// * `message_text` - Text message to encode (e.g., "CQ W1ABC FN42")
    /// * `transmit_power` - Transmit power for contest exchanges (optional)
    ///
    /// # Returns
    /// Array of 79 symbol values (0-7) ready for transmission
    pub fn encode_message(&mut self, message_text: &str, transmit_power: Option<u8>) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        // Parse the message text into structured format
        let ft8_message = self.parse_message_text(message_text, transmit_power)?;
        
        // Encode message into 77-bit payload
        let payload_bits = self.encode_message_payload(&ft8_message)?;
        
        // Calculate CRC-14 checksum
        let crc = calculate_crc14(&payload_bits);
        
        // Combine payload and CRC into 91-bit message
        let mut message_bits = BitVec::with_capacity(PAYLOAD_BITS + CRC_BITS);
        message_bits.extend_from_bitslice(&payload_bits);
        
        // Append CRC bits (MSB first)
        for i in (0..CRC_BITS).rev() {
            message_bits.push((crc >> i) & 1 != 0);
        }
        
        // Apply LDPC error correction encoding (91 bits -> 174 bits)
        let ldpc_codeword = self.ldpc_encoder.encode(&message_bits)?;
        
        // Generate symbol sequence with Costas arrays
        let symbols = self.generate_symbols(&ldpc_codeword)?;
        
        Ok(symbols)
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
        
        // Validate characters (only basic ASCII allowed)
        for ch in text.chars() {
            if !self.is_valid_freetext_char(ch) {
                return Err(Ft8Error::MessageDecodingError(
                    format!("Invalid character in free text: '{}'", ch)
                ));
            }
        }
        
        self.encode_message(text, None)
    }

    /// Encode contest exchange with power
    pub fn encode_contest_exchange(&mut self, to_callsign: &str, from_callsign: &str, report_db: i8, power_watts: u8) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if power_watts == 0 || power_watts > 99 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid power value: {} watts (1-99)", power_watts)
            ));
        }
        
        let message_text = format!("{} {} R{:+03}", to_callsign, from_callsign, report_db);
        self.encode_message(&message_text, Some(power_watts))
    }

    /// Encode telemetry data (custom format)
    pub fn encode_telemetry(&mut self, data: &[u8]) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if data.len() > 9 {
            return Err(Ft8Error::MessageDecodingError(
                "Telemetry data too long (max 9 bytes)".to_string()
            ));
        }
        
        // Create telemetry message
        let ft8_message = Ft8Message {
            message_type: MessageType::Telemetry,
            from_callsign: None,
            to_callsign: None,
            grid_square: None,
            signal_report: None,
            text: None,
            payload_bits: BitVec::new(),
            crc: 0,
            crc_valid: false,
        };
        
        let payload_bits = self.encode_telemetry_payload(data)?;
        let crc = calculate_crc14(&payload_bits);
        
        let mut message_bits = payload_bits;
        for i in (0..CRC_BITS).rev() {
            message_bits.push((crc >> i) & 1 != 0);
        }
        
        let ldpc_codeword = self.ldpc_encoder.encode(&message_bits)?;
        let symbols = self.generate_symbols(&ldpc_codeword)?;
        
        Ok(symbols)
    }

    /// Parse message text into structured FT8 message
    fn parse_message_text(&mut self, text: &str, _transmit_power: Option<u8>) -> Ft8Result<Ft8Message> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        
        if parts.is_empty() {
            return Err(Ft8Error::MessageDecodingError("Empty message".to_string()));
        }
        
        let mut message = Ft8Message::default();
        
        // Determine message type and parse accordingly
        match parts[0] {
            "CQ" => {
                message.message_type = MessageType::Cq;
                if parts.len() >= 2 {
                    let start_idx = if parts[1] == "DX" { 2 } else { 1 };
                    if parts.len() > start_idx {
                        message.from_callsign = Some(parts[start_idx].to_string());
                    }
                    if parts.len() > start_idx + 1 {
                        message.grid_square = Some(parts[start_idx + 1].to_string());
                    }
                }
            }
            _ => {
                // Check for signal report or other message types
                if parts.len() >= 3 {
                    let third_part = parts[2];
                    if third_part == "RRR" {
                        message.message_type = MessageType::Ack;
                        message.to_callsign = Some(parts[0].to_string());
                        message.from_callsign = Some(parts[1].to_string());
                    } else if third_part == "73" {
                        message.message_type = MessageType::Final;
                        message.to_callsign = Some(parts[0].to_string());
                        message.from_callsign = Some(parts[1].to_string());
                    } else if third_part.starts_with('+') || third_part.starts_with('-') || third_part.starts_with('R') {
                        message.message_type = MessageType::Report;
                        message.to_callsign = Some(parts[0].to_string());
                        message.from_callsign = Some(parts[1].to_string());
                        
                        let report_str = if third_part.starts_with('R') {
                            &third_part[1..]
                        } else {
                            third_part
                        };
                        
                        if let Ok(report) = report_str.parse::<i8>() {
                            message.signal_report = Some(report);
                        }
                    } else if self.is_grid_square(third_part) {
                        message.message_type = MessageType::Response;
                        message.to_callsign = Some(parts[0].to_string());
                        message.from_callsign = Some(parts[1].to_string());
                        message.grid_square = Some(third_part.to_string());
                    }
                } else {
                    // Free text or grid-only message
                    if parts.len() == 1 && self.is_grid_square(parts[0]) {
                        message.message_type = MessageType::GridOnly;
                        message.grid_square = Some(parts[0].to_string());
                    } else {
                        message.message_type = MessageType::FreeText;
                        message.text = Some(text.to_string());
                    }
                }
            }
        }
        
        Ok(message)
    }

    /// Encode FT8 message into 77-bit payload
    fn encode_message_payload(&mut self, message: &Ft8Message) -> Ft8Result<BitVec> {
        let mut payload = BitVec::with_capacity(PAYLOAD_BITS);
        
        // Encode message type (3 bits)
        let type_bits = match message.message_type {
            MessageType::Cq => 0u8,
            MessageType::Response => 1u8,
            MessageType::Report => 2u8,
            MessageType::Ack => 3u8,
            MessageType::Final => 4u8,
            MessageType::FreeText => 5u8,
            MessageType::GridOnly => 6u8,
            MessageType::Telemetry => 7u8,
            MessageType::Unknown => 0u8,
        };
        
        for i in (0..3).rev() {
            payload.push((type_bits >> i) & 1 != 0);
        }
        
        // Encode message content based on type
        match message.message_type {
            MessageType::Cq => self.encode_cq_payload(&mut payload, message)?,
            MessageType::Response => self.encode_response_payload(&mut payload, message)?,
            MessageType::Report => self.encode_report_payload(&mut payload, message)?,
            MessageType::Ack | MessageType::Final => self.encode_ack_payload(&mut payload, message)?,
            MessageType::FreeText => self.encode_freetext_payload(&mut payload, message)?,
            MessageType::GridOnly => self.encode_grid_payload(&mut payload, message)?,
            MessageType::Telemetry => self.encode_telemetry_structured_payload(&mut payload, message)?,
            MessageType::Unknown => {
                // Pad with zeros
                while payload.len() < PAYLOAD_BITS {
                    payload.push(false);
                }
            }
        }
        
        // Ensure exactly 77 bits
        payload.resize(PAYLOAD_BITS, false);
        
        Ok(payload)
    }

    /// Encode CQ message payload
    fn encode_cq_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        // Encode callsign (28 bits)
        if let Some(ref callsign) = message.from_callsign {
            let callsign_bits = self.callsign_table.encode_callsign(callsign)?;
            self.append_u32_bits(payload, callsign_bits, 28);
        } else {
            self.append_u32_bits(payload, 0, 28);
        }
        
        // Encode grid square (15 bits)
        if let Some(ref grid) = message.grid_square {
            let grid_bits = self.encode_grid_square(grid)?;
            self.append_u32_bits(payload, grid_bits, 15);
        } else {
            self.append_u32_bits(payload, 0, 15);
        }
        
        // Pad remaining bits
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        
        Ok(())
    }

    /// Encode response message payload
    fn encode_response_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        // Encode to_callsign (28 bits)
        if let Some(ref callsign) = message.to_callsign {
            let callsign_bits = self.callsign_table.encode_callsign(callsign)?;
            self.append_u32_bits(payload, callsign_bits, 28);
        } else {
            self.append_u32_bits(payload, 0, 28);
        }
        
        // Encode from_callsign (28 bits)
        if let Some(ref callsign) = message.from_callsign {
            let callsign_bits = self.callsign_table.encode_callsign(callsign)?;
            self.append_u32_bits(payload, callsign_bits, 28);
        } else {
            self.append_u32_bits(payload, 0, 28);
        }
        
        // Encode grid square (15 bits)
        if let Some(ref grid) = message.grid_square {
            let grid_bits = self.encode_grid_square(grid)?;
            self.append_u32_bits(payload, grid_bits, 15);
        } else {
            self.append_u32_bits(payload, 0, 15);
        }
        
        // Pad remaining bits
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        
        Ok(())
    }

    /// Encode signal report message payload
    fn encode_report_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        // Encode to_callsign (28 bits)
        if let Some(ref callsign) = message.to_callsign {
            let callsign_bits = self.callsign_table.encode_callsign(callsign)?;
            self.append_u32_bits(payload, callsign_bits, 28);
        } else {
            self.append_u32_bits(payload, 0, 28);
        }
        
        // Encode from_callsign (28 bits)
        if let Some(ref callsign) = message.from_callsign {
            let callsign_bits = self.callsign_table.encode_callsign(callsign)?;
            self.append_u32_bits(payload, callsign_bits, 28);
        } else {
            self.append_u32_bits(payload, 0, 28);
        }
        
        // Encode signal report (7 bits, offset by +35)
        let report_value = if let Some(report) = message.signal_report {
            (report + 35) as u32
        } else {
            35 // 0 dB default
        };
        self.append_u32_bits(payload, report_value, 7);
        
        // Pad remaining bits
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        
        Ok(())
    }

    /// Encode acknowledgment message payload
    fn encode_ack_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        // Same structure as response but different type
        self.encode_response_payload(payload, message)
    }

    /// Encode free text message payload
    fn encode_freetext_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        if let Some(ref text) = message.text {
            // Encode each character as 6 bits
            for ch in text.chars().take(MAX_FREETEXT_LENGTH) {
                let char_value = self.encode_character(ch)?;
                self.append_u32_bits(payload, char_value as u32, 6);
            }
        }
        
        // Pad remaining bits
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        
        Ok(())
    }

    /// Encode grid-only message payload
    fn encode_grid_payload(&mut self, payload: &mut BitVec, message: &Ft8Message) -> Ft8Result<()> {
        if let Some(ref grid) = message.grid_square {
            let grid_bits = self.encode_grid_square(grid)?;
            self.append_u32_bits(payload, grid_bits, 15);
        } else {
            self.append_u32_bits(payload, 0, 15);
        }
        
        // Pad remaining bits
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        
        Ok(())
    }

    /// Encode structured telemetry message payload
    fn encode_telemetry_structured_payload(&mut self, payload: &mut BitVec, _message: &Ft8Message) -> Ft8Result<()> {
        // For structured telemetry messages - application specific
        // Pad with zeros for now
        while payload.len() < PAYLOAD_BITS {
            payload.push(false);
        }
        Ok(())
    }

    /// Encode raw telemetry data payload
    fn encode_telemetry_payload(&self, data: &[u8]) -> Ft8Result<BitVec> {
        let mut payload = BitVec::with_capacity(PAYLOAD_BITS);
        
        // Telemetry type (3 bits)
        self.append_u32_bits(&mut payload, 7, 3);
        
        // Encode data bytes
        for &byte in data {
            self.append_u32_bits(&mut payload, byte as u32, 8);
        }
        
        // Pad remaining bits
        payload.resize(PAYLOAD_BITS, false);
        
        Ok(payload)
    }

    /// Generate 79-symbol sequence from LDPC codeword
    fn generate_symbols(&self, ldpc_codeword: &BitSlice) -> Ft8Result<[u8; NUM_SYMBOLS]> {
        if ldpc_codeword.len() != 174 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid LDPC codeword length: {}", ldpc_codeword.len())
            ));
        }
        
        let mut symbols = [0u8; NUM_SYMBOLS];
        
        // First 7 symbols: Costas array
        symbols[0..7].copy_from_slice(&self.costas_arrays.sync1);
        
        // Next 36 symbols: First half of data
        for i in 0..36 {
            let bit_start = i * 3;
            if bit_start + 2 < 58 {
                let symbol_bits = &ldpc_codeword[bit_start..bit_start + 3];
                symbols[7 + i] = self.bits_to_symbol(symbol_bits);
            }
        }
        
        // Middle 7 symbols: Second Costas array
        symbols[43..50].copy_from_slice(&self.costas_arrays.sync2);
        
        // Last 29 symbols: Second half of data
        for i in 0..29 {
            let bit_start = 58 + i * 3;
            if bit_start + 2 < ldpc_codeword.len() {
                let symbol_bits = &ldpc_codeword[bit_start..bit_start + 3];
                symbols[50 + i] = self.bits_to_symbol(symbol_bits);
            }
        }
        
        Ok(symbols)
    }

    /// Convert 3 bits to FT8 symbol (0-7)
    fn bits_to_symbol(&self, bits: &BitSlice) -> u8 {
        let mut symbol = 0u8;
        for (i, bit) in bits.iter().take(3).enumerate() {
            if *bit {
                symbol |= 1 << (2 - i);
            }
        }
        symbol
    }

    /// Append u32 value as bits to BitVec
    fn append_u32_bits(&self, bitvec: &mut BitVec, value: u32, num_bits: usize) {
        for i in (0..num_bits).rev() {
            bitvec.push((value >> i) & 1 != 0);
        }
    }

    /// Encode grid square (4 or 6 character) to 15-bit value
    fn encode_grid_square(&self, grid: &str) -> Ft8Result<u32> {
        if grid.len() < 4 || grid.len() > 6 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid grid square length: {}", grid.len())
            ));
        }
        
        let grid_chars: Vec<char> = grid.to_uppercase().chars().collect();
        
        // Validate grid format
        if !grid_chars[0].is_ascii_alphabetic() || !grid_chars[1].is_ascii_alphabetic() ||
           !grid_chars[2].is_ascii_digit() || !grid_chars[3].is_ascii_digit() {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid grid square format: {}", grid)
            ));
        }
        
        let lon = (grid_chars[0] as u32 - b'A' as u32) * 20 + 
                  (grid_chars[2] as u32 - b'0' as u32) * 2;
        let lat = (grid_chars[1] as u32 - b'A' as u32) * 10 + 
                  (grid_chars[3] as u32 - b'0' as u32);
        
        let grid_value = lat * 180 + lon;
        
        if grid_value >= (1 << 15) {
            return Err(Ft8Error::MessageDecodingError(
                format!("Grid square value out of range: {}", grid_value)
            ));
        }
        
        Ok(grid_value)
    }

    /// Encode character to 6-bit value
    fn encode_character(&self, ch: char) -> Ft8Result<u8> {
        match ch {
            ' ' => Ok(0),
            'A'..='Z' => Ok((ch as u8) - b'A' + 1),
            '0'..='9' => Ok((ch as u8) - b'0' + 27),
            '+' => Ok(37),
            '-' => Ok(38),
            '.' => Ok(39),
            '/' => Ok(40),
            '?' => Ok(41),
            _ => Err(Ft8Error::MessageDecodingError(
                format!("Invalid character for FT8 encoding: '{}'", ch)
            )),
        }
    }

    /// Check if character is valid for free text
    fn is_valid_freetext_char(&self, ch: char) -> bool {
        matches!(ch, ' ' | 'A'..='Z' | '0'..='9' | '+' | '-' | '.' | '/' | '?')
    }

    /// Check if string is a valid grid square
    fn is_grid_square(&self, s: &str) -> bool {
        if s.len() != 4 && s.len() != 6 {
            return false;
        }
        
        let chars: Vec<char> = s.to_uppercase().chars().collect();
        chars[0].is_ascii_alphabetic() && chars[1].is_ascii_alphabetic() &&
        chars[2].is_ascii_digit() && chars[3].is_ascii_digit()
    }
}

impl Default for Ft8Encoder {
    fn default() -> Self {
        Self::new()
    }
}

/// LDPC encoder for FT8 error correction
struct LdpcEncoder {
    // Generator matrix and encoding tables would go here
    // For now, simplified implementation
}

impl LdpcEncoder {
    fn new() -> Self {
        Self {}
    }
    
    /// Encode 91-bit message to 174-bit LDPC codeword
    fn encode(&self, message_bits: &BitSlice) -> Ft8Result<BitVec> {
        if message_bits.len() != 91 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid LDPC input length: {}", message_bits.len())
            ));
        }
        
        // Simplified LDPC encoding - in practice this would use the actual FT8 LDPC matrix
        let mut codeword = BitVec::with_capacity(174);
        codeword.extend_from_bitslice(message_bits);
        
        // Add parity bits (simplified)
        while codeword.len() < 174 {
            // XOR selected information bits to generate parity
            let parity = codeword[0] ^ codeword[1] ^ codeword[2];
            codeword.push(parity);
        }
        
        Ok(codeword)
    }
}

/// Costas arrays for FT8 synchronization
struct CostasArrays {
    sync1: [u8; 7],
    sync2: [u8; 7],
}

impl CostasArrays {
    fn new() -> Self {
        Self {
            sync1: [3, 1, 4, 0, 6, 5, 2], // First Costas array
            sync2: [2, 5, 6, 0, 4, 1, 3], // Second Costas array  
        }
    }
}

/// Callsign encoding table for FT8
struct CallsignEncodingTable {
    // Standard callsign encoding maps
    standard_callsigns: HashMap<String, u32>,
    hash_callsigns: HashMap<String, u32>,
    next_hash_value: u32,
}

impl CallsignEncodingTable {
    fn new() -> Self {
        Self {
            standard_callsigns: HashMap::new(),
            hash_callsigns: HashMap::new(),
            next_hash_value: 0,
        }
    }
    
    /// Encode callsign to 28-bit value
    fn encode_callsign(&mut self, callsign: &str) -> Ft8Result<u32> {
        let callsign = callsign.to_uppercase();
        
        // Validate callsign format
        if !self.is_valid_callsign(&callsign) {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid callsign format: {}", callsign)
            ));
        }
        
        // Check standard encoding first
        if let Some(&value) = self.standard_callsigns.get(&callsign) {
            return Ok(value);
        }
        
        // Try standard callsign encoding pattern
        if let Ok(value) = self.encode_standard_callsign(&callsign) {
            self.standard_callsigns.insert(callsign.clone(), value);
            return Ok(value);
        }
        
        // Fall back to hash encoding
        if let Some(&value) = self.hash_callsigns.get(&callsign) {
            return Ok(value + 262_144_000);
        }
        
        // Create new hash entry
        let hash_value = self.next_hash_value;
        self.hash_callsigns.insert(callsign, hash_value);
        self.next_hash_value += 1;
        
        Ok(hash_value + 262_144_000)
    }
    
    /// Encode standard format callsign
    fn encode_standard_callsign(&self, callsign: &str) -> Ft8Result<u32> {
        // Simplified standard callsign encoding
        // Real implementation would follow FT8 specification exactly
        let mut value = 0u32;
        
        for (i, ch) in callsign.chars().enumerate() {
            if i >= 6 { break; }
            
            let char_value = match ch {
                'A'..='Z' => (ch as u32) - ('A' as u32) + 1,
                '0'..='9' => (ch as u32) - ('0' as u32) + 27,
                _ => 0,
            };
            
            value = value * 37 + char_value;
        }
        
        if value < 262_144_000 {
            Ok(value)
        } else {
            Err(Ft8Error::MessageDecodingError(
                "Callsign encoding overflow".to_string()
            ))
        }
    }
    
    /// Validate callsign format
    fn is_valid_callsign(&self, callsign: &str) -> bool {
        if callsign.is_empty() || callsign.len() > 6 {
            return false;
        }
        
        callsign.chars().all(|c| c.is_ascii_alphanumeric())
    }
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
        let encoder = Ft8Encoder::new();
        assert!(encoder.callsign_table.standard_callsigns.is_empty());
    }

    #[test]
    fn test_encode_cq_message() {
        let mut encoder = Ft8Encoder::new();
        let result = encoder.encode_cq("W1ABC", "FN42", false);
        assert!(result.is_ok());
        
        let symbols = result.unwrap();
        assert_eq!(symbols.len(), NUM_SYMBOLS);
        assert!(symbols.iter().all(|&s| s < NUM_TONES as u8));
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
    fn test_grid_square_encoding() {
        let encoder = Ft8Encoder::new();
        let result = encoder.encode_grid_square("FN42");
        assert!(result.is_ok());
        
        let value = result.unwrap();
        assert!(value < (1 << 15));
    }

    #[test]
    fn test_character_encoding() {
        let encoder = Ft8Encoder::new();
        assert_eq!(encoder.encode_character(' ').unwrap(), 0);
        assert_eq!(encoder.encode_character('A').unwrap(), 1);
        assert_eq!(encoder.encode_character('0').unwrap(), 27);
        assert_eq!(encoder.encode_character('+').unwrap(), 37);
    }

    #[test]
    fn test_costas_arrays() {
        let costas = CostasArrays::new();
        assert_eq!(costas.sync1.len(), 7);
        assert_eq!(costas.sync2.len(), 7);
        assert!(costas.sync1.iter().all(|&s| s < 8));
        assert!(costas.sync2.iter().all(|&s| s < 8));
    }

    #[test]
    fn test_bits_to_symbol() {
        let encoder = Ft8Encoder::new();
        let bits = bitvec![0, 0, 0];
        assert_eq!(encoder.bits_to_symbol(&bits), 0);
        
        let bits = bitvec![1, 1, 1];
        assert_eq!(encoder.bits_to_symbol(&bits), 7);
        
        let bits = bitvec![1, 0, 1];
        assert_eq!(encoder.bits_to_symbol(&bits), 5);
    }

    #[test]
    fn test_message_parsing() {
        let mut encoder = Ft8Encoder::new();
        
        let cq_msg = encoder.parse_message_text("CQ W1ABC FN42", None).unwrap();
        assert_eq!(cq_msg.message_type, MessageType::Cq);
        assert_eq!(cq_msg.from_callsign.as_deref(), Some("W1ABC"));
        assert_eq!(cq_msg.grid_square.as_deref(), Some("FN42"));
        
        let resp_msg = encoder.parse_message_text("W1ABC K1DEF FN42", None).unwrap();
        assert_eq!(resp_msg.message_type, MessageType::Response);
        
        let report_msg = encoder.parse_message_text("K1DEF W1ABC -12", None).unwrap();
        assert_eq!(report_msg.message_type, MessageType::Report);
        assert_eq!(report_msg.signal_report, Some(-12));
    }

    #[test]
    fn test_ldpc_encoding() {
        let encoder = LdpcEncoder::new();
        let message_bits = bitvec![0; 91];
        let result = encoder.encode(&message_bits);
        assert!(result.is_ok());
        
        let codeword = result.unwrap();
        assert_eq!(codeword.len(), 174);
    }
}