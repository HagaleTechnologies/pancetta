# Client identity binding and hello-gated read stream — Design Spec

Date: 2026-10-12
Status: Implemented in pancetta (PAN-95); client adoption pending dispensa
Ticket: PAN-95
Related: dispensa Q-0019 §5 (scoped `hello.capabilityToken`), dispensa Q-0023;
`contracts/auth/e2e-auth.v1.schema.json`; pairing.v1 (`idSig`, keyId);
`2026-07-20-concurrent-multi-client-station-agent-design.md` (the multi-peer session this
amends); `docs/security-review-remote-rig-2026-07-02.md`

## Context

The station agent's multi-peer relay session (`MultiPeerSession`) admits up to 8 remote clients
over one relay websocket, each with its own Noise IK session. Two properties of that session did
not match the `e2e-auth.v1` contract:

- A peer's station-side identity was the keyId the relay put in the envelope's `src` field. The
  Noise responder did not read the initiator's static key, so the identity was not tied to the
  key the handshake authenticated.
- Display and control events (decodes, QSO progress, frequency, spectrum, `controlState`) went
  to every established transport from the moment its handshake completed. The scoped
  `hello.capabilityToken` check gated only the inbound `qsy`/`setSplit` actions.

This change brings pancetta in line with dispensa Q-0019 §5 (a peer acts only within the scopes
of a verified `hello` capability) and Q-0023. After it:

1. A peer's identity is the keyId proven inside the Noise handshake. The relay `src` label is
   routing metadata that must match it.
2. A peer receives nothing until it holds a currently-valid `status`-or-higher capability from a
   verified `hello`.

The TX-arm chain (`capabilityToken`, `txArmGrant.clientSig`, `tx_arm_allow_list`, local
`ArmState`) is independently rooted and unchanged.

## Wire format

```
Noise suite (unchanged):  Noise_IK_25519_ChaChaPoly_BLAKE2s, empty prologue
Initiator static key:     MUST be the device's enrolled X25519 agreement key
                          (the same key whose public half it enrolled as
                          pairing.v1 `agreementPublicKey`)
msg1 payload (NEW):       UTF-8 JSON object, at most 1024 bytes
```

The msg1 payload is the **client identity proof**:

```json
{"v":1,"identityPublicKey":"<b64url Ed25519 public key>","idSig":"<b64url Ed25519 signature>"}
```

| Field | Type | Meaning |
|---|---|---|
| `v` | integer | Proof format version. Must be `1`. Any other value is refused. |
| `identityPublicKey` | string | Raw 32-byte Ed25519 identity public key, unpadded base64url. Same encoding as pairing.v1 `identityPublicKey`. |
| `idSig` | string | Raw 64-byte Ed25519 signature, unpadded base64url. Byte-identical to the pairing.v1 `idSig` the device already computed at enrollment: Ed25519 over `"cqdx-pair-idsig-v1" ‖ 0x00 ‖ agreementPublicKeyRaw`. No new signature or key is introduced. |
| any other field | — | Ignored. This leaves room for additive extension; the whole payload is already authenticated by the handshake. |

Rules:

- The payload is at most 1024 bytes. A longer payload is refused.
- Field names are camelCase (ADR-0003), matching the pairing.v1 DTOs.
- `v` is checked before the keys are decoded. A version other than `1` is refused; there is no
  fallback to an older format.
- `identityPublicKey` must decode to exactly 32 bytes and `idSig` to exactly 64 bytes.
- The initiator's Noise static key must be the enrolled agreement key, because `idSig` is checked
  over the static key the handshake authenticated (see Verification rules). A client that uses any
  other static key fails verification.

A client builds the proof from primitives it already has (panino `identity.ts`, cqdx
`device-crypto.ts`, per the `pairing-sig.vectors.v1` conformance notes):

```ts
const proof = JSON.stringify({
  v: 1,
  identityPublicKey: b64url(device.identityPublicKeyRaw),
  idSig: b64url(device.idSig),            // the enrollment idSig, reused
});
const msg1 = noise.initiator(device.agreementPrivateKey, agentAgreementPublicKey)
                  .writeMessage(utf8(proof));
```

msg2's payload stays empty.

## Verification rules

The station runs these checks in order, before it allocates any per-peer state and before it
sends msg2:

1. `env.src` is in the station-local admission list (`tx_allow_list`), and fewer than 8 peers are
   connected. This is the existing cheap pre-filter; it runs before any handshake state exists.
2. msg1 decrypts. Noise IK authenticates the initiator's static key, and the station reads it via
   `get_remote_static()`. Because the IK msg1 payload is encrypted under a key that mixes the
   `ss` DH, a successful decrypt proves the sender holds that static key's private half.
3. The payload parses as a version-1 proof.
4. `idSig` verifies (`verify_strict`) under `identityPublicKey` over
   `"cqdx-pair-idsig-v1" ‖ 0x00 ‖ <the Noise remote static key>`.
5. `keyId := b64url_nopad(SHA-256(302a300506032b6570032100 ‖ identityPublicKey))` must equal
   `env.src`.

The peer is then established under that keyId. keyIds are self-certifying, so no registry or
provisioning step is needed.

| Outcome | What the client sees | Station log (`target: "agent"`, WARN) |
|---|---|---|
| All checks pass | `env{dst: keyId, payload: msg2}`, then nothing until it sends `hello` | — |
| `src` not allow-listed | no reply | `remote peer refused peer=<src> reason=NotAllowListed` |
| 8 peers already connected | no reply | `… reason=AtCapacity` |
| msg1 fails Noise | no reply | `… reason=Handshake` |
| empty msg1 payload (clients that predate this change) | no reply | `… reason=IdentityProofMissing` |
| proof malformed / wrong version / `idSig` does not verify over the handshake key | no reply | `… reason=IdentityProofInvalid` |
| proof valid, but its keyId ≠ `src` | no reply | `… reason=IdentityMismatch` |

A refused client gets no explicit rejection message; it hits its own connect timeout (panino:
10 s). This matches how an unlisted peer was already refused.

## Read-stream authorization

```
BEFORE                                       AFTER
client → env(msg1)                           client → env(msg1 + identity proof)
station → env(msg2)                          station → env(msg2)
station → event controlState   ← greeting    (nothing)
station → event decoded/…      ← read feed   (nothing — events are not queued for it)
client → frame hello{capabilityToken}        client → {"frame":"hello","hello":{"protocolVersion":1,
                                                        "capabilityToken":"<JWS>"}}
                                               JWS must: verify against pinned IdP key, aud = agent,
                                               clientKeyId == proven keyId, scopes ⊇ status (or qsy/tx),
                                               exp > now
                                             station → event controlState   ← greeting moved here
                                             station → event decoded/…      ← read feed starts
                                             … token exp passes …
                                             (nothing further until a new hello with a fresh token;
                                              station logs INFO "peer capability expired — station
                                              data paused until it sends a hello with a fresh
                                              capabilityToken")
```

Scope tiers are ordered `status` < `qsy` < `tx`; a higher scope implies the lower ones.

| Station behavior | Required capability |
|---|---|
| Send any frame to the peer (events, `controlState`, `error`) | `status` or higher, unexpired |
| Run `qsy` (`setFrequency`) or `setSplit` | `qsy` or higher, unexpired |
| Arm TX | `tx`, plus the unchanged `txArmGrant` chain |

Expiry:

- The station stores the verified capability's scopes and `exp` per peer. Every check compares
  `exp` with the current time, so an expired capability stops working at once.
- A per-loop sweep clears expired capabilities and logs each affected peer once at INFO.
- If the expired peer is the current controller, the sweep also disarms remote TX and releases
  control (the same fail-safe as the controller disconnecting), and the remaining authorized
  peers get the updated `controlState`. The station does not keep transmitting for a controller
  whose capability it has stopped honoring. (A controller that never sent a valid `hello` holds no
  capability to lose; requiring one before control is a separate follow-up.)
- A new `hello` with a fresh token restores delivery. An invalid `hello` clears the peer's
  capability; if that peer held a capability and is the current controller, the station disarms
  and releases control the same way, so a failed `hello` cannot hide an expired controller from
  the sweep.

No frame of any kind goes to a peer without a currently-valid `status`-or-higher capability:
no events, no `controlState`, and no `error` frames, including the "another client holds
control" refusal. The station still processes that peer's inbound frames under the existing
control rules (`takeControl`, implicit grab, `disarm`, `heartbeat`). A `hello` never grabs
control.

## Example

Test device: Ed25519 identity seed `0x22`×32, X25519 agreement secret `0x23`×32.

- Agreement public key: `mkUDqYqxD-jTVMnELL0MnXlE9S59FNjqWXdefcnjv0s`
- keyId: `dE82zKZ-sZEssoLgjoa9L-DQkASijPbnxr1Wzda_Zao`
- msg1 payload (170 bytes):

```json
{"v":1,"identityPublicKey":"oJql9HpnWYAv-VX43C0qFKXJnSO-l_hkEn_5ODRVpPA","idSig":"tjDTm307i52bJPpXjlGKKS5p2Qvj8p0dFgQgJ9eNKwyq0GOMSAv4fMnJu7u_2wG4bsFIVFa1E_u44qKh_XaLCQ"}
```

The test `fixed_seed_example_matches_spec` in `pancetta-agent/src/peer_identity.rs` pins this
payload, so the spec and the code cannot drift.

### Station implementation

- `pancetta-agent/src/peer_identity.rs`: `verify_client_identity_proof` (checks 3-5; returns the
  proven keyId) and the reference encoder.
- `pancetta-agent/src/noise.rs`: `ResponderHandshake::remote_static` (check 2).
- `pancetta-agent/src/multi_session.rs`: `bootstrap_peer` (runs the checks before msg2),
  `RefuseReason` (the six refusal reasons), `broadcast_to` (fan-out where the caller names the
  recipients; there is no unfiltered broadcast).
- `pancetta/src/coordinator/station_agent/mod.rs`: `PeerAuth` (scopes plus `exp_ms`),
  `PeerCtx::grants` (tier and expiry check), `peer_may_receive` (the outbound gate used by
  `deliver_sends` and `drain_read_stream`), `sweep_expired_peer_auth`.

## Rollout

- The station enforces both rules on release. There is no config opt-out. The station agent
  stays default-OFF.
- Clients must:
  1. send the identity proof in msg1, using their enrolled agreement key as the Noise initiator
     static key;
  2. re-send `hello` with a fresh `capabilityToken` before the current token's `exp`. Non-TX
     tokens last at most 900 s (`MAX_NON_ENABLED_TTL_SEC` in `pancetta-agent/src/capability.rs`).
- Until panino and cqdx `/rig` ship these changes, the station refuses their connections
  (`reason=IdentityProofMissing`). A client that never re-sends `hello` stops receiving data when
  its token expires.
- The pancetta PR merges only after the dispensa question below is filed.
- Order: file the dispensa question, merge and release pancetta, clients ship the proof and
  re-hello, then a live staging round trip through `relay.cqdx.io`.

## Alternatives considered

- **cqdx delivers the client's identity and agreement keys on authorization edges:** no coverage
  for stations using a static-config `tx_allow_list`, and handshake success would depend on poll
  freshness.
- **Trust-on-first-use pinning of a client's static key:** the first contact is unverified.
- **A `cnf` key-binding claim in `capabilityToken`:** binds at `hello` time, after per-peer state
  exists, and makes the IdP the authority for the binding.
- **A config opt-out for the proof requirement:** a new config surface and a security downgrade
  that is easy to leave on.
- **A one-time `error` frame telling a client its capability expired:** it would be the one frame
  sent to an unauthorized peer, breaking the "no frame of any kind" rule; clients re-send `hello`
  before `exp` instead.

## Proposed dispensa question

Ready to file as `dispensa/questions/Q-NNNN-client-identity-proof-in-noise-msg1.md`; assign the
number when filing.

````markdown
# Q-NNNN: Client identity proof in Noise IK msg1, and re-hello before token exp

Status: Open
From: pancetta
To: cqdx, panino
Affects: contracts/auth/e2e-auth.v1.schema.json

### Question

pancetta's station agent now ties each relay peer to a device identity proven inside the Noise IK
handshake, and sends a peer nothing until it holds a valid `hello` capability. This needs a small
amendment to `e2e-auth.v1`. We ask cqdx and panino to concur on three points.

#### (a) msg1 payload: client identity proof

The Noise IK msg1 payload (empty today) carries a UTF-8 JSON object of at most 1024 bytes:

    {"v":1,"identityPublicKey":"<b64url>","idSig":"<b64url>"}

- `v`: integer, must be `1`; any other value is refused.
- `identityPublicKey`: raw 32-byte Ed25519 identity public key, unpadded base64url (pairing.v1
  encoding).
- `idSig`: raw 64-byte Ed25519 signature, unpadded base64url. It is the device's existing
  pairing.v1 `idSig`: Ed25519 over `"cqdx-pair-idsig-v1" ‖ 0x00 ‖ agreementPublicKeyRaw`.
- Unknown fields are ignored.

The station, before sending msg2: checks `src` against its allow-list and capacity; decrypts msg1
and reads the initiator static key; verifies `idSig` (`verify_strict`) over
`"cqdx-pair-idsig-v1" ‖ 0x00 ‖ <initiator static key>`; derives
`keyId = b64url_nopad(SHA-256(302a300506032b6570032100 ‖ identityPublicKey))`; and requires
`keyId == env.src`. Any failure: no msg2, no state, no reply.

#### (b) Noise initiator static = enrolled agreement key

A client's Noise IK initiator static key must be the X25519 key it enrolled as pairing.v1
`agreementPublicKey`. A client using any other static key must switch.

#### (c) Re-send hello before token exp

A peer receives no frames (events, `controlState`, `error`) until it sends a `hello` whose
`capabilityToken` verifies, names its proven keyId as `clientKeyId`, carries `status` or higher,
and is unexpired. When that token's `exp` passes, delivery stops until a new `hello` arrives.
Clients must re-send `hello` with a fresh token before `exp` (non-TX tokens are at most 900 s).
`qsy`/`setSplit` likewise require an unexpired `qsy`+ capability.

### Proposal

Amend `contracts/auth/e2e-auth.v1.schema.json` with (a)-(c), and add
`contracts/auth/client-identity-proof.vectors.v1.json` with this example:

- Ed25519 identity seed `0x22`×32, X25519 agreement secret `0x23`×32
- agreementPublicKey `mkUDqYqxD-jTVMnELL0MnXlE9S59FNjqWXdefcnjv0s`
- keyId `dE82zKZ-sZEssoLgjoa9L-DQkASijPbnxr1Wzda_Zao`
- msg1 payload (170 bytes):
  `{"v":1,"identityPublicKey":"oJql9HpnWYAv-VX43C0qFKXJnSO-l_hkEn_5ODRVpPA","idSig":"tjDTm307i52bJPpXjlGKKS5p2Qvj8p0dFgQgJ9eNKwyq0GOMSAv4fMnJu7u_2wG4bsFIVFa1E_u44qKh_XaLCQ"}`

### Status

pancetta: implemented (PAN-95), spec
`docs/superpowers/specs/2026-10-12-client-identity-binding-design.md`. Clients that do not send
the proof are refused with `reason=IdentityProofMissing` in the station log.

cqdx: pending.
panino: pending.
````
