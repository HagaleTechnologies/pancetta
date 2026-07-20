# Concurrent Multi-Client Station Agent — Design Spec

**Date:** 2026-07-20
**Status:** Approved (design reviewed and accepted by K5ARH 2026-07-20). Supersedes the
"Proposed shape" section of `2026-07-20-multi-client-station-agent-design.md` (the earlier
scoping doc); everything else in that doc — the relay-side confirmation, the what's-single-peer
inventory — still stands and is not repeated here.
**Author:** Claude Fable 5 (under K5ARH supervision)
**Related:** dispensa Q-0043; `2026-07-01-station-agent-p3-design.md` (the security spine this
must not disturb); pancetta PR #180 (dynamic single-client selection, merged — the learn-then-vet
admission gate this design generalizes per-peer)

## Goal and scope

Up to `MAX_CLIENTS` (8) concurrently connected, independently-Noise-sessioned clients on the one
relay websocket — the relay contract's existing cap, explicitly accepted by the operator as
sufficient ("if the contract caps at 8 and that's reasonable, that's fine"). Exactly **one**
client at a time holds *control* (arm/PTT, QSY, split, TX-initiation); every other connected
client is a live **read-only viewer**. All clients are station-local `tx_allow_list` members —
this is N of the operator's own paired devices, not public spectators.

**No relay or cqdx-side changes.** The relay already fans in up to 8 client legs and routes `env`
frames by keyId (`relay.v1`, Q-0021 #1); `presence` frames carry the affected `peer` keyId and an
`up`/`down` state (confirmed against the schema — per-peer lifecycle is fully contract-supported).

Decisions made during design review:

- **Control model: explicit controller, free grab.** One controller at a time; any admitted
  client can take control at any moment (single-operator assumption). Chosen over
  "last-action-wins" (no way for UIs to show who's driving; stale tabs can act by accident) and
  over "locked until release" (recreates the walk-to-the-other-device annoyance).
- **Architecture: `MultiPeerSession` in `pancetta-agent`.** Library-level demux next to
  `AgentSession`, unit-testable with multi-peer mocks. Chosen over a coordinator-level dispatcher
  wrapping N `AgentSession`s behind fake per-peer `WsConn`s (leaky — relay-level frames belong to
  no one peer) and over per-peer async tasks + channels (locks/actor plumbing that 8 peers of
  low-rate control traffic doesn't justify).

## Component 1: `MultiPeerSession` (`pancetta-agent`, new module beside `session.rs`)

Owns the single `WsConn` and the relay leg (hello → auth → ready, unchanged — still one agent
leg). Demuxes inbound frames:

- `env` → route by DO-authenticated `src` to that peer's state: `ResponderHandshake` msg1/msg2 →
  `NoiseTransport`, exactly `AgentSession`'s per-peer machinery, held N times in a
  `HashMap<String, PeerState>`.
- **Admission:** an unknown `src` is checked against the allow-list (`HashSet<String>` passed at
  construction — the station-local `tx_allow_list`, same authoritative-list-not-relay-admission
  posture as PR #180) *before* any handshake state is created. Not a member → frame dropped +
  `warn!`, no state allocated (an unlisted peer cannot even cost us a handshake). Map full
  (defensive; the relay caps at 8 first) → dropped + `warn!`.
- `presence { peer, state: down }` → remove that peer's state only; report the removal to the
  caller (the coordinator decides whether it was the controller). `up` → informational.
- `error`/`bye`/socket close → terminal for the whole session, as today.

API (shape, not signatures): `process_next() → Poll` where `Poll` distinguishes
`Plaintext { peer, bytes }`, `PeerEstablished { peer }`, `PeerDown { peer }`, `Idle`
(relay-phase progress), and `Closed`; plus `send_to(peer, plaintext)` and
`broadcast(plaintext)` (encrypt once per established peer's transport, each in its own `env`
with `dst` = that peer). Per-peer phase tracking replaces the current whole-session
`session_phase` forward-progress heuristic; socket-drain detection stays at the socket level
(`recv_text → None`), which is cleaner than today's phase-comparison workaround.

`AgentSession` is **kept untouched** as the tested single-peer reference (its unit tests, and the
handshake/transport internals `MultiPeerSession` reuses — factor shared per-peer handshake logic
into a common private helper if extraction is clean, otherwise tolerate the duplication and note
it). The coordinator switches to `MultiPeerSession` unconditionally: one connected client is
simply N=1.

## Component 2: controller role (`pancetta/src/coordinator/station_agent`)

`ArmContext` changes: `expected_client_key_id: String` and `hello_scopes: Option<String>` become
per-peer (moved into or keyed by the peer map); new `controller: Option<String>` (keyId).

Rules, in order of precedence:

1. **`takeControl`** (existing reserved verb, currently a no-op) from any admitted,
   transport-established peer **always succeeds** — free grab. If it displaces a different
   controller whose arm is live, the arm is **disarmed first** (audited). Arms never transfer:
   the new controller must arm fresh through the full capabilityToken + clientSig grant
   verification.
2. **`releaseControl`** from the current controller clears `controller` (and disarms if armed —
   control loss is TX-off). From a non-controller: logged no-op.
3. **Implicit grab (backward compatibility):** a control-mutating action (`Arm`, `Qsy`,
   `SetSplit`, `TxRequest`, `StopCq`) from peer P when `controller == None` sets
   `controller = Some(P)` and then proceeds. Today's panino never sends `takeControl`; a single
   client connecting and arming works byte-for-byte as it does now.
4. **Exclusivity:** a control-mutating action from a non-controller while a controller is
   assigned is refused — `warn!`, audited, and answered with an error frame to that peer (so
   its UI can render "take control first"). It does NOT implicitly grab.
5. **Safety asymmetries** (deliberate, reviewed):
   - **`Disarm` is accepted from ANY admitted peer**, controller or not — fail-safe TX-OFF beats
     exclusivity (consistent with disarm-any's existing posture).
   - `Heartbeat` is accepted from any peer; `ArmState`'s existing `arm_jti` + monotonic-`seq`
     binding already guarantees only the armer's heartbeats slide the window.
6. **Controller loss ⇒ disarm:** controller peer `down`, whole-session teardown, or component
   shutdown → `controller = None` + disarm (generalizes today's `disarm_on_loss`; a mere
   *listener* dropping disarms nothing).
7. `Hello`/scope verification runs per-peer exactly as today (each peer presents its own
   capability token; scopes gate that peer's `qsy`/`tx` actions).

## Component 3: read stream over the relay (new work)

Ground truth: the relay leg is **control-only today** — the module doc's "read stream (minimal
v1)" bullet is aspirational. The read-only view (decodes, QSO progress, scalar status) exists
only in the localhost `remote_gateway` component, which already has the `translate` layer
(bus `MessageType` → `pancetta_protocol` `ServerEvent`) and an additive bus seam
(`relay_to_gateway`, placed after every existing `→Tui` send — additive-only invariant).

This design adds the relay-side read stream by **sharing the gateway's translation pump rather
than duplicating it** (planning-time refinement of the approved "single fan-out seam" idea —
same property, less duplication):

- The pump (`handle_bus_msg`: bus → `ServerEvent` incl. decode enrichment with dial frequency /
  station-lookup / snapshot folding) is hoisted out of the gateway-enabled path into a shared
  **display feed** started when *either* the localhost gateway *or* the station agent is active.
  The coordinator's `gateway_enabled` emit-site flag generalizes to `display_feed_enabled`
  (same `Arc<AtomicBool>`, set by either component) — emit sites keep exactly one call, the TUI
  path is untouched, and the gateway's HTTP server stays gated on its own config flag.
- The station agent subscribes a `broadcast::Receiver<ServerEvent>` to the feed's existing
  `evt_tx` and, between (timeout-bounded) control-frame reads, drains it and `broadcast()`s each
  event as `ServerFrame::Event` JSON inside per-peer-encrypted `env` frames — the same rig-api.v1
  wire types panino already speaks over the localhost gateway. (The `WsConn` seam gains a
  timeout-bounded receive so the loop can interleave; today's `recv_text` blocks indefinitely.)
- **Control-state visibility needs NO new protocol event**: rig-api.v1 already defines
  `ServerEvent::ControlState { controlHeldByMe, transmitArmed }` (receiver-relative). The agent
  sends each peer its own `ControlState` on every controller/arm transition and on session
  establishment; refusals ride the existing `ServerEvent::Error`. Nothing additive on the wire —
  the dispensa note becomes purely informational.
- Backpressure: the tokio `broadcast` channel's bounded ring is the drop-oldest queue (`Lagged`
  = events skipped, logged); the read stream is lossy by design and control frames are never
  queued behind it.

## Component 4: `CAPACITY` terminal-code fix (bundled)

`pancetta-agent/src/relay.rs` carries 11 of the contract's 12 terminal codes — `CAPACITY` (sent
by the relay on a 9th-client attempt) is missing. It's this feature's own error surface, so the
one-line fix and its test ride along here rather than as a separate PR.

## Security invariants

Unchanged and untouched: the 5-check TX-authority chain (relay admission → Noise E2E →
capabilityToken → txArmGrant clientSig → local `ArmState` + consent + heartbeat window), the
armed-TX gate failing CLOSED, `TxOrigin::Remote` tagging, drop-stale-TX. The controller role is
an exclusivity layer *in front of* the arm path; it never substitutes for any check.

New invariants (added to the tested set):

- A peer not in `tx_allow_list` never allocates handshake state, never receives the read stream,
  and never reaches dispatch.
- Control-mutating actions execute only for the controller (or a peer becoming controller via
  rules 1/3).
- An arm never survives a controller transition, and never transfers between peers.
- A listener disconnect never disarms; a controller disconnect always does.

## Error handling

- Per-peer Noise/handshake failure or malformed frame: that peer's state is dropped/reset;
  everyone else's session is unaffected (strictly better isolation than today's whole-session
  teardown on any error).
- Whole-socket loss: existing teardown → disarm → reconnect-with-backoff loop, unchanged; all
  peer state clears (peers re-handshake on reconnect, controller starts `None`).
- Poisoned arm lock: fails CLOSED, as everywhere else.

## Testing

- `pancetta-agent`: new multi-peer mock (interleaved scripted frames from 2–3 peers). Tests:
  independent handshakes complete; plaintext routes to the right peer (no cross-talk); unknown
  `src` dropped without state; `presence down` removes exactly one peer; `broadcast` produces one
  correctly-`dst`'d `env` per established peer; capacity guard.
- Coordinator (`station_agent` tests, extending the existing harness): implicit grab on arm
  (single-client compat); explicit `takeControl` grab + takeover-disarms-prior-arm;
  non-controller QSY/arm refused + error frame emitted; disarm-from-listener accepted;
  controller `down` disarms, listener `down` doesn't; `controllerChanged` broadcast on each
  transition; read-stream fan-out reaches all established peers.
- All existing single-peer `AgentSession` + `station_agent` tests stay green, unmodified.

## Out of scope

- **Hundreds/public listeners** — a cqdx-side broadcast product (publish-once, fan-out at the
  edge), needs its own cross-repo design if ever wanted; the 8-cap is accepted for now.
- Automatic client provisioning (Q-0043's main ask) — still open with the group.
- Takeover confirmation UX ("MacBook wants control — allow?") — client-side, panino's call.
- TLS/wss or any change to the localhost `remote_gateway` — untouched.

## Cross-repo coordination (dispensa)

No wire changes at all: `controlState`, `error`, `takeControl`, and `releaseControl` all already
exist in rig-api.v1 — this work *defines semantics* for the previously no-op verbs and starts
emitting `controlState`/read-stream events over the relay leg. File an informational dispensa
note to panino/cqdx when implementation lands: no relay changes, no breaking changes, panino may
optionally render controller state and a take-control button.
