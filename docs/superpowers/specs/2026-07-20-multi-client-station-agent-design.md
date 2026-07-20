# Multi-Client Station Agent — Design Spec

**Date:** 2026-07-20
**Status:** Proposed (security-critical; scopes a follow-on to the P3.4b/c station-agent work in
`2026-07-01-station-agent-p3-design.md`). Not started — filed alongside a small, already-shipped
"dynamic single-client selection" fix (same session) that solves the immediate operator pain
without needing this design.
**Author:** Claude Sonnet 5 (under K5ARH supervision)
**Superseded (in part):** the "Proposed shape" section below is superseded by the approved
`2026-07-20-concurrent-multi-client-station-agent-design.md`; the relay-side confirmation and
what's-single-peer inventory in this doc remain the reference.
**Related:** dispensa Q-0043 (client pairing friction), Q-0035 (client keyId visibility)

## Why this is separate from the dynamic-selection fix

Two different problems got conflated when this was first raised:

1. **"I never know which web browser I'll launch."** Solved same-session, no design needed:
   `station_agent::mod.rs`'s `run_one_session` used to pre-pick ONE fixed keyId (the
   lexicographically-smallest entry of `tx_allow_list`) at component startup and pin
   `AgentSession` to it for the component's entire lifetime — any *other* allow-listed client's
   `env.src` was silently dropped by the Noise-session's peer guard, so only that one browser
   could ever connect, even across restarts. The fix: `AgentSession` already supports learning
   the peer from the DO-authenticated `env.src` of its first frame (empty `client_key_id` at
   construction) — this was already built and unit-tested, just never wired up. Now
   `run_one_session` constructs with an empty peer, lets it learn from whichever client connects,
   and vets the learned peer against `tx_allow_list` before granting any scope (still only ONE
   peer per session, but not hardcoded to a fixed one — a fresh peer is learned, and re-vetted,
   on every reconnect).
2. **"Allow multiple clients per session"** — i.e., N clients connected AT THE SAME TIME (e.g. a
   laptop dashboard tab and a phone browser both live). This is real architecture work, scoped
   below, not yet built.

## Goal

Support up to `MAX_CLIENTS` (8 — cqdx's own relay-side cap, confirmed live in the
`relay.v1` contract) concurrently-connected, independently-Noise-sessioned clients against ONE
running pancetta station-agent component, sharing the one physical radio's TX-arm state.

## The relay side already supports this — confirmed, not assumed

`dispensa/contracts/relay/relay.v1.schema.json` (clarified 2026-07-05, Q-0021 #1): the Durable
Object admits exactly one **agent** leg but up to `MAX_CLIENTS` (8) concurrent, independently
Noise-sessioned **client** legs per agent — "genuine multi-operator fan-in, not one-at-a-time
hand-off." `env` frames route by `dst` keyId among the connected client set; a `CAPACITY` terminal
error code exists for a 9th-client attempt. **This is not new relay/cqdx work** — pancetta's own
transport layer is the only thing that's single-client today.

Bonus finding while confirming this: pancetta's `RelayFrame`/terminal-code list in
`pancetta-agent/src/relay.rs` is missing `CAPACITY` from the contract's 12 terminal codes (has 11).
Small, unrelated drift bug — worth its own tiny fix, separate from this spec.

## What's single-peer today, precisely

- `AgentSession` (`pancetta-agent/src/session.rs`) owns exactly ONE `Option<ResponderHandshake>` /
  `Option<NoiseTransport>` and one `client_key_id: String` — architecturally one peer, one
  handshake, for the life of the struct.
- `run_one_session` (`pancetta/src/coordinator/station_agent/mod.rs`) drives exactly one
  `AgentSession` over the one `WsConn` for the life of one relay connection.
- `ArmContext.expected_client_key_id` is a single `String` (post-dynamic-selection-fix: learned +
  vetted once per session, still singular).

## Proposed shape (not built — this is the scoping, not the implementation)

1. **Demultiplex at the transport layer, not the session layer.** The single `WsConn` to the relay
   carries interleaved `env` frames from potentially several `src` client keyIds simultaneously (the
   relay already does this fan-in). Replace `run_one_session`'s single `AgentSession` with a
   dispatcher that:
   - Owns the raw `WsConn` read loop itself (today `AgentSession::process_next` does the
     `ws.recv_text()` call internally — this needs inverting so the dispatcher owns receive and
     hands parsed frames to the right per-peer state).
   - Keeps a `HashMap<String, PeerSession>` keyed by learned/vetted client keyId, each holding its
     own `ResponderHandshake`/`NoiseTransport`/`hello_scopes` (the per-session state
     `ArmContext` currently holds as scalars needs to become per-peer).
   - On a `src` not yet in the map: if `tx_allow_list` permits it (and the map has room —
     `MAX_CLIENTS`), start a new `PeerSession`; if not, drop/refuse (mirroring today's
     allow-list gate, now per-connecting-peer instead of once-per-run_one_session).
   - Routes each frame's decrypt/dispatch through the SAME `dispatch_action`/`ArmContext` shared
     state (`arm`, `verifier`, `client_keys`, `tx_allow_list`, `revoked_jtis`, `seen_jtis`, `audit`)
     — only `expected_client_key_id`/`hello_scopes` need to move from `ArmContext` scalars to
     per-`PeerSession` fields.
   - Sends: each `PeerSession`'s outbound `env` must be tagged with the correct `dst` (today
     `AgentSession` hardcodes `dst: self.client_key_id.clone()` — becomes "dst: this peer's
     keyId" per-session, which the existing code already does correctly per-peer; just needs to
     stay so under multiplexing).

2. **`ArmState` stays exactly as-is — single-owner, unchanged.** One physical radio, one arm. Any
   one of the N connected, allow-listed clients can send a `txArm`/`Disarm`/`Heartbeat`; whichever
   grant last succeeded is "the" arm, same collision/overwrite semantics that already exist today
   (a second `Arm` from a different peer while one is already armed is not a new scenario this
   introduces — it already existed in principle, just never reachable with only one peer able to
   connect). **No new arm model needed.** This is what keeps the security-critical spine (P0–P3.4b,
   the 5-check TX-authority chain) completely untouched by this change.

3. **Test infrastructure:** `MockWs`/`TestInitiator` (station_agent test module) and `session.rs`'s
   own mock helpers are single-peer-scripted (`vec![hello, ready, env_msg1, ...]` as one linear
   script). A multi-peer test needs a mock that can interleave two clients' frames and assert both
   land in their own `PeerSession` without cross-talk — new test scaffolding, not a small addition.

## Explicitly out of scope for this spec

- Any change to the txArmGrant/capabilityToken verification logic itself (L3 of the P3 design) —
  unaffected; still runs per-frame, per-peer, unchanged.
- Any relay/cqdx-side work — the relay already supports this; confirmed via the recorded contract,
  not re-asked here.
- Deciding whether a SECOND client should be able to *pre-empt* an already-armed first client's TX
  authority, or must wait for a disarm/heartbeat-timeout first. Today's `ArmState::arm()` semantics
  (whatever they resolve to on a second `Arm` while already armed) apply unchanged; if that turns
  out to be operationally surprising once multiple clients are actually live, that's a follow-up,
  not a blocker to building the demux layer.

## Estimate

Multi-day design+implementation once picked up: the demux/dispatcher rewrite of `run_one_session`
+ `AgentSession`'s internal receive-loop inversion, new per-peer state plumbing through
`ArmContext`, and new multi-peer test scaffolding. Touches the transport layer directly under the
TX-arm gate — warrants its own implementation session with a dedicated review pass (not bundled
into an unrelated change), per this repo's care-around-TX-safety norms.
