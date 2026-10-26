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

- **Worker inbox** — the TX worker reads its requeue front before its bus channel; anything it takes off the channel and puts back goes on that front (`TxInbox::requeue_front`), never back through the bus (PAN-73).
- **Slot scheduling** — `resolve_required_parity` / `schedule_tx` in
  `pancetta/src/coordinator/tx.rs` pick and time the slot; early = pad silence,
  late = skip-ahead cursor up to a bounded budget, then defer.
- **Coalescing** — `coalesce_transmit_requests` multi-streams concurrent
  same-parity QSOs into one `MultiTransmitRequest`; a short collection window
  batches serial manual keypresses (the multi-TX slow-start fix, still only a
  partial mitigation — see `project_symptom_c_multi_tx_deferred` in the
  assistant's memory). Bundles now **exclude** (never coerce) a stream whose
  parity or frequency disagrees with the rest of the bundle (2026-07-18).
  Gotcha: `active_tx_qsos` is undifferentiated, so it cannot tell a Fox Hound
  from an ordinary over-cap QSO; Fox provenance lives in `fox_admitted_qsos`,
  and the coalescer raises its cap to the Fox-admitted streams *present* in
  the backlog, never the set's size (PAN-76, `docs/DECISIONS/tx-scheduling.md`).
- **Drop-stale-TX gate** — the worker re-checks QSO liveness (`tx_qso_is_live`)
  at the last instant before PTT and drops frames whose QSO has ended, in
  **both** the single-TX and multi-TX arms (the multi-TX arm's defer-time
  recheck was missing until 2026-07-18); this gate **fails open** on a
  poisoned lock (contrast the safety gate in [[fail-closed-arm-gate]]).
- **Offset hold** — a QSO's TX audio offset is latched at open and held for the
  whole exchange; the only mid-QSO mover is the adaptive stall escape (PAN-72):
  `stall_cycles` counts slots the DX failed to advance, and at the threshold the
  QSO switches to an allocator-picked offset or reverts to the last known-good
  one. Auto mode only; `u` forces the same move on demand.
  The frequency allocator itself had 4 independent scoring bugs (mislabeled
  spectral axis, wall-clock-vs-decode parity stamping, a scoring floor that
  clamped the wrong way, and a dead own-frequency registry) fixed 2026-07-18.
- **Known-good crediting reads aired frames, not just provenance** (PAN-83):
  after a relocation, a grace-window advance on a split credits the offset of
  the frame that aired in our slot just before the reply's — the TX worker's
  `TxFrameSent` report, recorded per QSO in `KeepCallLedger::last_aired`. Tests
  that want the aired-frame path must go through `process_message_with_parity`
  and report the aired frame in the reply's preceding slot (see
  `reply_window_now` in `pan72_stall_detection_tests`); `process_message`
  without parity exercises the provenance fallback.
- **The double-PTT-for-73 bug** (open since 2026-07-17) was root-caused and
  fixed 2026-07-18: `coordinator/tx.rs`'s Step-4c late pivot rewrote an
  in-flight frame's text to the freshest intent without consuming it, so the
  newer request that produced that intent keyed the identical 73 again a slot
  later. Fixed with a worker-local consume-once tombstone
  (`is_pivot_duplicate`, `coordinator/mod.rs`) — see
  `docs/qso-tx-deep-review-2026-07-18.md` for the full mechanism.
- **Pivot ownership (PAN-185):** a pivot only targets an intent whose request
  is confirmed queued (`request_queued`), and the tombstone decides who tells
  Autonomous: a refused pivot stays silent and its queued request reports; a
  keyed pivot reports and its absorbed request drops silently. See
  `docs/DECISIONS/tx-scheduling.md`.

## Why it is shaped this way

FT8 is deaf while transmitting, so the half-duplex [[parity-rule]] is the
load-bearing constraint: every concurrent active QSO must share one parity so
the opposite window stays free to hear. Read that gotcha before changing any
admission or coalescing logic.

Testing gotcha (PAN-82): a test that trips the stall threshold via
`rearm_manual_calls_at` must rearm on DISTINCT own-parity TX slots (the
`for slot in [1i64, 3]` / `opened_at + 15*slot` pattern — a +15/+30 pair can
resolve to the same own-parity slot under PAN-128's one-count-per-own-parity-
slot rule, leaving the threshold never deterministically tripped) and must
report the threshold frame aired (`confirm_last_frame_aired`) before a
`StallDetected`-origin `apply_tx_offset_switch`, or PAN-182's trigger hold
refuses the commit with `OffsetActionTriggerOutcomePending` on wall-clock
phase alone. See `an_advance_rescuing_the_pre_switch_cq_latches_the_callers_frequency`
and `an_advance_answering_the_pre_switch_frame_credits_the_old_offset` in
`pancetta-qso/src/qso_manager.rs` for the repaired shape.

Caller-admission gotcha (PAN-120): the QSO decode loop and autonomous initiation
are separate paths. A new direct caller now requires local console presence or
`autonomous.unattended_policy = "respond_only"`, even with the autonomous toggle
off. The default is `off` and changes need a restart. Existing exchanges still
advance without presence; autonomous CQ and pounce still require it. See the
[normative decision](../../docs/DECISIONS/tx-scheduling.md#direct-caller-presence-policy-pan-120).
