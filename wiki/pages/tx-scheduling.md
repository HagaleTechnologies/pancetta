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
  commit: 6927e02c
  date: 2026-07-18
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
`docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`. A 2026-07-18
deep review (`docs/qso-tx-deep-review-2026-07-18.md`) re-audited the scheduler,
the frequency allocator, and multi-TX end to end and shipped 5 batches of
fixes, including the open double-PTT-for-73 bug — see that doc and
`docs/DECISIONS/tx-scheduling.md`'s own summary section for what changed.

## How it works

- **Slot scheduling** — `resolve_required_parity` / `schedule_tx` in
  `pancetta/src/coordinator/tx.rs` pick and time the slot; early = pad silence,
  late = skip-ahead cursor up to a bounded budget, then defer.
- **Coalescing** — `coalesce_transmit_requests` multi-streams concurrent
  same-parity QSOs into one `MultiTransmitRequest`; a short collection window
  batches serial manual keypresses (the multi-TX slow-start fix, still only a
  partial mitigation — see `project_symptom_c_multi_tx_deferred` in the
  assistant's memory). Bundles now **exclude** (never coerce) a stream whose
  parity or frequency disagrees with the rest of the bundle (2026-07-18).
- **Drop-stale-TX gate** — the worker re-checks QSO liveness (`tx_qso_is_live`)
  at the last instant before PTT and drops frames whose QSO has ended, in
  **both** the single-TX and multi-TX arms (the multi-TX arm's defer-time
  recheck was missing until 2026-07-18); this gate **fails open** on a
  poisoned lock (contrast the safety gate in [[fail-closed-arm-gate]]).
- **Offset hold** — a QSO's TX audio offset is latched at open and held for the
  whole exchange; the only mid-QSO mover is the operator/stuck-DX escape hop.
  The frequency allocator itself had 4 independent scoring bugs (mislabeled
  spectral axis, wall-clock-vs-decode parity stamping, a scoring floor that
  clamped the wrong way, and a dead own-frequency registry) fixed 2026-07-18.
- **The double-PTT-for-73 bug** (open since 2026-07-17) was root-caused and
  fixed 2026-07-18: `coordinator/tx.rs`'s Step-4c late pivot rewrote an
  in-flight frame's text to the freshest intent without consuming it, so the
  newer request that produced that intent keyed the identical 73 again a slot
  later. Fixed with a worker-local consume-once tombstone
  (`is_pivot_duplicate`, `coordinator/mod.rs`) — see
  `docs/qso-tx-deep-review-2026-07-18.md` for the full mechanism.

## Why it is shaped this way

FT8 is deaf while transmitting, so the half-duplex [[parity-rule]] is the
load-bearing constraint: every concurrent active QSO must share one parity so
the opposite window stays free to hear. Read that gotcha before changing any
admission or coalescing logic.
