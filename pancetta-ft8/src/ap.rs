//! A Priori (AP) decoding support for FT8.
//!
//! Provides AP context types and LLR injection for AP-enhanced decoding.
//! AP decoding uses known information (own callsign, active QSO partner)
//! to improve decode success at low SNR by injecting high-confidence
//! LLR values at known bit positions in the 77-bit FT8 payload.
//!
//! FT8 77-bit payload layout:
//! - Bits 0-27:  calling station callsign (28 bits)
//! - Bits 28-55: called station callsign (28 bits)
//! - Bits 56-76: report/grid/message content (21 bits)

#![allow(dead_code)]

/// High-confidence LLR magnitude for known AP bits.
const AP_LLR_MAGNITUDE: f32 = 15.0;

/// WSJT-X constants for callsign encoding (same as encoder.rs)
const NTOKENS: u32 = 2_063_592;
const MAX22: u32 = 4_194_304;

// ---------------------------------------------------------------------------
// Standalone pack28 (avoids dependency on transmit-gated encoder module)
// ---------------------------------------------------------------------------

/// Pack a callsign into a 28-bit integer, matching WSJT-X encoding.
/// Returns `(packed_value, suffix_flag)` or `None` on failure.
fn pack28(callsign: &str) -> Option<(u32, u8)> {
    // Special tokens
    match callsign {
        "DE" => return Some((0, 0)),
        "QRZ" => return Some((1, 0)),
        "CQ" => return Some((2, 0)),
        _ => {}
    }

    // CQ with modifier
    if callsign.starts_with("CQ ") && callsign.len() < 8 {
        let modifier = &callsign[3..];
        if let Some(v) = parse_cq_modifier(modifier) {
            return Some((3 + v, 0));
        }
        return None;
    }

    // Detect /R or /P suffix
    let (base, ip) = if callsign.ends_with("/P") || callsign.ends_with("/R") {
        (&callsign[..callsign.len() - 2], 1u8)
    } else {
        (callsign, 0u8)
    };

    let n28 = pack_basecall(base)?;
    Some((NTOKENS + MAX22 + n28, ip))
}

fn parse_cq_modifier(modifier: &str) -> Option<u32> {
    if modifier.is_empty() || modifier.len() > 4 {
        return None;
    }
    let bytes = modifier.as_bytes();
    let all_digits = bytes.iter().all(|b| b.is_ascii_digit());
    let all_letters = bytes.iter().all(|b| b.is_ascii_uppercase());

    if all_digits && modifier.len() == 3 {
        let nnn: u32 = modifier.parse().ok()?;
        Some(nnn)
    } else if all_letters && modifier.len() <= 4 {
        let mut m: u32 = 0;
        for &b in bytes {
            m = 27 * m + ((b - b'A') as u32 + 1);
        }
        Some(1000 + m)
    } else {
        None
    }
}

fn pack_basecall(callsign: &str) -> Option<u32> {
    let length = callsign.len();
    if !(3..=6).contains(&length) {
        return None;
    }
    let bytes = callsign.as_bytes();
    let mut c6 = [b' '; 6];

    if callsign.starts_with("3DA0") && length > 4 && length <= 7 {
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
        c6[0] = b'Q';
        for (i, &b) in bytes[2..].iter().enumerate() {
            if i + 1 < 6 {
                c6[i + 1] = b;
            }
        }
    } else if length >= 3 && bytes[2].is_ascii_digit() && length <= 6 {
        c6[..length].copy_from_slice(&bytes[..length]);
    } else if length >= 2 && bytes[1].is_ascii_digit() && length <= 5 {
        c6[1..1 + length].copy_from_slice(&bytes[..length]);
    } else {
        return None;
    }

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

fn nchar_alphanum_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'0'..=b'9' => Some((c - b'0') as u32 + 1),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 11),
        _ => None,
    }
}

fn nchar_alphanum(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 10),
        _ => None,
    }
}

fn nchar_numeric(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        _ => None,
    }
}

fn nchar_letters_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 1),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Bit helpers
// ---------------------------------------------------------------------------

/// Convert a 28-bit packed value to a bool array, MSB first.
pub fn u32_to_bits_28(value: u32) -> [bool; 28] {
    let mut bits = [false; 28];
    for i in 0..28 {
        bits[i] = (value >> (27 - i)) & 1 == 1;
    }
    bits
}

// ---------------------------------------------------------------------------
// AP types
// ---------------------------------------------------------------------------

/// AP level controlling how much a priori information is injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApLevel {
    /// No AP injection.
    Ap0,
    /// Inject own callsign at bits 28-55 (called station).
    Ap1,
    /// Inject a recent caller's callsign at bits 0-27 (calling station).
    /// The specific caller is selected externally via `inject_ap2_caller`.
    Ap2,
    /// Inject both: active QSO partner at bits 0-27, own call at bits 28-55.
    Ap3,
    /// AP3 + inject i3 type bits (74-76) as 0,0,0 (standard message / RR73).
    Ap4,
}

/// QSO progress within an active AP-tracked contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsoApProgress {
    /// Waiting for a signal report from the other station.
    WaitingForReport,
    /// Waiting for confirmation (RR73 / RRR / 73).
    WaitingForConfirmation,
}

/// Own callsign encoded for AP injection.
#[derive(Debug, Clone)]
pub struct MyCallAp {
    pub callsign: String,
    pub packed_28: u32,
    pub bits: [bool; 28],
}

impl MyCallAp {
    /// Create from a callsign string. Returns `None` if the callsign cannot be
    /// encoded with the standard 28-bit packing.
    pub fn new(callsign: &str) -> Option<Self> {
        let (packed, _ip) = pack28(callsign)?;
        Some(Self {
            callsign: callsign.to_string(),
            packed_28: packed,
            bits: u32_to_bits_28(packed),
        })
    }
}

/// A recently-heard callsign, used for AP2 injection.
#[derive(Debug, Clone)]
pub struct RecentCallAp {
    pub callsign: String,
    pub packed_28: u32,
    pub bits: [bool; 28],
    pub last_snr: f32,
}

impl RecentCallAp {
    /// Create from a callsign and its last observed SNR.
    /// Returns `None` if the callsign cannot be encoded.
    pub fn new(callsign: &str, snr: f32) -> Option<Self> {
        let (packed, _ip) = pack28(callsign)?;
        Some(Self {
            callsign: callsign.to_string(),
            packed_28: packed,
            bits: u32_to_bits_28(packed),
            last_snr: snr,
        })
    }
}

/// Active QSO context for AP3/AP4 injection.
#[derive(Debug, Clone)]
pub struct QsoAp {
    pub their_call: String,
    pub their_packed_28: u32,
    pub their_bits: [bool; 28],
    pub progress: QsoApProgress,
}

impl QsoAp {
    /// Create from the other station's callsign and current QSO progress.
    /// Returns `None` if the callsign cannot be encoded.
    pub fn new(their_call: &str, progress: QsoApProgress) -> Option<Self> {
        let (packed, _ip) = pack28(their_call)?;
        Some(Self {
            their_call: their_call.to_string(),
            their_packed_28: packed,
            their_bits: u32_to_bits_28(packed),
            progress,
        })
    }
}

/// Full AP context holding all known information for AP-enhanced decoding.
#[derive(Debug, Clone, Default)]
pub struct ApContext {
    /// Own station callsign.
    pub my_call: Option<MyCallAp>,
    /// Recently heard callsigns (candidates for AP2).
    pub recent_calls: Vec<RecentCallAp>,
    /// Currently active QSO, if any.
    pub active_qso: Option<QsoAp>,
}

// ---------------------------------------------------------------------------
// LLR injection
// ---------------------------------------------------------------------------

/// Inject a single bit into the LLR array at the given position.
/// `true` bits → negative LLR (bit = 1), `false` → positive LLR (bit = 0).
#[inline]
fn inject_bit(llrs: &mut [f32], pos: usize, bit: bool) {
    if pos < llrs.len() {
        llrs[pos] = if bit { -AP_LLR_MAGNITUDE } else { AP_LLR_MAGNITUDE };
    }
}

/// Inject 28 known bits starting at `offset` in the LLR array.
fn inject_28_bits(llrs: &mut [f32], offset: usize, bits: &[bool; 28]) {
    for (i, &b) in bits.iter().enumerate() {
        inject_bit(llrs, offset + i, b);
    }
}

/// Inject AP LLRs according to the given level and context.
///
/// # Arguments
/// * `llrs` - mutable slice of LLR values (must be at least 77 elements for
///   a full FT8 payload, though the function tolerates shorter slices).
/// * `level` - the AP level to apply.
/// * `context` - the AP context containing known callsigns / QSO state.
pub fn inject_ap_llrs(llrs: &mut [f32], level: ApLevel, context: &ApContext) {
    match level {
        ApLevel::Ap0 => { /* no injection */ }

        ApLevel::Ap1 => {
            // Inject own callsign at bits 28-55 (called station)
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 28, &my_call.bits);
            }
        }

        ApLevel::Ap2 => {
            // AP2 is caller-specific; use inject_ap2_caller() directly.
            // This path is a no-op — the caller chooses which RecentCallAp
            // to inject via inject_ap2_caller().
        }

        ApLevel::Ap3 => {
            // Inject active QSO partner at bits 0-27
            if let Some(ref qso) = context.active_qso {
                inject_28_bits(llrs, 0, &qso.their_bits);
            }
            // Inject own callsign at bits 28-55
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 28, &my_call.bits);
            }
        }

        ApLevel::Ap4 => {
            // Same as AP3 …
            if let Some(ref qso) = context.active_qso {
                inject_28_bits(llrs, 0, &qso.their_bits);
            }
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 28, &my_call.bits);
            }
            // … plus i3 type bits at 74-76 = false, false, false (type 0)
            inject_bit(llrs, 74, false);
            inject_bit(llrs, 75, false);
            inject_bit(llrs, 76, false);
        }
    }
}

/// Inject a specific recent callsign at bits 0-27 (AP2 calling station).
///
/// This is called externally for each candidate caller when attempting AP2
/// decoding passes.
pub fn inject_ap2_caller(llrs: &mut [f32], caller: &RecentCallAp) {
    inject_28_bits(llrs, 0, &caller.bits);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_u32_to_bits_28() {
        // All zeros
        assert_eq!(u32_to_bits_28(0), [false; 28]);

        // MSB only
        let bits = u32_to_bits_28(1 << 27);
        assert!(bits[0]);
        assert!(!bits[1]);
        assert!(!bits[27]);

        // LSB only
        let bits = u32_to_bits_28(1);
        assert!(!bits[0]);
        assert!(bits[27]);

        // All ones (28-bit)
        let bits = u32_to_bits_28(0x0FFF_FFFF);
        assert!(bits.iter().all(|&b| b));
    }

    #[test]
    fn test_my_call_ap_creation() {
        let ap = MyCallAp::new("K5ARH").expect("K5ARH should encode");
        assert_eq!(ap.callsign, "K5ARH");
        // Verify round-trip: bits should reconstruct the packed value
        let mut reconstructed: u32 = 0;
        for (i, &b) in ap.bits.iter().enumerate() {
            if b {
                reconstructed |= 1 << (27 - i);
            }
        }
        assert_eq!(reconstructed, ap.packed_28);

        // Invalid callsign should return None
        assert!(MyCallAp::new("!!!").is_none());
    }

    #[test]
    fn test_inject_ap1() {
        let my_call = MyCallAp::new("K5ARH").expect("K5ARH should encode");
        let ctx = ApContext {
            my_call: Some(my_call.clone()),
            recent_calls: vec![],
            active_qso: None,
        };

        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Ap1, &ctx);

        // Bits 0-27 should be untouched (0.0)
        for i in 0..28 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }

        // Bits 28-55 should be injected with +-15.0
        for i in 28..56 {
            let expected_bit = my_call.bits[i - 28];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} mismatch", i);
        }

        // Bits 56-76 should be untouched
        for i in 56..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn test_inject_ap3() {
        let my_call = MyCallAp::new("K5ARH").expect("K5ARH should encode");
        let qso = QsoAp::new("W1AW", QsoApProgress::WaitingForReport)
            .expect("W1AW should encode");
        let ctx = ApContext {
            my_call: Some(my_call.clone()),
            recent_calls: vec![],
            active_qso: Some(qso.clone()),
        };

        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Ap3, &ctx);

        // Bits 0-27: their callsign (W1AW)
        for i in 0..28 {
            let expected_bit = qso.their_bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (their call) mismatch", i);
        }

        // Bits 28-55: my callsign (K5ARH)
        for i in 28..56 {
            let expected_bit = my_call.bits[i - 28];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (my call) mismatch", i);
        }

        // Bits 56-76 should be untouched
        for i in 56..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }
}
