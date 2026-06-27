//! Events sent server→client.
use crate::dto::{DecodedView, DxRow, PendingCall, QsoProgress};
use pancetta_core::TxPolicy;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Decoded(DecodedView),
    DxHunter {
        rows: Vec<DxRow>,
    },
    ActiveQsos {
        qsos: Vec<QsoProgress>,
        pending: Vec<PendingCall>,
    },
    Frequency {
        vfo: u8,
        frequency_hz: u64,
    },
    Split {
        tx_hz: u64,
    },
    SignalStrength {
        db_over_s9: i32,
    },
    TxStatus {
        active: bool,
    },
    TxPolicy {
        policy: TxPolicy,
    },
    /// Control/arm state for the receiving session.
    ControlState {
        control_held_by_me: bool,
        transmit_armed: bool,
    },
    Status {
        component: String,
        status: String,
    },
    Error {
        component: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::DecodedView;
    use chrono::Utc;
    #[test]
    fn event_roundtrips() {
        let e = ServerEvent::TxPolicy {
            policy: TxPolicy::Full,
        };
        assert_eq!(
            serde_json::from_str::<ServerEvent>(&serde_json::to_string(&e).unwrap()).unwrap(),
            e
        );
        let d = ServerEvent::Decoded(DecodedView {
            timestamp: Utc::now(),
            frequency_hz: 14_075_931.0,
            snr: -8,
            delta_time: 0.2,
            delta_freq: 0.0,
            call_sign: Some("D2UY".into()),
            grid_square: None,
            message: "CQ D2UY JI64".into(),
            slot_parity: None,
            is_directed_at_us: false,
            worked_before: false,
            needed: true,
            atno: true,
            priority_score: Some(720),
        });
        assert_eq!(
            serde_json::from_str::<ServerEvent>(&serde_json::to_string(&d).unwrap()).unwrap(),
            d
        );
    }
}
