# TX Allow-List Auto-Populate — Design Spec

**Date:** 2026-07-22
**Status:** Proposed. Not started.
**Author:** Claude Sonnet 5 (under K5ARH supervision)
**Related:** dispensa Q-0043 (client pairing needs to stop requiring manual `tx_allow_list` config
edits) — cqdx's 2026-07-21 answer to this spec's motivating question; Q-0042 (panino dashboard
stuck "Reconnecting" — root cause was an empty `tx_allow_list` at boot, this spec's cold-start fix
closes that bug class, though Q-0042's own instance still needs its separate manual fix); Q-0035
(client keyId visibility, Resolved — the value this spec's fetch replaces the operator's
copy-paste of).

## Problem

`network.station_agent.tx_allow_list` is a static list, read once from `pancetta.toml` at
`start_station_agent_component` startup. Today, admitting a new client requires: the operator
finds the client's keyId in panino's PairingScreen, hand-edits the toml, and restarts the whole
pancetta process — with zero validation the pasted string is even a real, currently-authorized
client. Worse: if the list is empty at boot (e.g. before the operator has done this edit even
once), `start_station_agent_component` logs a warning and returns without ever attempting the
relay connection — for the rest of the process's lifetime. This is Q-0042's exact root cause.

cqdx's answer (2026-07-21, Q-0043): the data needed already exists and requires no new cqdx work —
`GET /api/v1/authorizations` (PAT or session auth) returns the operator's live
`authorization_edges` rows, `{id, agentKeyId, clientKeyId, scopes, createdAt}`, already filtered to
non-revoked. Recommendation: pancetta filters client-side to rows matching this agent's own
`agentKeyId` and builds `tx_allow_list` from the resulting `clientKeyId`s; for "no restart needed,"
poll this endpoint periodically (30-60s) rather than build a new relay push channel (the relay is
deliberately kept an opaque pipe).

## Contract gap (flagged, not blocking)

`GET /api/v1/authorizations` is **not yet documented** in
`dispensa/contracts/cqdx-api/cqdx-api.v1.schema.json` — checked directly, no entry exists. cqdx's
answer describes the shape in prose only, pointing at its own `apps/web/src/lib/server/registry.ts`
`listAuthorizationEdges` implementation, not a pinned contract. This is the same category of gap
CLAUDE.md already tracks for `GET /api/v1/spots?live=true`'s envelope key. Per this repo's own
CLAUDE.md ("clone dispensa... propose changes there first" for cross-cutting work), this spec's
plan includes proposing a contract entry in dispensa documenting the endpoint as it actually ships,
alongside — not blocking — the pancetta-side implementation, which is built defensively (tolerant
JSON deserialization, treats an unexpected shape as a poll failure rather than a panic) given the
shape isn't yet contractually pinned.

## Goal

1. **Cold start:** `station_agent` no longer permanently bails when `tx_allow_list` starts empty.
   It always attempts the relay connection; an empty list just means zero peers are admitted until
   the first successful poll populates it.
2. **Live updates:** once cqdx integration is enabled, `tx_allow_list` becomes a live, shared,
   periodically-refreshed set — a revoked or newly-authorized client takes effect within one poll
   interval, not just on the next reconnect. This requires `tx_allow_list` to change from an owned
   value cloned once into `MultiPeerSession` at construction, to a shared reference
   (`Arc<RwLock<HashSet<String>>>`) the admission check reads live.
3. **Source of truth:** when cqdx integration is enabled (`network.cqdx.token` configured), cqdx's
   authorization data is authoritative — the static config `tx_allow_list` is used only as the
   fallback/seed when cqdx integration is disabled or the token is missing (today's existing
   behavior, unchanged in that case).

## Design

### 1. New `pancetta-cqdx` client method

`CqdxClient::fetch_authorizations(&self) -> Result<Vec<AuthorizationEdge>>`, following the exact
shape of `fetch_live_spots`/`fetch_needed_grids` (`pancetta-cqdx/src/client.rs`): `GET
{base_url}/api/v1/authorizations`, `.bearer_auth(self.token.expose_secret())`, `check_status` +
`checked_json`. New type in `pancetta-cqdx/src/types.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationEdge {
    pub id: String,
    pub agent_key_id: String,
    pub client_key_id: String,
    pub scopes: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationsResponse {
    pub authorization_edges: Vec<AuthorizationEdge>,
}
```

Given the contract gap above: unlike `fetch_needed_grids` (where a 404 is treated as "endpoint not
live yet, empty is harmless"), `fetch_authorizations` treats a `404` as a genuine **error**, not an
empty result — a 404 here plausibly means the wrong path/URL, and silently returning `Ok(vec![])`
would let the poll loop treat "empty because the request failed" as a legitimate "this agent
really has zero authorized clients" state and revoke everyone connected. Only a `200` with a
genuinely empty `authorization_edges: []` body counts as "zero clients," a real and normal state
(e.g. right after first pairing, before any client is added). A 404 propagates as an error like any
other non-2xx status, letting the poll loop's fail-safe (§3 below) keep the last known good set.
Any JSON deserialization failure is likewise caught and logged as a poll failure rather than
propagated as a panic.

### 2. Shared, live `tx_allow_list`

`pancetta/src/coordinator/station_agent/mod.rs`: `tx_allow_list`'s type changes from an owned
`HashSet<String>` to `Arc<RwLock<HashSet<String>>>`, threaded through `RunConfig`/`ArmContext` the
same structural way as today (same field, shared instead of cloned). `MultiPeerSession`'s admission
check (`ctx.tx_allow_list.contains(client_key_id)`) reads through the shared reference — same call
shape, live storage.

### 3. Poll task + cold-start fix

Remove the `if tx_allow_list.is_empty() { warn!(...); return; }` early bail in
`start_station_agent_component`. The component always proceeds to spawn `run_session_loop` (today's
existing connection-attempt path), and — only when `network.cqdx.token` is configured — spawns a
second task alongside it:

- On an interval (`network.cqdx.authorizations_poll_interval_secs`, default 45, new config field),
  calls `fetch_authorizations`, filters to rows where `agent_key_id == identity.key_id()` (this
  station's own identity, already available at startup), collects `client_key_id`s into a
  `HashSet`, and replaces the shared set's contents (`*shared.write().unwrap() = new_set`).
- **Fail-safe:** on any poll failure (network error, non-2xx after the 404 special-case, JSON
  error) — log `WARN`, do NOT touch the shared set, retry next interval. A transient cqdx outage
  (relevant right now — cqdx.io's production deploy is currently down) must never spuriously
  revoke an already-admitted, connected client.
- When `network.cqdx.token` is NOT configured: no poll task is spawned; `tx_allow_list` is
  seeded once from the config's static list and never changes afterward — today's exact behavior,
  unchanged.

### 4. Testing

- `pancetta-cqdx`: unit tests for `fetch_authorizations` (success, 404-as-empty, malformed-JSON
  handling) using the crate's existing mock-server test pattern.
- `pancetta` station_agent: unit tests for the agent-key-id filter (pure function, given a list of
  edges and this agent's own key, returns the right client-key-id set), the fail-safe
  keep-last-known-good behavior (mocked poll failure), and the cold-start-no-longer-bails change
  (empty initial list still reaches `run_session_loop`).
- An integration-style test confirming a live update to the shared set is visible to a connected
  `MultiPeerSession`'s admission check without a reconnect (extends the existing multi-client test
  scaffolding from the concurrent-multi-client-station-agent work, PR #188).

### 5. Documentation

- Propose a `GET /api/v1/authorizations` entry in `dispensa/contracts/cqdx-api/cqdx-api.v1.schema.json`
  documenting the endpoint as cqdx's own answer describes it, flagging the envelope key
  (`authorization_edges`, assumed from prose) as unconfirmed against the live API, same pattern as
  the existing `spots?live=true` gap note.
- Update dispensa Q-0043 with this spec's link once implemented, and set Status appropriately
  (still `Answered`, not `Resolved` — Resolved is the asker's call once satisfied, per the
  question's own template convention).
- `docs/DECISIONS/remote-operation.md`: append a dated entry once shipped.

## Non-goals

- No change to the relay/cqdx wire protocol — this adds only a new authenticated GET call, no new
  push channel (matches cqdx's own "keep the relay opaque" reasoning).
- No change to how panino or other clients discover their own keyId (Q-0035, already resolved).
- No change to `tx_allow_list`'s config schema — it remains the fallback field.
- Does not fix Q-0042's own specific instance (that station's dashboard staying disconnected until
  this ships and a poll succeeds) — that thread still needs its own manual fix or waits for this to
  land and the operator to restart once.
- Does not address cqdx.io's current production outage — this feature is inert (falls back to
  config-seeded behavior, poll fails safely) until that clears.
