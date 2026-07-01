//! txArmGrant.clientSig conformance test (e2e-auth.v1), drift-guarded.
//!
//! source: dispensa contracts/auth/tx-arm-grant.vectors.v1.json — keep in sync.
//!
//! Cross-implementation interop: cqdx / pancetta (Rust) / panino (TS) MUST all
//! reproduce `canonicalBytesHex` byte-for-byte and verify `clientSig` over it.
//! Canonicalization: UTF-8 of JSON over every present grant field EXCEPT
//! clientSig, object keys sorted ascending (ASCII lexicographic == Rust
//! `BTreeMap<String>` UTF-8 byte order for ASCII keys), no whitespace, integers
//! as plain JSON numbers. `serde_json::to_vec` over a `BTreeMap<String, Value>`
//! reproduces exactly this.

use std::collections::BTreeMap;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/tx-arm-grant.vectors.v1.json");

/// Decode unpadded base64url (`-`→`+`, `_`→`/`, re-pad to a multiple of 4).
fn decode_b64url(input: &str) -> Vec<u8> {
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
        .decode(s)
        .expect("valid base64url")
}

/// Collect the fixture's `grant` object into a sorted `BTreeMap`, then serialize
/// to the canonical byte form (no whitespace, plain integers, sorted keys).
fn canonical_bytes(grant: &serde_json::Map<String, Value>) -> Vec<u8> {
    // Explicit BTreeMap so key order is guaranteed sorted regardless of any
    // serde_json `preserve_order` feature state elsewhere in the workspace.
    let sorted: BTreeMap<String, Value> =
        grant.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_json::to_vec(&sorted).expect("serialize canonical grant")
}

fn load() -> Value {
    serde_json::from_str(FIXTURE).expect("parse fixture JSON")
}

#[test]
fn canonical_bytes_reproduce_vector() {
    let vector = load();
    let grant = vector["grant"].as_object().expect("grant is an object");
    let expected_hex = vector["canonicalBytesHex"]
        .as_str()
        .expect("canonicalBytesHex is a string");

    let canon = canonical_bytes(grant);
    let got_hex = hex::encode(&canon);

    assert_eq!(
        got_hex, expected_hex,
        "canonical bytes diverged from vector\n  got:  {got_hex}\n  want: {expected_hex}"
    );
}

#[test]
fn client_sig_verifies_over_canonical_bytes() {
    let vector = load();
    let grant = vector["grant"].as_object().expect("grant is an object");

    let canon = canonical_bytes(grant);

    let pk_raw = decode_b64url(
        vector["clientPublicKeyRaw"]
            .as_str()
            .expect("clientPublicKeyRaw is a string"),
    );
    let sig_raw = decode_b64url(vector["clientSig"].as_str().expect("clientSig is a string"));

    let pk_bytes: [u8; 32] = pk_raw
        .as_slice()
        .try_into()
        .expect("public key is 32 bytes");
    let sig_bytes: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .expect("signature is 64 bytes");

    let vk = VerifyingKey::from_bytes(&pk_bytes).expect("valid ed25519 verifying key");
    let sig = Signature::from_bytes(&sig_bytes);

    vk.verify_strict(&canon, &sig)
        .expect("clientSig must verify over canonical bytes");
}

#[test]
fn mutated_grant_fails_verification() {
    let vector = load();
    let mut grant = vector["grant"]
        .as_object()
        .expect("grant is an object")
        .clone();

    // The signature is bound to the exact canonical bytes: any field change
    // (here, the operator callsign) must break verification.
    grant.insert(
        "operatorCallsign".to_string(),
        Value::String("W1AW".to_string()),
    );
    let canon = canonical_bytes(&grant);

    let pk_raw = decode_b64url(
        vector["clientPublicKeyRaw"]
            .as_str()
            .expect("clientPublicKeyRaw is a string"),
    );
    let sig_raw = decode_b64url(vector["clientSig"].as_str().expect("clientSig is a string"));

    let pk_bytes: [u8; 32] = pk_raw
        .as_slice()
        .try_into()
        .expect("public key is 32 bytes");
    let sig_bytes: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .expect("signature is 64 bytes");

    let vk = VerifyingKey::from_bytes(&pk_bytes).expect("valid ed25519 verifying key");
    let sig = Signature::from_bytes(&sig_bytes);

    assert!(
        vk.verify_strict(&canon, &sig).is_err(),
        "verification must FAIL for a mutated grant"
    );
}
