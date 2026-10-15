//! Client identity proof carried in the Noise IK msg1 payload (PAN-95;
//! proposed e2e-auth.v1 amendment — see
//! docs/superpowers/specs/2026-10-12-client-identity-binding-design.md).
//!
//! Noise IK authenticates the initiator's static X25519 key, but not who owns
//! it. The client closes that gap by putting its pairing.v1 identity triple in
//! msg1: its Ed25519 identity public key plus the enrollment `idSig` (Ed25519
//! over `"cqdx-pair-idsig-v1" ‖ 0x00 ‖ agreementPublicKeyRaw`). The station
//! verifies `idSig` over the key the handshake ACTUALLY authenticated and names
//! the peer by the keyId derived from the proven identity key. keyIds are
//! self-certifying, so no registry or provisioning is needed.
//!
//! Wire format: UTF-8 JSON, at most [`MAX_CLIENT_IDENTITY_PROOF_LEN`] bytes,
//! `{"v":1,"identityPublicKey":"<b64url 32B>","idSig":"<b64url 64B>"}`; unknown
//! fields are ignored, any `v` other than 1 is refused.

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::keys::key_id_from_identity_public;
use crate::pairing::IDSIG_TAG;

/// The only proof format version this station accepts.
pub const CLIENT_IDENTITY_PROOF_VERSION: u8 = 1;

/// Upper bound on the msg1 proof payload, checked before parsing.
pub const MAX_CLIENT_IDENTITY_PROOF_LEN: usize = 1024;

/// The msg1 payload DTO (camelCase, matching pairing.v1 field names).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentityProof {
    /// Proof format version; must be [`CLIENT_IDENTITY_PROOF_VERSION`].
    pub v: u8,
    /// Raw 32-byte Ed25519 identity public key, unpadded base64url.
    pub identity_public_key: String,
    /// Raw 64-byte pairing.v1 `idSig`, unpadded base64url.
    pub id_sig: String,
}

/// Why a msg1 payload does not prove a client identity.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PeerIdentityError {
    /// The handshake carried no proof (empty msg1 payload).
    #[error("no client identity proof in the handshake")]
    Missing,
    /// The payload is not a well-formed proof.
    #[error("client identity proof malformed: {0}")]
    Malformed(&'static str),
    /// The proof's `v` is not one this station understands.
    #[error("unsupported client identity proof version {0}")]
    UnsupportedVersion(u8),
    /// `idSig` does not verify over the handshake-authenticated static key.
    #[error("client identity proof does not bind the handshake key")]
    BadSignature,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode an unpadded base64url field into exactly `N` bytes.
fn decode_exact<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], PeerIdentityError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|_| PeerIdentityError::Malformed(what))?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| PeerIdentityError::Malformed(what))
}

/// Encode a proof (reference encoder for clients; also used by tests).
pub fn encode_client_identity_proof(identity_public_raw: &[u8; 32], id_sig: &[u8; 64]) -> Vec<u8> {
    let proof = ClientIdentityProof {
        v: CLIENT_IDENTITY_PROOF_VERSION,
        identity_public_key: b64url(identity_public_raw),
        id_sig: b64url(id_sig),
    };
    serde_json::to_vec(&proof).expect("a struct of strings always serializes")
}

/// Verify `payload` against the handshake-authenticated `remote_static` and
/// return the PROVEN keyId.
///
/// Order: empty → `Missing`; > MAX → `Malformed`; JSON → `Malformed`;
/// `v != 1` → `UnsupportedVersion`; b64url decode + exact 32/64-byte lengths +
/// `VerifyingKey::from_bytes` → `Malformed`; `verify_strict` over
/// `IDSIG_TAG ‖ 0x00 ‖ remote_static` → `BadSignature`.
pub fn verify_client_identity_proof(
    payload: &[u8],
    remote_static: &[u8; 32],
) -> Result<String, PeerIdentityError> {
    if payload.is_empty() {
        return Err(PeerIdentityError::Missing);
    }
    if payload.len() > MAX_CLIENT_IDENTITY_PROOF_LEN {
        return Err(PeerIdentityError::Malformed("proof exceeds 1024 bytes"));
    }
    let proof: ClientIdentityProof = serde_json::from_slice(payload)
        .map_err(|_| PeerIdentityError::Malformed("not a client identity proof object"))?;
    if proof.v != CLIENT_IDENTITY_PROOF_VERSION {
        return Err(PeerIdentityError::UnsupportedVersion(proof.v));
    }
    let identity_public: [u8; 32] = decode_exact(
        &proof.identity_public_key,
        "identityPublicKey is not 32 bytes of base64url",
    )?;
    let id_sig: [u8; 64] = decode_exact(&proof.id_sig, "idSig is not 64 bytes of base64url")?;
    let vk = VerifyingKey::from_bytes(&identity_public)
        .map_err(|_| PeerIdentityError::Malformed("identityPublicKey is not an Ed25519 point"))?;

    // Same layout as `AgentIdentity::sign_domain`: tag ‖ 0x00 ‖ msg, where msg
    // is the key Noise authenticated — NOT anything the client claims.
    let mut msg = Vec::with_capacity(IDSIG_TAG.len() + 1 + remote_static.len());
    msg.extend_from_slice(IDSIG_TAG.as_bytes());
    msg.push(0x00);
    msg.extend_from_slice(remote_static);
    vk.verify_strict(&msg, &Signature::from_bytes(&id_sig))
        .map_err(|_| PeerIdentityError::BadSignature)?;

    Ok(key_id_from_identity_public(&identity_public))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::AgentIdentity;

    fn proof_of(id: &AgentIdentity) -> Vec<u8> {
        let id_sig = id.sign_domain(IDSIG_TAG, &id.agreement_public_raw());
        encode_client_identity_proof(&id.identity_public_raw(), &id_sig)
    }

    #[test]
    fn valid_proof_verifies_and_yields_derived_key_id() {
        let device = AgentIdentity::generate();
        let got = verify_client_identity_proof(&proof_of(&device), &device.agreement_public_raw());
        assert_eq!(got, Ok(device.key_id()));
    }

    #[test]
    fn empty_payload_is_missing() {
        let device = AgentIdentity::generate();
        assert_eq!(
            verify_client_identity_proof(b"", &device.agreement_public_raw()),
            Err(PeerIdentityError::Missing)
        );
    }

    #[test]
    fn proof_replayed_under_a_different_static_is_rejected() {
        let client = AgentIdentity::generate();
        let relay = AgentIdentity::generate();
        // The client's genuine proof, but the handshake authenticated the
        // relay's static key: the idSig does not cover it.
        assert_eq!(
            verify_client_identity_proof(&proof_of(&client), &relay.agreement_public_raw()),
            Err(PeerIdentityError::BadSignature)
        );
    }

    #[test]
    fn proof_from_another_identity_yields_that_identity() {
        let client = AgentIdentity::generate();
        let relay = AgentIdentity::generate();
        let got = verify_client_identity_proof(&proof_of(&relay), &relay.agreement_public_raw())
            .expect("relay's own proof is valid for the relay");
        assert_eq!(got, relay.key_id());
        assert_ne!(
            got,
            client.key_id(),
            "a proof can only name its own identity"
        );
    }

    #[test]
    fn malformed_json_is_rejected() {
        let device = AgentIdentity::generate();
        for bad in [&b"not json"[..], b"{}", b"[1,2]", b"{\"v\":1}"] {
            assert!(
                matches!(
                    verify_client_identity_proof(bad, &device.agreement_public_raw()),
                    Err(PeerIdentityError::Malformed(_))
                ),
                "{:?} must be Malformed",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn wrong_key_or_signature_length_is_rejected() {
        let device = AgentIdentity::generate();
        let rs = device.agreement_public_raw();
        let id_sig = device.sign_domain(IDSIG_TAG, &rs);
        let short_key = serde_json::json!({
            "v": 1,
            "identityPublicKey": b64url(&device.identity_public_raw()[..31]),
            "idSig": b64url(&id_sig),
        });
        let short_sig = serde_json::json!({
            "v": 1,
            "identityPublicKey": b64url(&device.identity_public_raw()),
            "idSig": b64url(&id_sig[..63]),
        });
        for bad in [short_key, short_sig] {
            let bytes = serde_json::to_vec(&bad).unwrap();
            assert!(
                matches!(
                    verify_client_identity_proof(&bytes, &rs),
                    Err(PeerIdentityError::Malformed(_))
                ),
                "{bad} must be Malformed"
            );
        }
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let device = AgentIdentity::generate();
        let mut proof: serde_json::Value = serde_json::from_slice(&proof_of(&device)).unwrap();
        proof["v"] = serde_json::json!(2);
        let bytes = serde_json::to_vec(&proof).unwrap();
        assert_eq!(
            verify_client_identity_proof(&bytes, &device.agreement_public_raw()),
            Err(PeerIdentityError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let device = AgentIdentity::generate();
        let mut payload = proof_of(&device);
        // Pad a valid proof with trailing JSON whitespace up to MAX + 1 bytes:
        // still valid JSON, but over the length limit.
        payload.resize(MAX_CLIENT_IDENTITY_PROOF_LEN + 1, b' ');
        assert_eq!(payload.len(), 1025);
        assert!(matches!(
            verify_client_identity_proof(&payload, &device.agreement_public_raw()),
            Err(PeerIdentityError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let device = AgentIdentity::generate();
        let mut proof: serde_json::Value = serde_json::from_slice(&proof_of(&device)).unwrap();
        proof["client"] = serde_json::json!("x");
        let bytes = serde_json::to_vec(&proof).unwrap();
        assert_eq!(
            verify_client_identity_proof(&bytes, &device.agreement_public_raw()),
            Ok(device.key_id())
        );
    }

    /// Pins the `## Example` section of the design spec to real code.
    #[test]
    fn fixed_seed_example_matches_spec() {
        let device = AgentIdentity::from_secret_bytes([0x22; 32], [0x23; 32]);
        let payload = proof_of(&device);
        let want = r#"{"v":1,"identityPublicKey":"oJql9HpnWYAv-VX43C0qFKXJnSO-l_hkEn_5ODRVpPA","idSig":"tjDTm307i52bJPpXjlGKKS5p2Qvj8p0dFgQgJ9eNKwyq0GOMSAv4fMnJu7u_2wG4bsFIVFa1E_u44qKh_XaLCQ"}"#;
        assert_eq!(String::from_utf8(payload.clone()).unwrap(), want);
        assert_eq!(payload.len(), 170);
        assert_eq!(
            b64url(&device.agreement_public_raw()),
            "mkUDqYqxD-jTVMnELL0MnXlE9S59FNjqWXdefcnjv0s"
        );
        assert_eq!(
            verify_client_identity_proof(&payload, &device.agreement_public_raw()),
            Ok("dE82zKZ-sZEssoLgjoa9L-DQkASijPbnxr1Wzda_Zao".to_string())
        );
    }
}
