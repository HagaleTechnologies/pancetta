//! Adversarial verification suite for the capability + txArmGrant crown jewel.
//!
//! Mints a valid cqdx-issued capabilityToken (a hand-signed Ed25519 compact JWS)
//! and a valid client-signed txArmGrant, then asserts that a fully valid pair
//! yields exactly one `VerifiedArmGrant` while every single-condition mutation
//! fails CLOSED (`Err`, never `Ok`). This is the ONLY place a `VerifiedArmGrant`
//! is minted from wire input, so the coverage here is exhaustive by design.

use std::collections::{BTreeMap, HashSet};

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde_json::{json, Value};

use pancetta_agent::capability::{CapError, CapabilityVerifier, VerifiedCapability, MAX_ARM_MS};
use pancetta_agent::pairing::IdpKey;

const AGENT_KEY_ID: &str = "agentKeyId000000";
const CLIENT_KEY_ID: &str = "clientKeyId00000";
const IDP_KID: &str = "idp-kid-1";
const OPERATOR: &str = "K5ARH";
const NOW_MS: i64 = 1_700_000_000_000; // fixed "now"

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Deterministic 32-byte seed → Ed25519 signing key (no rng dependency).
fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn idp_key() -> SigningKey {
    signing_key(0x11)
}
fn client_key() -> SigningKey {
    signing_key(0x22)
}

fn verifier_with_pin(vk: &VerifyingKey, kid: &str) -> CapabilityVerifier {
    CapabilityVerifier {
        agent_key_id: AGENT_KEY_ID.to_string(),
        pinned_idp_keys: vec![IdpKey {
            kid: kid.to_string(),
            public_key: vk.to_bytes(),
        }],
    }
}

fn default_verifier() -> CapabilityVerifier {
    verifier_with_pin(&idp_key().verifying_key(), IDP_KID)
}

/// The station-local TX-allow-list containing the test client's keyId — the
/// fail-closed gate honors a grant only if its `clientKeyId` is present here.
fn allow_list() -> HashSet<String> {
    HashSet::from([CLIENT_KEY_ID.to_string()])
}

// --- capabilityToken minting ------------------------------------------------

/// Mint a compact JWS with the given header + payload, signed by `key`.
fn mint_jws(header: &Value, payload: &Value, key: &SigningKey) -> String {
    let header_b64 = b64url(&serde_json::to_vec(header).unwrap());
    let payload_b64 = b64url(&serde_json::to_vec(payload).unwrap());
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig = key.sign(signing_input.as_bytes());
    let sig_b64 = b64url(&sig.to_bytes());
    format!("{header_b64}.{payload_b64}.{sig_b64}")
}

fn default_header() -> Value {
    json!({ "alg": "EdDSA", "kid": IDP_KID, "typ": "JWT" })
}

fn default_payload() -> Value {
    // exp is Unix epoch SECONDS per e2e-auth.v1; put it comfortably ahead.
    json!({
        "iss": "cqdx",
        "sub": "acct-1",
        "operatorCallsign": OPERATOR,
        "aud": AGENT_KEY_ID,
        "clientKeyId": CLIENT_KEY_ID,
        "scopes": ["status", "qsy", "tx"],
        "iat": NOW_MS / 1000 - 10,
        "exp": NOW_MS / 1000 + 600,
        "jti": "cap-jti-1"
    })
}

fn valid_token() -> String {
    mint_jws(&default_header(), &default_payload(), &idp_key())
}

// --- txArmGrant minting -----------------------------------------------------

/// Canonical bytes over every field except clientSig (sorted keys, no ws).
fn canonical_bytes(grant: &serde_json::Map<String, Value>) -> Vec<u8> {
    let sorted: BTreeMap<String, Value> = grant
        .iter()
        .filter(|(k, _)| k.as_str() != "clientSig")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::to_vec(&sorted).unwrap()
}

/// Sign `grant` with `key` and insert `clientSig`.
fn sign_grant(mut grant: serde_json::Map<String, Value>, key: &SigningKey) -> Value {
    let canon = canonical_bytes(&grant);
    let sig = key.sign(&canon);
    grant.insert("clientSig".to_string(), json!(b64url(&sig.to_bytes())));
    Value::Object(grant)
}

fn base_grant() -> serde_json::Map<String, Value> {
    json!({
        "aud": AGENT_KEY_ID,
        "clientKeyId": CLIENT_KEY_ID,
        "sessionId": "sess-1",
        "capabilityJti": "cap-jti-1",
        "operatorCallsign": OPERATOR,
        "armedAt": NOW_MS,
        "armedUntil": NOW_MS + 300_000, // +5 min, within MAX_ARM_MS
        "heartbeatIntervalSec": 10,
        "jti": "arm-jti-1"
    })
    .as_object()
    .unwrap()
    .clone()
}

fn valid_grant() -> Value {
    sign_grant(base_grant(), &client_key())
}

/// Verify a capability then a grant end-to-end with fresh replay state.
fn full_verify(
    v: &CapabilityVerifier,
    token: &str,
    grant: &Value,
    client_vk: &VerifyingKey,
    now_ms: i64,
) -> Result<pancetta_agent::arm::VerifiedArmGrant, CapError> {
    let cap = v.verify_capability_token(token, now_ms)?;
    let mut seen = HashSet::new();
    v.verify_arm_grant(grant, &cap, client_vk, &allow_list(), now_ms, &mut seen)
}

// ===========================================================================
// HAPPY PATH
// ===========================================================================

#[test]
fn happy_path_mints_verified_arm_grant() {
    let v = default_verifier();
    let out = full_verify(
        &v,
        &valid_token(),
        &valid_grant(),
        &client_key().verifying_key(),
        NOW_MS,
    )
    .expect("valid capability + grant must verify");
    assert_eq!(out.operator_callsign, OPERATOR);
    assert_eq!(out.ttl_ms, 300_000);
    assert!(out.scope_tx);
}

#[test]
fn happy_path_capability_claims_extracted() {
    let v = default_verifier();
    let cap = v
        .verify_capability_token(&valid_token(), NOW_MS)
        .expect("valid token");
    assert_eq!(cap.client_key_id, CLIENT_KEY_ID);
    assert_eq!(cap.jti, "cap-jti-1");
    assert!(cap.scopes.iter().any(|s| s == "tx"));
    // exp is seconds in the token; exp_ms normalizes to ms.
    assert_eq!(cap.exp_ms, (NOW_MS / 1000 + 600) * 1000);
}

// ===========================================================================
// CAPABILITY-TOKEN ADVERSARIAL
// ===========================================================================

#[test]
fn cap_alg_none_rejected() {
    let v = default_verifier();
    let header = json!({ "alg": "none", "kid": IDP_KID });
    let token = mint_jws(&header, &default_payload(), &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::BadAlg)
    );
}

#[test]
fn cap_alg_hs256_rejected() {
    let v = default_verifier();
    let header = json!({ "alg": "HS256", "kid": IDP_KID });
    let token = mint_jws(&header, &default_payload(), &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::BadAlg)
    );
}

#[test]
fn cap_kid_not_pinned_rejected() {
    let v = default_verifier();
    let header = json!({ "alg": "EdDSA", "kid": "some-other-kid" });
    let token = mint_jws(&header, &default_payload(), &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::NotPinned)
    );
}

#[test]
fn cap_signed_by_non_pinned_key_rejected() {
    // Header claims the pinned kid, but the signature is from a DIFFERENT key.
    let v = default_verifier();
    let impostor = signing_key(0x99);
    let token = mint_jws(&default_header(), &default_payload(), &impostor);
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::BadSignature)
    );
}

#[test]
fn cap_tampered_payload_rejected() {
    // Sign a valid token, then swap the payload segment for a re-encoded,
    // scope-escalated one — the signature no longer matches.
    let v = default_verifier();
    let header_b64 = b64url(&serde_json::to_vec(&default_header()).unwrap());
    let good_payload_b64 = b64url(&serde_json::to_vec(&default_payload()).unwrap());
    let signing_input = format!("{header_b64}.{good_payload_b64}");
    let sig = idp_key().sign(signing_input.as_bytes());
    let sig_b64 = b64url(&sig.to_bytes());

    let mut tampered = default_payload();
    tampered["aud"] = json!("attackerAgentId0");
    let tampered_b64 = b64url(&serde_json::to_vec(&tampered).unwrap());
    let token = format!("{header_b64}.{tampered_b64}.{sig_b64}");
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::BadSignature)
    );
}

#[test]
fn cap_wrong_aud_rejected() {
    let v = default_verifier();
    let mut payload = default_payload();
    payload["aud"] = json!("someOtherAgentId");
    let token = mint_jws(&default_header(), &payload, &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::AudMismatch)
    );
}

#[test]
fn cap_expired_rejected() {
    let v = default_verifier();
    let mut payload = default_payload();
    payload["exp"] = json!(NOW_MS / 1000 - 1); // one second in the past
    let token = mint_jws(&default_header(), &payload, &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::Expired)
    );
}

#[test]
fn cap_exp_exactly_now_rejected() {
    // exp <= now => Expired (boundary, fails closed at equality).
    let v = default_verifier();
    let mut payload = default_payload();
    payload["exp"] = json!(NOW_MS / 1000);
    let token = mint_jws(&default_header(), &payload, &idp_key());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::Expired)
    );
}

#[test]
fn cap_malformed_two_segments_rejected() {
    let v = default_verifier();
    let full = valid_token();
    let two = full.rsplit_once('.').unwrap().0.to_string(); // header.payload
    assert_eq!(
        v.verify_capability_token(&two, NOW_MS),
        Err(CapError::MalformedJws)
    );
}

#[test]
fn cap_malformed_four_segments_rejected() {
    let v = default_verifier();
    let token = format!("{}.extra", valid_token());
    assert_eq!(
        v.verify_capability_token(&token, NOW_MS),
        Err(CapError::MalformedJws)
    );
}

#[test]
fn cap_non_base64_segment_rejected() {
    let v = default_verifier();
    let token = "!!!.@@@.###";
    assert_eq!(
        v.verify_capability_token(token, NOW_MS),
        Err(CapError::MalformedJws)
    );
}

// ===========================================================================
// TX-ARM-GRANT ADVERSARIAL
// ===========================================================================

fn valid_cap(v: &CapabilityVerifier) -> VerifiedCapability {
    v.verify_capability_token(&valid_token(), NOW_MS).unwrap()
}

#[test]
fn grant_wrong_client_sig_rejected() {
    // Sign the grant with a DIFFERENT client key than the one the agent pins.
    let v = default_verifier();
    let cap = valid_cap(&v);
    let wrong_signer = signing_key(0x77);
    let grant = sign_grant(base_grant(), &wrong_signer);
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::BadSignature));
}

#[test]
fn grant_mutated_field_after_signing_rejected() {
    // A valid grant whose canonical bytes changed after signing (field flip).
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut grant = valid_grant();
    grant["operatorCallsign"] = json!("W1AW"); // canonical bytes now differ
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::BadSignature));
}

#[test]
fn grant_wrong_aud_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    g.insert("aud".to_string(), json!("attackerAgentId0"));
    let grant = sign_grant(g, &client_key()); // re-sign so the sig is valid
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::AudMismatch));
}

#[test]
fn grant_client_key_id_mismatch_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v); // cap.client_key_id == CLIENT_KEY_ID
    let mut g = base_grant();
    g.insert("clientKeyId".to_string(), json!("differentClient0"));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    // Allow-list the mutated client so the ClientMismatch gate (not the earlier
    // allow-list gate) is the one that fires.
    let list = HashSet::from(["differentClient0".to_string()]);
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &list,
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::ClientMismatch));
}

#[test]
fn grant_armed_until_in_past_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    g.insert("armedUntil".to_string(), json!(NOW_MS - 1));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::Expired));
}

#[test]
fn grant_armed_until_equal_now_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    g.insert("armedUntil".to_string(), json!(NOW_MS)); // <= now
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::Expired));
}

#[test]
fn grant_armed_until_ten_years_rejected_as_too_long() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    let ten_years = NOW_MS + 10 * 365 * 24 * 3_600_000;
    g.insert("armedUntil".to_string(), json!(ten_years));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::ArmTooLong));
}

#[test]
fn grant_armed_window_at_max_is_accepted_boundary() {
    // Exactly MAX_ARM_MS is accepted; one past is ArmTooLong.
    let v = default_verifier();
    let cap = valid_cap(&v);

    let mut g = base_grant();
    g.insert("armedUntil".to_string(), json!(NOW_MS + MAX_ARM_MS));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    let out = v
        .verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen,
        )
        .expect("exactly MAX_ARM_MS is within bound");
    assert_eq!(out.ttl_ms, MAX_ARM_MS);

    let mut g2 = base_grant();
    g2.insert("armedUntil".to_string(), json!(NOW_MS + MAX_ARM_MS + 1));
    let grant2 = sign_grant(g2, &client_key());
    let mut seen2 = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant2,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen2,
        ),
        Err(CapError::ArmTooLong)
    );
}

#[test]
fn grant_heartbeat_zero_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    g.insert("heartbeatIntervalSec".to_string(), json!(0));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen
        ),
        Err(CapError::BadHeartbeat)
    );
}

#[test]
fn grant_heartbeat_too_large_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let mut g = base_grant();
    g.insert("heartbeatIntervalSec".to_string(), json!(100_000));
    let grant = sign_grant(g, &client_key());
    let mut seen = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen
        ),
        Err(CapError::BadHeartbeat)
    );
}

/// e2e-auth.v1 heartbeat boundary: 5 & 15 accepted, 4 & 16 rejected.
#[test]
fn grant_heartbeat_bounds_are_exact() {
    let v = default_verifier();
    let cap = valid_cap(&v);

    let check = |hb: i64| -> Result<pancetta_agent::arm::VerifiedArmGrant, CapError> {
        let mut g = base_grant();
        g.insert("heartbeatIntervalSec".to_string(), json!(hb));
        let grant = sign_grant(g, &client_key());
        let mut seen = HashSet::new();
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen,
        )
    };

    assert!(check(5).is_ok(), "hb=5 (min) accepted");
    assert!(check(15).is_ok(), "hb=15 (max) accepted");
    assert_eq!(check(4), Err(CapError::BadHeartbeat), "hb=4 rejected");
    assert_eq!(check(16), Err(CapError::BadHeartbeat), "hb=16 rejected");
}

/// e2e-auth.v1 armedUntil boundary: window == 600_000 accepted, == 600_001
/// rejected as ArmTooLong.
#[test]
fn grant_armed_window_600000_boundary_exact() {
    assert_eq!(
        MAX_ARM_MS, 600_000,
        "MAX_ARM_MS is the 10-min normative bound"
    );
    let v = default_verifier();
    let cap = valid_cap(&v);

    let check = |window: i64| -> Result<pancetta_agent::arm::VerifiedArmGrant, CapError> {
        let mut g = base_grant();
        g.insert("armedUntil".to_string(), json!(NOW_MS + window));
        let grant = sign_grant(g, &client_key());
        let mut seen = HashSet::new();
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen,
        )
    };

    assert!(check(600_000).is_ok(), "window == 600_000 accepted");
    assert_eq!(
        check(600_001),
        Err(CapError::ArmTooLong),
        "window == 600_001 rejected"
    );
}

// --- Fix 1: station-local TX-allow-list ------------------------------------

/// A perfectly-signed, otherwise-valid grant whose clientKeyId is NOT in the
/// station-local TX-allow-list is rejected — a relay/cloud compromise alone can
/// never cause TX.
#[test]
fn grant_client_not_in_allow_list_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let grant = valid_grant(); // fully valid + correctly signed
    let mut seen = HashSet::new();
    // Allow-list does NOT contain CLIENT_KEY_ID (some other client only).
    let other_list = HashSet::from(["someOtherClient0".to_string()]);
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &other_list,
            NOW_MS,
            &mut seen,
        ),
        Err(CapError::ClientNotAllowed)
    );
}

/// Fail-closed default: an EMPTY allow-list rejects every grant.
#[test]
fn grant_empty_allow_list_rejects_all() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let grant = valid_grant();
    let mut seen = HashSet::new();
    let empty = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &empty,
            NOW_MS,
            &mut seen,
        ),
        Err(CapError::ClientNotAllowed)
    );
}

/// The same grant passes the allow-list gate (and the whole pipeline) when the
/// station-local allow-list contains its clientKeyId.
#[test]
fn grant_client_in_allow_list_passes_gate() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let grant = valid_grant();
    let mut seen = HashSet::new();
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(), // contains CLIENT_KEY_ID
        NOW_MS,
        &mut seen,
    );
    assert!(out.is_ok(), "allow-listed client passes: {out:?}");
}

// --- Fix 2: capabilityJti bind ---------------------------------------------

/// A grant whose capabilityJti does not equal the verified capability's jti is
/// rejected — the arm must ride a specific in-window capability.
#[test]
fn grant_capability_jti_mismatch_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v); // cap.jti == "cap-jti-1"
    let mut g = base_grant();
    g.insert("capabilityJti".to_string(), json!("some-other-cap-jti"));
    let grant = sign_grant(g, &client_key()); // re-signed so the sig is valid
    let mut seen = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen,
        ),
        Err(CapError::CapabilityMismatch)
    );
}

#[test]
fn grant_capability_without_tx_scope_rejected() {
    // Build a capability whose scopes lack "tx", then a validly-signed grant.
    let v = default_verifier();
    let mut payload = default_payload();
    payload["scopes"] = json!(["status", "qsy"]);
    let token = mint_jws(&default_header(), &payload, &idp_key());
    let cap = v.verify_capability_token(&token, NOW_MS).unwrap();

    let grant = valid_grant();
    let mut seen = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen
        ),
        Err(CapError::NoTxScope)
    );
}

#[test]
fn grant_same_jti_twice_second_is_replay() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let grant = valid_grant();
    let mut seen = HashSet::new();

    // First use succeeds.
    v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    )
    .expect("first use of a fresh jti");

    // Same jti again in the same session => replay.
    let out = v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &allow_list(),
        NOW_MS,
        &mut seen,
    );
    assert_eq!(out, Err(CapError::ReplayedJti));
}

#[test]
fn grant_missing_client_sig_rejected() {
    let v = default_verifier();
    let cap = valid_cap(&v);
    let grant = Value::Object(base_grant()); // no clientSig field at all
    let mut seen = HashSet::new();
    assert_eq!(
        v.verify_arm_grant(
            &grant,
            &cap,
            &client_key().verifying_key(),
            &allow_list(),
            NOW_MS,
            &mut seen
        ),
        Err(CapError::MalformedClaim("clientSig".to_string()))
    );
}

// ===========================================================================
// PROPERTY / INVARIANT: a VerifiedArmGrant ⇒ every condition held
// ===========================================================================

/// A knob that, when flipped, must turn the happy path into an `Err`.
#[derive(Clone, Copy)]
enum Flip {
    None,
    AlgNone,
    Kid,
    SigKey,
    Aud,
    Exp,
    GrantSigKey,
    GrantAud,
    GrantClient,
    ArmedPast,
    ArmedTooLong,
    HeartbeatLow,
    HeartbeatHigh,
    NoTxScope,
    Replay,
}

const ALL_FLIPS: &[Flip] = &[
    Flip::None,
    Flip::AlgNone,
    Flip::Kid,
    Flip::SigKey,
    Flip::Aud,
    Flip::Exp,
    Flip::GrantSigKey,
    Flip::GrantAud,
    Flip::GrantClient,
    Flip::ArmedPast,
    Flip::ArmedTooLong,
    Flip::HeartbeatLow,
    Flip::HeartbeatHigh,
    Flip::NoTxScope,
    Flip::Replay,
];

/// Run the full pipeline with exactly one condition flipped. Returns the final
/// verify result (the grant verification, gated by a preceding capability
/// verification which may itself fail).
fn run_flip(flip: Flip) -> Result<pancetta_agent::arm::VerifiedArmGrant, CapError> {
    let v = default_verifier();

    // --- capability side ---
    let mut header = default_header();
    let mut payload = default_payload();
    let mut sign_with = idp_key();
    match flip {
        Flip::AlgNone => header["alg"] = json!("none"),
        Flip::Kid => header["kid"] = json!("unpinned-kid"),
        Flip::SigKey => sign_with = signing_key(0x99),
        Flip::Aud => payload["aud"] = json!("otherAgent000000"),
        Flip::Exp => payload["exp"] = json!(NOW_MS / 1000 - 5),
        Flip::NoTxScope => payload["scopes"] = json!(["status"]),
        _ => {}
    }
    let token = mint_jws(&header, &payload, &sign_with);
    let cap = v.verify_capability_token(&token, NOW_MS)?;

    // --- grant side ---
    let mut g = base_grant();
    let mut grant_signer = client_key();
    match flip {
        Flip::GrantSigKey => grant_signer = signing_key(0x55),
        Flip::GrantAud => {
            g.insert("aud".to_string(), json!("otherAgent000000"));
        }
        Flip::GrantClient => {
            g.insert("clientKeyId".to_string(), json!("otherClient00000"));
        }
        Flip::ArmedPast => {
            g.insert("armedUntil".to_string(), json!(NOW_MS - 10));
        }
        Flip::ArmedTooLong => {
            g.insert("armedUntil".to_string(), json!(NOW_MS + MAX_ARM_MS + 5));
        }
        Flip::HeartbeatLow => {
            g.insert("heartbeatIntervalSec".to_string(), json!(0));
        }
        Flip::HeartbeatHigh => {
            g.insert("heartbeatIntervalSec".to_string(), json!(9_999));
        }
        _ => {}
    }
    let grant = sign_grant(g, &grant_signer);

    let mut seen = HashSet::new();
    if matches!(flip, Flip::Replay) {
        // Pre-seed the jti so this attempt is a replay.
        seen.insert("arm-jti-1".to_string());
    }
    // Allow-list both the default client AND the GrantClient flip's mutated id,
    // so that flip reaches the ClientMismatch gate (not the earlier allow-list
    // gate). Every other flip's clientKeyId is CLIENT_KEY_ID, still allowed.
    let list = HashSet::from([CLIENT_KEY_ID.to_string(), "otherClient00000".to_string()]);
    v.verify_arm_grant(
        &grant,
        &cap,
        &client_key().verifying_key(),
        &list,
        NOW_MS,
        &mut seen,
    )
}

#[test]
fn property_verified_grant_implies_all_conditions() {
    for &flip in ALL_FLIPS {
        let out = run_flip(flip);
        match flip {
            Flip::None => {
                let g = out.expect("unflipped pipeline must yield a VerifiedArmGrant");
                assert_eq!(g.operator_callsign, OPERATOR);
                assert!(g.scope_tx);
                assert_eq!(g.ttl_ms, 300_000);
            }
            _ => {
                assert!(
                    out.is_err(),
                    "flipping one condition must fail closed, got Ok for a mutated pipeline"
                );
            }
        }
    }
}

/// Cross-check that each flip maps to the SPECIFIC expected error (no accidental
/// pass-through via the wrong gate).
#[test]
fn property_each_flip_hits_its_gate() {
    let cases: &[(Flip, CapError)] = &[
        (Flip::AlgNone, CapError::BadAlg),
        (Flip::Kid, CapError::NotPinned),
        (Flip::SigKey, CapError::BadSignature),
        (Flip::Aud, CapError::AudMismatch),
        (Flip::Exp, CapError::Expired),
        (Flip::GrantSigKey, CapError::BadSignature),
        (Flip::GrantAud, CapError::AudMismatch),
        (Flip::GrantClient, CapError::ClientMismatch),
        (Flip::ArmedPast, CapError::Expired),
        (Flip::ArmedTooLong, CapError::ArmTooLong),
        (Flip::HeartbeatLow, CapError::BadHeartbeat),
        (Flip::HeartbeatHigh, CapError::BadHeartbeat),
        (Flip::NoTxScope, CapError::NoTxScope),
        (Flip::Replay, CapError::ReplayedJti),
    ];
    for (flip, expected) in cases {
        assert_eq!(run_flip(*flip), Err(expected.clone()));
    }
}
