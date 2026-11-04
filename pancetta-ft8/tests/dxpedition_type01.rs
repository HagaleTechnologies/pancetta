//! PAN-259: WSJT-X DXpedition (i3=0 n3=1) type 0.1 interoperability.
//!
//! The reference payload below was produced by
//! `tests/fixtures/gen_dxpedition_type01.py`, an independent Python
//! re-implementation of WSJT-X `packjt77.f90` `pack77_01` that shares no code
//! with pancetta (provenance in `tests/fixtures/dxpedition_type01.md`). These
//! tests drive only the crate's public API.

use bitvec::prelude::*;
use pancetta_ft8::message::{MessageParser, MessageType};

/// `K1ABC RR73; W9XYZ <KH1/KH7Z> -08`, 77 bits MSB first.
const REFERENCE_BITS: &str =
    "00001001101111011110001101010000110000101001001110111000001100100101011001000";

/// The same 77 bits as 10 payload bytes (3 zero pad bits).
#[cfg(feature = "transmit")]
const REFERENCE_PAYLOAD: [u8; 10] = [0x09, 0xBD, 0xE3, 0x50, 0xC2, 0x93, 0xB8, 0x32, 0x56, 0x40];

fn reference_bits() -> BitVec {
    REFERENCE_BITS.chars().map(|c| c == '1').collect()
}

#[test]
fn dxpedition_reference_frame_decodes_into_both_contacts() {
    let mut parser = MessageParser::new();
    parser.add_callsign("KH1/KH7Z");
    let msg = parser.parse_payload(&reference_bits()).unwrap();

    assert_eq!(msg.message_type, MessageType::DXpedition);
    let contacts = msg.dxpedition.as_ref().expect("both contacts");
    assert_eq!(contacts.rr73_to, "K1ABC", "first call receives RR73");
    assert_eq!(
        contacts.report_to, "W9XYZ",
        "second call receives the report"
    );
    assert_eq!(contacts.report_db, -8);
    assert_eq!(contacts.fox_hash10, 201);
    assert_eq!(contacts.fox_call.as_deref(), Some("KH1/KH7Z"));
    assert_eq!(msg.from_callsign.as_deref(), Some("KH1/KH7Z"));
    assert_eq!(
        msg.text.as_deref(),
        Some("K1ABC RR73; W9XYZ <KH1/KH7Z> -08")
    );
}

#[test]
fn dxpedition_reference_frame_with_unknown_fox_has_no_sender() {
    let parser = MessageParser::new();
    let msg = parser.parse_payload(&reference_bits()).unwrap();
    let contacts = msg.dxpedition.as_ref().unwrap();
    assert_eq!(contacts.fox_call, None);
    assert_eq!(msg.from_callsign, None);
    assert_eq!(msg.text.as_deref(), Some("K1ABC RR73; W9XYZ <...> -08"));
}

#[cfg(feature = "transmit")]
#[test]
fn dxpedition_encoder_matches_independent_reference_payload() {
    use pancetta_ft8::encoder::pack_dxpedition_contacts;
    assert_eq!(
        pack_dxpedition_contacts("K1ABC", "W9XYZ", "KH1/KH7Z", -8).unwrap(),
        REFERENCE_PAYLOAD
    );
    // Odd reports quantize the way WSJT-X does: -7 goes out as -08.
    assert_eq!(
        pack_dxpedition_contacts("K1ABC", "W9XYZ", "KH1/KH7Z", -7).unwrap(),
        REFERENCE_PAYLOAD
    );
    // Reports saturate at the 5-bit field's ends.
    let high = pack_dxpedition_contacts("K1ABC", "W9XYZ", "KH1/KH7Z", 40).unwrap();
    let low = pack_dxpedition_contacts("K1ABC", "W9XYZ", "KH1/KH7Z", -40).unwrap();
    let parser = MessageParser::new();
    let to_bits =
        |p: [u8; 10]| -> BitVec { (0..77).map(|i| p[i / 8] & (0x80 >> (i % 8)) != 0).collect() };
    let high = parser.parse_payload(&to_bits(high)).unwrap();
    let low = parser.parse_payload(&to_bits(low)).unwrap();
    assert_eq!(high.dxpedition.unwrap().report_db, 32);
    assert_eq!(low.dxpedition.unwrap().report_db, -30);
}

#[cfg(feature = "transmit")]
#[test]
fn dxpedition_encoder_refuses_invalid_calls() {
    use pancetta_ft8::encoder::Ft8Encoder;
    let mut encoder = Ft8Encoder::new();
    assert!(encoder
        .encode_dxpedition_contacts("CQ", "W9XYZ", "KH1/KH7Z", -8)
        .is_err());
    assert!(encoder
        .encode_dxpedition_contacts("K1ABC", "W9XYZ", "", -8)
        .is_err());
    let symbols = encoder
        .encode_dxpedition_contacts("K1ABC", "W9XYZ", "KH1/KH7Z", -8)
        .unwrap();
    assert_eq!(symbols.len(), 79);
}
