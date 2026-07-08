---
id: tx-scheduling
title: How does TX scheduling work and why is it shaped this way?
kind: subsystem
status: current
maintainer: agent
sources:
  - pancetta/src/coordinator/tx.rs
  - pancetta/src/coordinator/qso.rs
  - pancetta-qso/src/qso_manager.rs
  - docs/DECISIONS/tx-scheduling.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
  - parity-rule
---
TX scheduling is WSJT-X-style and driven by *slot parity*: every decoded frame
carries a `slot_parity`, a QSO latches `tx_parity = opposite_of(dx_parity)` at
start, and the scheduler keys the next slot of that parity so we never collide
with the DX and stay half-duplex-safe. The constants (late-cursor budget, defer
window, collect window, completed-TX grace) are normative in the code and the
digest — this page orients, it does not restate them. Full digest:
`docs/DECISIONS/tx-scheduling.md`; design spec
`docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`.

## How it works

- **Slot scheduling** — `resolve_required_parity` / `schedule_tx` in
  `pancetta/src/coordinator/tx.rs` pick and time the slot; early = pad silence,
  late = skip-ahead cursor up to a bounded budget, then defer.
- **Coalescing** — `coalesce_transmit_requests` multi-streams concurrent
  same-parity QSOs into one `MultiTransmitRequest`; a short collection window
  batches serial manual keypresses (the multi-TX slow-start fix).
- **Drop-stale-TX gate** — the worker re-checks QSO liveness (`tx_qso_is_live`)
  at the last instant before PTT and drops frames whose QSO has ended; this gate
  **fails open** on a poisoned lock (contrast the safety gate in
  [[fail-closed-arm-gate]]).
- **Offset hold** — a QSO's TX audio offset is latched at open and held for the
  whole exchange; the only mid-QSO mover is the operator/stuck-DX escape hop.

## Why it is shaped this way

FT8 is deaf while transmitting, so the half-duplex [[parity-rule]] is the
load-bearing constraint: every concurrent active QSO must share one parity so
the opposite window stays free to hear. Read that gotcha before changing any
admission or coalescing logic.
