---
id: fail-closed-arm-gate
title: What will bite you about the armed-TX gate?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta-agent/src/arm.rs
  - pancetta/src/coordinator/tx.rs
verified:
  commit: eac7aab
  date: 2026-07-07
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

- `pancetta-agent/src/arm.rs:436` — `ArmState::tx_permitted`, the pure clock-free
  AND (armed ∧ scope ∧ TTL ∧ heartbeat-fresh ∧ consent ∧ ¬kill). Adding a
  conjunct can only make it *more* restrictive.
- `pancetta/src/coordinator/tx.rs:337` — `remote_tx_permitted`, the coordinator
  gate that **fails closed** before keying PTT.
- `pancetta/src/coordinator/tx.rs:315` — `tx_qso_is_live`, the neighboring
  liveness gate that **fails open** — the one you must NOT mirror for safety.

Rationale, TTL/heartbeat constants, and the `e2e-auth.v1` contract are normative
in `docs/DECISIONS/remote-operation.md` and the code; see [[remote-operation]].
