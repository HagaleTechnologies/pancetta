# Security Review — Remote-Rig-Control Stack (2026-07-02)

**Status update (2026-07-25):** of the four load-bearing pre-flight items in the Bottom Line
below, three are resolved. (a) The Noise-static/read-qsy relay-admission gap and (c) the
`clientSig` domain-separation tag both shipped in pancetta PR #73 (merged 2026-07-03, adopting
dispensa Q-0019 #5/#6 — see `contracts/auth/e2e-auth.v1.schema.json` and cqdx PRs #143/#144). (d)
The `e2e-auth.v1` §4/§6 wording contradiction was fixed in dispensa's own contract the same day
(2026-07-02, Q-0018 resolution) — never a pancetta change. (b) Agent-key rotation/revocation has
`dispensa/adr/0010-key-rotation-and-keyid-revocation.md` (Proposed; pancetta + panino gave
concurring positions 2026-07-03) but the registry/relay-side plumbing is explicitly cqdx's to
build; pancetta's only follow-on (an `AuditKind` removal-actor distinction) is blocked on a seam
(`ArmContext.revoked_jtis`) that is still inert. No pancetta-side code is actionable on (b) right
now. **No open pancetta engineering work remains from this review's four gate items.**

Deep review of the whole remote-rig stack: `pancetta-agent` (arm/audit/noise/keys/pairing/
capability/relay/session/control) + `pancetta/src/coordinator/station_agent/{mod,net}` + the
coordinator TX gate (`tx.rs`) + QSO-origin routing (`qso.rs`). Anchored to the three findings
panino raised from its own audit (dispensa **Q-0018** CRITICAL, **Q-0019** batch, **Q-0017**
reframe). Every claim below was verified against pancetta source, not the contract prose.

Nothing is deployed: `[network.station_agent].enabled` and `remote_tx_enabled` default OFF,
`REMOTE_RIG_ENABLED=0` everywhere, TX-live gated behind the still-unbuilt passkey step-up. This
is pre-flight hardening, not a live-incident review.

## Headline

**The crown-jewel guarantee holds in the pancetta implementation: a cqdx/relay/cloud compromise
alone cannot key a victim's transmitter.** The TX-arm path is rooted in three cryptographically
independent things a cloud breach does not control — the client's **device Ed25519 key** (signs
the grant), the **station-local TX-allow-list** (static config, never cloud-synced), and the
**pinned IdP key** (verifies the capability token). Two auditors independently traced the arm
path and the live PTT gate and found no bypass frame.

**One real Medium finding**: the **read / qsy / setSplit** (non-TX control) path is authorized by
*relay admission*, not by the Noise client identity — the agent never binds the Noise remote
static to the allow-listed `clientKeyId`. This does not affect TX (independently gated) but a
full cqdx/relay compromise could drive `setSplit`/`qsy`. Fix is a Noise-static pin + scope check,
and needs a small cross-repo contract touch (deliver the token on the read path). Not yet built.

## Q-0018 (CRITICAL, allow-list re-sync contradiction) — pancetta is NOT vulnerable

The attack requires cloud-forged graph state to *add* an attacker's `clientKeyId` to the victim's
TX-allow-list. **That mutation path does not exist in pancetta.**

- The allow-list is a static local-config `Vec<String>` (`network.rs:299`, `#[serde(default)]`,
  empty), collected into a `HashSet` **once** at component start
  (`station_agent/mod.rs:541`), moved read-only into `ArmContext.tx_allow_list`. Every use is
  `.contains(...)`; there is no `.insert`, no reassignment, no reconnect refresh outside
  `#[cfg(test)]`.
- Repo-wide grep: no `authorizationEdge`, no allow-list `resync`/`re-sync`, no network→allow-list
  write. `PairedState` holds only `agent_key_id` + pinned `idp_keys` — pairing cannot inject
  entries. The documented future deny-list seam (`ArmContext.revoked_jtis`) is `HashSet::new()`,
  never populated, and only *restricts* (safe direction).
- The arm chain (`capability.rs::verify_arm_grant`) requires, correctly ordered and fail-closed:
  `require_tx_enabled` (txEnabledUntil present ∧ > now) → deny-list → capability token verified
  against **pinned** IdP key as a **separate** input (not read from the grant; `alg==EdDSA`,
  `verify_strict`, `aud`, `exp`, TTL/enablement backstops) → `clientSig` `verify_strict` against
  the client **device** key loaded from local disk → `clientKeyId ∈ allow-list` (checked twice) →
  jti/capabilityJti/window/heartbeat/scope binds. Arm-mutex poison fails closed.

**Verdict: the §4/§6 contradiction is a documentation defect in `e2e-auth.v1`, not a pancetta
vulnerability.** Recommend dispensa fix §6 wording (cloud may only *shrink* TX authority; adding a
`clientKeyId` needs out-of-band station-local consent) so no future implementer builds the
graph-resync.

## Q-0017 (agent-static MITM) — pancetta side is complete

The agent produces and enrolls the full verifiable triple: at enroll it signs its
`agreementPublicKey` with `idSig` under `cqdx-pair-idsig-v1` and POSTs
`{keyId, identityPublicKey, agreementPublicKey, idSig}` (`pairing.rs:234-248`; `keyId` = SHA-256
of the Ed25519 SPKI). So a client *can* verify `deriveKeyId(identityPublicKey)==agentKeyId` and
`idSig` over the Noise static — the binding is fully derivable. The remaining work is **cqdx's**
(serve the triple, not just raw X25519, on `/capability` or `/agents/{id}`) and the **client's**
(verify + TOFU-pin). No pancetta change needed beyond posting `identityPublicKey`+`idSig` to
Q-0017 so panino can build/verify against the real values.

## Q-0019 batch — per-item pancetta position

1. **txEnabledUntil is a token-theft control, not a cqdx-breach control** — agree; re-document.
   pancetta enforces it (`require_tx_enabled` first in the arm chain) but the real cqdx-breach
   backstop is the Q-0018 station-local allow-list.
2. **24h enabled-window + fail-open offline deny-list** — real. pancetta's deny-list is inert/
   empty today. When wired, consider fail-**closed** on enabled-token arming once the deny-list is
   staler than N minutes unless the operator opts into offline-armed operation. (pancetta-owned
   when the seam is populated.)
3. **CSRF/Origin is cqdx's server gate** — pancetta only sends a spoofable compat `Origin` header
   (default None = prod; staging opt-in) and relies on it for nothing; real auth is the Ed25519
   PoP + server-side keyId recomputation. cqdx to ship the Bearer/API-key exemption.
4. **No agent/client key rotation or compromise-recovery** — CONFIRMED real gap. `keys.rs`
   generates once and persists; a stolen `identity.key`+`agreement.key` lets an attacker
   authenticate to the relay as the agent and complete Noise as the agent. Recovery is manual
   (delete `key_dir`, re-pair, operator removes stale `agentKeyId` on cqdx). Needs an ADR: agent
   key rotation + a keyId-revocation path distinct from edge deletion.
5. **Read/qsy authz must be rooted in the E2E client static, not relay admission** — CONFIRMED,
   see the Medium finding below. Currently rooted in relay admission.
6. **Give `txArmGrant.clientSig` a domain-separation tag** — concur (interop-locked). pancetta's
   **signer is uniformly tagged** (`sign_domain` is the only signing method; no bare-byte signer =
   no cross-protocol oracle). pancetta only *verifies* `clientSig` over bare canonical JSON — the
   sole untagged signature in the system, safe today only by first-byte disjointness (`{`=0x7b vs
   `cqdx-`=0x63). pancetta will adopt `domainSep("cqdx-tx-arm-grant-v1", canonicalGrantBytes)` +
   re-vector on the group's concurrence; verify is a one-line change.

## Medium finding — read/qsy/setSplit authorization is relay-admission-rooted

`dispatch_action` handles `Qsy` and `SetSplit` with **no authorization check** and **no scope
check** (`station_agent/mod.rs:226-244`) — reaching the dispatch requires only that the frame
passed the `env.src` guard and decrypted under the Noise transport. Neither binds a cryptographic
client identity:

- The `src` guard (`session.rs:221-225`) compares the **relay-DO-stamped `src`** to the pinned/
  learned `client_key_id` — that is the relay's word (`relay.rs:117`, "the DO stamps `src`"). A
  compromised relay/DO can stamp `src = <allow-listed clientKeyId>` on a malicious client's frames.
- The Noise handshake **never authenticates the initiator's static**: `ResponderHandshake::
  read_msg1` (`noise.rs:82-90`) never calls `get_remote_static()`, and nothing compares the Noise
  remote X25519 to the allow-listed `clientKeyId`. Noise IK here gives confidentiality +
  *responder* auth (client knows it's the real agent), but the agent does not authenticate *who*
  the client is.
- No `capabilityToken` rides the read/qsy path (it only rides `txArm`), so `qsy`/`setSplit` are
  not even scope-gated — a legitimate `status`-only client could issue `qsy`.

**Impact.** A full cqdx/relay compromise (which holds the agent's X25519 static via enrollment and
can stamp `src`) could open a Noise session and drive `setSplit` (sets the rig TX dial) or `qsy`.
It cannot key TX (independently gated), but `setSplit` moving the TX dial is a plausible
out-of-band-emission vector via a subsequent *local* (non-arm-gated) transmission. Severity
**Medium**: non-TX, pre-deployment, but a genuine relay-root gap contradicting "relay/cloud
compromise alone can never cause control."

**Peer-learning composition** (the `a22c745d` change, this review's other input): learning
`client_key_id` from the first DO-stamped `env.src` trusts the relay's claim and does not bind to
the Noise static — but it composes **safely with TX** (arming is independently rooted in the
client device Ed25519 signature + allow-list + pinned token, none of which touch `env.src`), and
is **production-dormant** (the component pins `client_key_id` from the allow-list at start, so the
empty→learn branch only fires in tests). It does not create a new TX hole; it does inherit the
pre-existing relay-root gap on read/qsy.

**Recommended fix (needs a small cross-repo contract touch):** pin the client's X25519 Noise
static at pairing and, in `read_msg1`, assert `get_remote_static() == pinned_static_for(
client_key_id)` before establishing transport — rooting read/qsy identity in Noise, not the relay
`src` stamp. Add capability-scope checks before honoring `qsy`/`setSplit`, which requires the
token to ride a control frame (or the read `hello`), not only `txArm`. Coordinate the token-on-
read-path shape with cqdx (dispensa) before building.

**Doc/code mismatch to fix first:** `mod.rs:30-32` documents a read/snapshot stream, but
`send_control` is never called in production (`run_one_session` only does auth→handshake→dispatch)
— the read feed is not actually wired yet. Fix the authz root **before** wiring it, so it doesn't
ship relay-rooted.

## Minor / LOW (local, non-blocking)

- **operatorCallsign provenance** (`capability.rs:553`): `VerifiedArmGrant.operator_callsign` is
  taken from the client-signed grant, not cross-checked against the IdP token's `operatorCallsign`
  claim. Not a TX bypass (client already armed) but weakens Part-97 audit attribution — bind
  `grant.operatorCallsign == capability.operatorCallsign`.
- **Session-scoped replay set** (`seen_jtis`, reset on reconnect): a grant `jti` replayed across a
  reconnect boundary isn't caught by the set; bounded by `armedUntil ≤ now+10min` + tx-enabled
  window + required valid `clientSig`. Low, but the single-use guarantee is per-session not durable.
- **Key-file perm TOCTOU** (`keys.rs:209-218`): `fs::write` then `set_permissions(0600)` leaves a
  brief umask-wide window on `identity.key`/`agreement.key`. Prefer `OpenOptions.mode(0o600).
  create_new(true)`. Also `persist()` only tightens the dir to 0700 when it *creates* it.
- **No future-`iat`/`nbf` rejection** in `verify_capability_token` (only `exp<=now`). Minor.
- **Nondeterministic peer pin** (`mod.rs:553`, `tx_allow_list.iter().next()`): arbitrary HashSet
  element becomes the session peer — latent correctness bug once the allow-list grows past the
  documented single-client v1. Make it an explicit configured peer.
- **`src: None` skips the guard** (`session.rs:221`, `if let Some(src)`): trust-the-relay
  assumption; add a defensive reject-absent-src-once-admitted.
- **Allow-list not hot-reloaded**: loaded once at start; a changed `tx_allow_list` needs a
  restart. Safe direction (stale = denies), but an ops sharp edge.

## Bottom line

TX enforcement is sound and fail-closed — Q-0018 does not bite pancetta. The load-bearing pre-flight
items are cross-repo: (a) close the read/qsy relay-root gap with a Noise-static pin + scope check
(pancetta build + small contract touch), (b) add agent-key rotation/revocation (dispensa ADR), (c)
adopt the `clientSig` domain-sep tag (interop-locked), (d) fix the `e2e-auth.v1` §4/§6 wording.
None require action before the current staging round-trip; all are required before any production
`REMOTE_RIG_ENABLED` flip.
