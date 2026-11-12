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
- **Supersede ownership (PAN-279):** every unaired frame settles through
  `coordinator::tx_outcome::settle_unaired_frame`. A supersede's abandoned
  frame always rolls back its own attempt with the QSO engine, and fails to
  Autonomous only when no same-QSO successor (re-key, requeued superseder,
  carried bundle item) will report. A `NotViable` superseder is requeued at the
  front of the inbox, never dropped. Coalescer-excluded entries report to both
  consumers; folded entries with an attempt id roll back their own attempt.

A registered self-CQ opening has no accepted predecessor. The QSO forwarder
marks it before starting the CQ and consumes that marker on its first
`MessageToSend`, whether the TX channel accepts it or rejects it. A rejected
opening sends a durable failed `TransmitComplete` to Autonomous, after its
registration, so cancellation needs no later TX request to settle the pending
association and speculative state. An accepted opening leaves completion to
the worker. Later rejected resends send only the QSO accounting report because
an accepted predecessor may still own the outcome (PAN-238).

## Why it is shaped this way

FT8 is deaf while transmitting, so the half-duplex [[parity-rule]] is the
load-bearing constraint: every concurrent active QSO must share one parity so
the opposite window stays free to hear. Read that gotcha before changing any
admission or coalescing logic.

Testing gotcha (PAN-82, PAN-239): a test that trips the stall threshold via
`rearm_manual_calls_at` must rearm on DISTINCT own-parity TX slots, and should
anchor the rearm instants to the QSO's own slot grid (`next_own_ft8_slot`, as
`stall_on_two_own_parity_slots` does) rather than to `opened_at`. `opened_at`
is the real wall clock, so `opened_at`-relative rearms resolve to different
slots depending on the second the test started, and a +15/+30 pair can
collapse onto one slot under PAN-128's one-count-per-own-parity-slot rule. It
must also report the threshold frame aired (`confirm_last_frame_aired`) before
a `StallDetected`-origin `apply_tx_offset_switch`, or PAN-182's trigger hold
refuses the commit with `OffsetActionTriggerOutcomePending`. See
`an_advance_rescuing_the_pre_switch_cq_latches_the_callers_frequency` and
`an_advance_answering_the_pre_switch_frame_credits_the_old_offset` in
`pancetta-qso/src/qso_manager.rs`.

Testing gotcha (PAN-223): the stall ledger's slot comes from the TX worker's
`TxFrameScheduled` report (`QsoManager::record_scheduled_tx_slot`), not only
from the rearm's prediction, so a manager test of a deferred frame drives it by
calling that method with the slot the worker would pick. A real-worker test
that needs a deterministic schedule should give the request the parity of the
slot after `now` (it always defers to that slot's start), or wait into a slot
past FT8's 1.7 s acceptance cap; see `pan223_scheduled_slot_worker_tests` in
`pancetta/src/coordinator/tx.rs`.

Caller-admission gotcha (PAN-120): the QSO decode loop and autonomous initiation
are separate paths. A new direct caller now requires local console presence or
`autonomous.unattended_policy = "respond_only"`, even with the autonomous toggle
off. The default is `off` and changes need a restart. Existing exchanges still
advance without presence; autonomous CQ and pounce still require it. See the
[normative decision](../../docs/DECISIONS/tx-scheduling.md#direct-caller-presence-policy-pan-120).

TX-worker tests must wait on the worker stage they assert (PAN-237). In
`pancetta/src/coordinator/tx.rs`,
`headless_frame_is_durably_logged_even_when_the_tui_channel_is_full` leaves
TUI unread until the matching successful `TransmitComplete`. A visible log
record alone is too early: the best-effort TUI relay follows the durable
append. `dial_moved_out_of_band_before_key_is_refused` holds `ptt_sync_gate`
before submitting its request, waits for `tx_before_ptt_gate`, changes the
dial, and releases the mutex before awaiting completion. Queue status alone
does not hold the worker before its final band-plan check. Keep these event
boundaries when changing the tests; a settling sleep cannot establish either
ordering.

PTT-method gotcha (PAN-121): `[rig.ptt].method` does not choose how TX is keyed.
Normal PTT always goes to rigctld as `T 1`/`T 0`, and managed rigctld is spawned
without a PTT option. Setup, the rig picker and bookmarks only store or print the
method; only `test-rig --ptt` acts on it. PAN-339 makes that diagnostic use
the existing daemon for link checks and CAT PTT; Serial still pulses RTS.
Link-only testing never opens the configured serial device.
CAT and Serial pulses both require fresh split-aware band admission.
Unknown split state refuses PTT even with edge warnings disabled. PTT setters
require `RPRT 0`, and the diagnostic attempts unkey after key-down errors or
Ctrl+C. Mock-daemon tests in `pancetta/tests/test_rig_cli.rs` prove protocol
behavior, not physical rig operation.
There is no CM108 variant. See
[the audit](../../docs/DECISIONS/tx-scheduling.md#what-the-configured-ptt-method-actually-controls-pan-121-2026-11-01).

Supersede gotcha (PAN-322): a supersede that arrives in the PTT lead, after
key-down and before the target slot starts (Step 6), is always `NotViable`.
`schedule_tx` marks any slot but the current one `deferred`, and until the
target slot starts the current slot is the previous one. The worker unkeys,
settles the old frame and requeues the superseder for its own next slot. So the
real worker never reaches the single-frame Step 6 `Replace` and `Bundle`
handling. See
[the decision](../../docs/DECISIONS/tx-scheduling.md#one-handler-per-tx-worker-abort-outcome-pan-322-2026-11-10).

Testing gotcha (PAN-322): on a shutdown after key-down the worker sends no
PTT-off of its own. The `PttGuard` drop spawns a task that sends it, so its
place against the `TxStatusGuard` drop's TUI messages changes from run to run,
and bus message ids do not order the two. Assert order per channel, as
`pancetta/tests/fixtures/tx_abort_transcripts.golden` does, never one merged
order across channels.

Testing gotcha (PAN-322): to abort a worker test frame between key-down and the
target slot (Step 6), set `station.ptt_lead_ms = 2000`. With the default 80 ms
lead that window is an 80 ms race. Send the frame 4 to 10 s into a slot, so it
targets the next slot and waits the full lead, and act on the first
`SetPtt(true)`. `pancetta/src/coordinator/tx/abort_transcripts.rs` does this.

<<<<<<< HEAD
## Replaying playback and stall tests

PAN-325 gives `QsoManager` and the TX worker an injected UTC source, with
`Utc::now` as the production default. The pan72 stall tests create every manager
at the same even-slot origin. The pan71 playback tests derive UTC from paused
Tokio time and use current-thread runtimes. Advance Tokio time to choose a slot;
changing a test timestamp alone does not move the worker's clock. The arm clock
remains separate because it models remote authorization, not slot scheduling.
`scripts/pan325_load_loop.sh` repeats both groups while a build runs in a separate
target directory, so Cargo's target lock cannot serialize away the load.
=======
## Real-worker integration tests

CoordSim lives in `pancetta/tests/common/mod.rs` and drives the production TX
worker through the disabled-by-default `test-harness` feature. Run
`cargo test -p pancetta --features test-harness --test coord_sim --test tx_ptt_integration`;
the default workspace test command skips these targets. These tests use real
UTC slot scheduling and FT8 playback durations. The injected arm clock controls
authorization time only. See `pancetta/tests/common/README.md` for shutdown
ownership and the supersede missing-unkey mutation check.
>>>>>>> daa28a0a85c2283aebb025b210ebab9a1a16292a
