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
`pending_autonomous_cq_dispatch_failures`. This design reuses that exact shape for the request
queue itself.

> **Amended during implementation (Task 9 review) — one deviation from the design as written
> below.** The original design had the Autonomous task capture a `QsoManager` clone **once at
> spawn time**, alongside `active_tx_qsos`/`tx_policy`/etc., to make the commit call. That is a
> real bug: the Autonomous task is *not* respawned when only the Qso component restarts, so after
> a supervised Qso restart the captured clone points at the dead manager's QSO map and every
> commit would silently apply to an abandoned instance. The fix adds one genuinely new primitive —
> `ApplicationCoordinator::qso_manager_watch: tokio::sync::watch::Sender<Option<QsoManager>>`.
> `start_qso_component` publishes each fresh handle with **`send_replace`** (never `send`: tokio's
> `send` discards the value when `receiver_count() == 0`, and the Qso component starts *before*
> the Autonomous one, so a plain `send` would drop the very first handle and leave the feature
> inert until the first crash); the Autonomous task `.subscribe()`s once at spawn and re-`borrow`s
> fresh **every tick**. `health.rs`'s supervisor needs none of this — it runs inline with
> `&mut self` in the un-spawned coordinator loop, where `self.qso_manager_for_supervisor` is
> always already fresh. So the "no new concurrency primitive" claim below holds for the mailbox,
> but not for the handle: restart-safety required one. Everything else in this section is as
> built.

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
- New `QsoEvent::TxOffsetActionNeeded { qso_id: QsoId, action: OffsetAction,
  raised_at_generation: u32 }` (the generation field added by Codex round 1, finding 8;
  the coordinator's mailbox element is the matching `OffsetActionRequest` struct) where:
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
- New `pub async fn apply_tx_offset_switch(&self, qso_id: QsoId, new_offset_hz: f64,
  raised_at_generation: Option<u32>) -> Result<f64, QsoManagerError>` — the one external
  mutation entry point. **Amended by Codex round 1 on PR #350 (findings 3 and 8):** two
  staleness guards run before ANY field is touched, because the coordinator drains its
  mailbox only once per 15-second slot. (a) A terminal QSO is refused with
  `QsoManagerError::QsoNotActive` — completed entries stay in the map and stay in the
  coordinator's active snapshots for the 45s trailing-73 grace window, so a late action
  still *finds* its QSO; `QsoState::set_frequency` already refused terminal states but
  `metadata.frequency` did not, so the write went through and the drain mirrored a phantom
  offset into `active_tx_offsets`. (b) `raised_at_generation` is the QSO's new
  `QsoMetadata::advance_generation` (bumped in lockstep with the `stall_cycles` reset and
  the `last_known_good_offset_hz` record) at the moment the action was raised; if it has
  moved since, the DX answered in between and the request is refused with
  `QsoManagerError::OffsetActionStale` rather than dragging the QSO off the offset that just
  worked. `None` means operator-forced (`u`), which is current by construction. Both
  refusals answer `true` to `QsoManagerError::is_expected_offset_action_refusal` and the
  drain logs them at `debug!`, not `warn!`. On success it also emits the new
  `QsoEvent::TxOffsetApplied` (finding 7) so the coordinator can rebuild the
  `ActiveQsosSnapshot`. Sets
  `metadata.frequency` **and the QSO state's own embedded frequency** (via
  `QsoState::set_frequency`, mirroring the Hound QSY block: `Completed` is built from the
  preceding state's `frequency`, so metadata alone would log the pre-switch offset), clamps
  defensively to `ACTIVE_QSO_TX_OFFSET_MIN_HZ..=ACTIVE_QSO_TX_OFFSET_MAX_HZ` (200–2900 — NOT
  the removed hop's 300–2700 autonomous-*pick* band, which would narrow a `Revert` back to an
  unclamped reply offset the QSO actually worked on), clears `pending_freq_drift`,
  resets `stall_cycles` to 0, and logs at `target: "tx.freq"` — at `info!`, since the
  silence-based detector fires far more often than the identical-repeat hop it replaced. Returns
  the applied (post-clamp) offset so the coordinator's `active_tx_offsets` mirror stores what
  actually landed. Does **not** force an
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
  `drain_pending_autonomous_cq_dispatch_failures`: for each queued `OffsetActionRequest`, resolve
  `Switch{avoid_hz}` via `op.allocate_smart_frequency_avoiding(None, None, Some(avoid_hz),
  &reserved_hz)`, or use `target_hz` directly for `Revert`; either way, call
  `qso_manager.apply_tx_offset_switch(qso_id, resolved_hz).await`. Runs unconditionally
  (independent of `autonomous_enabled_runtime`/`tx_policy` — a metadata write is harmless even if
  nothing will transmit on it; actual TX gating still happens downstream at dispatch time as
  today).
- This requires the Autonomous task to hold a callable handle to `QsoManager` for the commit
  call. ~~Capture a clone at spawn time the same way `active_tx_qsos`/`tx_policy`/etc. are
  captured today.~~ **As built:** a spawn-time capture is unsafe across a Qso-component restart
  (the Autonomous task is not respawned with it), so the handle arrives over
  `qso_manager_watch` — see the amendment note in "Architecture" above. The drain re-borrows it
  fresh each tick; `None` means the Qso component isn't up yet, which also means nothing could
  have been queued.
- The drain also mirrors the applied offset into `active_tx_offsets` (the coordinator's
  own-frequency snapshot). `apply_tx_offset_switch` emits no `QsoEvent`, and that map is otherwise
  refreshed only on a `StateChanged` carrying a frequency, so without the mirror the allocator's
  self-avoidance — and the manual nudge's `avoid_hz`, read from the same map — would keep seeing
  the pre-switch offset until the QSO's next state transition.
- **Amended by Codex round 1 on PR #350.** Three further guards on the drain:
  - *Finding 6 — re-read `tx_freq_mode` at commit time.* An action queued under Auto can be
    drained after the operator presses `f` for Hold. A `Switch` would then resolve through
    `allocate_smart_frequency`'s Hold early return (which ignores `avoid_hz` and hands back
    the *parked* offset, dragging a held QSO onto it) and a `Revert` never consults the
    allocator at all — both violate Hold's stickiness. The whole batch is discarded once the
    mode reads Hold. The TUI's `u` handler applies the same gate at *enqueue* time; both ends
    are needed, because the mode can flip in between.
  - *Finding 1 — reserve each committed offset for the rest of the batch.* The operator's
    own-frequency snapshot is synced from `active_tx_offsets` only LATER in the same tick
    (`set_own_frequencies`), so every `Switch` in one batch would otherwise rank against the
    identical stale view, each excluding only its own original offset — two concurrent QSOs
    could land on the same "best" candidate. The drain accumulates every offset it actually
    commits (`Revert` targets included) and passes them to
    `allocate_smart_frequency_avoiding` as extra HARD exclusions, matching how `avoid_hz`
    itself was promoted from the soft `own_frequencies` penalty in Codex round 6 on PR #276.
    Bounded today by `max_concurrent_qsos` (default 1), but real the moment that is raised.
  - *Finding 7 — refresh the UI snapshot.* `apply_tx_offset_switch` now emits
    `QsoEvent::TxOffsetApplied`; the QSO event-forwarder's new arm calls the extracted
    `push_active_qso_snapshot` helper (shared with the pre-existing `StateChanged`/
    `QsoCompleted`/`QsoFailed` push sites). Without it the banner and the TX-placement
    stream marker keep the pre-switch offset through multiple switch/revert cycles — a
    stalled exchange is precisely where no further state transition arrives.

## Manual "nudge" keystroke — `pancetta-tui`

- New key: **`u`** ("un-stick"; confirmed unused against the full existing keymap). Sends a new
  `TuiCommand::NudgeTxOffset` (no fields — unlike `t`/`a`, the target isn't known client-side, so
  there's no local optimistic state to flip).
- `tui_relay.rs` handling:
  - If the coordinator's `active_tx_qsos` snapshot is non-empty, push a forced
    `(qso_id, OffsetAction::Switch{avoid_hz: current_offset})` onto
    `pending_qso_offset_requests` for that QSO — same mailbox, same drain path, threshold check
    bypassed since this is operator-forced. **As built:** gated on
    `TxFreqMode::allows_auto_change()` exactly like the CQ branch below. In Hold the allocator's
    early return ignores `avoid_hz` and yields the parked offset, so a queued Switch would either
    no-op or drag a held QSO onto the park offset while the status line claimed a nudge; instead
    the operator gets "TX offset is Hold — press `f` for Auto to enable nudging" and neither the
    mailbox nor the CQ flag is touched.
  - **Amended by Codex round 1 on PR #350 (finding 4):** the Hold gate is hoisted ABOVE the
    active-QSO branch, so a Hold-mode `u` with NO active QSO also reports `HeldNoOp` instead
    of a phantom CQ nudge (`decide_at` computes `should_switch = self.tx_freq_auto() && ...`,
    so a flag armed in Hold is consumed and discarded). The finding's second half — "in Auto
    but not currently CQ-hunting the request is also discarded" — is deliberately NOT gated:
    `AutonomousOperator::state` is private to the autonomous task with no shared snapshot or
    atomic carrying it, and any value the relay could read at keypress time would be a
    prediction anyway (`decide_at` re-evaluates `idle_cycles >= cq_after_idle_cycles` for
    that cycle). The outcome is instead named `CqNudgeArmed` and the status line says
    "CQ-offset nudge armed — applies on the next CQ cycle". Finding 9 additionally stops such
    a request being lost when that cycle arrives with thin decode history.
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
