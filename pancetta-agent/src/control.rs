//! Control-frame mapping — the **pure, total** translation from a decrypted
//! inner wire frame to a coordinator-executable [`ControlAction`].
//!
//! Decrypted inner frames arriving over the Noise channel are one of:
//!
//! 1. A **rig-api.v1** `clientFrame` (`dispensa contracts/rig/rig-api.v1.schema.json`)
//!    — the read/control surface the remote gateway speaks. A `clientFrame` is
//!    either `{frame:"hello", …}` or `{frame:"command", command:{cmd:"…", …}}`.
//!    Only the `command` frames carry actions; `hello` is a handshake nicety and
//!    maps to [`ControlAction::Unsupported`] here (handled elsewhere).
//! 2. An **e2e-auth.v1** inner-control frame — either a `txHeartbeat`
//!    (`$defs.txHeartbeat`, the dead-man keep-alive) or a `txArmGrant`
//!    (`$defs.txArmGrant`, the explicitly-armed TX authorization) which may
//!    arrive as an inner control frame to be verified by
//!    [`crate::capability`] before TX is armed.
//!
//! This module is **pure**: no IO, no coordinator dependency, no clock. It
//! parses JSON and discriminates by the type tag, returning a `ControlAction`.
//! It is **total** over well-formed JSON: an unknown/unsupported frame type maps
//! to [`ControlAction::Unsupported`] (logged + ignored upstream, NOT an error).
//! Only genuinely malformed JSON returns [`ControlError`].
//!
//! ## Command → action mapping (rig-api.v1 `clientCommand`)
//!
//! | wire `cmd`         | fields                                   | [`ControlAction`]                    |
//! |--------------------|------------------------------------------|--------------------------------------|
//! | `setFrequency`     | `vfo`, `frequencyHz`                      | [`Qsy`](ControlAction::Qsy)          |
//! | `setSplit`         | `enabled`, `txFrequencyHz`               | [`SetSplit`](ControlAction::SetSplit)|
//! | `callStation`      | `callsign`, `frequencyHz`, `dxParity?`   | [`TxRequest`](ControlAction::TxRequest) (`CallStation`) |
//! | `answerCaller`     | `callsign`, `frequencyHz`, `step`, `snr?`| [`TxRequest`](ControlAction::TxRequest) (`AnswerCaller`)|
//! | `startCq`          | `frequencyOffsetHz`                       | [`TxRequest`](ControlAction::TxRequest) (`StartCq`)     |
//! | `stopCq`           | —                                        | [`StopCq`](ControlAction::StopCq)    |
//! | `takeControl`      | —                                        | [`TakeControl`](ControlAction::TakeControl)   |
//! | `releaseControl`   | —                                        | [`ReleaseControl`](ControlAction::ReleaseControl) |
//! | `setTransmitArmed` | `armed`                                  | [`Disarm`](ControlAction::Disarm) when `armed==false`; `armed==true` alone is NOT a grant (a real arm carries a signed `txArmGrant`) so it maps to [`Unsupported`](ControlAction::Unsupported) |
//!
//! ## e2e-auth.v1 inner-control frames
//!
//! | wire `type`   | [`ControlAction`]                          |
//! |---------------|--------------------------------------------|
//! | `txHeartbeat` | [`Heartbeat`](ControlAction::Heartbeat)    |
//! | `txArmGrant`  | [`Arm`](ControlAction::Arm) (raw grant carried through for [`crate::capability`] verification) |
//!
//! ## Adaptations from the task brief
//!
//! The task brief listed illustrative variants (`ReadStatus`, `SetMode`,
//! `transmit`). rig-api.v1 has **no** `status`, `setMode`, or generic
//! `transmit` command — the TX surface is the three specific TX-initiation
//! commands (`callStation` / `answerCaller` / `startCq`), all folded into
//! [`ControlAction::TxRequest`] with a [`TxKind`] discriminant. `SetMode` and
//! `ReadStatus` therefore have no wire source and are intentionally absent;
//! any future/unknown `cmd` string is handled by the total `Unsupported` arm.

use serde_json::Value;

/// Error mapping a decrypted inner frame — only genuinely malformed JSON.
///
/// An unknown/unsupported *but well-formed* frame is **not** an error; it maps
/// to [`ControlAction::Unsupported`].
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The decrypted bytes were not valid JSON.
    #[error("malformed control frame JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Which TX-initiation the client requested. Mirrors the three TX-capable
/// rig-api.v1 `clientCommand`s. All are gated by the armed-capability path
/// (`ArmState`/[`crate::capability`]) before the coordinator executes them.
#[derive(Debug, Clone, PartialEq)]
pub enum TxKind {
    /// `callStation` — initiate a QSO by calling a specific station.
    CallStation {
        /// The DX callsign to call.
        callsign: String,
        /// The DX's audio offset (Hz within the passband) we heard them on.
        frequency_hz: f64,
        /// The DX's slot parity, if known (`"even"` / `"odd"`).
        dx_parity: Option<String>,
    },
    /// `answerCaller` — answer a station that called us, opening at `step`.
    AnswerCaller {
        /// The caller's callsign.
        callsign: String,
        /// The caller's audio offset (Hz within the passband).
        frequency_hz: f64,
        /// Exchange-ladder rung to open at (`grid`/`report`/`reportAck`/`rr73`/`seventyThree`).
        step: String,
        /// The caller's slot parity, if known.
        dx_parity: Option<String>,
        /// The caller's SNR, if the client supplied it.
        snr: Option<f64>,
    },
    /// `startCq` — begin calling CQ at the given audio offset.
    StartCq {
        /// The audio offset (Hz within the passband) to call CQ on.
        offset_hz: f64,
    },
}

/// A decrypted inner frame mapped to the action the coordinator will execute.
///
/// Pure data — carries no trust. TX-capable variants ([`TxRequest`](Self::TxRequest),
/// [`Arm`](Self::Arm)) are still subject to the fail-closed arm/capability gate
/// downstream; this enum only records *what was asked*.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlAction {
    /// `setFrequency` — retune the rig dial (QSY).
    Qsy {
        /// Target VFO index (rig-api.v1 `vfo`).
        vfo: i64,
        /// Target dial frequency in Hz.
        frequency_hz: f64,
    },
    /// `setSplit` — enable/disable split and set the TX dial.
    SetSplit {
        /// Whether split is enabled.
        enabled: bool,
        /// The TX dial frequency in Hz (meaningful when `enabled`).
        tx_frequency_hz: f64,
    },
    /// A TX-initiation request (`callStation` / `answerCaller` / `startCq`).
    TxRequest(TxKind),
    /// `stopCq` — cancel any in-progress CQ.
    StopCq,
    /// `takeControl` — client requests exclusive control of the station.
    TakeControl,
    /// `releaseControl` — client releases control.
    ReleaseControl,
    /// e2e-auth.v1 `txHeartbeat` — the dead-man keep-alive for an armed window.
    Heartbeat {
        /// The `txArmGrant.jti` this heartbeat keeps alive.
        arm_jti: String,
        /// Monotonic per-arm sequence number (agent rejects `seq <= last`).
        seq: u64,
    },
    /// e2e-auth.v1 `txArmGrant` arriving as an inner control frame. The raw
    /// grant JSON is carried through untouched for [`crate::capability`] to
    /// verify (client signature, allow-list, window, heartbeat bound). This
    /// variant asserts *nothing* about validity.
    Arm {
        /// The raw `txArmGrant` object, verified downstream (never trusted here).
        grant: Value,
    },
    /// `setTransmitArmed { armed: false }` — explicit disarm request.
    Disarm,
    /// A well-formed but unknown/unsupported frame — logged + ignored upstream.
    Unsupported,
}

/// Parse a decrypted inner frame and map it to a [`ControlAction`].
///
/// Pure + total over well-formed JSON. Unknown/unsupported (but well-formed)
/// frames → `Ok(ControlAction::Unsupported)`. Malformed JSON → `Err`.
pub fn map_client_frame(decrypted: &[u8]) -> Result<ControlAction, ControlError> {
    let v: Value = serde_json::from_slice(decrypted)?;

    // e2e-auth.v1 inner-control frames are discriminated by a `type` tag.
    if let Some(ty) = v.get("type").and_then(Value::as_str) {
        return Ok(map_auth_control(ty, &v));
    }

    // rig-api.v1 clientFrame is discriminated by a `frame` tag.
    match v.get("frame").and_then(Value::as_str) {
        Some("command") => match v.get("command") {
            Some(cmd) => Ok(map_client_command(cmd)),
            None => Ok(ControlAction::Unsupported),
        },
        // "hello" (and any other frame) has no coordinator action.
        _ => Ok(ControlAction::Unsupported),
    }
}

/// Map an e2e-auth.v1 inner-control frame (`type`-tagged).
fn map_auth_control(ty: &str, v: &Value) -> ControlAction {
    match ty {
        "txHeartbeat" => {
            let arm_jti = v.get("armJti").and_then(Value::as_str);
            let seq = v.get("seq").and_then(Value::as_u64);
            match (arm_jti, seq) {
                (Some(arm_jti), Some(seq)) => ControlAction::Heartbeat {
                    arm_jti: arm_jti.to_string(),
                    seq,
                },
                // Missing required fields → not a usable heartbeat.
                _ => ControlAction::Unsupported,
            }
        }
        "txArmGrant" => ControlAction::Arm { grant: v.clone() },
        _ => ControlAction::Unsupported,
    }
}

/// Map a rig-api.v1 `clientCommand` object (`cmd`-tagged).
fn map_client_command(cmd: &Value) -> ControlAction {
    let name = match cmd.get("cmd").and_then(Value::as_str) {
        Some(name) => name,
        None => return ControlAction::Unsupported,
    };

    match name {
        "setFrequency" => {
            match (
                cmd.get("vfo").and_then(Value::as_i64),
                cmd.get("frequencyHz").and_then(json_number),
            ) {
                (Some(vfo), Some(frequency_hz)) => ControlAction::Qsy { vfo, frequency_hz },
                _ => ControlAction::Unsupported,
            }
        }
        "setSplit" => {
            match (
                cmd.get("enabled").and_then(Value::as_bool),
                cmd.get("txFrequencyHz").and_then(json_number),
            ) {
                (Some(enabled), Some(tx_frequency_hz)) => ControlAction::SetSplit {
                    enabled,
                    tx_frequency_hz,
                },
                _ => ControlAction::Unsupported,
            }
        }
        "callStation" => {
            match (
                cmd.get("callsign").and_then(Value::as_str),
                cmd.get("frequencyHz").and_then(json_number),
            ) {
                (Some(callsign), Some(frequency_hz)) => {
                    ControlAction::TxRequest(TxKind::CallStation {
                        callsign: callsign.to_string(),
                        frequency_hz,
                        dx_parity: cmd
                            .get("dxParity")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                }
                _ => ControlAction::Unsupported,
            }
        }
        "answerCaller" => {
            match (
                cmd.get("callsign").and_then(Value::as_str),
                cmd.get("frequencyHz").and_then(json_number),
                cmd.get("step").and_then(Value::as_str),
            ) {
                (Some(callsign), Some(frequency_hz), Some(step)) => {
                    ControlAction::TxRequest(TxKind::AnswerCaller {
                        callsign: callsign.to_string(),
                        frequency_hz,
                        step: step.to_string(),
                        dx_parity: cmd
                            .get("dxParity")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        snr: cmd.get("snr").and_then(json_number),
                    })
                }
                _ => ControlAction::Unsupported,
            }
        }
        "startCq" => match cmd.get("frequencyOffsetHz").and_then(json_number) {
            Some(offset_hz) => ControlAction::TxRequest(TxKind::StartCq { offset_hz }),
            None => ControlAction::Unsupported,
        },
        "stopCq" => ControlAction::StopCq,
        "takeControl" => ControlAction::TakeControl,
        "releaseControl" => ControlAction::ReleaseControl,
        "setTransmitArmed" => match cmd.get("armed").and_then(Value::as_bool) {
            // Only an explicit disarm is actionable from this command; a bare
            // `armed:true` is NOT a grant — a real arm carries a signed
            // txArmGrant, so `armed:true` is a no-op here.
            Some(false) => ControlAction::Disarm,
            _ => ControlAction::Unsupported,
        },
        _ => ControlAction::Unsupported,
    }
}

/// Accept both integer and float JSON numbers as `f64` (the schema uses
/// `integer` for dial/offset Hz but `number` for `snr`/`startCq` offset).
fn json_number(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> ControlAction {
        map_client_frame(v.to_string().as_bytes()).expect("well-formed JSON maps")
    }

    #[test]
    fn set_frequency_maps_to_qsy() {
        let action = map(json!({
            "frame": "command",
            "command": { "cmd": "setFrequency", "vfo": 0, "frequencyHz": 14074000 }
        }));
        assert_eq!(
            action,
            ControlAction::Qsy {
                vfo: 0,
                frequency_hz: 14074000.0
            }
        );
    }

    #[test]
    fn set_split_maps_to_set_split() {
        let action = map(json!({
            "frame": "command",
            "command": { "cmd": "setSplit", "enabled": true, "txFrequencyHz": 14076000 }
        }));
        assert_eq!(
            action,
            ControlAction::SetSplit {
                enabled: true,
                tx_frequency_hz: 14076000.0
            }
        );
    }

    #[test]
    fn call_station_maps_to_tx_request() {
        let action = map(json!({
            "frame": "command",
            "command": {
                "cmd": "callStation",
                "callsign": "K5ARH",
                "frequencyHz": 1500,
                "dxParity": "even"
            }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::CallStation {
                callsign: "K5ARH".to_string(),
                frequency_hz: 1500.0,
                dx_parity: Some("even".to_string()),
            })
        );
    }

    #[test]
    fn call_station_optional_parity_absent() {
        let action = map(json!({
            "frame": "command",
            "command": { "cmd": "callStation", "callsign": "W1XYZ", "frequencyHz": 1200 }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::CallStation {
                callsign: "W1XYZ".to_string(),
                frequency_hz: 1200.0,
                dx_parity: None,
            })
        );
    }

    #[test]
    fn answer_caller_maps_to_tx_request_with_step_and_snr() {
        let action = map(json!({
            "frame": "command",
            "command": {
                "cmd": "answerCaller",
                "callsign": "N0CALL",
                "frequencyHz": 800,
                "step": "report",
                "snr": -12,
                "dxParity": "odd"
            }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::AnswerCaller {
                callsign: "N0CALL".to_string(),
                frequency_hz: 800.0,
                step: "report".to_string(),
                dx_parity: Some("odd".to_string()),
                snr: Some(-12.0),
            })
        );
    }

    #[test]
    fn answer_caller_snr_optional() {
        let action = map(json!({
            "frame": "command",
            "command": {
                "cmd": "answerCaller",
                "callsign": "N0CALL",
                "frequencyHz": 800,
                "step": "seventyThree"
            }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::AnswerCaller {
                callsign: "N0CALL".to_string(),
                frequency_hz: 800.0,
                step: "seventyThree".to_string(),
                dx_parity: None,
                snr: None,
            })
        );
    }

    #[test]
    fn start_cq_maps_to_tx_request() {
        let action = map(json!({
            "frame": "command",
            "command": { "cmd": "startCq", "frequencyOffsetHz": 1234.5 }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::StartCq { offset_hz: 1234.5 })
        );
    }

    #[test]
    fn stop_cq_take_release_control() {
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"stopCq"}})),
            ControlAction::StopCq
        );
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"takeControl"}})),
            ControlAction::TakeControl
        );
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"releaseControl"}})),
            ControlAction::ReleaseControl
        );
    }

    #[test]
    fn set_transmit_armed_false_disarms_true_is_unsupported() {
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"setTransmitArmed","armed":false}})),
            ControlAction::Disarm
        );
        // armed:true alone is NOT a grant.
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"setTransmitArmed","armed":true}})),
            ControlAction::Unsupported
        );
    }

    #[test]
    fn tx_heartbeat_maps_to_heartbeat() {
        let action = map(json!({
            "type": "txHeartbeat",
            "armJti": "arm-abc-123",
            "seq": 7,
            "ts": 1719000000000_u64
        }));
        assert_eq!(
            action,
            ControlAction::Heartbeat {
                arm_jti: "arm-abc-123".to_string(),
                seq: 7,
            }
        );
    }

    #[test]
    fn tx_heartbeat_missing_fields_is_unsupported() {
        assert_eq!(
            map(json!({"type":"txHeartbeat","seq":1})),
            ControlAction::Unsupported
        );
    }

    #[test]
    fn tx_arm_grant_carries_raw_grant_through() {
        let grant = json!({
            "type": "txArmGrant",
            "aud": "agent-1",
            "clientKeyId": "client-1",
            "sessionId": "sess-1",
            "operatorCallsign": "K5ARH",
            "armedUntil": 1719000600000_u64,
            "heartbeatIntervalSec": 10,
            "jti": "arm-abc-123",
            "clientSig": "sig-base64url"
        });
        let action = map(grant.clone());
        assert_eq!(action, ControlAction::Arm { grant });
    }

    #[test]
    fn hello_frame_is_unsupported() {
        let action = map(json!({
            "frame": "hello",
            "hello": { "protocolVersion": 1, "clientName": "Panino", "clientVersion": "0.1.0" }
        }));
        assert_eq!(action, ControlAction::Unsupported);
    }

    #[test]
    fn unknown_command_is_unsupported() {
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"setMode","mode":"FT4"}})),
            ControlAction::Unsupported
        );
    }

    #[test]
    fn unknown_frame_and_type_are_unsupported() {
        assert_eq!(map(json!({"frame":"quux"})), ControlAction::Unsupported);
        assert_eq!(map(json!({"type":"quux"})), ControlAction::Unsupported);
        assert_eq!(map(json!({"nonsense": true})), ControlAction::Unsupported);
    }

    #[test]
    fn command_frame_missing_command_is_unsupported() {
        assert_eq!(map(json!({"frame":"command"})), ControlAction::Unsupported);
    }

    #[test]
    fn malformed_json_is_error() {
        assert!(map_client_frame(b"not json at all").is_err());
        assert!(map_client_frame(b"{unterminated").is_err());
        assert!(map_client_frame(b"").is_err());
    }
}
