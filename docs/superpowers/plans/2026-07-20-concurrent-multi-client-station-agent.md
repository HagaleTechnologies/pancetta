# Concurrent Multi-Client Station Agent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Up to 8 concurrently connected, independently-Noise-sessioned clients on the one relay
websocket, with exactly one holding control (arm/QSY/TX) at a time and everyone else a live
read-only viewer.

**Architecture:** New `MultiPeerSession` demux in `pancetta-agent` (owns the socket, per-peer
Noise state keyed by DO-authenticated `env.src`); coordinator `station_agent` gains a
`controller: Option<keyId>` role (free grab via the already-reserved `takeControl` verb, implicit
grab for backward compat); the localhost gateway's translation pump is hoisted into a shared
display feed the station agent subscribes to and fans out per-peer-encrypted.

**Tech Stack:** Rust, tokio, snow (Noise_IK_25519_ChaChaPoly_BLAKE2s), serde_json,
crossbeam-channel, tokio broadcast channels.

**Spec:** `docs/superpowers/specs/2026-07-20-concurrent-multi-client-station-agent-design.md` —
read it first; it is authoritative on semantics. The earlier
`2026-07-20-multi-client-station-agent-design.md` holds the what's-single-peer inventory.

## Global Constraints

- The 5-check TX-authority chain (relay admission → Noise E2E → capabilityToken →
  txArmGrant clientSig → local `ArmState` + consent + heartbeat) is NOT modified by any task.
- The armed-TX gate fails CLOSED; poisoned lock ⇒ no remote TX; no remote frame is ever
  `TxOrigin::Local`.
- `tx_allow_list` (station-local) remains the authoritative admission gate — never relay
  admission alone. A peer not in the list must never allocate handshake state.
- All existing tests stay green. `pancetta-tui` behavior stays byte-identical. Existing
  `→Tui`/`→Qso` bus sends are never modified (additive-only invariant).
- **CRITICAL (multi-peer security):** a `txArmGrant`'s `clientKeyId` and a `hello` token's
  `clientKeyId` must be checked against **the peer that sent the frame** (the demuxed `env.src`
  identity), never against a session-global scalar — cross-peer replay of another peer's frames
  must fail.
- Gate before every push: `cargo fmt` (then `--check` clean), `cargo clippy --workspace
  --features transmit -- -D warnings`, `cargo test --workspace --features transmit`.
- Commit after every task (worktree branch `docs/concurrent-multi-client-design` already has the
  spec; implementation should branch fresh from `origin/main` as `feat/multi-client-agent` —
  NEVER from the stale local `main` ref).

---

### Task 1: `CAPACITY` terminal code

**Files:**
- Modify: `pancetta-agent/src/relay.rs:158-181` (TERMINAL_CODES + test)

**Interfaces:**
- Produces: `is_terminal("CAPACITY") == true`. No other task depends on it directly; it is the
  relay's 9th-client rejection code (contract `x-error-codes.terminal`, 12 codes; we carry 11).

- [ ] **Step 1: Write the failing test** — extend the existing `is_terminal_classification` test
  in `pancetta-agent/src/relay.rs`:

```rust
        assert!(is_terminal("CAPACITY"));
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pancetta-agent is_terminal_classification`
Expected: FAIL (`assertion failed: is_terminal("CAPACITY")`)

- [ ] **Step 3: Add `"CAPACITY"` to `TERMINAL_CODES`** (keep the list in contract order; it goes
  after `"AGENT_OCCUPIED"`).

- [ ] **Step 4: Run to verify pass**: same command, expected PASS.

- [ ] **Step 5: Commit**

```bash
git add pancetta-agent/src/relay.rs
git commit -m "fix(agent): add missing CAPACITY terminal code (relay.v1 drift)"
```

---

### Task 2: timeout-bounded receive on the `WsConn` seam

**Files:**
- Modify: `pancetta-agent/src/relay.rs` (trait `WsConn`, new `RecvOutcome` enum)
- Modify: `pancetta/src/coordinator/station_agent/net.rs` (`RealWsConn` override)
- Test: inline `#[cfg(test)]` in `pancetta-agent/src/relay.rs`

**Interfaces:**
- Produces:
```rust
pub enum RecvOutcome { Frame(String), Quiet, Closed }
// on trait WsConn (default method — existing mock impls compile unchanged):
fn recv_text_within(&mut self, timeout: std::time::Duration) -> Result<RecvOutcome, RelayError>;
```
- Task 3's `MultiPeerSession::process_next` and Task 6's read-stream interleaving consume this.
  `Quiet` = nothing arrived within the timeout (session healthy, go do other work); `Closed` =
  socket drained/closed.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn recv_text_within_default_maps_recv_text() {
        struct M(Vec<Option<String>>); // reversed script
        impl WsConn for M {
            fn send_text(&mut self, _s: String) -> Result<(), RelayError> { Ok(()) }
            fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
                Ok(self.0.pop().flatten())
            }
        }
        let mut m = M(vec![None, Some("x".into())]);
        let d = std::time::Duration::from_millis(1);
        assert!(matches!(m.recv_text_within(d).unwrap(), RecvOutcome::Frame(f) if f == "x"));
        assert!(matches!(m.recv_text_within(d).unwrap(), RecvOutcome::Closed));
    }
```

- [ ] **Step 2: Run to verify fail** — `cargo test -p pancetta-agent recv_text_within` →
  compile error (`RecvOutcome` not found).

- [ ] **Step 3: Implement** in `relay.rs` next to the trait:

```rust
/// Outcome of a timeout-bounded receive.
#[derive(Debug)]
pub enum RecvOutcome {
    /// A text frame arrived.
    Frame(String),
    /// Nothing arrived within the timeout; the connection is still open.
    Quiet,
    /// The connection is closed/drained.
    Closed,
}
```
and on the trait, a **default method** (mocks inherit blocking semantics — they never produce
`Quiet`, which keeps every existing scripted test's behavior identical):

```rust
    /// Receive the next text frame, waiting at most `timeout`. The default
    /// implementation delegates to [`WsConn::recv_text`] (blocking, never
    /// `Quiet`) so scripted test mocks need no changes; real socket
    /// implementations override it with a genuine bounded wait.
    fn recv_text_within(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<RecvOutcome, RelayError> {
        let _ = timeout;
        Ok(match self.recv_text()? {
            Some(t) => RecvOutcome::Frame(t),
            None => RecvOutcome::Closed,
        })
    }
```

Then override in `RealWsConn` (`net.rs`) — it already bridges via a handle +
`block_on`; mirror `recv_text`'s body but wrap the channel recv:

```rust
    fn recv_text_within(&mut self, timeout: Duration) -> Result<RecvOutcome, RelayError> {
        let rx = &mut self.inbound;
        let out = self.rt.block_on(async move {
            match tokio::time::timeout(timeout, rx.recv()).await {
                Err(_elapsed) => Some(RecvOutcome::Quiet),
                Ok(Some(Some(t))) => Some(RecvOutcome::Frame(t)),
                Ok(Some(None)) | Ok(None) => Some(RecvOutcome::Closed),
            }
        });
        Ok(out.unwrap_or(RecvOutcome::Closed))
    }
```
(Adapt field/handle names to `RealWsConn`'s actual members — same ones its `recv_text` uses.
`Ok(None)` = channel closed; `Ok(Some(None))` = the pump's explicit close sentinel.)

- [ ] **Step 4: Run** `cargo test -p pancetta-agent recv_text_within` → PASS, and
  `cargo test -p pancetta-agent` + `cargo test -p pancetta --lib station_agent` → all green
  (default method proves no mock broke).

- [ ] **Step 5: Commit** — `feat(agent): timeout-bounded WsConn receive (RecvOutcome)`

---

### Task 3: `MultiPeerSession` demux in `pancetta-agent`

**Files:**
- Create: `pancetta-agent/src/multi_session.rs`
- Modify: `pancetta-agent/src/lib.rs` (add `pub mod multi_session;`)
- Test: inline `#[cfg(test)]` in the new module

**Interfaces:**
- Consumes: `WsConn`/`RecvOutcome` (Task 2), `ResponderHandshake`/`NoiseTransport` (`noise.rs`),
  `RelayFrame`/`parse_frame`/`encode_env_payload`/`decode_env_payload`/`is_terminal`
  (`relay.rs`), `AgentIdentity` (`keys.rs` — `agreement_private_bytes()`, `key_id()`,
  `sign_domain(...)`).
- Produces (Task 4/5/6 rely on these exact names):

```rust
pub const MAX_PEERS: usize = 8; // relay contract MAX_CLIENTS

pub enum Poll {
    /// A decrypted control-frame plaintext from an established peer.
    Plaintext { peer: String, plaintext: Vec<u8> },
    /// A peer completed its Noise handshake; `session_id` is its channel binding.
    PeerEstablished { peer: String, session_id: String },
    /// A relay-admitted peer was refused (not allow-listed / capacity). No state allocated.
    PeerRefused { peer: String },
    /// A peer left (presence down) or its transport failed; its state is gone.
    PeerDown { peer: String },
    /// A benign frame advanced the relay leg (ready/presence-up/transient error).
    Idle,
    /// Nothing arrived within the timeout.
    Quiet,
    /// The socket is closed/drained — the session is over.
    Closed,
}

pub struct MultiPeerSession<'a, W: WsConn> { /* private */ }
impl<'a, W: WsConn> MultiPeerSession<'a, W> {
    pub fn new(ws: W, identity: &'a AgentIdentity, allowed: HashSet<String>) -> Self;
    pub fn authenticate(&mut self) -> Result<(), SessionError>;      // same leg as AgentSession
    pub fn process_next(&mut self, timeout: Duration) -> Result<Poll, SessionError>;
    pub fn send_to(&mut self, peer: &str, plaintext: &[u8]) -> Result<(), SessionError>;
    pub fn broadcast(&mut self, plaintext: &[u8]) -> usize;          // best-effort, count sent
    pub fn session_id(&self, peer: &str) -> Option<&str>;
    pub fn established_peers(&self) -> impl Iterator<Item = &str>;
}
```

**Implementation notes (read before coding):**
- `authenticate()` is a copy of `AgentSession::authenticate` (recv `hello` → send `auth` with
  the `"cqdx-relay-agent-auth-v1"` domain-tag signature). Copy it; do NOT modify `session.rs` —
  `AgentSession` stays untouched as the single-peer reference.
- Internal per-peer state is minimal because Noise IK completes in one round trip from the
  responder's view: on the first `env` from an admitted new peer, run
  `ResponderHandshake::new(&identity.agreement_private_bytes())` → `read_msg1(&payload)` →
  `write_msg2(&[])` → send `Env { dst: peer, payload, src: None }` → capture
  `b64url(handshake_hash())` → `into_transport()`. Store only:
  `struct PeerState { transport: NoiseTransport, session_id: String }` in
  `HashMap<String, PeerState>`.
- Frame dispatch in `process_next` (after `recv_text_within` → `Quiet`/`Closed` mapping and
  `parse_frame`):
  - `Ready { .. }` → mark admitted, `Idle`.
  - `Presence { peer, state }`: `"down"` and peer known → remove, `PeerDown{peer}`; else `Idle`.
  - `Error { code, .. }` → terminal per `is_terminal` ⇒ `Err(SessionError::Terminal)`, else `Idle`.
  - `Bye { .. }` → `Err(SessionError::UnexpectedClose("peer sent bye"))`.
  - `Hello`/`Auth` post-auth → `Err(SessionError::UnexpectedFrame ...)` (as `AgentSession`).
  - `Env { payload, src, .. }`:
    - `src == None` → drop (`Idle`) — the DO stamps `src` on every forwarded env; an
      unattributable env is never trusted (same posture as `AgentSession`).
    - `src` known in map → `decode_env_payload` → `transport.decrypt`; on decrypt error remove
      the peer and return `PeerDown{peer}` (per-peer failure isolation — NOT a session error);
      on success `Plaintext{peer, plaintext}`.
    - `src` unknown: not in `allowed` OR `peers.len() >= MAX_PEERS` → `PeerRefused{peer}` (no
      state, no reply); otherwise run the handshake bootstrap above → `PeerEstablished`.
      A handshake error (bad msg1) → `PeerRefused{peer}` with no state retained.
- `broadcast` encrypts the SAME plaintext once per established peer (each `NoiseTransport` has
  its own keys/nonces — never reuse ciphertext across peers) and skips (does not tear down) any
  peer whose encrypt/send fails, removing that peer's state; returns how many sends succeeded.
- No `tracing` in this crate's session modules today — keep the library silent; callers log.

**Tests to write (all in the new module; build test frames exactly like
`session.rs`'s existing tests — reuse its patterns: `MockWs` with an
`Arc<Mutex<VecDeque<String>>>` inbound you can push to mid-test + captured outbound, and the
`TestInitiator` Noise-IK client helper, with `identity.agreement_public_raw()` as the initiator's
`remote_pub`; distinct client static keys per peer, e.g. seeds `[0xAA; 32]` / `[0xBB; 32]`):**

- [ ] **Step 1: Write the failing tests** (names + assertions):

```rust
#[test] fn two_peers_handshake_and_route_independently()
// hello→auth→ready, then peerA msg1, peerB msg1, then transport envs from each in
// alternating order. Assert: two PeerEstablished with DIFFERENT session_ids; each
// outbound msg2 env has dst == the right peer; each Plaintext carries the right
// peer + the exact bytes that initiator encrypted (no cross-talk).

#[test] fn unlisted_peer_is_refused_without_state()
// src "mallory" not in allowed → Poll::PeerRefused, outbound has NO new env,
// established_peers() does not contain it; a follow-up env from mallory refuses again.

#[test] fn ninth_peer_refused_at_capacity()
// allowed contains 9 ids; establish 8 (MAX_PEERS), then the 9th's msg1 → PeerRefused.

#[test] fn presence_down_removes_only_that_peer()
// establish A and B; presence{peer:A, state:"down"} → PeerDown(A);
// B still decrypts fine; a new env from A (re-handshake msg1) re-establishes A.

#[test] fn broadcast_reaches_each_established_peer_individually()
// establish A and B; broadcast(b"evt") == 2; exactly one outbound env per peer with
// dst==peer; each initiator decrypts its copy to b"evt"; the OTHER initiator's
// transport CANNOT decrypt it (assert decrypt error — per-peer keys).

#[test] fn garbage_transport_frame_drops_only_that_peer()
// establish A and B; env{src:A, payload: valid-b64 of junk} → PeerDown(A); B unaffected.
```

- [ ] **Step 2: Run to verify fail** — `cargo test -p pancetta-agent multi_session` → compile
  errors (module doesn't exist). Write the module skeleton + tests; iterate until failures are
  assertion-level, then...
- [ ] **Step 3: Implement `MultiPeerSession`** per the notes above.
- [ ] **Step 4: Run** `cargo test -p pancetta-agent` → all green (including the untouched
  `session.rs` suite).
- [ ] **Step 5: Commit** — `feat(agent): MultiPeerSession — per-peer Noise demux over one relay socket`

---

### Task 4: coordinator switch to `MultiPeerSession` (N=1 parity, per-peer identity)

**Files:**
- Modify: `pancetta/src/coordinator/station_agent/mod.rs` (`ArmContext`, `run_one_session`,
  `dispatch_action`, `dispatch_hello`, `verify_and_arm` call-path, `session_phase` deletion,
  existing tests)

**Interfaces:**
- Consumes: `MultiPeerSession`/`Poll` (Task 3).
- Produces (Task 5 relies on): `ArmContext` shaped as below; `dispatch_action` taking
  `peer: &str` and the sender-bound verification.

```rust
struct PeerCtx {
    /// Scopes granted by this PEER's most recent verified hello.capabilityToken.
    hello_scopes: Option<Vec<String>>,
}
struct ArmContext {
    arm: Arc<Mutex<ArmState>>,
    verifier: CapabilityVerifier,
    client_keys: std::collections::HashMap<String, VerifyingKey>,
    tx_allow_list: HashSet<String>,
    revoked_jtis: HashSet<String>,
    seen_jtis: HashSet<String>,       // stays GLOBAL across peers (jti replay is global)
    audit: AuditLog,
    /// Per-established-peer session context, keyed by vetted client keyId.
    peers: std::collections::HashMap<String, PeerCtx>,
    /// Task 5 adds: controller: Option<String>,
}
async fn dispatch_action(
    action: ControlAction,
    peer: &str,               // NEW — the demuxed, allow-listed sender
    ctx: &mut ArmContext,
    bus: &MessageBus,
    session_id: &str,         // NOW the SENDING PEER's session_id
    now: i64,
) -> Dispatch
```

- [ ] **Step 1: Rework `run_one_session`** to:

```rust
async fn run_one_session<W: pancetta_agent::relay::WsConn>(
    ws: W,
    identity: &AgentIdentity,
    ctx: &mut ArmContext,
    bus: &MessageBus,
) {
    ctx.peers.clear();
    let mut sess = MultiPeerSession::new(ws, identity, ctx.tx_allow_list.clone());
    if let Err(e) = sess.authenticate() {
        debug!(target: "agent", "relay authenticate failed: {e}");
        return;
    }
    loop {
        match sess.process_next(RECV_TICK) {
            Ok(Poll::Plaintext { peer, plaintext }) => {
                let action = match map_client_frame(&plaintext) {
                    Ok(a) => a,
                    Err(e) => {
                        debug!(target: "agent.control", "malformed control frame: {e}");
                        continue;
                    }
                };
                let sid = sess.session_id(&peer).unwrap_or_default().to_string();
                if dispatch_action(action, &peer, ctx, bus, &sid, now_ms()).await
                    == Dispatch::Teardown
                {
                    return;
                }
            }
            Ok(Poll::PeerEstablished { peer, .. }) => {
                ctx.peers.insert(peer, PeerCtx { hello_scopes: None });
            }
            Ok(Poll::PeerRefused { peer }) => {
                warn!(target: "agent", peer = %peer,
                    "relay-admitted peer refused (not allow-listed or at capacity)");
            }
            Ok(Poll::PeerDown { peer }) => {
                ctx.peers.remove(&peer);
                // Task 5 adds controller-loss disarm here.
            }
            Ok(Poll::Idle) | Ok(Poll::Quiet) => {
                // Task 6 drains the read-stream feed here on Quiet.
            }
            Ok(Poll::Closed) => return,
            Err(e) => {
                debug!(target: "agent", "session error: {e}");
                return;
            }
        }
    }
}
```
with `const RECV_TICK: Duration = Duration::from_millis(200);` at module top, and delete the
now-unused `session_phase` helper and the `expected_client_key_id`/`hello_scopes` scalars
(update `run_session_loop`'s `ArmContext` literal to `peers: std::collections::HashMap::new()`).

- [ ] **Step 2: Bind verification to the sender.** Inside `dispatch_action`/helpers, replace
  every read of `ctx.expected_client_key_id` with the `peer` parameter, and every read/write of
  `ctx.hello_scopes` with `ctx.peers.get(peer)/get_mut(peer)` (a frame from a peer with no
  `PeerCtx` entry — cannot happen via the demux, but code defensively — is refused). Specifically:
  - `dispatch_hello(capability_token, ctx, now)` → `dispatch_hello(capability_token, peer, ctx, now)`;
    its `token.clientKeyId == expected_client_key_id` check becomes `== peer`; verified scopes
    land in `ctx.peers.get_mut(peer).hello_scopes`.
  - `verify_and_arm(...)`'s grant check `grant.client_key_id == ctx.expected_client_key_id`
    becomes `== peer` (this is the cross-peer-replay blocker — Global Constraints bullet).
  - `Qsy`/`SetSplit` scope checks read the PEER's `hello_scopes`.
- [ ] **Step 3: Update the existing test harness** in this file: `dispatch_action` call sites
  gain `CLIENT_KEY_ID` as `peer`; tests that pre-set `ctx.expected_client_key_id`/`hello_scopes`
  instead insert `ctx.peers.insert(CLIENT_KEY_ID.into(), PeerCtx { hello_scopes: ... })`. The
  two PR-#180 integration tests (`dynamic_selection_accepts_whichever_allowlisted_peer_connects`,
  `relay_admitted_peer_not_in_allow_list_is_refused`) keep their scripted frames — the refusal
  assertion changes from "session returns" to "no `PeerCtx` created / no scope served" (with
  `MultiPeerSession` a refused peer no longer tears the session down; assert the session stays
  up and the peer got nothing).
- [ ] **Step 4: Run** `cargo test -p pancetta --lib station_agent` → all green;
  `cargo test -p pancetta-agent` → green.
- [ ] **Step 5: Commit** — `feat(agent): station-agent drives MultiPeerSession (per-peer identity + scopes)`

---

### Task 5: controller role — free grab, exclusivity, ControlState

**Files:**
- Modify: `pancetta/src/coordinator/station_agent/mod.rs`

**Interfaces:**
- Consumes: Task 4's shapes; `pancetta_protocol::{ServerEvent, ServerFrame}`;
  `ArmState::{disarm, is_armed, tx_permitted}`.
- Produces: `DispatchOutcome { flow: Dispatch, sends: Vec<PeerSend> }` — dispatch stays pure
  (no ws access); `run_one_session` performs the sends.

```rust
enum SendTarget { One(String), AllPeers }
struct PeerSend { to: SendTarget, frame: pancetta_protocol::ServerFrame }
struct DispatchOutcome { flow: Dispatch, sends: Vec<PeerSend> }
// dispatch_action's return type becomes DispatchOutcome (same params as Task 4).
```

**Semantics to implement (spec §Component 2 — the numbered rules are authoritative):**
1. `ControlAction::TakeControl` from any peer in `ctx.peers`: if a DIFFERENT controller was set
   and the arm is live → `arm.disarm(now)` + audit first. Set `ctx.controller = Some(peer)`.
2. `ControlAction::ReleaseControl` from the controller: disarm if armed, `ctx.controller = None`.
   From anyone else: `debug!` no-op.
3. Implicit grab: in the `Arm`/`Qsy`/`SetSplit`/`TxRequest`/`StopCq` arms, first run:
   `if ctx.controller.is_none() { ctx.controller = Some(peer.to_string()); /* + ControlState sends */ }`
4. Exclusivity: if `ctx.controller.as_deref() != Some(peer)` for those same actions → refuse:
   `warn!` + audit (`AuditKind::TxDenied`, detail `"refused: not controller"`) + send
   `ServerEvent::Error { component: "stationAgent".into(), message: "another client holds control — takeControl first".into() }`
   to that peer only; do NOT execute the action.
5. Safety asymmetries: `Disarm` and `Heartbeat` are dispatched for ANY peer in `ctx.peers`,
   exactly as today — no controller check (`ArmState`'s jti/seq binding already scopes
   heartbeats to the armer).
6. Controller loss: in `run_one_session`'s `PeerDown` arm — if `ctx.controller.as_deref() ==
   Some(&peer)`, disarm + `ctx.controller = None` + broadcast ControlState. A non-controller
   `PeerDown` disarms nothing. `run_session_loop`'s existing `disarm_on_loss` after session end
   stays; add `ctx.controller = None` there too.
7. ControlState emission — add a helper and call it on: every controller transition (rules 1–3,
   6), every successful arm, every disarm, and each `PeerEstablished` (greeting, targeted at
   just that peer):

```rust
/// Per-receiver control/arm state frames (rig-api.v1 `controlState`).
fn control_state_sends(ctx: &ArmContext, now: i64, only: Option<&str>) -> Vec<PeerSend> {
    let armed = ctx.arm.lock().map(|s| s.tx_permitted(now)).unwrap_or(false);
    ctx.peers
        .keys()
        .filter(|p| only.is_none_or(|o| o == p.as_str()))
        .map(|p| PeerSend {
            to: SendTarget::One(p.clone()),
            frame: pancetta_protocol::ServerFrame::Event {
                event: pancetta_protocol::ServerEvent::ControlState {
                    control_held_by_me: ctx.controller.as_deref() == Some(p.as_str()),
                    transmit_armed: armed,
                },
            },
        })
        .collect()
}
```
8. In `run_one_session`, after each dispatch: serialize each `PeerSend.frame` with
   `serde_json::to_vec` and deliver via `sess.send_to(...)` / iterate `established_peers()`
   (collect the keys first to satisfy the borrow checker). A failed send is `debug!`-logged.

- [ ] **Step 1: Write the failing tests** (extend this module's harness; use the existing
  real-handshake helper to run two peers by scripting both initiators, or drive
  `dispatch_action` directly for pure-logic cases):

```rust
#[tokio::test] async fn implicit_grab_on_first_arm_single_client_compat()
// no takeControl ever sent; Arm from CLIENT_KEY_ID arms exactly as the existing
// arm_from_allowlisted_client_permits_tx does, and ctx.controller == Some(CLIENT_KEY_ID).

#[tokio::test] async fn non_controller_qsy_refused_with_error_frame()
// controller=A (via TakeControl); Qsy from B (both in ctx.peers, B has qsy scope) →
// no RigControl on the bus, outcome.sends contains an Error frame targeted One(B).

#[tokio::test] async fn take_control_disarms_previous_controllers_arm()
// A armed (reuse the arm fixture); TakeControl from B → arm.is_armed()==false,
// controller==Some(B), sends include ControlState to both peers
// (B: control_held_by_me=true, A: false; both transmit_armed=false).

#[tokio::test] async fn disarm_accepted_from_non_controller()
// A armed+controller; Disarm from B → disarmed (fail-safe wins).

#[tokio::test] async fn controller_peer_down_disarms_listener_down_does_not()
// via run_one_session with scripted frames: A controller+armed, B listener;
// presence down B → still armed; presence down A → disarmed, controller None.

#[tokio::test] async fn release_control_clears_and_disarms()
```

- [ ] **Step 2: Run to verify fail** — `cargo test -p pancetta --lib station_agent` → new tests
  fail to compile (`DispatchOutcome` missing) or assert-fail.
- [ ] **Step 3: Implement** rules 1–8 (mechanical refactor of `dispatch_action`'s return value:
  wrap every existing `Dispatch::Continue`/`Teardown` in `DispatchOutcome { flow, sends }`).
- [ ] **Step 4: Run** the full module suite → green.
- [ ] **Step 5: Commit** — `feat(agent): one-controller-at-a-time with free grab + controlState emission`

---

### Task 6: shared display feed + read stream over the relay

**Files:**
- Modify: `pancetta/src/coordinator/remote_gateway/mod.rs` (hoist pump construction)
- Modify: `pancetta/src/coordinator/mod.rs` (field rename `gateway_enabled` →
  `display_feed_enabled`; hold the feed handle)
- Modify: `pancetta/src/coordinator/station_agent/mod.rs` (subscribe + drain + broadcast)
- Modify (mechanical rename only): `pancetta/src/coordinator/autonomous.rs:989`,
  `pancetta/src/coordinator/hamlib.rs:433,527`, `pancetta/src/coordinator/tui_relay.rs:1551`,
  `pancetta/src/coordinator/qso.rs:1726,1979,2144` — the 7 `relay_to_gateway(...)` call sites
  keep one call each; only the captured flag's name changes.

**Interfaces:**
- Consumes: `handle_bus_msg` (`remote_gateway/mod.rs:143` — moves intact), Task 5's send path.
- Produces:

```rust
/// Shared bus→ServerEvent pump, started when EITHER the localhost gateway OR the
/// station agent read stream needs it. Owns the translation + rolling snapshot.
pub(crate) struct DisplayFeed {
    pub evt_tx: tokio::sync::broadcast::Sender<pancetta_protocol::ServerEvent>,
    pub snapshot: std::sync::Arc<tokio::sync::RwLock<pancetta_protocol::StateSnapshot>>,
}
// RunConfig gains: events: Option<tokio::sync::broadcast::Receiver<ServerEvent>>
```

- [ ] **Step 1: Hoist the pump.** In `remote_gateway/mod.rs`, add
  `pub(crate) async fn start_display_feed(&mut self) -> Result<()>` on
  `ApplicationCoordinator` that contains what `start_remote_gateway_component` currently does up
  to (and including) spawning the `handle_bus_msg` pump task over the
  `ComponentId::RemoteGateway` bus channel — creating `evt_tx`, `snapshot`, `op_freq`, the
  station-lookup handle — storing `DisplayFeed` on the coordinator
  (`display_feed: Option<DisplayFeed>` field). Gate: start it when
  `config.network.remote_gateway.enabled || station_agent_will_activate` (compute the latter
  with the same enabled+paired+allow-list checks `start_station_agent_component` uses — factor
  that predicate into a small `fn station_agent_active(cfg) -> bool` so the two never drift).
  When the feed is off, keep today's drain task. `start_remote_gateway_component` keeps ONLY the
  axum/localhost server part, consuming `self.display_feed.as_ref()` handles; it stays gated on
  its own `enabled` flag exactly as today. Call `start_display_feed` before both component
  starts in the coordinator startup sequence.
- [ ] **Step 2: Rename the emit-site flag.** `gateway_enabled` → `display_feed_enabled` on the
  coordinator + `relay_to_gateway`'s doc/param name + the 7 call sites' captured clones. Set it
  true when the feed starts (either trigger). No signature shape changes.
- [ ] **Step 3: Subscribe the station agent.** In `start_station_agent_component`, after the
  allow-list check: `let events = self.display_feed.as_ref().map(|f| f.evt_tx.subscribe());`
  → into `RunConfig { events, ... }` → `ArmContext` untouched; `run_session_loop` hands
  `events` (as `&mut Option<Receiver<_>>`) to `run_one_session`.
- [ ] **Step 4: Drain + broadcast.** In `run_one_session`'s `Ok(Poll::Quiet) | Ok(Poll::Idle)`
  arm (and after each successful dispatch, so a chatty control session can't starve the feed):

```rust
    drain_read_stream(events, &mut sess);
```

```rust
/// Fan pending display events out to every established peer. Lossy by design:
/// Lagged means the bounded broadcast ring dropped old events — log and continue.
fn drain_read_stream<W: pancetta_agent::relay::WsConn>(
    events: &mut Option<tokio::sync::broadcast::Receiver<pancetta_protocol::ServerEvent>>,
    sess: &mut MultiPeerSession<'_, W>,
) {
    let Some(rx) = events.as_mut() else { return };
    loop {
        match rx.try_recv() {
            Ok(ev) => {
                let frame = pancetta_protocol::ServerFrame::Event { event: ev };
                match serde_json::to_vec(&frame) {
                    Ok(bytes) => {
                        let _ = sess.broadcast(&bytes);
                    }
                    Err(e) => debug!(target: "agent", "read-stream serialize failed: {e}"),
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                debug!(target: "agent", skipped = n, "read stream lagged — events dropped");
            }
            Err(_) => break, // Empty or Closed
        }
    }
}
```
- [ ] **Step 5: Tests.**
  - `remote_gateway` existing tests stay green (the hoist must be behavior-preserving for the
    localhost path).
  - New station_agent test `read_stream_events_reach_every_established_peer`: build a
    `broadcast::channel`, establish two peers via the scripted harness, send
    `ServerEvent::TxStatus { active: true }` into the sender, run the loop a tick; assert each
    initiator decrypts a `{"frame":"event","event":{"event":"txStatus","active":true}}` env
    (match on parsed JSON, not string equality).
  - New test `read_stream_absent_feed_is_inert`: `events: None` → loop runs, no outbound envs
    beyond handshake traffic.
- [ ] **Step 6: Run** `cargo test -p pancetta --lib` (station_agent + remote_gateway modules) →
  green.
- [ ] **Step 7: Commit** — `feat(agent,gateway): shared display feed; encrypted read stream over the relay`

---

### Task 7: docs, dispensa note, full gate

**Files:**
- Modify: `pancetta/src/coordinator/station_agent/mod.rs:1-40` (module doc: multi-peer,
  controller rules, read stream now real)
- Modify: `docs/DECISIONS/remote-operation.md` (dated digest entry: what shipped, the two safety
  asymmetries, spec/plan pointers)
- Modify: `CHANGELOG.md` (Unreleased: multi-client concurrent sessions, one-controller rule,
  relay read stream, CAPACITY fix)
- Modify: `docs/superpowers/specs/2026-07-20-concurrent-multi-client-station-agent-design.md`
  (Status → Implemented, PR link)
- Create: `dispensa/questions/00NN-pancetta-concurrent-multi-client-shipped.md` (informational,
  From pancetta To all: semantics now live for `takeControl`/`releaseControl`; agent now emits
  `controlState` + read-stream events over the relay; up to 8 clients; no wire/relay changes;
  panino MAY add controller UI. Next free NN at filing time; separate dispensa PR — branch,
  `git pull --rebase` first.)

- [ ] **Step 1: Write all doc updates** (content per the spec — no new decisions here).
- [ ] **Step 2: Full gate**: `cargo fmt` then `cargo fmt --check` (must be empty),
  `cargo clippy --workspace --features transmit -- -D warnings`,
  `cargo test --workspace --features transmit` → all green.
- [ ] **Step 3: Commit** — `docs: multi-client station-agent shipped (spec→Implemented, DECISIONS, CHANGELOG)`
- [ ] **Step 4: Push branch + open PR** (controller session pushes, not subagents; PR body
  summarizes the controller rules and links the spec). File the dispensa PR from
  `/Users/thagale/Code/dispensa`, `cd`-ing there explicitly in the same command (a `--repo` flag
  alone picks up the WRONG local branch context — this bit us before).

---

## Self-Review (performed at plan-writing time)

- **Spec coverage:** Component 1 → Task 3; Component 2 → Tasks 4–5; Component 3 → Tasks 2+6;
  Component 4 → Task 1; security invariants → Global Constraints + Task 4 Step 2 + Task 5 tests;
  error handling → Task 3 (per-peer isolation) + Task 4 (session teardown unchanged); testing
  section → mirrored per-task; cross-repo note → Task 7. No gaps found.
- **Known judgment calls left to the implementer** (each is bounded and non-semantic): exact
  field names inside `RealWsConn`'s override (mirror its `recv_text`); whether the shared
  handshake bootstrap is factored out of `AgentSession` or copied (spec allows either; copying
  is fine — `session.rs` must not change behavior); borrow-checker plumbing of
  `events`/`sess` through `run_one_session`.
- **Type consistency check:** `Poll`, `PeerCtx`, `DispatchOutcome`, `PeerSend`, `SendTarget`,
  `DisplayFeed`, `RecvOutcome`, `control_state_sends`, `drain_read_stream`, `RECV_TICK`,
  `MAX_PEERS` — names used identically across Tasks 2–6.
