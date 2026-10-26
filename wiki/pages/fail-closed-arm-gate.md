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

- `pancetta-agent/src/arm.rs` — `ArmState::tx_permitted`, the pure clock-free
  AND of armed, scope, all authority deadlines, fresh heartbeat, consent and no kill. Adding a
  conjunct can only make it *more* restrictive.
- `pancetta/src/coordinator/tx.rs` — `remote_tx_permitted`, the coordinator
  gate that **fails closed** before keying PTT.
- `pancetta/src/coordinator/tx.rs` — `tx_qso_is_live`, the neighboring
  liveness gate that **fails open** — the one you must NOT mirror for safety.

## A lapsed arm stays lapsed

`ArmState::heartbeat` (`arm.rs`) expires the arm *before* checking
`armJti`/`seq`, and the station session loop runs `tick` on every iteration
(`station_agent/mod.rs`). A heartbeat at or after the 30 s deadline can
never revive the arm; only a fresh signed `txArm` can. The 5–15 s
`heartbeatIntervalSec` is the client's send cadence, not the station's timeout.

Timing-sensitive calls take an `ArmTime` (wall ms + monotonic `Instant` +
suspend-aware ms): silence and lifetime use both the monotonic and the
suspend-aware clock (either reaching a limit expires the arm, so a host suspend
longer than 30 s needs a fresh arm). Wall time checks the earliest of signed `armedUntil`,
capability expiry and TX enablement. The verification-time effective lifetime cap cannot
be enlarged by acceptance delay or rollback. The gate helpers take a closure (`ArmTime::now` in production) and call it
only after locking the arm, so a lock wait cannot make the sample stale.

Testing gotchas: build `ArmTime`s from one shared `Instant` origin plus explicit
offsets; `tokio::time::pause` does not move `std::time::Instant`. Session-loop
tests inject `ArmContext::clock` instead of sleeping, and must arm on that same
timeline. A `MockWs` with an empty queue reports *Closed*, not *Quiet*, so an
established-but-silent session needs a transport that returns
`RecvOutcome::Quiet` (see the `Pan97Ws` harness in the station-agent tests).

Rationale, TTL/heartbeat constants, and the `e2e-auth.v1` contract are normative
in `docs/DECISIONS/remote-operation.md` and the code; see [[remote-operation]].

PAN-105 keeps the capability and enablement horizons on the accepted arm. Equality expires.
Ties prefer capability, then enablement, then arm, with reasons `capability-expired`,
`tx-enablement-expired`, and `ttl-expired`. Real enabled tokens require equal capability and
enablement horizons; unequal-horizon fixtures test the state machine only. Fresh heartbeats,
renewed hello authority and restored consent cannot extend the old arm. A fresh signed arm
is required after expiry.

Active remote single and summed-bundle playback polls this same gate about every 50 ms.
Denial releases PTT before confirmed audio flush and unsuccessful completion. It does not
wait for audit or state notifications. Device and transport scheduling add latency. The
playback-wait helper accepts a clock, with production supplying `ArmTime::now`; worker tests
advance the accepted arm's timeline through that clock, without substituting `disarm()`.
