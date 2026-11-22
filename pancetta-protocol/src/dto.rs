//! View payloads sent server→client.
//!
//! All structs use `#[serde(rename_all = "camelCase")]` so that field names
//! in JSON match the dispensa rig-api.v1 schema (ADR-0003). Fields that hold
//! pancetta-core enum types with PascalCase serialization use `wire_serde`
//! helpers to re-map them to the schema's camelCase values.
use crate::wire_serde;
use pancetta_core::slot::SlotParity;
use pancetta_core::time::UtcTimestamp;
use serde::{Deserialize, Serialize};

/// A DX-Hunter row (a spotted/decoded station).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DxRow {
    /// The station's callsign.
    pub call_sign: String,
    /// The station's Maidenhead grid locator; `None` when not known.
    pub grid_square: Option<String>,
    /// Frequency in Hz the station was heard or spotted on.
    pub frequency_hz: f64,
    /// Operating mode name, for example `"FT8"`.
    pub mode: String,
    /// Signal-to-noise ratio in dB.
    pub snr: i32,
    /// Distance from our station in kilometres; `None` when not known.
    pub distance_km: Option<f64>,
    /// Bearing from our station in degrees; `None` when not known.
    pub bearing: Option<f64>,
    /// True when this station has been worked before.
    pub worked_before: bool,
    /// True only when the station's cqdx.io needed list has loaded and names this entity; false when not needed or unknown.
    pub needed: bool,
    /// True only when `needed` is true and cqdx.io lists the entity as an all-time new one.
    pub atno: bool,
    /// Ranking score for the row; higher ranks first.
    pub priority: u32,
    /// DXCC entity name; `None` when not resolved.
    pub entity_name: Option<String>,
    /// cqdx.io rarity tier slug (for example `"rare"`); `None` when not known.
    pub rarity_tier: Option<String>,
    /// Audio offset in Hz within the receive passband; `None` when not known.
    pub audio_offset_hz: Option<u64>,
    /// Slot parity the station transmits in; `None` when not known.
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub slot_parity: Option<SlotParity>,
    /// When the station was last heard or spotted (UTC).
    pub last_seen: UtcTimestamp,
    /// "local" | "network" | "both" (string for forward-compat).
    pub source: String,
}

/// QSO progress (the exchange ladder + last messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QsoProgress {
    /// Stable id of the QSO (UUID string). No `ClientCommand` targets a QSO
    /// by this id; the call and answer commands select stations by callsign.
    pub qso_id: String,
    /// The other station's callsign.
    pub their_callsign: String,
    /// Short human-readable state name, for example `"wait rpt"` or `"sending RR73"`.
    pub state: String,
    /// Audio frequency in Hz (offset within the passband) this QSO is worked on.
    pub frequency_hz: f64,
    /// Slot parity we transmit in for this QSO; `None` when not yet assigned.
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub tx_parity: Option<SlotParity>,
    /// Exchange-ladder rung labels, left to right; empty for states with no ladder.
    pub ladder_labels: Vec<String>,
    /// Per rung of `ladder_labels`, true when that rung's message is one we transmit.
    pub ladder_ours: Vec<bool>,
    /// Index of the current rung in `ladder_labels`; `0` when the labels are
    /// empty (states with no ladder), so check the labels before indexing.
    pub ladder_index: usize,
    /// Human-readable line describing what the QSO is doing now; empty when there is no ladder.
    pub now_line: String,
    /// Human-readable line describing what is expected next; empty when there is no ladder.
    pub next_line: String,
    /// Raw text of the most recent recorded outbound message; `None` until
    /// one is recorded. A manual opener records its first message before it
    /// airs, so this can be set before the first transmission.
    pub last_tx_text: Option<String>,
    /// Raw text of the last message received from the other station; `None` before any.
    pub last_rx_text: Option<String>,
    /// Signal report in dB we sent them; `None` until one is set.
    pub report_sent: Option<i32>,
    /// Signal report in dB they sent us; `None` until one is received.
    pub report_received: Option<i32>,
    /// Short summary of the other station's latest decoded frame on the band
    /// (for example `"CQ"` or `"→ us -09"`); `None` when nothing recent was heard.
    pub dx_last_activity: Option<String>,
    /// When this QSO started (UTC).
    pub started_at: UtcTimestamp,
}

/// A manual call parked in the cross-parity queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingCall {
    /// DX callsign the operator chose to work.
    pub callsign: String,
    /// Slot parity the DX transmits in (we would reply on the opposite one);
    /// `None` when not known.
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub dx_parity: Option<SlotParity>,
    /// Wall-clock seconds since the call was parked in the queue.
    pub waited_secs: u64,
}

/// A single spectrum/waterfall row for the `spectrum` server event
/// (dispensa Q-0024). `bin_start_hz` is RF-absolute (dial + audio bin
/// offset, converted by the remote gateway); `mags_db` is raw,
/// pre-normalization dB, one value per bin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spectrum {
    /// RF-absolute frequency in Hz of bin 0 (dial plus audio bin offset).
    pub bin_start_hz: f64,
    /// Width of each bin in Hz.
    pub bin_width_hz: f64,
    /// Raw, pre-normalization magnitude in dB, one value per bin.
    pub mags_db: Vec<f32>,
    /// Sequence number, monotone per display-feed session, so consumers can detect drops.
    pub seq: u64,
    /// Time of the waterfall row (UTC).
    pub timestamp: UtcTimestamp,
}

/// One ranked TX-placement candidate — dispensa Q-0026.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSlice {
    /// Candidate TX audio offset in Hz (baseband, not RF-absolute).
    pub offset_hz: f64,
    /// Allocator score for this candidate; higher is better.
    pub score: f64,
    /// No decode activity within 50 Hz in the First (even) slot across retained history.
    pub clear_first: bool,
    /// No decode activity within 50 Hz in the Second (odd) slot across retained history.
    pub clear_second: bool,
}

/// Per-window TX-placement/openness snapshot — dispensa Q-0026. Mirrors
/// `pancetta_qso::frequency::PlacementSnapshot` field-for-field, minus
/// `clear_both_slots`/`noise_floor` (local-TUI-only, not part of the agreed
/// wire contract). `offset_hz` values are baseband/audio-passband offsets,
/// NOT RF-absolute (unlike `Spectrum::bin_start_hz`) — consumers combine
/// with the `frequency` serverEvent's dial Hz for an absolute value, the
/// same convention the local TUI already uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// Top-N ranked candidates, sorted by score descending.
    pub slices: Vec<PlacementSlice>,
    /// Per-bin openness code across the full allocation range:
    /// 0 = busy in both slots, 1 = second-only clear, 2 = first-only clear, 3 = clear in both.
    pub openness: Vec<u8>,
    /// Bin width in Hz (the allocator's step).
    pub bin_hz: f64,
    /// Allocation range `(min, max)` in Hz, baseband like `offset_hz`.
    pub range: (f64, f64),
}

/// A single decoded FT8 frame (the live decode feed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodedView {
    /// When the frame was decoded (UTC).
    pub timestamp: UtcTimestamp,
    /// RF-absolute frequency in Hz: the RX dial plus the frame's audio offset.
    pub frequency_hz: f64,
    /// Signal-to-noise ratio in dB, rounded to the nearest integer.
    pub snr: i32,
    /// Decoder time offset (DT) of the frame in seconds.
    pub delta_time: f32,
    /// Audio frequency offset of the frame in Hz (baseband, not RF-absolute).
    pub delta_freq: f32,
    /// Sending station's callsign; `None` when the message carries none.
    pub call_sign: Option<String>,
    /// Grid locator in the message; `None` when the message carries none.
    pub grid_square: Option<String>,
    /// Full decoded message text.
    pub message: String,
    /// Slot parity the frame was received in; `None` when not known.
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub slot_parity: Option<SlotParity>,
    /// True when the message is addressed to our callsign.
    pub is_directed_at_us: bool,
    /// True when the sending station has already been worked on this frame's band.
    pub worked_before: bool,
    /// True only when the station's cqdx.io needed list has loaded and names this entity; false when not needed or unknown.
    pub needed: bool,
    /// True only when `needed` is true and cqdx.io lists the entity as an all-time new one.
    pub atno: bool,
    /// Priority score for the frame; the remote gateway currently always sends `None`.
    pub priority_score: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dxrow_roundtrips() {
        let r = DxRow {
            call_sign: "D2UY".into(),
            grid_square: Some("JI64".into()),
            frequency_hz: 14_074_000.0,
            mode: "FT8".into(),
            snr: -11,
            distance_km: None,
            bearing: None,
            worked_before: false,
            needed: true,
            atno: true,
            priority: 720,
            entity_name: Some("Angola".into()),
            rarity_tier: Some("rare".into()),
            audio_offset_hz: Some(1934),
            slot_parity: Some(SlotParity::Even),
            last_seen: UtcTimestamp::now(),
            source: "local".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        // Verify camelCase field names appear in the output.
        assert!(j.contains(r#""callSign""#), "expected callSign in: {j}");
        assert!(
            j.contains(r#""frequencyHz""#),
            "expected frequencyHz in: {j}"
        );
        assert!(
            j.contains(r#""workedBefore""#),
            "expected workedBefore in: {j}"
        );
        assert!(
            j.contains(r#""slotParity":"even""#),
            "expected slotParity:even in: {j}"
        );
        assert!(j.contains(r#""lastSeen""#), "expected lastSeen in: {j}");
        assert_eq!(serde_json::from_str::<DxRow>(&j).unwrap(), r);
    }

    #[test]
    fn qsoprogress_and_pending_and_decoded_roundtrip() {
        let q = QsoProgress {
            qso_id: "abc".into(),
            their_callsign: "ZL3IO".into(),
            state: "WaitingForReport".into(),
            frequency_hz: 14_075_500.0,
            tx_parity: Some(SlotParity::Odd),
            ladder_labels: vec!["Grid".into(), "Rpt".into()],
            ladder_ours: vec![true, false],
            ladder_index: 1,
            now_line: "TX: ZL3IO K5ARH R-09".into(),
            next_line: "RR73".into(),
            last_tx_text: Some("ZL3IO K5ARH R-09".into()),
            last_rx_text: Some("K5ARH ZL3IO -12".into()),
            report_sent: Some(-9),
            report_received: Some(-12),
            dx_last_activity: Some("\u{2192} us -12".into()),
            started_at: UtcTimestamp::now(),
        };
        let j = serde_json::to_string(&q).unwrap();
        assert!(j.contains(r#""qsoId""#), "expected qsoId in: {j}");
        assert!(
            j.contains(r#""theirCallsign""#),
            "expected theirCallsign in: {j}"
        );
        assert!(
            j.contains(r#""txParity":"odd""#),
            "expected txParity:odd in: {j}"
        );
        assert!(
            j.contains(r#""ladderLabels""#),
            "expected ladderLabels in: {j}"
        );
        assert!(j.contains(r#""ladderOurs""#), "expected ladderOurs in: {j}");
        assert_eq!(serde_json::from_str::<QsoProgress>(&j).unwrap(), q);

        let p = PendingCall {
            callsign: "VK9XX".into(),
            dx_parity: Some(SlotParity::Even),
            waited_secs: 45,
        };
        let pj = serde_json::to_string(&p).unwrap();
        assert!(
            pj.contains(r#""dxParity":"even""#),
            "expected dxParity:even in: {pj}"
        );
        assert!(
            pj.contains(r#""waitedSecs""#),
            "expected waitedSecs in: {pj}"
        );
        assert_eq!(serde_json::from_str::<PendingCall>(&pj).unwrap(), p);

        let d = DecodedView {
            timestamp: UtcTimestamp::now(),
            frequency_hz: 14_075_931.0,
            snr: -8,
            delta_time: 0.2,
            delta_freq: 0.0,
            call_sign: Some("D2UY".into()),
            grid_square: Some("JI64".into()),
            message: "CQ D2UY JI64".into(),
            slot_parity: Some(SlotParity::Even),
            is_directed_at_us: false,
            worked_before: false,
            needed: true,
            atno: true,
            priority_score: Some(720),
        };
        let dj = serde_json::to_string(&d).unwrap();
        assert!(
            dj.contains(r#""frequencyHz""#),
            "expected frequencyHz in: {dj}"
        );
        assert!(dj.contains(r#""deltaTime""#), "expected deltaTime in: {dj}");
        assert!(dj.contains(r#""deltaFreq""#), "expected deltaFreq in: {dj}");
        assert!(dj.contains(r#""callSign""#), "expected callSign in: {dj}");
        assert!(
            dj.contains(r#""isDirectedAtUs""#),
            "expected isDirectedAtUs in: {dj}"
        );
        assert!(
            dj.contains(r#""slotParity":"even""#),
            "expected slotParity:even in: {dj}"
        );
        assert_eq!(serde_json::from_str::<DecodedView>(&dj).unwrap(), d);
    }

    #[test]
    fn spectrum_roundtrips_and_is_camel_case() {
        let s = Spectrum {
            bin_start_hz: 14_074_000.0,
            bin_width_hz: 5.86,
            mags_db: vec![-104.2, -98.1, -110.0],
            seq: 1234,
            timestamp: UtcTimestamp::now(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains(r#""binStartHz""#), "expected binStartHz in: {j}");
        assert!(j.contains(r#""binWidthHz""#), "expected binWidthHz in: {j}");
        assert!(j.contains(r#""magsDb""#), "expected magsDb in: {j}");
        assert!(j.contains(r#""seq":1234"#), "expected seq in: {j}");
        assert_eq!(serde_json::from_str::<Spectrum>(&j).unwrap(), s);
    }

    #[test]
    fn placement_fields_are_camel_case() {
        let p = Placement {
            slices: vec![PlacementSlice {
                offset_hz: 1500.0,
                score: 42.5,
                clear_first: true,
                clear_second: false,
            }],
            openness: vec![3, 2, 0],
            bin_hz: 5.86,
            range: (200.0, 3000.0),
        };
        let j = serde_json::to_string(&p).unwrap();
        assert!(
            j.contains(r#""offsetHz":1500.0"#),
            "expected offsetHz in: {j}"
        );
        assert!(
            j.contains(r#""clearFirst":true"#),
            "expected clearFirst in: {j}"
        );
        assert!(
            j.contains(r#""clearSecond":false"#),
            "expected clearSecond in: {j}"
        );
        assert!(j.contains(r#""binHz":5.86"#), "expected binHz in: {j}");
        assert!(
            j.contains(r#""range":[200.0,3000.0]"#),
            "expected range array in: {j}"
        );
        let round: Placement = serde_json::from_str(&j).unwrap();
        assert_eq!(round, p);
    }
}
