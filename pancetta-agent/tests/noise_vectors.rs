//! Noise IK interop conformance test.
//!
//! Drives a `Noise_IK_25519_ChaChaPoly_BLAKE2s` handshake + transport phase
//! with fixed (test-only) ephemeral keys and asserts every emitted byte
//! against the shared cross-implementation interop vector. This is the single
//! byte-level oracle that closes the JS<->Rust interop risk for the encrypted
//! rig-control channel (dispensa ADR-0002 / e2e-auth.v1): cqdx (@cqdx/noise),
//! panino (NoiseSession), and pancetta (snow) MUST all reproduce it.
//!
//! source: dispensa contracts/auth/noise-ik.vectors.v1.json — keep in sync
//! (the fixture under tests/fixtures/ is a VENDORED copy; do not edit it).

use serde_json::Value;

const VECTOR_JSON: &str = include_str!("fixtures/noise-ik.vectors.v1.json");
const PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

fn hexs(v: &Value, key: &str) -> Vec<u8> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing/non-string field `{key}` in vector"));
    hex::decode(s).unwrap_or_else(|e| panic!("field `{key}` is not valid hex: {e}"))
}

#[test]
fn ik_vector_byte_for_byte() {
    let root: Value = serde_json::from_str(VECTOR_JSON).expect("fixture is valid JSON");
    let vector = root.get("vector").expect("`vector` object present");

    let prologue = hexs(vector, "init_prologue");
    let init_static = hexs(vector, "init_static");
    let init_eph = hexs(vector, "init_ephemeral");
    let init_remote_static = hexs(vector, "init_remote_static");
    let resp_static = hexs(vector, "resp_static");
    let resp_eph = hexs(vector, "resp_ephemeral");
    let want_handshake_hash = vector
        .get("handshake_hash")
        .and_then(Value::as_str)
        .expect("handshake_hash present")
        .to_string();

    let messages = vector
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array present");

    // Decode all payloads + expected ciphertexts up front.
    let payloads: Vec<Vec<u8>> = messages.iter().map(|m| hexs(m, "payload")).collect();
    let ciphertexts: Vec<Vec<u8>> = messages.iter().map(|m| hexs(m, "ciphertext")).collect();
    assert_eq!(payloads.len(), 6, "vector should carry 6 messages");

    // Fresh Builder per handshake state — `.build_*` consumes params, so
    // re-parse for the responder.
    let mut initiator = snow::Builder::new(PARAMS.parse().expect("params parse"))
        .prologue(&prologue)
        .local_private_key(&init_static)
        .remote_public_key(&init_remote_static)
        .fixed_ephemeral_key_for_testing_only(&init_eph)
        .build_initiator()
        .expect("build initiator");

    let mut responder = snow::Builder::new(PARAMS.parse().expect("params parse"))
        .prologue(&prologue)
        .local_private_key(&resp_static)
        .fixed_ephemeral_key_for_testing_only(&resp_eph)
        .build_responder()
        .expect("build responder");

    // --- Handshake msg1: initiator -> responder ---
    let mut buf = vec![0u8; 4096];
    let n = initiator
        .write_message(&payloads[0], &mut buf)
        .expect("initiator write msg1");
    assert_eq!(
        hex::encode(&buf[..n]),
        hex::encode(&ciphertexts[0]),
        "handshake msg1 ciphertext mismatch (got vs want)"
    );

    let mut rbuf = vec![0u8; 4096];
    let rn = responder
        .read_message(&buf[..n], &mut rbuf)
        .expect("responder read msg1");
    assert_eq!(
        &rbuf[..rn],
        payloads[0].as_slice(),
        "responder decrypted msg1 payload mismatch"
    );

    // --- Handshake msg2: responder -> initiator ---
    let n = responder
        .write_message(&payloads[1], &mut buf)
        .expect("responder write msg2");
    assert_eq!(
        hex::encode(&buf[..n]),
        hex::encode(&ciphertexts[1]),
        "handshake msg2 ciphertext mismatch (got vs want)"
    );

    let rn = initiator
        .read_message(&buf[..n], &mut rbuf)
        .expect("initiator read msg2");
    assert_eq!(
        &rbuf[..rn],
        payloads[1].as_slice(),
        "initiator decrypted msg2 payload mismatch"
    );

    // --- Handshake hash (both sides, BEFORE into_transport_mode) ---
    assert_eq!(
        hex::encode(initiator.get_handshake_hash()),
        want_handshake_hash,
        "initiator handshake_hash mismatch (got vs want)"
    );
    assert_eq!(
        hex::encode(responder.get_handshake_hash()),
        want_handshake_hash,
        "responder handshake_hash mismatch (got vs want)"
    );

    // --- Transport phase ---
    let mut initiator = initiator
        .into_transport_mode()
        .expect("initiator into transport");
    let mut responder = responder
        .into_transport_mode()
        .expect("responder into transport");

    // messages[2]: initiator -> responder (init send nonce = 0)
    transport_step(
        &mut initiator,
        &mut responder,
        &payloads[2],
        &ciphertexts[2],
        "msg2 init->resp",
    );
    // messages[3]: responder -> initiator (resp send nonce = 0)
    transport_step(
        &mut responder,
        &mut initiator,
        &payloads[3],
        &ciphertexts[3],
        "msg3 resp->init",
    );
    // messages[4]: initiator -> responder (init send nonce = 1)
    transport_step(
        &mut initiator,
        &mut responder,
        &payloads[4],
        &ciphertexts[4],
        "msg4 init->resp n=1",
    );
    // messages[5]: responder -> initiator (resp send nonce = 1)
    transport_step(
        &mut responder,
        &mut initiator,
        &payloads[5],
        &ciphertexts[5],
        "msg5 resp->init n=1",
    );
}

/// One transport-mode message: `sender` encrypts `payload`, assert the
/// ciphertext byte-for-byte, then `receiver` decrypts and recovers `payload`.
fn transport_step(
    sender: &mut snow::TransportState,
    receiver: &mut snow::TransportState,
    payload: &[u8],
    want_ciphertext: &[u8],
    label: &str,
) {
    let mut buf = vec![0u8; 4096];
    let n = sender
        .write_message(payload, &mut buf)
        .unwrap_or_else(|e| panic!("{label}: sender write_message failed: {e}"));
    assert_eq!(
        hex::encode(&buf[..n]),
        hex::encode(want_ciphertext),
        "{label}: transport ciphertext mismatch (got vs want)"
    );

    let mut rbuf = vec![0u8; 4096];
    let rn = receiver
        .read_message(&buf[..n], &mut rbuf)
        .unwrap_or_else(|e| panic!("{label}: receiver read_message failed: {e}"));
    assert_eq!(
        &rbuf[..rn],
        payload,
        "{label}: receiver decrypted payload mismatch"
    );
}
