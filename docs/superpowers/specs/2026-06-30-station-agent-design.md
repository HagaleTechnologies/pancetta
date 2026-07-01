# Pancetta Station Agent (remote rig control) — Design Spec

**Date:** 2026-06-30
**Status:** **Proposed — awaiting operator review** (security-critical / ADR-0002-governed; NOT self-approved)
**Author:** Claude Opus 4.8 (under K5ARH supervision)

> This spec deliberately does **not** re-decide any security question. The security & auth model is
> **dispensa ADR-0002 (Accepted)** + **e2e-auth.v1 (Adopted)** + the pairing/relay concurrence pancetta
> filed 2026-06-30 (dispensa Q-0010/Q-0011). This spec is pancetta's **implementation architecture** on
> top of those already-agreed decisions. Where a choice touches the security model, it cites the ADR
> rather than inventing.

## Goal

Give a pancetta station (headless, at the rig) a **station agent**: an outbound-only, end-to-end-encrypted
remote-control endpoint that lets an authenticated, *present* remote operator (panino) **read status, tune/QSY,
and — under an explicit, time-boxed, re-auth-gated arm — transmit**, while the **station remains the final,
independent TX authority** (a relay/cloud compromise alone can never key the transmitter). This is the
counterpart to the read-only `remote_gateway` v1 (ADR-0009) — it adds the *control* path ADR-0002 governs.

## What already exists (build ON this, don't duplicate)

- **Read-only gateway v1** (`coordinator/remote_gateway/`, ADR-0009): outbound? No — it's a **localhost WS
  server**, default-off, read-only. The agent is a **different** component (outbound WSS dialer to the cqdx
  relay DO), though it can reuse the bus→protocol `translate.rs` for the read stream.
- **The coordinator's TX worker** (`coordinator/tx.rs`) is *already the single choke point that keys PTT*,
  and already honors `TxPolicy` (`Full|RespondOnly|Disabled`) + the drop-stale-TX gate. This is exactly where
  the agent's **armed-TX gate** belongs — the agent never touches PTT directly; it can only *request* TX via
  the same bus path the TUI uses, and the TX worker enforces the arm.
- **Local-kill-primacy already shipped:** `TxPolicy::Disabled` + Shift+Q emergency stop. ADR-0002 §5(d)
  is satisfied by construction — the local control point always wins.
- **`pancetta-protocol`** crate: the rig-api.v1 wire types (camelCase) for the read stream.
- **Noise vector PASS:** `snow` reproduces `contracts/auth/noise-ik.vectors.v1.json` byte-for-byte
  (verified 2026-06-30) — the E2E crypto primitive is proven.

## Architecture

A new **`pancetta-agent`** crate + a coordinator component `start_station_agent_component` (mirrors
`start_remote_gateway_component`: disabled → drain; enabled → dial + serve). Layered so each layer is
testable and the security-critical gate is small and auditable.

```
 cqdx relay DO (per-rig, opaque pipe)  ⇄  outbound WSS
        │  relay.v1 frames: hello/auth/ready/presence/env/bye/error
        ▼
 [L1 Relay transport]  — dials out, handles the agent auth leg
        │  env.payload (opaque)
        ▼
 [L2 Noise session]    — Noise_IK_25519_ChaChaPoly_BLAKE2s (snow); agent = responder
        │  decrypted control frames (rig-api.v1 clientCommand / serverEvent)
        ▼
 [L3 Capability + arm gate]  — verify capabilityToken (pinned idpKeys); enforce scope;
        │                       TX requires an explicit, time-boxed ARM + dead-man heartbeat
        ▼
 [L4 Command execution]  — maps allowed commands onto the EXISTING coordinator bus
        │  (QSY/tune → RigControl; TX-request → the same TransmitRequest path the TUI uses,
        │   gated by the TX worker's arm check); read stream ← translate.rs
        ▼
 coordinator message bus  →  TX worker (final PTT gate) / hamlib / QSO engine
```

### L1 — Relay transport (`relay.v1`, BLOCKED on cqdx authoring the schema)
Outbound persistent WSS to the per-rig DO (`idFromName(agentKeyId)`). Implements the **agent leg** pancetta
concurred to (Q-0011): receive `hello{challenge}` → send `auth{role:"agent", agentKeyId, sig}` where
`sig = Ed25519(identity, "cqdx-relay-agent-auth-v1\0" || challenge)` (**domain-tagged** per our counter) →
accept `ready` → observe `presence`. Forwards L2 ciphertext in `env.payload` (unpadded base64url). Reconnect
with backoff; `NO_PEER`/`UNKNOWN_DST` transient, admission errors terminal. Frame types are a
`#[serde(tag="t")]` enum. **Cannot be finalized until `contracts/relay/relay.v1` is recorded by cqdx.**

### L2 — Noise E2E (`snow`, buildable NOW)
`Noise_IK_25519_ChaChaPoly_BLAKE2s`, agent = **responder** (holds the static `s` the client pre-knows from
pairing). Handshake msg1/msg2 + transport ride in `env.payload`; the AEAD counter is the only replay/ordering
control (no relay `seq`, per our concurrence). This layer is schema-independent — the interop vector is frozen.
**First brick = land the conformance test in-repo** (drift-guarded, like panino/cqdx have).

### L3 — Capability + ARM gate (the security core; partly BLOCKED on pairing.v1)
- **Pairing client** (`pairing.v1`, BLOCKED on cqdx): agent generates its keypair on first run (private key
  never leaves disk), enrolls via a single-use pairing code, pins the returned `idpKeys` set. Flat two-key
  body + `challengeId` + domain-tagged `idSig` (our concurrence).
- **Capability verification:** every control frame carries a capabilityToken; the agent verifies it against a
  **pinned** idpKey (reject if not pinned — no silent re-fetch), checks audience = this agent, scope, expiry.
- **The ARM state machine (ADR-0002 §5, the invariant):** TX is a *distinct* capability that must be
  **explicitly armed**, **time-boxed** (`armed for N minutes`), **re-auth-gated**, and **dead-man bound**:
  - (a) **Dead-man / heartbeat:** armed-TX requires a live authenticated operator session with a heartbeat;
    on heartbeat/idle timeout or channel loss → **auto-disarm (fail TX-off)**. §97.109(c).
  - (b) **No unattended remote origination:** the remote path exposes *human-driven* TX only; autonomous CQ
    origination is never remotely armable. §97.221.
  - (c) **Operator-callsign attribution + append-only audit log:** record who armed TX and what was sent.
    §97.103/.105.
  - (d) **Local-kill-primacy:** the station's `TxPolicy::Disabled`/Shift+Q instantly disarms and always wins;
    the agent observes and cannot override it. §97.103.
  This gate is a **new `Arc<AtomicU8>`-style arm state** the TX worker consults *in addition to* `TxPolicy`,
  set only by L3 on a valid arm and cleared by any of: disarm command, heartbeat loss, TTL expiry, local kill.

### L4 — Command execution (mostly buildable NOW behind the gate)
Maps the allowed rig-api.v1 `clientCommand`s onto the **existing** coordinator bus — no new rig control code:
- read status → the existing `translate.rs` read stream (reuse gateway v1's mapping).
- QSY/tune/split/mode → existing `RigControlMessage` / band-change / `SetSplit` / `[rig].mode` paths.
- TX request → the **same `TransmitRequest` bus message the TUI emits**, but the TX worker only keys it if the
  L3 arm is live (a `qso_id == None` remote free-text/QSO TX is dropped when disarmed, exactly like the
  drop-stale-TX gate drops ended-QSO TX). **The agent never calls PTT.**

## Build phasing (what's buildable now vs blocked)

| Phase | Deliverable | Blocked on |
|-------|-------------|-----------|
| **P0** | `pancetta-agent` crate + **Noise conformance test** (snow vs the shared vector, drift-guarded) | nothing — DO NOW |
| **P1** | L2 Noise session wrapper (handshake responder + transport) + unit tests (loopback initiator/responder) | nothing |
| **P2** | L3 ARM state machine + TX-worker arm gate + dead-man/heartbeat + audit log — **pure, unit-tested, no network** (drive it with a mock control channel) | nothing (the *safety* core can be built + tested offline first) |
| **P3** | L1 relay transport (`relay.v1`) + L3 pairing client (`pairing.v1`) | **cqdx records `contracts/relay/relay.v1` + `contracts/pairing/pairing.v1`** (from our concurrence) |
| **P4** | Wire-up component + config `[network.station_agent]` (default OFF) + coord_sim end-to-end + on-air arm/disarm test | P1–P3 |

Phasing lets pancetta build the **safety-critical L3 gate and L2 crypto first, fully offline and tested**,
so by the time cqdx freezes the wire we're assembling proven parts — the highest-risk code (the TX arm) is not
rushed against a network deadline.

## Scope / non-goals (v1)
- **Default OFF**, opt-in config, localhost pairing enrollment only. No autonomous mode over remote (ever).
- No browser/panino-specific code (that's panino's client); pancetta is the **agent** end only.
- Reuses the coordinator's existing rig-control + TX paths; adds NO new way to key PTT.
- Delegation edges (ADR-0002 §5 multi-op) are honored at the token layer (the agent just verifies the
  capabilityToken's scope/subject) — no separate pancetta delegation UI in v1.

## Risks / careful points
1. **The TX arm gate is the crown jewel.** It must fail-safe (any doubt → TX-off), be independent of the
   relay/cloud (L3 enforces locally), and never be settable by a decrypted frame alone without a valid,
   pinned-key-verified, unexpired, in-scope capabilityToken **and** a live heartbeat. Build + adversarially
   test it offline (P2) before any network.
2. **Local-kill-primacy must compose** with the existing `TxPolicy`/Shift+Q — the arm is *ANDed* with policy,
   never ORed; `Disabled` disarms.
3. **Contract drift:** L1/L3 wire types must be generated from / drift-guarded against the frozen
   `relay.v1`/`pairing.v1` (adopt-then-Accept discipline, like rig-api.v1).
4. **Key storage:** agent private key on disk with strict perms; never logged; never uploaded (ADR-0002 §4).
5. **Audit log:** append-only, local; records arm/disarm/TX with operator callsign + timestamp.

## Testing
- P0/P1: Noise vector byte-for-byte (in-repo) + a loopback initiator↔responder handshake+transport test.
- P2: exhaustive ARM state-machine unit tests — arm requires valid token; expires on TTL; disarms on heartbeat
  loss / local kill / disarm; TX dropped when disarmed; `Disabled` policy overrides an active arm; audit entries
  emitted. Adversarial: forged/expired/wrong-audience/unpinned-key token never arms; a decrypted frame without a
  token never arms.
- P4: coord_sim rig-level — armed remote TX keys PTT; disarm mid-slot drops the pending TX; heartbeat-loss
  auto-disarm; local Shift+Q wins over an active remote arm.

## Open questions for operator review
1. **Crate name** `pancetta-agent` (proposed) vs `pancetta-station-agent`.
2. **Build now vs wait:** proposed = build P0–P2 now (offline, safety-critical, unblocked), hold P3/P4 for
   cqdx's frozen contracts. Confirm you want the offline safety core built ahead of the wire.
3. **ARM UX at the station:** should the *local* operator have to consent to a remote arm (a station-side
   "allow remote TX" toggle, default off, independent of the token), as an extra Part-97 control-operator
   belt-and-suspenders? (Proposed: yes — a `remote_tx_enabled` local gate, default OFF, ANDed with everything.)
4. **Audit log location/format:** `~/.pancetta/agent-audit.log` append-only JSONL? (Proposed: yes.)
