---
id: remote-operation
title: How does remote operation work and why is it shaped this way?
kind: subsystem
status: current
maintainer: agent
sources:
  - pancetta/src/coordinator/remote_gateway/mod.rs
  - pancetta/src/coordinator/station_agent/mod.rs
  - pancetta-agent/src/arm.rs
  - pancetta-agent/src/audit.rs
  - pancetta-config/src/network.rs
  - docs/DECISIONS/remote-operation.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
  - fail-closed-arm-gate
  - additive-only-gateway
---
Remote operation is two separate, default-OFF subsystems: a **read-only
gateway** (`remote_gateway/`) that streams decodes/QSO-progress/status to a
client (panino) over localhost WebSocket with NO control path, and the
**station agent** (`pancetta-agent` + `coordinator/station_agent/`) that is the
final authority for remote *TX*, gated by an armed-TX state machine. The safety
model, wire contracts (dispensa ADR-0002/0003/0009, `e2e-auth.v1`), and TTL
constants are normative in the code and `docs/DECISIONS/remote-operation.md` —
cited, not restated. Spec:
`docs/superpowers/specs/2026-06-26-remote-operation-design.md`.

## How it works

- **Read-only gateway** — a component mirroring `start_pskreporter_component`;
  it only *reads* the bus. Its feed is [[additive-only-gateway]]: extra bus
  sends gated behind an atomic, existing `→Tui` sends untouched.
- **Station agent** — loads identity, dials the relay, runs Noise-IK E2E, and
  dispatches control frames (Arm/Heartbeat/Disarm/Qsy/TxRequest). Remote TX
  requests carry `TxOrigin::Remote` and are arm-gated end to end.
- **Armed-TX gate** — `ArmState::tx_permitted` (`pancetta-agent/src/arm.rs`) is
  a pure AND of armed ∧ scope ∧ TTL ∧ heartbeat-fresh ∧ consent ∧ ¬kill; the
  coordinator gate [[fail-closed-arm-gate]] fails CLOSED and ANDs under
  `TxPolicy`.
- **Gotcha: relay IO must stay bounded (PAN-102).** Never add an untimed
  `block_in_place` wait to `RealWsConn`: code inside `block_in_place` cannot
  be cancelled, so an unbounded wait can hold the session task past
  `abort()`. Every wait takes a `RelayTimeouts` deadline, and a relay
  transport failure must end the whole session (the socket is shared by every
  peer), which then disarms and reconnects.

## Why it is shaped this way

Read and TX are split so the low-risk view can ship freely while the TX path
carries the full Part-97 safety burden: fail-closed, dead-man heartbeat,
local-consent, and no autonomous-over-remote. The agent remains the final TX
authority; no remote QSO frame is ever emitted as `TxOrigin::Local`.

## Gotcha: the audit log is shared, bounded, and never on the safety path (PAN-104)

Every `AuditLog` clone shares one writer mutex, rate buckets, counters and
health, so rotation stays bounded across the station agent, the TX worker and
config reload. Never write audit records while holding the arm mutex or an
eligibility lock: collect the `ArmEffect`s under the lock, publish the disarm
and the list change, then call `apply_arm_effects`. The two revocation paths
(`revoke_static_allow_list_entries` and the authorization poll) used to audit
under the arm lock, so a slow disk delayed both the revocation and every TX
re-check queued behind it. TX-worker interrupts release PTT before the audit
write. Audit failure is a readiness warning only, never a TX mute cause, and a
valid arm proceeds. The normative text is in
`docs/DECISIONS/remote-operation.md` (PAN-104) and `docs/CONFIG.md`.
