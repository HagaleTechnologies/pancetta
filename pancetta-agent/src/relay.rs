//! relay.v1 wire frames + a mockable WebSocket seam for the agent leg.
//!
//! The RigRelay Durable Object (cqdx) speaks a flat, `t`-discriminated JSON
//! frame set — one frame per WebSocket text message. This module models those
//! frames ([`RelayFrame`]) with **exact** on-wire field names, a size-bounded
//! parser ([`parse_frame`]), error-code classification ([`is_terminal`]), and a
//! synchronous [`WsConn`] trait so the session driver can be exercised with a
//! scripted mock instead of a real socket. The async dial/reconnect loop lands
//! in P3.4.
//!
//! Contract: `dispensa/contracts/relay/relay.v1.schema.json`
//! (relay.v1, adopted 2026-06-30; auth.sig domain-separation per Q-0011).
//!
//! Wire conventions honored here:
//! - Binary fields (`challenge`, `sig`, `payload`) are **unpadded base64url**.
//! - `ready`/`presence` are DO-emitted only; the agent only ever receives them
//!   on its single DO connection, so it trusts them as DO-origin.
//! - The DO stamps `src` on forwarded `env` frames; senders never set `src`.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Maximum size of a single inbound WS text frame, in bytes. Frames larger than
/// this are rejected ([`RelayError::FrameTooLarge`]) BEFORE any parse, closing
/// the unbounded-allocation vector the P0–P2 review flagged.
pub const MAX_FRAME_BYTES: usize = 65536;

/// Upper bound on a single **decoded** Noise message (handshake or transport)
/// carried in `env.payload`, per the Noise spec (RFC-level 65535-byte cap). A
/// payload whose decoded length exceeds this is rejected without allocating the
/// oversized buffer downstream.
pub const MAX_NOISE_MSG: usize = 65535;

/// Errors from frame parsing / payload decoding.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// The raw text frame exceeded [`MAX_FRAME_BYTES`].
    #[error("frame too large: {len} bytes exceeds max {max}")]
    FrameTooLarge {
        /// The offending frame length.
        len: usize,
        /// The configured maximum.
        max: usize,
    },

    /// The frame text was not valid JSON for any known `RelayFrame` variant.
    #[error("bad frame: {0}")]
    BadFrame(String),

    /// An `env.payload` was not valid unpadded base64url.
    #[error("bad payload base64: {0}")]
    BadPayload(String),

    /// A decoded `env.payload` exceeded [`MAX_NOISE_MSG`].
    #[error("payload too large: {len} decoded bytes exceeds max {max}")]
    PayloadTooLarge {
        /// The offending decoded length.
        len: usize,
        /// The configured maximum.
        max: usize,
    },

    /// The WS transport failed (send/recv). Carries a human-readable reason.
    #[error("ws transport error: {0}")]
    Transport(String),
}

/// A single relay.v1 wire frame. Serialized as flat JSON with a `t` tag; all
/// field names are camelCase to match the contract exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum RelayFrame {
    /// DO → agent: opening frame carrying the 32-byte challenge (unpadded
    /// base64url) the agent must sign. (The client-role hello is never seen by
    /// the agent, so its `role`/`capabilityToken` fields are not modeled here.)
    #[serde(rename = "hello")]
    Hello {
        /// 32 random bytes, unpadded base64url. Signed (domain-separated) in
        /// [`RelayFrame::Auth`].
        challenge: String,
    },

    /// Agent → DO: authenticate by signing the `hello` challenge.
    #[serde(rename = "auth")]
    Auth {
        /// Always `"agent"`.
        role: String,
        /// The agent's keyId (unpadded base64url SHA-256 of the identity SPKI).
        #[serde(rename = "agentKeyId")]
        agent_key_id: String,
        /// Unpadded base64url Ed25519 over
        /// `domainSeparate("cqdx-relay-agent-auth-v1", challengeBytes)`.
        sig: String,
    },

    /// DO → agent: admission complete.
    #[serde(rename = "ready")]
    Ready {
        /// The routing keyId of this connection (the agent's keyId).
        #[serde(rename = "keyId")]
        key_id: String,
        /// Whether the peer (client leg) is connected at the moment of admission.
        #[serde(rename = "peerPresent")]
        peer_present: bool,
    },

    /// DO → agent: the peer joined (`up`) or left (`down`).
    #[serde(rename = "presence")]
    Presence {
        /// The keyId of the peer whose state changed.
        peer: String,
        /// `"up"` or `"down"`.
        state: String,
    },

    /// Opaque data frame. Carries Noise handshake or transport bytes in
    /// `payload` (unpadded base64url). The agent sets `dst = clientKeyId`; the
    /// DO stamps `src` on forwarded frames.
    #[serde(rename = "env")]
    Env {
        /// Target keyId.
        dst: String,
        /// Opaque unpadded base64url payload.
        payload: String,
        /// Sender keyId, stamped by the DO on forwarded frames only.
        #[serde(skip_serializing_if = "Option::is_none")]
        src: Option<String>,
    },

    /// Graceful close signal (either direction).
    #[serde(rename = "bye")]
    Bye {
        /// Optional human-readable reason.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Error frame from the DO.
    #[serde(rename = "error")]
    Error {
        /// Machine-readable code; see [`is_terminal`].
        code: String,
        /// Optional human-readable detail (never machine-parsed).
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl RelayFrame {
    /// Serialize to the on-wire JSON text.
    pub fn to_json(&self) -> Result<String, RelayError> {
        serde_json::to_string(self).map_err(|e| RelayError::BadFrame(e.to_string()))
    }
}

/// Terminal error codes from the schema `x-error-codes.terminal`: the DO sends
/// the error then closes. No retry without fresh credentials.
const TERMINAL_CODES: &[&str] = &[
    "BAD_AUTH",
    "TOKEN_INVALID",
    "TOKEN_EXPIRED",
    "TOKEN_REVOKED",
    "AUD_MISMATCH",
    "SCOPE_EMPTY",
    "AGENT_OCCUPIED",
    "NOT_ADMITTED",
    "FRAME_TOO_LARGE",
    "BAD_FRAME",
    "BAD_ROUTE",
];

/// Classify an `error.code` as terminal (`true`) or transient (`false`).
///
/// Terminal codes come from the schema's `x-error-codes.terminal` list; every
/// other code (including the transient `UNKNOWN_DST`/`NO_PEER`) is treated as
/// transient. An unknown code is treated as transient (fail-open on the
/// keep-waiting side) — a terminal error the DO means to enforce is also
/// accompanied by a socket close, so the session ends regardless.
pub fn is_terminal(code: &str) -> bool {
    TERMINAL_CODES.contains(&code)
}

/// Parse a raw inbound WS text frame into a [`RelayFrame`], rejecting oversized
/// input BEFORE the serde parse (bounding allocation).
pub fn parse_frame(text: &str) -> Result<RelayFrame, RelayError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(RelayError::FrameTooLarge {
            len: text.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    serde_json::from_str::<RelayFrame>(text).map_err(|e| RelayError::BadFrame(e.to_string()))
}

/// Decode an `env.payload` (unpadded base64url) into raw bytes, bounding the
/// decoded length to [`MAX_NOISE_MSG`].
pub fn decode_env_payload(payload: &str) -> Result<Vec<u8>, RelayError> {
    // Bound the *encoded* length first — base64 expands 3→4, so an encoded
    // string longer than ceil(MAX_NOISE_MSG/3)*4 cannot decode to <= the bound.
    // We still re-check the decoded length after, but this avoids allocating a
    // huge decode buffer for a pathological input that slipped the frame bound.
    let max_encoded = (MAX_NOISE_MSG / 3 + 1) * 4;
    if payload.len() > max_encoded {
        return Err(RelayError::PayloadTooLarge {
            len: payload.len(),
            max: MAX_NOISE_MSG,
        });
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| RelayError::BadPayload(e.to_string()))?;
    if bytes.len() > MAX_NOISE_MSG {
        return Err(RelayError::PayloadTooLarge {
            len: bytes.len(),
            max: MAX_NOISE_MSG,
        });
    }
    Ok(bytes)
}

/// Encode raw bytes as an `env.payload` (unpadded base64url).
pub fn encode_env_payload(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A synchronous WebSocket seam so the session driver can be tested with a
/// scripted mock. The production async implementation lands in P3.4.
pub trait WsConn {
    /// Send a text frame.
    fn send_text(&mut self, s: String) -> Result<(), RelayError>;
    /// Receive the next text frame, or `None` if the connection is closed.
    fn recv_text(&mut self) -> Result<Option<String>, RelayError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_frame_exact_wire_shape() {
        let f = RelayFrame::Auth {
            role: "agent".to_string(),
            agent_key_id: "KID".to_string(),
            sig: "SIG".to_string(),
        };
        let json = f.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"t":"auth","role":"agent","agentKeyId":"KID","sig":"SIG"}"#
        );
        // Round-trips back to the same value.
        assert_eq!(parse_frame(&json).unwrap(), f);
    }

    #[test]
    fn env_frame_exact_wire_shape_without_src() {
        let f = RelayFrame::Env {
            dst: "CLIENT".to_string(),
            payload: "UEFZ".to_string(),
            src: None,
        };
        let json = f.to_json().unwrap();
        // src is omitted when None (senders never set src).
        assert_eq!(json, r#"{"t":"env","dst":"CLIENT","payload":"UEFZ"}"#);
        assert_eq!(parse_frame(&json).unwrap(), f);
    }

    #[test]
    fn env_frame_with_src_round_trips() {
        let json = r#"{"t":"env","dst":"AGENT","payload":"UEFZ","src":"CLIENT"}"#;
        let f = parse_frame(json).unwrap();
        assert_eq!(
            f,
            RelayFrame::Env {
                dst: "AGENT".to_string(),
                payload: "UEFZ".to_string(),
                src: Some("CLIENT".to_string()),
            }
        );
    }

    #[test]
    fn hello_frame_round_trips() {
        let json = r#"{"t":"hello","challenge":"Q0hBTA"}"#;
        let f = parse_frame(json).unwrap();
        assert_eq!(
            f,
            RelayFrame::Hello {
                challenge: "Q0hBTA".to_string()
            }
        );
    }

    #[test]
    fn ready_and_presence_round_trip() {
        let ready = RelayFrame::Ready {
            key_id: "K".to_string(),
            peer_present: true,
        };
        assert_eq!(
            ready.to_json().unwrap(),
            r#"{"t":"ready","keyId":"K","peerPresent":true}"#
        );
        assert_eq!(parse_frame(&ready.to_json().unwrap()).unwrap(), ready);

        let presence = RelayFrame::Presence {
            peer: "P".to_string(),
            state: "up".to_string(),
        };
        assert_eq!(
            presence.to_json().unwrap(),
            r#"{"t":"presence","peer":"P","state":"up"}"#
        );
        assert_eq!(parse_frame(&presence.to_json().unwrap()).unwrap(), presence);
    }

    #[test]
    fn bye_and_error_omit_optional_fields() {
        let bye = RelayFrame::Bye { reason: None };
        assert_eq!(bye.to_json().unwrap(), r#"{"t":"bye"}"#);

        let err = RelayFrame::Error {
            code: "NO_PEER".to_string(),
            message: None,
        };
        assert_eq!(err.to_json().unwrap(), r#"{"t":"error","code":"NO_PEER"}"#);
    }

    #[test]
    fn parse_frame_rejects_oversized_text() {
        let big = "x".repeat(MAX_FRAME_BYTES + 1);
        let err = parse_frame(&big);
        assert!(matches!(err, Err(RelayError::FrameTooLarge { .. })));
    }

    #[test]
    fn parse_frame_rejects_junk() {
        assert!(matches!(
            parse_frame("not json"),
            Err(RelayError::BadFrame(_))
        ));
        assert!(matches!(
            parse_frame(r#"{"t":"nope"}"#),
            Err(RelayError::BadFrame(_))
        ));
    }

    #[test]
    fn decode_env_payload_bounds_decoded_length() {
        // A valid small payload decodes fine.
        let ok = encode_env_payload(b"hello");
        assert_eq!(decode_env_payload(&ok).unwrap(), b"hello");

        // An encoded string that would decode to > MAX_NOISE_MSG is rejected on
        // the encoded-length pre-check (never allocating the decode buffer).
        let huge = "A".repeat((MAX_NOISE_MSG / 3 + 1) * 4 + 4);
        assert!(matches!(
            decode_env_payload(&huge),
            Err(RelayError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn decode_env_payload_rejects_bad_base64() {
        assert!(matches!(
            decode_env_payload("!!!not-base64!!!"),
            Err(RelayError::BadPayload(_))
        ));
    }

    #[test]
    fn is_terminal_classification() {
        assert!(is_terminal("BAD_AUTH"));
        assert!(is_terminal("AGENT_OCCUPIED"));
        assert!(is_terminal("FRAME_TOO_LARGE"));
        assert!(!is_terminal("NO_PEER"));
        assert!(!is_terminal("UNKNOWN_DST"));
        // Unknown codes fall on the transient (keep-waiting) side.
        assert!(!is_terminal("SOME_FUTURE_CODE"));
    }
}
