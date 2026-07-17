//! WSJT-X UDP wire codec: byte-level `Writer`/`Reader` primitives plus the
//! `OutMsg` (pancetta → companion apps) and `InMsg` (companion apps →
//! pancetta) message enums.
//!
//! Byte-level rules (magic, framing, primitive encodings, field lists) are
//! normative per `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md`
//! §3 (primitives) and §4 (per-message field tables) — this module implements
//! those tables exactly, with no fields added, reordered, or improvised.
//!
//! This module is a pure codec: it has no socket, no runtime wiring, and no
//! knowledge of pancetta's coordinator state. That comes in later tasks.

/// Datagram magic number, first 4 bytes of every WSJT-X UDP message
/// (`docs/.../protocol-notes.md` §2).
const MAGIC: u32 = 0xadbc_cbda;

/// Schema value pancetta emits on every outbound datagram. WSJT-X 2.x/3.x
/// all send schema 2; schema 3 is negotiable via Heartbeat `MaxSchema` but
/// changes only field *encodings* (never observed in the wild), not layout.
const SCHEMA_OUT: u32 = 2;

/// Inbound schema values pancetta accepts (parsed identically per §7).
const SCHEMA_IN_MIN: u32 = 2;
const SCHEMA_IN_MAX: u32 = 3;

// Message type IDs (§2 table) — only the subset OutMsg/InMsg implement.
const TYPE_HEARTBEAT: u32 = 0;
const TYPE_STATUS: u32 = 1;
const TYPE_DECODE: u32 = 2;
const TYPE_CLEAR: u32 = 3;
const TYPE_REPLY: u32 = 4;
const TYPE_QSO_LOGGED: u32 = 5;
const TYPE_CLOSE: u32 = 6;
const TYPE_REPLAY: u32 = 7;
const TYPE_HALT_TX: u32 = 8;
const TYPE_LOGGED_ADIF: u32 = 12;

/// A `QDateTime` value as WSJT-X puts it on the wire: `(Julian Day Number,
/// milliseconds since midnight UTC)`. Timespec is always UTC (`1`) on
/// output; see [`Writer::qdatetime_utc`] / [`Reader::read_qdatetime_utc`]
/// and protocol notes §3.
pub(crate) type QDateTimeUtc = (u64, u32);

/// Append-only byte writer implementing the Qt `QDataStream` big-endian
/// primitive encodings used by every WSJT-X UDP message (protocol notes
/// §3). Shared by every `OutMsg` variant's `encode` arm — no per-message
/// duplication of byte-packing logic.
pub(crate) struct Writer(Vec<u8>);

impl Writer {
    pub(crate) fn new() -> Self {
        Writer(Vec::new())
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.0
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.0.push(u8::from(v));
    }

    /// utf8 string: u32 BE byte-length then the UTF-8 bytes (QByteArray, not
    /// QString/UTF-16). Always emits the explicit `0x00000000` empty-length
    /// form, never the null sentinel — both are accepted by every receiver
    /// (protocol notes §3).
    pub(crate) fn utf8(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }

    /// Null-string sentinel: length `0xFFFFFFFF`, no bytes follow. Distinct
    /// from `utf8("")` only on the wire — both decode to `""`.
    pub(crate) fn utf8_null(&mut self) {
        self.u32(u32::MAX);
    }

    /// `QTime`: milliseconds since midnight UTC, same wire shape as a plain
    /// u32 but named for call-site clarity.
    pub(crate) fn qtime_ms(&mut self, ms: u32) {
        self.u32(ms);
    }

    /// `QDateTime`: JDN (u64 BE) + ms-since-midnight (u32 BE) + timespec
    /// (u8). Pancetta only ever emits timespec `1` (UTC) — never `2`/`3`
    /// (protocol notes §3).
    pub(crate) fn qdatetime_utc(&mut self, jdn: u64, ms: u32) {
        self.u64(jdn);
        self.u32(ms);
        self.u8(1);
    }
}

/// Sequential byte reader implementing the inverse of [`Writer`]. Every
/// `read_*` method returns `None` when the buffer is exhausted before the
/// field could be read in full — this is the protocol's absent-field
/// versioning rule (protocol notes §3: "end-of-datagram before a field =
/// field absent"), not a decode error. Trailing bytes after the last field
/// a caller reads are simply never consumed.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.remaining() < n {
            return None;
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }

    pub(crate) fn read_u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub(crate) fn read_u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }

    pub(crate) fn read_i32(&mut self) -> Option<i32> {
        self.take(4)
            .map(|b| i32::from_be_bytes(b.try_into().unwrap()))
    }

    pub(crate) fn read_u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }

    pub(crate) fn read_f64(&mut self) -> Option<f64> {
        self.take(8)
            .map(|b| f64::from_be_bytes(b.try_into().unwrap()))
    }

    pub(crate) fn read_bool(&mut self) -> Option<bool> {
        self.read_u8().map(|b| b != 0)
    }

    /// utf8 string. Both the null sentinel (`0xFFFFFFFF` length) and the
    /// explicit empty form (`0x00000000` length) decode to `""`.
    pub(crate) fn read_utf8(&mut self) -> Option<String> {
        let len = self.read_u32()?;
        if len == u32::MAX {
            return Some(String::new());
        }
        let bytes = self.take(len as usize)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    pub(crate) fn read_qtime_ms(&mut self) -> Option<u32> {
        self.read_u32()
    }

    /// `QDateTime`. Tolerates timespec `2` (offset-from-UTC) by additionally
    /// consuming the trailing i32 offset field, per protocol notes §3;
    /// pancetta never emits it but py-wsjtx confirms peers do.
    pub(crate) fn read_qdatetime_utc(&mut self) -> Option<QDateTimeUtc> {
        let jdn = self.read_u64()?;
        let ms = self.read_u32()?;
        let timespec = self.read_u8()?;
        if timespec == 2 {
            self.read_i32()?;
        }
        Some((jdn, ms))
    }
}

/// Messages pancetta emits (the WSJT-X role side of the protocol).
/// Field lists are exactly protocol notes §4 OUT tables, in order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutMsg {
    /// Type 0, sent every 15s.
    Heartbeat {
        max_schema: u32,
        version: String,
        revision: String,
    },
    /// Type 1, sent on state change; all 21 fields per §4.
    Status {
        dial_frequency: u64,
        mode: String,
        dx_call: String,
        report: String,
        tx_mode: String,
        tx_enabled: bool,
        transmitting: bool,
        decoding: bool,
        rx_df: u32,
        tx_df: u32,
        de_call: String,
        de_grid: String,
        dx_grid: String,
        tx_watchdog: bool,
        sub_mode: String,
        fast_mode: bool,
        special_operation_mode: u8,
        frequency_tolerance: u32,
        tr_period: u32,
        configuration_name: String,
        tx_message: String,
    },
    /// Type 2, per decode.
    Decode {
        new: bool,
        time_ms: u32,
        snr: i32,
        delta_time: f64,
        delta_freq: u32,
        mode: String,
        message: String,
        low_confidence: bool,
        off_air: bool,
    },
    /// Type 3, header only.
    Clear,
    /// Type 5, when a QSO is logged; all 17 fields per §4.
    QsoLogged {
        date_time_off: QDateTimeUtc,
        dx_call: String,
        dx_grid: String,
        tx_frequency: u64,
        mode: String,
        report_sent: String,
        report_received: String,
        tx_power: String,
        comments: String,
        name: String,
        date_time_on: QDateTimeUtc,
        operator_call: String,
        my_call: String,
        my_grid: String,
        exchange_sent: String,
        exchange_received: String,
        adif_propagation_mode: String,
    },
    /// Type 6, header only.
    Close,
    /// Type 12, a complete ADIF fragment.
    LoggedAdif { adif: String },
}

impl OutMsg {
    fn type_id(&self) -> u32 {
        match self {
            OutMsg::Heartbeat { .. } => TYPE_HEARTBEAT,
            OutMsg::Status { .. } => TYPE_STATUS,
            OutMsg::Decode { .. } => TYPE_DECODE,
            OutMsg::Clear => TYPE_CLEAR,
            OutMsg::QsoLogged { .. } => TYPE_QSO_LOGGED,
            OutMsg::Close => TYPE_CLOSE,
            OutMsg::LoggedAdif { .. } => TYPE_LOGGED_ADIF,
        }
    }

    /// Encode the full datagram: header (magic, schema 2, type, `id`) then
    /// the type-specific fields in protocol-notes §4 order.
    pub(crate) fn encode(&self, id: &str) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(SCHEMA_OUT);
        w.u32(self.type_id());
        w.utf8(id);
        match self {
            OutMsg::Heartbeat {
                max_schema,
                version,
                revision,
            } => {
                w.u32(*max_schema);
                w.utf8(version);
                w.utf8(revision);
            }
            OutMsg::Status {
                dial_frequency,
                mode,
                dx_call,
                report,
                tx_mode,
                tx_enabled,
                transmitting,
                decoding,
                rx_df,
                tx_df,
                de_call,
                de_grid,
                dx_grid,
                tx_watchdog,
                sub_mode,
                fast_mode,
                special_operation_mode,
                frequency_tolerance,
                tr_period,
                configuration_name,
                tx_message,
            } => {
                w.u64(*dial_frequency);
                w.utf8(mode);
                w.utf8(dx_call);
                w.utf8(report);
                w.utf8(tx_mode);
                w.bool(*tx_enabled);
                w.bool(*transmitting);
                w.bool(*decoding);
                w.u32(*rx_df);
                w.u32(*tx_df);
                w.utf8(de_call);
                w.utf8(de_grid);
                w.utf8(dx_grid);
                w.bool(*tx_watchdog);
                w.utf8(sub_mode);
                w.bool(*fast_mode);
                w.u8(*special_operation_mode);
                w.u32(*frequency_tolerance);
                w.u32(*tr_period);
                w.utf8(configuration_name);
                w.utf8(tx_message);
            }
            OutMsg::Decode {
                new,
                time_ms,
                snr,
                delta_time,
                delta_freq,
                mode,
                message,
                low_confidence,
                off_air,
            } => {
                w.bool(*new);
                w.qtime_ms(*time_ms);
                w.i32(*snr);
                w.f64(*delta_time);
                w.u32(*delta_freq);
                w.utf8(mode);
                w.utf8(message);
                w.bool(*low_confidence);
                w.bool(*off_air);
            }
            OutMsg::Clear => {}
            OutMsg::QsoLogged {
                date_time_off,
                dx_call,
                dx_grid,
                tx_frequency,
                mode,
                report_sent,
                report_received,
                tx_power,
                comments,
                name,
                date_time_on,
                operator_call,
                my_call,
                my_grid,
                exchange_sent,
                exchange_received,
                adif_propagation_mode,
            } => {
                w.qdatetime_utc(date_time_off.0, date_time_off.1);
                w.utf8(dx_call);
                w.utf8(dx_grid);
                w.u64(*tx_frequency);
                w.utf8(mode);
                w.utf8(report_sent);
                w.utf8(report_received);
                w.utf8(tx_power);
                w.utf8(comments);
                w.utf8(name);
                w.qdatetime_utc(date_time_on.0, date_time_on.1);
                w.utf8(operator_call);
                w.utf8(my_call);
                w.utf8(my_grid);
                w.utf8(exchange_sent);
                w.utf8(exchange_received);
                w.utf8(adif_propagation_mode);
            }
            OutMsg::Close => {}
            OutMsg::LoggedAdif { adif } => {
                w.utf8(adif);
            }
        }
        w.into_vec()
    }
}

/// Messages pancetta consumes (sent by companion apps such as GridTracker).
/// Field lists are exactly protocol notes §4 IN tables, in order.
/// `Other(type)` is the catch-all for every inbound type this codec does
/// not (yet) act on — bytes beyond the header are ignored, matching the
/// "ignore unknown message types" framing rule (§2).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InMsg {
    /// Type 0. Liveness/schema info only; GridTracker never sends one.
    Heartbeat {
        max_schema: u32,
        version: String,
        revision: String,
    },
    /// Type 3. `window` is `None` when the field is absent (older peer).
    Clear { window: Option<u8> },
    /// Type 4, the remote-initiation ("double-click") message; 8 fields.
    Reply {
        time_ms: u32,
        snr: i32,
        delta_time: f64,
        delta_freq: u32,
        mode: String,
        message: String,
        low_confidence: bool,
        modifiers: u8,
    },
    /// Type 6, header only.
    Close,
    /// Type 7, header only.
    Replay,
    /// Type 8, header only + `AutoTxOnly`.
    HaltTx { auto_tx_only: bool },
    /// Any message type this codec does not model a variant for.
    Other(u32),
}

impl InMsg {
    /// Decode one datagram. Returns `None` if the magic doesn't match or
    /// the header itself (schema, type, id) can't be read — those aren't
    /// recoverable "absent field" cases, they mean this isn't a WSJT-X UDP
    /// datagram at all. Beyond the header, per-field absence (truncation)
    /// degrades to that field's default rather than failing the whole
    /// decode, and trailing bytes past the last field read are ignored —
    /// both are the protocol's actual versioning mechanism (§3).
    pub(crate) fn decode(buf: &[u8]) -> Option<(String, InMsg)> {
        let mut r = Reader::new(buf);
        let magic = r.read_u32()?;
        if magic != MAGIC {
            return None;
        }
        let schema = r.read_u32()?;
        if !(SCHEMA_IN_MIN..=SCHEMA_IN_MAX).contains(&schema) {
            return None;
        }
        let msg_type = r.read_u32()?;
        let id = r.read_utf8()?;

        let msg = match msg_type {
            TYPE_HEARTBEAT => InMsg::Heartbeat {
                max_schema: r.read_u32().unwrap_or_default(),
                version: r.read_utf8().unwrap_or_default(),
                revision: r.read_utf8().unwrap_or_default(),
            },
            TYPE_CLEAR => InMsg::Clear {
                window: r.read_u8(),
            },
            TYPE_REPLY => InMsg::Reply {
                time_ms: r.read_qtime_ms().unwrap_or_default(),
                snr: r.read_i32().unwrap_or_default(),
                delta_time: r.read_f64().unwrap_or_default(),
                delta_freq: r.read_u32().unwrap_or_default(),
                mode: r.read_utf8().unwrap_or_default(),
                message: r.read_utf8().unwrap_or_default(),
                low_confidence: r.read_bool().unwrap_or_default(),
                modifiers: r.read_u8().unwrap_or_default(),
            },
            TYPE_CLOSE => InMsg::Close,
            TYPE_REPLAY => InMsg::Replay,
            TYPE_HALT_TX => InMsg::HaltTx {
                auto_tx_only: r.read_bool().unwrap_or_default(),
            },
            other => InMsg::Other(other),
        };
        Some((id, msg))
    }
}

#[cfg(test)]
mod golden {
    use super::*;

    // Captured WSJT-X 2.2.2 heartbeat (test vector from k0swe/wsjtx-go, Apache-2.0).
    const HEARTBEAT: &str = "adbccbda00000002000000000000000657534a542d5800000003000000053\
                             22e322e3200000006306439623936";
    // Captured FT8 decode: "~", "JA2EJP N4BP 73", SNR -5, DT 0.2, DF 1302, new=true.
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

    #[test]
    fn heartbeat_round_trips_byte_exact() {
        let bytes = hex(HEARTBEAT);
        let (id, msg) = InMsg::decode(&bytes).expect("decodes");
        assert_eq!(id, "WSJT-X");
        let InMsg::Heartbeat {
            max_schema,
            version,
            revision,
        } = msg
        else {
            panic!()
        };
        assert_eq!(
            (max_schema, version.as_str(), revision.as_str()),
            (3, "2.2.2", "0d9b96")
        );
        // Re-encode must be byte-identical (same field order, schema 2, utf8 lengths).
        let out = OutMsg::Heartbeat {
            max_schema: 3,
            version: "2.2.2".into(),
            revision: "0d9b96".into(),
        };
        assert_eq!(out.encode("WSJT-X"), bytes);
    }

    #[test]
    fn decode_msg_encodes_byte_exact() {
        // The captured packet's DeltaTime field is only float32-precision: WSJT-X
        // computes DT internally as a C++ `float` and widens to `double` solely for
        // the wire write, so the 8 bytes here are the double-widened bit pattern of
        // 0.2f32 (0x3fc99999a0000000), not the closest f64 to 0.2 (0x3fc999999999999a).
        // Reproduce those exact bits rather than the literal `0.2` so the byte-exact
        // assertion holds against the real capture; this is an artifact of the
        // golden vector, not something pancetta's own encoder needs to replicate.
        let delta_time = f64::from_bits(0x3fc9_9999_a000_0000);
        let out = OutMsg::Decode {
            new: true,
            time_ms: 0x0259baf8,
            snr: -5,
            delta_time,
            delta_freq: 1302,
            mode: "~".into(),
            message: "JA2EJP N4BP 73".into(),
            low_confidence: false,
            off_air: false,
        };
        assert_eq!(out.encode("WSJT-X"), hex(DECODE));
    }

    #[test]
    fn reader_tolerates_truncation_and_trailing_bytes() {
        let mut bytes = hex(HEARTBEAT);
        bytes.truncate(bytes.len() - 10); // drop revision → absent field, not error
        let (_, msg) = InMsg::decode(&bytes).expect("short datagram still decodes");
        let InMsg::Heartbeat { revision, .. } = msg else {
            panic!()
        };
        assert_eq!(revision, "");
        let mut bytes = hex(HEARTBEAT);
        bytes.extend_from_slice(&[0xde, 0xad]); // trailing bytes → ignored
        assert!(InMsg::decode(&bytes).is_some());
    }

    #[test]
    fn null_and_empty_strings_both_decode_to_empty() {
        let mut w = Writer::new();
        w.utf8_null(); // 0xFFFFFFFF
        w.utf8(""); // 0x00000000
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.read_utf8(), Some(String::new()));
        assert_eq!(r.read_utf8(), Some(String::new()));
    }

    #[test]
    fn qdatetime_utc_encoding() {
        // 2020-10-30 = JDN 2459153; 12:34:56.789 UTC = 45_296_789 ms; timespec 1.
        let mut w = Writer::new();
        w.qdatetime_utc(2459153, 45_296_789);
        let b = w.as_slice();
        assert_eq!(&b[0..8], &2459153u64.to_be_bytes());
        assert_eq!(&b[8..12], &45_296_789u32.to_be_bytes());
        assert_eq!(b[12], 1);
    }
}

#[cfg(test)]
mod additional {
    use super::*;

    /// Header-only OUT messages carry no fields beyond magic/schema/type/id.
    #[test]
    fn clear_and_close_are_header_only() {
        let clear = OutMsg::Clear.encode("WSJT-X - pancetta");
        let close = OutMsg::Close.encode("WSJT-X - pancetta");
        let mut r = Reader::new(&clear);
        assert_eq!(r.read_u32(), Some(MAGIC));
        assert_eq!(r.read_u32(), Some(SCHEMA_OUT));
        assert_eq!(r.read_u32(), Some(TYPE_CLEAR));
        assert_eq!(r.read_utf8(), Some("WSJT-X - pancetta".to_string()));
        assert_eq!(r.remaining(), 0);

        let mut r = Reader::new(&close);
        assert_eq!(r.read_u32(), Some(MAGIC));
        assert_eq!(r.read_u32(), Some(SCHEMA_OUT));
        assert_eq!(r.read_u32(), Some(TYPE_CLOSE));
        assert_eq!(r.read_utf8(), Some("WSJT-X - pancetta".to_string()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn status_round_trips_all_21_fields_in_order() {
        let status = OutMsg::Status {
            dial_frequency: 14_074_000,
            mode: "FT8".into(),
            dx_call: "K1ABC".into(),
            report: "-15".into(),
            tx_mode: "FT8".into(),
            tx_enabled: true,
            transmitting: false,
            decoding: true,
            rx_df: 1500,
            tx_df: 1500,
            de_call: "K5ARH".into(),
            de_grid: "EM12".into(),
            dx_grid: "FN42".into(),
            tx_watchdog: false,
            sub_mode: "".into(),
            fast_mode: false,
            special_operation_mode: 0,
            frequency_tolerance: u32::MAX,
            tr_period: 15,
            configuration_name: "Default".into(),
            tx_message: "K1ABC K5ARH EM12".into(),
        };
        let bytes = status.encode("WSJT-X - pancetta");

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u32(), Some(MAGIC));
        assert_eq!(r.read_u32(), Some(SCHEMA_OUT));
        assert_eq!(r.read_u32(), Some(TYPE_STATUS));
        assert_eq!(r.read_utf8(), Some("WSJT-X - pancetta".to_string()));
        assert_eq!(r.read_u64(), Some(14_074_000));
        assert_eq!(r.read_utf8(), Some("FT8".to_string()));
        assert_eq!(r.read_utf8(), Some("K1ABC".to_string()));
        assert_eq!(r.read_utf8(), Some("-15".to_string()));
        assert_eq!(r.read_utf8(), Some("FT8".to_string()));
        assert_eq!(r.read_bool(), Some(true));
        assert_eq!(r.read_bool(), Some(false));
        assert_eq!(r.read_bool(), Some(true));
        assert_eq!(r.read_u32(), Some(1500));
        assert_eq!(r.read_u32(), Some(1500));
        assert_eq!(r.read_utf8(), Some("K5ARH".to_string()));
        assert_eq!(r.read_utf8(), Some("EM12".to_string()));
        assert_eq!(r.read_utf8(), Some("FN42".to_string()));
        assert_eq!(r.read_bool(), Some(false));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.read_bool(), Some(false));
        assert_eq!(r.read_u8(), Some(0));
        assert_eq!(r.read_u32(), Some(u32::MAX));
        assert_eq!(r.read_u32(), Some(15));
        assert_eq!(r.read_utf8(), Some("Default".to_string()));
        assert_eq!(r.read_utf8(), Some("K1ABC K5ARH EM12".to_string()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn qso_logged_round_trips_all_17_fields_in_order() {
        let msg = OutMsg::QsoLogged {
            date_time_off: (2459153, 45_296_789),
            dx_call: "K1ABC".into(),
            dx_grid: "FN42".into(),
            tx_frequency: 14_074_200,
            mode: "FT8".into(),
            report_sent: "-10".into(),
            report_received: "-08".into(),
            tx_power: "100".into(),
            comments: "".into(),
            name: "".into(),
            date_time_on: (2459153, 45_280_000),
            operator_call: "K5ARH".into(),
            my_call: "K5ARH".into(),
            my_grid: "EM12".into(),
            exchange_sent: "".into(),
            exchange_received: "".into(),
            adif_propagation_mode: "".into(),
        };
        let bytes = msg.encode("WSJT-X - pancetta");

        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u32(), Some(MAGIC));
        assert_eq!(r.read_u32(), Some(SCHEMA_OUT));
        assert_eq!(r.read_u32(), Some(TYPE_QSO_LOGGED));
        assert_eq!(r.read_utf8(), Some("WSJT-X - pancetta".to_string()));
        assert_eq!(r.read_qdatetime_utc(), Some((2459153, 45_296_789)));
        assert_eq!(r.read_utf8(), Some("K1ABC".to_string()));
        assert_eq!(r.read_utf8(), Some("FN42".to_string()));
        assert_eq!(r.read_u64(), Some(14_074_200));
        assert_eq!(r.read_utf8(), Some("FT8".to_string()));
        assert_eq!(r.read_utf8(), Some("-10".to_string()));
        assert_eq!(r.read_utf8(), Some("-08".to_string()));
        assert_eq!(r.read_utf8(), Some("100".to_string()));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.read_qdatetime_utc(), Some((2459153, 45_280_000)));
        assert_eq!(r.read_utf8(), Some("K5ARH".to_string()));
        assert_eq!(r.read_utf8(), Some("K5ARH".to_string()));
        assert_eq!(r.read_utf8(), Some("EM12".to_string()));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.read_utf8(), Some("".to_string()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn logged_adif_round_trips() {
        let adif = "<adif_ver:5>3.1.0<programid:6>WSJT-X<EOH>\n<call:5>K1ABC<EOR>\n";
        let bytes = OutMsg::LoggedAdif { adif: adif.into() }.encode("WSJT-X");
        let mut r = Reader::new(&bytes);
        assert_eq!(r.read_u32(), Some(MAGIC));
        assert_eq!(r.read_u32(), Some(SCHEMA_OUT));
        assert_eq!(r.read_u32(), Some(TYPE_LOGGED_ADIF));
        assert_eq!(r.read_utf8(), Some("WSJT-X".to_string()));
        assert_eq!(r.read_utf8(), Some(adif.to_string()));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn inbound_clear_window_present_and_absent() {
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(2);
        w.u32(TYPE_CLEAR);
        w.utf8("WSJT-X");
        w.u8(1); // Window = 1 (clear Rx Frequency window)
        let (id, msg) = InMsg::decode(w.as_slice()).expect("decodes");
        assert_eq!(id, "WSJT-X");
        assert_eq!(msg, InMsg::Clear { window: Some(1) });

        // No Window byte at all → tolerated absence, not an error.
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(2);
        w.u32(TYPE_CLEAR);
        w.utf8("WSJT-X");
        let (_, msg) = InMsg::decode(w.as_slice()).expect("decodes");
        assert_eq!(msg, InMsg::Clear { window: None });
    }

    #[test]
    fn inbound_reply_all_8_fields() {
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(2);
        w.u32(TYPE_REPLY);
        w.utf8("WSJT-X");
        w.qtime_ms(0x0259baf8);
        w.i32(-5);
        w.f64(0.2);
        w.u32(1302);
        w.utf8("~");
        w.utf8("CQ K1ABC FN42");
        w.bool(false);
        w.u8(0); // Modifiers — GridTracker always sends 0
        let (id, msg) = InMsg::decode(w.as_slice()).expect("decodes");
        assert_eq!(id, "WSJT-X");
        assert_eq!(
            msg,
            InMsg::Reply {
                time_ms: 0x0259baf8,
                snr: -5,
                delta_time: 0.2,
                delta_freq: 1302,
                mode: "~".into(),
                message: "CQ K1ABC FN42".into(),
                low_confidence: false,
                modifiers: 0,
            }
        );
    }

    #[test]
    fn inbound_close_replay_halt_tx_and_other() {
        for (type_id, expected) in [(TYPE_CLOSE, InMsg::Close), (TYPE_REPLAY, InMsg::Replay)] {
            let mut w = Writer::new();
            w.u32(MAGIC);
            w.u32(2);
            w.u32(type_id);
            w.utf8("WSJT-X");
            let (_, msg) = InMsg::decode(w.as_slice()).expect("decodes");
            assert_eq!(msg, expected);
        }

        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(2);
        w.u32(TYPE_HALT_TX);
        w.utf8("WSJT-X");
        w.bool(true);
        let (_, msg) = InMsg::decode(w.as_slice()).expect("decodes");
        assert_eq!(msg, InMsg::HaltTx { auto_tx_only: true });

        // Unknown/unmodeled type → Other(type), header still parsed, no panic.
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(2);
        w.u32(9); // FreeText — not modeled by this codec yet
        w.utf8("WSJT-X");
        w.utf8("hello");
        w.bool(true);
        let (id, msg) = InMsg::decode(w.as_slice()).expect("decodes");
        assert_eq!(id, "WSJT-X");
        assert_eq!(msg, InMsg::Other(9));
    }

    #[test]
    fn decode_rejects_bad_magic_and_accepts_schema_2_and_3() {
        let mut w = Writer::new();
        w.u32(0xdead_beef);
        w.u32(2);
        w.u32(TYPE_HEARTBEAT);
        w.utf8("WSJT-X");
        assert!(InMsg::decode(w.as_slice()).is_none());

        for schema in [2u32, 3] {
            let mut w = Writer::new();
            w.u32(MAGIC);
            w.u32(schema);
            w.u32(TYPE_HEARTBEAT);
            w.utf8("WSJT-X");
            w.u32(3);
            w.utf8("2.6.1");
            w.utf8("");
            assert!(InMsg::decode(w.as_slice()).is_some());
        }

        // Schema outside 2..=3 is not accepted.
        let mut w = Writer::new();
        w.u32(MAGIC);
        w.u32(1);
        w.u32(TYPE_HEARTBEAT);
        w.utf8("WSJT-X");
        assert!(InMsg::decode(w.as_slice()).is_none());
    }
}
