# Station Agent — Offline Safety Core (P0–P2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.
> Each implementer: run `cargo fmt --all` + `cargo check --workspace --all-targets --features transmit` in the **FOREGROUND** (blocking) before committing. Never push (controller pushes). This is **security-critical** code — TDD, small commits, adversarial tests.

**Goal:** Build the schema-independent, offline, safety-critical core of the pancetta station agent: the Noise E2E layer and the armed-TX gate (with dead-man/heartbeat, local consent gate, and audit log), fully unit- and adversarially-tested WITHOUT any network. P3 (relay/pairing wire) is deferred until cqdx records `relay.v1`/`pairing.v1`.

**Spec:** `docs/superpowers/specs/2026-06-30-station-agent-design.md`. Security model: dispensa ADR-0002 (Accepted). **Operator decisions (2026-06-30):** build P0–P2 now; add a station-side local `remote_tx_enabled` gate (default OFF) ANDed with the token/arm.

**Branch:** `feat/station-agent` (off main; spec already committed).

**Regression invariant across all phases:** the agent is **default-OFF and not wired into the running coordinator in P0–P2** — it's new crates/modules with no call site in the live path yet. The existing station (TUI/local TX/FT8) is untouched. The ONLY coordinator change is P2's TX-worker arm check, which is a no-op unless an arm is set (and no code sets it until P3/P4).

---

## Phase 0 — `pancetta-agent` crate + Noise conformance

### Task P0.1: crate scaffold
**Files:** `pancetta-agent/Cargo.toml`, `pancetta-agent/src/lib.rs`, root `Cargo.toml` (workspace members).
- [ ] Create `pancetta-agent` crate; add to workspace `members` (NOT `default-members` if the workspace distinguishes — match how `pancetta-protocol` is listed). Deps: `snow = "0.9"`, `thiserror`, `tracing`, `serde`, `serde_json`; dev-deps: `hex`. `#![allow(missing_docs)] // TODO` like sibling crates.
- [ ] `lib.rs`: module stubs `pub mod noise;` `pub mod arm;` `pub mod audit;` (empty for now).
- [ ] Verify: `cargo build -p pancetta-agent`. Commit `feat(agent): pancetta-agent crate scaffold`.

### Task P0.2: Noise interop conformance test (drift-guarded)
**Files:** `pancetta-agent/tests/noise_vectors.rs`, vendor a copy of the vector.
- [ ] Vendor `contracts/auth/noise-ik.vectors.v1.json` from dispensa into `pancetta-agent/tests/fixtures/noise-ik.vectors.v1.json` (copy the file; add a header comment: source = dispensa `contracts/auth/noise-ik.vectors.v1.json`, keep in sync).
- [ ] Test `ik_vector_byte_for_byte`: load the fixture, drive `snow` as BOTH initiator and responder with the fixed static+ephemeral keys (`Builder::…fixed_ephemeral_key_for_testing_only`), prologue `"John Galt"`, and assert msg1, msg2, `get_handshake_hash()` (both sides), and all transport messages (incl the nonce=1 pair) equal the vector's ciphertext hex byte-for-byte. (Reference impl that PASSES — controller already verified snow 0.9.6 reproduces it: initiator local=init_static remote=init_remote_static; responder local=resp_static; transport order init→resp, resp→init, init→resp(n=1), resp→init(n=1).)
- [ ] Verify: `cargo test -p pancetta-agent noise_vectors`. Commit `test(agent): Noise IK interop conformance vs shared vector (pancetta ✓)`.

## Phase 1 — Noise session wrapper (L2)

### Task P1.1: `NoiseResponder` / `NoiseInitiator` session API
**Files:** `pancetta-agent/src/noise.rs`.
- [ ] A thin, misuse-resistant wrapper over snow for the agent (responder) role: `NoiseResponder::new(local_static: &[u8;32]) -> Result<Handshaking>`; `Handshaking::read_msg1(&mut self, buf) -> Result<()>`, `write_msg2(&mut self, payload) -> Result<Vec<u8>>`, `into_transport(self) -> Result<Transport>`; `Transport::{encrypt(&mut, pt)->Vec<u8>, decrypt(&mut, ct)->Result<Vec<u8>>}`. Errors via `thiserror` (`NoiseError`). Zeroize the static on drop if cheap (or note as follow-up). Provide a matching `NoiseInitiator` for tests/loopback only (behind `#[cfg(test)]` or a `testing` feature — the agent is only ever the responder in production).
- [ ] Tests: **loopback** — initiator and responder complete the handshake with random keys and round-trip several transport messages both directions; a tampered ciphertext fails `decrypt`; a replayed transport message fails `decrypt` (Noise counter). Assert the responder never accepts a second msg1.
- [ ] Verify foreground: `cargo test -p pancetta-agent noise`. Commit `feat(agent): Noise responder/initiator session wrapper + loopback tests`.

## Phase 2 — Armed-TX gate + dead-man + local consent + audit (the security core)

> This is the crown-jewel. It is a **pure state machine driven by a clock and events** — NO network, NO real PTT. It produces an "is TX permitted right now?" answer the coordinator TX worker will consult. Build + adversarially test it entirely offline.

### Task P2.1: audit log
**Files:** `pancetta-agent/src/audit.rs`.
- [ ] Append-only JSONL audit writer `AuditLog` → `~/.pancetta/agent-audit.log` (path injectable for tests). Records `AuditEvent { ts, kind: Armed|Disarmed|TxRequested|TxDenied|LocalKill, operator_callsign: Option<String>, detail }`. Append + flush per event; never panics on IO error (log at warn, target `agent.audit`). Timestamps passed in (no `Utc::now()` inside — inject a clock, mirror the slot-test style) so tests are deterministic.
- [ ] Tests: events append as one JSON object per line; a temp-file log round-trips; IO error is swallowed. Commit `feat(agent): append-only JSONL audit log`.

### Task P2.2: ARM state machine (pure)
**Files:** `pancetta-agent/src/arm.rs`.
- [ ] `ArmState` machine, pure, clock-injected. Inputs (events): `Arm { operator_callsign, scope, ttl, now }` (requires a pre-validated capability — the *token verification* is P3; here assume the caller passes a `VerifiedArmGrant` value so this layer is pure policy), `Heartbeat { now }`, `Disarm`, `LocalKillEngaged`, `LocalConsent(bool)` (the station-side `remote_tx_enabled` toggle), `Tick { now }`. Output: `tx_permitted(now) -> bool` and a `reason`.
- [ ] **Invariants (encode as the state machine + tests):**
  - TX permitted ⇔ **ALL** of: an unexpired arm (now < armed_at + ttl), a fresh heartbeat (now − last_heartbeat < `HEARTBEAT_TIMEOUT`, default 30s), local consent ON, and not locally-killed. It is an **AND** of every gate — never an OR.
  - `Disarm`, TTL expiry, heartbeat timeout, `LocalKillEngaged`, or `LocalConsent(false)` each independently → `tx_permitted == false` immediately.
  - A `Tick` past TTL or heartbeat-timeout auto-disarms (emits an audit `Disarmed{reason}` via a returned effect, not by calling audit directly — keep it pure; return `Vec<ArmEffect>`).
  - Re-arm requires a fresh grant (an expired arm never silently resurrects).
- [ ] **Adversarial tests:** no-arm ⇒ never permitted; arm without local consent ⇒ never permitted; arm + consent then heartbeat stops ⇒ permitted flips false at `HEARTBEAT_TIMEOUT`; arm + consent then `LocalKillEngaged` ⇒ false and stays false until re-armed AND kill cleared; TTL boundary exact (permitted at ttl−1, not at ttl); local consent OFF overrides an otherwise-valid arm. Property test: for random event sequences, `tx_permitted` is false whenever ANY gate is closed.
- [ ] Commit `feat(agent): armed-TX state machine (AND of arm+heartbeat+local-consent+not-killed) + adversarial tests`.

### Task P2.3: coordinator TX-worker arm gate (the one live-path change)
**Files:** `pancetta/src/coordinator/{mod.rs,tx.rs}`.
- [ ] Add a coordinator-held `remote_tx_arm: Arc<...>` shared handle exposing `tx_permitted() -> bool` (backed by the P2.2 state machine behind a lock, or a snapshot atomic the agent updates). In P0–P2 **nothing sets an arm**, and `remote_tx_enabled` local consent defaults OFF, so `tx_permitted()` is **always false** → the gate is inert for all *remote* TX. 
- [ ] In `tx.rs`, gate ONLY **remote-originated** TX requests (a new `TransmitRequest.origin: TxOrigin::{Local,Remote}` field, default `Local` at every existing call site so **local TX is byte-identical**). A `Remote` request keys PTT only if `remote_tx_arm.tx_permitted()`; else drop it (no PTT, audit `TxDenied`, clear strip) — exactly like the drop-stale-TX gate. Local requests are never gated by the arm. This composes with (ANDs under) the existing `TxPolicy` — `Disabled` still hard-mutes everything.
- [ ] Tests (coord_sim / tx unit): a `Remote` request with no arm → dropped, no PTT; a `Local` request → unaffected (regression, byte-identical); with a test-injected live arm, a `Remote` request keys PTT; `TxPolicy::Disabled` overrides even a live arm.
- [ ] Verify foreground (full workspace check). Commit `feat(coord/tx): remote-TX arm gate (inert until agent arms; local TX unchanged)`.

### Task P2.4: config + docs
**Files:** `pancetta-config/src/network.rs` (or a new `agent.rs`), `CLAUDE.md`.
- [ ] `[network.station_agent]` config `StationAgentConfig { enabled: bool = false, remote_tx_enabled: bool = false, audit_log_path: Option<PathBuf> }` (validated; enabled has no effect in P0–P2 since the transport isn't built). `remote_tx_enabled` is the LOCAL consent gate (default OFF).
- [ ] CLAUDE.md architecture bullet: the station agent (P0–P2 offline core landed; P3 relay/pairing pending cqdx contracts); emphasize default-OFF, local-consent-gate, agent-is-final-TX-authority, no autonomous-over-remote.
- [ ] Commit `feat(config)+docs(agent): station_agent config (default OFF) + CLAUDE.md`.

## Phase wrap
- [ ] Final code-review subagent over the branch — focus: **the ARM gate is a pure AND of all safety conditions and fails safe**; local TX byte-identical (origin defaults Local); nothing arms in P0–P2 so the live path is inert; Noise conformance real. 
- [ ] fast gate → PR → **wait CI green** → merge; sync main. (Then P3 waits on cqdx `relay.v1`/`pairing.v1`.)

---
## Self-review
- **Security core built + adversarially tested OFFLINE before any wire.** ✓
- **Local TX byte-identical** (`TxOrigin::Local` default; arm gates only Remote). ✓
- **AND-not-OR** arm composition; local consent default OFF; local-kill primacy preserved. ✓
- **Nothing is wired to arm** until P3/P4 — P0–P2 is inert in the live path. ✓
