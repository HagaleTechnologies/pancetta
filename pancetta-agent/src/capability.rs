//! Capability-token + txArmGrant verification — the **security crown jewel** of
//! the remote-operation TX path. This module is the ONLY place a
//! [`VerifiedArmGrant`](crate::arm::VerifiedArmGrant) is minted from untrusted
//! wire input, so every check below fails **CLOSED**: any parse failure, any
//! signature failure, any missing/invalid claim returns `Err` — never a
//! permissive default, never a partially-trusted grant.
//!
//! Two independent verifications, both required before TX can ever be armed:
//!
//! 1. [`CapabilityVerifier::verify_capability_token`] — the cqdx-issued JWS
//!    (`alg: "EdDSA"`, Ed25519) is verified against a **PINNED** IdP key
//!    (pinned out-of-band at pairing — NEVER refetched, defeating IdP-MITM per
//!    dispensa e2e-auth.v1 §7). Confirms `aud == our agent keyId`, `exp` in the
//!    future, and extracts the `scopes` / `clientKeyId` / `jti`.
//!
//! 2. [`CapabilityVerifier::verify_arm_grant`] — the txArmGrant is verified
//!    against the **client device key** (`clientSig`, Ed25519 over the canonical
//!    grant bytes — station-rooted TX proof; cqdx never signs this, so a cloud
//!    breach alone can NEVER forge a valid arm, per e2e-auth.v1 §4). It further
//!    honors the grant ONLY if the grant's `clientKeyId` is in the
//!    **STATION-LOCAL TX-allow-list** (`tx_allow_list`, distinct from the pinned
//!    IdP keys) — so a relay/cloud compromise alone can NEVER cause TX
//!    (e2e-auth.v1 `$defs.txArmGrant`, "pancetta bound"). Binds the grant to the
//!    verified capability's `clientKeyId` AND its `jti` (via the grant's
//!    `capabilityJti`), enforces the armed window bound, the heartbeat interval
//!    bound, the `tx` scope, and a single-use `jti` (replay).
//!
//! Canonicalization of the grant reuses the P3.0-proven approach
//! (`BTreeMap<String, serde_json::Value>` → `serde_json::to_vec`, sorted keys,
//! no whitespace, plain integers), matching
//! `dispensa contracts/auth/tx-arm-grant.vectors.v1.json`.
//!
//! ## Contract note (e2e-auth.v1)
//! - capabilityToken `exp` is **Unix epoch SECONDS** (schema line 65), so it is
//!   compared against `now_ms / 1000`; [`VerifiedCapability::exp_ms`] is the
//!   normalized millisecond form (`exp * 1000`).
//! - `kid` lives in the **JWS header**, not the payload.
//! - The armed-window / heartbeat bounds enforced here are the schema's
//!   **normative** `$defs.txArmGrant` values (both labelled "pancetta bound",
//!   i.e. pancetta MUST enforce): `armedUntil` window **≤ 10 min**
//!   ([`MAX_ARM_MS`] = 600_000 ms) and `heartbeatIntervalSec` in **[5, 15] s**
//!   ([`MIN_HEARTBEAT_SEC`], [`MAX_HEARTBEAT_SEC`]). Both are rejected (not
//!   clamped) when out of range ([`CapError::ArmTooLong`] /
//!   [`CapError::BadHeartbeat`]).
//! - The grant's `clientKeyId` MUST be present in the caller-supplied
//!   station-local `tx_allow_list` ([`CapError::ClientNotAllowed`]) and the
//!   grant's `capabilityJti` MUST equal the verified capability's `jti`
//!   ([`CapError::CapabilityMismatch`]).
//!
//! The grant's own `jti` is carried onto [`VerifiedArmGrant::jti`] so the armed
//! session can bind subsequent heartbeats to *this* arm (contract
//! `$defs.txHeartbeat.armJti`): a heartbeat naming a different arm, or replaying
//! a non-monotonic `seq`, is rejected without sliding the dead-man window (see
//! [`crate::arm::ArmState::heartbeat`]).

use std::collections::HashSet;

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;

use crate::arm::VerifiedArmGrant;
use crate::pairing::IdpKey;

/// The maximum armed-window length this agent will accept in a txArmGrant
/// (`armedUntil - now`). A grant asking for a longer window is **rejected**
/// ([`CapError::ArmTooLong`]) rather than silently clamped — an absurd arm is a
/// red flag, not something to quietly truncate. The e2e-auth.v1 normative bound
/// is **10 minutes** in milliseconds (`$defs.txArmGrant.armedUntil`).
pub const MAX_ARM_MS: i64 = 600_000;

/// Minimum accepted `heartbeatIntervalSec` (e2e-auth.v1 normative bound: 5 s).
pub const MIN_HEARTBEAT_SEC: i64 = 5;
/// Maximum accepted `heartbeatIntervalSec` (e2e-auth.v1 normative bound: 15 s).
pub const MAX_HEARTBEAT_SEC: i64 = 15;

/// A verifier holding this agent's own keyId (the expected `aud`) and the set of
/// PINNED IdP public keys. Constructed once from [`crate::pairing::PairedState`]
/// after pairing; the pin set is never mutated from live network input.
#[derive(Clone, Debug)]
pub struct CapabilityVerifier {
    /// This agent's keyId — the required `aud` on every token/grant.
    pub agent_key_id: String,
    /// The pinned IdP public keys. A token's header `kid` MUST match one of
    /// these; there is NO live refetch.
    pub pinned_idp_keys: Vec<IdpKey>,
}

/// A capabilityToken that passed every check in
/// [`CapabilityVerifier::verify_capability_token`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedCapability {
    /// The granted scopes (e.g. `["status", "tx"]`).
    pub scopes: Vec<String>,
    /// The authorized client device's keyId (`clientKeyId`). An arm grant must
    /// bind to this exact client.
    pub client_key_id: String,
    /// The token's unique id.
    pub jti: String,
    /// Expiry in **milliseconds** (normalized from the schema's epoch-seconds
    /// `exp`).
    pub exp_ms: i64,
}

/// Every way capability/arm verification can fail. There is intentionally no
/// `Ok`-with-warning path: each variant is a hard rejection.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CapError {
    /// The compact JWS was not exactly three base64url segments, or a segment
    /// did not decode / did not parse as JSON.
    #[error("malformed JWS")]
    MalformedJws,
    /// The JWS header `alg` was not `"EdDSA"` (covers `"none"`, `"HS256"`, …).
    #[error("unsupported or forbidden alg")]
    BadAlg,
    /// The header `kid` was not in the pinned IdP key set (no live refetch).
    #[error("token kid is not a pinned IdP key")]
    NotPinned,
    /// The Ed25519 signature did not verify over the signing input.
    #[error("bad signature")]
    BadSignature,
    /// `aud` did not equal this agent's keyId.
    #[error("aud mismatch")]
    AudMismatch,
    /// The token/grant expiry is at or before `now`.
    #[error("expired")]
    Expired,
    /// A required claim/field was missing or the wrong JSON type.
    #[error("missing or malformed claim: {0}")]
    MalformedClaim(String),
    /// The grant's `clientKeyId` did not match the verified capability's.
    #[error("grant client does not match capability")]
    ClientMismatch,
    /// The grant's `clientKeyId` is NOT in the station-local TX-allow-list — so
    /// a relay/cloud compromise alone can never cause TX. An empty allow-list
    /// rejects every grant (fail-closed default).
    #[error("client key id not in station-local TX-allow-list")]
    ClientNotAllowed,
    /// The grant's `capabilityJti` did not equal the verified capability's `jti`
    /// (the arm was not bound to this in-window capability).
    #[error("grant capabilityJti does not match capability jti")]
    CapabilityMismatch,
    /// `armedUntil - now` exceeded [`MAX_ARM_MS`] (rejected, not clamped).
    #[error("armed window too long")]
    ArmTooLong,
    /// `heartbeatIntervalSec` was outside `[5, 15]` (e2e-auth.v1 normative).
    #[error("heartbeat interval out of bounds")]
    BadHeartbeat,
    /// The capability did not include the `"tx"` scope.
    #[error("capability lacks tx scope")]
    NoTxScope,
    /// The grant's `jti` was already seen this session (single-use replay).
    #[error("replayed jti")]
    ReplayedJti,
}

/// Decode unpadded (or padded) base64url into bytes. Fails closed to
/// [`CapError::MalformedJws`] on any decode error.
fn decode_b64url_jws(input: &str) -> Result<Vec<u8>, CapError> {
    decode_b64url(input).map_err(|_| CapError::MalformedJws)
}

/// Decode unpadded (or padded) base64url; `Err(())` on failure. Shared by the
/// JWS segment decode and the `clientSig` decode.
fn decode_b64url(input: &str) -> Result<Vec<u8>, ()> {
    // Reject anything that is obviously not base64url before touching the
    // engine (defense-in-depth; the standard engine would also reject).
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
        .map_err(|_| ())
}

/// Canonical grant bytes: every present field EXCEPT `clientSig`, keys sorted
/// (BTreeMap), no whitespace, integers as plain numbers. Mirrors the P3.0
/// vector's canonicalization exactly.
fn canonical_grant_bytes(grant: &serde_json::Map<String, Value>) -> Result<Vec<u8>, CapError> {
    use std::collections::BTreeMap;
    let sorted: BTreeMap<String, Value> = grant
        .iter()
        .filter(|(k, _)| k.as_str() != "clientSig")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::to_vec(&sorted).map_err(|_| CapError::MalformedClaim("grant".to_string()))
}

impl CapabilityVerifier {
    /// Verify a cqdx-issued capabilityToken (compact JWS). See the module docs
    /// for the fail-closed step list. `now_ms` is Unix epoch **milliseconds**.
    pub fn verify_capability_token(
        &self,
        jws: &str,
        now_ms: i64,
    ) -> Result<VerifiedCapability, CapError> {
        // 1. Split into exactly three compact segments.
        let segments: Vec<&str> = jws.split('.').collect();
        if segments.len() != 3 {
            return Err(CapError::MalformedJws);
        }
        let (header_b64, payload_b64, sig_b64) = (segments[0], segments[1], segments[2]);

        let header_bytes = decode_b64url_jws(header_b64)?;
        let payload_bytes = decode_b64url_jws(payload_b64)?;
        let header: Value =
            serde_json::from_slice(&header_bytes).map_err(|_| CapError::MalformedJws)?;
        let payload: Value =
            serde_json::from_slice(&payload_bytes).map_err(|_| CapError::MalformedJws)?;

        // 2. Header: alg == "EdDSA" (reject "none"/"HS256"/anything else), kid.
        let alg = header
            .get("alg")
            .and_then(Value::as_str)
            .ok_or(CapError::BadAlg)?;
        if alg != "EdDSA" {
            return Err(CapError::BadAlg);
        }
        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .ok_or(CapError::MalformedJws)?;

        // 3. kid MUST be pinned — never refetch.
        let pinned = self
            .pinned_idp_keys
            .iter()
            .find(|k| k.kid == kid)
            .ok_or(CapError::NotPinned)?;
        let vk = VerifyingKey::from_bytes(&pinned.public_key).map_err(|_| CapError::NotPinned)?;

        // 4. Verify Ed25519 over the ASCII bytes `header_b64 . payload_b64`
        //    (the exact first-two compact segments joined by '.').
        let sig_raw = decode_b64url_jws(sig_b64)?;
        let sig_bytes: [u8; 64] = sig_raw
            .as_slice()
            .try_into()
            .map_err(|_| CapError::BadSignature)?;
        let sig = Signature::from_bytes(&sig_bytes);
        let mut signing_input = Vec::with_capacity(header_b64.len() + 1 + payload_b64.len());
        signing_input.extend_from_slice(header_b64.as_bytes());
        signing_input.push(b'.');
        signing_input.extend_from_slice(payload_b64.as_bytes());
        vk.verify_strict(&signing_input, &sig)
            .map_err(|_| CapError::BadSignature)?;

        // 5. Payload claims.
        let aud = payload
            .get("aud")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("aud".to_string()))?;
        if aud != self.agent_key_id {
            return Err(CapError::AudMismatch);
        }

        // exp is Unix epoch SECONDS (e2e-auth.v1 schema line 65).
        let exp_s = payload
            .get("exp")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapError::MalformedClaim("exp".to_string()))?;
        let now_s = now_ms.div_euclid(1000);
        if exp_s <= now_s {
            return Err(CapError::Expired);
        }

        let scopes = payload
            .get("scopes")
            .and_then(Value::as_array)
            .ok_or_else(|| CapError::MalformedClaim("scopes".to_string()))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| CapError::MalformedClaim("scopes[]".to_string()))
            })
            .collect::<Result<Vec<String>, CapError>>()?;

        let client_key_id = payload
            .get("clientKeyId")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("clientKeyId".to_string()))?
            .to_string();

        let jti = payload
            .get("jti")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("jti".to_string()))?
            .to_string();

        Ok(VerifiedCapability {
            scopes,
            client_key_id,
            jti,
            exp_ms: exp_s.saturating_mul(1000),
        })
    }

    /// Verify a txArmGrant and mint a [`VerifiedArmGrant`]. This is the ONLY
    /// public constructor of a `VerifiedArmGrant` outside `arm`'s own tests.
    /// Every step fails closed; see the module docs.
    ///
    /// `capability` MUST come from a prior [`Self::verify_capability_token`] for
    /// the same session; `client_verifying_key` is the client's pinned identity
    /// key (from the station-local TX-allow-list); `tx_allow_list` is the
    /// station-local set of allowed client keyIds (distinct from the pinned IdP
    /// keys) — the grant is honored ONLY if its `clientKeyId` is present, so a
    /// relay/cloud compromise alone can never cause TX (an **empty** allow-list
    /// rejects every grant, fail-closed); `seen_jtis` is the session-scoped
    /// single-use replay set.
    pub fn verify_arm_grant(
        &self,
        grant: &Value,
        capability: &VerifiedCapability,
        client_verifying_key: &VerifyingKey,
        tx_allow_list: &HashSet<String>,
        now_ms: i64,
        seen_jtis: &mut HashSet<String>,
    ) -> Result<VerifiedArmGrant, CapError> {
        let obj = grant
            .as_object()
            .ok_or_else(|| CapError::MalformedClaim("grant".to_string()))?;

        // 1. Extract clientSig; canonical bytes over ALL other fields.
        let client_sig_b64 = obj
            .get("clientSig")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("clientSig".to_string()))?;
        let canon = canonical_grant_bytes(obj)?;

        // 2. Verify clientSig (Ed25519, verify_strict) over the canonical bytes.
        let sig_raw = decode_b64url(client_sig_b64).map_err(|_| CapError::BadSignature)?;
        let sig_bytes: [u8; 64] = sig_raw
            .as_slice()
            .try_into()
            .map_err(|_| CapError::BadSignature)?;
        let sig = Signature::from_bytes(&sig_bytes);
        client_verifying_key
            .verify_strict(&canon, &sig)
            .map_err(|_| CapError::BadSignature)?;

        // 3. aud == our agent keyId; clientKeyId == the verified capability's.
        let aud = obj
            .get("aud")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("aud".to_string()))?;
        if aud != self.agent_key_id {
            return Err(CapError::AudMismatch);
        }
        let client_key_id = obj
            .get("clientKeyId")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("clientKeyId".to_string()))?;

        // 3a. STATION-LOCAL TX-allow-list — the CORE fail-closed gate. Check
        //     EARLY, before trusting any downstream field: a grant whose
        //     clientKeyId is not station-locally allow-listed is refused even if
        //     perfectly signed. An empty allow-list rejects every grant.
        if !tx_allow_list.contains(client_key_id) {
            return Err(CapError::ClientNotAllowed);
        }

        if client_key_id != capability.client_key_id {
            return Err(CapError::ClientMismatch);
        }

        // 3b. Bind the arm to THIS capability: capabilityJti == capability.jti.
        let capability_jti = obj
            .get("capabilityJti")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("capabilityJti".to_string()))?;
        if capability_jti != capability.jti {
            return Err(CapError::CapabilityMismatch);
        }

        // 4. armedUntil window + heartbeat bounds.
        let armed_until = obj
            .get("armedUntil")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapError::MalformedClaim("armedUntil".to_string()))?;
        if armed_until <= now_ms {
            return Err(CapError::Expired);
        }
        let ttl_ms = armed_until - now_ms;
        if ttl_ms > MAX_ARM_MS {
            return Err(CapError::ArmTooLong);
        }
        let heartbeat = obj
            .get("heartbeatIntervalSec")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapError::MalformedClaim("heartbeatIntervalSec".to_string()))?;
        if !(MIN_HEARTBEAT_SEC..=MAX_HEARTBEAT_SEC).contains(&heartbeat) {
            return Err(CapError::BadHeartbeat);
        }

        // 5. Capability scope must include "tx".
        if !capability.scopes.iter().any(|s| s == "tx") {
            return Err(CapError::NoTxScope);
        }

        // 6. jti single-use within the session.
        let jti = obj
            .get("jti")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("jti".to_string()))?
            .to_string();
        if !seen_jtis.insert(jti.clone()) {
            return Err(CapError::ReplayedJti);
        }

        // 7. Mint the grant. operatorCallsign is required (Part-97 attribution).
        let operator_callsign = obj
            .get("operatorCallsign")
            .and_then(Value::as_str)
            .ok_or_else(|| CapError::MalformedClaim("operatorCallsign".to_string()))?
            .to_string();

        Ok(VerifiedArmGrant {
            operator_callsign,
            ttl_ms,
            scope_tx: true,
            jti,
        })
    }
}
