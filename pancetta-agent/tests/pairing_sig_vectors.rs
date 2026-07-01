//! Pairing identity-signature conformance test (pairing.v1), drift-guarded.
//!
//! source: dispensa contracts/auth/pairing-sig.vectors.v1.json — keep in sync
//! (the fixture under tests/fixtures/ is a VENDORED copy; do not edit it).
//!
//! Cross-implementation interop: cqdx / panino (TS) / pancetta (Rust) MUST all
//! reproduce `keyId` plus the idSig and PoP **signed payloads** byte-for-byte
//! and verify `idSig`/`pop` over them under the identity public key. This test
//! locks pancetta against drift from the shared oracle.
//!
//! Domain separation: `domainSeparate(tag, payload) = utf8(tag) ‖ 0x00 ‖ payload`.
//!   - idSig tag = "cqdx-pair-idsig-v1", payload = RAW 32-byte X25519 agreement
//!     public key (NOT its base64url string).
//!   - PoP tag = "cqdx-pair-challenge-v1", payload = UTF-8 bytes of the
//!     base64url nonce STRING (NOT the decoded nonce bytes).
//!
//! This guards the REAL `pancetta_agent::keys::AgentIdentity`: the fixture's
//! identity seed (0x01×32) and agreement private key (0x02×32) are written to a
//! temp keystore and loaded via `AgentIdentity::load_or_generate`, so
//! `key_id()` and `sign_domain()` are exercised on the production impl (there is
//! no from-seed public ctor; the file path is the only seam that seeds it).

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use pancetta_agent::keys::AgentIdentity;

const VECTOR_JSON: &str = include_str!("fixtures/pairing-sig.vectors.v1.json");

const IDSIG_TAG: &str = "cqdx-pair-idsig-v1";
const POP_TAG: &str = "cqdx-pair-challenge-v1";
/// Ed25519 SPKI DER prefix (`302a300506032b6570032100`) that keyId hashes over.
const ED25519_SPKI_PREFIX_HEX: &str = "302a300506032b6570032100";

fn strf<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    let mut cur = v;
    for k in path {
        cur = cur
            .get(k)
            .unwrap_or_else(|| panic!("missing field `{}` in vector", path.join(".")));
    }
    cur.as_str()
        .unwrap_or_else(|| panic!("field `{}` is not a string", path.join(".")))
}

fn hexb(v: &Value, path: &[&str]) -> Vec<u8> {
    hex::decode(strf(v, path)).unwrap_or_else(|e| panic!("`{}` not hex: {e}", path.join(".")))
}

/// Unpadded, padding-tolerant base64url decode (re-pads before decoding).
fn b64url_decode(s: &str) -> Vec<u8> {
    let padded = {
        let mut t = s.to_string();
        while !t.len().is_multiple_of(4) {
            t.push('=');
        }
        t
    };
    base64::engine::general_purpose::URL_SAFE
        .decode(padded.as_bytes())
        .unwrap_or_else(|e| panic!("base64url decode failed for `{s}`: {e}"))
}

fn b64url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `domainSeparate(tag, payload) = utf8(tag) ‖ 0x00 ‖ payload`.
fn domain_separate(tag: &str, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(tag.len() + 1 + payload.len());
    buf.extend_from_slice(tag.as_bytes());
    buf.push(0x00);
    buf.extend_from_slice(payload);
    buf
}

/// Load a real `AgentIdentity` seeded from raw private key bytes, by writing the
/// key files `load_or_generate` reads (identity.key = Ed25519 seed,
/// agreement.key = X25519 static secret). Returns the identity + the temp dir
/// path (caller cleans up).
fn seeded_identity(
    id_seed: &[u8; 32],
    agr_secret: &[u8; 32],
) -> (AgentIdentity, std::path::PathBuf) {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "pancetta-pairing-sig-vec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("identity.key"), id_seed).unwrap();
    std::fs::write(dir.join("agreement.key"), agr_secret).unwrap();
    let id = AgentIdentity::load_or_generate(&dir).expect("load seeded identity");
    (id, dir)
}

#[test]
fn pairing_sig_vector_byte_for_byte() {
    let root: Value = serde_json::from_str(VECTOR_JSON).expect("fixture is valid JSON");

    // --- Fixture values ---------------------------------------------------
    let id_seed: [u8; 32] = hexb(&root, &["testIdentity", "identityPrivateSeedHex"])
        .try_into()
        .expect("identity seed is 32 bytes");
    let agr_secret: [u8; 32] = hexb(&root, &["testIdentity", "agreementPrivateKeyHex"])
        .try_into()
        .expect("agreement private key is 32 bytes");

    let want_id_pub_hex = strf(&root, &["testIdentity", "identityPublicKeyHex"]);
    let want_id_pub_raw = strf(&root, &["testIdentity", "identityPublicKeyRaw"]);
    let want_key_id = strf(&root, &["testIdentity", "keyId"]);
    let want_agr_pub_raw = strf(&root, &["testIdentity", "agreementPublicKeyRaw"]);

    let want_idsig_payload_hex = strf(&root, &["idSig", "signedPayloadHex"]);
    let idsig_sig = b64url_decode(strf(&root, &["idSig", "signature"]));

    let challenge_nonce_b64u = strf(&root, &["pop", "challengeNonceB64u"]);
    let want_pop_payload_hex = strf(&root, &["pop", "signedPayloadHex"]);
    let pop_sig = b64url_decode(strf(&root, &["pop", "signature"]));

    // Raw 32-byte agreement public key (idSig signs OVER this, not its string).
    let agreement_pub_raw = b64url_decode(want_agr_pub_raw);
    assert_eq!(
        agreement_pub_raw.len(),
        32,
        "agreement pubkey must be 32 bytes"
    );

    // === Standalone derivation (matches the shared conventions) ===========
    // Identity public key from the 0x01×32 seed.
    let signing = ed25519_dalek::SigningKey::from_bytes(&id_seed);
    let vk: VerifyingKey = signing.verifying_key();
    let id_pub = vk.to_bytes();
    assert_eq!(
        hex::encode(id_pub),
        want_id_pub_hex,
        "identity public key (hex) must match fixture"
    );
    assert_eq!(
        b64url_encode(&id_pub),
        want_id_pub_raw,
        "identity public key (b64url) must match fixture"
    );

    // keyId = unpadded-b64url(SHA-256(SPKI-DER-prefix ‖ raw ed25519 pub)).
    let key_id = {
        use sha2::{Digest, Sha256};
        let mut spki = hex::decode(ED25519_SPKI_PREFIX_HEX).unwrap();
        spki.extend_from_slice(&id_pub);
        b64url_encode(&Sha256::digest(&spki))
    };
    assert_eq!(key_id, want_key_id, "keyId must match fixture");
    assert_eq!(
        key_id, "_RENMB0vB33hQUuPmfRBsUA_qyB7IFL70sBl5O6OfcI",
        "keyId must match the known-answer anchor"
    );

    // idSig signed payload = dsep("cqdx-pair-idsig-v1", raw agreement pubkey).
    let idsig_payload = domain_separate(IDSIG_TAG, &agreement_pub_raw);
    assert_eq!(
        hex::encode(&idsig_payload),
        want_idsig_payload_hex,
        "idSig signed payload must match fixture"
    );
    // The fixture signature verifies over that payload under the identity key.
    let idsig_signature = Signature::from_bytes(
        &idsig_sig
            .as_slice()
            .try_into()
            .expect("idSig signature is 64 bytes"),
    );
    vk.verify(&idsig_payload, &idsig_signature)
        .expect("idSig must verify over its signed payload under the identity key");

    // PoP signed payload = dsep("cqdx-pair-challenge-v1", utf8(nonce b64u STRING)).
    let pop_payload = domain_separate(POP_TAG, challenge_nonce_b64u.as_bytes());
    assert_eq!(
        hex::encode(&pop_payload),
        want_pop_payload_hex,
        "PoP signed payload must match fixture"
    );
    let pop_signature = Signature::from_bytes(
        &pop_sig
            .as_slice()
            .try_into()
            .expect("PoP signature is 64 bytes"),
    );
    vk.verify(&pop_payload, &pop_signature)
        .expect("PoP must verify over its signed payload under the identity key");

    // === Guard the REAL AgentIdentity impl ================================
    // Seed a production AgentIdentity from the fixture private keys and assert
    // its key_id() + sign_domain() reproduce the same conventions.
    let (identity, dir) = seeded_identity(&id_seed, &agr_secret);

    // Sanity: the seeded identity's public halves match the fixture, proving
    // the seam actually seeded from the fixture keys (not a fresh keygen).
    assert_eq!(
        b64url_encode(&identity.identity_public_raw()),
        want_id_pub_raw,
        "AgentIdentity identity pubkey must match fixture"
    );
    assert_eq!(
        b64url_encode(&identity.agreement_public_raw()),
        want_agr_pub_raw,
        "AgentIdentity agreement pubkey must match fixture"
    );

    // keyId through the real impl.
    assert_eq!(
        identity.key_id(),
        want_key_id,
        "AgentIdentity::key_id() must match fixture keyId"
    );

    // sign_domain() reproduces the byte-for-byte idSig + PoP signatures.
    // (Ed25519-dalek is deterministic per RFC 8032, so the signature bytes
    // themselves must match the oracle, not merely verify.)
    let real_idsig = identity.sign_domain(IDSIG_TAG, &agreement_pub_raw);
    assert_eq!(
        real_idsig.as_slice(),
        idsig_sig.as_slice(),
        "AgentIdentity::sign_domain(idSig) must reproduce the fixture signature byte-for-byte"
    );
    let real_pop = identity.sign_domain(POP_TAG, challenge_nonce_b64u.as_bytes());
    assert_eq!(
        real_pop.as_slice(),
        pop_sig.as_slice(),
        "AgentIdentity::sign_domain(PoP) must reproduce the fixture signature byte-for-byte"
    );

    // Verify the real-impl signatures under the real-impl verifying key too.
    let rvk = identity.verifying_key();
    rvk.verify(&idsig_payload, &Signature::from_bytes(&real_idsig))
        .expect("real idSig must verify");
    rvk.verify(&pop_payload, &Signature::from_bytes(&real_pop))
        .expect("real PoP must verify");

    let _ = std::fs::remove_dir_all(&dir);
}
