//! View payloads sent server→client.
//!
//! All structs use `#[serde(rename_all = "camelCase")]` so that field names
//! in JSON match the dispensa rig-api.v1 schema (ADR-0003). Fields that hold
//! pancetta-core enum types with PascalCase serialization use `wire_serde`
//! helpers to re-map them to the schema's camelCase values.
use crate::wire_serde;
use chrono::{DateTime, Utc};
use pancetta_core::slot::SlotParity;
use serde::{Deserialize, Serialize};

/// A DX-Hunter row (a spotted/decoded station).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DxRow {
    pub call_sign: String,
    pub grid_square: Option<String>,
    pub frequency_hz: f64,
    pub mode: String,
    pub snr: i32,
    pub distance_km: Option<f64>,
    pub bearing: Option<f64>,
    pub worked_before: bool,
    pub needed: bool,
    pub atno: bool,
    pub priority: u32,
    pub entity_name: Option<String>,
    pub rarity_tier: Option<String>,
    pub audio_offset_hz: Option<u64>,
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub slot_parity: Option<SlotParity>,
    pub last_seen: DateTime<Utc>,
    /// "local" | "network" | "both" (string for forward-compat).
    pub source: String,
}

/// QSO progress (the exchange ladder + last messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QsoProgress {
    pub qso_id: String,
    pub their_callsign: String,
    pub state: String,
    pub frequency_hz: f64,
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub tx_parity: Option<SlotParity>,
    pub ladder_labels: Vec<String>,
    pub ladder_ours: Vec<bool>,
    pub ladder_index: usize,
    pub now_line: String,
    pub next_line: String,
    pub last_tx_text: Option<String>,
    pub last_rx_text: Option<String>,
    pub report_sent: Option<i32>,
    pub report_received: Option<i32>,
    pub dx_last_activity: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// A manual call parked in the cross-parity queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingCall {
    pub callsign: String,
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub dx_parity: Option<SlotParity>,
    pub waited_secs: u64,
}

/// A single decoded FT8 frame (the live decode feed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodedView {
    pub timestamp: DateTime<Utc>,
    pub frequency_hz: f64,
    pub snr: i32,
    pub delta_time: f32,
    pub delta_freq: f32,
    pub call_sign: Option<String>,
    pub grid_square: Option<String>,
    pub message: String,
    #[serde(with = "wire_serde::slot_parity_opt")]
    pub slot_parity: Option<SlotParity>,
    pub is_directed_at_us: bool,
    pub worked_before: bool,
    pub needed: bool,
    pub atno: bool,
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
            last_seen: Utc::now(),
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
            started_at: Utc::now(),
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
            timestamp: Utc::now(),
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
}
