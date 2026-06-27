# remote_gateway (read-only) — Implementation Plan (Remote-op Sub-plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A new, default-OFF, **localhost-bound** `remote_gateway` coordinator component that serves the read-only v1 view (DX Hunter + QSO progress + state snapshot) to WebSocket clients using the `pancetta-protocol` wire types. No control, no remote-TX, no network exposure (operator boundary).

**Architecture:** axum + tower-http + rustls WS server, started as an optional component (mirrors `start_pskreporter_component`). It receives display events via **additive dual-destination** bus sends (a new `ComponentId::RemoteGateway`), translates them to `pancetta_protocol::ServerEvent`, and fans them out to connected clients via an internal `tokio::sync::broadcast`. On connect: `Hello`→`Welcome{StateSnapshot}` handshake, then live events. **TUI invariant:** every coordinator edit is ADDITIVE (the existing `→Tui` sends are untouched).

**Tech Stack:** axum (ws), tower-http, rustls/tokio-rustls, tokio broadcast, serde_json, pancetta-protocol.

**Grounding (from event-flow map):**
- Component template: `pancetta/src/coordinator/psk_reporter.rs:24` (`start_pskreporter_component`); called in `mod.rs` ~line 988 next to autonomous/dx_cluster/pskreporter.
- `ComponentId` enum: `message_bus.rs:33` (+ Display ~:60). `create_channel(id)`: `message_bus.rs:772`. `send_message` routes by `destination`: `message_bus.rs:804/822`.
- Display-event emit sites (dual-target these, gated): decodes `ft8.rs:747` (crossbeam) + bus fan-out ~`ft8.rs:754-783`; `ActiveQsosSnapshot` `qso.rs:1333/1539/1648`; freq `hamlib.rs:378`; s-meter `hamlib.rs:461`; split `autonomous.rs:560`; tx-status `tx.rs:219`; autonomous `autonomous.rs:596`.
- Config: `pancetta-config/src/network.rs:15` `NetworkConfig` (sub-configs with `#[serde(default)] pub enabled: bool`, e.g. `PskReporterConfig` :194).
- Protocol types: `pancetta_protocol::{ServerEvent, ClientFrame, ServerFrame, Hello, Welcome, StateSnapshot, DxRow, QsoProgress, PendingCall, DecodedView}` (already on main).
- Serde-ready bus payloads to translate FROM: `ActiveQsoSnapshotItem`/`PendingCallSnapshotItem` (`message_bus.rs`).

**Execution note:** station is DOWN — heavy builds OK. axum/tower/rustls is a big first compile.

---

## File structure

| File | Responsibility | Change |
|------|----------------|--------|
| `Cargo.toml` (root) | workspace deps: axum, tower-http, tokio-rustls/rustls | add to `[workspace.dependencies]` |
| `pancetta/Cargo.toml` | pull axum/tower-http/rustls + `pancetta-protocol` | add deps |
| `pancetta-config/src/network.rs` | `RemoteGatewayConfig { enabled, bind_addr }` on `NetworkConfig` | add struct + field |
| `pancetta/src/message_bus.rs` | `ComponentId::RemoteGateway` (+ Display) | add variant |
| `pancetta/src/coordinator/remote_gateway/mod.rs` | the component: WS server, broadcast, handshake | create |
| `pancetta/src/coordinator/remote_gateway/translate.rs` | bus `MessageType` → `ServerEvent`; build `StateSnapshot` | create |
| `pancetta/src/coordinator/{ft8,qso,hamlib,tx,autonomous}.rs` | ADDITIVE dual-destination sends (gated) | modify (additive only) |
| `pancetta/src/coordinator/mod.rs` | `start_remote_gateway_component()` call | modify |
| `pancetta/src/coordinator/mod.rs` or `components.rs` | declare `mod remote_gateway;` | modify |

---

## Task 1: Config — `RemoteGatewayConfig` (default-off)

**Files:** Modify `pancetta-config/src/network.rs`

- [ ] **Step 1: Add the struct + field** — mirror `PskReporterConfig` style:
```rust
/// Read-only remote view gateway (Panino client). Default OFF; localhost-bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteGatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Defaults to localhost only (no network exposure) until the
    /// authenticated control path (Sub-plan C) exists.
    #[serde(default = "default_gateway_bind")]
    pub bind_addr: String,
}
fn default_gateway_bind() -> String { "127.0.0.1:4080".to_string() }
impl Default for RemoteGatewayConfig {
    fn default() -> Self { Self { enabled: false, bind_addr: default_gateway_bind() } }
}
```
Add to `NetworkConfig` (near the other sub-configs): `#[serde(default)] pub remote_gateway: RemoteGatewayConfig,`. Confirm `NetworkConfig` derives `Default`/serde and that omitting the TOML section yields `enabled=false`.

- [ ] **Step 2: Test** — add a unit test: a `NetworkConfig` deserialized from TOML WITHOUT a `[network.remote_gateway]` section has `remote_gateway.enabled == false` and `bind_addr == "127.0.0.1:4080"`.
- [ ] **Step 3: Verify** — `cargo test -p pancetta-config` → pass.
- [ ] **Step 4: Commit** — `feat(config): RemoteGatewayConfig (default-off, localhost bind)`.

---

## Task 2: `ComponentId::RemoteGateway`

**Files:** Modify `pancetta/src/message_bus.rs`

- [ ] **Step 1:** Add `RemoteGateway` to the `ComponentId` enum (`:33`) and a `Display` arm (`:60`) → `"remote_gateway"`. (Enum already derives serde/Hash/etc.)
- [ ] **Step 2: Verify** — `cargo build -p pancetta` → clean (exhaustive matches on ComponentId, if any, get a new arm; fix as needed — search `match.*ComponentId` and add `RemoteGateway =>` arms, typically grouped with other optional components).
- [ ] **Step 3: Commit** — `feat(bus): ComponentId::RemoteGateway`.

---

## Task 3: Workspace deps (axum/tower-http/rustls)

**Files:** root `Cargo.toml`, `pancetta/Cargo.toml`

- [ ] **Step 1:** Add to root `[workspace.dependencies]`:
```toml
axum = { version = "0.7", features = ["ws"] }
tower-http = { version = "0.6", features = ["trace", "limit"] }
tokio-rustls = "0.26"
rustls = "0.23"
```
(Pick versions compatible with the workspace's tokio 1.50 / hyper; if axum 0.8 is current and compatible, use it — verify it builds. `tower-http` `limit` = body/size limits; `trace` = request tracing.)
- [ ] **Step 2:** In `pancetta/Cargo.toml` add: `axum = { workspace = true }`, `tower-http = { workspace = true }`, `tokio-rustls = { workspace = true }`, `rustls = { workspace = true }`, `pancetta-protocol = { path = "../pancetta-protocol" }`.
- [ ] **Step 3: cargo-deny pre-check** — the push gate runs cargo-deny (licenses/advisories). Run `cargo deny check licenses advisories 2>&1 | tail` after adding deps; if axum/tower-http/rustls trip an advisory or a license not in the allow-list, note it (may need a `deny.toml` allow entry — but axum/tower/rustls are MIT/Apache, should pass).
- [ ] **Step 4: Verify** — `cargo build -p pancetta` → clean (compiles the new deps; heavy first build).
- [ ] **Step 5: Commit** — `build: add axum/tower-http/rustls for remote_gateway` (include Cargo.lock).

---

## Task 4: Translation — bus events → `pancetta_protocol::ServerEvent`

**Files:** Create `pancetta/src/coordinator/remote_gateway/translate.rs`

- [ ] **Step 1:** Write pure functions converting the v1 bus payloads to protocol types. Mirror the field-mapping `tui_relay` already does. Concretely:
  - `fn qso_item_to_progress(item: &ActiveQsoSnapshotItem) -> QsoProgress` (field-for-field; ladder_labels/ours/index, now/next lines, last_tx/rx, reports, dx_last_activity, started_at).
  - `fn pending_item_to_call(item: &PendingCallSnapshotItem) -> PendingCall`.
  - `fn decoded_to_view(msg: &pancetta_ft8::DecodedMessage, /* enrichment */) -> DecodedView` — OR, simpler for v1, translate from the TUI-side `DecodedMessageView` if the gateway taps that; **decision:** the gateway taps the BUS decode (raw `DecodedMessage`); build `DecodedView` from it (timestamp, freq, snr, dt, df, callsign/grid parsed from the message, message text, slot_parity). Enrichment fields (worked_before/needed/atno/priority) can be `false`/`None` in v1 OR filled from `cached_lookup` (the gateway has `self.cached_lookup`) — v1: fill needed/atno/worked_before from `cached_lookup`, priority `None` (the priority f64 is computed in tui_relay; for v1 leave `None` or replicate later).
  - `fn server_event_from_bus(msg: &MessageType, ...) -> Option<ServerEvent>` — match the v1 variants: `ActiveQsosSnapshot{qsos,pending}` → `ServerEvent::ActiveQsos{...}`; `RigControl(FrequencyResponse{vfo,frequency})` → `Frequency`; `RigControl(SignalStrengthResponse{db_over_s9})` → `SignalStrength`; `SplitStatus{tx_hz}` → `Split`; `TxStatus{active}` → `TxStatus`; `TxPolicyStatus{policy}` → `TxPolicy`; `DxMessage(Spot{..})` → a `DxHunter` row (or accumulate). Return `None` for events not in v1.
- [ ] **Step 2: Tests** — unit-test `qso_item_to_progress` + `decoded_to_view` mappings (construct a sample bus item, assert the protocol struct fields).
- [ ] **Step 3: Verify** — `cargo test -p pancetta translate` (or the module path) → pass.
- [ ] **Step 4: Commit** — `feat(gateway): bus→protocol translation`.

---

## Task 5: The `remote_gateway` component (axum WS, broadcast, handshake)

**Files:** Create `pancetta/src/coordinator/remote_gateway/mod.rs`; declare `mod remote_gateway;`

- [ ] **Step 1:** `start_remote_gateway_component(&mut self) -> Result<()>` mirroring `start_pskreporter_component`:
  - Read `config.network.remote_gateway`; if `!enabled` → create+drain the `ComponentId::RemoteGateway` channel (so dual-target sends don't flood the bus when off — the psk drain pattern) + return Ok. (Gateway senders are gated too, but the drain is belt-and-suspenders.)
  - If enabled: `create_channel(ComponentId::RemoteGateway)` → `gw_rx`. Create a `tokio::sync::broadcast::channel::<ServerEvent>(1024)` (`evt_tx`, `evt_rx`). Snapshot the relevant shared state for `StateSnapshot` building (operating_frequency_hz, split atomic, tx_policy, cached_lookup; QSO snapshot — see note).
  - Spawn the **bus→broadcast pump**: a task that drains `gw_rx.try_recv()`, runs `server_event_from_bus`, and `evt_tx.send(event)` (ignore lagged receivers). Also maintains a "latest snapshot" `Arc<RwLock<StateSnapshot>>` updated as events arrive (so a new client gets current state).
  - Spawn the **axum server** bound to `config.bind_addr` (default `127.0.0.1:4080`): a `/ws` route with `axum::extract::ws::WebSocketUpgrade`. Per-connection handler: read the client's `Hello` frame (validate `protocol_version`), send `Welcome{ snapshot }` (from the latest-snapshot Arc), then subscribe to `evt_tx` and forward each `ServerEvent` as a `ServerFrame::Event` JSON text message until disconnect. v1 read-only: IGNORE any inbound `ClientCommand` (log it; control is Sub-plan C). Apply a tower-http body/message size limit + a connection timeout.
  - rustls TLS: for v1 localhost, plain ws:// is acceptable (same machine); add a TODO + config hook for rustls (wss://) — OR wire rustls now with a self-signed/local cert. **v1 decision:** plain `ws://` on localhost (no cert hassle); rustls/wss is Sub-plan C (when network-exposed). Note this clearly.
  - Register both spawned handles in `self.named_task_handles`.
- [ ] **Step 2: Snapshot building** — the QSO part of `StateSnapshot` needs the active-QSO data. v1 simplest: the snapshot Arc starts empty and fills as the first `ActiveQsosSnapshot` event arrives (the coordinator emits these periodically). So a client connecting gets whatever's accumulated; acceptable for v1. (A pull-current-state-from-QsoManager handshake is a Sub-plan-C refinement.)
- [ ] **Step 3: Tests** — an integration test: start the gateway on a random localhost port (config enabled, bind `127.0.0.1:0`), connect a `tokio-tungstenite` client, send `Hello`, assert a `Welcome` frame is received and parses. Then publish a synthetic `ServerEvent` via the broadcast and assert the client receives it as a `ServerFrame::Event`. (Use the test-only constructor path; if standing up the full coordinator is too heavy, extract the axum router + broadcast into a testable `serve(bind, evt_rx, snapshot)` fn and test THAT.)
- [ ] **Step 4: Verify** — `cargo test -p pancetta remote_gateway` → pass; `cargo build -p pancetta` clean.
- [ ] **Step 5: Commit** — `feat(gateway): axum WS remote_gateway (read-only, localhost, handshake+snapshot+fanout)`.

---

## Task 6: Wire startup + additive dual-destination event feeds

**Files:** Modify `pancetta/src/coordinator/mod.rs` (+ ft8/qso/hamlib/tx/autonomous.rs — ADDITIVE only)

- [ ] **Step 1:** In `mod.rs` run(), after `start_pskreporter_component().await?` (~:988), add `self.start_remote_gateway_component().await?;`. Declare `mod remote_gateway;`.
- [ ] **Step 2: Additive dual-destination** — a helper on the coordinator: `fn gateway_enabled(&self) -> bool` (cached at startup into an `Arc<AtomicBool>` so the hot path is a cheap load). At each v1 emit site, AFTER the existing `→Tui` send, add (only when `gateway_enabled`):
  ```rust
  if gw_enabled.load(Ordering::Relaxed) {
      let _ = bus.send_message(ComponentMessage::new(<source>, ComponentId::RemoteGateway, <same MessageType clone>, Instant::now())).await;
  }
  ```
  Apply at: `qso.rs` ActiveQsosSnapshot (3 sites), `hamlib.rs` Frequency + SignalStrength, `tx.rs` TxStatus, `autonomous.rs` Split + AutonomousStatus, and the `ft8.rs` decode fan-out (add a RemoteGateway clone alongside the Qso/PskReporter clones). DO NOT modify the existing `→Tui` sends.
  - For `TxPolicyStatus`/`Split` TUI-local echoes (`tui_relay.rs:716/955/998/1124/1185`): v1 may omit policy from the gateway, OR additively also `bus.send_message(→Tui or →RemoteGateway, TxPolicyStatus)` — **v1 decision: omit TxPolicy/extra-split from the gateway** (keep tui_relay untouched; revisit in C). The autonomous.rs:560 split bus-send already covers split.
- [ ] **Step 3: TUI-untouched check** — `git diff --stat main -- pancetta-tui` MUST be empty. The coordinator edits are additive (existing →Tui sends byte-identical).
- [ ] **Step 4: Verify** — `cargo build -p pancetta` clean; with gateway disabled (default), behavior unchanged (the dual-sends are skipped).
- [ ] **Step 5: Commit** — `feat(gateway): wire startup + additive dual-destination event feeds (gated default-off)`.

---

## Task 7: Docs + final verification

- [ ] **Step 1:** CLAUDE.md architecture bullet for `remote_gateway` (default-off, localhost, read-only, axum, `pancetta-protocol`, additive-dual-destination, no remote-TX). Reference the spec.
- [ ] **Step 2: Full gate** — `bash scripts/check.sh` (station down → fine). Must pass (incl. cargo-deny on the new deps).
- [ ] **Step 3: Manual smoke (optional)** — with `[network.remote_gateway] enabled=true` in a test config, start pancetta headless, `wscat`/a tiny client to `ws://127.0.0.1:4080/ws`, send `Hello`, see `Welcome`+events. (Operator can do this; or a scripted check.)
- [ ] **Step 4:** Land via the controller (push, single gate).

---

## Self-review checkpoints
- **Spec §10.1 v1 view:** DX Hunter (decodes→DxRow + DxMessage spots) ✓; QSO progress (ActiveQsos→QsoProgress) ✓. Control = NONE (Sub-plan C) ✓.
- **TUI invariant:** all coordinator edits additive; `pancetta-tui` diff empty (Task 6 step 3) ✓.
- **Operator boundary:** localhost bind default; no control commands honored (read-only); plain ws:// localhost; rustls/wss + auth deferred to C ✓.
- **Default-off:** config `enabled=false` default; disabled → drain channel, no behavior change ✓.
- **cargo-deny:** new deps vetted (Task 3 step 3) — the gate will enforce.

## Deferred to Sub-plan C
Control commands (CallStation/AnswerCaller/StartCq/SetFrequency) wired to the bus (mappings already in the event-flow map §5); station-paired auth; transmit-arm; fail-safe; control arbitration; rustls/wss; network exposure; TxPolicy in the stream; cqdx-network DX-Hunter rows; the priority f64 in DecodedView.
