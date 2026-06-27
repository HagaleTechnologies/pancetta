//! Handshake + full-state snapshot + top-level wire frames.
use crate::command::ClientCommand;
use crate::dto::{DecodedView, DxRow, PendingCall, QsoProgress};
use crate::event::ServerEvent;
use pancetta_core::TxPolicy;
use serde::{Deserialize, Serialize};

/// Client's opening frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
}

/// Server's reply: negotiated version + full current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub server_version: String,
    pub snapshot: StateSnapshot,
}

/// Full current state sent once on connect, before live deltas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub frequency_hz: u64,
    pub split_tx_hz: u64,
    pub tx_policy: TxPolicy,
    pub dx_hunter: Vec<DxRow>,
    pub active_qsos: Vec<QsoProgress>,
    pub pending_calls: Vec<PendingCall>,
    pub recent_decodes: Vec<DecodedView>,
}

/// Top-level frame client→server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    Command(ClientCommand),
}

/// Top-level frame server→client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome(Welcome),
    Event(ServerEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    #[test]
    fn frames_roundtrip() {
        let hello = ClientFrame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "Panino".into(),
            client_version: "0.1.0".into(),
        });
        assert_eq!(
            serde_json::from_str::<ClientFrame>(&serde_json::to_string(&hello).unwrap()).unwrap(),
            hello
        );

        let snap = StateSnapshot {
            frequency_hz: 14_074_000,
            split_tx_hz: 0,
            tx_policy: TxPolicy::Full,
            dx_hunter: vec![],
            active_qsos: vec![],
            pending_calls: vec![],
            recent_decodes: vec![],
        };
        let welcome = ServerFrame::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.9.5".into(),
            snapshot: snap,
        });
        assert_eq!(
            serde_json::from_str::<ServerFrame>(&serde_json::to_string(&welcome).unwrap()).unwrap(),
            welcome
        );
    }
}
