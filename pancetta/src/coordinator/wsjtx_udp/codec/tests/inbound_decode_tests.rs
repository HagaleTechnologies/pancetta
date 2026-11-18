//! PAN-381: inbound Decode(2) and Status(1) parsing.

use super::{InMsg, OutMsg};

// Captured FT8 decode (same vector as `golden::DECODE`): "~", "JA2EJP N4BP 73",
// SNR -5, DT 0.2 (float32-widened), DF 1302, new=true, Id "WSJT-X".
const DECODE: &str = "adbccbda000000020000000200000006\
                      57534a542d58010259baf8fffffffb3fc99999a000000000000516000000017e\
                      0000000e4a4132454a50204e34425020373300 00";

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn decode_out(message: &str) -> OutMsg {
    OutMsg::Decode {
        new: true,
        time_ms: 39_435_000,
        snr: -5,
        delta_time: 0.2,
        delta_freq: 1302,
        mode: "~".into(),
        message: message.into(),
        low_confidence: true,
        off_air: true,
    }
}

/// Header (magic, schema, type, utf8 id) length for `id`.
fn header_len(id: &str) -> usize {
    4 + 4 + 4 + 4 + id.len()
}

#[test]
fn captured_decode_parses_every_field() {
    let (id, msg) = InMsg::decode(&hex(DECODE)).expect("captured decode parses");
    assert_eq!(id, "WSJT-X");
    assert_eq!(
        msg,
        InMsg::Decode {
            new: true,
            time_ms: 39_435_000,
            snr: -5,
            delta_time: f64::from_bits(0x3fc9_9999_a000_0000),
            delta_freq: 1302,
            mode: "~".into(),
            message: "JA2EJP N4BP 73".into(),
            low_confidence: false,
            off_air: false,
        }
    );
}

#[test]
fn ft4_decode_parses_plus_glyph() {
    // The `ft4_decode_encodes_byte_exact` vector.
    let bytes = hex("adbccbda00000002000000020000001157534a542d58202d2070616e63657474610102b32c95fffffff63fc999999999999a00000516000000012b0000000e435120444c31414243204a4f36320000");
    let (id, msg) = InMsg::decode(&bytes).expect("ft4 decode parses");
    assert_eq!(id, "WSJT-X - pancetta");
    assert_eq!(
        msg,
        InMsg::Decode {
            new: true,
            time_ms: 45_296_789,
            snr: -10,
            delta_time: 0.2,
            delta_freq: 1302,
            mode: "+".into(),
            message: "CQ DL1ABC JO62".into(),
            low_confidence: false,
            off_air: false,
        }
    );
}

#[test]
fn decode_truncated_after_message_defaults_flags() {
    let mut bytes = decode_out("CQ K1ABC FN42").encode("WSJT-X");
    bytes.truncate(bytes.len() - 2); // drop LowConfidence and OffAir
    let (_, msg) = InMsg::decode(&bytes).expect("truncated decode parses");
    let InMsg::Decode {
        message,
        low_confidence,
        off_air,
        ..
    } = msg
    else {
        panic!("expected Decode, got {msg:?}");
    };
    assert_eq!(message, "CQ K1ABC FN42");
    assert!(!low_confidence);
    assert!(!off_air);
}

#[test]
fn header_only_decode_has_empty_message() {
    let mut bytes = decode_out("CQ K1ABC FN42").encode("WSJT-X");
    bytes.truncate(header_len("WSJT-X"));
    let (id, msg) = InMsg::decode(&bytes).expect("header-only decode parses");
    assert_eq!(id, "WSJT-X");
    assert_eq!(
        msg,
        InMsg::Decode {
            new: false,
            time_ms: 0,
            snr: 0,
            delta_time: 0.0,
            delta_freq: 0,
            mode: String::new(),
            message: String::new(),
            low_confidence: false,
            off_air: false,
        }
    );
}

#[test]
fn decode_with_bad_magic_or_schema_is_rejected() {
    let mut bad_magic = hex(DECODE);
    bad_magic[0] = 0x00;
    assert!(InMsg::decode(&bad_magic).is_none());

    let mut schema4 = hex(DECODE);
    schema4[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x04]);
    assert!(InMsg::decode(&schema4).is_none());
}

#[test]
fn status_parses_dial_and_mode() {
    // The `active_qso_status_encodes_byte_exact` bytes.
    let bytes = hex("adbccbda00000002000000010000001157534a542d58202d2070616e63657474610000000000d6c0900000000346543800000006444c31414243000000032d31300000000346543801000000000516000005dc000000054b3541524800000004454d3132000000044a4f363200000000000000ffffffff0000000f0000000744656661756c7400000011444c31414243204b3541524820522d3130");
    let (id, msg) = InMsg::decode(&bytes).expect("status parses");
    assert_eq!(id, "WSJT-X - pancetta");
    assert_eq!(
        msg,
        InMsg::Status {
            dial_frequency: 14_074_000,
            mode: "FT8".into(),
        }
    );
}

#[test]
fn status_truncated_after_dial_has_empty_mode() {
    let mut w = super::Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(1);
    w.utf8("WSJT-X");
    w.u64(7_074_000);
    let (_, msg) = InMsg::decode(&w.into_vec()).expect("short status parses");
    assert_eq!(
        msg,
        InMsg::Status {
            dial_frequency: 7_074_000,
            mode: String::new(),
        }
    );
}
