# Station Agent P3 — Relay/Pairing Wire + Command Execution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.
> **SECURITY-CRITICAL.** TDD, adversarial tests first, every check fails CLOSED. Each implementer: run
> `cargo fmt --all` + `cargo clippy -p pancetta-agent -- -D warnings` + relevant tests in the **FOREGROUND**
> (never a detached monitor). Never push (controller pushes). Private keys 0600, never logged.

**Goal:** Wire the P0–P2 safety core to the network — pair, connect the relay, run Noise E2E, verify
capability + arm grants, execute control commands — so a remote operator (panino) can read/QSY/TX under a
live, locally-consented arm. Relay stays zero-trust; the station is the final TX authority.

**Spec:** `docs/superpowers/specs/2026-07-01-station-agent-p3-design.md`. **Contracts (recorded, dispensa
`2707dcc`):** `contracts/{relay/relay.v1,pairing/pairing.v1,auth/tx-arm-grant.vectors.v1,auth/e2e-auth.v1}`.
Both crypto vectors verified PASS in Rust. Security refinements (domain-sep sigs) folded in.

**Branch:** `feat/station-agent-p3` off main.

**Regression invariant:** the agent stays **default-OFF and unwired into the live run loop until P3.4**;
until then it's new modules in `pancetta-agent` with no coordinator call site. P3.4 wiring is gated behind
`[network.station_agent].enabled=false` + a completed pairing, so a stock station is untouched.

---

## P3.0 — vendor contracts + txArmGrant conformance (offline, quick)
**Files:** `pancetta-agent/tests/fixtures/`, `pancetta-agent/Cargo.toml`, `pancetta-agent/tests/tx_arm_grant_vectors.rs`.
- [ ] Vendor into `pancetta-agent/tests/fixtures/`: `tx-arm-grant.vectors.v1.json` (from dispensa `contracts/auth/`). (relay.v1/pairing.v1 schemas are consumed as types, not validated at runtime here — vendor them under `tests/fixtures/schemas/` only if a drift test is cheap; optional.)
- [ ] Add deps to `pancetta-agent`: `ed25519-dalek = "2"`, `base64 = "0.22"` (dev-dep `serde_json` already present). 
- [ ] Test `tx_arm_grant_canonical_and_sig`: parse the fixture; rebuild canonical bytes via `BTreeMap<String, serde_json::Value>` → `serde_json::to_vec` (sorted keys, no whitespace, plain ints); assert `hex == canonicalBytesHex`; decode `clientPublicKeyRaw`+`clientSig` (unpadded base64url, tolerant); assert `VerifyingKey::verify_strict(canon, sig)` OK. (Controller verified this PASSES.) A mutated grant field → canonical bytes differ → `verify` fails (adversarial).
- [ ] Verify FOREGROUND: `cargo test -p pancetta-agent tx_arm_grant`; clippy clean. Commit `test(agent): txArmGrant canonical + clientSig conformance (drift-guarded)`.

## P3.1 — pairing crypto + client (offline crypto; HTTP behind a trait)
**Files:** `pancetta-agent/src/keys.rs`, `pancetta-agent/src/pairing.rs`.
- [ ] `keys.rs`: `AgentIdentity` — generate/load Ed25519 identity + X25519 agreement keypairs; persist to a dir (0600 perms; use `std::os::unix::fs::PermissionsExt`), never log. `key_id()` = `b64url_unpadded(SHA-256(spki_der))` where `spki_der = hex!("302a300506032b6570032100") ‖ ed25519_pub_raw`. Domain-sep signer: `sign_domain(tag: &str, msg: &[u8]) -> [u8;64]` = Ed25519 over `tag.as_bytes() ‖ 0x00 ‖ msg`. Tests: keyId stable across load; a known (pub → keyId) vector if derivable; `sign_domain("cqdx-relay-agent-auth-v1", c)` ≠ `sign_domain("cqdx-pair-idsig-v1", c)` for the same `c` (domain separation proven); generated dir is 0600.
- [ ] `pairing.rs`: pure request/response DTOs matching `pairing.v1` `$defs` (`agentEnrollRequest`, `enrollChallenge`, `enrollCompleteRequest`, `agentEnrollCompleteResponse`, `idpKeyEntry`, `pairingCodeResponse`) — serde camelCase, `#[serde(deny_unknown_fields)]` where safe. An `PairingClient<H: HttpTransport>` trait for the 3 POSTs (`HttpTransport` = a tiny async `post(url, json) -> Result<json>` trait so tests inject a mock; a real `reqwest` impl behind a feature or in P3.4). `enroll(code) ->` builds `agentEnrollRequest{keyId, identityPublicKey, agreementPublicKey, idSig=sign_domain("cqdx-pair-idsig-v1", agreement_pub), …}`, posts, signs the returned `nonce` with PoP `sign_domain("cqdx-pair-challenge-v1", nonce)`, posts complete, **pins `idpKeys`**. Returns a `PairedState { agent_key_id, idp_keys: Vec<IdpKey>, … }` persisted locally.
- [ ] Tests (mock HTTP): happy-path enroll pins idpKeys; the request carries the domain-sep idSig; a complete response with malformed idpKeys → error (no partial pin); PoP signs the nonce with the right tag.
- [ ] Verify FOREGROUND (`cargo test -p pancetta-agent`; clippy). Commit `feat(agent): pairing crypto (keyId/SPKI, domain-sep idSig/PoP) + enroll client (mockable HTTP)`.

## P3.2 — capability + arm-grant verification → VerifiedArmGrant (the L3 security core)
**Files:** `pancetta-agent/src/capability.rs`.
- [ ] `CapabilityVerifier { pinned_idp_keys: Vec<IdpKey>, agent_key_id: String }`.
- [ ] `verify_capability_token(jws: &str) -> Result<VerifiedCapability>`: parse compact JWS; require `kid ∈ pinned` (else `Err(NotPinned)` — NEVER refetch); verify Ed25519 sig over `header.payload`; check `aud == agent_key_id`, `exp > now`, parse `scopes`. Adversarial: non-pinned kid, wrong aud, expired, bad sig, tampered payload → all `Err`, never Ok.
- [ ] `verify_arm_grant(grant_json: &serde_json::Value, client_pinned_key: &VerifyingKey, now_ms) -> Result<pancetta_agent::arm::VerifiedArmGrant>`: rebuild canonical bytes (BTreeMap sorted, EXCLUDING `clientSig`) — reuse the P3.0-proven routine; verify `clientSig` (Ed25519) against the client's pinned key; check `aud == agent_key_id`, `armedUntil > now_ms`, **clamp/reject** absurd `armedUntil` (> now + MAX_ARM_MS, e.g. 24h) and `heartbeatIntervalSec` (∉ [1, 300]); require the capability scope includes `tx`; single-use `jti` (caller tracks a seen-set). Produce `VerifiedArmGrant { operator_callsign, ttl_ms = armedUntil - now, scope_tx: true }`. **This is the ONLY place a VerifiedArmGrant is minted in the live path.**
- [ ] Adversarial tests (exhaustive): valid grant → Ok; wrong clientSig → Err; mutated field (canonical bytes change) → Err; wrong aud → Err; past armedUntil → Err; absurd armedUntil (10 years) → Err/clamp; heartbeatIntervalSec 0 or 100000 → Err; no tx scope → Err; replayed jti → Err. Property: a VerifiedArmGrant is produced ⇒ (sig valid ∧ aud ok ∧ fresh ∧ tx-scope ∧ sane bounds ∧ jti unseen). Commit `feat(agent): capability + arm-grant verification → VerifiedArmGrant (adversarial, fail-closed)`.

## P3.3 — relay transport (`relay.v1`) + Noise-over-env driver
**Files:** `pancetta-agent/src/relay.rs`, `pancetta-agent/src/session.rs`.
- [ ] `relay.rs`: `#[serde(tag="t")] enum RelayFrame { Hello{…}, Auth{…}, Ready{…}, Presence{…}, Env{…}, Bye{…}, Error{…} }` (camelCase, matching relay.v1 exactly; `#[serde(rename_all="camelCase")]`). Parse/serialize round-trip tests against the schema's field names. `MAX_FRAME_BYTES` bound on `env.payload` (reject oversized BEFORE base64-decode — closes P0–P2 MINOR). A `RelayTransport<W: WsSink+WsStream>` trait so a mock WS drives tests (no real network in unit tests).
- [ ] Agent-leg driver: on `Hello{challenge}` → send `Auth{agentKeyId, sig=identity.sign_domain("cqdx-relay-agent-auth-v1", decoded_challenge)}`; on `Ready` mark admitted; on `Env{payload, src}` verify `src == peer` then feed the Noise driver; on `Error{code}` classify terminal/transient (from the schema's x-error-codes). 
- [ ] `session.rs`: glue L1↔L2 — the first inbound `env` carries Noise msg1 → `ResponderHandshake::read_msg1` → `write_msg2` back as an `env{dst:clientKeyId}` → `into_transport`; thereafter `env.payload` = transport ciphertext ↔ decrypted control frames. One session per connection.
- [ ] Tests (mock WS): full agent-leg choreography to `ready`; Noise handshake over env completes; an `env` with `src != peer` dropped; oversized payload rejected; terminal error → stop, transient → await presence. Commit `feat(agent): relay.v1 transport + Noise-over-env session driver (mockable WS)`.

## P3.4 — command execution + coordinator component wiring (default OFF)
**Files:** `pancetta-agent/src/control.rs`, `pancetta/src/coordinator/station_agent/{mod.rs}` (new), `pancetta/src/coordinator/mod.rs`, `pancetta-config` (relay_url/pairing_api_url/key_dir).
- [ ] `control.rs`: map decrypted rig-api.v1 `clientCommand` → an action enum (ReadStatus, Qsy, SetSplit, SetMode, TxRequest, Heartbeat, Arm{grant}, Disarm). Pure mapping + validation; unknown/unsupported → ignored + audited.
- [ ] `coordinator/station_agent/mod.rs`: `start_station_agent_component` (mirror `start_remote_gateway_component`): disabled OR unpaired → drain/no-op; enabled+paired → spawn the dial+serve loop (real `reqwest`/`tokio-tungstenite` WS behind this crate boundary). On `Arm{grant}` → `capability.verify_*` → `remote_tx_arm.lock().arm(verified, now)` + audit; on `Heartbeat` → `arm.heartbeat(now)`; `Disarm`/presence-down/heartbeat-timeout → `arm.disarm`/rely on P2 tick. TxRequest → emit `TransmitRequest{ origin: Remote, … }` (the P2 gate + key-time re-check enforce arming). Read stream ← `translate.rs` → `env` ciphertext out. Wire `remote_tx_arm` (already on the coordinator) + seed `set_local_consent(config.remote_tx_enabled)`.
- [ ] Config: add `relay_url: Option<String>`, `pairing_api_url: Option<String>`, `key_dir: Option<String>` to `[network.station_agent]`. Default None → component inert.
- [ ] Tests: component disabled → drains, no dial; a mock end-to-end (mock relay+client) arms then TxRequest keys PTT through the real coordinator gate; heartbeat-loss auto-disarms and the next TxRequest is dropped at key-time; `remote_tx_enabled=false` blocks arming; `TxPolicy::Disabled` overrides. Full `cargo check --workspace --all-targets --features transmit` FOREGROUND. Commit `feat(coord): station-agent component — pair/connect/verify/execute (default OFF; Remote TX via P2 gate)`.

## P3.5 — docs + adversarial security review + land
- [ ] CLAUDE.md + ARCHITECTURE.md bullets (the full 5-check spine; default OFF; final TX authority). Update the P0–P2 bullet to "P3 wired".
- [ ] Address the carried P0–P2 followups if not already: Noise `MAX_MSG_LEN` on decrypt (done in P3.3 bound), `arm.rs:324` expect→match (NIT), audit fsync note.
- [ ] **Adversarial security-review subagent** over the whole P3 surface — focus: each of the 5 spine checks fails closed; no VerifiedArmGrant path bypasses sig/aud/expiry/scope/bounds/jti; pinned-key enforcement (no refetch); private keys never logged; oversized/malformed frames never panic; the relay compromise model (opaque pipe) holds; local consent + Shift+Q primacy intact. Fix all CRITICAL/IMPORTANT before merge.
- [ ] fast gate → PR → **wait CI green** → merge; sync main. Update dispensa: mark P3 built; note any contract feedback.

---
## Self-review
- **5-check spine, each fail-closed**; VerifiedArmGrant minted in exactly one audited place after all checks.
- **Contracts matched byte-exact** (vendored vectors drift-guard canonicalization + Noise).
- **Default OFF + unpaired = inert**; local consent + Shift+Q primacy preserved; local TX byte-identical.
- **Relay = zero-trust opaque pipe**; a cloud/relay/token breach yields transport only, not PTT.
