//! PAN-381 AC4/AC5: the pure ingest core, driven with test-controlled
//! `Instant`s.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use pancetta_config::OperatingMode;
use pancetta_core::slot::SlotParity;

use super::super::codec::{OutMsg, Writer};
use super::{DecodeIngest, Dropped, IngestLimits, Ingested};
use crate::coordinator::ft8::sanitize_decoded_field;
use crate::coordinator::pipeline::{DecodeOrigin, RelayedDecode};

const OWN_ID: &str = "WSJT-X - pancetta";
const WSJTX: &str = "WSJT-X";

fn local() -> SocketAddr {
    "127.0.0.1:50123".parse().unwrap()
}

fn ingest() -> DecodeIngest {
    DecodeIngest::new(OWN_ID, IngestLimits::default())
}

/// A Status carrying only the fields the listener reads.
fn status(id: &str, dial: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(1);
    w.utf8(id);
    w.u64(dial);
    w.utf8("FT8");
    w.into_vec()
}

/// A header-only (plus optional bool) request-type datagram.
fn header(kind: u32, id: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(kind);
    w.utf8(id);
    w.into_vec()
}

#[derive(Clone)]
struct D {
    new: bool,
    time_ms: u32,
    snr: i32,
    delta_time: f64,
    delta_freq: u32,
    mode: &'static str,
    message: String,
    low_confidence: bool,
    off_air: bool,
}

impl D {
    fn live(message: &str) -> Self {
        D {
            new: true,
            time_ms: 39_435_000,
            snr: -5,
            delta_time: 0.2,
            delta_freq: 1302,
            mode: "~",
            message: message.to_string(),
            low_confidence: false,
            off_air: false,
        }
    }

    fn encode(&self, id: &str) -> Vec<u8> {
        OutMsg::Decode {
            new: self.new,
            time_ms: self.time_ms,
            snr: self.snr,
            delta_time: self.delta_time,
            delta_freq: self.delta_freq,
            mode: self.mode.to_string(),
            message: self.message.clone(),
            low_confidence: self.low_confidence,
            off_air: self.off_air,
        }
        .encode(id)
    }
}

fn decoded(i: Ingested) -> RelayedDecode {
    match i {
        Ingested::Decode(d) => d,
        other => panic!("expected a decode, got {other:?}"),
    }
}

fn dropped(i: Ingested) -> Dropped {
    match i {
        Ingested::Dropped(d) => d,
        other => panic!("expected a drop, got {other:?}"),
    }
}

/// An ingest that has already heard WSJT-X's Status at 14.074 MHz.
fn ready(now: Instant) -> DecodeIngest {
    let mut ing = ingest();
    assert!(matches!(
        ing.accept(&status(WSJTX, 14_074_000), local(), now),
        Ingested::SenderUpdated
    ));
    ing
}

#[test]
fn live_decode_after_status_maps_every_field() {
    let now = Instant::now();
    let mut ing = ready(now);
    let d = decoded(ing.accept(&D::live("CQ DX JA1ABC PM95").encode(WSJTX), local(), now));
    assert_eq!(
        d.origin,
        DecodeOrigin::WsjtxUdp {
            mode: OperatingMode::Ft8
        }
    );
    assert_eq!(d.message.text, "CQ DX JA1ABC PM95");
    assert_eq!(d.message.snr_db, -5.0);
    assert_eq!(d.message.frequency_offset, 1302.0);
    assert_eq!(d.message.time_offset, 0.2);
    // 39_435_000 ms / 15_000 = 2629 → odd.
    assert_eq!(d.message.slot_parity, Some(SlotParity::Odd));
    assert_eq!(d.message.captured_dial_hz, Some(14_074_000));
    assert_eq!(d.dial_hz, 14_074_000);
    assert_eq!(d.message.message.from_callsign.as_deref(), Some("JA1ABC"));
    assert_eq!(d.message.message.grid_square.as_deref(), Some("PM95"));
}

#[test]
fn ft4_decode_uses_seven_and_a_half_second_parity() {
    let now = Instant::now();
    let mut ing = ready(now);
    let mut ft4 = D::live("CQ DL1ABC JO62");
    ft4.mode = "+";
    ft4.time_ms = 7_500;
    let d = decoded(ing.accept(&ft4.encode(WSJTX), local(), now));
    assert_eq!(
        d.origin,
        DecodeOrigin::WsjtxUdp {
            mode: OperatingMode::Ft4
        }
    );
    assert_eq!(d.message.slot_parity, Some(SlotParity::Odd));
    ft4.time_ms = 15_000;
    let d = decoded(ing.accept(&ft4.encode(WSJTX), local(), now));
    assert_eq!(d.message.slot_parity, Some(SlotParity::Even));
    // WSJT-X renders the 7.5 s and 22.5 s FT4 slot starts at whole-second
    // resolution (`000007`, `000022`); they are still odd slots.
    for whole_second in [7_000, 22_000, 86_392_000] {
        ft4.time_ms = whole_second;
        let d = decoded(ing.accept(&ft4.encode(WSJTX), local(), now));
        assert_eq!(
            d.message.slot_parity,
            Some(SlotParity::Odd),
            "FT4 time_ms {whole_second}"
        );
    }

    // The same 15 s mark is odd for FT8.
    let mut ft8 = D::live("CQ DL1ABC JO62");
    ft8.time_ms = 15_000;
    let d = decoded(ing.accept(&ft8.encode(WSJTX), local(), now));
    assert_eq!(d.message.slot_parity, Some(SlotParity::Odd));
}

#[test]
fn decode_text_is_sanitized_like_native() {
    let now = Instant::now();
    let mut ing = ready(now);
    let raw = format!("  CQ K1ABC\u{1b}[31m\u{7} FN42 {}  ", "X".repeat(80));
    let d = decoded(ing.accept(&D::live(&raw).encode(WSJTX), local(), now));
    let expected = sanitize_decoded_field(raw.trim()).trim().to_string();
    assert_eq!(d.message.text, expected);
    assert_eq!(d.message.text.chars().count(), 64);
    assert!(!d
        .message
        .text
        .chars()
        .any(|c| c == '\u{1b}' || c == '\u{7f}' || c < '\u{20}'));
    for field in [
        &d.message.message.from_callsign,
        &d.message.message.to_callsign,
        &d.message.message.grid_square,
        &d.message.message.text,
    ]
    .into_iter()
    .flatten()
    {
        assert!(field.chars().count() <= 64);
        assert!(!field.chars().any(|c| c == '\u{1b}' || c < '\u{20}'));
    }
}

#[test]
fn free_text_keeps_text_but_no_callsigns() {
    let now = Instant::now();
    let mut ing = ready(now);
    for text in ["HELLO WORLD TEST", "TNX BOB 73 GL"] {
        let d = decoded(ing.accept(&D::live(text).encode(WSJTX), local(), now));
        assert_eq!(d.message.text, text);
        assert_eq!(d.message.message.from_callsign, None, "{text}");
        assert_eq!(d.message.message.to_callsign, None, "{text}");
        assert_eq!(d.message.message.grid_square, None, "{text}");
    }
}

#[test]
fn new_status_dial_rebands_later_decodes() {
    let now = Instant::now();
    let mut ing = ready(now);
    let d = decoded(ing.accept(&D::live("CQ K1ABC FN42").encode(WSJTX), local(), now));
    assert_eq!(d.dial_hz, 14_074_000);
    ing.accept(&status(WSJTX, 7_074_000), local(), now);
    let d = decoded(ing.accept(&D::live("CQ K1ABC FN42").encode(WSJTX), local(), now));
    assert_eq!(d.dial_hz, 7_074_000);
    assert_eq!(d.message.captured_dial_hz, Some(7_074_000));
}

#[test]
fn non_loopback_source_is_dropped() {
    let now = Instant::now();
    let remote: SocketAddr = "192.0.2.1:2237".parse().unwrap();
    let mut ing = ingest();
    assert_eq!(
        dropped(ing.accept(&status(WSJTX, 14_074_000), remote, now)),
        Dropped::NonLoopback
    );
    let mut ing = ready(now);
    assert_eq!(
        dropped(ing.accept(&D::live("CQ K1ABC FN42").encode(WSJTX), remote, now)),
        Dropped::NonLoopback
    );
    // IPv6 loopback and IPv4-mapped loopback are local.
    let v6: SocketAddr = "[::1]:50123".parse().unwrap();
    decoded(ing.accept(&D::live("CQ K1ABC FN42").encode(WSJTX), v6, now));
    let mapped: SocketAddr = "[::ffff:127.0.0.1]:50123".parse().unwrap();
    decoded(ing.accept(&D::live("CQ K1ABC FN42").encode(WSJTX), mapped, now));
}

#[test]
fn undecodable_datagram_is_malformed() {
    let now = Instant::now();
    let mut ing = ready(now);
    assert_eq!(
        dropped(ing.accept(b"hello", local(), now)),
        Dropped::Malformed
    );
    assert_eq!(dropped(ing.accept(&[], local(), now)), Dropped::Malformed);
    let mut schema9 = D::live("CQ K1ABC FN42").encode(WSJTX);
    schema9[4..8].copy_from_slice(&9u32.to_be_bytes());
    assert_eq!(
        dropped(ing.accept(&schema9, local(), now)),
        Dropped::Malformed
    );
}

#[test]
fn own_instance_id_is_dropped_as_echo() {
    let now = Instant::now();
    let mut ing = ingest();
    assert_eq!(
        dropped(ing.accept(&status(OWN_ID, 14_074_000), local(), now)),
        Dropped::OwnEcho
    );
    assert_eq!(
        dropped(ing.accept(&D::live("CQ K1ABC FN42").encode(OWN_ID), local(), now)),
        Dropped::OwnEcho
    );
}

#[test]
fn replayed_off_air_and_low_confidence_are_filtered() {
    let now = Instant::now();
    let mut ing = ready(now);
    let mut replay = D::live("CQ K1ABC FN42");
    replay.new = false;
    let mut wav = D::live("CQ K1ABC FN42");
    wav.off_air = true;
    let mut low = D::live("CQ K1ABC FN42");
    low.low_confidence = true;
    for d in [replay, wav, low] {
        assert_eq!(
            dropped(ing.accept(&d.encode(WSJTX), local(), now)),
            Dropped::Filtered
        );
    }
}

#[test]
fn unknown_mode_glyph_is_filtered() {
    let now = Instant::now();
    let mut ing = ready(now);
    for glyph in ["#", "$", "@", "", "FT8", " ~"] {
        let mut d = D::live("CQ K1ABC FN42");
        d.mode = glyph;
        assert_eq!(
            dropped(ing.accept(&d.encode(WSJTX), local(), now)),
            Dropped::Filtered,
            "glyph {glyph:?}"
        );
    }
}

#[test]
fn out_of_range_fields_are_malformed() {
    let now = Instant::now();
    let mut ing = ready(now);
    let base = D::live("CQ K1ABC FN42");
    let mut bad = Vec::new();
    let mut d = base.clone();
    d.time_ms = 86_400_000;
    bad.push(d);
    for snr in [61, -61, i32::MAX] {
        let mut d = base.clone();
        d.snr = snr;
        bad.push(d);
    }
    for dt in [f64::NAN, f64::INFINITY, 30.5, -30.5] {
        let mut d = base.clone();
        d.delta_time = dt;
        bad.push(d);
    }
    let mut d = base.clone();
    d.delta_freq = 6_001;
    bad.push(d);
    for d in bad {
        assert_eq!(
            dropped(ing.accept(&d.encode(WSJTX), local(), now)),
            Dropped::Malformed
        );
    }

    // The edges are accepted.
    let mut d = base.clone();
    d.time_ms = 86_399_999;
    d.snr = 60;
    d.delta_time = -30.0;
    d.delta_freq = 6_000;
    decoded(ing.accept(&d.encode(WSJTX), local(), now));
    d.snr = -60;
    d.delta_time = 30.0;
    decoded(ing.accept(&d.encode(WSJTX), local(), now));
}

#[test]
fn empty_message_is_malformed() {
    let now = Instant::now();
    let mut ing = ready(now);
    for text in ["", "   ", "\u{1b}\u{7}\u{7f}", " \u{1b} "] {
        assert_eq!(
            dropped(ing.accept(&D::live(text).encode(WSJTX), local(), now)),
            Dropped::Malformed,
            "{text:?}"
        );
    }
}

#[test]
fn decode_before_status_waits_for_dial() {
    let now = Instant::now();
    let mut ing = ingest();
    let bytes = D::live("CQ K1ABC FN42").encode(WSJTX);
    assert_eq!(dropped(ing.accept(&bytes, local(), now)), Dropped::NoStatus);
    ing.accept(&status(WSJTX, 14_074_000), local(), now);
    decoded(ing.accept(&bytes, local(), now));
    // A second instance with no Status of its own is not banded by the first.
    assert_eq!(
        dropped(ing.accept(&D::live("CQ K1ABC FN42").encode("WSJT-X - 2"), local(), now)),
        Dropped::NoStatus
    );
}

#[test]
fn zero_dial_status_and_close_forget_the_sender() {
    let now = Instant::now();
    let bytes = D::live("CQ K1ABC FN42").encode(WSJTX);

    let mut ing = ingest();
    ing.accept(&status(WSJTX, 0), local(), now);
    assert_eq!(dropped(ing.accept(&bytes, local(), now)), Dropped::NoStatus);

    let mut ing = ready(now);
    decoded(ing.accept(&bytes, local(), now));
    ing.accept(&status(WSJTX, 0), local(), now);
    assert_eq!(dropped(ing.accept(&bytes, local(), now)), Dropped::NoStatus);

    ing.accept(&status(WSJTX, 14_074_000), local(), now);
    decoded(ing.accept(&bytes, local(), now));
    assert!(matches!(
        ing.accept(&header(6, WSJTX), local(), now),
        Ingested::SenderUpdated
    ));
    assert_eq!(dropped(ing.accept(&bytes, local(), now)), Dropped::NoStatus);
}

#[test]
fn intake_budget_caps_each_window() {
    let t0 = Instant::now();
    let mut ing = ready(t0);
    let bytes = D::live("CQ K1ABC FN42").encode(WSJTX);
    for i in 0..250 {
        let at = t0 + Duration::from_millis(i);
        decoded(ing.accept(&bytes, local(), at));
    }
    assert_eq!(
        dropped(ing.accept(&bytes, local(), t0 + Duration::from_millis(14_999))),
        Dropped::OverBudget
    );
    // The next window accepts again.
    decoded(ing.accept(&bytes, local(), t0 + Duration::from_secs(15)));
    // Drops for other reasons do not consume the budget.
    let mut ing = ready(t0);
    let mut replay = D::live("CQ K1ABC FN42");
    replay.new = false;
    for _ in 0..300 {
        ing.accept(&replay.encode(WSJTX), local(), t0);
    }
    decoded(ing.accept(&bytes, local(), t0));
}

#[test]
fn requests_on_the_listener_are_ignored() {
    let now = Instant::now();
    let mut ing = ready(now);
    // Heartbeat, Clear, Replay, HaltTx: header-only is enough to parse.
    for kind in [0u32, 3, 7, 8] {
        assert!(
            matches!(
                ing.accept(&header(kind, WSJTX), local(), now),
                Ingested::Ignored
            ),
            "type {kind}"
        );
    }
    // Reply (type 4) with a full body.
    let mut w = Writer::new();
    w.u32(0xadbc_cbda);
    w.u32(2);
    w.u32(4);
    w.utf8(WSJTX);
    w.u32(39_435_000);
    w.u32((-5i32) as u32);
    w.u64(0.2f64.to_bits());
    w.u32(1302);
    w.utf8("~");
    w.utf8("CQ K1ABC FN42");
    w.bool(false);
    w.u8(0);
    assert!(matches!(
        ing.accept(&w.into_vec(), local(), now),
        Ingested::Ignored
    ));
    // An unmodelled type.
    assert!(matches!(
        ing.accept(&header(9, WSJTX), local(), now),
        Ingested::Ignored
    ));
}

#[test]
fn sender_table_is_capped() {
    let t0 = Instant::now();
    let mut ing = ingest();
    let at = |i: u64| t0 + Duration::from_millis(i);
    for i in 0..8u64 {
        ing.accept(&status(&format!("S{i}"), 14_074_000 + i), local(), at(i));
    }
    // Refresh S0, so S1 is now the least recently updated.
    ing.accept(&status("S0", 14_074_000), local(), at(8));
    ing.accept(&status("S8", 7_074_000), local(), at(9));

    let decode_from = |ing: &mut DecodeIngest, id: &str| {
        ing.accept(&D::live("CQ K1ABC FN42").encode(id), local(), at(10))
    };
    assert_eq!(dropped(decode_from(&mut ing, "S1")), Dropped::NoStatus);
    for id in ["S0", "S2", "S3", "S4", "S5", "S6", "S7"] {
        decoded(decode_from(&mut ing, id));
    }
    assert_eq!(decoded(decode_from(&mut ing, "S8")).dial_hz, 7_074_000);
}
