# PAN-72: Adaptive TX-offset switching on stall + manual re-pick keystroke — design

**Ticket:** PAN-72 — autonomous CQ hunting and mid-QSO TX currently keep hammering the same
audio offset even when nothing is getting through. Two asks from Tony:

1. In Auto TX-freq mode, after a configurable number of cycles with no genuinely new response
   from the DX (silence *or* a repeated/non-advancing frame), move to another offset. If the
   QSO progresses again on the new offset but then stalls a *second* time, revert to the offset
   that was last known to work, rather than searching again.
2. A manual "try something else" keystroke that works even while the station is running fully
   autonomously — nudges whatever is currently active (an in-progress QSO, or the CQ-hunting
   offset) to a new slot immediately, without waiting for the threshold and without leaving Auto
   mode.

Approved via a brainstorming session 2026-09-04; the session escalated this from a bounded
change to architectural after two rounds of clarifying questions surfaced real cross-module
plumbing gaps (see "Approaches considered" below).

## Existing mechanisms this extends

- **CQ hunting** (`pancetta-qso/src/autonomous.rs`) already tracks `cq_no_response_streak` and
  switches the self-CQ offset via the smart allocator (`allocate_smart_frequency`, `avoid` = the
  current CQ offset) once `AutonomousConfig::cq_no_response_switch_after` (default 5) is hit.
  This ticket only lowers that default to 4 — no other change needed here. CQ hunting gets no
  "revert to known-good" concept: there's no confirmed-contact signal to anchor a "known good"
  offset on before a QSO exists.
- **Mid-QSO** (`pancetta-qso/src/qso_manager.rs`) only reacts to a DX *repeating the exact same
  frame* (`QsoMetadata::dx_repeat_count`, `DX_STUCK_REPEAT_THRESHOLD = 4`), hopping the offset by
  a fixed `+300Hz` (`stuck_hopped_offset`) with no spectral awareness — deliberately, per its own
  doc comment, because `qso_manager.rs` has no access to the smart allocator or spectral state.
  Total silence (the DX sending nothing at all after we transmit) is never counted today. This
  ticket generalizes that detection and switches its offset-picking to the smart allocator.

## Architecture

`SmartFrequencyAllocator`/spectral snapshot/decode history live *only* inside
`AutonomousOperator` (`pancetta-qso/src/autonomous.rs`) — this is why the existing mid-QSO hop is
dumb, and per AGENTS.md's single-scorer invariant, `QsoManager` must not gain its own allocator
instance. `AutonomousOperator` itself is a pure decision engine owned by a single supervised
tokio task (`pancetta/src/coordinator/autonomous.rs`), ticking once per 15s FT8 slot; it is
entirely **push-fed** by coordinator-computed snapshots (`active_tx_qsos`, `active_tx_offsets`)
and never reaches into `QsoManager` directly.

The established pattern in this codebase for "an external task needs the Autonomous task to do
one specific thing once" is a `Mutex<Vec<_>>` mailbox, pushed to by the producer and drained via
`std::mem::take` once per tick by the Autonomous task's own loop — already used for
`pending_autonomous_cq_dispatch_failures`. This design reuses that exact shape rather than
inventing a new concurrency primitive.

```
QsoManager (detects stall, owns metadata)
   │  emits QsoEvent::TxOffsetActionNeeded{qso_id, action}
   ▼
coordinator/qso.rs event-forwarder (existing task)
   │  pushes onto pending_qso_offset_requests: Arc<Mutex<Vec<_>>>
   ▼
coordinator/autonomous.rs tick loop (existing task, once per 15s slot)
   │  drains mailbox; Switch → op.allocate_smart_frequency(avoid_hz)
   │                    Revert → target_hz directly (no allocator call)
   ▼
QsoManager::apply_tx_offset_switch(qso_id, resolved_hz)   (new, sole external mutator)
```

`QsoManager` remains the sole owner/mutator of `QsoMetadata` and the sole detector of "does this
QSO need an offset action" (pure bookkeeping — counting cycles, comparing to a threshold, no
scoring). `AutonomousOperator` remains the sole caller of the allocator (single-scorer intact).
The coordinator only wires the two together using patterns that already exist and are already
tested.

### Approaches considered

1. **(Recommended — above.)** Event-emitted from `QsoManager`, coordinator-mediated mailbox,
   commit via a new `QsoManager` method. Reuses two existing, proven shapes
   (`pending_autonomous_cq_dispatch_failures`'s mailbox, `active_tx_qsos`/`active_tx_offsets`'s
   snapshot-push). No new concurrency primitive; single-scorer preserved.
2. **Give `QsoManager` its own `SmartFrequencyAllocator` handle**, avoiding the event/mailbox
   indirection. Rejected: creates a second scoring path, directly violating the single-scorer
   invariant in AGENTS.md ("the TX-placement display and autonomous decisions share one allocator
   path").
3. **Let the Autonomous task pull QSO state directly** (hold an `Arc<QsoManager>`, poll it each
   tick) instead of reacting to an emitted event. Rejected: no existing code pulls from
   `QsoManager` this way — every existing channel into the Autonomous task is a coordinator-
   computed *push* (snapshots or one-shot mailboxes). Introducing a pull-style dependency here
   would be a new, asymmetric coupling for no benefit over the mailbox approach.

## Data & state — `pancetta-qso`

`pancetta-qso/src/states.rs`, `QsoMetadata`:

- Replace `dx_repeat_count: u32` with `stall_cycles: u32` — increments on *either* silence (we
  transmitted this slot per `rearm_manual_calls_at` and the DX has not advanced) *or* a
  repeated/non-advancing frame; resets to 0 on any forward state advance. `last_rx_text` is kept
  (still needed to detect "identical repeat" vs. "different non-advancing frame", which resets
  the old counter to 1 rather than 0 — that nuance carries over unchanged, just feeding the
  renamed/broadened counter).
- Add `last_known_good_offset_hz: Option<f64>` — set to `metadata.frequency` every time
  `stall_cycles` resets to 0 via a genuine forward advance (not merely because we just applied a
  switch/revert). `None` until the first advance.

`pancetta-qso/src/qso_manager.rs`:

- Silence detection integrates into the existing `rearm_manual_calls_at` per-slot re-send
  check (the natural "we just transmitted again because the DX still hasn't advanced"
  checkpoint) rather than adding a second timer or per-cycle scan.
- Remove `DX_STUCK_REPEAT_THRESHOLD`, `STUCK_TX_HOP_HZ`, and `stuck_hopped_offset` — the inline
  hop in `process_message_for_qso` (currently ~qso_manager.rs:2984-3033) is replaced by the
  generalized path below. One mechanism, not two.
- New `QsoEvent::TxOffsetActionNeeded { qso_id: QsoId, action: OffsetAction }` where:
  ```rust
  pub enum OffsetAction {
      /// We're on the known-good offset (or none is recorded yet) — find a new one.
      Switch { avoid_hz: f64 },
      /// We're stuck again on a previously-switched offset — go back to what worked.
      Revert { target_hz: f64 },
  }
  ```
  Emitted once `stall_cycles >= config.qso_stall_switch_after` (new field, default 4), gated to
  `tx_freq_mode == Auto` exactly like today's hop gate. Emitting resets `stall_cycles` to 0
  immediately so the same stall doesn't re-fire every subsequent tick.
- Decision rule at emission time: if `last_known_good_offset_hz` is `None`, or
  `(metadata.frequency - last_known_good_offset_hz.unwrap()).abs() < f64::EPSILON` (float
  equality via epsilon, matching the existing style in `autonomous.rs`'s own switch-result
  guard) → `Switch{avoid_hz: metadata.frequency}`; else → `Revert{target_hz:
  last_known_good_offset_hz.unwrap()}`. This is the ping-pong: switch away from known-good on
  first stall, revert to known-good on a second stall while on the switched offset, switch away
  again if it stalls a third time, and so on.
- New `pub async fn apply_tx_offset_switch(&self, qso_id: QsoId, new_offset_hz: f64) ->
  Result<(), QsoManagerError>` — the one external mutation entry point. Sets
  `metadata.frequency`, clears `pending_freq_drift`, resets `stall_cycles` to 0, logs at
  `target: "tx.freq"` (mirrors the removed inline hop's own logging). Does **not** force an
  immediate retransmission — the next naturally-scheduled send (the next `rearm_manual_calls_at`
  resend, or whatever event next constructs a message for this QSO) picks up the new value
  because message construction reads `metadata.frequency` fresh each time, same as today.

## Coordinator wiring — `pancetta` binary

`pancetta/src/coordinator/mod.rs`:

- New field `pending_qso_offset_requests: Arc<std::sync::Mutex<Vec<(QsoId, OffsetAction)>>>`,
  same shape as `pending_autonomous_cq_dispatch_failures`.

`pancetta/src/coordinator/qso.rs`:

- The existing QSO event-forwarding task (already the source of `active_tx_qsos`/
  `active_tx_offsets`) pushes onto the new field on `QsoEvent::TxOffsetActionNeeded`.

`pancetta/src/coordinator/autonomous.rs`:

- `allocate_smart_frequency` gains a `pub` visibility bump (currently private on
  `AutonomousOperator`) so the coordinator — a separate crate (`pancetta`), for which
  `pub(crate)` would not suffice — can call it. No signature change.
- New drain step in the existing tick arm, mirroring
  `drain_pending_autonomous_cq_dispatch_failures`: for each queued `(qso_id, action)`, resolve
  `Switch{avoid_hz}` via `op.allocate_smart_frequency(None, None, Some(avoid_hz))`, or use
  `target_hz` directly for `Revert`; either way, call
  `qso_manager.apply_tx_offset_switch(qso_id, resolved_hz).await`. Runs unconditionally
  (independent of `autonomous_enabled_runtime`/`tx_policy` — a metadata write is harmless even if
  nothing will transmit on it; actual TX gating still happens downstream at dispatch time as
  today).
- This requires the Autonomous task to hold a callable handle to `QsoManager` for the commit
  call — confirm/wire the same handle the coordinator already uses elsewhere (`send_message`
  etc.) is reachable from this task; if it isn't already captured in the spawned closure, capture
  a clone at spawn time the same way `active_tx_qsos`/`tx_policy`/etc. are captured today.

## Manual "nudge" keystroke — `pancetta-tui`

- New key: **`u`** ("un-stick"; confirmed unused against the full existing keymap). Sends a new
  `TuiCommand::NudgeTxOffset` (no fields — unlike `t`/`a`, the target isn't known client-side, so
  there's no local optimistic state to flip).
- `tui_relay.rs` handling:
  - If the coordinator's `active_tx_qsos` snapshot is non-empty, push a forced
    `(qso_id, OffsetAction::Switch{avoid_hz: current_offset})` onto
    `pending_qso_offset_requests` for that QSO — same mailbox, same drain path, threshold check
    bypassed since this is operator-forced.
  - Else, if the Autonomous task's current state is `CallingCq` (hunting), trigger today's
    CQ-switch immediately: a new `pending_cq_offset_nudge: Arc<AtomicBool>` flag, set here and
    checked by the tick loop alongside its existing `should_switch` computation — same
    `allocate_smart_frequency(avoid=current_cq_offset_hz)` call the periodic CQ-switch already
    makes, just triggered on demand instead of by streak.
  - Else (not hunting, no active QSO): `TuiMessage::StatusUpdate` "nothing to nudge — no active
    QSO or CQ".
- Does **not** touch `tx_freq_mode` or `tx_offset_hold_hz` (stays in Auto, per Tony's answer: "stay
  in auto, but make the tx window sticky for the duration of the current QSO... reevaluate
  openness for the next QSO in an automatic fashion") and does **not** touch the auto-repark
  mechanism (`should_repark` in `coordinator/autonomous.rs`) — different atomic
  (`tx_offset_hold_hz`, the CQ *park* offset), different purpose, its own "never repark while any
  QSO is active" safety rule is unrelated to changing an already-active QSO's own frequency.

## Config — `pancetta-config`

Two independent `AutonomousConfig` types exist (`pancetta-config/src/autonomous.rs`, the
TOML/hot-reload-facing type, and `pancetta-qso/src/autonomous.rs`, the runtime type
`AutonomousOperator` holds) — both need the `cq_no_response_switch_after` default change, kept in
sync by whatever code already maps one to the other:

- `cq_no_response_switch_after` default lowered 5 → 4 in both crates' defaults (and the test
  asserting the old default, `autonomous_config_default_cq_no_response_switch_after_is_5`, gets
  updated to assert 4).

`qso_stall_switch_after` is a **different** case: the threshold check for it lives in
`pancetta-qso/src/qso_manager.rs`, which reads `pancetta_qso::QsoManagerConfig` — a completely
separate Rust type from `AutonomousConfig` that `QsoManager` has no visibility into. Its natural
new home is `QsoManagerConfig::TimeoutConfig` (sibling to `manual_call_max_calls`,
`repetitive_tx_timeout_secs` — qso_manager.rs:345-384), **not** either `AutonomousConfig` type.
`TimeoutConfig`'s fields currently have no TOML surface at all — the coordinator constructs
`QsoManagerConfig` with `..Default::default()` for `timeouts`
(`pancetta/src/coordinator/qso.rs:2420-2440`) — so this needs new plumbing, not just a new field:

- New field `qso_stall_switch_after: u32` on `pancetta_qso::qso_manager::TimeoutConfig`, default
  4, `#[serde(default = "default_qso_stall_switch_after")]` + companion free fn (mirrors
  `repetitive_tx_timeout_secs`/`default_repetitive_tx_timeout_secs` immediately above it).
- The operator-facing TOML knob stays under the existing `[autonomous]` section in
  `pancetta-config/src/autonomous.rs` (same section as `cq_no_response_switch_after` — one
  logical "adaptive TX behavior" section from the operator's point of view, even though the two
  numbers land in different Rust types internally): new
  `AutonomousConfig::qso_stall_switch_after: u32`, default 4, same
  `#[serde(default = "...")]` + companion free fn + `merge_with` line + `validate_section` `== 0`
  rejection + old-TOML-compat test pattern as `cq_no_response_switch_after`.
- The coordinator threads it across the crate boundary at the `QsoManagerConfig` construction
  site (`pancetta/src/coordinator/qso.rs:2417-2441`), the same way `hound_cfg`/`dup_cfg`/
  `active_mode` locals already get threaded from disparate config sections into
  `QsoManagerConfig` fields there: pull `self.config.autonomous.qso_stall_switch_after` into a
  local, and set it explicitly on the `timeouts: TimeoutConfig { .. }` given to
  `QsoManagerConfig` (which today relies on `..Default::default()` for the whole `timeouts`
  field — this becomes the first explicitly-threaded `TimeoutConfig` field).
- `docs/CONFIG.md` documents both `[autonomous]` fields.

## Explicitly out of scope / invariants preserved

- **Auto-initiated QSOs** (`CallInitiation::Auto` — the autonomous operator's own CQ-answers/
  pounces) are bounded to 2 total calls / 30s in `RespondingToCq`/`SendingReport`
  (`report_timeout=30s`, `AUTO_RESEND_MAX_CALLS=2`) — a deliberately tight, heavily-tested safety
  bound (SM-F6) that multiple tests assert stays unmodified. This mechanism is written generally
  (applies to Manual and Auto QSOs alike, same as the mechanism it replaces) but by construction
  will essentially never accumulate `qso_stall_switch_after` (4) stalls before that existing
  watchdog retires an Auto QSO first — dormant there, per Tony's explicit call. `report_timeout`,
  `AUTO_RESEND_MAX_CALLS`, and `repetitive_tx_timeout_secs` are untouched.
- **CQ-hunting** gets no known-good/revert concept — switch-only, per Tony's answer (no
  confirmed-contact signal to anchor a "known good" CQ offset on).
- **Auto-repark** (`should_repark`, `coordinator/autonomous.rs`) is untouched — separate
  mechanism, separate atomic, separate purpose (see "Manual nudge" section above).
- An Autonomous-task crash/restart rebuilds `AutonomousOperator` from scratch (loses
  `cq_no_response_streak`, etc. — pre-existing behavior, unaffected by this change);
  `pending_qso_offset_requests` entries queued before a restart are still drained correctly since
  the mailbox lives on `ApplicationCoordinator`, not inside the operator.

## Testing

- `pancetta-qso` (`qso_manager.rs`): `stall_cycles` increments via silence (through
  `rearm_manual_calls_at`) and via a repeated frame; resets on advance; `TxOffsetActionNeeded`
  emission at threshold, Auto-mode gating; the Switch→Revert→Switch ping-pong against
  `last_known_good_offset_hz`; `apply_tx_offset_switch` mutation correctness.
- `pancetta-config`: new-field defaults, the type-driven "merge carries all fields" guardrail
  (extends automatically), an old-TOML-compat test for the new field, updated default-value
  assertion for the lowered CQ threshold.
- `pancetta` coordinator: a test exercising the drain-and-commit path
  (`pending_qso_offset_requests` → `apply_tx_offset_switch`), following whatever granularity the
  existing coordinator test harness supports (integration-style if that's the existing
  convention — don't invent new harness machinery for this alone).
- `pancetta-tui`: a `tui_relay.rs` dispatch test for `NudgeTxOffset`, mirroring the existing
  `SetTxOffset`/`ToggleAutonomous` dispatch tests.
- No on-air/loopback coverage is possible (needs a genuinely silent or slow-to-respond second
  station) — flagged for an on-air sanity check after merge, consistent with recent PAN tickets.
