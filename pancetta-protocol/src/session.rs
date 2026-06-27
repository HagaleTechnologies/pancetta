//! Handshake + full-state snapshot + top-level wire frames.
//!
//! Frame enums use struct variants with a named payload field so that serde's
//! internally-tagged enum nests the payload object rather than flattening it.
//! For example `ClientFrame::Hello` serialises as:
//! ```json
//! { "frame": "hello", "hello": { "protocolVersion": 1, … } }
//! ```
//! which matches the dispensa rig-api.v1 schema's `clientFrame` shape.
use crate::command::ClientCommand;
use crate::dto::{DecodedView, DxRow, PendingCall, QsoProgress};
use crate::event::ServerEvent;
use crate::wire_serde;
use pancetta_core::TxPolicy;
use serde::{Deserialize, Serialize};

/// Client's opening frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
}

/// Server's reply: negotiated version + full current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Welcome {
    pub protocol_version: u32,
    pub server_version: String,
    pub snapshot: StateSnapshot,
}

/// Full current state sent once on connect, before live deltas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub frequency_hz: u64,
    pub split_tx_hz: u64,
    #[serde(with = "wire_serde::tx_policy")]
    pub tx_policy: TxPolicy,
    pub dx_hunter: Vec<DxRow>,
    pub active_qsos: Vec<QsoProgress>,
    pub pending_calls: Vec<PendingCall>,
    pub recent_decodes: Vec<DecodedView>,
}

/// Top-level frame client→server.
///
/// Each variant carries its payload in a named field so serde nests it:
/// `{"frame":"hello","hello":{…}}` and `{"frame":"command","command":{…}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "camelCase")]
pub enum ClientFrame {
    /// Opening handshake.
    Hello { hello: Hello },
    /// A `ClientCommand` relayed over the wire.
    Command { command: ClientCommand },
}

impl ClientFrame {
    /// Convenience constructor for the hello frame.
    pub fn hello(hello: Hello) -> Self {
        ClientFrame::Hello { hello }
    }

    /// Convenience constructor for a command frame.
    pub fn command(command: ClientCommand) -> Self {
        ClientFrame::Command { command }
    }
}

/// Top-level frame server→client.
///
/// Each variant carries its payload in a named field so serde nests it:
/// `{"frame":"welcome","welcome":{…}}` and `{"frame":"event","event":{…}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "camelCase")]
pub enum ServerFrame {
    /// Server's opening reply carrying a full state snapshot.
    Welcome { welcome: Welcome },
    /// A `ServerEvent` pushed to the client.
    Event { event: ServerEvent },
}

impl ServerFrame {
    /// Convenience constructor for the welcome frame.
    pub fn welcome(welcome: Welcome) -> Self {
        ServerFrame::Welcome { welcome }
    }

    /// Convenience constructor for an event frame.
    pub fn event(event: ServerEvent) -> Self {
        ServerFrame::Event { event }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    use pancetta_core::TxPolicy;

    fn empty_snapshot() -> StateSnapshot {
        StateSnapshot {
            frequency_hz: 14_074_000,
            split_tx_hz: 0,
            tx_policy: TxPolicy::Full,
            dx_hunter: vec![],
            active_qsos: vec![],
            pending_calls: vec![],
            recent_decodes: vec![],
        }
    }

    #[test]
    fn frames_roundtrip() {
        let hello = ClientFrame::hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "Panino".into(),
            client_version: "0.1.0".into(),
        });
        let hj = serde_json::to_string(&hello).unwrap();
        assert_eq!(serde_json::from_str::<ClientFrame>(&hj).unwrap(), hello);

        let welcome = ServerFrame::welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.9.5".into(),
            snapshot: empty_snapshot(),
        });
        let wj = serde_json::to_string(&welcome).unwrap();
        assert_eq!(serde_json::from_str::<ServerFrame>(&wj).unwrap(), welcome);
    }

    #[test]
    fn frame_shapes_match_schema() {
        // clientFrame hello → {"frame":"hello","hello":{…}}
        let hf = ClientFrame::hello(Hello {
            protocol_version: 1,
            client_name: "Panino".into(),
            client_version: "0.1.0".into(),
        });
        let j = serde_json::to_string(&hf).unwrap();
        assert!(
            j.contains(r#""frame":"hello""#),
            "expected frame:hello in: {j}"
        );
        assert!(
            j.contains(r#""hello":{"#),
            "expected nested hello object in: {j}"
        );
        assert!(
            j.contains(r#""protocolVersion""#),
            "expected protocolVersion (camelCase) in: {j}"
        );
        assert!(
            j.contains(r#""clientName""#),
            "expected clientName (camelCase) in: {j}"
        );

        // serverFrame welcome → {"frame":"welcome","welcome":{…}}
        let wf = ServerFrame::welcome(Welcome {
            protocol_version: 1,
            server_version: "0.9.5".into(),
            snapshot: empty_snapshot(),
        });
        let j = serde_json::to_string(&wf).unwrap();
        assert!(
            j.contains(r#""frame":"welcome""#),
            "expected frame:welcome in: {j}"
        );
        assert!(
            j.contains(r#""welcome":{"#),
            "expected nested welcome object in: {j}"
        );
        assert!(
            j.contains(r#""serverVersion""#),
            "expected serverVersion (camelCase) in: {j}"
        );

        // serverFrame event → {"frame":"event","event":{…}}
        let ef = ServerFrame::event(ServerEvent::TxStatus { active: false });
        let j = serde_json::to_string(&ef).unwrap();
        assert!(
            j.contains(r#""frame":"event""#),
            "expected frame:event in: {j}"
        );
        assert!(
            j.contains(r#""event":{"#),
            "expected nested event object in: {j}"
        );
    }

    #[test]
    fn snapshot_camel_case_fields() {
        let snap = empty_snapshot();
        let j = serde_json::to_string(&snap).unwrap();
        assert!(
            j.contains(r#""frequencyHz""#),
            "expected frequencyHz in: {j}"
        );
        assert!(j.contains(r#""splitTxHz""#), "expected splitTxHz in: {j}");
        assert!(
            j.contains(r#""txPolicy":"full""#),
            "expected txPolicy:full in: {j}"
        );
        assert!(j.contains(r#""dxHunter""#), "expected dxHunter in: {j}");
        assert!(j.contains(r#""activeQsos""#), "expected activeQsos in: {j}");
        assert!(
            j.contains(r#""pendingCalls""#),
            "expected pendingCalls in: {j}"
        );
        assert!(
            j.contains(r#""recentDecodes""#),
            "expected recentDecodes in: {j}"
        );
        assert_eq!(serde_json::from_str::<StateSnapshot>(&j).unwrap(), snap);
    }

    #[test]
    fn command_frame_shape() {
        use crate::command::ClientCommand;
        let cf = ClientFrame::command(ClientCommand::StopCq);
        let j = serde_json::to_string(&cf).unwrap();
        assert!(
            j.contains(r#""frame":"command""#),
            "expected frame:command in: {j}"
        );
        assert!(
            j.contains(r#""command":{"#),
            "expected nested command object in: {j}"
        );
        assert!(
            j.contains(r#""cmd":"stopCq""#),
            "expected cmd:stopCq in: {j}"
        );
        assert_eq!(serde_json::from_str::<ClientFrame>(&j).unwrap(), cf);
    }
}
