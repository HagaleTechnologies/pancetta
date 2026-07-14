//! Control-frame mapping — the **pure, total** translation from a decrypted
//! inner wire frame to a coordinator-executable [`ControlAction`].
//!
//! Decrypted inner frames arriving over the Noise channel are one of:
//!
//! 1. A **rig-api.v1** `clientFrame` (`dispensa contracts/rig/rig-api.v1.schema.json`)
//!    — the read/control surface the remote gateway speaks. A `clientFrame` is
//!    either `{frame:"hello", …}` or `{frame:"command", command:{cmd:"…", …}}`.
//!    `hello` maps to [`ControlAction::Hello`] (Q-0019 #5, cqdx PR #144,
//!    2026-07-02): it MAY carry a `capabilityToken` that the dispatcher
//!    verifies against the pinned IdP keys and binds to the E2E-connected
//!    client identity, rooting read/qsy scope in that verified identity —
//!    never in relay admission (the relay envelope's `src` is
//!    relay-forgeable). A `hello` with no/invalid/mismatched token is served
//!    nothing scoped (v1 back-compat: the token is optional on the wire).
//! 2. An **e2e-auth.v1** inner-control frame — a `txHeartbeat`
//!    (`$defs.txHeartbeat`, the dead-man keep-alive), a `txArm`
//!    (`$defs.txArm`, the explicitly-armed TX authorization: the
//!    `capabilityToken` + client-signed `grant` carried as SIBLINGS), or a
//!    `txDisarm` (`$defs.txDisarm`, the explicit disarm carrying the
//!    `armJti` it releases). The `txArm` token + grant are verified by
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
//! | `setTransmitArmed` | `armed`                                  | [`Disarm`](ControlAction::Disarm) when `armed==false` (with an empty `arm_jti` — this command carries no armJti); `armed==true` alone is NOT a grant (a real arm carries a signed `txArm`) so it maps to [`Unsupported`](ControlAction::Unsupported) |
//!
//! ## e2e-auth.v1 inner-control frames
//!
//! | wire `type`   | [`ControlAction`]                          |
//! |---------------|--------------------------------------------|
//! | `txHeartbeat` | [`Heartbeat`](ControlAction::Heartbeat)    |
//! | `txArm`       | [`Arm`](ControlAction::Arm) (`capabilityToken` + `grant` carried through as SIBLINGS for [`crate::capability`] verification; a missing token or grant is a **hard error**, NOT `Unsupported`) |
//! | `txDisarm`    | [`Disarm`](ControlAction::Disarm) (carries the `armJti` being released — a sanity match, not a security gate) |
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
    /// A well-formed-JSON but structurally-invalid **security** frame: a
    /// `txArm` missing its `capabilityToken` or `grant`. This is a HARD error
    /// (fail-closed) — never a silent [`ControlAction::Unsupported`] — because a
    /// partial arm frame must never be treated as a benign no-op.
    #[error("malformed security frame: {0}")]
    MalformedFrame(String),
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
    /// e2e-auth.v1 `txArm` inner-control frame (`$defs.txArm`). Carries the
    /// cqdx-issued `capabilityToken` (compact JWS) and the client-signed
    /// `grant` as **SIBLINGS** — the token is NOT inside the grant's
    /// `clientSig` canonical bytes (`clientSig` signs only the `txArmGrant`,
    /// which references the token via `capabilityJti`). Both are carried
    /// through untouched for [`crate::capability`] to verify as SEPARATE
    /// inputs; this variant asserts *nothing* about validity.
    Arm {
        /// The cqdx-issued capabilityToken (compact JWS), verified downstream
        /// against the pinned IdP key — a SEPARATE input from `grant`.
        capability_token: String,
        /// The raw `txArmGrant` object, verified downstream (never trusted
        /// here). Its `clientSig` covers only these grant fields.
        grant: Value,
    },
    /// e2e-auth.v1 `txDisarm` (`$defs.txDisarm`) — explicit disarm. Carries the
    /// `armJti` being released (a **sanity match** to the current arm, NOT a
    /// security gate; disarm-any is fail-safe TX-OFF). Also produced by
    /// `setTransmitArmed { armed: false }` (with an empty `arm_jti`, since that
    /// legacy command carries no armJti).
    Disarm {
        /// The `txArmGrant.jti` this disarm targets (empty when unknown).
        arm_jti: String,
    },
    /// rig-api.v1 `hello` (dispensa Q-0019 #5, cqdx PR #144, amended
    /// 2026-07-02). Carries an OPTIONAL `capabilityToken` (compact JWS) that
    /// roots read/qsy authorization in the E2E identity, never in relay
    /// admission (the relay envelope's `src` is relay-forgeable). Optional on
    /// the wire for v1 back-compat, but a session with no/invalid/mismatched
    /// token is served nothing scoped — see the dispatcher.
    Hello {
        /// The cqdx-issued capabilityToken, if the client sent one. `None` on
        /// a legacy hello (v1 back-compat) — served nothing scoped.
        capability_token: Option<String>,
    },
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
        return map_auth_control(ty, &v);
    }

    // rig-api.v1 clientFrame is discriminated by a `frame` tag.
    match v.get("frame").and_then(Value::as_str) {
        Some("command") => match v.get("command") {
            Some(cmd) => Ok(map_client_command(cmd)),
            None => Ok(ControlAction::Unsupported),
        },
        // "hello" carries an OPTIONAL capabilityToken (Q-0019 #5) that the
        // dispatcher verifies to root read/qsy scope for the rest of the
        // session. A hello with no `hello` object, or a non-string token
        // field, is treated as carrying no token (back-compat), not an error.
        Some("hello") => {
            let capability_token = v
                .get("hello")
                .and_then(|h| h.get("capabilityToken"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(ControlAction::Hello { capability_token })
        }
        // Any other frame has no coordinator action.
        _ => Ok(ControlAction::Unsupported),
    }
}

/// Map an e2e-auth.v1 inner-control frame (`type`-tagged).
///
/// A `txArm` missing its `capabilityToken` or `grant` is a **hard error**
/// ([`ControlError::MalformedFrame`]) — a partial arm must fail closed, never be
/// silently treated as [`ControlAction::Unsupported`]. Every other unknown or
/// under-specified frame maps to `Unsupported` (benign no-op upstream).
fn map_auth_control(ty: &str, v: &Value) -> Result<ControlAction, ControlError> {
    match ty {
        "txHeartbeat" => {
            let arm_jti = v.get("armJti").and_then(Value::as_str);
            let seq = v.get("seq").and_then(Value::as_u64);
            match (arm_jti, seq) {
                (Some(arm_jti), Some(seq)) => Ok(ControlAction::Heartbeat {
                    arm_jti: arm_jti.to_string(),
                    seq,
                }),
                // Missing required fields → not a usable heartbeat.
                _ => Ok(ControlAction::Unsupported),
            }
        }
        "txArm" => {
            // capabilityToken + grant are SIBLINGS. Both are REQUIRED; a
            // partial arm fails closed (a hard error), never a silent no-op.
            let capability_token = v
                .get("capabilityToken")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ControlError::MalformedFrame("txArm missing capabilityToken".to_string())
                })?
                .to_string();
            let grant = v
                .get("grant")
                .cloned()
                .ok_or_else(|| ControlError::MalformedFrame("txArm missing grant".to_string()))?;
            Ok(ControlAction::Arm {
                capability_token,
                grant,
            })
        }
        "txDisarm" => {
            // armJti is a sanity match (not a gate); disarm-any is fail-safe.
            // A missing armJti still disarms (empty string), never an error.
            let arm_jti = v
                .get("armJti")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(ControlAction::Disarm { arm_jti })
        }
        _ => Ok(ControlAction::Unsupported),
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
            // `txArm`, so `armed:true` is a no-op here. This legacy command
            // carries no armJti, so the disarm targets "any current arm".
            Some(false) => ControlAction::Disarm {
                arm_jti: String::new(),
            },
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
                "callsign": "N0CALL",
                "frequencyHz": 1500,
                "dxParity": "even"
            }
        }));
        assert_eq!(
            action,
            ControlAction::TxRequest(TxKind::CallStation {
                callsign: "N0CALL".to_string(),
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
        // setTransmitArmed{armed:false} → Disarm with an empty arm_jti (this
        // legacy command carries no armJti).
        assert_eq!(
            map(json!({"frame":"command","command":{"cmd":"setTransmitArmed","armed":false}})),
            ControlAction::Disarm {
                arm_jti: String::new()
            }
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
    fn tx_arm_maps_token_and_grant_as_siblings() {
        // Frozen e2e-auth.v1 $defs.txArm: {type, capabilityToken, grant} — the
        // token is a SIBLING of the grant, NOT nested inside it.
        let grant = json!({
            "aud": "agent-1",
            "clientKeyId": "client-1",
            "sessionId": "sess-1",
            "capabilityJti": "cap-abc",
            "operatorCallsign": "N0CALL",
            "armedUntil": 1719000600000_u64,
            "heartbeatIntervalSec": 10,
            "jti": "arm-abc-123",
            "clientSig": "sig-base64url"
        });
        let action = map(json!({
            "type": "txArm",
            "capabilityToken": "hdr.payload.sig",
            "grant": grant.clone()
        }));
        assert_eq!(
            action,
            ControlAction::Arm {
                capability_token: "hdr.payload.sig".to_string(),
                grant,
            }
        );
    }

    #[test]
    fn tx_arm_missing_capability_token_is_hard_error() {
        // A partial arm (no capabilityToken) must fail CLOSED, not map to
        // Unsupported.
        let err = map_client_frame(
            json!({
                "type": "txArm",
                "grant": { "jti": "arm-1", "clientSig": "s" }
            })
            .to_string()
            .as_bytes(),
        );
        assert!(matches!(err, Err(ControlError::MalformedFrame(_))));
    }

    #[test]
    fn tx_arm_missing_grant_is_hard_error() {
        let err = map_client_frame(
            json!({
                "type": "txArm",
                "capabilityToken": "hdr.payload.sig"
            })
            .to_string()
            .as_bytes(),
        );
        assert!(matches!(err, Err(ControlError::MalformedFrame(_))));
    }

    #[test]
    fn tx_disarm_maps_arm_jti() {
        assert_eq!(
            map(json!({ "type": "txDisarm", "armJti": "arm-abc-123" })),
            ControlAction::Disarm {
                arm_jti: "arm-abc-123".to_string()
            }
        );
    }

    #[test]
    fn tx_disarm_missing_arm_jti_still_disarms_any() {
        // armJti is a sanity match, not a gate — a missing one still disarms.
        assert_eq!(
            map(json!({ "type": "txDisarm" })),
            ControlAction::Disarm {
                arm_jti: String::new()
            }
        );
    }

    #[test]
    fn hello_frame_without_token_carries_none() {
        // v1 back-compat: a legacy hello with no capabilityToken maps to
        // Hello{capability_token: None}, not Unsupported — the dispatcher
        // decides what "no token" means for scope (nothing scoped).
        let action = map(json!({
            "frame": "hello",
            "hello": { "protocolVersion": 1, "clientName": "Panino", "clientVersion": "0.1.0" }
        }));
        assert_eq!(
            action,
            ControlAction::Hello {
                capability_token: None
            }
        );
    }

    #[test]
    fn hello_frame_with_token_carries_it() {
        let action = map(json!({
            "frame": "hello",
            "hello": { "protocolVersion": 1, "capabilityToken": "eyJ.abc.def" }
        }));
        assert_eq!(
            action,
            ControlAction::Hello {
                capability_token: Some("eyJ.abc.def".to_string())
            }
        );
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

    // ── Drift-guard: our txArm/txDisarm wire mapping must track the FROZEN
    //    e2e-auth.v1 $defs (camelCase `capabilityToken`/`armJti`, `type` consts).
    //    If cqdx/dispensa renames a field, these break loudly.
    #[test]
    fn drift_guard_tx_arm_frozen_field_names() {
        // The frozen $defs.txArm shape: {type:"txArm", capabilityToken, grant}.
        let action = map(json!({
            "type": "txArm",
            "capabilityToken": "hdr.pl.sig",
            "grant": { "capabilityJti": "cap-1", "jti": "arm-1" }
        }));
        assert_eq!(
            action,
            ControlAction::Arm {
                capability_token: "hdr.pl.sig".to_string(),
                grant: json!({ "capabilityJti": "cap-1", "jti": "arm-1" }),
            }
        );
        // A snake_case sibling (`capability_token`) is NOT the frozen name →
        // treated as a missing token → hard error (drift caught).
        let err = map_client_frame(
            json!({ "type": "txArm", "capability_token": "x", "grant": {} })
                .to_string()
                .as_bytes(),
        );
        assert!(matches!(err, Err(ControlError::MalformedFrame(_))));
        // Wrong `type` const → Unsupported (not an Arm).
        assert_eq!(
            map(json!({ "type": "txArmGrant", "capabilityToken": "x", "grant": {} })),
            ControlAction::Unsupported
        );
    }

    #[test]
    fn drift_guard_tx_disarm_frozen_field_names() {
        // Frozen $defs.txDisarm: {type:"txDisarm", armJti}.
        assert_eq!(
            map(json!({ "type": "txDisarm", "armJti": "arm-1" })),
            ControlAction::Disarm {
                arm_jti: "arm-1".to_string()
            }
        );
        // snake_case `arm_jti` is NOT the frozen name → treated as absent
        // (disarm-any, empty arm_jti) rather than picking it up.
        assert_eq!(
            map(json!({ "type": "txDisarm", "arm_jti": "arm-1" })),
            ControlAction::Disarm {
                arm_jti: String::new()
            }
        );
    }
}
