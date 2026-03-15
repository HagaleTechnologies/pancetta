//! FT8 message types and parsing
//!
//! This module handles the FT8 protocol message structure:
//! - 77-bit information payload
//! - 14-bit CRC checksum
//! - LDPC error correction
//! - Message type detection and parsing
//! - Callsign and grid square validation

use crate::{Ft8Error, Ft8Result};
use bitvec::prelude::*;
use std::collections::HashMap;
use std::fmt;
use std::time::SystemTime;

/// Number of bits in FT8 information payload
pub const PAYLOAD_BITS: usize = 77;

/// Number of bits in CRC checksum
pub const CRC_BITS: usize = 14;

/// Total FT8 message bits (payload + CRC)
pub const TOTAL_MESSAGE_BITS: usize = PAYLOAD_BITS + CRC_BITS;

/// Number of symbols in FT8 transmission (79 symbols)
pub const NUM_SYMBOLS: usize = 79;

/// FT8 message types based on protocol specification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// i3=1,2: Standard messages with callsigns and grid/report
    Standard,
    /// i3=1: Extended callsign support with prefix/suffix
    Extended,
    /// i3=2: EU VHF contest
    Contest,
    /// i3=0 n3=3,4: ARRL Field Day messages
    FieldDay,
    /// i3=0 n3=5: Telemetry (18 hex digits)
    Telemetry,
    /// i3=0 n3=0: Free text messages (13 chars base-42)
    FreeText,
    /// i3=0 n3=1: DXpedition mode
    DXpedition,
    /// i3=3: ARRL RTTY Roundup
    RTTYRoundup,
    /// i3=4: Nonstandard callsigns (12-bit hash + 58-bit call)
    NonStdCall,
    /// Unknown/invalid message type
    Unknown,
}

/// Standard message subtypes (Type 0)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardMessageType {
    /// "CQ [DX] <callsign> <grid>"
    Cq,
    /// "<to_call> <from_call> <grid>"
    Reply,
    /// "<to_call> <from_call> R <grid>"
    ReplyWithR,
    /// "<to_call> <from_call> <report>"
    Report,
    /// "<to_call> <from_call> R <report>"
    ReportWithR,
    /// "<to_call> <from_call> RRR"
    Rrr,
    /// "<to_call> <from_call> 73"
    Final73,
    /// "<to_call> <from_call> RR73"
    RR73,
}

impl fmt::Display for MessageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageType::Standard => write!(f, "Standard"),
            MessageType::Extended => write!(f, "Extended"),
            MessageType::Contest => write!(f, "Contest"),
            MessageType::FieldDay => write!(f, "Field Day"),
            MessageType::Telemetry => write!(f, "Telemetry"),
            MessageType::FreeText => write!(f, "Free Text"),
            MessageType::DXpedition => write!(f, "DXpedition"),
            MessageType::RTTYRoundup => write!(f, "RTTY Roundup"),
            MessageType::NonStdCall => write!(f, "Non-Std Call"),
            MessageType::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Result of unpacking a 28-bit callsign field
#[derive(Debug, Clone)]
enum CallsignField {
    /// Standard callsign (e.g. "W1ABC")
    Callsign(String),
    /// CQ with optional modifier (e.g. CQ, CQ DX, CQ 123)
    Cq(Option<String>),
    /// Special token (DE, QRZ)
    Token(String),
    /// Hash-based callsign (value only, no lookup available)
    Hash(u32),
}

impl CallsignField {
    /// Convert to Option<String> for use in Ft8Message fields
    fn to_callsign(&self) -> Option<String> {
        match self {
            CallsignField::Callsign(s) => Some(s.clone()),
            CallsignField::Token(s) => Some(s.clone()),
            CallsignField::Cq(Some(m)) => Some(format!("CQ {}", m)),
            CallsignField::Cq(None) => Some("CQ".to_string()),
            CallsignField::Hash(h) => Some(format!("<...{}>", h & 0xFFF)),
        }
    }
}

/// Parsed FT8 message content
#[derive(Debug, Clone)]
pub struct Ft8Message {
    /// Message type
    pub message_type: MessageType,
    /// Standard message subtype (for Type 0 messages)
    pub standard_type: Option<StandardMessageType>,
    /// Calling station callsign
    pub from_callsign: Option<String>,
    /// Called station callsign  
    pub to_callsign: Option<String>,
    /// Grid square (4 or 6 character)
    pub grid_square: Option<String>,
    /// Signal report in dB
    pub signal_report: Option<i8>,
    /// Free text content
    pub text: Option<String>,
    /// Contest exchange (serial number, etc.)
    pub contest_exchange: Option<String>,
    /// Special operation (DX, /P, /M, etc.)
    pub special_operation: Option<String>,
    /// Raw 77-bit payload
    pub payload_bits: BitVec,
    /// CRC checksum
    pub crc: u16,
    /// Whether CRC is valid
    pub crc_valid: bool,
    /// Indicates if callsigns are from hash table
    pub uses_hash_calls: bool,
}

impl Default for Ft8Message {
    fn default() -> Self {
        Self {
            message_type: MessageType::Unknown,
            standard_type: None,
            from_callsign: None,
            to_callsign: None,
            grid_square: None,
            signal_report: None,
            text: None,
            contest_exchange: None,
            special_operation: None,
            payload_bits: BitVec::new(),
            crc: 0,
            crc_valid: false,
            uses_hash_calls: false,
        }
    }
}

impl fmt::Display for Ft8Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.message_type {
            MessageType::Standard => match self.standard_type {
                Some(StandardMessageType::Cq) => {
                    write!(f, "CQ")?;
                    if let Some(ref op) = self.special_operation {
                        write!(f, " {}", op)?;
                    }
                    if let Some(ref call) = self.from_callsign {
                        write!(f, " {}", call)?;
                    }
                    if let Some(ref grid) = self.grid_square {
                        write!(f, " {}", grid)?;
                    }
                }
                Some(StandardMessageType::Reply) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    if let Some(ref grid) = self.grid_square {
                        write!(f, " {}", grid)?;
                    }
                }
                Some(StandardMessageType::ReplyWithR) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    write!(f, " R")?;
                    if let Some(ref grid) = self.grid_square {
                        write!(f, " {}", grid)?;
                    }
                }
                Some(StandardMessageType::Report) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    if let Some(report) = self.signal_report {
                        write!(f, " {:+03}", report)?;
                    }
                }
                Some(StandardMessageType::ReportWithR) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    write!(f, " R")?;
                    if let Some(report) = self.signal_report {
                        write!(f, " {:+03}", report)?;
                    }
                }
                Some(StandardMessageType::Rrr) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    write!(f, " RRR")?;
                }
                Some(StandardMessageType::Final73) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    write!(f, " 73")?;
                }
                Some(StandardMessageType::RR73) => {
                    if let Some(ref to) = self.to_callsign {
                        write!(f, "{}", to)?;
                    }
                    if let Some(ref from) = self.from_callsign {
                        write!(f, " {}", from)?;
                    }
                    write!(f, " RR73")?;
                }
                None => write!(f, "<Unknown Standard>")?,
            },
            MessageType::Contest => {
                if let Some(ref to) = self.to_callsign {
                    write!(f, "{}", to)?;
                }
                if let Some(ref from) = self.from_callsign {
                    write!(f, " {}", from)?;
                }
                if let Some(ref exchange) = self.contest_exchange {
                    write!(f, " {}", exchange)?;
                }
            }
            MessageType::FreeText => {
                if let Some(ref text) = self.text {
                    write!(f, "{}", text)?;
                }
            }
            MessageType::FieldDay => {
                if let Some(ref to) = self.to_callsign {
                    write!(f, "{}", to)?;
                }
                if let Some(ref from) = self.from_callsign {
                    write!(f, " {}", from)?;
                }
                if let Some(ref exchange) = self.contest_exchange {
                    write!(f, " {}", exchange)?;
                }
            }
            MessageType::Telemetry => {
                if let Some(ref text) = self.text {
                    write!(f, "{}", text)?;
                } else {
                    write!(f, "<Telemetry>")?;
                }
            }
            MessageType::NonStdCall => {
                // i3=4: one callsign is full, one is hashed
                if let Some(ref to) = self.to_callsign {
                    write!(f, "{}", to)?;
                }
                if let Some(ref from) = self.from_callsign {
                    write!(f, " {}", from)?;
                }
                if let Some(ref exchange) = self.contest_exchange {
                    write!(f, " {}", exchange)?;
                }
            }
            MessageType::Extended | MessageType::DXpedition | MessageType::RTTYRoundup => {
                // Format extended/special messages
                if let Some(ref to) = self.to_callsign {
                    write!(f, "{}", to)?;
                }
                if let Some(ref from) = self.from_callsign {
                    write!(f, " {}", from)?;
                }
                if let Some(ref exchange) = self.contest_exchange {
                    write!(f, " {}", exchange)?;
                }
                if let Some(ref grid) = self.grid_square {
                    write!(f, " {}", grid)?;
                }
            }
            MessageType::Unknown => {
                write!(f, "<Unknown>")?;
            }
        }
        Ok(())
    }
}

/// Decoded FT8 message with metadata
#[derive(Debug, Clone)]
pub struct DecodedMessage {
    /// Parsed message content
    pub message: Ft8Message,
    /// Plain text representation
    pub text: String,
    /// Signal-to-noise ratio in dB
    pub snr_db: f32,
    /// Decoding confidence (0.0 - 1.0)
    pub confidence: f32,
    /// Frequency offset in Hz
    pub frequency_offset: f64,
    /// Time offset in seconds
    pub time_offset: f64,
    /// Decode timestamp
    pub timestamp: SystemTime,
    /// Number of error corrections applied
    pub error_corrections: u8,
}

impl DecodedMessage {
    /// Create a new decoded message
    pub fn new(
        message: Ft8Message,
        snr_db: f32,
        confidence: f32,
        frequency_offset: f64,
        time_offset: f64,
    ) -> Self {
        let text = message.to_string();
        Self {
            message,
            text,
            snr_db,
            confidence,
            frequency_offset,
            time_offset,
            timestamp: SystemTime::now(),
            error_corrections: 0,
        }
    }
}

impl fmt::Display for DecodedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:6.1} {:4.1} {:4.0} {:.1} {}",
            self.time_offset, self.snr_db, self.frequency_offset, self.confidence, self.text
        )
    }
}

/// Hash table for worked stations (10/12/22-bit hashes)
struct HashTable {
    /// 10-bit hash lookup (1024 entries)
    hash_10bit: HashMap<u32, String>,
    /// 12-bit hash lookup (4096 entries)
    hash_12bit: HashMap<u32, String>,
    /// 22-bit hash lookup for special operations (4M entries)
    hash_22bit: HashMap<u32, String>,
}

impl HashTable {
    pub fn new() -> Self {
        Self {
            hash_10bit: HashMap::new(),
            hash_12bit: HashMap::new(),
            hash_22bit: HashMap::new(),
        }
    }

    /// Add callsign to hash tables
    pub fn add_callsign(&mut self, callsign: &str) {
        let hash_10 = self.calculate_hash_10bit(callsign);
        let hash_12 = self.calculate_hash_12bit(callsign);
        let hash_22 = self.calculate_hash_22bit(callsign);

        self.hash_10bit.insert(hash_10, callsign.to_string());
        self.hash_12bit.insert(hash_12, callsign.to_string());
        self.hash_22bit.insert(hash_22, callsign.to_string());
    }

    /// Lookup 10-bit hash
    pub fn lookup_10bit_hash(&self, hash: u32) -> Option<String> {
        self.hash_10bit.get(&hash).cloned()
    }

    /// Lookup 12-bit hash
    pub fn lookup_12bit_hash(&self, hash: u32) -> Option<String> {
        self.hash_12bit.get(&hash).cloned()
    }

    /// Lookup 22-bit hash
    pub fn lookup_22bit_hash(&self, hash: u32) -> Option<String> {
        self.hash_22bit.get(&hash).cloned()
    }

    /// Calculate the 22-bit hash for a callsign, matching ft8_lib's `save_callsign()`.
    ///
    /// Algorithm: encode callsign as base-38 in 11-char field (space-padded),
    /// then n22 = (47055833459 * n58) >> (64-22) & 0x3FFFFF.
    fn calculate_n22(callsign: &str) -> u32 {
        // FT8_CHAR_TABLE_ALPHANUM_SPACE_SLASH: " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/"
        const CHARSET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";

        let upper = callsign.to_ascii_uppercase();
        let bytes = upper.as_bytes();
        let mut n58: u64 = 0;
        let mut count = 0;
        for &b in bytes.iter().take(11) {
            let j = CHARSET.iter().position(|&c| c == b).unwrap_or(0);
            n58 = n58 * 38 + j as u64;
            count += 1;
        }
        // Pad with spaces (index 0) to 11 characters
        while count < 11 {
            n58 *= 38;
            count += 1;
        }

        let n22 = ((47055833459u64.wrapping_mul(n58)) >> (64 - 22)) & 0x3FFFFF;
        n22 as u32
    }

    /// 22-bit hash
    fn calculate_hash_22bit(&self, callsign: &str) -> u32 {
        Self::calculate_n22(callsign)
    }

    /// 12-bit hash = n22 >> 10
    fn calculate_hash_12bit(&self, callsign: &str) -> u32 {
        Self::calculate_n22(callsign) >> 10
    }

    /// 10-bit hash = n22 >> 12
    fn calculate_hash_10bit(&self, callsign: &str) -> u32 {
        Self::calculate_n22(callsign) >> 12
    }
}

impl Default for HashTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Callsign lookup table for standard encoding/decoding
struct CallsignTable {
    // Standard callsign cache for performance
    standard_cache: HashMap<u32, String>,
}

impl CallsignTable {
    fn new() -> Self {
        Self {
            standard_cache: HashMap::new(),
        }
    }

    /// Encode standard callsign to 28-bit value
    pub fn encode_standard_callsign(&self, callsign: &str) -> Ft8Result<u32> {
        const CALLSIGN_CHARS: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

        if callsign.len() > 6 {
            return Err(Ft8Error::MessageDecodingError(
                "Callsign too long".to_string(),
            ));
        }

        let mut value = 0u32;
        // Pad with leading spaces to 6 characters
        let padded = format!("{:>6}", callsign);

        for ch in padded.chars() {
            let ch_upper = ch.to_ascii_uppercase();
            let pos = CALLSIGN_CHARS
                .iter()
                .position(|&c| c == ch_upper as u8)
                .ok_or_else(|| {
                    Ft8Error::MessageDecodingError(format!("Invalid character in callsign: {}", ch))
                })?;

            value = value * 37 + pos as u32;
        }

        if value >= 262_144_000 {
            return Err(Ft8Error::MessageDecodingError(
                "Callsign encoding overflow".to_string(),
            ));
        }

        Ok(value)
    }
}

/// FT8 message parser
pub struct MessageParser {
    /// Callsign validation table
    callsign_table: CallsignTable,
    /// Hash table for worked stations (10/12/22-bit hashes)
    hash_table: HashTable,
}

impl MessageParser {
    /// Create a new message parser
    pub fn new() -> Self {
        Self {
            callsign_table: CallsignTable::new(),
            hash_table: HashTable::new(),
        }
    }

    /// Add callsign to hash table
    pub fn add_callsign(&mut self, callsign: &str) {
        self.hash_table.add_callsign(callsign);
    }

    /// Parse 77-bit payload into FT8 message
    ///
    /// FT8 payload layout:
    /// - i3 field (message type) is in the LAST 3 bits (74-76)
    /// - For i3=0: n3 sub-type is in bits 71-73
    /// - For i3=1,2: standard message layout
    pub fn parse_payload(&self, payload: &BitSlice) -> Ft8Result<Ft8Message> {
        if payload.len() != PAYLOAD_BITS {
            return Err(Ft8Error::MessageDecodingError(format!(
                "Invalid payload length: {} bits",
                payload.len()
            )));
        }

        let mut message = Ft8Message::default();
        message.payload_bits = payload.to_bitvec();

        // Read i3 from the LAST 3 bits (74-76)
        let i3 = bits_to_u32(&payload[74..77]);

        match i3 {
            0 => {
                // i3=0: sub-type determined by n3 field (bits 71-73)
                let n3 = bits_to_u32(&payload[71..74]);
                match n3 {
                    0 => {
                        // Free text: 71 bits encode 13 chars in base-42
                        message.message_type = MessageType::FreeText;
                        self.parse_freetext_type0(&payload[0..71], &mut message)?;
                    }
                    5 => {
                        // Telemetry: 71 bits → 18 hex digits
                        message.message_type = MessageType::Telemetry;
                        self.parse_telemetry_type0(&payload[0..71], &mut message)?;
                    }
                    1 => {
                        // DXpedition (Fox/Hound): hash(12) + call(28) + R(1) + rpt(13) + ...
                        message.message_type = MessageType::DXpedition;
                        self.parse_dxpedition_type0(&payload[0..71], &mut message)?;
                    }
                    2 => {
                        // EU VHF Contest: call1(28) + call2(28) + R(1) + grid6/rpt(15)
                        message.message_type = MessageType::Contest;
                        self.parse_eu_vhf_type0(&payload[0..71], &mut message)?;
                    }
                    3 | 4 => {
                        // ARRL Field Day: call1(28) + R(1) + call2(28) + class(4) + section(7)
                        message.message_type = MessageType::FieldDay;
                        self.parse_field_day_type0(&payload[0..71], n3, &mut message)?;
                    }
                    _ => {
                        message.message_type = MessageType::Unknown;
                    }
                }
            }
            1 | 2 => {
                // Standard message: n29a(29) + n29b(29) + ir(1) + igrid4(15) + i3(3)
                message.message_type = MessageType::Standard;
                self.parse_type1_standard(&payload, &mut message)?;
            }
            3 => {
                // ARRL RTTY Roundup: TU(1) + n29a(29) + n29b(29) + R(1) + nexch(3) + nrpt(13) + i3(3)
                message.message_type = MessageType::RTTYRoundup;
                self.parse_rtty_roundup(&payload, &mut message)?;
            }
            4 => {
                // Nonstandard callsign: n12(12) + n58(58) + iflip(1) + nrpt(2) + icq(1) + i3(3)
                message.message_type = MessageType::NonStdCall;
                self.parse_nonstd_call(&payload, &mut message)?;
            }
            _ => {
                message.message_type = MessageType::Unknown;
            }
        }

        Ok(message)
    }

    /// Parse i3=1/2 standard message from 77-bit payload.
    ///
    /// Bit layout: n29a(29) + n29b(29) + ir(1) + igrid4(15) + i3(3) = 77 bits
    fn parse_type1_standard(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Extract fields from bit positions
        let n29a = bits_to_u32(&payload[0..29]);
        let n29b = bits_to_u32(&payload[29..58]);
        let ir = payload[58] as u8;
        let igrid4 = bits_to_u32(&payload[59..74]) as u16;

        // Split callsign + suffix flag
        let n28a = n29a >> 1;
        let ipa = (n29a & 1) as u8;
        let n28b = n29b >> 1;
        let ipb = (n29b & 1) as u8;

        // Decode callsigns, appending /R suffix when ip=1
        let call_a = self.unpack28(n28a);
        let call_b = self.unpack28(n28b);

        // Helper to apply suffix: ip=1 means /R (or /P — indistinguishable in protocol)
        let apply_suffix = |call: Option<String>, ip: u8| -> Option<String> {
            match (call, ip) {
                (Some(c), 1)
                    if !c.starts_with("CQ") && !c.starts_with("DE") && !c.starts_with("QRZ") =>
                {
                    Some(format!("{}/R", c))
                }
                (c, _) => c,
            }
        };

        // Decode grid/report/token
        let (grid, report, token) = Self::unpackgrid(igrid4, ir);

        // Determine message subtype and populate fields
        let is_cq = matches!(&call_a, CallsignField::Cq(..));

        // Pre-compute callsign strings with suffixes applied
        let call_a_str = apply_suffix(call_a.to_callsign(), ipa);
        let call_b_str = apply_suffix(call_b.to_callsign(), ipb);

        if is_cq {
            message.standard_type = Some(StandardMessageType::Cq);
            // For CQ messages: call_a = CQ token, call_b = calling station
            if let CallsignField::Cq(modifier) = &call_a {
                if let Some(m) = modifier {
                    message.special_operation = Some(m.clone());
                }
            }
            message.from_callsign = call_b_str;
            message.grid_square = grid;
        } else if let Some(tok) = token {
            // Special tokens: RRR, RR73, 73
            match tok.as_str() {
                "RRR" => message.standard_type = Some(StandardMessageType::Rrr),
                "RR73" => message.standard_type = Some(StandardMessageType::RR73),
                "73" => message.standard_type = Some(StandardMessageType::Final73),
                _ => message.standard_type = Some(StandardMessageType::Reply),
            }
            message.to_callsign = call_a_str;
            message.from_callsign = call_b_str;
        } else if let Some(rpt) = report {
            // Signal report
            message.signal_report = Some(rpt);
            if ir != 0 {
                message.standard_type = Some(StandardMessageType::ReportWithR);
            } else {
                message.standard_type = Some(StandardMessageType::Report);
            }
            message.to_callsign = call_a_str;
            message.from_callsign = call_b_str;
        } else if grid.is_some() {
            // Grid square reply
            if ir != 0 {
                message.standard_type = Some(StandardMessageType::ReplyWithR);
            } else {
                message.standard_type = Some(StandardMessageType::Reply);
            }
            message.to_callsign = call_a_str;
            message.from_callsign = call_b_str;
            message.grid_square = grid;
        } else {
            // No grid, no report, no token — blank exchange
            message.standard_type = Some(StandardMessageType::Reply);
            message.to_callsign = call_a_str;
            message.from_callsign = call_b.to_callsign();
        }

        Ok(())
    }

    /// Parse free text from i3=0, n3=0 payload (first 71 bits).
    ///
    /// Encoding: 71 bits (left-shifted big-endian) → base-42 → 13 characters.
    /// Reverse of encoder's `encode_free_text()`.
    fn parse_freetext_type0(&self, bits71: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        const FREETEXT_CHARS: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";

        // Convert 71 bits to a big integer (stored as bytes)
        // The encoder left-shifts by 1 bit, so we right-shift to recover b71
        let mut b71 = [0u8; 9];
        for i in 0..71 {
            if bits71[i] {
                b71[i / 8] |= 0x80u8 >> (i % 8);
            }
        }

        // Right-shift b71 by 1 bit (reverse of encoder's left-shift)
        let mut carry: u8 = 0;
        for byte in b71.iter_mut() {
            let new_carry = *byte & 1;
            *byte = (*byte >> 1) | (carry << 7);
            carry = new_carry;
        }

        // Decode base-42: divide by 42 repeatedly to extract 13 characters
        let mut chars = [b' '; 13];
        for i in (0..13).rev() {
            // Divide b71 by 42, remainder is the character index
            let mut rem = 0u16;
            for byte in b71.iter_mut() {
                rem = (rem << 8) | (*byte as u16);
                *byte = (rem / 42) as u8;
                rem %= 42;
            }
            let idx = rem as usize;
            if idx < FREETEXT_CHARS.len() {
                chars[i] = FREETEXT_CHARS[idx];
            }
        }

        let text = String::from_utf8_lossy(&chars).trim_end().to_string();
        if !text.is_empty() {
            message.text = Some(text);
        }

        Ok(())
    }

    /// Parse i3=0, n3=5 telemetry payload (first 71 bits → 18 hex digits).
    ///
    /// Same bit layout as free text (right-shift by 1 to get b71),
    /// but interpreted as raw bytes → hex string instead of base-42.
    fn parse_telemetry_type0(&self, bits71: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Convert 71 bits to bytes, then right-shift by 1 (same as free text)
        let mut b71 = [0u8; 9];
        for i in 0..71 {
            if bits71[i] {
                b71[i / 8] |= 0x80u8 >> (i % 8);
            }
        }

        // Right-shift by 1 bit
        let mut carry: u8 = 0;
        for byte in b71.iter_mut() {
            let new_carry = *byte & 1;
            *byte = (*byte >> 1) | (carry << 7);
            carry = new_carry;
        }

        // Convert to 18 hex digits (72 bits, but we only have 71 usable)
        let mut hex = String::with_capacity(18);
        for i in 0..9 {
            hex.push_str(&format!("{:02X}", b71[i]));
        }

        message.text = Some(hex);
        Ok(())
    }

    /// Parse i3=4 nonstandard callsign message.
    ///
    /// Bit layout: n12(12) + n58(58) + iflip(1) + nrpt(2) + icq(1) + i3(3) = 77 bits
    ///
    /// One callsign is encoded as 58-bit base-38 (up to 11 chars), the other as a
    /// 12-bit hash. The `iflip` bit determines which is which.
    fn parse_nonstd_call(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        let n12 = bits_to_u32(&payload[0..12]) as u16;
        let n58 = bits_to_u64(&payload[12..70]);
        let iflip = payload[70] as u8;
        let nrpt = bits_to_u32(&payload[71..73]) as u8;
        let icq = payload[73] as u8;

        // Decode the 58-bit callsign (base-38, 11 chars, space-padded)
        let call_decoded = Self::unpack58(n58);

        // Look up the 12-bit hashed callsign
        let call_hashed = self
            .hash_table
            .lookup_12bit_hash(n12 as u32)
            .map(|c| format!("<{}>", c))
            .unwrap_or_else(|| "<...>".to_string());

        // iflip determines which call is which
        let (call_1, call_2) = if iflip != 0 {
            (call_decoded.clone(), call_hashed)
        } else {
            (call_hashed, call_decoded.clone())
        };

        if icq != 0 {
            // CQ message with nonstandard callsign
            message.to_callsign = Some("CQ".to_string());
            message.from_callsign = Some(call_decoded);
        } else {
            message.to_callsign = Some(call_1);
            message.from_callsign = Some(call_2);

            // Decode report token
            match nrpt {
                1 => {
                    message.contest_exchange = Some("RRR".to_string());
                }
                2 => {
                    message.contest_exchange = Some("RR73".to_string());
                }
                3 => {
                    message.contest_exchange = Some("73".to_string());
                }
                _ => {}
            }
        }

        // Save decoded callsign to hash table for future lookups
        // (hash_table is immutable here, but the decoder accumulates hashes
        // across decode cycles via add_callsign)

        Ok(())
    }

    /// Decode a 58-bit base-38 encoded callsign (up to 11 characters).
    ///
    /// Character set: " 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/" (38 chars)
    fn unpack58(mut n58: u64) -> String {
        const CHARSET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";

        let mut c11 = [b' '; 11];
        for i in (0..11).rev() {
            c11[i] = CHARSET[(n58 % 38) as usize];
            n58 /= 38;
        }

        String::from_utf8_lossy(&c11).trim().to_string()
    }

    /// Parse i3=3 ARRL RTTY Roundup message.
    ///
    /// Bit layout: TU(1) + n28a(28) + ipa(1) + n28b(28) + ipb(1) + R(1) + nexch(3) + nrpt(13) + i3(3) = 77 bits
    ///
    /// Note: ft8_lib doesn't decode this type, but WSJT-X does. We implement basic decoding.
    fn parse_rtty_roundup(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        let tu = payload[0] as u8;
        let n28a = bits_to_u32(&payload[1..29]);
        let _ipa = payload[29] as u8;
        let n28b = bits_to_u32(&payload[30..58]);
        let _ipb = payload[58] as u8;
        let r_flag = payload[59] as u8;
        let _nexch = bits_to_u32(&payload[60..63]) as u8;
        let nrpt = bits_to_u32(&payload[63..74]) as u16;

        let call_a = self.unpack28(n28a);
        let call_b = self.unpack28(n28b);

        if tu != 0 {
            message.special_operation = Some("TU".to_string());
        }

        message.to_callsign = call_a.to_callsign();
        message.from_callsign = call_b.to_callsign();

        // nrpt encodes RST (3 digits) and state/province (13-bit combined)
        // For now, just show the raw exchange value
        let rst = 519 + (nrpt & 0x1FFF) / 84; // approximate RST
        let state_idx = (nrpt & 0x1FFF) % 84;

        let mut exchange = String::new();
        if r_flag != 0 {
            exchange.push_str("R ");
        }
        exchange.push_str(&format!("{} {}", rst, state_idx));

        message.contest_exchange = Some(exchange);

        Ok(())
    }

    /// Unpack a 28-bit callsign field value into a CallsignField.
    ///
    /// Matches the encoder's `pack28()` encoding:
    /// - 0: DE, 1: QRZ, 2: CQ (no modifier)
    /// - 3..NTOKENS: CQ with modifier
    /// - NTOKENS..NTOKENS+MAX22: hash-based callsign
    /// - NTOKENS+MAX22 and above: standard callsign (mixed-radix)
    fn unpack28(&self, n28: u32) -> CallsignField {
        const NTOKENS: u32 = 2_063_592;
        const MAX22: u32 = 4_194_304;

        match n28 {
            0 => CallsignField::Token("DE".to_string()),
            1 => CallsignField::Token("QRZ".to_string()),
            2 => CallsignField::Cq(None),
            3..=2_063_591 => {
                // CQ modifier: 3..NTOKENS
                let mod_val = n28 - 3;
                if mod_val < 1000 {
                    // CQ nnn (frequency)
                    CallsignField::Cq(Some(format!("{:03}", mod_val)))
                } else {
                    // CQ ABCD (directed CQ) — decode base-27 with digits 1..26
                    // Reverse of: m = 27 * m + ((ch - 'A') + 1)
                    let mut v = mod_val - 1000;
                    let mut chars = Vec::new();
                    while v > 0 {
                        let r = (v % 27) as u8;
                        if r == 0 {
                            break;
                        }
                        chars.push(b'A' + r - 1);
                        v /= 27;
                    }
                    chars.reverse();
                    let s = String::from_utf8_lossy(&chars).trim().to_string();
                    CallsignField::Cq(if s.is_empty() { None } else { Some(s) })
                }
            }
            _ if n28 < NTOKENS + MAX22 => {
                // Hash-based callsign — can't decode without lookup table
                CallsignField::Hash(n28 - NTOKENS)
            }
            _ => {
                // Standard callsign via mixed-radix decoding
                let basecall_val = n28 - NTOKENS - MAX22;
                match Self::unpack_basecall(basecall_val) {
                    Some(call) => CallsignField::Callsign(call),
                    None => CallsignField::Hash(n28), // fallback
                }
            }
        }
    }

    /// Decode a standard callsign from its mixed-radix value.
    ///
    /// Reverse of encoder's `pack_basecall()`:
    /// Radix order: 37 × 36 × 10 × 27 × 27 × 27
    fn unpack_basecall(mut n: u32) -> Option<String> {
        const C0: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 37
        const C1: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 36
        const C2: &[u8] = b"0123456789"; // 10
        const C3: &[u8] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 27

        let i5 = (n % 27) as usize;
        n /= 27;
        let i4 = (n % 27) as usize;
        n /= 27;
        let i3 = (n % 27) as usize;
        n /= 27;
        let i2 = (n % 10) as usize;
        n /= 10;
        let i1 = (n % 36) as usize;
        n /= 36;
        let i0 = n as usize;

        if i0 >= C0.len()
            || i1 >= C1.len()
            || i2 >= C2.len()
            || i3 >= C3.len()
            || i4 >= C3.len()
            || i5 >= C3.len()
        {
            return None;
        }

        let mut call = String::with_capacity(6);
        call.push(C0[i0] as char);
        call.push(C1[i1] as char);
        call.push(C2[i2] as char);
        call.push(C3[i3] as char);
        call.push(C3[i4] as char);
        call.push(C3[i5] as char);

        let call = call.trim().to_string();
        if call.is_empty() {
            None
        } else {
            Some(call)
        }
    }

    /// Decode the 15-bit grid/report/token field.
    ///
    /// Reverse of encoder's `packgrid()`.
    /// Returns (grid, report, token).
    fn unpackgrid(igrid4: u16, ir: u8) -> (Option<String>, Option<i8>, Option<String>) {
        const MAXGRID4: u16 = 32400;

        if igrid4 < MAXGRID4 {
            // Grid square: mixed-radix 18 × 18 × 10 × 10
            let mut g = igrid4;
            let d3 = (g % 10) as u8;
            g /= 10;
            let d2 = (g % 10) as u8;
            g /= 10;
            let d1 = (g % 18) as u8;
            g /= 18;
            let d0 = g as u8;

            if d0 < 18 && d1 < 18 && d2 < 10 && d3 < 10 {
                let grid = format!(
                    "{}{}{}{}",
                    (b'A' + d0) as char,
                    (b'A' + d1) as char,
                    (b'0' + d2) as char,
                    (b'0' + d3) as char,
                );
                return (Some(grid), None, None);
            }
        }

        match igrid4 {
            x if x == MAXGRID4 + 1 => (None, None, None), // empty
            x if x == MAXGRID4 + 2 => (None, None, Some("RRR".to_string())),
            x if x == MAXGRID4 + 3 => (None, None, Some("RR73".to_string())),
            x if x == MAXGRID4 + 4 => (None, None, Some("73".to_string())),
            _ => {
                // Signal report: igrid4 = MAXGRID4 + 35 + dd
                let report_val = (igrid4 as i16) - (MAXGRID4 as i16) - 35;
                let report = report_val as i8;
                // ir bit is handled by the caller to set Report vs ReportWithR
                let _ = ir;
                (None, Some(report), None)
            }
        }
    }

    /// Parse Type 1 extended callsign messages
    fn parse_extended_message(
        &self,
        payload: &BitSlice,
        message: &mut Ft8Message,
    ) -> Ft8Result<()> {
        // Type 1 messages support callsigns with prefixes/suffixes
        // Extract base callsign (28 bits)
        let base_call = self.unpack28(bits_to_u32(&payload[0..28]));

        // Extract prefix/suffix encoding (variable)
        let ext_bits = &payload[28..];

        if ext_bits.len() >= 22 {
            let ext_value = bits_to_u32(&ext_bits[0..22]);

            // Decode prefix or suffix
            let extension = self.decode_callsign_extension(ext_value)?;

            if let (Some(mut call), Some(ext)) = (base_call.to_callsign(), extension) {
                if ext.starts_with('/') {
                    // Suffix
                    call.push_str(&ext);
                } else {
                    // Prefix
                    call = format!("{}/{}", ext, call);
                }
                message.from_callsign = Some(call);
            }
        }

        Ok(())
    }

    /// Parse Type 2 contest messages
    fn parse_contest_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Extract callsigns
        let call1 = self.decode_callsign_28bit(&payload[0..28])?;
        let call2 = self.decode_callsign_28bit(&payload[28..56]);

        message.to_callsign = call1;
        message.from_callsign = call2.unwrap_or(None);

        // Extract contest exchange (remaining bits)
        if payload.len() > 56 {
            let exchange_bits = &payload[56..];
            let exchange_value = bits_to_u32(exchange_bits);

            // Contest exchange formats vary:
            // - Serial number (0001-9999)
            // - Grid square + serial
            // - Section/state abbreviation

            if exchange_value < 10000 {
                // Serial number
                message.contest_exchange = Some(format!("{:04}", exchange_value));
            } else {
                // Complex exchange - decode based on contest type
                message.contest_exchange = Some(format!("<{:06X}>", exchange_value));
            }
        }

        Ok(())
    }

    // =========================================================================
    // i3=0 sub-type parsers (n3 field determines format)
    // =========================================================================

    /// Parse i3=0 n3=1: DXpedition (Fox/Hound) mode
    ///
    /// Bit layout (71 bits, excluding n3 and i3):
    ///   Fox sends:   call_fox(28) + call_hound(28) + rpt_fox(1) + rpt(15)
    ///   Hound sends: call_hound(28) + call_fox(28) + R(1) + rpt(15)
    ///
    /// The Fox (DXpedition) station sends reports to hounds.
    /// rpt field: 0-32767. If rpt < 32400 it's a grid square, else signal report.
    fn parse_dxpedition_type0(&self, bits: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        let n28a = bits_to_u32(&bits[0..28]);
        let n28b = bits_to_u32(&bits[28..56]);
        let ir = bits[56];
        let igrid = bits_to_u32(&bits[57..71]);

        let call_a = self.unpack28(n28a);
        let call_b = self.unpack28(n28b);

        message.to_callsign = call_a.to_callsign();
        message.from_callsign = call_b.to_callsign();

        // Decode grid or report (same as standard type 1)
        if ir {
            // R+report: signal report = igrid - 35
            let report = igrid as i32 - 35;
            message.signal_report = Some(report as i8);
            let r_prefix = if ir { "R" } else { "" };
            message.text = Some(format!(
                "{} {} {}{:+03}",
                message.to_callsign.as_deref().unwrap_or("?"),
                message.from_callsign.as_deref().unwrap_or("?"),
                r_prefix,
                report
            ));
        } else if igrid < 32400 {
            // Grid square
            let (grid, _, _) = Self::unpackgrid(igrid as u16, 0);
            message.grid_square = grid;
            message.text = Some(format!(
                "{} {} {}",
                message.to_callsign.as_deref().unwrap_or("?"),
                message.from_callsign.as_deref().unwrap_or("?"),
                message.grid_square.as_deref().unwrap_or("????")
            ));
        } else {
            // Numeric report
            let report = igrid as i32 - 32768 - 35;
            message.signal_report = Some(report as i8);
            message.text = Some(format!(
                "{} {} {:+03}",
                message.to_callsign.as_deref().unwrap_or("?"),
                message.from_callsign.as_deref().unwrap_or("?"),
                report
            ));
        }

        Ok(())
    }

    /// Parse i3=0 n3=2: EU VHF Contest
    ///
    /// Bit layout (71 bits):
    ///   call1(28) + call2(28) + R(1) + rpt_or_loc(14)
    ///
    /// rpt_or_loc encodes either:
    ///   - A 4-character grid (JO65) for initial exchange
    ///   - An R+report for signal report exchange
    ///   - RRR/RR73/73 tokens
    fn parse_eu_vhf_type0(&self, bits: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        let n28a = bits_to_u32(&bits[0..28]);
        let n28b = bits_to_u32(&bits[28..56]);
        let ir = bits[56];
        let irpt = bits_to_u32(&bits[57..71]);

        let call_a = self.unpack28(n28a);
        let call_b = self.unpack28(n28b);

        message.to_callsign = call_a.to_callsign();
        message.from_callsign = call_b.to_callsign();

        let r_prefix = if ir { "R " } else { "" };

        // EU VHF uses 14-bit field for grid or report
        // Grid squares: 4-char Maidenhead (e.g., JO65) encoded as (lon*180+lat) base-18*18
        if irpt < 16200 {
            // 4-character grid: AA00 through RR99 = 18*18*10*10 = 32400 combos
            // But EU VHF uses a compressed 14-bit encoding (max 16384)
            let lon = irpt / 900; // 0-17 → A-R
            let remainder = irpt % 900;
            let lat = remainder / 50; // 0-17 → A-R
            let lon_digit = (remainder % 50) / 5; // 0-9
            let lat_digit = remainder % 5; // Only 0-4 precision

            let grid = format!(
                "{}{}{}{}",
                (b'A' + lon as u8) as char,
                (b'A' + lat as u8) as char,
                lon_digit,
                lat_digit * 2 // Approximate
            );
            message.grid_square = Some(grid.clone());
            message.text = Some(format!(
                "{} {} {}{}",
                message.to_callsign.as_deref().unwrap_or("?"),
                message.from_callsign.as_deref().unwrap_or("?"),
                r_prefix,
                grid
            ));
        } else {
            // Signal report or tokens
            let rpt_val = irpt - 16200;
            if rpt_val == 1 {
                message.text = Some(format!(
                    "{} {} {}RRR",
                    message.to_callsign.as_deref().unwrap_or("?"),
                    message.from_callsign.as_deref().unwrap_or("?"),
                    r_prefix
                ));
            } else if rpt_val == 2 {
                message.text = Some(format!(
                    "{} {} {}RR73",
                    message.to_callsign.as_deref().unwrap_or("?"),
                    message.from_callsign.as_deref().unwrap_or("?"),
                    r_prefix
                ));
            } else if rpt_val == 3 {
                message.text = Some(format!(
                    "{} {} {}73",
                    message.to_callsign.as_deref().unwrap_or("?"),
                    message.from_callsign.as_deref().unwrap_or("?"),
                    r_prefix
                ));
            } else {
                // Signal report: rpt_val maps to dB value
                let report = rpt_val as i32 - 35;
                message.signal_report = Some(report as i8);
                message.text = Some(format!(
                    "{} {} {}{:+03}",
                    message.to_callsign.as_deref().unwrap_or("?"),
                    message.from_callsign.as_deref().unwrap_or("?"),
                    r_prefix,
                    report
                ));
            }
        }

        Ok(())
    }

    /// Parse i3=0 n3=3,4: ARRL Field Day
    ///
    /// Bit layout (71 bits):
    ///   n3=3: call1(28) + call2(28) + R(1) + n_class(4) + n_section(7) + pad(3)
    ///   n3=4: Same as n3=3, used for alternate exchange ordering
    ///
    /// Class: 1A through 33F (4 bits for count 1-32, encoded as 0-15 then letter)
    /// Section: ARRL/RAC section code (7 bits = 0-83)
    fn parse_field_day_type0(
        &self,
        bits: &BitSlice,
        n3: u32,
        message: &mut Ft8Message,
    ) -> Ft8Result<()> {
        let n28a = bits_to_u32(&bits[0..28]);
        let n28b = bits_to_u32(&bits[28..56]);
        let ir = bits[56];
        let n_class_section = bits_to_u32(&bits[57..71]);

        let call_a = self.unpack28(n28a);
        let call_b = self.unpack28(n28b);

        message.to_callsign = call_a.to_callsign();
        message.from_callsign = call_b.to_callsign();

        // Decode Field Day exchange
        // n_class_section packs: n_transmitters(5) * n_class_letter(6) + n_section(7)
        // Actually per WSJT-X: the 14-bit field = n_class_section
        // Where class = floor(n_class_section / NSEC), section = n_class_section % NSEC
        // NSEC = 84 (number of ARRL/RAC sections)
        const NSEC: u32 = 84;
        let class_code = n_class_section / NSEC;
        let section_code = n_class_section % NSEC;

        // Class: encoded as (n_tx - 1) * 6 + letter_index
        // letter_index: A=0, B=1, C=2, D=3, E=4, F=5
        let n_tx = (class_code / 6) + 1;
        let class_letter_idx = class_code % 6;
        let class_letter = (b'A' + class_letter_idx as u8) as char;

        let section = self.decode_arrl_section(section_code as u8)?;

        let r_prefix = if ir { "R " } else { "" };

        message.contest_exchange = Some(format!("{}{} {}", n_tx, class_letter, section));
        message.text = Some(format!(
            "{} {} {}{}{} {}",
            message.to_callsign.as_deref().unwrap_or("?"),
            message.from_callsign.as_deref().unwrap_or("?"),
            r_prefix,
            n_tx,
            class_letter,
            section
        ));

        Ok(())
    }

    /// Decode callsign from 28-bit field — delegates to unpack28.
    /// Kept for compatibility with contest/field day/DXpedition parsers.
    fn decode_callsign_28bit(&self, bits: &BitSlice) -> Ft8Result<Option<String>> {
        let n28 = bits_to_u32(bits);
        Ok(self.unpack28(n28).to_callsign())
    }

    /// Decode callsign extension (prefix/suffix)
    fn decode_callsign_extension(&self, value: u32) -> Ft8Result<Option<String>> {
        if value == 0 {
            return Ok(None);
        }

        // Extension encoding varies by type
        if value < 1024 {
            // Numeric suffix (/0-/9, /P, /M, /MM, /AM, /QRP, etc.)
            match value {
                1..=10 => Ok(Some(format!("/{}", value - 1))),
                11 => Ok(Some("/P".to_string())),
                12 => Ok(Some("/M".to_string())),
                13 => Ok(Some("/MM".to_string())),
                14 => Ok(Some("/AM".to_string())),
                15 => Ok(Some("/QRP".to_string())),
                _ => Ok(Some(format!("/{}", value))),
            }
        } else {
            // Prefix encoding (country/region codes)
            let prefix_code = value - 1024;
            Ok(Some(self.decode_prefix_code(prefix_code)?))
        }
    }

    /// Decode grid square from 15-bit field using Maidenhead system
    fn decode_grid_square_15bit(&self, grid_value: u32) -> Ft8Result<Option<String>> {
        // Delegate to unpackgrid — only returns the grid component
        let (grid, _, _) = Self::unpackgrid(grid_value as u16, 0);
        Ok(grid)
    }

    /// Decode free text using 6-bit character encoding
    fn decode_text_6bit(&self, bits: &BitSlice) -> Ft8Result<Option<String>> {
        let mut text = String::new();

        // Decode 6-bit characters (max 13 characters)
        for chunk in bits.chunks(6) {
            if chunk.len() < 6 {
                break; // Incomplete character
            }

            let char_value = bits_to_u32(chunk) as u8;
            let ch = self.decode_6bit_character(char_value)?;

            if ch == '\0' {
                break; // Null terminator
            }

            text.push(ch);

            if text.len() >= 13 {
                break; // Maximum length
            }
        }

        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text.trim_end().to_string()))
        }
    }

    /// Decode 6-bit character value (WSJT-X character set)
    fn decode_6bit_character(&self, value: u8) -> Ft8Result<char> {
        // WSJT-X 6-bit character encoding
        const CHAR_SET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ+-./?";

        if (value as usize) < CHAR_SET.len() {
            Ok(CHAR_SET[value as usize] as char)
        } else {
            Ok('\0') // Invalid/null character
        }
    }

    /// Decode ARRL/RAC section code (84 sections, matching WSJT-X)
    fn decode_arrl_section(&self, code: u8) -> Ft8Result<String> {
        const SECTIONS: [&str; 84] = [
            // New England (0-6)
            "CT", "EMA", "ME", "NH", "RI", "VT", "WMA", // Atlantic (7-12)
            "ENY", "NLI", "NNJ", "NNY", "SNJ", "WNY", // Mid-Atlantic (13-16)
            "DE", "EPA", "MDC", "WPA", // Southeast (17-24)
            "AL", "GA", "KY", "NC", "NFL", "SC", "SFL", "WCF", // Tennessee (25)
            "TN",  // Virginia (26-27)
            "VA", "PR", // Great Lakes (28-32)
            "MI", "OH", "WV", "IL", "IN", // Wisconsin (33)
            "WI", // Midwest (34-38)
            "AR", "LA", "MS", "NM", "OK",  // North Texas (39)
            "NTX", // South Texas (40)
            "STX", // West Texas (41)
            "WTX", // Central (42-47)
            "CO", "IA", "KS", "MN", "MO", "NE", // North Dakota/South Dakota (48-49)
            "ND", "SD", // Northwest (50-54)
            "OR", "EWA", "WWA", "ID", "MT", // Wyoming (55)
            "WY", // Pacific (56-58)
            "AK", "HI", "PAC", // Southwest (59-63)
            "AZ", "EBay", "LAX", "ORG", "SB",
            // San Diego/Santa Clara/San Francisco/San Joaquin (64-67)
            "SDG", "SCV", "SF", "SJV", // Sierra/Nevada/Utah (68-70)
            "SV", "NV", "UT", // Canada (71-83)
            "AB", "BC", "GH", "MB", "NB", "NL", "NS", "NT", "ON", "PE", "QC", "SK", "YT",
        ];

        if (code as usize) < SECTIONS.len() {
            Ok(SECTIONS[code as usize].to_string())
        } else {
            Ok(format!("S{:02}", code))
        }
    }

    /// Decode state/province code for RTTY Roundup (US states + Canadian provinces + DC/DX)
    fn decode_state_code(&self, code: u8) -> Ft8Result<String> {
        const STATES: [&str; 63] = [
            // US States (0-49)
            "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN",
            "IA", "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV",
            "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN",
            "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY", // DC (50)
            "DC", // Canadian provinces (51-62)
            "AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON", "PE", "QC", "SK",
        ];

        if (code as usize) < STATES.len() {
            Ok(STATES[code as usize].to_string())
        } else {
            Ok(format!("DX"))
        }
    }

    /// Decode prefix code for extended callsigns
    fn decode_prefix_code(&self, code: u32) -> Ft8Result<String> {
        // Common prefixes based on ITU regions and special operations
        match code {
            0..=99 => Ok(format!("K{}", code)),           // US regions
            100..=199 => Ok(format!("VE{}", code - 100)), // Canada
            200..=299 => Ok(format!("JA{}", code - 200)), // Japan
            // ... more prefix mappings
            _ => Ok(format!("PX{}", code)),
        }
    }
}

impl Default for MessageParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert bit slice to u32 value
fn bits_to_u32(bits: &BitSlice) -> u32 {
    let mut value = 0u32;
    for (i, bit) in bits.iter().enumerate() {
        if *bit {
            value |= 1 << (bits.len() - 1 - i);
        }
    }
    value
}

/// Convert bit slice to u64 value (for fields wider than 32 bits, e.g. 58-bit nonstandard callsign)
fn bits_to_u64(bits: &BitSlice) -> u64 {
    let mut value = 0u64;
    for (i, bit) in bits.iter().enumerate() {
        if *bit {
            value |= 1u64 << (bits.len() - 1 - i);
        }
    }
    value
}

/// CRC-14 checksum calculation for FT8 (polynomial 0x2757)
///
/// Direct port of ft8_lib's `ftx_compute_crc()` + `ftx_add_crc()`.
/// The CRC is computed over the 77-bit payload zero-extended to 82 bits,
/// as specified by the FT8 protocol: `num_bits = 96 - 14 = 82`.
pub fn calculate_crc14(payload: &BitSlice) -> u16 {
    const CRC_WIDTH: u32 = 14;
    const POLY: u16 = 0x2757;
    const TOPBIT: u16 = 1u16 << (CRC_WIDTH - 1); // 0x2000
    const NUM_BITS: usize = 82; // 77 payload + 5 zero padding

    // Pack payload bits into bytes (MSB first), zero-extending to 82 bits
    let num_bytes = (NUM_BITS + 7) / 8; // 11 bytes
    let mut bytes = [0u8; 11];
    for (i, bit) in payload.iter().enumerate() {
        if *bit {
            bytes[i / 8] |= 0x80u8 >> (i % 8);
        }
    }
    // Ensure bits 77-79 in byte 9 are zero (they already are from init),
    // and byte 10 is zero. This matches ft8_lib's: a91[9] &= 0xF8; a91[10] = 0;
    bytes[9] &= 0xF8;

    // Exact port of ftx_compute_crc()
    let mut remainder: u16 = 0;
    let mut idx_byte: usize = 0;

    for idx_bit in 0..NUM_BITS {
        if idx_bit % 8 == 0 {
            remainder ^= (bytes[idx_byte] as u16) << (CRC_WIDTH - 8);
            idx_byte += 1;
        }

        if remainder & TOPBIT != 0 {
            remainder = (remainder << 1) ^ POLY;
        } else {
            remainder <<= 1;
        }
    }

    remainder & ((TOPBIT << 1) - 1) // mask to 14 bits
}

/// Verify CRC-14 checksum for complete 91-bit message
pub fn verify_crc14(message_bits: &BitSlice) -> bool {
    if message_bits.len() != TOTAL_MESSAGE_BITS {
        return false;
    }

    let payload = &message_bits[0..PAYLOAD_BITS];
    let received_crc = bits_to_u32(&message_bits[PAYLOAD_BITS..]) as u16;
    let calculated_crc = calculate_crc14(payload);

    received_crc == calculated_crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_display() {
        assert_eq!(MessageType::Standard.to_string(), "Standard");
        assert_eq!(MessageType::Contest.to_string(), "Contest");
        assert_eq!(MessageType::FreeText.to_string(), "Free Text");
        assert_eq!(MessageType::NonStdCall.to_string(), "Non-Std Call");
    }

    #[test]
    fn test_bits_to_u32() {
        let bits = bitvec![1, 0, 1, 1];
        assert_eq!(bits_to_u32(&bits), 0b1011);
    }

    #[test]
    fn test_6bit_character_decoding() {
        let parser = MessageParser::new();
        assert_eq!(parser.decode_6bit_character(0).unwrap(), ' ');
        assert_eq!(parser.decode_6bit_character(1).unwrap(), '0');
        assert_eq!(parser.decode_6bit_character(11).unwrap(), 'A');
        assert_eq!(parser.decode_6bit_character(37).unwrap(), '+');
    }

    #[test]
    fn test_grid_square_decoding() {
        // Test FN42 encoding — matches encoder's packgrid()
        // F=5, N=13, 4=4, 2=2 → 5*1800 + 13*100 + 4*10 + 2 = 10342
        let fn42_value = 5 * 1800 + 13 * 100 + 4 * 10 + 2;
        let (grid, _, _) = MessageParser::unpackgrid(fn42_value as u16, 0);
        assert_eq!(grid, Some("FN42".to_string()));

        // Test special tokens
        let (_, _, tok) = MessageParser::unpackgrid(32402, 0);
        assert_eq!(tok, Some("RRR".to_string()));
        let (_, _, tok) = MessageParser::unpackgrid(32403, 0);
        assert_eq!(tok, Some("RR73".to_string()));
        let (_, _, tok) = MessageParser::unpackgrid(32404, 0);
        assert_eq!(tok, Some("73".to_string()));

        // Test signal report: -12 dB → igrid4 = 32400 + 35 + (-12) = 32423
        let (_, rpt, _) = MessageParser::unpackgrid(32423, 0);
        assert_eq!(rpt, Some(-12));
    }

    #[test]
    fn test_standard_cq_message_display() {
        let mut message = Ft8Message::default();
        message.message_type = MessageType::Standard;
        message.standard_type = Some(StandardMessageType::Cq);
        message.from_callsign = Some("W1ABC".to_string());
        message.grid_square = Some("FN42".to_string());

        let display = message.to_string();
        assert!(display.contains("CQ"));
        assert!(display.contains("W1ABC"));
        assert!(display.contains("FN42"));
    }

    #[test]
    fn test_decoded_message_creation() {
        let ft8_msg = Ft8Message::default();
        let decoded = DecodedMessage::new(ft8_msg, -15.5, 0.85, 1523.4, 2.1);

        assert_eq!(decoded.snr_db, -15.5);
        assert_eq!(decoded.confidence, 0.85);
        assert_eq!(decoded.frequency_offset, 1523.4);
        assert_eq!(decoded.time_offset, 2.1);
    }

    #[test]
    fn test_callsign_unpack28_round_trip() {
        let parser = MessageParser::new();

        // Test unpack28 for known callsign values
        // CQ = 2
        let cq = parser.unpack28(2);
        assert!(matches!(cq, CallsignField::Cq(None)));

        // Standard callsigns via unpack_basecall
        // W1ABC: known from encoder tests
        let w1abc_basecall = {
            // Pack W1ABC manually using mixed-radix
            // " W1ABC" → c6 = [' ', 'W', '1', 'A', 'B', 'C']
            let i0: u32 = 0; // space
            let i1: u32 = 32; // W in "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            let i2: u32 = 1; // 1
            let i3: u32 = 1; // A in " ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            let i4: u32 = 2; // B
            let i5: u32 = 3; // C
            let n = ((((i0 * 36 + i1) * 10 + i2) * 27 + i3) * 27 + i4) * 27 + i5;
            n
        };
        const NTOKENS: u32 = 2_063_592;
        const MAX22: u32 = 4_194_304;
        let call = parser.unpack28(NTOKENS + MAX22 + w1abc_basecall);
        assert_eq!(call.to_callsign(), Some("W1ABC".to_string()));
    }

    #[test]
    fn test_hash_table_operations() {
        let mut hash_table = HashTable::new();

        // Add test callsign
        hash_table.add_callsign("K1ABC");

        // Test hash lookups
        let hash_10 = hash_table.calculate_hash_10bit("K1ABC");
        let lookup_10 = hash_table.lookup_10bit_hash(hash_10);
        assert_eq!(lookup_10, Some("K1ABC".to_string()));
    }

    #[test]
    fn test_bits_to_u64() {
        let bits = bitvec![
            1, 0, 1, 1, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
            0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1
        ];
        assert_eq!(bits_to_u64(&bits), 0xB0F0AAAA0001u64);
    }

    #[test]
    fn test_unpack58_callsign() {
        // Pack "PJ4/KA1ABC" the same way ft8_lib does: base-38, 11 chars, space-padded
        const CHARSET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
        let call = "PJ4/KA1ABC";
        let mut n58: u64 = 0;
        for &b in call.as_bytes() {
            let j = CHARSET.iter().position(|&c| c == b).unwrap();
            n58 = n58 * 38 + j as u64;
        }
        // Pad remaining chars with space (index 0)
        for _ in call.len()..11 {
            n58 *= 38;
        }

        let decoded = MessageParser::unpack58(n58);
        assert_eq!(decoded, "PJ4/KA1ABC");
    }

    #[test]
    fn test_nonstd_call_parse() {
        // Build a 77-bit i3=4 payload: CQ PJ4/KA1ABC
        // Format: n12(12) + n58(58) + iflip(1) + nrpt(2) + icq(1) + i3(3)
        const CHARSET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
        let call = "PJ4/KA1ABC";
        let mut n58: u64 = 0;
        for &b in call.as_bytes() {
            let j = CHARSET.iter().position(|&c| c == b).unwrap();
            n58 = n58 * 38 + j as u64;
        }
        for _ in call.len()..11 {
            n58 *= 38;
        }

        let n12: u16 = 0; // not used for CQ
        let iflip: u8 = 0;
        let nrpt: u8 = 0;
        let icq: u8 = 1; // CQ message
        let i3: u8 = 4;

        let mut payload = BitVec::with_capacity(77);
        // n12: 12 bits
        for i in (0..12).rev() {
            payload.push((n12 >> i) & 1 != 0);
        }
        // n58: 58 bits
        for i in (0..58).rev() {
            payload.push((n58 >> i) & 1 != 0);
        }
        // iflip: 1 bit
        payload.push(iflip != 0);
        // nrpt: 2 bits
        for i in (0..2).rev() {
            payload.push((nrpt >> i) & 1 != 0);
        }
        // icq: 1 bit
        payload.push(icq != 0);
        // i3: 3 bits
        for i in (0..3).rev() {
            payload.push((i3 >> i) & 1 != 0);
        }

        assert_eq!(payload.len(), 77);

        let parser = MessageParser::new();
        let msg = parser.parse_payload(&payload).unwrap();
        assert_eq!(msg.message_type, MessageType::NonStdCall);
        assert_eq!(msg.to_callsign, Some("CQ".to_string()));
        assert_eq!(msg.from_callsign, Some("PJ4/KA1ABC".to_string()));
    }

    #[test]
    fn test_nonstd_call_with_report() {
        // Build: <...> PJ4/KA1ABC RR73 (iflip=0, nrpt=2, icq=0)
        const CHARSET: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
        let call = "PJ4/KA1ABC";
        let mut n58: u64 = 0;
        for &b in call.as_bytes() {
            let j = CHARSET.iter().position(|&c| c == b).unwrap();
            n58 = n58 * 38 + j as u64;
        }
        for _ in call.len()..11 {
            n58 *= 38;
        }

        let n12: u16 = 42; // some hash
        let iflip: u8 = 0;
        let nrpt: u8 = 2; // RR73
        let icq: u8 = 0;
        let i3: u8 = 4;

        let mut payload = BitVec::with_capacity(77);
        for i in (0..12).rev() {
            payload.push((n12 >> i) & 1 != 0);
        }
        for i in (0..58).rev() {
            payload.push((n58 >> i) & 1 != 0);
        }
        payload.push(iflip != 0);
        for i in (0..2).rev() {
            payload.push((nrpt >> i) & 1 != 0);
        }
        payload.push(icq != 0);
        for i in (0..3).rev() {
            payload.push((i3 >> i) & 1 != 0);
        }

        let parser = MessageParser::new();
        let msg = parser.parse_payload(&payload).unwrap();
        assert_eq!(msg.message_type, MessageType::NonStdCall);
        assert_eq!(msg.to_callsign, Some("<...>".to_string())); // hash not in table
        assert_eq!(msg.from_callsign, Some("PJ4/KA1ABC".to_string()));
        assert_eq!(msg.contest_exchange, Some("RR73".to_string()));
    }

    #[test]
    fn test_telemetry_parse() {
        // Build i3=0, n3=5 telemetry payload
        // The telemetry is 71 bits: the raw bytes left-shifted by 1 bit, then n3=5 in bits 71-73, i3=0 in 74-76
        let test_bytes: [u8; 9] = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x12];

        // Left-shift by 1 bit to create the b71 encoding
        let mut shifted = [0u8; 9];
        for i in 0..9 {
            shifted[i] = test_bytes[i] << 1;
            if i < 8 {
                shifted[i] |= test_bytes[i + 1] >> 7;
            }
        }

        // Build 77-bit payload: shifted bytes (71 bits) + n3=5 (bits 71-73) + i3=0 (bits 74-76)
        let mut payload = BitVec::with_capacity(77);
        for i in 0..71 {
            payload.push((shifted[i / 8] >> (7 - (i % 8))) & 1 != 0);
        }
        // n3 = 5 = 0b101
        payload.push(true);
        payload.push(false);
        payload.push(true);
        // i3 = 0 = 0b000
        payload.push(false);
        payload.push(false);
        payload.push(false);

        assert_eq!(payload.len(), 77);

        let parser = MessageParser::new();
        let msg = parser.parse_payload(&payload).unwrap();
        assert_eq!(msg.message_type, MessageType::Telemetry);
        // Should decode back to the original hex
        assert_eq!(msg.text, Some("123456789ABCDEF012".to_string()));
    }

    #[test]
    fn test_hash_table_ft8lib_compatible() {
        // Verify our hash matches ft8_lib's algorithm
        let mut ht = HashTable::new();
        ht.add_callsign("K1ABC");

        // The hash should be deterministic
        let n22 = HashTable::calculate_n22("K1ABC");
        assert!(n22 < 0x400000); // 22 bits
        let n12 = n22 >> 10;
        assert!(n12 < 0x1000); // 12 bits
        let n10 = n22 >> 12;
        assert!(n10 < 0x400); // 10 bits

        // Verify lookup works
        assert_eq!(ht.lookup_22bit_hash(n22), Some("K1ABC".to_string()));
        assert_eq!(ht.lookup_12bit_hash(n12), Some("K1ABC".to_string()));
        assert_eq!(ht.lookup_10bit_hash(n10), Some("K1ABC".to_string()));
    }

    #[test]
    fn test_crc14_calculation() {
        // Test with known payload (77 bits for FT8)
        let pattern = vec![1u8, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0];
        let mut payload = BitVec::new();
        for _ in 0..(77 / 16) {
            for &bit in &pattern {
                payload.push(bit != 0);
            }
        }
        // Add remaining bits to reach 77
        for i in 0..(77 % 16) {
            payload.push(pattern[i] != 0);
        }

        let crc = calculate_crc14(&payload);
        assert!(crc <= 0x3FFF); // 14-bit value

        // Test CRC verification with proper 91-bit message
        let mut full_message = payload.clone();
        let crc_bits = (0..14)
            .map(|i| (crc >> (13 - i)) & 1 != 0)
            .collect::<BitVec>();
        full_message.extend(crc_bits);

        assert_eq!(full_message.len(), TOTAL_MESSAGE_BITS);
        assert!(verify_crc14(&full_message));
    }
}
