//! View payloads sent server→client.
use chrono::{DateTime, Utc};
use pancetta_core::slot::SlotParity;
use serde::{Deserialize, Serialize};

/// A DX-Hunter row (a spotted/decoded station).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub slot_parity: Option<SlotParity>,
    pub last_seen: DateTime<Utc>,
    /// "local" | "network" | "both" (string for forward-compat).
    pub source: String,
}

/// QSO progress (the exchange ladder + last messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QsoProgress {
    pub qso_id: String,
    pub their_callsign: String,
    pub state: String,
    pub frequency_hz: f64,
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
pub struct PendingCall {
    pub callsign: String,
    pub dx_parity: Option<SlotParity>,
    pub waited_secs: u64,
}

/// A single decoded FT8 frame (the live decode feed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedView {
    pub timestamp: DateTime<Utc>,
    pub frequency_hz: f64,
    pub snr: i32,
    pub delta_time: f32,
    pub delta_freq: f32,
    pub call_sign: Option<String>,
    pub grid_square: Option<String>,
    pub message: String,
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
        assert_eq!(serde_json::from_str::<QsoProgress>(&j).unwrap(), q);

        let p = PendingCall {
            callsign: "VK9XX".into(),
            dx_parity: Some(SlotParity::Even),
            waited_secs: 45,
        };
        assert_eq!(
            serde_json::from_str::<PendingCall>(&serde_json::to_string(&p).unwrap()).unwrap(),
            p
        );

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
        assert_eq!(
            serde_json::from_str::<DecodedView>(&serde_json::to_string(&d).unwrap()).unwrap(),
            d
        );
    }
}
