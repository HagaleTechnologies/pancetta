---
id: fail-closed-arm-gate
title: What will bite you about the armed-TX gate?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta-agent/src/arm.rs
  - pancetta/src/coordinator/tx.rs
  - pancetta/src/coordinator/station_agent/mod.rs
verified:
  commit: 6e65be2
  date: 2026-10-13
links:
  - remote-operation
---
The remote-TX arm gate **fails CLOSED**: on a poisoned lock (or any verify
error) it denies TX, the exact opposite of the drop-stale-TX liveness gate,
which fails *open*. Two gates guarding the same PTT with opposite failure
directions live a few lines apart in the same file — copy the wrong pattern and
you either mute local TX on a lock hiccup or, far worse, permit unattended
remote TX when the gate should have refused. The arm gate also ANDs *under*
`TxPolicy` (Disabled hard-mutes everything first) and is only applied to
`TxOrigin::Remote` requests; local TX is byte-identical and never gated here.

## Symptom

Either remote TX that should be denied gets keyed (a Part-97 safety failure), or
a poisoned-lock path that should key local TX goes silent. Both come from
mixing up which gate you are near.

## Where the invariant lives

- `pancetta-agent/src/arm.rs:577` — `ArmState::tx_permitted`, the pure clock-free
  AND (armed ∧ scope ∧ TTL ∧ heartbeat-fresh ∧ consent ∧ ¬kill). Adding a
  conjunct can only make it *more* restrictive.
- `pancetta/src/coordinator/tx.rs:3699` — `remote_tx_permitted`, the coordinator
  gate that **fails closed** before keying PTT.
- `pancetta/src/coordinator/tx.rs:3222` — `tx_qso_is_live`, the neighboring
  liveness gate that **fails open** — the one you must NOT mirror for safety.

## A lapsed arm stays lapsed

`ArmState::heartbeat` (`arm.rs:436`) expires the arm *before* checking
`armJti`/`seq`, and the station session loop runs `tick` on every iteration
(`station_agent/mod.rs:1910`). A heartbeat at or after the 30 s deadline can
never revive the arm; only a fresh signed `txArm` can. The 5–15 s
`heartbeatIntervalSec` is the client's send cadence, not the station's timeout.

Timing-sensitive calls take an `ArmTime` (wall ms + monotonic `Instant`):
silence and lifetime use the monotonic clock, the signed `armedUntil` uses wall
time. The gate helpers take a closure (`ArmTime::now` in production) and call it
only after locking the arm, so a lock wait cannot make the sample stale.

Testing gotchas: build `ArmTime`s from one shared `Instant` origin plus explicit
offsets; `tokio::time::pause` does not move `std::time::Instant`. Session-loop
tests inject `ArmContext::clock` instead of sleeping, and must arm on that same
timeline. A `MockWs` with an empty queue reports *Closed*, not *Quiet*, so an
established-but-silent session needs a transport that returns
`RecvOutcome::Quiet` (see the `Pan97Ws` harness in the station-agent tests).

Rationale, TTL/heartbeat constants, and the `e2e-auth.v1` contract are normative
in `docs/DECISIONS/remote-operation.md` and the code; see [[remote-operation]].
