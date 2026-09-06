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
- **Amended by Codex round 2 on PR #350.** Three more, all on the same drain:
  - *Finding 1 — Hound picks stay in the Hound region.* Round 1's known-good re-anchor fixed
    `Revert`, but a `Switch` still resolved through the general 300–2800 Hz allocator, which
    knows nothing about the Hound calling/response regions. A post-QSY pick below
    `response_min_hz` puts our R+report where the Fox is not listening. The drain now takes one
    `get_qso` read per request and derives a hard Hz window from `metadata.hound` /
    `metadata.hound_qsyed` (the calling region before the QSY, the response region after), passed
    to the new `AutonomousOperator::allocate_smart_frequency_in_range`. That method filters the
    ranked candidates to the window alongside the existing hard exclusions, returns `avoid_hz`
    (the established "no valid relocation" signal) when the window empties the list, and clamps
    the window-blind legacy `allocate_cq_frequency` fallback into it. Bounds come from the live
    `QsoManagerConfig` (operator `[hound]` TOML); a non-finite or inverted window degrades to no
    constraint rather than panicking in `f64::clamp`.
  - *Finding 2 — validate revert targets against current occupancy.* With `max_concurrent_qsos
    > 1` another QSO can occupy the known-good offset after we switch away from it; the
    allocator-free `Revert` would commit anyway, placing two of our own streams inside
    `min_separation_hz` (the TX coalescer then excludes one from the bundle). The drain checks
    the target against `active_tx_offsets` — the QSO's own entry excluded — plus this batch's
    reservations, using the allocator's own `min_separation_hz` (exposed as
    `AutonomousOperator::min_own_separation_hz`), and on a hit resolves a fresh `Switch`
    avoiding both the stalled offset and the occupied target instead. Fails OPEN on a poisoned
    lock: this is placement quality, not a TX-safety gate.
  - *Finding 3 — drain on this slot's inputs.* The drain ran BEFORE `update_spectral`,
    `update_live_spots` and `feed_decoded_messages` consumed the just-finished slot, so every
    `Switch` ranked against the preceding slot's occupancy and the placement instrument computed
    later in the tick could disagree with the decision. It now runs immediately after those
    three — still ahead of `set_own_frequencies` (so the committed-offset mirror lands in time
    for the same tick's sync) and still ahead of the `auto_config_enabled` gate (so a
    manual-only operator's `u` nudge still drains).
- **Amended by Codex round 2 on PR #350 (finding 5) — muted cycles are not stalls.** Under
  `TxPolicy::Disabled` the coordinator's `tx_hard_mute_reason` blocks every frame
  `rearm_manual_calls_at` re-emits, but the keep-calling loop still re-armed each slot and counted
  each blocked re-send as a silent on-air cycle — four of them in Auto moved an established QSO
  off a known-good offset nothing had transmitted on. `QsoManager` now observes the global policy
  through a shared `Arc<AtomicU8>` (`set_tx_policy_source`, mirroring `set_tx_freq_mode_source`,
  wired in `start_qso_component`, defaulting to a private `Full` so existing callers are
  unchanged), and gates both the `stall_cycles` increment and the action emission on
  `allows_any_tx()`. Accumulated cycles are kept rather than reset — they came from real
  transmissions and remain valid evidence once TX resumes.
- **Amended by Codex round 3 on PR #350.** Two findings: the hard exclusions (`avoid_hz` plus
  the batch reservations) bound only inside `allocate_smart_frequency_in_range`'s ranked
  spectral branch, so the deterministic no-snapshot fallback — every resolution until the first
  waterfall lands — could still hand two same-batch `Switch` actions the identical offset
  (`FrequencyAllocator::allocate_cq_frequency_excluding` now enforces the same exclusions and
  window there); and the allocator's "no valid relocation exists" signal (`avoid_hz` returned
  unchanged) was committed as a real move, clearing accumulated `stall_cycles` evidence and
  announcing a `TxOffsetApplied` for a relocation that never happened
  (`apply_tx_offset_switch` refuses it as `OffsetActionNoOp`; the drain reserves the unmoved
  offset for the rest of the batch and does not requeue — PAN-79).
- **Amended by Codex round 4 on PR #350.** Three more:
  - *Finding 1 — other live QSOs are hard exclusions, not a scoring penalty.* `reserved_hz`
    only ever held offsets committed earlier in the SAME batch, so the common crowded-spectrum
    shape (several live QSOs, exactly one switching) left the rest out of the exclusion set
    entirely — they reached the allocator only as `score_candidate`'s soft `-50`
    `own_frequencies` penalty, whose own comment concedes it "effectively eliminates" rather
    than eliminates, and which at drain time is applied against a one-tick-stale occupancy view
    (`set_own_frequencies` syncs later in the tick). The drain's new `other_live_tx_offsets`
    folds the whole `active_tx_offsets` view — minus the QSO being resolved — into the same
    hard `also_avoid_hz` channel, on both the `Switch` path and the `Revert` re-resolution
    fallback. Fails open on a poisoned lock, matching `revert_target_is_taken`.
  - *Finding 2 — revalidate the Hound region at commit time.* Round 2's window was a `get_qso`
    snapshot taken BEFORE the allocator ran and handed to an awaited
    `apply_tx_offset_switch`; the Fox's first report performs the mandatory
    calling-region → response-region QSY, and an operator-forced `u` nudge deliberately carries
    no `raised_at_generation`, so the advance guard could not catch a QSY landing in that
    window — the resolved low offset would commit and undo the QSY. `hound_switch_range_hz`
    moves into `pancetta-qso` so resolve and commit share one definition (and it gains the
    inverted/non-finite sanitization the allocator already applied), and
    `apply_tx_offset_switch` re-derives it from the locked `progress` it is about to mutate,
    refusing an out-of-region offset with the new expected-refusal
    `OffsetActionOutsideHoundRegion`. Refusing rather than re-clamping keeps the stall evidence
    so the detector re-raises against the correct region.
  - *Finding 3 — the vacated offset stays valid while its reply is in flight.* A relocation is
    committed AFTER the frame that triggered it has gone out on the old offset:
    `rearm_manual_calls_at` re-sends at `metadata.frequency` and trips the threshold in the same
    pass, and the drain commits on its next tick. An unanswered manual CQ then REJECTED the
    caller's answer — `CallingCq` carries the new offset, round 1's fix deliberately leaves
    `partner_freq` `None` pre-establishment, and the gate is only 15 Hz wide — and
    `maybe_answer_caller` would spawn a duplicate QSO in its place; an established QSO's reply
    routed but credited `last_known_good_offset_hz` to an offset never transmitted on, poisoning
    the Revert target. Fixed on the RECEIVING side rather than by resequencing the trigger: the
    last pre-switch frame is already in flight when the move commits, so delaying either the
    emission or the commit by a cycle only moves the same window. New
    `QsoMetadata::pre_switch_offset` records (vacated offset, when we left it); for
    `PRE_SWITCH_OFFSET_GRACE` (two FT8 slots — long enough for that reply's round trip, short
    enough to lapse before a reply to our first POST-switch frame could arrive) it is a second
    accepted RX baseline in `is_message_relevant`/`classify_relevance`, at the same tolerance
    and on top of (never instead of) the current one, and it is the offset a forward advance
    credits as known-good. Consumed on the first advance after a switch.
- **Amended by Codex round 5 on PR #350.** Two refinements to round 4's `pre_switch_offset`
  mechanism, both about a relocation that is NOT a stall-triggered one:
  - *Finding 1 — a deferred frame pivots when only its offset changed.*
    `coordinator::tx_pivot_target` — the "freshest `MessageToSend` at key-time" check the TX
    worker's Step 4c and `pivot_bundle_items` both run — compared only `message_text`, so a
    frame that advanced by MOVING rather than by re-rendering was invisible to it. A switch
    relocates the QSO without changing what the frame says, so the next rearm publishes the same
    text at the new `applied_hz`; an older request for that QSO still in the worker's (up to
    ~30 s) pre-PTT wait therefore got `None` back and keyed on the offset the switch existed to
    leave. Round 4's receive-side grace does not help here — it makes a reply to the stale
    offset ROUTE, it does not make the transmitted frame fresh — and the exposure outlives the
    triggering-resend window, since any later post-switch rearm hits it once a new-frequency
    intent exists. `frequency_offset` now joins `message_text` in the comparison (matching
    `is_pivot_duplicate`'s tombstone identity and `classify_incoming_during_tx`'s duplicate
    check, PAN-73 rounds 3 and 6), with a `PIVOT_OFFSET_EPSILON_HZ` (0.5 Hz) guard so f64
    round-tripping cannot report an unchanged re-send as a pivot.
  - *Finding 2 — credit the offset the advancing reply actually decoded at.* Round 4's crediting
    rule checked only the clock, so any advance inside `PRE_SWITCH_OFFSET_GRACE` recorded the
    VACATED offset. Sound for a stall-triggered switch (its trigger IS the old-offset resend),
    but an operator-forced `u` nudge has no such coupling: it can land on a QSO about to
    transmit, the next rearm goes out on the NEW offset, and the answer to that lands well
    inside the same 30 s window — crediting an offset that had just demonstrably failed, and
    aiming a later `Revert` straight back at it. The reply's decode frequency is the evidence
    that separates the two: the vacated offset is credited only when `message.frequency`
    matches that baseline under the SAME tolerance the relevance gate admitted the frame with
    (captured from the PRE-transition state so a callsign latched by this very advance cannot
    widen it retroactively); anything else — the new offset, or a `partner_freq` split matching
    neither — credits the current offset. Overlapping baselines (a move smaller than the
    tolerance) still resolve to the vacated one, which has the proven history. The grace is
    consumed either way, as before.
- **Amended by Codex round 6 on PR #350.** Four findings, all fixed inline:
  - *Finding 1 — the report sub-rung is forward progress.* `progress_rank` ranked both
    `SendingReport { their_report: None }` and `{ Some(_) }` at rung 1, so the DX's first
    `SignalReport` — the advance that turns our outbound from a plain report into an R-report —
    left `advance_generation` unmoved. A switch queued by the preceding threshold-hitting rearm
    then passed the commit-time stale-action guard and relocated the QSO at the exact moment the
    DX answered; below the threshold, the partial stall streak carried forward instead of
    resetting. The rung is split (and `Contest(ExchangingInfo)` gets the analogous `their_serial`
    split so the dormant contest wiring cannot inherit the bug), with the rungs above renumbered.
    `ladder_rank` is deliberately untouched — the manual regression predicate depends on both
    `SendingReport` shapes comparing equal there — and a repeated report (`Some -> Some`, the
    "the DX never copied our R" arm) still correctly reads as no progress.
  - *Finding 2 — credit the NEW offset when an operator nudge moved a pre-existing split.*
    Round 5's decode-frequency check is evidence only on a Tx=Rx QSO. On a QSO that already
    carried a `partner_freq` the DX transmits there no matter where we key, so its reply cannot
    discriminate our vacated offset from our new one, and the round-5 correction therefore fell
    back to crediting the vacated offset unconditionally. That is right for a STALL-triggered
    switch — it only fires because the old offset demonstrably stopped working, and the frame
    that triggered it went out on that offset a slot before the commit — and wrong for an
    OPERATOR-forced `u` nudge, which indicts nothing and after which our very next frame goes
    out on the new offset. Provenance is now carried rather than inferred:
    `OffsetRelocationOrigin` (`StallDetected { raised_at_generation }` | `OperatorForced`)
    replaces the bare `Option<u32>` on `OffsetActionRequest` and in `apply_tx_offset_switch`'s
    signature, and `QsoMetadata::pre_switch_offset` becomes a `PreSwitchOffset { offset_hz,
    left_at, operator_forced }`. The Tx=Rx path is unchanged.
  - *Finding 3 — generate candidates across the configured Hound window.* A `[hound]` response
    region above the allocator's own 200–2800 Hz range (2850–2900 Hz, say) was only ever a
    FILTER over candidates already generated from that range, so it emptied both the ranked
    spectral list and the legacy fallback scan: the allocator returned `avoid_hz` unchanged and
    every post-QSY Hound stall switch or `u` nudge was refused as a no-op, with the configured
    region never searched. Both scans now take their bounds from one shared
    `frequency::sweep_bounds`, which WIDENS the sweep to cover the supplied window and never
    narrows it, with the grid still anchored at the configured floor — so every offset the old
    sweep produced is still produced and an in-range window ranks exactly as before. Ceiling
    reconciled at the same time: `[hound]` validation capped at 3000 Hz, admitting a
    (2900, 3000] region that no relocation could ever land in (`apply_tx_offset_switch` clamps
    to `ACTIVE_QSO_TX_OFFSET_MAX_HZ` = 2900 and its region guard then refuses its own clamped
    value) and that `hound_offset_for`'s QSY would write into `metadata.frequency` with no clamp
    at all. The bound is now 200–2900 Hz, the real transmittable envelope.
  - *Finding 4 — score a switch against the QSO's own TX parity.* The drain passed `None` for
    `target_parity`, so `rank_candidates_with_parity` took its slot-blind scoring path even
    though the QSO's `metadata.tx_parity` is latched at creation and every frame it emits goes
    out on it. Slot-blind, a frequency quiet only in the OPPOSITE (listening) slot can outrank
    one genuinely clear in the slot we will actually key in, so adaptive recovery could move a
    colliding QSO straight into another collision. The `get_qso` snapshot the Hound-region and
    revert-occupancy checks already take carries the authoritative parity; it now feeds both the
    ordinary `Switch` resolution and the occupied-target `Revert` fallback. Parity only weights
    scoring — it never filters a candidate out — so the search can never fail more often, and a
    QSO with no latched parity degrades to exactly the previous behaviour.

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
  - **Amended by Codex round 2 on PR #350 (finding 4):** `active_tx_qsos` deliberately retains a
    completed QSO for 45 seconds so its trailing 73 can transmit, so the blind first-entry pick
    could name a terminal QSO — or shadow a genuinely live concurrent one — and report "Nudging
    active QSO" for a request the drain then refused with `QsoNotActive`. `resolve_nudge_tx_offset`
    now takes the engine's authoritative live-QSO id set (read at keypress time from the same
    restart-safe `qso_manager_watch` handle the drain re-borrows each tick — read-only; commits
    still happen only in the drain) and skips any snapshot entry missing from it, so a
    grace-window-only entry falls through to the CQ fallback. `None` (Qso component not up)
    preserves the pre-filter behavior. This is a SELECTION-time filter complementing round 1's
    commit-time refusal: the commit check keeps the engine correct, this one keeps the status
    line honest.
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
