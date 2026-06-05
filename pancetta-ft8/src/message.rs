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
use std::time::{Duration, SystemTime};

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
                    // WSJT-X / ft8_lib convention: "K1ABC W9XYZ R-12" — the
                    // sign-prefixed report is appended directly to the `R`
                    // with no separating space.
                    write!(f, " R")?;
                    if let Some(report) = self.signal_report {
                        write!(f, "{:+03}", report)?;
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

impl Ft8Message {
    /// Check whether a decoded message looks like a plausible FT8 transmission.
    ///
    /// OSD-2 produces many CRC-14 false positives (random noise that happens to
    /// pass the 1-in-16384 CRC check). This method rejects decoded payloads that
    /// parse into structurally invalid messages: unknown type, missing callsigns,
    /// or text that contains no callsign-like token.
    pub fn is_plausible(&self) -> bool {
        match self.message_type {
            MessageType::Unknown => false,
            MessageType::FreeText => {
                // Free text messages are rare on-air and are a common source
                // of CRC false positives. The previous structural filter
                // (multi-word with one alphabetic word ≥2 chars) was too
                // lenient: Batch 32 Diagnostic Y found 16 FreeText emissions
                // per hard-200 cleared the structural gate but were 100% FP
                // by jt9 baseline (zero TPs). The autonomous-station profile
                // doesn't generate or expect free-text traffic; reject
                // unconditionally. (Future: an opt-in `accept_freetext`
                // config field could re-enable when needed.)
                false
            }
            MessageType::Telemetry => {
                // Telemetry is not used by pancetta and is the #1 source of
                // CRC-14 false positives: random 71-bit noise payloads with
                // i3=0/n3=5 produce valid-looking hex strings ~14% of the time.
                // Reject unconditionally.
                false
            }
            // hb-058: contest-only message types pancetta never operates —
            // ARRL RTTY Roundup (i3=3), ARRL Field Day (i3=0/n3=3,4), and
            // the EU VHF contest (i3=2). Like Telemetry, they are a
            // disproportionate CRC-14 false-positive source: on the curated
            // hard-200 corpus they account for 433 novel (FP-likely) decodes
            // and ZERO jt9-matched recoveries. pancetta is a general / DX
            // station, not a contest logger, so rejecting them is pure
            // precision with no recall cost.
            //
            // Batch 32 Diagnostic Y revisited the original "DXpedition is
            // deliberately not rejected" stance: on K5ARH's full hard-200
            // corpus, 69 DXpedition-typed emissions are 100% FP with 0 jt9
            // truths matching. The original rationale (real DXpeditions are
            // a high-value target) holds in principle, but K5ARH's eval
            // corpus contains no active DXpedition windows. Reject by
            // default; reintroduce as an opt-in mode when an operator is
            // explicitly hunting active DXpeditions. (Future: a
            // `Ft8Config::accept_dxpedition: bool` field could re-enable.)
            //
            // Same applies to FreeText: even after the structural multi-
            // word/alphabetic-word check above (which already rejects most
            // garbage), 16 emissions per hard-200 cleared the structural
            // gate but were FPs. Reject unconditionally for the autonomous-
            // station profile.
            MessageType::Contest
            | MessageType::FieldDay
            | MessageType::RTTYRoundup
            | MessageType::DXpedition => false,
            MessageType::Standard | MessageType::NonStdCall | MessageType::Extended => {
                // ALL present callsigns must look valid.
                let calls: Vec<&str> = [&self.from_callsign, &self.to_callsign]
                    .iter()
                    .filter_map(|opt| opt.as_deref())
                    .collect();

                if calls.is_empty() {
                    return false;
                }

                if !calls.iter().all(|call| Self::looks_like_callsign(call)) {
                    return false;
                }

                // Both callsigns having /R suffix is extremely rare in real
                // traffic but common in CRC-14 collisions on noise (the
                // packed encoding makes /R a low-cost bit pattern).
                if calls.len() == 2 {
                    let both_portable = calls.iter().all(|c| c.contains('/'));
                    if both_portable {
                        return false;
                    }
                }

                // Validate fields THAT ARE PRESENT — don't require absent
                // fields. CRC-collision noise lands wrong by either
                // wrapping a signal_report into impossible territory or by
                // setting grid_square to a malformed string. The protocol
                // also defines a legitimate "empty exchange" code
                // (MAXGRID4+1) which produces a Reply with no grid, used
                // as a bare callsign-pair acknowledgement; we accept it as
                // long as the calls themselves look valid.
                if !self.has_plausible_payload() {
                    return false;
                }

                true
            }
        }
    }

    /// Validate the SOMETHING field for Standard messages.
    ///
    /// Approach: validate fields that ARE PRESENT against protocol rules.
    /// Reject only when a present field is malformed or out of range.
    /// Don't require fields that the protocol allows to be absent.
    ///
    /// CRC-collision noise lands as either a wrapped-i8 signal_report
    /// (e.g., +83 dB, impossible) or as a malformed grid string. Both
    /// are caught here.
    ///
    /// The legitimate "empty exchange" protocol code (MAXGRID4+1)
    /// produces a Reply with no grid — used as a bare callsign-pair
    /// ack. We accept it; the both-calls-valid gate above already
    /// filters the noise patterns most likely to land here.
    fn has_plausible_payload(&self) -> bool {
        // Non-Standard formats that survive to here (DXpedition,
        // NonStdCall, Extended) carry their own format-specific fields
        // (contest_exchange, etc.) and don't use standard_type. Accept
        // them once both calls have already been validated by the caller.
        // (The contest types RTTYRoundup / Contest / FieldDay are rejected
        // outright by is_plausible before reaching this helper — hb-058.)
        if !matches!(self.message_type, MessageType::Standard) {
            return true;
        }

        // CQ messages: parser sets to_callsign = None and from_callsign
        // = the calling station, so the "two calls present" gate above
        // sees only one call. They're identified by standard_type = Cq.
        // (Some early text-path parses leave standard_type unset and
        // mark to_callsign = "CQ" instead — accept that form too.)
        if let Some(ref to) = self.to_callsign {
            if to == "CQ" || to.starts_with("CQ ") {
                return true;
            }
        }

        let Some(stype) = self.standard_type else {
            // Standard message with no recognized subtype = parser
            // couldn't classify the payload. Reject as uninterpretable.
            return false;
        };

        match stype {
            // hb-072: validate the CQ modifier (special_operation) against
            // a whitelist of known FT8 conventions. The fp_format_audit
            // (hb-058 instrument) showed 55 hard-200 novels with "CQ
            // <something>" where the modifier was garbage (CRC-14
            // collision noise); the corresponding 77 real "CQ <modifier>"
            // decodes all use modifiers in the whitelist below. None when
            // bare CQ (no modifier) — accept.
            StandardMessageType::Cq => match self.special_operation.as_deref() {
                None => true,
                Some(m) => Self::is_valid_cq_modifier(m),
            },

            // Token-based exchanges: protocol-defined, no field-level
            // validation needed beyond what the parser already enforced.
            StandardMessageType::Rrr | StandardMessageType::RR73 | StandardMessageType::Final73 => {
                true
            }

            // Reply / ReplyWithR: grid is optional (the "empty exchange"
            // protocol code lands here with grid = None — a valid bare
            // callsign-pair ack). When grid IS present, it must be a
            // valid 4-char Maidenhead — a malformed grid string means
            // the unpacker landed on garbage from a CRC collision.
            StandardMessageType::Reply | StandardMessageType::ReplyWithR => {
                match self.grid_square.as_deref() {
                    Some(g) => Self::is_valid_4char_maidenhead(g),
                    None => true,
                }
            }

            // Report / ReportWithR: signal_report must be in the valid FT8
            // range. The protocol packs reports as `igrid4 - MAXGRID4 - 35`
            // cast to i8, so unallocated igrid4 values wrap to nonsense
            // numbers (e.g., +83 dB). Accept only physically possible
            // values; -50..+49 is generous (real-world FT8 reports are
            // -30..+20, but allow margin for intentionally extreme tests).
            StandardMessageType::Report | StandardMessageType::ReportWithR => {
                match self.signal_report {
                    Some(r) => (-50..=49).contains(&r),
                    None => false,
                }
            }
        }
    }

    /// Check that `g` is a valid 4-character Maidenhead grid (e.g., FN42).
    /// First two chars in A-R, next two in 0-9. Used to filter wrap-induced
    /// "grid" strings that pass mixed-radix bounds but aren't real locators.
    fn is_valid_4char_maidenhead(g: &str) -> bool {
        let chars: Vec<char> = g.chars().collect();
        if chars.len() != 4 {
            return false;
        }
        ('A'..='R').contains(&chars[0])
            && ('A'..='R').contains(&chars[1])
            && chars[2].is_ascii_digit()
            && chars[3].is_ascii_digit()
    }

    /// Heuristic suspicion score (0 = normal, higher = more suspicious).
    ///
    /// Used for progressive validation: messages with low confidence AND
    /// high suspicion are rejected, while either alone is acceptable.
    pub fn suspicion_score(&self) -> u32 {
        let mut score = 0u32;

        let calls: Vec<&str> = [&self.from_callsign, &self.to_callsign]
            .iter()
            .filter_map(|opt| opt.as_deref())
            .collect();

        // Both callsigns exactly 6 characters (packed space-fill) — common
        // in CRC collisions, uncommon in real QSOs
        if calls.len() == 2 && calls.iter().all(|c| c.replace('/', "").len() == 6) {
            score += 1;
        }

        // Any callsign has a portable suffix (/R, /P, etc.)
        if calls.iter().any(|c| c.contains('/')) {
            score += 1;
        }

        // CQ with non-standard modifier (not DX, NA, EU, RU, AS, AF, etc.)
        if let Some(ref op) = self.special_operation {
            let known = [
                "DX", "NA", "EU", "AS", "AF", "SA", "OC", "AN", "RU", "POTA", "SOTA", "QRP", "FD",
                "TU", "TEST",
            ];
            let is_numeric = op.chars().all(|c| c.is_ascii_digit());
            if !known.contains(&op.as_str()) && !is_numeric {
                score += 2;
            }
        }

        score
    }

    /// Check if a callsign prefix matches a valid ITU allocation.
    ///
    /// Extracts the 1-2 character prefix from a callsign and checks it against
    /// known ITU prefix allocations. This eliminates OSD false positives like
    /// "QY3HUG", "XO4XKQ", "H63SII" which have structurally valid callsign
    /// formats but use prefixes never allocated by the ITU.
    ///
    /// Prefix extraction:
    /// - Starts with digit: prefix is first 2 chars (e.g., "3B8ABC" -> "3B")
    /// - Starts with letter + digit: prefix is first letter (e.g., "W1ABC" -> "W")
    /// - Starts with two letters: prefix is first 2 letters (e.g., "VE3XYZ" -> "VE")
    fn is_valid_itu_prefix(callsign: &str) -> bool {
        let chars: Vec<char> = callsign.chars().collect();
        if chars.len() < 3 {
            return false;
        }

        // Extract prefix based on pattern
        if chars[0].is_ascii_digit() {
            // Starts with digit: prefix is 2 chars (e.g., 3B, 4X, 9A)
            if chars.len() < 2 {
                return false;
            }
            let prefix: String = chars[..2].iter().collect();
            return Self::is_allocated_prefix_2char_numeric(&prefix);
        }

        if chars[0].is_ascii_alphabetic() && chars.len() > 1 && chars[1].is_ascii_digit() {
            // Could be single letter prefix (e.g., W1ABC -> W) or
            // letter+digit prefix (e.g., A71A -> A7, H44ABC -> H4)
            if Self::is_allocated_prefix_1char(chars[0]) {
                return true;
            }
            // Check letter+digit as a 2-char prefix
            let prefix: String = chars[..2].iter().collect();
            return Self::is_allocated_prefix_letter_digit(&prefix);
        }

        if chars[0].is_ascii_alphabetic() && chars.len() > 1 && chars[1].is_ascii_alphabetic() {
            // Two letter prefix (e.g., VE, JA, EA, DL)
            let prefix: String = chars[..2].iter().collect();
            return Self::is_allocated_prefix_2char_alpha(&prefix);
        }

        false
    }

    /// Check single-letter ITU prefix allocations.
    ///
    /// Only letters that are used as standalone single-char prefixes (letter+digit)
    /// return true. Letters like H, L, O, X always require a second letter to form
    /// a valid prefix (e.g., HA, HB, not H3). This is important for rejecting
    /// false positives like "H63SII".
    fn is_allocated_prefix_1char(c: char) -> bool {
        // These letters are allocated as standalone single-letter prefixes:
        // B (China: B1-B9), C (various), D (Germany: D1-D9 via DA-DR block),
        // E (various), F (France), G (UK), I (Italy), J (Japan: JA block covers J1-J9),
        // K (USA), M (UK), N (USA), R (Russia), S (Sweden/Poland), T (various),
        // U (Russia/Ukraine/etc), V (various), W (USA), Y (various), Z (various)
        //
        // NOT standalone: A (always 2-char like AA, AP), H (always HA, HB, etc.),
        // L (always LA, LU, etc.), O (always OA, OE, etc.), P (always PA, PY, etc.),
        // Q (reserved for Q-codes), X (always XE, XU, etc.)
        matches!(
            c,
            'B' | 'C'
                | 'D'
                | 'E'
                | 'F'
                | 'G'
                | 'I'
                | 'J'
                | 'K'
                | 'M'
                | 'N'
                | 'R'
                | 'S'
                | 'T'
                | 'U'
                | 'V'
                | 'W'
                | 'Y'
                | 'Z'
        )
    }

    /// Check 2-char alphabetic prefix allocations (letter+letter).
    /// Covers the major ITU allocations. When in doubt, accept.
    fn is_allocated_prefix_2char_alpha(prefix: &str) -> bool {
        let bytes = prefix.as_bytes();
        if bytes.len() != 2 {
            return false;
        }
        let first = bytes[0];
        let second = bytes[1];
        match first {
            // A: AA-AL=USA, AP=Pakistan, A2=Botswana, A3=Tonga, A4=Oman,
            //    A5=Bhutan, A6=UAE, A7=Qatar, A9=Bahrain
            b'A' => matches!(second, b'A'..=b'L' | b'M'..=b'P' | b'R'..=b'Z'),
            // B: BA-BZ=China (BA-BT), BV=Taiwan, BY=China
            b'B' => true, // All B? allocated
            // C: CA-CE=Chile, CF-CK=Canada, CM-CO=Cuba, CP=Bolivia,
            //    CQ-CU=Portugal, CV-CX=Uruguay, CY-CZ=Canada, CN=Morocco
            b'C' => true, // All C? allocated
            // D: DA-DR=Germany, DS-DT=South Korea, DU-DZ=Philippines
            b'D' => true, // All D? allocated
            // E: EA-EH=Spain, EI=Ireland, EK=Armenia, EL=Liberia,
            //    EP-EQ=Iran, ER=Moldova, ES=Estonia, ET=Ethiopia, EU-EW=Belarus, EX=Kyrgyzstan, EY=Tajikistan, EZ=Turkmenistan
            b'E' => true, // All E? allocated
            // F: FA-FZ=France
            b'F' => true,
            // G: GA-GZ=UK
            b'G' => true,
            // H: HA-HB=Hungary/Switzerland, HC-HD=Ecuador, HE=Switzerland,
            //    HF=Poland, HH=Haiti, HI=Dominican Republic, HJ-HK=Colombia,
            //    HL=South Korea, HM=North Korea, HP=Panama, HQ=Honduras,
            //    HR=Honduras, HS=Thailand, HT=Nicaragua, HU=El Salvador, HV=Vatican, HZ=Saudi Arabia
            b'H' => matches!(second, b'A'..=b'Z'),
            // I: IA-IZ=Italy
            b'I' => true,
            // J: JA-JS=Japan, JT-JV=Mongolia, JW-JX=Norway, JY=Jordan, JZ=Indonesia
            //    JD=Ogasawara/Minami Torishima
            b'J' => true, // All J? allocated
            // K: KA-KZ=USA
            b'K' => true,
            // L: LA-LN=Norway, LO-LW=Argentina, LX=Luxembourg, LY=Lithuania, LZ=Bulgaria
            b'L' => matches!(second, b'A'..=b'N' | b'O'..=b'W' | b'X' | b'Y' | b'Z'),
            // M: MA-MZ=UK
            b'M' => true,
            // N: NA-NZ=USA
            b'N' => true,
            // O: OA-OC=Peru, OD=Lebanon, OE=Austria, OF-OJ=Finland, OK-OL=Czech,
            //    OM=Slovakia, ON-OT=Belgium, OU-OZ=Denmark
            b'O' => true, // All O? allocated
            // P: PA-PI=Netherlands, PJ=Netherlands Antilles, PK-PO=Indonesia,
            //    PP-PY=Brazil, PZ=Suriname
            b'P' => true, // All P? allocated
            // Q: QA-QZ reserved for Q-codes, NOT valid callsign prefixes
            b'Q' => false,
            // R: RA-RZ=Russia
            b'R' => true,
            // S: SA-SM=Sweden, SN-SR=Poland, SS-SM=Egypt, ST=Sudan, SU=Egypt,
            //    SV-SZ=Greece
            b'S' => true, // All S? allocated
            // T: TA-TC=Turkey, TD=Guatemala, TE=Costa Rica, TF=Iceland,
            //    TG=Guatemala, TI=Costa Rica, TJ=Cameroon, TK=Corsica,
            //    TL=Central Africa, TN=Congo, TO-TQ=France overseas, TR=Gabon,
            //    TS=Tunisia, TT=Chad, TU=Ivory Coast, TY=Benin, TZ=Mali
            b'T' => matches!(second, b'A'..=b'U' | b'Y' | b'Z'),
            // U: UA-UI=Russia, UJ-UM=Uzbekistan, UN-UQ=Kazakhstan, UR-UZ=Ukraine
            b'U' => matches!(second, b'A'..=b'Z'),
            // V: VA-VG=Canada, VH-VN=Australia, VO=Canada, VP-VQ=UK overseas,
            //    VR=Hong Kong, VS=UK overseas, VU=India, VV-VW=unassigned?, VX-VY=Canada, VZ=Australia
            b'V' => {
                matches!(second, b'A'..=b'G' | b'H'..=b'N' | b'O' | b'P'..=b'Q' | b'R' | b'S' | b'U' | b'X'..=b'Z')
            }
            // W: WA-WZ=USA
            b'W' => true,
            // X: XA-XI=Mexico, XJ-XO=Canada, XP=Denmark(Greenland), XQ-XR=Chile,
            //    XS=China, XT=Burkina Faso, XU=Cambodia, XV=Vietnam, XW=Laos,
            //    XX=Macao, XY-XZ=Myanmar
            b'X' => {
                matches!(second, b'A'..=b'I' | b'J'..=b'O' | b'P' | b'Q'..=b'R' | b'S' | b'T' | b'U' | b'V' | b'W' | b'X' | b'Y'..=b'Z')
            }
            // Y: YA=Afghanistan, YB-YH=Indonesia, YI=Iraq, YJ=Vanuatu,
            //    YK=Syria, YL=Latvia, YM=Turkey, YN=Nicaragua, YO=Romania,
            //    YS=El Salvador, YT-YU=Serbia, YV-YY=Venezuela, YZ=Serbia
            // NOT: YP, YQ, YR are Romania
            b'Y' => matches!(second, b'A'..=b'Z'),
            // Z: ZA=Albania, ZB-ZJ=UK overseas, ZK-ZM=New Zealand, ZN-ZO=UK overseas,
            //    ZP=Paraguay, ZR-ZU=South Africa, ZV-ZZ=Brazil
            b'Z' => matches!(second, b'A'..=b'U' | b'V'..=b'Z'),
            _ => false,
        }
    }

    /// Check letter+digit 2-char prefix allocations (e.g., A7=Qatar, H4=Solomon Islands).
    /// These are prefixes where the first character is a letter and second is a digit,
    /// but the letter alone is NOT a standalone prefix.
    fn is_allocated_prefix_letter_digit(prefix: &str) -> bool {
        let bytes = prefix.as_bytes();
        if bytes.len() != 2 || !bytes[0].is_ascii_alphabetic() || !bytes[1].is_ascii_digit() {
            return false;
        }
        // ITU allocated letter+digit prefixes (non-exhaustive, covering major ones)
        matches!(
            prefix,
            // A: A2=Botswana, A3=Tonga, A4=Oman, A5=Bhutan, A6=UAE, A7=Qatar, A9=Bahrain
            "A2" | "A3" | "A4" | "A5" | "A6" | "A7" | "A9" |
            // H: H4=Solomon Islands, H4 is the only H+digit allocation
            "H4" |
            // L: L2-L9=Argentina (LU block)
            "L2" | "L3" | "L4" | "L5" | "L6" | "L7" | "L8" | "L9" |
            // O: no standalone O+digit
            // P: P2=Papua New Guinea, P4=Aruba, P5=North Korea
            "P2" | "P4" | "P5" |
            // X: no standalone X+digit
            // A catch-all for any we might have missed: be permissive for common ones
            "O2" | "O3" | "O4" | "O5" | "O6" | "O7" | "O8" | "O9"
        )
    }

    /// Check 2-char prefix starting with a digit (e.g., 3B, 4X, 9A).
    fn is_allocated_prefix_2char_numeric(prefix: &str) -> bool {
        let bytes = prefix.as_bytes();
        if bytes.len() != 2 || !bytes[1].is_ascii_alphabetic() {
            return false;
        }
        let first = bytes[0];
        let second = bytes[1].to_ascii_uppercase();
        match first {
            // 2: 2D-2M=UK, 2E=UK
            b'2' => matches!(second, b'D' | b'E' | b'I' | b'J' | b'M' | b'W'),
            // 3: 3A=Monaco, 3B=Mauritius, 3C=Equatorial Guinea, 3D=Eswatini/Fiji,
            //    3G=Chile, 3V=Tunisia, 3W=Vietnam, 3X=Guinea, 3Y=Bouvet, 3Z=Poland
            b'3' => matches!(
                second,
                b'A' | b'B' | b'C' | b'D' | b'G' | b'V' | b'W' | b'X' | b'Y' | b'Z'
            ),
            // 4: 4J-4K=Azerbaijan, 4L=Georgia, 4M=Venezuela, 4O=Montenegro,
            //    4S=Sri Lanka, 4U=UN, 4V=Haiti, 4W=Timor-Leste, 4X=Israel, 4Z=Israel
            b'4' => matches!(
                second,
                b'J' | b'K' | b'L' | b'M' | b'O' | b'S' | b'U' | b'V' | b'W' | b'X' | b'Z'
            ),
            // 5: 5A=Libya, 5B=Cyprus, 5C=Morocco, 5H-5I=Tanzania, 5N-5O=Nigeria,
            //    5R-5S=Madagascar, 5T=Mauritania, 5U=Niger, 5V=Togo, 5W=Samoa,
            //    5X=Uganda, 5Y-5Z=Kenya
            b'5' => matches!(
                second,
                b'A' | b'B'
                    | b'C'
                    | b'H'
                    | b'I'
                    | b'N'
                    | b'O'
                    | b'R'
                    | b'S'
                    | b'T'
                    | b'U'
                    | b'V'
                    | b'W'
                    | b'X'
                    | b'Y'
                    | b'Z'
            ),
            // 6: 6K-6N=South Korea, 6O=Somalia, 6V-6W=Senegal, 6Y=Jamaica
            b'6' => matches!(second, b'K'..=b'N' | b'O' | b'V' | b'W' | b'Y'),
            // 7: 7J-7N=Japan, 7O=Yemen, 7P=Lesotho, 7Q=Malawi, 7R=Algeria,
            //    7S=Sweden, 7T-7Y=Algeria, 7X=Algeria, 7Z=Saudi Arabia
            b'7' => {
                matches!(second, b'J'..=b'N' | b'O' | b'P' | b'Q' | b'R' | b'S' | b'T'..=b'Y' | b'Z')
            }
            // 8: 8P=Barbados, 8Q=Maldives, 8R=Guyana, 8S=Sweden, 8J-8N=Japan
            b'8' => matches!(second, b'J'..=b'N' | b'P' | b'Q' | b'R' | b'S'),
            // 9: 9A=Croatia, 9G=Ghana, 9H=Malta, 9I-9J=Zambia, 9K=Kuwait,
            //    9L=Sierra Leone, 9M=Malaysia, 9N=Nepal, 9O-9T=Congo (DRC),
            //    9U=Burundi, 9V=Singapore, 9W=Malaysia, 9X=Rwanda, 9Y-9Z=Trinidad
            b'9' => matches!(
                second,
                b'A' | b'G' | b'H' | b'I' | b'J' | b'K' | b'L' | b'M' | b'N' | b'O'
                    ..=b'T' | b'U' | b'V' | b'W' | b'X' | b'Y' | b'Z'
            ),
            _ => false,
        }
    }

    /// Check if a string looks like a ham radio callsign.
    ///
    /// Uses the FT8 packed callsign format constraints. The 28-bit encoding
    /// packs calls as: c0(37) * c1(36) * c2(10) * c3(27) * c4(27) * c5(27).
    /// Position c2 is always a digit (0-9), c3-c5 are letters or space,
    /// c0 is space/digit/letter, c1 is digit/letter. This means the third
    /// hb-072: validate a CQ message's modifier (`special_operation`)
    /// against the known FT8 conventions. Real `CQ <modifier>` traffic on
    /// the hard-200 corpus uses one of: a continent / propagation
    /// indicator (DX, NA, SA, EU, AS, AF, OC), a power class (QRP), a
    /// program tag (POTA, SOTA, FD, RU, TEST), an all-digit CQ-zone /
    /// numeric exchange (≤ 3 digits), or a short alpha prefix /
    /// state code (≤ 3 letters, e.g. K, W, JA, NY). CRC-14 collision
    /// noise typically lands on garbage tokens outside this set — the
    /// fp_format_audit (hb-058) found 55 hard-200 novels here vs 77
    /// real, and the real ones all match this whitelist.
    fn is_valid_cq_modifier(modifier: &str) -> bool {
        if modifier.is_empty() {
            return true;
        }
        let upper: String = modifier.to_ascii_uppercase();
        // Named modifiers seen in real on-air traffic.
        if matches!(
            upper.as_str(),
            "DX" | "NA"
                | "SA"
                | "EU"
                | "AS"
                | "AF"
                | "OC"
                | "QRP"
                | "POTA"
                | "SOTA"
                | "FD"
                | "RU"
                | "TEST"
        ) {
            return true;
        }
        let len = modifier.chars().count();
        if !(1..=4).contains(&len) {
            return false;
        }
        // CQ zone / numeric exchange (1-3 digits).
        if len <= 3 && modifier.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        // Short alpha prefix / state code (1-3 letters).
        if len <= 3 && modifier.chars().all(|c| c.is_ascii_alphabetic()) {
            return true;
        }
        false
    }

    /// character is always a digit, and chars 4-6 are always letters/space.
    ///
    /// Valid examples: W1AW, KA7RLM, R9AA, 4X1RF, 3DA0WW, 9A1A.
    /// Rejects: 817ZOH (positions 0-2 all digits), Q8JCE (no digit at pos 2).
    fn looks_like_callsign(s: &str) -> bool {
        // Hash-based callsigns like <...nnn> are OK
        if s.starts_with('<') {
            return true;
        }
        // Strip /R, /P, /MM etc. portable suffixes for validation
        let base = s.split('/').next().unwrap_or(s);
        let chars: Vec<char> = base.chars().collect();
        let len = chars.len();
        // Packed callsigns are always 6 chars (space-padded), but after
        // trimming they're 3-6 chars.
        if len < 3 || len > 6 {
            return false;
        }
        if !chars.iter().all(|c| c.is_ascii_alphanumeric()) {
            return false;
        }

        // The FT8 encoding guarantees: in the 6-char packed representation,
        // position 2 (0-indexed) is always a digit. For trimmed calls shorter
        // than 3 chars at the front, position 2 in the PACKED form maps to
        // different positions in the trimmed form. But since unpack_basecall
        // already validated the encoding, we just need to check the structural
        // pattern: there must be exactly one digit "separator" with letters
        // after it (the suffix).
        //
        // Find the last digit — suffix letters come after it.
        let last_digit_pos = chars.iter().rposition(|c| c.is_ascii_digit());
        match last_digit_pos {
            Some(pos) => {
                // Must have at least 1 suffix letter after the last digit
                let suffix_len = len - pos - 1;
                if suffix_len < 1 {
                    return false;
                }
                // Suffix must be all letters
                if !chars[pos + 1..].iter().all(|c| c.is_ascii_alphabetic()) {
                    return false;
                }
                // The last digit should be at position 0, 1, or 2
                // (corresponding to the c2 digit in the 6-char packed form).
                // Position 3+ would mean digits in the suffix region.
                if pos > 2 {
                    return false;
                }
                // Count total digits — at most 2 (e.g., 4V2, 3D0)
                let digit_count = chars.iter().filter(|c| c.is_ascii_digit()).count();
                if digit_count > 2 {
                    return false;
                }
                // Check ITU prefix validity to reject OSD false positives
                // with structurally valid but unallocated prefixes (e.g., QY, XO, H6)
                if !Self::is_valid_itu_prefix(base) {
                    return false;
                }
                true
            }
            None => false, // no digits at all
        }
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
    /// Tone symbols (79 values, 0-7) for signal reconstruction in multi-pass subtraction.
    /// None if symbols were not preserved during decoding.
    pub tone_symbols: Option<Vec<u8>>,
    /// AP (A Priori) level used for this decode: 0 = no AP, 1-4 = AP level used.
    pub ap_level: u8,
    /// Parity of the FT8 slot whose audio produced this decode. `None` until
    /// the coordinator's decoder dispatch tags it (which it does for every
    /// message routed to TUI / QSO / autonomous). Constructors leave it
    /// unset because they don't have access to the slot timing.
    pub slot_parity: Option<pancetta_core::slot::SlotParity>,
    /// hb-129: Time elapsed from window-start until this decode passed CRC
    /// and became available (presentation-time, not arrival-time).
    /// `None` for decodes produced by external paths (ft8_lib FFI) or
    /// for unit-test scaffolding where the decode pipeline doesn't run.
    /// Used by the research harness to compute Time-To-First-Decode (TTFD)
    /// per WAV — see `research/ideation/2026-06-01-metric.md` M1.
    pub decode_time_into_window: Option<Duration>,
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
            tone_symbols: None,
            ap_level: 0,
            slot_parity: None,
            decode_time_into_window: None,
        }
    }

    /// Create a DecodedMessage from ft8_lib FFI output.
    ///
    /// Parses the text string into an Ft8Message with best-effort field
    /// extraction.  The SNR comes from ft8_lib's `status.time` field
    /// (which is actually the SNR estimate in the C code), and frequency
    /// from `status.freq`.
    pub fn from_ft8lib(text: &str, freq_hz: f32, snr: f32, ldpc_errors: i32) -> Self {
        let message = Ft8Message::from_text(text);
        Self {
            text: text.to_string(),
            message,
            snr_db: snr,
            confidence: if ldpc_errors == 0 { 1.0 } else { 0.8 },
            frequency_offset: freq_hz as f64,
            time_offset: 0.0,
            timestamp: SystemTime::now(),
            error_corrections: ldpc_errors.clamp(0, 255) as u8,
            tone_symbols: None,
            ap_level: 0,
            slot_parity: None,
            decode_time_into_window: None,
        }
    }
}

impl Ft8Message {
    /// Parse an FT8 message from its text representation.
    ///
    /// Handles common formats produced by ft8_lib / WSJT-X:
    /// - `CQ [DX|NA|...] <call> <grid>`
    /// - `<to> <from> <grid>`
    /// - `<to> <from> [R] <report>`
    /// - `<to> <from> RR73|RRR|73`
    pub fn from_text(text: &str) -> Self {
        let parts: Vec<&str> = text.split_whitespace().collect();
        let mut msg = Ft8Message::default();
        msg.message_type = MessageType::Standard;

        if parts.is_empty() {
            msg.message_type = MessageType::FreeText;
            msg.text = Some(text.to_string());
            return msg;
        }

        if parts[0] == "CQ" {
            msg.standard_type = Some(StandardMessageType::Cq);
            let mut idx = 1;
            // Check for CQ modifier (DX, NA, POTA, etc.)
            if idx < parts.len()
                && !Self::text_looks_like_callsign(parts[idx])
                && !Self::text_looks_like_grid(parts[idx])
            {
                msg.special_operation = Some(parts[idx].to_string());
                idx += 1;
            }
            if idx < parts.len() {
                msg.from_callsign = Some(parts[idx].to_string());
                idx += 1;
            }
            if idx < parts.len() && Self::text_looks_like_grid(parts[idx]) {
                msg.grid_square = Some(parts[idx].to_string());
            }
            return msg;
        }

        // Two-callsign messages: <to> <from> <suffix>
        if parts.len() >= 2 {
            msg.to_callsign = Some(parts[0].to_string());
            msg.from_callsign = Some(parts[1].to_string());

            if parts.len() == 2 {
                // Bare exchange — treat as reply
                msg.standard_type = Some(StandardMessageType::Reply);
                return msg;
            }

            let rest = &parts[2..];
            match rest {
                ["RR73"] => msg.standard_type = Some(StandardMessageType::RR73),
                ["RRR"] => msg.standard_type = Some(StandardMessageType::Rrr),
                ["73"] => msg.standard_type = Some(StandardMessageType::Final73),
                ["R", grid] if Self::text_looks_like_grid(grid) => {
                    msg.standard_type = Some(StandardMessageType::ReplyWithR);
                    msg.grid_square = Some(grid.to_string());
                }
                ["R", report] if Self::text_looks_like_report(report) => {
                    msg.standard_type = Some(StandardMessageType::ReportWithR);
                    msg.signal_report = Self::text_parse_report(report);
                }
                [grid] if Self::text_looks_like_grid(grid) => {
                    msg.standard_type = Some(StandardMessageType::Reply);
                    msg.grid_square = Some(grid.to_string());
                }
                [report] if Self::text_looks_like_report(report) => {
                    msg.standard_type = Some(StandardMessageType::Report);
                    msg.signal_report = Self::text_parse_report(report);
                }
                _ => {
                    // Unrecognized suffix — store as free text
                    msg.standard_type = Some(StandardMessageType::Reply);
                }
            }
            return msg;
        }

        // Fallback: free text
        msg.message_type = MessageType::FreeText;
        msg.text = Some(text.to_string());
        msg
    }

    fn text_looks_like_callsign(s: &str) -> bool {
        s.len() >= 3
            && s.len() <= 10
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
            && s.chars().any(|c| c.is_ascii_digit())
    }

    fn text_looks_like_grid(s: &str) -> bool {
        if s.len() != 4 {
            return false;
        }
        let b = s.as_bytes();
        (b'A'..=b'R').contains(&b[0].to_ascii_uppercase())
            && (b'A'..=b'R').contains(&b[1].to_ascii_uppercase())
            && b[2].is_ascii_digit()
            && b[3].is_ascii_digit()
    }

    fn text_looks_like_report(s: &str) -> bool {
        if s.len() < 2 {
            return false;
        }
        let first = s.as_bytes()[0];
        (first == b'+' || first == b'-') && s[1..].chars().all(|c| c.is_ascii_digit())
    }

    fn text_parse_report(s: &str) -> Option<i8> {
        s.parse::<i8>().ok()
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

/// FT8 message parser
pub struct MessageParser {
    /// Hash table for worked stations (10/12/22-bit hashes)
    hash_table: HashTable,
}

impl MessageParser {
    /// Create a new message parser
    pub fn new() -> Self {
        Self {
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

        // Filter "RR73" from grids — it's a QSO-completion token, not a
        // Maidenhead locator (igrid4=32373 collides with the RR73 token at
        // igrid4=32403). Allow grids on both CQ and reply messages.
        let filtered_grid = grid.as_ref().filter(|g| g.as_str() != "RR73").cloned();

        if is_cq {
            message.standard_type = Some(StandardMessageType::Cq);
            if let CallsignField::Cq(modifier) = &call_a {
                if let Some(m) = modifier {
                    message.special_operation = Some(m.clone());
                }
            }
            message.from_callsign = call_b_str;
            message.grid_square = filtered_grid;
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
            message.signal_report = Some(rpt);
            if ir != 0 {
                message.standard_type = Some(StandardMessageType::ReportWithR);
            } else {
                message.standard_type = Some(StandardMessageType::Report);
            }
            message.to_callsign = call_a_str;
            message.from_callsign = call_b_str;
        } else if grid.is_some() {
            // Non-CQ with grid — reply carrying a grid locator
            if ir != 0 {
                message.standard_type = Some(StandardMessageType::ReplyWithR);
            } else {
                message.standard_type = Some(StandardMessageType::Reply);
            }
            message.to_callsign = call_a_str;
            message.from_callsign = call_b_str;
            message.grid_square = filtered_grid;
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
        _n3: u32,
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
    let _num_bytes = (NUM_BITS + 7) / 8; // 11 bytes
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

    // ----- hb-029: exact-format Display assertions for every
    // StandardMessageType variant. These guard against regressions of the
    // hb-023 type (where ReportWithR was emitting "R -12" with a stray
    // space). Each test builds a minimal Ft8Message for the variant and
    // asserts `to_string()` exactly equals the WSJT-X / ft8_lib reference
    // format.

    #[test]
    fn test_cq_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Cq);
        m.from_callsign = Some("W1ABC".to_string());
        m.grid_square = Some("FN42".to_string());
        assert_eq!(m.to_string(), "CQ W1ABC FN42");
    }

    #[test]
    fn test_cq_with_modifier_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Cq);
        m.special_operation = Some("DX".to_string());
        m.from_callsign = Some("W1ABC".to_string());
        m.grid_square = Some("FN42".to_string());
        assert_eq!(m.to_string(), "CQ DX W1ABC FN42");
    }

    #[test]
    fn test_reply_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Reply);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        m.grid_square = Some("EM48".to_string());
        assert_eq!(m.to_string(), "K1ABC W9XYZ EM48");
    }

    #[test]
    fn test_reply_with_r_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::ReplyWithR);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        m.grid_square = Some("EM48".to_string());
        // ft8_lib unpackgrid (vendor/ft8_lib/ft8/message.c:1104) writes
        // "R " (with trailing space) before the grid in the ir=1 case.
        assert_eq!(m.to_string(), "K1ABC W9XYZ R EM48");
    }

    #[test]
    fn test_report_negative_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Report);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        m.signal_report = Some(-10);
        assert_eq!(m.to_string(), "K1ABC W9XYZ -10");
    }

    #[test]
    fn test_report_positive_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Report);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        m.signal_report = Some(5);
        assert_eq!(m.to_string(), "K1ABC W9XYZ +05");
    }

    #[test]
    fn test_rrr_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Rrr);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        assert_eq!(m.to_string(), "K1ABC W9XYZ RRR");
    }

    #[test]
    fn test_final73_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Final73);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        assert_eq!(m.to_string(), "K1ABC W9XYZ 73");
    }

    #[test]
    fn test_rr73_exact_format() {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::RR73);
        m.to_callsign = Some("K1ABC".to_string());
        m.from_callsign = Some("W9XYZ".to_string());
        assert_eq!(m.to_string(), "K1ABC W9XYZ RR73");
    }

    #[test]
    fn test_report_with_r_display_no_space_before_report() {
        // hb-023: WSJT-X / ft8_lib format the Roger+report response as
        // "K1ABC W9XYZ R-12" — the `R` is immediately followed by the
        // signed report with no separating space. The decoder must
        // produce text that matches this convention so that round-tripped
        // messages compare equal to their source text (and the synth
        // corpus eval can recognize them).
        let mut message = Ft8Message::default();
        message.message_type = MessageType::Standard;
        message.standard_type = Some(StandardMessageType::ReportWithR);
        message.to_callsign = Some("K1ABC".to_string());
        message.from_callsign = Some("W9XYZ".to_string());
        message.signal_report = Some(-12);

        let display = message.to_string();
        assert_eq!(display, "K1ABC W9XYZ R-12");
    }

    #[test]
    fn test_report_with_r_display_positive_report() {
        let mut message = Ft8Message::default();
        message.message_type = MessageType::Standard;
        message.standard_type = Some(StandardMessageType::ReportWithR);
        message.to_callsign = Some("K1ABC".to_string());
        message.from_callsign = Some("W9XYZ".to_string());
        message.signal_report = Some(5);

        let display = message.to_string();
        assert_eq!(display, "K1ABC W9XYZ R+05");
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
    fn decoded_message_default_slot_parity_is_none() {
        let ft8_msg = Ft8Message::default();
        let decoded = DecodedMessage::new(ft8_msg, -10.0, 0.9, 1500.0, 0.0);
        assert_eq!(decoded.slot_parity, None);
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
    fn test_itu_prefix_validation() {
        // Valid callsigns that should pass
        assert!(Ft8Message::is_valid_itu_prefix("W1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("K1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("VE3XYZ"));
        assert!(Ft8Message::is_valid_itu_prefix("JA1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("DL1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("R9AA"));
        assert!(Ft8Message::is_valid_itu_prefix("4X1RF"));
        assert!(Ft8Message::is_valid_itu_prefix("3B8ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("9A1A"));
        assert!(Ft8Message::is_valid_itu_prefix("F5ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("G3XYZ"));
        assert!(Ft8Message::is_valid_itu_prefix("HL1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("ZL1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("VK2ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("PY1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("LU1ABC"));
        assert!(Ft8Message::is_valid_itu_prefix("5N1ABC"));

        // Known OSD false positives that should FAIL
        assert!(
            !Ft8Message::is_valid_itu_prefix("QY3HUG"),
            "QY is not allocated (Q reserved for Q-codes)"
        );
        // XO is technically allocated to Canada (XJ-XO), so it passes ITU check.
        // H6 is not an allocated prefix (H requires 2-letter like HA, HB, or H4)
        assert!(
            !Ft8Message::is_valid_itu_prefix("H63SII"),
            "H6 is not an allocated prefix"
        );

        // Letter+digit prefixes (not standalone letter, but valid 2-char)
        assert!(Ft8Message::is_valid_itu_prefix("A71A"), "A7 = Qatar");
        assert!(Ft8Message::is_valid_itu_prefix("A61ABC"), "A6 = UAE");
        assert!(Ft8Message::is_valid_itu_prefix("P49ABC"), "P4 = Aruba");
        assert!(
            Ft8Message::is_valid_itu_prefix("H44ABC"),
            "H4 = Solomon Islands"
        );

        // Edge cases
        assert!(!Ft8Message::is_valid_itu_prefix("AB"), "Too short");
    }

    #[test]
    fn test_looks_like_callsign_with_itu() {
        // Valid callsigns pass both structural and ITU checks
        assert!(Ft8Message::looks_like_callsign("W1ABC"));
        assert!(Ft8Message::looks_like_callsign("K1ABC"));
        assert!(Ft8Message::looks_like_callsign("VE3XYZ"));
        assert!(Ft8Message::looks_like_callsign("JA1ABC"));
        assert!(Ft8Message::looks_like_callsign("9A1A"));
        assert!(Ft8Message::looks_like_callsign("4X1RF"));

        // OSD false positives should now be rejected
        assert!(!Ft8Message::looks_like_callsign("QY3HUG"));
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

    // -------- has_plausible_payload / is_plausible: SOMETHING gate --------

    fn standard_msg_with_grid(grid: &str) -> Ft8Message {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Reply);
        m.from_callsign = Some("K1ABC".to_string());
        m.to_callsign = Some("W1AW".to_string());
        m.grid_square = Some(grid.to_string());
        m
    }

    fn standard_msg_with_report(rpt: i8) -> Ft8Message {
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Report);
        m.from_callsign = Some("K1ABC".to_string());
        m.to_callsign = Some("W1AW".to_string());
        m.signal_report = Some(rpt);
        m
    }

    #[test]
    fn plausible_reply_with_real_grid_passes() {
        let m = standard_msg_with_grid("FN42");
        assert!(m.is_plausible(), "real grid should pass");
    }

    #[test]
    fn plausible_reply_with_invalid_grid_letters_rejects() {
        // Grids only allow A-R for the first two chars.
        let m = standard_msg_with_grid("ZZ42");
        assert!(!m.is_plausible(), "out-of-range grid letters must reject");
    }

    #[test]
    fn plausible_reply_with_no_grid_passes() {
        // The protocol "empty exchange" code (MAXGRID4+1) produces a
        // Reply with grid = None — a bare callsign-pair ack. Legitimate.
        let mut m = standard_msg_with_grid("FN42");
        m.grid_square = None;
        assert!(
            m.is_plausible(),
            "Reply with no grid is the empty-exchange code"
        );
    }

    #[test]
    fn contest_only_types_rejected() {
        // hb-058 (Batch 31 + 32): contest-only and DXpedition message
        // types are rejected outright. Batch 32 Diagnostic Y revisited
        // the original "DXpedition is highest-value hunt target" stance
        // — on K5ARH's hard-200 corpus 69/69 DXpedition emissions were
        // FP at 0% truth. Reject all four types unconditionally. A
        // future opt-in `accept_dxpedition` config can re-enable when
        // the operator is explicitly hunting an active DXpedition.
        for ty in [
            MessageType::RTTYRoundup,
            MessageType::FieldDay,
            MessageType::Contest,
            MessageType::DXpedition,
        ] {
            let mut m = Ft8Message::default();
            m.message_type = ty;
            m.standard_type = None;
            m.from_callsign = Some("K1ABC".to_string());
            m.to_callsign = Some("W1AW".to_string());
            assert!(
                !m.is_plausible(),
                "{ty:?} must be rejected (hb-058 Batch 32)"
            );
        }
    }

    #[test]
    fn freetext_rejected_unconditionally_batch_32() {
        // Batch 32: FreeText is now rejected by is_plausible. Previously
        // we passed FreeText messages that cleared a structural multi-
        // word/alphabetic-word check, but those messages were 16/16 FP
        // on hard-200. The autonomous-station profile doesn't generate
        // or expect free-text.
        let mut m = Ft8Message::default();
        m.message_type = MessageType::FreeText;
        m.text = Some("CQ DE K1ABC".to_string());
        assert!(!m.is_plausible(), "FreeText must be rejected (Batch 32)");

        let mut m2 = Ft8Message::default();
        m2.message_type = MessageType::FreeText;
        m2.text = Some("TNX 73 K1ABC".to_string());
        assert!(
            !m2.is_plausible(),
            "FreeText must be rejected even with sensible words"
        );
    }

    #[test]
    fn cq_modifier_whitelist() {
        // hb-072: validate the CQ modifier whitelist on real and garbage tokens.
        // Modern-parser CQ messages have to_callsign = None and the calling
        // station in from_callsign; the legacy "to_callsign = CQ" path is a
        // separate early-return in has_plausible_payload (doesn't hit the
        // modifier check).
        let mk = |modifier: Option<&str>| -> Ft8Message {
            let mut m = Ft8Message::default();
            m.message_type = MessageType::Standard;
            m.standard_type = Some(StandardMessageType::Cq);
            m.from_callsign = Some("K1ABC".to_string());
            m.to_callsign = None;
            m.grid_square = Some("FN42".to_string());
            m.special_operation = modifier.map(|s| s.to_string());
            m
        };
        // Real-traffic modifiers must pass.
        for ok in [
            "DX", "NA", "SA", "EU", "AS", "AF", "OC", "QRP", "POTA", "SOTA", "FD", "RU", "TEST",
            "K", "W", "JA", "VK", "NY", "MA", "5", "13", "100",
        ] {
            assert!(
                mk(Some(ok)).is_plausible(),
                "real modifier {ok:?} must pass"
            );
        }
        // Bare CQ (no modifier) must pass.
        assert!(mk(None).is_plausible(), "bare CQ must pass");
        // Garbage tokens typical of CRC-14 collisions must fail.
        for bad in [
            "ABCD",  // 4 letters — too long for short-prefix path
            "12345", // 5 digits — too long for zone path
            "K1ABC", // looks like a callsign, not a modifier
            "A1",    // alpha+digit — neither pure-digit nor pure-alpha
            "?!?",   // garbage
            "VERY",  // 4 letters
        ] {
            assert!(
                !mk(Some(bad)).is_plausible(),
                "garbage modifier {bad:?} must reject"
            );
        }
    }

    #[test]
    fn plausible_report_in_range_passes() {
        for rpt in [-30i8, -10, 0, 5, 20] {
            let m = standard_msg_with_report(rpt);
            assert!(m.is_plausible(), "report {} should pass", rpt);
        }
    }

    #[test]
    fn plausible_report_out_of_range_rejects() {
        // i8 wrap from corrupt unpackgrid lands as e.g. +83 or -89.
        for rpt in [83i8, -89, 100, -100] {
            let m = standard_msg_with_report(rpt);
            assert!(!m.is_plausible(), "out-of-range report {} must reject", rpt);
        }
    }

    #[test]
    fn plausible_token_messages_pass() {
        for stype in [
            StandardMessageType::Rrr,
            StandardMessageType::RR73,
            StandardMessageType::Final73,
        ] {
            let mut m = Ft8Message::default();
            m.message_type = MessageType::Standard;
            m.standard_type = Some(stype);
            m.from_callsign = Some("K1ABC".to_string());
            m.to_callsign = Some("W1AW".to_string());
            assert!(m.is_plausible(), "token msg {:?} should pass", stype);
        }
    }

    #[test]
    fn plausible_standard_with_no_subtype_rejects() {
        // Parser couldn't classify the payload — uninterpretable, must reject.
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = None;
        m.from_callsign = Some("K1ABC".to_string());
        m.to_callsign = Some("W1AW".to_string());
        assert!(
            !m.is_plausible(),
            "Standard message with no recognized subtype must reject"
        );
    }

    #[test]
    fn plausible_cq_passes_even_without_grid() {
        // Real parsed CQ messages have standard_type = Cq, from_callsign set,
        // to_callsign = None (the "CQ" lives in the standard_type, not as a
        // call-shaped string in to_callsign). Some CQ messages omit the grid.
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Cq);
        m.from_callsign = Some("K1ABC".to_string());
        // no to_callsign, no grid_square
        assert!(m.is_plausible(), "bare CQ should pass plausibility");
    }

    #[test]
    fn plausible_cq_with_grid_passes() {
        // The standard form: CQ K1ABC FN42.
        let mut m = Ft8Message::default();
        m.message_type = MessageType::Standard;
        m.standard_type = Some(StandardMessageType::Cq);
        m.from_callsign = Some("K1ABC".to_string());
        m.grid_square = Some("FN42".to_string());
        assert!(m.is_plausible(), "CQ + grid should pass plausibility");
    }
}
