# Station Agent P3 — Relay/Pairing Wire + Command Execution — Design Spec

**Date:** 2026-07-01
**Status:** Proposed (security-critical; builds on the operator-approved P0–P2 offline safety core + Accepted ADR-0002). Operator directive: "P3. Go deep and secure."
**Author:** Claude Opus 4.8 (under K5ARH supervision)

> Grounded in the **now-recorded** contracts (dispensa `2707dcc`): `contracts/relay/relay.v1.schema.json`,
> `contracts/pairing/pairing.v1.schema.json`, `contracts/auth/tx-arm-grant.vectors.v1.json` (+ `e2e-auth.v1`).
> pancetta's security refinements are folded in (domain-separated `auth.sig`/`idSig`/PoP). Both crypto
> interop vectors are **verified PASS in Rust** (Noise IK byte-for-byte + txArmGrant canonical/clientSig).

## Goal

Wire the proven P0–P2 safety core to the network: the station agent **dials the relay, authenticates, runs
Noise E2E with the remote client, verifies capability + arm grants, and executes control commands** —
read status, QSY/tune, and (only under a valid, live, locally-consented arm) transmit. The relay/cloud
stays a zero-trust opaque pipe; the station remains the final TX authority.

## The security spine (what makes a cloud/relay/token compromise insufficient to key TX)

Five independent checks, each of which must pass, in order, before a single PTT key:
1. **Relay admission** (DoS/rendezvous gate only) — the agent's `auth.sig` = Ed25519 over
   `utf8("cqdx-relay-agent-auth-v1") ‖ 0x00 ‖ challengeBytes` (domain-separated). NOT authoritative for control.
2. **Noise E2E** — IK handshake (agent = responder, static `s` pinned at pairing); all control frames are
   ciphertext in `env.payload`; a compromised relay sees only opaque bytes and cannot forge/replay
   (AEAD counter, verified in P0–P2).
3. **capabilityToken** — a compact JWS the agent **independently re-verifies** against a **pinned** IdP
   key (`idpKeys` from pairing); checks `aud == this agentKeyId`, `exp`, and scope. A relay-accepted token
   is re-checked here; the relay's admission check is not trusted.
4. **txArmGrant.clientSig** — the arm request is Ed25519-signed by the client over the canonical grant
   (sorted-key compact JSON, verified byte-for-byte in Rust); the agent verifies it against the
   client's pinned key and checks `aud`, `armedUntil`, `operatorCallsign`, `heartbeatIntervalSec`.
5. **The P2 ArmState + local consent** (already built) — even a fully valid grant only *arms*; TX is
   permitted iff armed ∧ tx-scope ∧ unexpired ∧ heartbeat-fresh ∧ **local_consent** ∧ ¬local_kill, and
   the coordinator re-checks at PTT key-time. `TxPolicy::Disabled`/Shift+Q overrides everything.

A breach of the relay/cloud gives an attacker steps 1–2's transport at most; steps 3–5 are enforced
locally by the agent + coordinator with keys the cloud never holds.

## Layers (concrete, per the recorded schemas)

### L1 — Relay transport (`relay.v1`)
Outbound persistent WSS to `wss://<relay-origin>/<agentKeyId>` (per-rig DO). Agent leg:
`DO→ hello{challenge}` → `agent→ auth{role:"agent", agentKeyId, sig}` (domain-sep) → `DO→ ready{keyId, peerPresent}`;
observe `presence{peer, state}`. Data: `env{t, dst, payload, src?}` — `payload` = opaque unpadded base64url
(Noise handshake msg1/msg2 + transport), `dst = clientKeyId`, DO stamps `src` (agent confirms `src == its
authenticated peer`). Frames are a `#[serde(tag="t")]` enum; agent **rejects** inbound `ready`/`presence`
spoofing only by treating them as DO-authoritative (they arrive only from the DO). Error codes:
terminal (`BAD_AUTH`/`TOKEN_*`/`AUD_MISMATCH`/`SCOPE_EMPTY`/`AGENT_OCCUPIED`/`NOT_ADMITTED`/`FRAME_TOO_LARGE`/`BAD_FRAME`/`BAD_ROUTE`) → no retry without fresh creds;
transient (`NO_PEER`/`UNKNOWN_DST`) → stay connected, await `presence{up}`. Reconnect with capped backoff.
**`env.payload` MUST be length-bounded (`MAX_FRAME_BYTES`) before base64-decode/Noise-decrypt** (closes the
P0–P2 review's MINOR unbounded-alloc finding).

### L2 — Noise E2E (P0–P2, integrate)
`ResponderHandshake`/`NoiseTransport` already built + conformance-proven. P3 drives them from the `env`
stream: decode `env.payload` → `read_msg1`/`write_msg2` (msg2 back out as an `env`), then transport
`decrypt`/`encrypt`. One Noise session per WS connection (no resumption; matches the contract).

### L3 — Pairing + capability + arm (`pairing.v1` + txArmGrant) — the security core
- **Pairing client** (offline crypto, mockable HTTP): generate Ed25519 identity + X25519 agreement keypairs
  on first run (private keys in `~/.pancetta/agent/`, 0600, never logged/uploaded); `keyId =
  b64url(SHA-256(SPKI_DER))` with the pinned prefix `302a300506032b6570032100 ‖ rawKey`; enroll via
  `POST /pair/codes`(owner) → `/pair/agent` (`{pairingCode, keyId, identityPublicKey, agreementPublicKey,
  idSig, …}`, `idSig` domain-sep `cqdx-pair-idsig-v1`) → `/pair/agent/complete` (`{challengeId, keyId,
  signature}`, PoP domain-sep `cqdx-pair-challenge-v1`) → **pin `idpKeys`**. Rotation = re-pair (v1).
- **capabilityToken verification:** compact JWS; verify signature against a **pinned** `idpKey` (reject if
  `kid` not pinned — no silent refetch); check `aud == agentKeyId`, `exp`, scope ⊇ requested.
- **txArmGrant verification → `VerifiedArmGrant`:** rebuild the canonical bytes (Rust `serde_json` over a
  sorted map — verified byte-for-byte), verify `clientSig` (Ed25519) against the client's pinned key,
  check `aud == agentKeyId`, `armedUntil` in the future, carry `operatorCallsign` + derive `ttl_ms` from
  `armedUntil` + `heartbeatIntervalSec`. Produce the P2 `VerifiedArmGrant { operator_callsign, ttl_ms,
  scope_tx }` and feed `ArmState::arm(...)`. **This is the ONLY producer of a VerifiedArmGrant** in the
  live path (P2 kept it a pure value; P3 is where it's minted, after all checks).

### L4 — Command execution
Decrypted control frames are rig-api.v1 `clientCommand`s. Map onto the **existing** coordinator bus:
- read status → reuse the gateway's `translate.rs` (send the read stream out as `env` ciphertext).
- QSY/tune/split/mode → existing `RigControlMessage`/band-change/`SetSplit`.
- **TX request → the existing `TransmitRequest` path with `origin: TxOrigin::Remote`** — the P2 gate does
  the rest (keys only if the arm permits, re-checked at key-time). Heartbeats from the client refresh
  `ArmState::heartbeat`; a missed heartbeat (`heartbeatIntervalSec`-derived) auto-disarms.
- The agent **never** exposes autonomous origination remotely (ADR-0002 §5b).

## Component + config
`start_station_agent_component` (mirrors `start_remote_gateway_component`): disabled → drain; enabled →
dial + serve. Config `[network.station_agent]` already exists (`enabled`, `remote_tx_enabled`); add
`relay_url`, `pairing_api_url`, key/pin storage dir. **Default OFF.** Enabling requires a completed pairing.

## Scope / non-goals (v1)
- Agent end only (no panino/client code). One paired client at a time (the DO is per-rig, one control peer).
- No delegation UI (the agent just verifies whatever scope the capabilityToken carries).
- No auto-rotation of IdP keys (re-pair). No relay-level session resumption.
- wss/TLS provided by the platform (Cloudflare) — the agent dials `wss://`.

## Risks / careful points
1. **Every one of the 5 spine checks fails CLOSED.** No check may be skippable by a malformed/oversized
   frame (bound + reject), a missing field (schema-validate), or an error path (deny, don't default-allow).
2. **Pinned-key enforcement:** a `kid`/kid-set not pinned at pairing is a hard reject, never a refetch.
3. **Private key hygiene:** 0600, never logged (audit `detail` carries only callsign/jti/reason), never in
   Debug/error strings; consider `zeroize` on the in-memory key.
4. **Canonicalization drift:** the txArmGrant canonical bytes are consensus-critical — the in-repo
   conformance test (vendored vector) guards it; any serde change that reorders keys/whitespace must fail it.
5. **Heartbeat/TTL come from the grant** (`heartbeatIntervalSec`, `armedUntil`) — clamp to sane bounds
   (reject absurd values) so a malicious grant can't set a 10-year arm; the local `remote_tx_enabled` +
   Shift+Q are the ultimate backstops regardless.
6. **Replay across sessions:** capabilityToken `jti`/txArmGrant `jti` should be single-use within a session;
   track seen jtis to prevent arm-replay (a captured arm frame can't re-arm after disarm).

## Testing (adversarial-first)
- **Vectors in-repo:** txArmGrant canonical + clientSig byte-for-byte (drift-guard); Noise (already).
- **Pairing:** keyId = SHA-256(SPKI) matches a known vector; domain-sep idSig/PoP produce distinct bytes
  from a relay auth.sig over the same 32 bytes (proves cross-protocol reuse is prevented).
- **Capability/arm verification (adversarial):** a token signed by a non-pinned key → reject; wrong `aud`
  → reject; expired → reject; a txArmGrant with a valid-looking but wrong `clientSig` → reject; a mutated
  grant field (so canonical bytes differ) → clientSig fails; a replayed arm `jti` after disarm → reject;
  an absurd `armedUntil`/`heartbeatIntervalSec` → clamped/rejected. Each must NOT produce a VerifiedArmGrant.
- **Relay transport:** frame parse round-trips; oversized `env.payload` rejected pre-decode; a `src` that
  isn't the authenticated peer → drop; terminal vs transient error handling; reconnect/backoff.
- **End-to-end (mock relay + mock client, offline):** pair → connect → Noise → capability → arm → remote
  TX keys PTT (through the real coordinator gate); heartbeat-loss auto-disarms mid-session and the next
  TX is dropped at key-time; `remote_tx_enabled=false` blocks the whole thing; Shift+Q/`TxPolicy::Disabled`
  wins over a live remote arm.
- **Fuzz-ish:** malformed JSON frames, truncated Noise messages, wrong-length keys never panic (Result).

## Open questions for operator review
1. **Relay/pairing URLs:** config `relay_url` + `pairing_api_url` (operator sets after cqdx publishes the
   origins). Placeholder-OFF until then.
2. **One control peer vs many:** v1 = one (the DO is per-rig; a second client connecting → `AGENT_OCCUPIED`
   at the relay). Confirm single-peer is fine for v1.
3. **jti replay store:** in-memory per-session set (proposed) vs persistent. In-memory is enough to stop
   intra-session replay; a process restart drops it (acceptable — a new session re-pairs the Noise channel).
