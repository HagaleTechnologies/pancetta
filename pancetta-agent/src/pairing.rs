//! Agent pairing/enrollment client for the cqdx `pairing.v1` contract.
//!
//! Flow (matches `contracts/pairing/pairing.v1.schema.json`):
//!   1. `POST /pair/agent` with `agentEnrollRequest` → `enrollChallenge`
//!      (`{challengeId, nonce}`).
//!   2. `POST /pair/agent/complete` with `enrollCompleteRequest`
//!      (`{challengeId, keyId, signature}`) → `agentEnrollCompleteResponse`
//!      (`{agentKeyId, idpKeys}`).
//!
//! Signatures (domain-separated per the schema):
//!   - `idSig` = Ed25519 by the identity key over
//!     `domainSeparate("cqdx-pair-idsig-v1", agreementPublicKeyBytes)`.
//!   - PoP `signature` = Ed25519 by the identity key over
//!     `domainSeparate("cqdx-pair-challenge-v1", utf8(nonce))`.
//!
//! On complete, the returned `idpKeys` are validated as a pin set (non-empty,
//! all `alg == "Ed25519"`, each `publicKey` decodes to exactly 32 bytes) — an
//! invalid set is an all-or-nothing rejection (no partial pin).

use std::fs;
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::keys::AgentIdentity;

/// Domain-separation tag for the identity→agreement binding signature.
pub const IDSIG_TAG: &str = "cqdx-pair-idsig-v1";
/// Domain-separation tag for the enrollment proof-of-possession signature.
pub const POP_TAG: &str = "cqdx-pair-challenge-v1";

const PAIRED_STATE_FILE: &str = "paired.json";

/// Errors from the pairing/enrollment flow.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    /// The transport (HTTP seam) failed.
    #[error("pairing transport error: {0}")]
    Transport(String),
    /// A server response could not be parsed into the expected shape.
    #[error("malformed pairing response: {0}")]
    MalformedResponse(String),
    /// The returned idpKeys pin set was invalid (empty, wrong alg, or a key
    /// that did not decode to 32 bytes). No partial pin is retained.
    #[error("invalid idpKeys pin set: {0}")]
    InvalidIdpKeys(String),
    /// Filesystem I/O for PairedState persistence.
    #[error("paired-state I/O error: {0}")]
    Io(String),
    /// PairedState JSON (de)serialization.
    #[error("paired-state serialization error: {0}")]
    Serde(String),
}

/// Encode bytes as unpadded base64url.
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Tolerantly decode unpadded (or padded) base64url into bytes.
fn decode_b64url(input: &str) -> Result<Vec<u8>, PairingError> {
    let mut s: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(&s)
        .map_err(|e| PairingError::MalformedResponse(format!("base64url decode: {e}")))
}

// ---------------------------------------------------------------------------
// Wire DTOs (camelCase, per pairing.v1)
// ---------------------------------------------------------------------------

/// `POST /pair/agent` request body (`agentEnrollRequest`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnrollRequest {
    /// Single-use pairing code from the cqdx web UI.
    pub pairing_code: String,
    /// Agent keyId (SPKI SHA-256, unpadded base64url).
    pub key_id: String,
    /// Ed25519 identity public key, 32 bytes, unpadded base64url.
    pub identity_public_key: String,
    /// X25519 static agreement public key, 32 bytes, unpadded base64url.
    pub agreement_public_key: String,
    /// Ed25519 idSig over the domain-separated agreement key, unpadded base64url.
    pub id_sig: String,
    /// Optional human-readable agent name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional platform identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// `enrollChallenge` response (`{challengeId, nonce}`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollChallenge {
    /// Opaque server-assigned pending-challenge id.
    pub challenge_id: String,
    /// Random challenge bytes (unpadded base64url) to sign as PoP.
    pub nonce: String,
}

/// `POST /pair/agent/complete` request body (`enrollCompleteRequest`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollCompleteRequest {
    /// The challengeId from [`EnrollChallenge`].
    pub challenge_id: String,
    /// The agent's keyId (server recomputes + rejects on mismatch).
    pub key_id: String,
    /// PoP signature over the domain-separated nonce, unpadded base64url.
    pub signature: String,
}

/// A single IdP public key entry in the pin set (`idpKeyEntry`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdpKeyEntry {
    /// Key identifier (base64url fingerprint).
    pub kid: String,
    /// Raw Ed25519 public key (32 bytes), unpadded base64url.
    pub public_key: String,
    /// Key algorithm; constrained to `"Ed25519"` in v1.
    pub alg: String,
}

/// `agentEnrollCompleteResponse` (`{agentKeyId, idpKeys}`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnrollCompleteResponse {
    /// The enrolled agent's keyId (confirmed by the server).
    pub agent_key_id: String,
    /// The IdP public-key pin set.
    pub idp_keys: Vec<IdpKeyEntry>,
}

/// `pairingCodeResponse` (`{code, prefix}`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingCodeResponse {
    /// The single-use pairing code to display.
    pub code: String,
    /// A short prefix for display disambiguation.
    pub prefix: String,
}

// ---------------------------------------------------------------------------
// Persisted paired state (public data only — NO secrets)
// ---------------------------------------------------------------------------

/// A validated, decoded IdP pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdpKey {
    /// Key identifier.
    pub kid: String,
    /// Raw 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
}

/// The result of a successful enrollment: the agent's keyId and the pinned IdP
/// key set. Persisted (public-only) so pins survive restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedState {
    /// The enrolled agent's keyId.
    pub agent_key_id: String,
    /// The pinned IdP public keys.
    pub idp_keys: Vec<IdpKey>,
}

impl PairedState {
    /// Persist to `dir/paired.json`. Public data only; contains no secrets.
    pub fn persist(&self, dir: &Path) -> Result<(), PairingError> {
        if !dir.exists() {
            fs::create_dir_all(dir).map_err(|e| PairingError::Io(e.to_string()))?;
        }
        let path = dir.join(PAIRED_STATE_FILE);
        let json =
            serde_json::to_vec_pretty(self).map_err(|e| PairingError::Serde(e.to_string()))?;
        fs::write(&path, json).map_err(|e| PairingError::Io(e.to_string()))?;
        Ok(())
    }

    /// Load from `dir/paired.json`.
    pub fn load(dir: &Path) -> Result<Self, PairingError> {
        let path = dir.join(PAIRED_STATE_FILE);
        let bytes = fs::read(&path).map_err(|e| PairingError::Io(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| PairingError::Serde(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// HTTP seam + client
// ---------------------------------------------------------------------------

/// A minimal, synchronous HTTP seam so the pairing client is trivially
/// mockable. The real implementation (P3.4) may block or wrap an async client.
pub trait PairingHttp {
    /// POST `body` to `path`, returning the parsed JSON response.
    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, PairingError>;
}

/// The agent-side pairing/enrollment client.
pub struct PairingClient<H: PairingHttp> {
    http: H,
    identity: AgentIdentity,
}

impl<H: PairingHttp> PairingClient<H> {
    /// Construct a client over an HTTP seam and an agent identity.
    pub fn new(http: H, identity: AgentIdentity) -> Self {
        Self { http, identity }
    }

    /// Run the full agent enrollment flow and return the validated pin set.
    pub fn enroll(
        &self,
        pairing_code: &str,
        name: Option<String>,
        platform: Option<String>,
    ) -> Result<PairedState, PairingError> {
        let key_id = self.identity.key_id();
        let id_pub = self.identity.identity_public_raw();
        let agr_pub = self.identity.agreement_public_raw();

        // idSig binds the agreement key to the identity key (domain-separated).
        let id_sig = self.identity.sign_domain(IDSIG_TAG, &agr_pub);

        let req = AgentEnrollRequest {
            pairing_code: pairing_code.to_string(),
            key_id: key_id.clone(),
            identity_public_key: b64url(&id_pub),
            agreement_public_key: b64url(&agr_pub),
            id_sig: b64url(&id_sig),
            name,
            platform,
        };
        let req_val = serde_json::to_value(&req).map_err(|e| PairingError::Serde(e.to_string()))?;

        let challenge_val = self.http.post("/pair/agent", req_val)?;
        let challenge: EnrollChallenge = serde_json::from_value(challenge_val)
            .map_err(|e| PairingError::MalformedResponse(e.to_string()))?;

        // PoP: sign the nonce (domain-separated). The nonce is a base64url
        // string; the schema signs utf8(nonce) — the string bytes as sent.
        let pop = self
            .identity
            .sign_domain(POP_TAG, challenge.nonce.as_bytes());

        let complete_req = EnrollCompleteRequest {
            challenge_id: challenge.challenge_id,
            key_id,
            signature: b64url(&pop),
        };
        let complete_val =
            serde_json::to_value(&complete_req).map_err(|e| PairingError::Serde(e.to_string()))?;

        let resp_val = self.http.post("/pair/agent/complete", complete_val)?;
        let resp: AgentEnrollCompleteResponse = serde_json::from_value(resp_val)
            .map_err(|e| PairingError::MalformedResponse(e.to_string()))?;

        let idp_keys = validate_idp_keys(&resp.idp_keys)?;

        Ok(PairedState {
            agent_key_id: resp.agent_key_id,
            idp_keys,
        })
    }
}

/// Validate the IdP pin set: non-empty, every `alg == "Ed25519"`, every
/// `publicKey` decodes to exactly 32 bytes. All-or-nothing — a single bad entry
/// rejects the whole set (no partial pin).
fn validate_idp_keys(entries: &[IdpKeyEntry]) -> Result<Vec<IdpKey>, PairingError> {
    if entries.is_empty() {
        return Err(PairingError::InvalidIdpKeys(
            "idpKeys pin set is empty".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if e.alg != "Ed25519" {
            return Err(PairingError::InvalidIdpKeys(format!(
                "unsupported alg {:?} for kid {:?}",
                e.alg, e.kid
            )));
        }
        let raw = decode_b64url(&e.public_key).map_err(|_| {
            PairingError::InvalidIdpKeys(format!("publicKey not base64url for kid {:?}", e.kid))
        })?;
        let public_key: [u8; 32] = raw.try_into().map_err(|v: Vec<u8>| {
            PairingError::InvalidIdpKeys(format!(
                "publicKey for kid {:?} is {} bytes, expected 32",
                e.kid,
                v.len()
            ))
        })?;
        out.push(IdpKey {
            kid: e.kid.clone(),
            public_key,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A mock HTTP seam: returns queued responses by path and captures the
    /// bodies that were POSTed.
    struct MockHttp {
        responses: HashMap<String, serde_json::Value>,
        captured: RefCell<Vec<(String, serde_json::Value)>>,
    }

    impl MockHttp {
        fn new(responses: HashMap<String, serde_json::Value>) -> Self {
            Self {
                responses,
                captured: RefCell::new(Vec::new()),
            }
        }

        fn body_for(&self, path: &str) -> serde_json::Value {
            self.captured
                .borrow()
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, b)| b.clone())
                .unwrap_or_else(|| panic!("no body captured for {path}"))
        }
    }

    impl PairingHttp for MockHttp {
        fn post(
            &self,
            path: &str,
            body: serde_json::Value,
        ) -> Result<serde_json::Value, PairingError> {
            self.captured.borrow_mut().push((path.to_string(), body));
            self.responses
                .get(path)
                .cloned()
                .ok_or_else(|| PairingError::Transport(format!("no mock response for {path}")))
        }
    }

    fn decode_test_b64url(s: &str) -> Vec<u8> {
        decode_b64url(s).unwrap()
    }

    fn good_responses() -> HashMap<String, serde_json::Value> {
        let mut m = HashMap::new();
        m.insert(
            "/pair/agent".to_string(),
            serde_json::json!({ "challengeId": "chal-123", "nonce": "bm9uY2UtYWJj" }),
        );
        m.insert(
            "/pair/agent/complete".to_string(),
            serde_json::json!({
                "agentKeyId": "agent-key-id-xyz",
                "idpKeys": [
                    { "kid": "idp-1", "publicKey": b64url(&[7u8; 32]), "alg": "Ed25519" },
                    { "kid": "idp-2", "publicKey": b64url(&[9u8; 32]), "alg": "Ed25519" }
                ]
            }),
        );
        m
    }

    #[test]
    fn happy_path_enroll_returns_pinned_idp_keys() {
        let http = MockHttp::new(good_responses());
        let identity = AgentIdentity::generate();
        let client = PairingClient::new(http, identity);

        let state = client
            .enroll(
                "PAIR-CODE-42",
                Some("K5ARH Rig".into()),
                Some("pancetta-macos".into()),
            )
            .unwrap();

        assert_eq!(state.agent_key_id, "agent-key-id-xyz");
        assert_eq!(state.idp_keys.len(), 2);
        assert_eq!(state.idp_keys[0].kid, "idp-1");
        assert_eq!(state.idp_keys[0].public_key, [7u8; 32]);
        assert_eq!(state.idp_keys[1].public_key, [9u8; 32]);
    }

    #[test]
    fn enroll_body_carries_domain_separated_idsig() {
        let http = MockHttp::new(good_responses());
        let identity = AgentIdentity::generate();
        let vk = identity.verifying_key();
        let agr_pub = identity.agreement_public_raw();
        let client = PairingClient::new(http, identity);

        let _ = client.enroll("code", None, None).unwrap();

        let body = client.http.body_for("/pair/agent");
        let req: AgentEnrollRequest = serde_json::from_value(body).unwrap();

        // idSig verifies as sign_domain(IDSIG_TAG, agreement_pub).
        let sig_bytes: [u8; 64] = decode_test_b64url(&req.id_sig).try_into().unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        let mut msg = IDSIG_TAG.as_bytes().to_vec();
        msg.push(0x00);
        msg.extend_from_slice(&agr_pub);
        assert!(
            vk.verify(&msg, &sig).is_ok(),
            "idSig must verify over domain-sep agreement key"
        );

        // The agreement public key in the body matches.
        assert_eq!(
            decode_test_b64url(&req.agreement_public_key),
            agr_pub.to_vec()
        );
        // keyId in body matches identity.
        assert_eq!(req.key_id, client.identity.key_id());
    }

    #[test]
    fn complete_body_signature_is_pop_over_nonce() {
        let http = MockHttp::new(good_responses());
        let identity = AgentIdentity::generate();
        let vk = identity.verifying_key();
        let client = PairingClient::new(http, identity);

        let _ = client.enroll("code", None, None).unwrap();

        let body = client.http.body_for("/pair/agent/complete");
        let req: EnrollCompleteRequest = serde_json::from_value(body).unwrap();
        assert_eq!(req.challenge_id, "chal-123");

        let sig_bytes: [u8; 64] = decode_test_b64url(&req.signature).try_into().unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        // PoP is over utf8(nonce) = the nonce string bytes as returned.
        let nonce = "bm9uY2UtYWJj";
        let mut msg = POP_TAG.as_bytes().to_vec();
        msg.push(0x00);
        msg.extend_from_slice(nonce.as_bytes());
        assert!(
            vk.verify(&msg, &sig).is_ok(),
            "PoP must verify over domain-sep nonce"
        );
    }

    #[test]
    fn idp_key_wrong_length_rejects_without_pin() {
        let mut r = good_responses();
        r.insert(
            "/pair/agent/complete".to_string(),
            serde_json::json!({
                "agentKeyId": "a",
                "idpKeys": [ { "kid": "k", "publicKey": b64url(&[1u8; 16]), "alg": "Ed25519" } ]
            }),
        );
        let client = PairingClient::new(MockHttp::new(r), AgentIdentity::generate());
        let err = client.enroll("code", None, None).unwrap_err();
        assert!(
            matches!(err, PairingError::InvalidIdpKeys(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn idp_key_wrong_alg_rejects_without_pin() {
        let mut r = good_responses();
        r.insert(
            "/pair/agent/complete".to_string(),
            serde_json::json!({
                "agentKeyId": "a",
                "idpKeys": [ { "kid": "k", "publicKey": b64url(&[1u8; 32]), "alg": "P-256" } ]
            }),
        );
        let client = PairingClient::new(MockHttp::new(r), AgentIdentity::generate());
        let err = client.enroll("code", None, None).unwrap_err();
        assert!(
            matches!(err, PairingError::InvalidIdpKeys(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn empty_idp_key_set_rejects() {
        let mut r = good_responses();
        r.insert(
            "/pair/agent/complete".to_string(),
            serde_json::json!({ "agentKeyId": "a", "idpKeys": [] }),
        );
        let client = PairingClient::new(MockHttp::new(r), AgentIdentity::generate());
        let err = client.enroll("code", None, None).unwrap_err();
        assert!(
            matches!(err, PairingError::InvalidIdpKeys(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn paired_state_round_trips() {
        let dir = {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "pancetta-agent-pairing-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            p
        };
        let state = PairedState {
            agent_key_id: "agent-abc".to_string(),
            idp_keys: vec![
                IdpKey {
                    kid: "k1".into(),
                    public_key: [3u8; 32],
                },
                IdpKey {
                    kid: "k2".into(),
                    public_key: [4u8; 32],
                },
            ],
        };
        state.persist(&dir).unwrap();
        let loaded = PairedState::load(&dir).unwrap();
        assert_eq!(state, loaded);

        let _ = fs::remove_dir_all(&dir);
    }
}
