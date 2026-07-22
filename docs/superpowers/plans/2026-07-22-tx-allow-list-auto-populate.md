# TX Allow-List Auto-Populate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Station-agent auto-populates and live-updates `tx_allow_list` from cqdx's
`GET /api/v1/authorizations` instead of requiring a manual keyId copy-paste + process restart
(dispensa Q-0043), and stops permanently bailing when the list starts empty (Q-0042's root cause).

**Architecture:** A new `pancetta-cqdx` client method feeds a periodic poll task in
`pancetta/src/coordinator/station_agent/mod.rs`. `ArmContext`'s `tx_allow_list`/`client_keys`
change from owned values to `Arc<RwLock<...>>` so the poll task's writes are visible to the
admission check (`verify_and_arm`) live, without waiting for a reconnect. A small new
`pancetta-config` field controls the poll interval. A separate, doc-only dispensa contract
proposal documents the previously-unpinned endpoint shape.

**Tech Stack:** Rust, reqwest/wiremock (existing `pancetta-cqdx` HTTP client pattern), tokio
(spawned poll task), `std::sync::{Arc, RwLock}`.

## Global Constraints

- **CLAUDE.md invariant — armed-TX gate fails CLOSED:** a poll failure (network error, cqdx
  down, malformed JSON) must never clear or replace the shared allow-list/key-map — it logs and
  keeps the last-known-good state, retrying next interval. A 404 is a genuine error here (NOT
  treated as "empty is fine," unlike `fetch_needed_grids` — an empty allow-list from a masked
  failure would revoke every connected client).
- **`client_keys` must refresh in lockstep with `tx_allow_list`** on every successful poll — a
  keyId appearing in the allow-list without its verifying key loaded fails signature
  verification, defeating the point of adding it.
- **Explicitly out of scope:** `pancetta-agent`'s `MultiPeerSession::new` keeps taking an owned
  `HashSet<String>` snapshot at construction (once per relay reconnect) — a brand-new client still
  waits for the next reconnect to establish as a peer. Do not touch `pancetta-agent` in this plan.
- **No new relay wire-protocol messages** — only a new authenticated GET call from pancetta to
  cqdx.
- Existing tests in `pancetta/src/coordinator/station_agent/mod.rs` that construct `ArmContext`
  directly or mutate its `tx_allow_list` field (lines noted per-task below, current as of
  `origin/main@8034f647`) must be updated to the new field type and continue passing unmodified in
  behavior — this is a plumbing/type change, not a semantic one, for every task except Task 4.

---

## Task 1: `pancetta-cqdx` — `fetch_authorizations` client method

**Files:**
- Modify: `pancetta-cqdx/src/types.rs` (new types, near the existing `SpotGroup`/`LiveSpotsResponse` section)
- Modify: `pancetta-cqdx/src/client.rs` (new method, near `fetch_live_spots`; new tests in the existing `mod tests` block)

**Interfaces:**
- Produces: `pub struct AuthorizationEdge { pub id: String, pub agent_key_id: String, pub client_key_id: String, pub scopes: Vec<String>, pub created_at: String }` (all pub fields, `Debug + Clone + Deserialize`), `pub struct AuthorizationsResponse { pub authorization_edges: Vec<AuthorizationEdge> }`, `pub async fn CqdxClient::fetch_authorizations(&self) -> Result<Vec<AuthorizationEdge>>`.
- Consumes: existing `CqdxClient::check_status`/`checked_json` private helpers, existing `CqdxError` variants (no new variant needed — a 404 propagates as `CqdxError::Server { status: 404, .. }` via `check_status`, exactly like any other non-2xx status; no special-casing required, unlike `fetch_needed_grids`).

This mirrors `fetch_live_spots` exactly, with NO 404 special-case (that's the key difference from `fetch_needed_grids` — see Global Constraints).

- [ ] **Step 1: Add the new types to `pancetta-cqdx/src/types.rs`**

Find the `LiveSpotsResponse` struct (search for `pub struct LiveSpotsResponse`) and insert after its
closing `}`:

```rust
// --- Authorizations (Q-0043 auto-populate) ---

/// One `authorization_edges` row from `GET /api/v1/authorizations` — an
/// operator-authorized (agentKeyId, clientKeyId) pairing, already filtered
/// server-side to non-revoked.
///
/// # Contract gap
///
/// This endpoint is **not yet documented** in
/// `dispensa/contracts/cqdx-api/cqdx-api.v1.schema.json` — the shape below is
/// inferred from cqdx's prose answer to dispensa Q-0043 (2026-07-21), pointing
/// at its own `apps/web/src/lib/server/registry.ts` `listAuthorizationEdges`
/// implementation, not a pinned contract. See Task 5 of
/// `docs/superpowers/plans/2026-07-22-tx-allow-list-auto-populate.md` for the
/// proposed contract entry. Field casing (camelCase) matches ADR-0003, same as
/// every other endpoint in this file.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationEdge {
    pub id: String,
    #[serde(rename = "agentKeyId")]
    pub agent_key_id: String,
    #[serde(rename = "clientKeyId")]
    pub client_key_id: String,
    pub scopes: Vec<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Envelope for `GET /api/v1/authorizations`. Envelope key
/// (`authorization_edges`) is UNVERIFIED against the live API — same category
/// of gap as `LiveSpotsResponse`'s `groups` key note above.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationsResponse {
    pub authorization_edges: Vec<AuthorizationEdge>,
}
```

- [ ] **Step 2: Write the failing tests in `pancetta-cqdx/src/client.rs`**

Find `test_fetch_live_spots` in the `mod tests` block and add these tests immediately after it:

```rust
#[tokio::test]
async fn test_fetch_authorizations() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/authorizations"))
        .and(header("Authorization", "Bearer pat_test_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorization_edges": [{
                "id": "auth_1",
                "agentKeyId": "agent_abc",
                "clientKeyId": "client_xyz",
                "scopes": ["status", "qsy"],
                "createdAt": "2026-07-20T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let client = test_client(&server.uri());
    let edges = client.fetch_authorizations().await.unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].agent_key_id, "agent_abc");
    assert_eq!(edges[0].client_key_id, "client_xyz");
    assert_eq!(edges[0].scopes, vec!["status", "qsy"]);
}

#[tokio::test]
async fn test_fetch_authorizations_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/authorizations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "authorization_edges": [] })),
        )
        .mount(&server)
        .await;

    let client = test_client(&server.uri());
    let edges = client.fetch_authorizations().await.unwrap();
    assert!(edges.is_empty());
}

/// A 404 is a genuine error here, NOT treated as "empty is fine" the way
/// `fetch_needed_grids` treats a missing endpoint — see this file's
/// Global Constraints note on why that distinction matters for an
/// allow-list-feeding endpoint.
#[tokio::test]
async fn test_fetch_authorizations_404_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/authorizations"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = test_client(&server.uri());
    let result = client.fetch_authorizations().await;
    assert!(result.is_err(), "404 must propagate as an error, not Ok(vec![])");
}

#[tokio::test]
async fn test_fetch_authorizations_401_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/authorizations"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = test_client(&server.uri());
    let result = client.fetch_authorizations().await;
    assert!(matches!(result, Err(CqdxError::Unauthorized)));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pancetta-cqdx fetch_authorizations -- --nocapture`
Expected: FAIL to compile — `fetch_authorizations` is not a method on `CqdxClient` yet.

- [ ] **Step 4: Implement `fetch_authorizations`**

In `pancetta-cqdx/src/client.rs`, find `fetch_live_spots` and add this method immediately after it:

```rust
/// Fetch this operator's live authorization edges (dispensa Q-0043 —
/// feeds pancetta's station-agent `tx_allow_list` auto-populate). Unlike
/// `fetch_needed_grids`, a 404 is NOT treated as "empty is fine" — it
/// propagates as a genuine error via `check_status`, since an
/// accidentally-empty allow-list here would revoke every connected
/// client rather than being a harmless missing-feature default.
pub async fn fetch_authorizations(&self) -> Result<Vec<AuthorizationEdge>> {
    let url = format!("{}/api/v1/authorizations", self.base_url);
    debug!("Fetching authorizations from {}", url);
    let resp = self
        .http
        .get(&url)
        .bearer_auth(self.token.expose_secret())
        .send()
        .await?;
    let resp = self.check_status(resp).await?;
    let body: AuthorizationsResponse = self.checked_json(resp).await?;
    Ok(body.authorization_edges)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pancetta-cqdx fetch_authorizations -- --nocapture`
Expected: 4/4 PASS (`test_fetch_authorizations`, `test_fetch_authorizations_empty`,
`test_fetch_authorizations_404_is_an_error`, `test_fetch_authorizations_401_is_unauthorized`).

- [ ] **Step 6: Run the full `pancetta-cqdx` test suite to check for regressions**

Run: `cargo test -p pancetta-cqdx`
Expected: all existing tests still PASS.

- [ ] **Step 7: Commit**

```bash
git add pancetta-cqdx/src/types.rs pancetta-cqdx/src/client.rs
git commit -m "feat(cqdx): add fetch_authorizations client method (Q-0043)

New CqdxClient::fetch_authorizations() -> Result<Vec<AuthorizationEdge>>,
mirroring fetch_live_spots. Unlike fetch_needed_grids, a 404 propagates
as a genuine error rather than being treated as an empty-but-valid
result — an accidentally-empty tx_allow_list from a masked failure
would revoke every connected client, a different risk profile than an
empty grid-priority set."
```

---

## Task 2: `pancetta-config` — poll-interval field

**Files:**
- Modify: `pancetta-config/src/network.rs` (`CqdxConfig` struct, its `Default` impl, and `NetworkConfig::validate_section`'s existing cqdx validation block)

**Interfaces:**
- Produces: `CqdxConfig::authorizations_poll_interval_secs: u64` (new pub field, default `45`).
- No `merge_with` changes needed — `NetworkConfig::merge_with` already does `self.cqdx = other.cqdx;` (whole-struct replace), confirmed at `pancetta-config/src/network.rs` (search `self.cqdx = other.cqdx`).

- [ ] **Step 1: Add the field to `CqdxConfig`**

Find `pub struct CqdxConfig` (search for `pub struct CqdxConfig`) and add after the existing
`poll_interval_secs: u64` field:

```rust
    /// TX-allow-list auto-populate poll interval, in seconds (dispensa
    /// Q-0043). Separate from `poll_interval_secs` (the priority-spot poll) —
    /// different concern, potentially different cadence.
    pub authorizations_poll_interval_secs: u64,
```

- [ ] **Step 2: Update `Default for CqdxConfig`**

Find `impl Default for CqdxConfig` and add the new field to the struct literal:

```rust
impl Default for CqdxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://cqdx.io".to_string(),
            token: None,
            poll_interval_secs: 30,
            authorizations_poll_interval_secs: 45,
        }
    }
}
```

- [ ] **Step 3: Write the failing validation test**

Find the existing test `test_cqdx_validation_enabled_with_token` (or similar, in
`pancetta-config/src/network.rs`'s test module) and add nearby:

```rust
#[test]
fn test_cqdx_validation_authorizations_poll_interval_too_low() {
    let mut config = NetworkConfig::default();
    config.cqdx.enabled = true;
    config.cqdx.token = Some("pat_abc123".to_string());
    config.cqdx.authorizations_poll_interval_secs = 5; // too low
    assert!(config.validate_section().is_err());
}

#[test]
fn test_cqdx_validation_authorizations_poll_interval_ok() {
    let mut config = NetworkConfig::default();
    config.cqdx.enabled = true;
    config.cqdx.token = Some("pat_abc123".to_string());
    config.cqdx.authorizations_poll_interval_secs = 45;
    assert!(config.validate_section().is_ok());
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p pancetta-config test_cqdx_validation_authorizations_poll_interval -- --nocapture`
Expected: `too_low` test FAILS (no validation exists yet — `is_err()` assertion fails since nothing
currently rejects a low value).

- [ ] **Step 5: Add the validation check**

Find the existing cqdx validation block in `NetworkConfig::validate_section` (search for
`"cqdx.io poll interval must be at least 10 seconds"`) and add immediately after that `if` block:

```rust
            if self.cqdx.authorizations_poll_interval_secs < 10 {
                return Err(ConfigError::Validation(
                    "cqdx.io authorizations poll interval must be at least 10 seconds".to_string(),
                ));
            }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p pancetta-config test_cqdx_validation_authorizations_poll_interval -- --nocapture`
Expected: both tests PASS.

- [ ] **Step 7: Run the full `pancetta-config` test suite**

Run: `cargo test -p pancetta-config`
Expected: all existing tests PASS — in particular, any test asserting the full field list of
`CqdxConfig`'s `Default` (search test output for failures mentioning `CqdxConfig` if any occur) —
if such a test exists and fails only because it doesn't know about the new field, update it to
include `authorizations_poll_interval_secs: 45` in its expected value; do not change its assertion
style otherwise.

- [ ] **Step 8: Commit**

```bash
git add pancetta-config/src/network.rs
git commit -m "feat(config): add cqdx.authorizations_poll_interval_secs (Q-0043)

New CqdxConfig field controlling how often station-agent polls
GET /api/v1/authorizations for tx_allow_list auto-populate. Default
45s. Same >=10s floor as the existing priority-spot poll_interval_secs."
```

---

## Task 3: `Arc<RwLock<...>>` conversion — mechanical plumbing, no behavior change

**Files:**
- Modify: `pancetta/src/coordinator/station_agent/mod.rs` (many sites — see below; all current line
  numbers as of `origin/main@8034f647`, locate by the shown surrounding code since exact lines will
  drift as edits land)

**Interfaces:**
- Produces: `ArmContext.tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>`,
  `ArmContext.client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>`
  (both were plain owned values before). `RunConfig`'s matching fields become the same shared
  types.
- Consumes: nothing new from other tasks — this task is self-contained plumbing.

**This task changes ZERO behavior** — it only changes storage from owned values to shared
references read/written through a lock, everywhere those two fields are touched. Task 4 (not this
one) is where the poll task and the cold-start-bail removal actually add new behavior. Splitting
these lets a reviewer verify "no behavior change" for this task in isolation.

- [ ] **Step 1: Change the two struct field declarations**

In `ArmContext` (search for `struct ArmContext`):

```rust
    /// Verifying keys for allow-listed clients, keyed by client keyId. Shared
    /// and periodically refreshed by the Q-0043 auto-populate poll task
    /// (Task 4) in lockstep with `tx_allow_list` — a keyId appearing in the
    /// allow-list without its verifying key loaded here fails signature
    /// verification, so the two must never update independently.
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    /// Station-local TX-allow-list. Shared and periodically refreshed by the
    /// Q-0043 auto-populate poll task (Task 4) when cqdx integration is
    /// enabled — a fail-closed poll failure never clears this, only a
    /// successful poll replaces its contents. When cqdx integration is
    /// disabled, this is seeded once from config and never changes (today's
    /// original behavior, preserved).
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
```

In `RunConfig` (search for `struct RunConfig`), change the same two fields to the same types:

```rust
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
```

- [ ] **Step 2: Update the admission check in `verify_and_arm`**

Find (search for `if !ctx.tx_allow_list.contains(client_key_id)`):

```rust
    if !ctx.tx_allow_list.contains(client_key_id) {
        return Err(format!(
            "client {client_key_id} not in station-local TX-allow-list"
        ));
    }
    let client_vk = *ctx
        .client_keys
        .get(client_key_id)
        .ok_or_else(|| format!("no device key for client {client_key_id}"))?;
```

Replace with (acquire both read guards once, up front, since both fields are read multiple times
in this function — `verify_arm_grant` a few lines below also needs `&HashSet<String>`):

```rust
    let allow_list = ctx.tx_allow_list.read().unwrap();
    if !allow_list.contains(client_key_id) {
        return Err(format!(
            "client {client_key_id} not in station-local TX-allow-list"
        ));
    }
    let client_keys = ctx.client_keys.read().unwrap();
    let client_vk = *client_keys
        .get(client_key_id)
        .ok_or_else(|| format!("no device key for client {client_key_id}"))?;
```

Then find the `verify_arm_grant` call a few lines below (search for `&ctx.tx_allow_list,` inside
the `ctx.verifier.verify_arm_grant(...)` call) and change that argument to `&allow_list,` (the
guard bound above, already a `&HashSet<String>` via `Deref`) — no other arguments in that call
change.

`allow_list`/`client_keys` (the two read guards) go out of scope naturally at the end of
`verify_and_arm` (a synchronous, non-async function — confirm this by checking its signature has
no `async` keyword — holding a `std::sync::RwLockReadGuard` across this function's body is safe
since it never awaits).

- [ ] **Step 3: Update `run_one_session`'s `MultiPeerSession::new` call**

Find (search for `MultiPeerSession::new(ws, identity, ctx.tx_allow_list.clone())`):

```rust
    let mut sess = MultiPeerSession::new(ws, identity, ctx.tx_allow_list.clone());
```

Replace with (per this plan's deferred-scope decision: `MultiPeerSession` still takes an owned
snapshot, so clone the HashSet's *contents* through the read lock, not the Arc itself):

```rust
    let allow_snapshot = ctx.tx_allow_list.read().unwrap().clone();
    let mut sess = MultiPeerSession::new(ws, identity, allow_snapshot);
```

- [ ] **Step 4: Update `start_station_agent_component`'s construction site**

Find (search for `let tx_allow_list: HashSet<String> = cfg.tx_allow_list.iter().cloned().collect();`):

```rust
        let tx_allow_list: HashSet<String> = cfg.tx_allow_list.iter().cloned().collect();
        let client_keys = load_client_device_keys(&key_dir, &tx_allow_list);
```

Replace with:

```rust
        let tx_allow_list: HashSet<String> = cfg.tx_allow_list.iter().cloned().collect();
        let client_keys = load_client_device_keys(&key_dir, &tx_allow_list);
        let tx_allow_list = Arc::new(std::sync::RwLock::new(tx_allow_list));
        let client_keys = Arc::new(std::sync::RwLock::new(client_keys));
```

The existing `if tx_allow_list.is_empty() { ... }` check a few lines below (search for `if tx_allow_list.is_empty()`)
now needs `.read().unwrap()`:

```rust
        if tx_allow_list.read().unwrap().is_empty() {
```

(This whole early-bail block is REMOVED in Task 4, not this one — for this task, just make it
compile against the new type, preserving today's exact behavior.)

The `RunConfig { ..., client_keys, tx_allow_list, ... }` construction a few lines further down
needs no change — the shorthand field syntax already refers to the (now-Arc-wrapped) local
variables of the same names.

- [ ] **Step 5: Update `run_session_loop`'s `ArmContext` construction**

Find (search for `let mut ctx = ArmContext {`, inside `run_session_loop`) — the
`client_keys: cfg.client_keys, tx_allow_list: cfg.tx_allow_list,` lines need no change (same
reasoning: the types now match on both sides).

- [ ] **Step 6: Update the two existing test call sites that mutate `tx_allow_list` directly**

Find both occurrences of `ctx.tx_allow_list.insert(PEER_B.to_string());` (search for that exact
string — two occurrences in this file's test module) and change each to:

```rust
        ctx.tx_allow_list.write().unwrap().insert(PEER_B.to_string());
```

- [ ] **Step 7: Update every test-helper `ArmContext` construction site**

Confirmed exactly 4 sites matching `tx_allow_list: allow` in this file's test module (grep for that
exact string to relocate them if line numbers have shifted):

1. **`fn ctx_with(allow_client: bool, have_device_key: bool) -> ArmContext`** (~line 1510): has
   local `let mut client_keys = std::collections::HashMap::new();` built above, referenced via the
   `client_keys,` shorthand. Change `client_keys,` → `client_keys: Arc::new(std::sync::RwLock::new(client_keys)),`
   and `tx_allow_list: allow,` → `tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),`.
2. **`fn run_session_with_connecting_peer(...)`** (~line 3023): same pattern as #1 — local
   `client_keys` built above via `let mut client_keys = std::collections::HashMap::new();`, same two
   replacements.
3. **`fn fresh_ctx(agent_kid: &str, allow: HashSet<String>) -> ArmContext`** (~line 3142): this one
   is DIFFERENT — `client_keys` is constructed **inline** in the struct literal as
   `client_keys: std::collections::HashMap::new(),`, not via a shorthand referencing a separate
   local variable. Change that line to
   `client_keys: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),` and
   `tx_allow_list: allow,` → `tx_allow_list: Arc::new(std::sync::RwLock::new(allow)),`.
4. **`fn ctx_with_agent(agent_kid: &str, allow_client: bool, have_device_key: bool) -> ArmContext`**
   (~line 3374): same pattern as #1/#2 — local `client_keys` built above, same two replacements.

If a grep for `tx_allow_list: allow` turns up a different count than 4 at this point (e.g. because
an earlier task's edits shifted something unexpectedly), stop and report NEEDS_CONTEXT rather than
guessing — do not leave any test helper constructing the old owned-value shape, or the crate will
fail to compile.

- [ ] **Step 8: Build**

Run: `cargo build -p pancetta`
Expected: builds cleanly. The compiler will point at any remaining site this plan's steps missed —
if so, apply the same `Arc::new(std::sync::RwLock::new(...))` wrapping pattern (for construction
sites) or `.read()`/`.write()` (for access sites) consistently with the sites already fixed above,
rather than inventing a different pattern.

- [ ] **Step 9: Run the full station_agent test module**

Run: `cargo test -p pancetta --lib coordinator::station_agent::`
Expected: all existing tests PASS, unmodified in behavior (this task changes storage type only).

- [ ] **Step 10: Commit**

```bash
git add pancetta/src/coordinator/station_agent/mod.rs
git commit -m "refactor(agent): tx_allow_list/client_keys become Arc<RwLock<...>>

Pure plumbing change, zero behavior change (verified: full
station_agent test module passes unmodified). Lets Task 4's poll task
write live updates the admission check (verify_and_arm) reads without
waiting for a reconnect. MultiPeerSession::new still takes an owned
snapshot (out of scope per the design's deferred-scope decision) —
cloned through the read lock at construction time, same as before."
```

---

## Task 4: Cold-start fix + poll task

**Files:**
- Modify: `pancetta/src/coordinator/station_agent/mod.rs` (`start_station_agent_component`, new
  poll-task function)
- Modify: `pancetta-config/src/network.rs` (the stale validation-warning text)

**Interfaces:**
- Consumes: `CqdxClient::fetch_authorizations` (Task 1), `CqdxConfig::authorizations_poll_interval_secs`
  (Task 2), the `Arc<RwLock<...>>` fields from Task 3.
- Produces: a new spawned task, `poll_authorizations_loop`, no new public interface consumed by
  anything outside this task.

- [ ] **Step 1: Remove the cold-start early bail**

Find (search for `if tx_allow_list.read().unwrap().is_empty() {`, the block Task 3 made compile
against the new type):

```rust
        if tx_allow_list.read().unwrap().is_empty() {
            warn!(
                target: "agent",
                "station agent paired but tx_allow_list is empty — no client to admit; idle, relay connection never attempted"
            );
            return self.spawn_station_agent_drain().await;
        }
```

Delete this block entirely. The component now always proceeds to spawn `run_session_loop`
regardless of whether `tx_allow_list` currently has any entries.

- [ ] **Step 2: Grab the cqdx config alongside the station_agent config**

Find (search for `let cfg = config.network.station_agent.clone();`) and add immediately after:

```rust
        let cqdx_cfg = config.network.cqdx.clone();
```

(`config` is the same `RwLockReadGuard` already in scope from `let config = self.config.read().await;`
a line above — this just clones one more sibling section before the guard drops at end of scope,
same pattern already used for `cfg`.)

- [ ] **Step 3: Spawn the poll task alongside the session-loop task, only when cqdx is enabled**

Find the existing `let handle = tokio::spawn(async move { run_session_loop(RunConfig { ... }).await; ... });`
block and, immediately BEFORE it (so `tx_allow_list`/`client_keys`/`key_dir` are still owned/clonable
before `RunConfig` moves them), insert:

```rust
        if cqdx_cfg.enabled && cqdx_cfg.token.as_ref().is_some_and(|t| !t.is_empty()) {
            let poll_tx_allow_list = tx_allow_list.clone();
            let poll_client_keys = client_keys.clone();
            let poll_key_dir = key_dir.clone();
            let poll_agent_key_id = paired.agent_key_id.clone();
            let poll_shutdown = self.shutdown_signal.clone();
            let poll_interval = Duration::from_secs(cqdx_cfg.authorizations_poll_interval_secs);
            tokio::spawn(async move {
                poll_authorizations_loop(
                    cqdx_cfg,
                    poll_agent_key_id,
                    poll_key_dir,
                    poll_tx_allow_list,
                    poll_client_keys,
                    poll_shutdown,
                    poll_interval,
                )
                .await;
            });
        }
```

- [ ] **Step 4: Write `poll_authorizations_loop`**

Add this new function near `load_client_device_keys` (search for `fn load_client_device_keys` and
insert after its closing `}`):

```rust
/// Periodically refresh the shared `tx_allow_list`/`client_keys` from cqdx's
/// live authorization data (dispensa Q-0043 auto-populate). Runs for the
/// lifetime of the station-agent component; only spawned when cqdx
/// integration is enabled with a token configured (see
/// `start_station_agent_component`).
///
/// Fail-safe: any poll failure (network error, non-2xx status including a
/// 404, malformed JSON) is logged at WARN and the shared state is left
/// UNTOUCHED — a transient cqdx outage (relevant right now: cqdx.io's
/// production deploy is currently down) must never spuriously revoke an
/// already-admitted, connected client. Only a successful poll (a 200 with a
/// parseable body, even if `authorization_edges` is genuinely empty) replaces
/// the shared contents.
async fn poll_authorizations_loop(
    cqdx_cfg: pancetta_config::network::CqdxConfig,
    agent_key_id: String,
    key_dir: std::path::PathBuf,
    tx_allow_list: Arc<std::sync::RwLock<HashSet<String>>>,
    client_keys: Arc<std::sync::RwLock<std::collections::HashMap<String, VerifyingKey>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    interval: Duration,
) {
    let Some(token) = cqdx_cfg.token.clone() else {
        return; // Defensive — caller already checked this before spawning.
    };
    let client = match pancetta_cqdx::CqdxClient::new(cqdx_cfg.base_url.clone(), token) {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "agent", "authorizations poll: failed to construct cqdx client: {e}; poll task exiting");
            return;
        }
    };

    while !shutdown.load(Ordering::Acquire) {
        match client.fetch_authorizations().await {
            Ok(edges) => {
                let new_allow: HashSet<String> = edges
                    .iter()
                    .filter(|e| e.agent_key_id == agent_key_id)
                    .map(|e| e.client_key_id.clone())
                    .collect();
                let new_keys = load_client_device_keys(&key_dir, &new_allow);
                let allow_len = new_allow.len();
                *tx_allow_list.write().unwrap() = new_allow;
                *client_keys.write().unwrap() = new_keys;
                debug!(
                    target: "agent",
                    "authorizations poll: refreshed tx_allow_list ({} client(s) authorized for this agent)",
                    allow_len
                );
            }
            Err(e) => {
                warn!(
                    target: "agent",
                    "authorizations poll failed: {e} — keeping last-known-good tx_allow_list, retrying in {:?}",
                    interval
                );
            }
        }
        tokio::time::sleep(interval).await;
    }
}
```

- [ ] **Step 5: Update the stale config-validation warning text**

Find (search for `"station_agent.enabled = true but tx_allow_list is empty — no client keyId to`)
in `pancetta-config/src/network.rs`'s `validate_section`:

```rust
        if self.station_agent.tx_allow_list.is_empty() {
            if self.station_agent.enabled {
                tracing::warn!(
                    target: "config.station_agent",
                    "station_agent.enabled = true but tx_allow_list is empty — no client keyId to \
                     admit, so the relay connection itself is never attempted (not just TX-gated); \
                     add the client's keyId to station_agent.tx_allow_list"
                );
            } else if self.station_agent.remote_tx_enabled {
```

Replace the first `tracing::warn!` call's message to reflect that an empty static config list is no
longer fatal when cqdx auto-populate is enabled:

```rust
        if self.station_agent.tx_allow_list.is_empty() {
            if self.station_agent.enabled && !self.cqdx.enabled {
                tracing::warn!(
                    target: "config.station_agent",
                    "station_agent.enabled = true but tx_allow_list is empty and cqdx auto-populate \
                     is disabled — no client keyId to admit; add the client's keyId to \
                     station_agent.tx_allow_list, or enable cqdx.io integration (dispensa Q-0043) \
                     so it populates automatically"
                );
            } else if self.station_agent.remote_tx_enabled {
```

(The `remote_tx_enabled` branch and everything else in this block is unchanged — only the first
condition gains `&& !self.cqdx.enabled` and the message is reworded.)

- [ ] **Step 6: Write tests for the agent-key-id filter and fail-safe behavior**

Add to `pancetta/src/coordinator/station_agent/mod.rs`'s test module (find an existing `#[test]` or
`#[tokio::test]` near the bottom of the file to insert nearby):

```rust
#[test]
fn authorizations_filter_matches_only_this_agents_edges() {
    let edges = vec![
        pancetta_cqdx::AuthorizationEdge {
            id: "1".to_string(),
            agent_key_id: "agent_this".to_string(),
            client_key_id: "client_a".to_string(),
            scopes: vec!["status".to_string()],
            created_at: "2026-07-20T00:00:00Z".to_string(),
        },
        pancetta_cqdx::AuthorizationEdge {
            id: "2".to_string(),
            agent_key_id: "agent_other".to_string(),
            client_key_id: "client_b".to_string(),
            scopes: vec!["status".to_string()],
            created_at: "2026-07-20T00:00:00Z".to_string(),
        },
    ];
    let filtered: HashSet<String> = edges
        .iter()
        .filter(|e| e.agent_key_id == "agent_this")
        .map(|e| e.client_key_id.clone())
        .collect();
    assert_eq!(filtered, HashSet::from(["client_a".to_string()]));
}

#[tokio::test]
async fn poll_authorizations_loop_keeps_last_known_good_on_failure() {
    // A cqdx client pointed at a URL with nothing listening fails every
    // request — confirms the poll loop's fail-safe leaves a pre-seeded
    // allow-list untouched rather than clearing it.
    let tx_allow_list = Arc::new(std::sync::RwLock::new(HashSet::from(["seed".to_string()])));
    let client_keys = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cqdx_cfg = pancetta_config::network::CqdxConfig {
        enabled: true,
        base_url: "http://127.0.0.1:1".to_string(), // nothing listening — every request fails
        token: Some("pat_test_token_0000000000".to_string()),
        poll_interval_secs: 30,
        authorizations_poll_interval_secs: 45,
    };
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(poll_authorizations_loop(
        cqdx_cfg,
        "agent_this".to_string(),
        std::env::temp_dir(),
        tx_allow_list.clone(),
        client_keys.clone(),
        shutdown_clone,
        Duration::from_millis(50),
    ));
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.store(true, Ordering::Release);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert_eq!(
        *tx_allow_list.read().unwrap(),
        HashSet::from(["seed".to_string()]),
        "a failing poll must never clear the pre-seeded allow-list"
    );
}
```

- [ ] **Step 7: Run the new tests**

Run: `cargo test -p pancetta --lib coordinator::station_agent:: authorizations_filter -- --nocapture`
Run: `cargo test -p pancetta --lib coordinator::station_agent:: poll_authorizations_loop -- --nocapture`
Expected: both PASS.

- [ ] **Step 8: Build and run the full station_agent + config test modules**

Run: `cargo build -p pancetta`
Run: `cargo test -p pancetta --lib coordinator::station_agent::`
Run: `cargo test -p pancetta-config`
Expected: all PASS, no regressions.

- [ ] **Step 9: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: PASS. This touches the TX-arm security surface directly — run the broadest available net
before committing, not just the targeted subsets above.

- [ ] **Step 10: Commit**

```bash
git add pancetta/src/coordinator/station_agent/mod.rs pancetta-config/src/network.rs
git commit -m "feat(agent): tx_allow_list auto-populate + cold-start fix (Q-0043)

station_agent no longer bails permanently when tx_allow_list starts
empty — it always attempts the relay connection now. When cqdx
integration is enabled with a token configured, a new poll task
periodically refreshes tx_allow_list/client_keys from
GET /api/v1/authorizations, filtered to this agent's own keyId.
Poll failures (including a 404) never clear the shared state, only a
successful poll replaces it. Updated the now-partially-stale
config-validation warning to account for cqdx auto-populate.

Docs: docs/superpowers/specs/2026-07-22-tx-allow-list-auto-populate-design.md"
```

---

## Task 5: Dispensa contract proposal (separate repo, doc-only)

**Files:**
- Modify: `~/Code/dispensa/contracts/cqdx-api/cqdx-api.v1.schema.json` (add the `GET /api/v1/authorizations` endpoint entry)
- Modify: `~/Code/dispensa/questions/0043-client-pairing-should-not-require-manual-config-editing.md` (append a pancetta implementation-status note)

**Interfaces:** None — documentation only, in a different repository from Tasks 1-4. This is a
separate PR against `dispensa`, not something to bundle into the same commit sequence as the
pancetta code above.

- [ ] **Step 1: Fetch dispensa fresh**

```bash
cd ~/Code/dispensa
git fetch origin
git status
```

If there are unrelated local changes or the branch is behind, resolve per this repo's standing
"always pull fresh" policy before editing — do not edit against a stale clone.

- [ ] **Step 2: Create a branch**

```bash
cd ~/Code/dispensa
git checkout -b docs/authorizations-endpoint-contract main
git pull --rebase origin main
```

- [ ] **Step 3: Add the endpoint entry to the contract**

In `contracts/cqdx-api/cqdx-api.v1.schema.json`, find the `x-endpoints` object (search for
`"x-endpoints"`) and add a new entry alongside the existing ones (e.g. near `"GET /api/v1/agents"`
if present, or after the last existing GET endpoint entry):

```json
    "GET /api/v1/authorizations": {
      "auth": "required (session or PAT)",
      "response": "#/$defs/authorizationsResponse",
      "note": "Added 2026-07-22 (pancetta Q-0043 auto-populate implementation). Shape inferred from cqdx's prose answer to Q-0043 (2026-07-21), pointing at apps/web/src/lib/server/registry.ts's listAuthorizationEdges — NOT yet confirmed against the live API response. Returns the caller's own non-revoked authorization_edges rows. Envelope key (authorization_edges) and field casing are UNVERIFIED — same category of open item as the historical spots?live=true 'groups' key question (now resolved). cqdx should confirm or correct this entry against its actual handler."
    },
```

Then find the `$defs` object and add the corresponding schema definitions (matching this file's
existing style — check how `spotsLiveResponse`/`SpotGroup`-equivalent defs are structured and
mirror that exactly rather than inventing a new convention):

```json
    "authorizationEdge": {
      "type": "object",
      "properties": {
        "id": { "type": "string" },
        "agentKeyId": { "type": "string" },
        "clientKeyId": { "type": "string" },
        "scopes": { "type": "array", "items": { "type": "string" } },
        "createdAt": { "type": "string" }
      },
      "required": ["id", "agentKeyId", "clientKeyId", "scopes", "createdAt"]
    },
    "authorizationsResponse": {
      "type": "object",
      "properties": {
        "authorization_edges": {
          "type": "array",
          "items": { "$ref": "#/$defs/authorizationEdge" }
        }
      },
      "required": ["authorization_edges"]
    }
```

- [ ] **Step 4: Append a status note to Q-0043**

In `questions/0043-client-pairing-should-not-require-manual-config-editing.md`, find the end of the
existing `### pancetta (2026-07-21) — concurrent multi-client shipped` section (the last dated
sub-section under `## Answer`) and append a new dated sub-section:

```markdown

### pancetta (2026-07-22) — auto-populate implemented; MultiPeerSession admission still snapshot-scoped

Implemented the auto-populate half cqdx recommended above: `station_agent` now polls
`GET /api/v1/authorizations` (default every 45s) and auto-populates/live-refreshes
`tx_allow_list` and its client-key map, filtered to this agent's own `agentKeyId`. No more
copy-paste-restart. A poll failure (including a 404) never clears the allow-list — only a
successful poll replaces it, so a transient cqdx outage can't spuriously revoke a connected
client.

Also fixes a related bug found during design: `station_agent` used to bail permanently (never
attempt the relay connection, ever) if `tx_allow_list` started empty — this was Q-0042's root
cause. It now always attempts the connection; an empty list just means zero peers admitted until
the first successful poll.

**Scoped narrower than "fully live," on purpose:** revocation of an already-arming/armed client is
live (the station-local admission check that gates arming reads the shared, poll-refreshed list on
every attempt). A brand-new client authorized *after* the current relay session started still
needs to wait for the next reconnect to establish as a peer — `pancetta-agent`'s
`MultiPeerSession::new` still takes an owned snapshot at construction, deliberately left untouched
this pass (touches a third crate's admission internals under the TX-arm gate; deferred as a named
follow-up).

Spec: `docs/superpowers/specs/2026-07-22-tx-allow-list-auto-populate-design.md` (pancetta repo).
Also proposing a `GET /api/v1/authorizations` contract entry in this repo alongside this note
(this same PR) — the endpoint wasn't previously documented in `cqdx-api.v1.schema.json`; shape is
inferred from this question's own prose answer above, not yet confirmed against the live API.
```

- [ ] **Step 5: Commit and push**

```bash
cd ~/Code/dispensa
git add contracts/cqdx-api/cqdx-api.v1.schema.json questions/0043-client-pairing-should-not-require-manual-config-editing.md
git commit -m "docs: propose GET /api/v1/authorizations contract entry (Q-0043)

Documents the endpoint pancetta's tx_allow_list auto-populate now
consumes (pancetta PR implementing this lands alongside). Shape
inferred from cqdx's own prose answer to Q-0043, not yet confirmed
against the live API — flagged as such in both the contract note and
the endpoint's schema description."
git push -u origin docs/authorizations-endpoint-contract
```

- [ ] **Step 6: Open a PR**

```bash
cd ~/Code/dispensa
gh pr create --title "docs: propose GET /api/v1/authorizations contract entry (Q-0043)" --body "$(cat <<'EOF'
## Summary
- Adds a contract entry for GET /api/v1/authorizations, previously undocumented in cqdx-api.v1.schema.json, consumed by pancetta's new tx_allow_list auto-populate (Q-0043).
- Shape is inferred from Q-0043's own prose answer (cqdx, 2026-07-21) pointing at listAuthorizationEdges — flagged as unverified against the live API in both the endpoint note and schema description, same pattern as the historical spots?live=true envelope-key question.
- Appends a pancetta implementation-status note to Q-0043 itself.

## Test plan
- [ ] cqdx confirms or corrects the envelope key / field casing against the actual live handler

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```
