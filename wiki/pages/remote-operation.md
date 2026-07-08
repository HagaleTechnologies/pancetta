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

## Why it is shaped this way

Read and TX are split so the low-risk view can ship freely while the TX path
carries the full Part-97 safety burden: fail-closed, dead-man heartbeat,
local-consent, and no autonomous-over-remote. The agent remains the final TX
authority; no remote QSO frame is ever emitted as `TxOrigin::Local`.
