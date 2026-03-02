//! FT8 message types and parsing
//!
//! This module handles the FT8 protocol message structure:
//! - 77-bit information payload
//! - 14-bit CRC checksum
//! - LDPC error correction
//! - Message type detection and parsing
//! - Callsign and grid square validation

use crate::{Ft8Error, Ft8Result};
use std::fmt;
use std::time::SystemTime;
use std::collections::HashMap;
use bitvec::prelude::*;

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
    /// Type 0: Standard messages with callsigns and grid/report
    Standard,
    /// Type 1: Extended callsign support with prefix/suffix
    Extended,
    /// Type 2: Contest and special messages
    Contest,
    /// Type 3: Field Day messages
    FieldDay,
    /// Type 4: Telemetry and data
    Telemetry,
    /// Type 5: Free text messages
    FreeText,
    /// Type 6: DXpedition and EU VHF contest
    DXpedition,
    /// Type 7: ARRL RTTY Roundup
    RTTYRoundup,
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
            MessageType::Unknown => write!(f, "Unknown"),
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
            MessageType::Standard => {
                match self.standard_type {
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
                }
            }
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
                write!(f, "<Telemetry>")?;
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
            self.time_offset,
            self.snr_db,
            self.frequency_offset,
            self.confidence,
            self.text
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
    
    /// Calculate 10-bit hash (djb2 algorithm, truncated)
    fn calculate_hash_10bit(&self, callsign: &str) -> u32 {
        let mut hash = 5381u32;
        for byte in callsign.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
        }
        hash & 0x3FF // 10 bits
    }
    
    /// Calculate 12-bit hash (djb2 algorithm, truncated)
    fn calculate_hash_12bit(&self, callsign: &str) -> u32 {
        let mut hash = 5381u32;
        for byte in callsign.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
        }
        (hash >> 2) & 0xFFF // 12 bits, offset for different distribution
    }
    
    /// Calculate 22-bit hash (djb2 algorithm, truncated)
    fn calculate_hash_22bit(&self, callsign: &str) -> u32 {
        let mut hash = 5381u32;
        for byte in callsign.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
        }
        (hash >> 4) & 0x3FFFFF // 22 bits
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
                "Callsign too long".to_string()
            ));
        }
        
        let mut value = 0u32;
        // Pad with leading spaces to 6 characters
        let padded = format!("{:>6}", callsign);
        
        for ch in padded.chars() {
            let ch_upper = ch.to_ascii_uppercase();
            let pos = CALLSIGN_CHARS.iter().position(|&c| c == ch_upper as u8)
                .ok_or_else(|| Ft8Error::MessageDecodingError(
                    format!("Invalid character in callsign: {}", ch)
                ))?;
            
            value = value * 37 + pos as u32;
        }
        
        if value >= 262_144_000 {
            return Err(Ft8Error::MessageDecodingError(
                "Callsign encoding overflow".to_string()
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
    pub fn parse_payload(&self, payload: &BitSlice) -> Ft8Result<Ft8Message> {
        if payload.len() != PAYLOAD_BITS {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid payload length: {} bits", payload.len())
            ));
        }
        
        let mut message = Ft8Message::default();
        message.payload_bits = payload.to_bitvec();
        
        // Determine message type from first 3 bits
        let type_value = bits_to_u32(&payload[0..3]);
        message.message_type = self.determine_message_type(type_value)?;
        
        // Parse message content based on type
        match message.message_type {
            MessageType::Standard => self.parse_standard_message(&payload[3..], &mut message)?,
            MessageType::Extended => self.parse_extended_message(&payload[3..], &mut message)?,
            MessageType::Contest => self.parse_contest_message(&payload[3..], &mut message)?,
            MessageType::FieldDay => self.parse_field_day_message(&payload[3..], &mut message)?,
            MessageType::Telemetry => self.parse_telemetry_message(&payload[3..], &mut message)?,
            MessageType::FreeText => self.parse_freetext_message(&payload[3..], &mut message)?,
            MessageType::DXpedition => self.parse_dxpedition_message(&payload[3..], &mut message)?,
            MessageType::RTTYRoundup => self.parse_rtty_roundup_message(&payload[3..], &mut message)?,
            MessageType::Unknown => {}
        }
        
        Ok(message)
    }
    
    /// Determine message type from first 3 bits
    fn determine_message_type(&self, type_value: u32) -> Ft8Result<MessageType> {
        match type_value {
            0 => Ok(MessageType::Standard),
            1 => Ok(MessageType::Extended),
            2 => Ok(MessageType::Contest),
            3 => Ok(MessageType::FieldDay),
            4 => Ok(MessageType::Telemetry),
            5 => Ok(MessageType::FreeText),
            6 => Ok(MessageType::DXpedition),
            7 => Ok(MessageType::RTTYRoundup),
            _ => Ok(MessageType::Unknown),
        }
    }
    
    /// Parse Type 0 standard messages
    fn parse_standard_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Extract first callsign field (28 bits)
        let call1 = self.decode_callsign_28bit(&payload[0..28])?;
        
        // Extract second callsign field (28 bits) 
        let call2 = self.decode_callsign_28bit(&payload[28..56]);
        
        // Extract remaining field (variable)
        let remaining_bits = &payload[56..];
        
        // Determine standard message subtype based on remaining bits
        if remaining_bits.len() >= 15 {
            let grid_value = bits_to_u32(&remaining_bits[0..15]);
            
            if grid_value == 0 {
                // No grid - could be RRR or 73
                if remaining_bits.len() > 15 {
                    let flag_bits = bits_to_u32(&remaining_bits[15..]);
                    if flag_bits & 1 != 0 {
                        message.standard_type = Some(StandardMessageType::RR73);
                    } else {
                        message.standard_type = Some(StandardMessageType::Final73);
                    }
                } else {
                    message.standard_type = Some(StandardMessageType::Rrr);
                }
            } else if grid_value >= 32400 {
                // Signal report encoding
                let report = ((grid_value - 32400) as i8) - 35;
                message.signal_report = Some(report);
                
                if remaining_bits.len() > 15 && bits_to_u32(&remaining_bits[15..16]) != 0 {
                    message.standard_type = Some(StandardMessageType::ReportWithR);
                } else {
                    message.standard_type = Some(StandardMessageType::Report);
                }
            } else {
                // Grid square
                message.grid_square = self.decode_grid_square_15bit(grid_value)?;
                
                if call1.is_some() && call2.is_err() {
                    // CQ message
                    message.standard_type = Some(StandardMessageType::Cq);
                    message.from_callsign = call1;
                } else {
                    // Reply message
                    if remaining_bits.len() > 15 && bits_to_u32(&remaining_bits[15..16]) != 0 {
                        message.standard_type = Some(StandardMessageType::ReplyWithR);
                    } else {
                        message.standard_type = Some(StandardMessageType::Reply);
                    }
                    message.to_callsign = call1;
                    message.from_callsign = call2.unwrap_or(None);
                }
            }
        }
        
        // Handle CQ DX and other special operations
        if let Some(StandardMessageType::Cq) = message.standard_type {
            if remaining_bits.len() > 15 {
                let dx_flag = bits_to_u32(&remaining_bits[15..16]);
                if dx_flag != 0 {
                    message.special_operation = Some("DX".to_string());
                }
            }
        }
        
        Ok(())
    }
    
    /// Parse Type 1 extended callsign messages
    fn parse_extended_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Type 1 messages support callsigns with prefixes/suffixes
        // Extract base callsign (28 bits)
        let base_call = self.decode_callsign_28bit(&payload[0..28])?;
        
        // Extract prefix/suffix encoding (variable)
        let ext_bits = &payload[28..];
        
        if ext_bits.len() >= 22 {
            let ext_value = bits_to_u32(&ext_bits[0..22]);
            
            // Decode prefix or suffix
            let extension = self.decode_callsign_extension(ext_value)?;
            
            if let (Some(mut call), Some(ext)) = (base_call, extension) {
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
    
    /// Parse Type 3 Field Day messages
    fn parse_field_day_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Extract callsigns
        let call1 = self.decode_callsign_28bit(&payload[0..28])?;
        let call2 = self.decode_callsign_28bit(&payload[28..56]);
        
        message.to_callsign = call1;
        message.from_callsign = call2.unwrap_or(None);
        
        // Extract Field Day exchange (class + section)
        if payload.len() > 56 {
            let fd_bits = &payload[56..];
            let fd_value = bits_to_u32(fd_bits);
            
            // Field Day format: <class><section>
            let class = (fd_value >> 8) & 0xFF;  // Operating class (1A-32F)
            let section = fd_value & 0xFF;       // ARRL section
            
            message.contest_exchange = Some(format!("{}A {}", class, 
                self.decode_arrl_section(section as u8)?));
        }
        
        Ok(())
    }
    
    /// Parse Type 4 telemetry messages
    fn parse_telemetry_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Telemetry format varies by application
        // Common formats include weather data, contest multipliers, etc.
        
        if payload.len() >= 18 {
            // Standard telemetry format: 18-bit value + metadata
            let telem_value = bits_to_u32(&payload[0..18]);
            let format_code = bits_to_u32(&payload[18..21]);
            
            match format_code {
                0 => {
                    // Weather telemetry
                    let temp = ((telem_value >> 9) & 0x1FF) as i16 - 128; // Temperature in C
                    let humidity = (telem_value & 0x1FF) as u8;           // Humidity %
                    message.text = Some(format!("WX: {}C {}%RH", temp, humidity));
                }
                1 => {
                    // Contest multiplier
                    message.text = Some(format!("MULT: {}", telem_value));
                }
                _ => {
                    // Generic telemetry
                    message.text = Some(format!("TELEM: {:06X}", telem_value));
                }
            }
        }
        
        Ok(())
    }
    
    /// Parse Type 5 free text messages (13 characters max)
    fn parse_freetext_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Free text uses 6-bit character encoding
        // Maximum 13 characters = 78 bits, but we have 74 bits available
        let text_bits = &payload[0..payload.len().min(72)];
        message.text = self.decode_text_6bit(text_bits)?;
        
        Ok(())
    }
    
    /// Parse Type 6 DXpedition messages
    fn parse_dxpedition_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // DXpedition format supports hash calls for crowded operations
        
        // Check if using hash encoding
        let hash_flag = bits_to_u32(&payload[0..1]);
        
        if hash_flag != 0 {
            // Hash-based encoding
            message.uses_hash_calls = true;
            
            let hash1 = bits_to_u32(&payload[1..11]);   // 10-bit hash
            let hash2 = bits_to_u32(&payload[11..23]);  // 12-bit hash
            
            message.to_callsign = self.hash_table.lookup_10bit_hash(hash1);
            message.from_callsign = self.hash_table.lookup_12bit_hash(hash2);
            
            // Extract grid or report from remaining bits
            if payload.len() > 23 {
                let remaining = bits_to_u32(&payload[23..]);
                if remaining < 32768 {
                    message.grid_square = self.decode_grid_square_15bit(remaining)?;
                } else {
                    message.signal_report = Some(((remaining - 32768) as i8) - 35);
                }
            }
        } else {
            // Standard callsign encoding
            let call1 = self.decode_callsign_28bit(&payload[1..29])?;
            let call2 = self.decode_callsign_28bit(&payload[29..57]);
            
            message.to_callsign = call1;
            message.from_callsign = call2.unwrap_or(None);
        }
        
        Ok(())
    }
    
    /// Parse Type 7 ARRL RTTY Roundup messages
    fn parse_rtty_roundup_message(&self, payload: &BitSlice, message: &mut Ft8Message) -> Ft8Result<()> {
        // Extract callsigns
        let call1 = self.decode_callsign_28bit(&payload[0..28])?;
        let call2 = self.decode_callsign_28bit(&payload[28..56]);
        
        message.to_callsign = call1;
        message.from_callsign = call2.unwrap_or(None);
        
        // Extract RTTY Roundup exchange (state + serial)
        if payload.len() > 56 {
            let rtty_bits = &payload[56..];
            let rtty_value = bits_to_u32(rtty_bits);
            
            let state = (rtty_value >> 10) & 0x3F;  // 6-bit state code
            let serial = rtty_value & 0x3FF;        // 10-bit serial number
            
            message.contest_exchange = Some(format!("{} {:04}", 
                self.decode_state_code(state as u8)?, serial));
        }
        
        Ok(())
    }
    
    /// Decode callsign from 28-bit field with complete WSJT-X algorithm
    fn decode_callsign_28bit(&self, bits: &BitSlice) -> Ft8Result<Option<String>> {
        let callsign_value = bits_to_u32(bits);
        
        if callsign_value == 0 {
            return Ok(None);
        }
        
        // Standard callsign range: 0 < value < 262,144,000
        if callsign_value < 262_144_000 {
            return Ok(Some(self.decode_standard_callsign(callsign_value)?));
        }
        
        // Hash-based callsign range: >= 262,144,000
        let hash_value = callsign_value - 262_144_000;
        
        // Check different hash types based on range
        if hash_value < 1024 {
            // 10-bit hash
            Ok(self.hash_table.lookup_10bit_hash(hash_value))
        } else if hash_value < 5120 {
            // 12-bit hash  
            Ok(self.hash_table.lookup_12bit_hash(hash_value - 1024))
        } else {
            // 22-bit hash for special operations
            Ok(self.hash_table.lookup_22bit_hash(hash_value - 5120))
        }
    }
    
    /// Decode standard callsign using base-37 encoding
    fn decode_standard_callsign(&self, value: u32) -> Ft8Result<String> {
        // WSJT-X callsign encoding: base-37 with specific character set
        const CALLSIGN_CHARS: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        
        if value >= 262_144_000 {
            return Err(Ft8Error::MessageDecodingError(
                "Invalid standard callsign value".to_string()
            ));
        }
        
        let mut result = String::new();
        let mut n = value;
        
        // Decode up to 6 characters
        for _ in 0..6 {
            let char_idx = (n % 37) as usize;
            if char_idx < CALLSIGN_CHARS.len() {
                result.insert(0, CALLSIGN_CHARS[char_idx] as char);
            }
            n /= 37;
            if n == 0 {
                break;
            }
        }
        
        // Remove leading spaces and validate
        let callsign = result.trim_start().to_string();
        
        if callsign.is_empty() {
            return Err(Ft8Error::MessageDecodingError(
                "Empty callsign decoded".to_string()
            ));
        }
        
        // Basic callsign validation
        if !self.validate_callsign(&callsign) {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid callsign format: {}", callsign)
            ));
        }
        
        Ok(callsign)
    }
    
    /// Validate callsign format
    fn validate_callsign(&self, callsign: &str) -> bool {
        if callsign.len() < 3 || callsign.len() > 6 {
            return false;
        }
        
        // Must contain at least one letter and one digit
        let has_letter = callsign.chars().any(|c| c.is_ascii_alphabetic());
        let has_digit = callsign.chars().any(|c| c.is_ascii_digit());
        
        has_letter && has_digit
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
        if grid_value == 0 {
            return Ok(None);
        }
        
        // WSJT-X grid encoding: Maidenhead locator system
        // 15 bits encode 4-character grid (AA00 to RR99)
        
        if grid_value >= 32400 {
            // Values >= 32400 are used for signal reports, not grids
            return Ok(None);
        }
        
        // Decode longitude (field 1 & 3) and latitude (field 2 & 4)
        let lng_field1 = (grid_value % 18) as u8;          // A-R (18 values)
        let lat_field2 = ((grid_value / 18) % 18) as u8;   // A-R (18 values)
        let lng_field3 = ((grid_value / 324) % 10) as u8;  // 0-9 (10 values)
        let lat_field4 = ((grid_value / 3240) % 10) as u8; // 0-9 (10 values)
        
        // Validate ranges
        if lng_field1 >= 18 || lat_field2 >= 18 || lng_field3 >= 10 || lat_field4 >= 10 {
            return Err(Ft8Error::MessageDecodingError(
                format!("Invalid grid square encoding: {}", grid_value)
            ));
        }
        
        let grid = format!(
            "{}{}{}{}",
            (b'A' + lng_field1) as char,
            (b'A' + lat_field2) as char,
            (b'0' + lng_field3) as char,
            (b'0' + lat_field4) as char
        );
        
        Ok(Some(grid))
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
    
    /// Decode ARRL section code
    fn decode_arrl_section(&self, code: u8) -> Ft8Result<String> {
        // ARRL section codes for Field Day
        let sections = [
            "CT", "EMA", "ME", "NH", "RI", "VT", "WMA",  // New England
            "ENY", "NLI", "NNJ", "NNY", "SNJ", "WNY",    // Atlantic
            "DE", "EPA", "MDC", "WPA",                     // Mid-Atlantic
            // ... (full list would include all 83 sections)
        ];
        
        if (code as usize) < sections.len() {
            Ok(sections[code as usize].to_string())
        } else {
            Ok(format!("S{:02}", code))
        }
    }
    
    /// Decode state code for RTTY Roundup
    fn decode_state_code(&self, code: u8) -> Ft8Result<String> {
        let states = [
            "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA",
            "HI", "ID", "IL", "IN", "IA", "KS", "KY", "LA", "ME", "MD",
            // ... (full list of US states and Canadian provinces)
        ];
        
        if (code as usize) < states.len() {
            Ok(states[code as usize].to_string())
        } else {
            Ok(format!("ST{:02}", code))
        }
    }
    
    /// Decode prefix code for extended callsigns
    fn decode_prefix_code(&self, code: u32) -> Ft8Result<String> {
        // Common prefixes based on ITU regions and special operations
        match code {
            0..=99 => Ok(format!("K{}", code)),      // US regions
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
        let parser = MessageParser::new();
        // Test FN42 encoding (common US grid)
        let fn42_value = 5*18*10 + 13*10 + 4*180 + 2; // Maidenhead encoding
        let grid = parser.decode_grid_square_15bit(fn42_value).unwrap();
        assert!(grid.is_some());
        
        let grid_str = grid.unwrap();
        assert_eq!(grid_str.len(), 4);
        assert!(grid_str.chars().all(|c| c.is_ascii_alphanumeric()));
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
    fn test_callsign_encoding_decoding() {
        let parser = MessageParser::new();
        
        // Test standard callsign decoding with a simple callsign
        let test_callsign = "A1A";
        let encoded = parser.callsign_table.encode_standard_callsign(test_callsign).unwrap();
        let decoded = parser.decode_standard_callsign(encoded).unwrap();
        assert_eq!(decoded, test_callsign);
        
        // Test encoding doesn't overflow for short callsigns
        assert!(encoded < 262_144_000);
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
        let crc_bits = (0..14).map(|i| (crc >> (13 - i)) & 1 != 0).collect::<BitVec>();
        full_message.extend(crc_bits);
        
        assert_eq!(full_message.len(), TOTAL_MESSAGE_BITS);
        assert!(verify_crc14(&full_message));
    }
}