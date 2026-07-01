# Station Agent P3.4c — Remote TX-initiation + heartbeat seq/arm_jti guard

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps `- [ ]`.
> SECURITY-CRITICAL. TDD, fail-closed. Run `cargo fmt --all` + `cargo clippy` + relevant tests in the
> FOREGROUND before committing. Never push (controller pushes).

**Goal:** Close the two deferred P3 follow-ups so a remote operator can actually work QSOs, safely:
(A) the heartbeat **seq-monotonic + arm_jti-match** guard (review IMPORTANT finding), and (B) route
remote TX-initiation (callStation/answerCaller/startCq) through the QSO engine as **`TxOrigin::Remote`**
so every frame it emits is arm-gated. Independent of dispensa Q-0014.

**Spec:** the P3 spec's follow-ups (`docs/superpowers/specs/2026-07-01-station-agent-p3-design.md`). Builds on P3 (merged, main).

**Branch:** `feat/station-agent-p3c` off main (already checked out).

**Regression invariant:** every existing (LOCAL) QSO's TX is byte-identical — `remote_origin` defaults
false ⇒ `TxOrigin::Local`. Remote is opt-in and default-OFF end to end.

---

## Piece A — heartbeat seq/arm_jti guard (pancetta-agent, offline)
### A1: VerifiedArmGrant carries jti; ArmState guards heartbeat
**Files:** `pancetta-agent/src/arm.rs`, `pancetta-agent/src/capability.rs`.
- [ ] Add `pub jti: String` to `arm::VerifiedArmGrant` (P3.2's `verify_arm_grant` already has the grant `jti` — set it). Update its constructor + all `#[cfg(test)]` builders.
- [ ] `ArmState`: on `arm(grant, now)` store `current_jti = grant.jti` and reset `last_heartbeat_seq = None`.
- [ ] Change `heartbeat(&mut self, now_ms)` → `heartbeat(&mut self, arm_jti: &str, seq: u64, now_ms) -> Vec<ArmEffect>`: **reject (no-op, return an audit `TxDenied`/`HeartbeatRejected` effect, do NOT slide the window)** if not armed, OR `arm_jti != current_jti`, OR `last_heartbeat_seq.is_some_and(|s| seq <= s)` (strictly monotonic). On accept: set `last_heartbeat_seq = Some(seq)` and slide `last_heartbeat_ms = now`.
- [ ] Tests: valid monotonic seq accepted (window slides); `seq <= last` rejected (window does NOT slide → dead-man still expires — assert `tx_permitted` flips false at the ORIGINAL timeout, proving a replayed heartbeat can't hold the arm); wrong `arm_jti` rejected; heartbeat while unarmed no-ops; a fresh `arm` resets seq (a stale seq from a prior arm doesn't block the new one). Update the property test's heartbeat calls to the new signature.
- [ ] Update all call sites (grep `\.heartbeat(` in pancetta-agent + pancetta). Commit `feat(agent): heartbeat seq-monotonic + arm_jti guard (dead-man can't be held open by replay)`.

### A2: component passes arm_jti + seq
**Files:** `pancetta/src/coordinator/station_agent/mod.rs`.
- [ ] `dispatch_action` `Heartbeat{arm_jti, seq}` → `remote_tx_arm.lock().heartbeat(&arm_jti, seq, now)`; audit on reject. Remove the `TODO(P3.4c)` there + at `capability.rs:50`.
- [ ] Test: a `Heartbeat` with a stale seq does not extend the arm (the e2e heartbeat-loss test still auto-disarms). Commit `feat(coord): station-agent heartbeat passes arm_jti+seq to the guard`.

## Piece B — remote TX-initiation through the QSO engine (arm-gated)
### B1: `QsoMetadata.remote_origin` + TransmitRequest origin threading
**Files:** `pancetta-qso/src/states.rs` (`QsoMetadata`), `pancetta-qso/src/qso_manager.rs` (initiation paths), `pancetta/src/coordinator/qso.rs` (`MessageToSend`→`TransmitRequest`).
- [ ] Add `#[serde(default)] pub remote_origin: bool` to `QsoMetadata` (default false at EVERY construction site — grep `QsoMetadata {`; byte-identical for all existing QSOs).
- [ ] Add a `remote_origin: bool` param to the manual initiation entry points the remote path uses (`respond_to_cq_with` / `respond_to_caller` / `start_cq` — thread it into the created `QsoMetadata.remote_origin`; existing callers pass `false`). (These already gained `partner_freq` similarly — mirror that.)
- [ ] In `coordinator/qso.rs` at the `MessageToSend`→`TransmitRequest` build (~:1645) **and every other TransmitRequest/MultiTransmitRequest built from a QSO's MessageToSend**, set `origin: if metadata.remote_origin { TxOrigin::Remote } else { TxOrigin::Local }`. (Look up the QSO's metadata by qso_id — the forwarder already has it, or fetch it.) **This is the security-critical line: a remote QSO's TX MUST be Remote.**
- [ ] Tests (`pancetta-qso` + `coord_sim`): a `remote_origin=true` QSO's emitted TransmitRequest carries `TxOrigin::Remote`; a normal QSO's carries `Local` (regression). coord_sim: a remote-origin QSO's TX is **dropped when the arm is not permitted** and **keys when armed** (reuse the P2.3/P3.4b arm-gate scenarios with a remote-origin QSO). Commit `feat(qso+coord): remote_origin QSOs emit TxOrigin::Remote (arm-gated end to end)`.

### B2: station-agent routes TxRequest → remote-origin QSO
**Files:** `pancetta/src/coordinator/station_agent/mod.rs`, the `QsoMessage` bus enum + its coord handler.
- [ ] `dispatch_action` `TxRequest(TxKind)`: instead of the current audited-but-dropped stub, route to the QSO engine:
  - `CallStation{callsign, frequency_hz, dx_parity}` → a bus `QsoMessage` that calls `respond_to_cq_with(..., remote_origin=true)` (mirror the TUI `CallStation`/`StartQso` handler, but remote-origin + still gated by the arm at TX time — the arm gate is the TX authority; QSO *creation* is allowed, TX is what's gated).
  - `AnswerCaller{...}` → `respond_to_caller(..., remote_origin=true)`.
  - `StartCq{...}` → `start_cq(..., remote_origin=true)`.
  Add remote-origin variants/params to the relevant `QsoMessage`s (or a `remote_origin` field), default false for all existing (TUI/autonomous) senders.
- [ ] **Do NOT bypass the arm:** the remote QSO's TransmitRequests are `TxOrigin::Remote` (B1) → the coordinator TX worker only keys them when `remote_tx_arm.tx_permitted()`. So an unarmed remote operator can create a QSO but it never transmits (every frame dropped + audited). Confirm + test this is the behavior (no arm ⇒ QSO created but silent).
- [ ] Tests: a remote `CallStation` with a live arm → the QSO's opening TX keys PTT (coord_sim); with no arm → created but no PTT (dropped, audited); disarm mid-QSO → subsequent TX dropped. Commit `feat(coord): station-agent routes remote TxRequest → remote-origin QSO (arm-gated, no bypass)`.

## Land
- [ ] CLAUDE.md: update the station-agent bullet — P3.4c wired (heartbeat guard live; remote TX-initiation via remote_origin QSOs, arm-gated; TODO(P3.4c) removed). Note remote_origin default-false = local byte-identical.
- [ ] Adversarial security-review subagent (focus: a remote QSO can NEVER emit `TxOrigin::Local`; heartbeat replay/wrong-jti can't hold the arm; unarmed remote QSO is silent; local QSOs byte-identical; no arm-bypass path).
- [ ] full workspace gate → PR → **wait CI green** → merge; sync main.

---
## Self-review
- **Security invariant:** remote-initiated QSO ⇒ `remote_origin=true` ⇒ every TransmitRequest `TxOrigin::Remote` ⇒ arm-gated. No path emits a remote QSO's TX as Local.
- **Dead-man hardened:** heartbeat requires monotonic seq + matching arm_jti; a replayed heartbeat cannot hold an arm past its window.
- **Regression:** `remote_origin` + heartbeat-sig changes default to today's behavior for all local/TUI/autonomous QSOs (byte-identical).
- **No arm bypass:** QSO creation is allowed remotely; TRANSMISSION is the gated act (arm + local consent + Shift+Q primacy all still apply).
