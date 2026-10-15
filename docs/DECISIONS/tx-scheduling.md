<!-- Covers: TX scheduling. Content moved from CLAUDE.md 2026-07-07.
     For current behavior trust the code and specs in `docs/superpowers/specs/`. -->

# TX scheduling

## DX-slot-aware TX scheduling

`pancetta/src/coordinator/tx.rs`: WSJT-X-style. Every `DecodedMessage` carries `slot_parity`; the QSO state machine latches `tx_parity = opposite_of(dx_parity)` at QSO start; the TX scheduler picks the next slot of that parity, padding silent samples if early or skip-ahead-cursoring into the modulated waveform if late (up to `tx_late_max_ms`, default 8s). Past that, defers 30s. Never collides with the DX's parity. See `docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md` — its "the first received decode will refine the parity for any subsequent QSO responses" promise is now implemented (2026-07-18 review, Batch 2): a QSO opened with no observed `dx_parity` (a cluster/DX-Hunter spot pick) latches a provisional parity and refines it to the true opposite-of-DX value on the first genuine decode from the latched partner.

## Half-duplex parity scheduler + always-answer-callers (#39)

`pancetta-qso/src/qso_manager.rs`, `pancetta/src/coordinator/qso.rs`: FT8 is deaf while transmitting, so **every concurrent active QSO must transmit on the same parity** — the opposite 15s window stays free to hear responses; we never TX in sequential windows. Pure `admit_new_qso(current_side, desired) -> TxAdmission::{Admit,Queue}` (idle adopts the side, unpinned desire rides the live side, same-side runs concurrent, cross-side queues — never preempts) + `QsoManager::current_tx_side()` (the parity active QSOs are committed to). Gated into the autonomous `StartAutonomousQso` handler: a cross-parity pounce/CQ is deferred this slot (the DX re-CQs and we re-evaluate once the side clears). **Always-answer-callers**: an always-on decode-loop path (`classify_caller_answer` + `maybe_answer_caller`) answers a station calling us **independent of the autonomous toggle** — `classify_caller_answer` maps a frame directed at us to the reply rung (CqResponse→Report, SignalReport→ReportAck, ReportAck→Rr73, FinalConfirmation→73; compound-aware). Gated by TxPolicy != Disabled + no-active-QSO-with-them (`has_active_qso_with`) + parity-admit + capacity (`max_concurrent_qsos`); it's a *response* not unattended *initiation*, so the FCC presence gate doesn't apply. Carries no failure-backoff, so it also satisfies #43 part 2 (answer a DX that calls us after our initiation watchdog retired the QSO).

## Manual multi-transmit + cross-parity queue (#40)

`pancetta/src/coordinator/qso.rs`: the TX coalescer (`coalesce_transmit_requests`) already multi-streams concurrent same-parity QSOs into one `MultiTransmitRequest`, so selecting 2+ DX in the **same window** runs them at once. The manual `StartQso` handler parity-gates each pick: same/idle side starts immediately; a **cross-window** pick is deferred into `PendingManualCalls` (dedup by callsign, bounded 16) with an operator status line, never preempting a live QSO. `promote_pending_manual_calls` drains the queue when a QSO goes terminal — on completion **after the 45s TX grace** (so the trailing 73 and the new opposite-window call never key in sequential slots) and immediately on failure — adopting the freed side (or the oldest queued call's parity when idle) and starting matching calls, oldest first, up to the free capacity (PAN-78; see below); cross-parity ones, and matching ones with no free slot, keep waiting. Pure `partition_pending_calls` split (unit-tested). Operator rule: queue cross-parity, never preempt, flip sides only when the current side's QSOs all finish. As of the 2026-07-18 review's Batch 2, `RespondToCaller` (the manual answer-a-caller path) runs through this same `admit_new_qso` gate too — previously it was the one manual entry point that admitted immediately regardless of the committed side.

Bundles are **not** allowed to silently disagree on parity or collide in frequency: `coalesce_transmit_requests` and the autonomous plan-bundler both exclude (never coerce) a retained stream whose `tx_parity` disagrees with the bundle's anchor, or whose `frequency_offset` falls within `MIN_TX_SEPARATION_HZ` of an already-retained stream — an excluded stream is not dropped, it transmits individually on its own parity/offset next cycle (Batch 2 for the parity check, Batch 3 for the frequency check). This closes a real gap: a bundle previously assumed its folded streams "naturally" shared one parity, which wasn't actually enforced anywhere.

## Manual admission capacity gate (PAN-78)

`max_concurrent_qsos` used to be enforced only by the TX worker's coalescer (`coalesce_transmit_requests`), which sees only the `TransmitRequest`s drained in one cycle. A QSO that has transmitted and is now waiting for a reply sends nothing, so it was invisible: with the default cap of 1, calling K1AAA and then K2BBB on the same window opened both, and K2BBB transmitted while K1AAA was still active. The cross-parity queue had the same hole: a window flip with N queued calls opened N QSOs at once.

**One rule.** `capacity_snapshot` (`pancetta/src/coordinator/qso.rs`) is the single concurrent-QSO capacity rule for every QSO the QSO component opens. It was extracted unchanged from `maybe_answer_caller`'s step-4 check, which still uses it (byte-identical decisions and diagnostics). Outside Fox mode it compares `QsoManager::active_qso_count()` (non-terminal QSOs) with `max_concurrent_qsos`; in Fox mode it compares `active_caller_qso_count()` with `fox_max_streams`. The decision itself is the pure `pancetta_qso::admit_within_capacity(active, cap)` (admit iff `active < cap`), the sibling of `admit_new_qso`. It deliberately does not count `active_tx_qsos`: that set keeps completed QSOs for the 45 s TX grace, and counting it would add a 45 s penalty to every back-to-back QSO. A completed QSO's trailing 73 alongside the next QSO's first call is still bounded per cycle by the unchanged TX cap.

**Refuse, not queue.** `StartQso`, `RespondToCaller`, `EngageHound` and `StartCq` check capacity after the placeholder, own-callsign and parity gates, immediately before the `QsoManager` open call. At capacity, `refuse_manual_open_at_capacity` refuses the open before any QSO, `MessageToSend` or `TransmitRequest` exists: a status line (`Not calling K2BBB — 1/1 QSOs active (max_concurrent_qsos); finish or abort one first`), a Warn `qso` Diagnostics entry (`Refused call to K2BBB — at capacity 1/1 (max_concurrent_qsos)`), and for a remote request a relayed `TxDenied` (the existing `error` event, `remote TX denied: call to K2BBB refused — at capacity 1/1 (max_concurrent_qsos)`). The relay is gated on `display_feed_enabled` (additive-only) and is not an agent-audit entry: a capacity refusal is not a security denial. Queuing was rejected: queued calls promote only on terminal events, so a refused call could key minutes after the operator moved on. A cross-parity pick still queues exactly as before; capacity is applied when it is promoted.

**Opens that add nothing still pass.** `QsoManager::manual_open_adds_active_qso(callsign, tx_off, step)` reports whether a manual open would add an active QSO or resolve to an existing one: FIX 1 re-call of an active manual QSO, FIX 3 one-for-one supersede of any active QSO with that callsign on that band, FIX B `Rr73` re-key of a manual QSO completed within the 45 s rework grace, and `SeventyThree` (opens at `Completed`, never active). Those opens are never refused, so the gate can never block the "repeated manual actions resolve to the existing QSO" invariant. The `manual_open_predicate_matches_real_open_effect` test pins the predicate to the real open functions. `EngageHound` is a manual `respond_to_cq_with` at the Fox's deterministic low calling offset, so `QsoManager::engage_hound_adds_active_qso` applies the same predicate at that offset: pressing Shift+H again on a Fox already being worked continues that QSO and is never refused. `StartCq` always opens a new QSO and skips the predicate.

**Capacity-aware promotion.** `promote_pending_manual_calls` takes `QsoCapacityLimits` and starts at most `cap - active` (saturating) queued calls per run, oldest first; `partition_pending_calls(queue, side, free_slots)` keeps the rest in order. Parity decides which calls may start; capacity decides how many. Each promoted entry consumes a slot even if it later resolves to an existing QSO: that can delay a queued call by one terminal event but never exceeds the cap.

**Why `tx.rs` is unchanged.** The coalescer's cap stays the per-cycle stream bound for frames already in flight, including the `admitted.len() == 1` fast path. PR #348's history (an active-count floor that defeated the cap, rounds 4→5; an over-cap re-enqueue livelock, round 6) is why admission is enforced here, against `QsoManager`'s true state, instead of being reconstructed from the TX worker's message-driven view. Admission refuses new QSOs at the true count; it never raises the ceiling for QSOs already admitted.

**Autonomous opens.** The autonomous task still gates on `active_tx_qsos.len()` before it sends `StartAutonomousQso`, but a manual open can fill the slot before that message is handled, so the handler re-checks `capacity_snapshot` (after its parity gate) and skips the open at capacity, rolling back a self-CQ's speculative state like a cross-parity deferral. PAN-76 (merged from main) keeps Fox-admitted Hounds transmitting after Fox mode disengages; its Fox-mode count (active caller QSOs plus Fox-admitted Hounds in their completed grace) lives in `capacity_snapshot`, so manual opens and promotion apply it too. In Fox mode only the Fox's own (Fox-admitted) CQ is a free stream; a CQ started by hand while Fox mode is on counts. Every admission path (the four manual opens, `StartAutonomousQso`, auto-answer, and queued-call promotion in the event forwarder) checks capacity and parity and opens the QSO under one shared admission lock, so two tasks can never both take the last free slot. PAN-75 (rolling back bookkeeping for frames the TX worker drops) is complementary.

### Multi-TX slow-start fix (2026-06-27)

`coordinator/tx.rs`: the worker used to coalesce the instant the FIRST `TransmitRequest` arrived (~10 ms after pickup), committing the slot to a single stream; sibling openings from serial manual keypresses (each crossing two async hops) landed *after* the batch formed and only joined on the next slot via keep-call rearm — so selecting N same-window DX trickled in one-stream-per-cycle instead of all firing in the first window. (Autonomous never showed it: it enqueues all openings as one tight batch between slots.) Fix: a brief **collection window** (`COALESCE_COLLECT_WINDOW_MS = 800`) before the pickup-coalesce, so same-parity openings batch into one `MultiTransmitRequest`. The window is **absorbed by the Step-6 slot-wait** (PTT keys slightly later; audio still emits at the slot boundary), so it adds no real latency in the common single-QSO case; kept short so a request arriving in the final fraction before its slot is rarely pushed a slot. The coalescer (`coalesce_transmit_requests`, already unit-tested) does the actual multi-fold.

## Drop-stale-TX gate (ended-QSO TX purge)

`coordinator/{mod.rs,qso.rs,tx.rs}`: closes the "we keep TXing every cycle after a QSO ended / only a restart fixes it" bug. The coordinator holds `active_tx_qsos: Arc<RwLock<HashSet<String>>>` (uppercased+trimmed qso ids, keyed via `active_tx_qso_key`). The QSO component keeps it in sync from the `QsoEvent` stream: **inserts** on `StateChanged` into any non-terminal active state; **removes immediately** on `StateChanged` into terminal `Failed` (covers Superseded / UserCancelled / Timeout / SignalLost) and on `QsoFailed`; **removes after a 45s grace** (`COMPLETED_TX_GRACE`, spawned delayed task) on `QsoCompleted` so the final 73 still goes out but leftover backlog is dropped shortly after. (Was 16s — too short: same-parity TX slots are **30s** apart, so a 73 emitted just too late for the immediate next slot defers a full 30s and the 16s grace expired first, silently dropping it — the operator saw us stop at R-report and had to send 73 by hand, observed on-air with G8KHF 2026-06-23. The coalescer keeps newest-per-QSO so only the single 73 is pending, hence a longer grace can't leak stale report frames.) The TX worker (`tx.rs`, helper `tx_qso_is_live`, fails *open* on a poisoned lock) re-checks at the last instant before keying PTT — after the pre-PTT slot sleep and at defer-time, in **both** the single-`TransmitRequest` and `MultiTransmitRequest` arms (the multi-TX arm's defer-time recheck landed in the 2026-07-18 review's Batch 5, reusing the same per-item `live_mask` partial-liveness rebuild the key-time gate already had) — and drops any item whose `qso_id` is no longer in the set: no PTT, clears the TX strip, sends a failed `TransmitComplete`, logs `target:"tx.policy"` "dropping stale TX for ended QSO …". Requests with `qso_id == None` (manual free-text / tune / test-TX) are never gated. The live-TX indicator is also made unmistakable: bold white-on-red `🔴 TX NOW` chip + frame in `render_tx_strip` for the full ~12.64s the message is keyed, mirrored on the QSO Status panel's "Now:" line (`ui/qso_status.rs::live_tx_for_qso`).

**Multi-TX bundle-arm pivot + tombstone (same-day follow-up, this batch):** Batch 1's Step-4c late pivot (`qso-engine.md`) only covers the single-`TransmitRequest` arm — it swaps ONE in-flight frame's text for a fresher `LatestTxIntent` at key-time. The `MultiTransmitRequest` arm had no equivalent: a bundle's items are encoded once (Step 1) and, until this fix, transmitted verbatim even if one item's QSO advanced (e.g. the DX sent RR73 and the QSO emitted its closing 73) while its frame sat bundled alongside a different, still-in-progress QSO during the pre-PTT wait — observed on-air (SM2LIY re-sent a stale "R-14" report twice after already sending RR73, while bundled with a second manual QSO to C6AVD). New pure fn `pivot_bundle_items` (`coordinator/mod.rs`) is the bundle-arm analogue of `tx_pivot_target`: per item, if `latest_tx_intent` has a differing entry for that `qso_id`, swap `message_text`+`frequency_offset` and report the pivot. `tx.rs`'s multi-TX arm's key-time Step 4b now runs this on the still-live subset right after computing `live_mask` (before the fast-path/rebuild branch) — a pivot alone (even with every item otherwise live) now **forces** the rebuild-via-`encode_and_modulate_multi_tx` path, since the item list holds different text than what Step 1 encoded; the existing partial-staleness rebuild machinery is reused unchanged, just fed the pivoted item list. A new "Step 0-dup (bundle)" gate, mirroring Batch 1's single-TX `is_pivot_duplicate` tombstone check, drops any bundle item that duplicates an already-pivoted-and-sent frame (dropping the whole bundle if every item turns out to be such a duplicate) before Step 0's policy mute.

## TX-frequency hold + adaptive stall escape

`pancetta-qso/src/qso_manager.rs`, `states.rs`, `autonomous.rs`, `pancetta/src/coordinator/{qso.rs,autonomous.rs,tui_relay.rs}`: we hold a QSO's TX audio offset for the whole exchange. The offset is latched once at QSO open into `QsoMetadata.frequency` and reused for every reply/keep-call (the allocator runs only at open); the one mid-QSO mover is the adaptive stall escape (PAN-72, 2026-09-04).

**Detection (`QsoMetadata.stall_cycles`).** The per-slot keep-call re-send in `rearm_manual_calls_at` *is* the stall signal — reaching it means the DX did not advance the QSO since the last slot — so that site is the sole incrementer of `stall_cycles`. This counts **silence** as well as a repeated/non-advancing frame, which the superseded mechanism never did. Any genuine forward advance (`process_message_for_qso`) resets it to 0 and latches the current offset into `QsoMetadata.last_known_good_offset_hz`.

**Action (`QsoEvent::TxOffsetActionNeeded`).** Once `stall_cycles >= TimeoutConfig::qso_stall_switch_after` (default 4, operator-facing as `[autonomous] qso_stall_switch_after`) and **only in `TxFreqMode::Auto`**, the manager emits `TxOffsetActionNeeded{qso_id, action}` and resets `stall_cycles` so one stall fires once. The Switch/Revert ping-pong is decided against `last_known_good_offset_hz`: none recorded, or we are currently *on* it → `Switch{avoid_hz: current}`; we are on a previously-switched offset → `Revert{target_hz: last_known_good}`. So a QSO switches away from known-good on the first stall, comes back on the second, switches away again on the third.

**Resolution.** `QsoManager` never calls the allocator (single-scorer invariant). The coordinator's QSO event-forwarder pushes the action onto `pending_qso_offset_requests: Arc<Mutex<Vec<_>>>` — the same mailbox shape as `pending_autonomous_cq_dispatch_failures` — which the Autonomous task drains once per slot: `Switch` resolves through the same `allocate_smart_frequency(avoid_hz)` the CQ-hunting switch uses, `Revert` uses `target_hz` directly. Either way it commits via `QsoManager::apply_tx_offset_switch`, the sole external mutator of an active QSO's offset (clamps to the *active-QSO* band [200, 2900] — deliberately wider than the 300–2700 band the allocator PICKS fresh offsets in, since a `Revert` target may be an unclamped reply offset the QSO demonstrably worked on; updates both `metadata.frequency` and the state's embedded frequency, clears `pending_freq_drift`, resets `stall_cycles`), and mirrors the applied offset into `active_tx_offsets` so the allocator's self-avoidance and the next nudge's `avoid_hz` see where the QSO actually is. This **closes FQ-F10**: routing through the smart allocator makes the move aware of our other active streams (`own_frequencies`) and of the spectrum, which the old blind `+300 Hz` hop was not.

**Manual escape (`u`).** The operator can force the same thing at any time without leaving Auto: `u` pushes a forced `Switch` for the active QSO onto that same mailbox (threshold bypassed), or — with no QSO running — arms `pending_cq_offset_nudge`, a one-shot flag the Autonomous task forwards into `AutonomousOperator::request_manual_switch`. It never touches `tx_freq_mode`/`tx_offset_hold_hz`. In Hold it is a declared no-op with a status line, not a silent one: the allocator's Hold branch ignores `avoid_hz` and returns the parked offset, so queueing a Switch there would either do nothing or drag a held QSO onto the park offset.

**Round-1 review hardening (Codex on PR #350, 2026-09-05).** Nine findings, all fixed inline per the convergence policy. Six touch this mechanism directly:

- *Forward progress is role-agnostic.* `dx_frame_advanced` was computed from `ladder_rank`, which is deliberately Caller-shaped so it can be compared against a `ResponseStep` in the manual context-reply path — it ranks neither `CallingCq` nor `WaitingForReport`. A caller answering an unanswered CQ therefore read as "no progress" and the CQ's stall streak carried into the established exchange (three unanswered CQs + one missed report slot = an immediate switch, with no known-good offset recorded). A separate `progress_rank` now covers both roles rung for rung (`CallingCq`/`RespondingToCq` → 0, `WaitingForReport`/`SendingReport` → 1, the confirmation pair → 2, `Completed` → 3) and drives the forward-progress predicate only; `ladder_rank` is untouched.
- *Hound QSY re-anchors known-good.* The QSY into the response region rides the same `RespondingToCq → SendingReport` advance that records known-good, so known-good was pinned to the PRE-QSY low calling offset and a later `SendingReport` stall emitted `Revert` straight back into the calling region — undoing the procedure move. The QSY block now records the post-QSY offset as known-good, keeping switch/revert working *within* the response region for a merely slow Fox.
- *Terminal QSOs refuse offset actions.* Completed entries stay in the map (and in `active_tx_offsets` for the 45s trailing-73 grace window), so a late action still found its QSO; `QsoState::set_frequency` refused terminal states but `metadata.frequency` did not. `apply_tx_offset_switch` now checks `state.is_active()` before mutating anything and returns `QsoManagerError::QsoNotActive`.
- *Staleness token.* The drain runs once per 15s slot, so the DX can advance in between — resetting `stall_cycles` and marking the current offset known-good — while the queued request still moves the QSO off it. `QsoMetadata::advance_generation` is bumped in lockstep with those two fields, carried on `TxOffsetActionNeeded` and on the mailbox's new `OffsetActionRequest` element, and re-checked at commit time (`QsoManagerError::OffsetActionStale`). An operator-forced `u` nudge carries `None` and is never stale. Both this and the terminal refusal answer `true` to `QsoManagerError::is_expected_offset_action_refusal`, so the drain logs them at `debug!`.
- *Hold is re-read at commit time, and batch offsets are reserved.* An action queued under Auto could be committed after the operator pressed `f` (a `Switch` resolves through the allocator's Hold early return onto the *parked* offset; a `Revert` skips the allocator entirely) — the drain now discards the whole batch when the mode reads Hold. And because `set_own_frequencies` only syncs the allocator later in the same tick, every `Switch` in one batch ranked against the identical stale view: two concurrent QSOs could pick the same candidate. The drain accumulates each committed offset and passes it to the new `allocate_smart_frequency_avoiding` as an extra HARD exclusion (same treatment `avoid_hz` got in Codex round 6 on PR #276, for the same reason: the `own_frequencies` soft penalty is not a guarantee).
- *The UI sees the move.* `apply_tx_offset_switch` now emits `QsoEvent::TxOffsetApplied`, and the event-forwarder's new arm calls the extracted `push_active_qso_snapshot` helper (shared with the pre-existing `StateChanged`/`QsoCompleted`/`QsoFailed` sites). An offset move is not a state transition, and a stalled exchange is exactly where no further transition arrives — the banner and TX-placement marker used to keep the pre-switch offset through multiple switch/revert cycles.

The `u` nudge picked up two of its own: the Hold gate is hoisted above the active-QSO branch so a Hold-mode nudge with no active QSO also reports a declared no-op (the outcome is `CqNudgeArmed`, not `CqNudge`, because whether the armed flag produces a move depends on the operator entering its CQ branch on a later cycle — a fact nothing outside the autonomous task can see, and one `decide_at` re-derives per cycle anyway); and `decide_at`'s thin-decode-history early return now restores `manual_switch_requested` instead of discarding it, since `mem::take` had already consumed it and the manual path's streak is normally below threshold, so the nudge was lost forever — in exactly the post-startup/post-band-hop situation where an operator is most likely to press `u`.

**Round-2 review hardening (Codex on PR #350, 2026-09-06).** Five more findings, all fixed inline:

- *Hound switches are region-constrained.* Round 1's known-good re-anchor fixed the `Revert` case, but a post-QSY Hound's `Switch` still resolved through the general 300–2800 Hz allocator and could land below `response_min_hz`, where the Fox is not listening. The drain now derives a hard Hz window from the QSO's live Hound state — the calling region while `!hound_qsyed`, the response region after — and passes it to a new `allocate_smart_frequency_in_range`, which honors it on the ranked path (returning `avoid_hz`, the established "no valid relocation" signal, when the window empties the list) and clamps the range-blind legacy fallback into it. Operator `[hound]` bounds are used; a non-finite or inverted window degrades to no constraint.
- *Revert targets are re-checked for occupancy.* A `Revert` bypasses the allocator, so with `max_concurrent_qsos > 1` nothing noticed another QSO taking the known-good offset after we switched away from it; committing put two of our own streams inside `min_separation_hz` and the TX coalescer then excluded one from the bundle. The drain now tests the target against `active_tx_offsets` (excluding the QSO's own entry) plus the batch reservations and, when taken, falls back to a fresh allocator-resolved switch avoiding both the stalled offset and the occupied target.
- *The drain runs on this slot's inputs.* It sat ahead of `update_spectral`/`update_live_spots`/`feed_decoded_messages`, so every switch ranked against the PRECEDING slot's occupancy and the placement instrument computed later in the tick could disagree with the decision. Moved below them — still above `set_own_frequencies` (so the committed-offset mirror lands in time for the same tick's sync) and above the `auto_config_enabled` gate (so a manual-only operator's `u` still drains).
- *`u` only nudges a live QSO.* `active_tx_qsos` retains a completed QSO for its 45s trailing-73 grace window, so the nudge's blind first-entry pick could name a terminal QSO — or shadow a live concurrent one — and report "Nudging active QSO" for a request the drain then refused with `QsoNotActive`. Selection now filters against the engine's own active-QSO set, read through the same restart-safe `qso_manager_watch` handle the drain uses; a grace-window-only snapshot correctly falls through to the CQ fallback. Selection-time filtering complements, not replaces, round 1's commit-time refusal.
- *Muted cycles are not stalls.* Under `TxPolicy::Disabled` the coordinator's hard mute blocks every frame `rearm_manual_calls_at` re-emits, yet each blocked re-send was counted as a silent on-air cycle — four of them in Auto moved an established QSO off a known-good offset nothing had transmitted on. `QsoManager` now observes the global policy through a shared atomic (`set_tx_policy_source`, mirroring `set_tx_freq_mode_source`, defaulting to a private `Full`), and both the counter and the action emission are gated on `allows_any_tx()`. Accumulated cycles are kept, not reset: they came from real transmissions.

**Round-3 review hardening (Codex on PR #350, 2026-09-06).** Two findings: the hard exclusions (`avoid_hz` + the batch reservations) bound only inside the allocator's ranked spectral branch, leaving the deterministic no-snapshot fallback — every resolution until the first waterfall lands — free to hand two same-batch switches the identical offset (`allocate_cq_frequency_excluding` now enforces the same exclusions and window there); and the allocator's "no valid relocation exists" signal (`avoid_hz` returned unchanged) was committed as a real move, clearing the QSO's accumulated stall evidence and announcing a `TxOffsetApplied` for a relocation that never happened (`apply_tx_offset_switch` now refuses it as `OffsetActionNoOp` — filed as PAN-79).

**Round-4 review hardening (Codex on PR #350, 2026-09-06).** Four findings; three fixed inline, one deliberately left as-is:

- *Other live QSOs are hard exclusions, not a scoring penalty.* Round 1's `reserved_hz` only ever held offsets committed earlier in the SAME drain batch, so the common crowded-spectrum shape — several live QSOs, exactly one of them switching — left the rest out of the exclusion set entirely. Their offsets reached the allocator only as `score_candidate`'s soft `-50` `own_frequencies` penalty ("effectively eliminates", not *does* eliminate), applied against an occupancy view `set_own_frequencies` does not sync until later in the same tick. The drain now folds the whole `active_tx_offsets` view (minus the QSO being resolved) into the same hard `also_avoid_hz` channel, on both the `Switch` path and the `Revert` re-resolution fallback.
- *The Hound region is revalidated at commit time.* Round 2's window was computed from a `get_qso` snapshot taken before the allocator ran and then handed to an awaited commit; the Fox's first report performs the mandatory calling→response QSY, and an operator-forced `u` nudge carries no generation token, so the advance guard could not catch a QSY landing in that window. `hound_switch_range_hz` moved into `pancetta-qso` (one definition for resolve and commit, now with the inverted/non-finite sanitization the allocator already applied), and `apply_tx_offset_switch` re-derives it from the locked `progress` it is about to mutate — refusing an out-of-region offset as `OffsetActionOutsideHoundRegion` rather than committing it. Refusing rather than re-clamping keeps the stall evidence so the detector re-raises against the correct region.
- *The offset a switch vacated stays valid while its reply is in flight.* A relocation is committed AFTER the frame that triggered it has gone out on the old offset — `rearm_manual_calls_at` re-sends and trips the threshold in one pass, the drain commits on its next tick — so whoever answers that frame answers an offset we have already left. An unanswered manual CQ rejected the answer outright (the new offset in `CallingCq`, `partner_freq` deliberately `None` pre-establishment, a 15 Hz gate) and would spawn a duplicate QSO; an established QSO credited `last_known_good_offset_hz` to an offset never transmitted on, poisoning the Revert target. Fixed on the receiving side rather than by resequencing the trigger, because the last pre-switch frame is already in flight when the move commits — delaying the emission or the commit by a cycle only moves the same window. `QsoMetadata::pre_switch_offset` records the vacated offset and when we left it; for two FT8 slots it is a second accepted RX baseline at the same tolerance (on top of, never instead of, the current one) and is the offset a forward advance credits as known-good. Consumed on the first advance after a switch.
- *Deliberately not fixed: `CallInitiation::Auto` QSOs can't reach the stall threshold.* `AUTO_RESEND_MAX_CALLS = 2` caps `stall_cycles` at 1 against a default threshold of 4, so an autonomously-initiated pounce/exchange times out before the adaptive mechanism ever fires — only manually-initiated QSOs benefit today. This is the PAN-72 design as approved (see the design doc's "Explicitly out of scope" section): written generally on purpose, left dormant for Auto QSOs specifically to avoid extending the protected 30s/2-call Auto safety bound (`report_timeout`, `AUTO_RESEND_MAX_CALLS` — guards against a past regression). Confirmed intentional rather than reversed; filed as PAN-81 to revisit with real on-air experience.

**Round-5 review hardening (Codex on PR #350, 2026-09-06).** Two findings, both refining round 4's `pre_switch_offset` mechanism for the relocation it did NOT reason about — an operator-forced `u` nudge, which unlike a stall switch is coupled to no old-offset triggering resend:

- *A deferred frame pivots when only its offset changed.* `tx_pivot_target` — the "freshest `MessageToSend` at key-time" check behind the TX worker's Step 4c and `pivot_bundle_items` — compared only `message_text`, so a frame that advanced by MOVING rather than by re-rendering was invisible to it. A switch relocates the QSO without changing what the frame says, so the next rearm publishes the same text at the new `applied_hz`, and an older request still in the worker's (up to ~30s) pre-PTT wait keyed on the offset the switch existed to leave. Round 4's receive-side grace makes a reply to the stale offset ROUTE; it does not make the transmitted frame fresh. `frequency_offset` now joins `message_text` in the comparison — matching what `is_pivot_duplicate` and `classify_incoming_during_tx` already do (PAN-73 rounds 3 and 6) — guarded by `PIVOT_OFFSET_EPSILON_HZ` (0.5 Hz) so float noise cannot report an unchanged re-send as a pivot and plant a tombstone that swallows the next identical request.
- *Credit the offset the advancing reply actually decoded at.* Round 4's crediting rule checked only the clock, so any advance inside `PRE_SWITCH_OFFSET_GRACE` recorded the VACATED offset as known-good. That is right for a stall switch, whose trigger IS the old-offset resend, but a `u` nudge can land on a QSO about to transmit: the next rearm goes out on the NEW offset and its answer arrives well inside the same 30s window, so round 4 credited an offset that had just demonstrably failed and aimed a later `Revert` back at it. The reply's decode frequency is the evidence: the vacated offset is credited only when `message.frequency` matches that baseline under the same tolerance the relevance gate admitted the frame with (taken from the pre-transition state, so a callsign latched by this very advance can't widen it retroactively); the new offset — or a `partner_freq` split matching neither — credits the current offset instead. Overlapping baselines still resolve to the vacated one (proven history). Grace consumed either way.

**Round-6 review hardening (Codex on PR #350, 2026-09-06).** Four findings — all labelled P2 by the reviewer, all real correctness bugs by substance, so fixed inline under the rounds-6-15 policy rather than ticketed:

- *The DX's first report is forward progress.* `progress_rank` collapsed `SendingReport { their_report: None }` and `{ Some(_) }` into one rung, so the transition that turns our outbound from a plain report into an R-report read as "no progress". A switch queued by the preceding threshold-hitting rearm therefore left `advance_generation` unmoved and passed the commit-time stale-action guard, relocating the QSO at the moment the DX finally answered; below the threshold the partial stall streak carried forward. Split (with the analogous `Contest(ExchangingInfo)` `their_serial` split so the dormant contest wiring can't inherit it). `ladder_rank` deliberately unchanged — the manual regression predicate needs both `SendingReport` shapes equal — and a repeated report (`Some -> Some`) still reads as no progress.
- *Credit the new offset when an operator nudge moved a SPLIT QSO.* Round 5's decode-frequency check is evidence only on a Tx=Rx QSO; on a pre-existing split (Hound, an `o` hold, a de-conflict nudge, a passband clamp) the DX transmits at `partner_freq` regardless of where we key, so its reply cannot discriminate our two offsets. The round-5 correction fell back to crediting the vacated offset unconditionally — right for a stall switch (which only fires because that offset stopped working, and whose triggering resend went out on it), wrong for a `u` nudge (which indicts nothing, and after which our very next frame goes out on the NEW offset). Provenance is now threaded rather than guessed: `OffsetRelocationOrigin` replaces the bare `Option<u32>` staleness token on `OffsetActionRequest` and in `apply_tx_offset_switch`'s signature (the token lives inside the `StallDetected` variant, so "no token" can no longer be conflated with "operator forced"), and `QsoMetadata::pre_switch_offset` becomes a `PreSwitchOffset` carrying `operator_forced`.
- *Search the full configured Hound window.* A `[hound]` response region above 2800 Hz was only ever used to FILTER candidates already generated from the allocator's fixed 200–2800 Hz range, so it emptied both the ranked list and the legacy fallback scan: every post-QSY Hound stall switch or `u` nudge resolved back to `avoid_hz` and was refused as a no-op with the configured region never searched. Both scans now take their bounds from one shared `frequency::sweep_bounds`, which only ever WIDENS the sweep and keeps the grid anchored at the configured floor, so an in-range window ranks exactly as before. Ceiling reconciled while there: `[hound]` validation capped at a round 3000 Hz, admitting a (2900, 3000] region no relocation could reach (`apply_tx_offset_switch` clamps to `ACTIVE_QSO_TX_OFFSET_MAX_HZ` = 2900, then its region guard refuses the clamped value) and that `hound_offset_for`'s QSY would write into `metadata.frequency` unclamped. Now 200–2900, the real transmittable envelope.
- *Score switches against the QSO's own TX parity.* The drain passed `None` for `target_parity`, putting `rank_candidates_with_parity` on its slot-blind path — where a frequency quiet only in the opposite (listening) slot can outrank one genuinely clear in the slot we will actually key in, so adaptive recovery could move a colliding QSO straight into another collision. The `get_qso` snapshot the Hound/revert checks already take carries the latched parity; it now feeds both the ordinary `Switch` and the occupied-target `Revert` fallback. Parity only weights scoring — it never filters — so the search can never fail more often.

**Round-7 review hardening (Codex on PR #350, 2026-09-06).** Six findings — all labelled P2 by the reviewer, all real correctness bugs by substance, so fixed inline under the rounds-6-15 policy. One of them (the decoder recentre) is the failure mode the whole mechanism exists to prevent:

- *Recentre the decoder after relocating an unanswered CQ.* With the QSO filter on (the default) `coordinator::ft8`'s main decode is collapsed to ±60 Hz around the shared `active_qso_freq_hz` hint, which was written in exactly one place — the forwarder's `StateChanged` arm. Round 1's `TxOffsetApplied` arm refreshed only the UI snapshot, so an applied switch left the hint on the abandoned offset while the allocator guarantees the move is at least `min_separation_hz` (75 Hz) away. On an unanswered `CallingCq` that is self-sustaining deafness: `partner_freq` is deliberately `None` there (round 5), so the state frequency IS the hint and the switch just changed it — an answer to our NEW CQ offset fell outside the decode search, no decode meant no `StateChanged`, and nothing ever refreshed the hint again. The arm now recomputes the hint from the POST-switch QSO through the extracted `decoder_hint_freq_for` (`partner_freq` when the DX's RX offset is known, the state frequency otherwise), which is also what keeps an ESTABLISHED QSO pointed at the DX — the switch moves only our TX side. A QSO that went terminal in between yields `None` and the hint is left alone; the terminal arms own that clear.
- *Coalesce a drain batch by QSO.* Two requests for one `qso_id` are ordinary — a double `u` press, or (given the streak fix below) an automatic action re-raised alongside a manual nudge. The loop committed all of them in sequence, each commit overwriting `pre_switch_offset` with the previous commit's destination: an intermediate offset nothing ever transmitted on, losing the record of where an answer to the real pre-drain frame is arriving. `coalesce_offset_requests_by_qso` keeps one request per QSO, an `OperatorForced` nudge outranking a `StallDetected` action in either arrival order (the nudge is deliberate and current by construction; the automatic action re-raises next slot anyway), first-arrival otherwise so the `reserved_hz` chain stays deterministic.
- *Preserve the threshold streak when no relocation exists.* PAN-79 taught `apply_tx_offset_switch` to refuse a no-op relocation precisely so a crowded band would not clear valid stall evidence — but the threshold-tripping path had already zeroed `stall_cycles` at EMISSION time, a slot earlier, so the refusal preserved nothing and recovery was pushed out by another full `qso_stall_switch_after` window even if a slot freed up immediately. The emission-time reset is gone: raising an action is not relocating. Two reset sites remain — a successful commit, and a genuine DX advance — and until one lands the QSO re-raises once per slot (bounded to one retry by the coalescing above).
- *Revalidate Hold immediately before each commit.* Round 1's check runs once, before the request loop, and each iteration then awaits twice (`get_qso`, the commit). A contended QSO lock leaves a real window for the operator's `f` press, after which every remaining request in the batch still committed against the stale reading — a `Switch` through the allocator's Hold early return onto the parked offset, a `Revert` without consulting the allocator at all. Re-read per request now; the batch-level check stays as the cheap early-out.
- *Count stalls only after EVERY TX gate permits the frame.* Round 2 closed the `TxPolicy` hard mute, but a `remote_origin` QSO's frames are additionally armed-TX gated in the TX worker. An arm expiry or explicit remote disarm drops every rearmed frame while `TxPolicy` still reads `Full`, and the detector counted that self-inflicted silence as DX silence. `QsoManager::set_remote_tx_permitted_source` (mirroring `set_tx_policy_source`) shares the predicate, consulted only for QSOs that are actually `remote_origin` and defaulting to "permitted" so nothing else changes. Read-only evidence, never a gate: the fail-CLOSED armed-TX check stays in the TX worker, and since `remote_tx_permitted` denies on a poisoned lock a poisoned arm also stops the counting.
- *Count stalls on the active mode's slot cadence.* The rearm gate hardcoded 15 s — FT8's period alone — while `qso_stall_switch_after` is documented in slots, so under FT4 (7.5 s) a threshold of 4 took ~60 s / 8 real slots and under FT2 (3.2 s) far worse, often letting the ordinary QSO watchdog win first. The gate moved to millisecond resolution so 7.5 s is expressible, and the period comes from the coordinator's **shared** `active_slot_ns` atomic — injected via `QsoManager::set_active_slot_ns_source`, mirroring `set_tx_policy_source`, and defaulting to FT8's 15 s so unwired callers are unchanged. The first cut derived it from the *cloned* `QsoManagerConfig::active_mode` string instead, which was wrong in a way the re-review caught: `QsoManager::start()` runs `timeout_check_loop` on a `clone()` and `Clone` copies `config` by value, while `set_active_mode` (the Shift+M handler) mutates only the coordinator's own binding — so a runtime switch into FT4/FT2 never retuned the cadence at all, and a switch FT4 → FT8 left the loop on 7.5 s for what was by then a genuine FT8 QSO, breaking the byte-identity invariant in that sequence. Reading `active_slot_ns` (which `try_switch_operating_mode` rewrites on every switch, from `Protocol::slot_ns()`) also removes the duplicated 15_000 / 7_500 / 3_200 lookup table: one source of slot truth, already correct for a build without the `ft2` feature, tracked live in both directions.

**PAN-140 (2026-09-12): first live on-air exercise found a real in-flight-PTT race.** Working W6M, a stall-switch committed 2.8s into an already-in-flight 12.64s transmission — `rearm_manual_calls_at`'s `stall_cycles` increment/threshold check has no guard against a PTT already keyed for the current slot, so the supersede path aborted the audio and re-keyed at the new frequency 2.8s late into the slot, garbling both frames (and with zero operator-visible log trace either way). Fixed in two commits (PR #369): a top-of-loop pre-check + re-queue in `drain_pending_qso_offset_requests`, then — after Codex's single review round caught that the pre-check's own `.await` for `apply_tx_offset_switch`'s write lock reopens the identical window — a real `ptt_active: Arc<AtomicBool>` (`QsoManager::set_ptt_active_source`, mirroring `set_tx_freq_mode_source`) re-checked INSIDE that write lock, immediately before the existing Hold recheck. New `OffsetActionPttInFlight` error, re-queued (not discarded, unlike Hold). Two sibling findings from the same live session, tracked separately: PAN-141 (a manually-latched TX parity can go stale before key-up against a fast-cycling pileup DX that changes callers — and therefore parity — in the gap) and PAN-142 (no operator-visible indicator during the two-strike frequency-drift confirm window, `maybe_confirm_frequency_drift_at`).

Separately, the autonomous collision-listen jitter (`process_collision_listen`) **holds while a QSO is active** (`active_qso_count>0`) — it only ever seeded the next CQ/opening offset, and only fires from a collision-LISTEN slot (a slot we listened on without transmitting), the "new information" precondition for changing TX frequency.

*Superseded:* the original escape was repeat-only — `QsoMetadata.{last_rx_text,dx_repeat_count}` counting consecutive *identical* non-advancing DX frames, hopping once via `stuck_hopped_offset` (+300 Hz, wrapping in [300,2700]) after `DX_STUCK_REPEAT_THRESHOLD` (4), with a *different* non-advancing frame resetting the streak to 1. Total silence — the common case — was never counted, and the hop was spectrally blind. All of those symbols are removed; one mechanism now, not two.

**Hold-mode now reflects the live parked offset** (2026-07-18 review, Batch 5): `AutonomousOperator::allocate_smart_frequency`'s Hold branch used to return only the static `config.tx_offset_hz`, ignoring the operator's actual `o`-modal parked offset. It now prefers the live `tx_offset_hold_hz` atomic (shared from the coordinator, same one the manual path already reads) when non-zero, falling back to the config value exactly as before when nothing is parked.

## Operator TX-offset control + multi-TX de-confliction

`pancetta-qso/src/qso_manager.rs`, `pancetta/src/coordinator/{qso.rs,tui_relay.rs}`, `pancetta-tui/src/{tui_runner.rs,app.rs,ui/mod.rs}`: on a **manual** call/answer (`StartQso`/`RespondToCaller`), our TX audio offset is chosen by priority **held-offset → de-conflict → Tx=Rx (fallback)**, decoupled from the DX's RX freq via the Hound `partner_freq` mechanism. (Operator note 2026-06-28: Tx=Rx is NOT the natural mode — like WSJT-X "Hold Tx Freq", they place their own offset; Tx=Rx is just the no-opinion fallback.) The pure `compute_manual_tx_offset(dx_freq, hold_mode, held_hz, active_offsets) -> (tx_off, Option<partner_freq>)` in `coordinator/qso.rs`: `candidate = (Hold && held!=0) ? held : dx_freq`, then `deconflict_offset(candidate, active_tx_offsets, MIN_TX_SEPARATION_HZ=75, 300, 2700)` (deterministic outward 25 Hz search, no RNG), then `partner = (tx_off != dx_freq).then_some(dx_freq)`. `respond_to_cq_with`/`respond_to_caller` gained a `partner_freq: Option<f64>` param (all other callers pass `None`; `engage_hound` passes `Some(fox_freq)`); `QsoManager::active_tx_offsets()` lists live streams to de-conflict against. **Regression invariant:** Auto + no held offset + no collision ⇒ `tx_off=dx_freq`, `partner_freq=None` ⇒ byte-identical Tx=Rx (guarded by unit + coord_sim tests). Operator sets the held offset with the **`o`** modal (200–2900 Hz; blank=clear→Auto; setting implies `TxFreqMode::Hold`) → `TuiCommand::SetTxOffset` → relay writes `tx_offset_hold_hz: Arc<AtomicU64>` (0=unset) + `tx_freq_mode`; a title-bar "TX off: NNNN (HOLD)" chip shows when held. The completed-QSO RF stamp uses `metadata.frequency` (our real TX offset), correct under Tx≠Rx. Scope: manual path only (autonomous allocator/pounce + Hound unchanged). Spec: `docs/superpowers/specs/2026-06-28-tx-offset-control-design.md`; plan: `docs/superpowers/plans/2026-06-28-tx-offset-control.md`.

## TX-failure operator visibility (2026-07-12)

`pancetta/src/coordinator/tx.rs`: every genuine TX-attempt failure (invalid frequency, encode/modulate error, defensive cursor-exceeded-waveform cases, multi-TX per-item encode error, whole-bundle multi-TX modulation error) used to only `warn!()` to the log file and send a `MessageType::TransmitComplete { success: false, .. }` consumed by Autonomous for self-CQ correlation (PAN-38), with failed QSO-keyed outcomes now mirrored to the QSO engine (PAN-75). From the operator's view a QSO that can never transmit looked identical to a QSO where the DX just isn't answering — it sat until the manual watchdog (5 min / 25 calls) or the autonomous per-state timeout fired, and the operator had to notice the coincidence and check the log file to learn TX never happened at all. New helper `emit_tx_failure_diagnostic` sends a `Warn`-level `DiagnosticEvent` (target `"tx.encode"`) alongside the existing log line + `TransmitComplete`, so `TX failed: <reason>` shows immediately in the TUI's Diagnostics overlay (Shift+D). Wired into 6 sites across the single-TX and multi-TX paths — including a strictly worse pre-existing gap found in the multi-TX per-item encode loop, where a failed item was silently dropped from `item_texts`/`symbol_sets` with **no** `TransmitComplete` ever sent for it (not even a delayed one). Deliberate policy/security skips (`TxPolicy::Disabled`, stale-QSO-at-defer-time drops, poisoned-arm-lock fail-closed) are deliberately NOT routed through this helper — those are intentional, already have their own operator-visible signal (TX-policy status / the emergency-stop toggle), and aren't the "invisible failure" gap this closes; see the helper's doc comment for the exact boundary. Does not touch QSO-state-machine timeout duration — shortening the wait before declaring a QSO dead (the memory's second, riskier idea) is a separate, deferred design question. Tests: `coordinator::tx::tx_failure_diagnostic_tests` (`tx.rs`).

## QSO/TX deep-review remediation (2026-07-18) — 5 batches, all landed

Full findings + rationale: `docs/qso-tx-deep-review-2026-07-18.md`. Scheduling/allocator-relevant fixes not already called out above, in landing order:

- **Batch 3** (frequency allocator correctness): the spectral snapshot fed to `SmartFrequencyAllocator` was mislabeled `freq_min_hz: 200.0` while the real waterfall bins start at 0 Hz, so every score read spectral data from the wrong bin by ~100-200 Hz — fixed to `0.0`. Decode-history occupancy was stamped with the wall clock at feed time instead of each message's own decoded `slot_parity`, inverting per-slot occupancy near a tick boundary — fixed to use the message's own parity. The "partially clear" scoring term used `15.0_f64.max(25.0 - activity*5.0)`, which clamps UP so a heavily-busy bin scored the same as a lightly-busy one — fixed to floor at 0. `register_qso_frequency`/`release_qso_frequency` were only ever called from unit tests, so the own-frequency-separation criterion could never fire in production — a new coordinator-level `active_tx_offsets` snapshot (mirroring `active_tx_qsos`'s own insert/remove points) is now synced into the allocator every autonomous tick via a bulk-replace `set_own_frequencies`. The autonomous pounce path and the always-on `maybe_answer_caller` path both latched a raw Tx=Rx candidate with no de-confliction against other active streams — both now run through the same `compute_manual_tx_offset`/`deconflict_offset` the manual path already used (a no-op when nothing collides).
- **Batch 5** (`6927e02c`/`41a8d8da`): frequency scoring gained parity-awareness — `rank_candidates_with_parity` weights clearness in the slot we'd actually transmit in far more heavily than the DX's own (irrelevant) listening window, wired end-to-end from the pounce path's known DX parity (`target_parity: None` degrades byte-identically for the self-CQ path, which can't commit to one slot at scoring time). The previously-dead `TuiCommand::FindClearOffset` (`t` key) now actually commits a real held offset via the same path the `o` modal uses, preferring the authoritative placement snapshot.

## Symptom C — adaptive coalesce window + timing-accuracy fix (2026-07-21)

`coordinator/tx.rs`: closes the residual gap left by the 2026-06-27 fix above (see "Multi-TX
slow-start fix" section) — the fixed `COALESCE_COLLECT_WINDOW_MS` (800ms) window still couldn't
batch serial manual keypresses realistically spaced 1-3s apart, so a pileup's siblings still
trickled in one-per-cycle via keep-call rearm instead of joining the first window. Confirmed still
open as of the 2026-07-18 deep review. Fix: the collection window now takes the same 800ms base
wait once, unconditionally (byte-identical to before for the common lone-request case), then
extends in further base-length increments only while the channel's queued-message count keeps
growing, capped by remaining `tx_late_max_ms` headroom (so a request already late in its slot
extends little or not at all) and a protocol-scaled ceiling (`COALESCE_MAX_EXTENSION_MS = 3000`ms
FT8-baseline, scaled like `coalesce_collect_window_ms`). Does not modify
`coalesce_backlog_into`/`coalesce_transmit_requests` — only how long the worker waits before that
existing drain runs once.

**Folded-in timing-accuracy fix:** tracing what happens downstream of `schedule_tx` found that its
`cursor_offset_samples`/`silent_pad_samples` (computed once, at Step 2, from the frozen
`request_received_at`) implicitly assumed audio ships immediately relative to that timestamp — but
for the common current-slot case, `target_slot` is already in the past, so the Step-6 slot-wait is a
no-op and audio actually ships whenever the worker reaches Step 7, i.e. `request_received_at` plus
however long the collection window, encoding, and gate checks actually took. That gap was small
(~800ms) and already-tolerated in production before this fix, but the wider adaptive window would
have grown it proportionally. Fix: `schedule_tx`'s pad/cursor math was split into a standalone
`pad_and_cursor_for_target(now, target_slot, sample_rate)` helper; both TX-worker arms now refresh
just the pad/cursor (never `target_slot`/`deferred` — that decision is made exactly once, per
Symptom B's existing protection) against a freshly-read clock immediately before PTT (Step 5),
using the already-decided `target_slot`. The single-TX arm's existing late-pivot logic (Step 4c)
needed no changes — it already reused `schedule`'s pad/cursor fields, so refreshing them in place
was sufficient. The multi-TX arm's fast-path/rebuild branching (from the double-73/SM2LIY-C6AVD
incident fix, PR #167) is unchanged in its own logic — only where the final audio trim happens
moved, from inside that branching to a single point right before Step 5.

Spec: `docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md`. Plan:
`docs/superpowers/plans/2026-07-21-symptom-c-adaptive-coalesce-window.md`.

## Autonomous runtime-arm gate no longer auto-arms on an interactive launch (2026-07-29)

`autonomous_enabled_runtime` (`hb-161` Phase 5 emergency-stop gate, `coordinator/mod.rs`) is the
atomic flag the autonomous decision loop checks before dispatching any `TransmitRequest`; Shift+Q
clears it, `a` in the TUI toggles it. Previously `start_autonomous_component` (`coordinator/autonomous.rs`)
seeded this gate directly from `config.autonomous.enabled` regardless of how pancetta was launched —
so an interactive TUI session with `autonomous.enabled = true` in config started already
transmitting autonomously, with no `a` keypress needed. Operator-reported (2026-07-29) as not
desirable: an interactive launch should always require an explicit arm.

Fixed by conditioning the seed on `--headless`: `seed_armed = auto_config_enabled && self.headless`.
An interactive (TUI) launch now always starts with the gate closed, requiring `a` to arm, regardless
of config — mirroring Shift+Q's disarm posture. A `--headless` launch is unaffected and still
auto-arms straight from config, since `docs/RUNBOOK.md` documents config as the *only* switch for a
supervised headless station (there is no TUI to press `a` in as an alternative — no other mechanism
exists to flip this gate at runtime). Tests:
`coordinator::autonomous::fq_f9_placement_feed_when_disabled_tests::interactive_launch_never_auto_arms_even_when_config_enabled`
and its `headless_launch_still_auto_arms_from_config_per_runbook` control.

## Hamlib restart windows hard-mute TX without changing operator policy (PAN-5), 2026-07-31

Hamlib supervisor teardown raises `tx_restart_inhibit`, a reference-counted hard-mute separate from
`TxPolicy`, before releasing PTT and tearing down rig-control resources. The inhibit spans teardown,
restart backoff, and the restart attempt, and clears by RAII on success, failure, budget exhaustion,
or unwind. Single-TX, multi-TX, supersede re-key, and manual TUI PTT paths re-check the combined hard
mute at key time; PTT-OFF remains unconditionally allowed.

The counter is deliberately not encoded in `TxPolicy`: policy records operator intent, while the
restart inhibit is transient supervisor-owned safety state. Keeping them separate preserves the
operator's `Disabled`, `RespondOnly`, or `Full` selection across both successful and failed restarts,
and permits nested safety holds without one clearer accidentally unmuting another.

## Rig CAT loss and dead audio output hard-mute TX (PAN-115), 2026-10-11

`tx_hard_mute_reason` takes two more inputs: `rig_conn_state` and a coordinator-owned
`audio_output_alive` flag. TX is refused with `rig is disconnected` while the rig state is
`PollingFailed` (10 failed 500 ms polls, the point where the RIG badge already flips), and with
`audio output device is dead` while the audio output is not calling back. Both follow PAN-5: they are
health-owned safety state ANDed into the single hard mute, never written into `TxPolicy`, so the
operator's own policy survives every loss and recovery, and both clear on their own.

Priority: restart inhibit, then loop-not-ready, then rig disconnected, then audio dead, then pending
CAT state, in-flight CAT command, and `Disabled`. A supervised Hamlib restart therefore still reads
`rig control is restarting`. Hardware faults outrank CAT bookkeeping because they explain more.

Only `PollingFailed` mutes. `NotConnected` also means mock rig, rig control disabled, and not tried yet,
so muting on it would break mock-rig and `--no-audio --test-tx` sessions. A real rig whose first
connect fails still reaches `PollingFailed` after 5 s, because the poll task runs regardless.
Rejected: a new `rig_enabled` atomic, since `PollingFailed` is only ever stored under `rig_enabled`.

The Hamlib poll loop now redials rigctld while the client reports Disconnected (2 s, doubling to a
30 s cap). This is required, not a convenience. `RigctldClient::get_status()` only reads a cached
flag, and before PAN-115 the only thing that redialled a dropped socket was the TX worker's own
`SetPtt`. With PTT refused, nothing would ever reconnect and TX would stay muted for good. Rejected:
a `rig_conn_state` check in the Hamlib `SetPtt` consumer, which would block that same redial during a
race; upstream gates already cover it.

Audio liveness comes from the cpal output callback, which does one relaxed `fetch_add` per run (no
clock read or lock on the RT thread). An `OutputWatchdog` on the audio thread turns the count into
alive/dead: dead immediately on a stream error, after 2 s without a callback (the RX watchdog's
budget), and alive again only on observed progress after a reopen. The flag fails closed: it is stored
`false` before the audio thread starts and a Drop guard stores `false` on any thread exit. No-device
modes (`--no-audio`, stub, replay) have no output to judge and keep the flag `true`. Rejected:
reusing the RX-input `health_audio_alive` flag, which says nothing about the output device.

Single TX and bundles also re-run the gate synchronously just before Step 4d-arm, closing the window
between the post-wait gate and `SetPtt` for faults that land in it. No mid-transmission abort: a fault
that starts while PTT is already keyed is still ended by the existing PTT watchdog.

## Out-of-band TX is hard-refused by the band-plan gate (PAN-118), 2026-10-14

The configured band plan (`[rig.frequency.band_plan]`) was display-only: it fed exactly one consumer,
the TUI's manual frequency-entry warning modal. The coordinator's TX gate never consulted it, so a
transmission whose absolute RF fell inside no amateur allocation (reproduced with the dial parked at
15.000 MHz — between 20 m and 17 m) still keyed `SetPtt{true}`. PAN-118 reverses the 2026-07-25
band-plan design's explicit non-goal ("no hard TX block — stays a soft, once-per-session warning"):
the plan is now load-bearing for TX safety.

**Sibling gate, not a ninth parameter.** Every input to `tx_hard_mute_reason` is global,
frequency-independent state ("may the station key at all"); band-plan compliance is per-request —
two bundle items at different audio offsets can straddle an edge at the same instant. Folding it in
would force the one frequency-less consumer (the QSO stall-evidence closure, `qso.rs`) to grow a
parameter it cannot have. Instead `tx_band_plan_mute_reason(tx_rf_hz, &band_plan)` is evaluated
ALONGSIDE the shared gate at every PTT-on decision point: single-TX Step 0 / post-wait /
last-instant, bundle Step 0 / Step 4b key-time / post-wait / final, TuneRequest (admission and
last-instant), and manual PTT
(`ptt_band_plan_refusal`, a sibling of `ptt_on_refusal`). The boolean core stays the single
`pancetta_tui::app::tx_rf_out_of_band_plan` implementation (region tables + `custom_bands`
override) shared with the TUI modal; the dial-vs-split resolution stays the single
`effective_tx_dial`, wrapped by `tx_rf_for_gate` (effective dial + absolute audio offset).

**Dial-0 sentinel skips the check.** `operating_frequency_hz == 0` is the codebase's "frequency not
yet known" state (mock rig, rig-disabled dev modes, `--replay` seed); refusing there would
permanently block TX in exactly the modes where PTT reaches no radio, and churn every worker-gate
test that legitimately never sets a dial. Production rig paths seed a real dial at startup (the
Hamlib poll loop's initial `get_frequency`, which startup blocks on), and PAN-115 already refuses
while the CAT link is `PollingFailed`, so the skip's fail-open window has no RF behind it. Rejected:
fail-closed on 0.

**`edge_warnings` is the single master switch** for warn + enforce — no new config fields, no `enforce`
flag. Before PAN-118 the field only suppressed the TUI modal; `edge_warnings = false` now also
disables the hard refusal, so no working config stops working (the documented escape hatch for a
station deliberately parked outside the FT8 tables). Config still loads once at startup; the worker
snapshots `BandPlanConfig` at start, which keeps the last-instant re-check synchronous (atomics + a
local struct; "nothing else runs between this call and the key" still holds).

**Per-item bundle semantics, with a physics boundary.** At bundle Step 0 (items still separate) and
the Step 4b key-time mask (where the existing partial-liveness machinery re-encodes the surviving
subset), an out-of-privilege item is dropped and reported while in-privilege siblings still key —
mirroring `bundle_live_mask`. At the two post-sum sites (post-wait re-key and final pre-PTT), the
waveform is one summed buffer: an out-of-privilege item's audio energy cannot be individually
extracted, so ANY out-of-privilege item holds the whole bundle (nothing keys; every item reported
unaired) — the same whole-bundle hold the existing final liveness check applies to a stale item.

**Bound: the gate reads the dial atomics, not confirmed RF.** `tx_rf_for_gate` reads the shared
`operating_frequency_hz` / `split_tx_frequency_hz` atomics, which can disagree with the rig around a
commanded QSY in both directions. The TUI relay's `SetFrequency` / `SetSplit` handlers and the
autonomous band hop store the requested value before enqueuing the CAT command, and the Hamlib poll
loop stores the rig's current reading every tick, so it can revert a requested value while that
command still sits in the queue. A key-time check in either window gates on a frequency the rig is
not keyed on: FIFO order applies the queued command before the later `SetPtt`, and the
pending/in-flight mute covers a command only once the Hamlib loop is applying it (or while it waits
in a restart pending slot). Closing this needs a confirmed-RF source (the rig's split is never read
back) or in-flight marking from the requested-value store through CAT application. That producer
ordering predates PAN-118 and is follow-up scope.

The QSO stall-evidence closure (`qso.rs`) is deliberately untouched; whether band-plan refusals
should suppress stall-escape counting is follow-up scope. The `coord_sim` fixture mirrors the gate
in its `drive_slot` chain using the re-exported real gate functions, exactly as it does for the
coalescer and arm gate.

## Coordinator-level QSO sim harness

`pancetta/tests/coord_sim.rs`: a durable, reusable `CoordSim` fixture that exercises the *coordinator's* TX gate + mock-rig PTT + multi-stream path, complementing the engine-level state-machine harness in `pancetta-qso/src/sim.rs`. It stands up a real `MessageBus`, a real `QsoManager`, a real `MockRig` behind a hamlib consumer (mirror of `coordinator/hamlib.rs`'s `SetPtt` handling), and the *real* shared `active_tx_qsos` set + `tx_policy` atomic. A scenario starts/advances QSOs via the manager's real entry points (`respond_to_cq_with` / `respond_to_caller` / `start_cq`), calls `pump_qso_events()` (a faithful mirror of `coordinator/qso.rs`'s populater — insert on `StateChanged→active`/`QsoCompleted`, remove on `Failed`/`QsoFailed`, forward `MessageToSend`→`TransmitRequest`), then `drive_slot(pending)` which replicates the worker's keying-decision chain — **policy hard-mute → coalesce (real `coalesce_transmit_requests`) → Step-4b gate (real `tx_qso_is_live` over the real set) → key/audio/unkey** — sending real `RigControl(SetPtt)` over the bus to the mock rig and **asserting at the rig level** (`mock.get_ptt() == On`, offset, release). **Determinism**: no `schedule_tx` UTC math and no slot sleep (that's unit-tested in `tx.rs::schedule_tx_tests`); only bounded ms `await`s — pass/fail never depends on wall-clock slot phase. A `Timeline` (keyed / dropped / per-slot offsets, mirroring `sim::Timeline`'s style + Display) carries the readable assertion helpers. Permanent scenarios: PTT-keys-for-scheduled-QSO (StateChanged-at-start fix), stale-TX-dropped-after-supersede (no PTT), coalesce-backlog (newest wins, older not keyed), two-simultaneous-QSOs on distinct freqs, TX-policy Disabled=silent / RespondOnly=in-progress-keys-but-initiation-suppressed / Full=keys, requested-offset-honored, manual-send-never-gated. Made testable by re-exporting `coalesce_transmit_requests` / `CoalesceEntry` / `resolve_required_parity` and widening `active_tx_qso_key` / `tx_qso_is_live` to `pub` from `coordinator/mod.rs` (visibility-only, behavior-preserving).

## Frames that never aired hand their keep-call bookkeeping back (PAN-75)

Every TX completion goes through `report_transmit_complete`. It preserves the
Autonomous completion and mirrors failures with a QSO id as
`QsoMessage::TxFrameNotSent` to the QSO component. That component calls
`QsoManager::roll_back_unsent_attempt`, reversing the matching keep-call's
`call_count`, `last_call_at`, rearm `stall_cycles` increment and `Sent` entry.
The one exception is the Step 0-dup pivot-duplicate drop
(`report_pivot_duplicate_dropped`): its payload already aired through the
earlier request's pivot, so the QSO keeps that attempt and the worker instead
reports the text the pivot replaced, which never aired as itself. The report
also carries the replaced frame's offset and, for single-frame requests, its
remote binding, because a pivot can change only those and leave two same-text
emissions. The rollback undoes the newest same-text emission that carried that
payload. If none did (an opening call or forward advance was replaced; those
have no ledger entry), nothing is rolled back, so the replaced frame stays
counted rather than risk removing the frame that aired.
A pivoted frame or multi-TX bundle refused before it airs (pre-PTT hard mute,
liveness or arm recheck, disarm, pre-slot clock jump) clears its pivot tombstones, so the
request that produced each pivoted frame stays queued and accounts for
itself; the worker reports the replaced frame to the QSO engine instead.
Autonomous still gets a failed completion for the pivoted payload, as on Step
0-dup; correlating it with the queued request's eventual outcome is PAN-185. An F8 abort keeps the
tombstone, so Step 0-dup later reports the replaced frame. Backlog coalescing
consumes pivot duplicates before it picks the newest request per QSO, so a
duplicate superseded in the backlog still reports its replaced frame. Multi-TX bundle items
carry no binding, which matches bundle pivots comparing only text and offset.
If a rollback takes `stall_cycles` from the switch threshold back below it
before the coordinator's drain commits a stall offset action that attempt
raised, the commit is refused (`OffsetActionStallRolledBack`) until the streak
reaches the threshold again. A rollback that arrives after the commit cannot
undo the move (follow-up: PAN-182).
Rearm, explicit resend and operator offset-switch retransmits record attempts.
A forward advance or committed offset switch clears the remembered stall
contributions too, so a delayed failure cannot subtract from a newer streak.
Relinking stamps preserves any intervening forward advance.

The ledger keeps a separate `last_emitted_at` anchor. Rearm pacing uses the later
of that anchor and `last_call_at`; rollback never changes the anchor. TX cadence,
coalescing order, parity and frequency selection therefore stay unchanged.

Correlation uses the newest matching `(qso_id, message_text)` within four slots,
with at most eight attempts per QSO. A same-text emission arriving between TX
drain and rollback can be matched instead, so its timestamp may differ by one
emission; the count still reverses one attempt. When the newer same-text
entry is an explicit resend, its flags are reversed instead of the failed
frame's (follow-up: PAN-183). Expired or unmatched reports and
terminal QSOs are no-ops. The ledger is shared across manager clones, pruned on
rearm and rollback, and never persisted.

The same accounting rule covers cap truncation, F8 abort, supersede, arm or
identity refusal, hard mute, PAN-141 hold, PAN-114 clock-jump drops, and
encode/PTT failure. This supersedes the PAN-141 spec's accepted tradeoff that a
held cycle spends a call. Existing time watchdogs and caps for counted calls
still apply. A failure can also mean an interrupted, partial frame; it is not
counted as a completed transmission.

The manual call-count cap waits for the frame that reached it. While any
keep-call attempt for the QSO is still inside the four-slot rollback window,
`check_timeouts_at` does not retire it on the call cap; rearm already stops at
the cap, so nothing new airs meanwhile and the time watchdog still applies. If
that final frame is reported unsent, `call_count` drops below the cap and
keep-calling resumes. Before this, the watchdog retired the QSO on the same
tick its re-call reached the cap, so the cap-reaching frame never aired and an
unsent report for it found no active QSO. Retirement on the call cap now lands
up to four slots after the last call.

Rollback reports are best-effort bus messages; a full or missing QSO channel
loses them (follow-up: PAN-185).

Shutdown aborts deliberately send no rollback. The TX and QSO loops stop on the
same global flag, and shutdown persists no active-QSO state. Adding a bus await
on that exit path would not preserve any observable accounting.

The opening call remains counted as one. Manual-regression re-sends and silently
coalesced, duplicate, parity-excluded or frequency-excluded frames keep their
existing accounting because those paths have no matching ledger entry or QSO
failure report. A single-frame supersede reports the frame it abandons whether
it re-keys (`Replace`) or is too late to (`NotViable`). A superseding request a
supersede drops (too late to re-key, a bundle a single-frame re-key cannot fold,
or over the cap of a multi-TX bundle) is not reported and stays counted. A
multi-TX supersede reports in-flight items it abandons as too late
(`NotViable`) or on an identity conflict. On `NotViable` it skips items the
re-enqueued bundle carries unchanged under the same authorization, and reports
the rest with their own offset (plus the old binding after an authorization
change) so the rollback cannot hit the replacement's attempt. Offsets within
0.5 Hz count as unchanged, and items count as carried only once the
re-enqueued bundle is confirmed queued. Rollback emits no new snapshot or QSO event; the existing status panel
and remote snapshot fields show corrected values on the next normal push.

## Fox-admitted QSOs keep their TX slots after Fox mode is turned off (PAN-76)

Turning Fox mode off (`SetFoxMode { on: false }`) clears `fox_mode` and
cancels only `CallingCq` QSOs. The Hound exchanges Fox mode admitted keep
running, but the TX worker's base ceiling (`effective_max_concurrent_qsos`)
falls straight back to `autonomous.max_concurrent_qsos`. Before this fix the
next same-parity backlog kept one Hound and truncated the rest every slot
until they timed out. A floor at `active_tx_qsos.len()` was tried and reverted
in PR #348 rounds 4-5: that set cannot tell a Hound from an ordinary over-cap
manual QSO, so the floor let a manual pileup through.

**What the set holds.** The coordinator owns
`fox_admitted_qsos: Arc<RwLock<HashSet<String>>>` (canonical
`active_tx_qso_key`s). The QSO component writes it at the only two places Fox
mode opens QSOs: the Fox CQ the engage arm starts, and every caller
`maybe_answer_caller` answers while its capacity check saw `fox_mode` set. The
CQ is included because a Hound calling within 15 Hz of it
(`FREQ_TOLERANCE_HZ`) converts the CQ QSO itself into that Hound's exchange,
and `maybe_answer_caller` then skips the Hound. No manual open path
(`StartQso`, `RespondToCaller`, `EngageHound`, `StartCq`, `CallStation`) or
the autonomous operator writes it.

**Lifecycle.** Entries leave at exactly `active_tx_qsos`'s removal points: a
`StateChanged` into `Failed`, `QsoFailed`, and the end of the 45 s
completed grace (`COMPLETED_QSO_REWORK_GRACE`). The grace matters because a
completed Hound's final RR73/73 airs inside it. A Fox CQ the disengage
cancels also leaves the set right there, before the forwarder sees the
cancel; otherwise an immediate re-engage reads it as a Hound in its grace.

**Coalescer rule.** `coalesce_backlog_into` passes the unraised ceiling plus
the set to `coalesce_transmit_requests_with_fox_priority`. Fox-admitted
streams are ordered before the other QSO-keyed streams, so they anchor the
bundle parity and win frequency-conflict tie-breaks, and the QSO cap is raised
to the number of Fox-admitted streams present in this backlog. Only ordinary
streams are truncated (and reported unsent through PAN-75). The cap counts
presence, never the set's size: Hounds idling in their grace window with no
frame would otherwise lend their slots to an ordinary pileup, which is round
5's regression in a narrower form. With no Fox-admitted stream present the
outcome equals `coalesce_transmit_requests`.

**Supersede ceiling.** The four mid-TX supersede sites pass
`supersede_max_concurrent_qsos(effective, live_fox_admitted_count(..))`,
where live means in both the Fox set and `active_tx_qsos`. The ceiling is the
live Fox-admitted count on top of the normal cap, the coalescer rule's shape,
so a mixed in-flight bundle (Hounds plus ordinary QSOs admitted under the
normal cap) stays under it and a Hound's fresh frame folds in. A ceiling of
only the larger of the two collapsed that bundle to the refreshed stream and
later keyed the stale Hound frame. A supersede never adds a QSO stream
(`classify_incoming_during_tx` supersedes only for an in-flight QSO or a
manual send), so the ceiling never admits more than the coalescer already did.

**Fail toward today.** A poisoned `fox_admitted_qsos` lock reads as empty and
`live_fox_admitted_count` returns 0 if either lock is poisoned, so TX never
becomes more permissive than the plain cap. Stale keys (a QSO that went
terminal before the insert, or a Qso-component restart) are inert for the
same presence/intersection reasons and are not pruned.

**Why not tear Hounds down on disengage.** A Hound that has sent its R-report
is one RR73 from a logged contact; cancelling it costs the chaser the QSO.
Letting the existing watchdog and grace drain each Hound needs no new
teardown policy, and the protection ends on its own as entries leave the set.

**Re-engage bound.** A Hound that converts the Fox CQ skips
`maybe_answer_caller`'s cap, so each extra Fox CQ could add one protected
stream. Turning Fox mode back on before earlier Hounds finish, or engaging it
twice, used to start another CQ every time; enough cycles grew the tracked
population past `MAX_RETAINED_TX_STREAMS` (8), and the coalescer's hard clamp
then cut a protected Hound. The engage arm now counts the Fox-admitted QSOs
first (`fox_admitted_split`). It starts no CQ while a Fox CQ is still calling,
or while Fox-admitted Hounds already number `fox.max_streams` or more. Those
Hounds are the active ones plus completed ones still in their 45 s grace:
they have left `get_active_qsos()`, but their final RR73/73 may not have aired,
so a fresh set of Hounds on top of them could still hit the clamp.
`fox_grace_hound_count` reads the grace from the QSO manager's own record (a
completed QSO stays in its map with `completed_at`; cancelled and failed ones
leave it), not from `active_tx_qsos`: a Hound opened already complete counts
the moment it opens, before the event forwarder runs, and a stale Fox-set key
cannot hold the CQ off. `fox_mode` still turns on, and the status line says no new CQ was
started. `maybe_answer_caller`'s Fox capacity check counts the same
grace-period Hounds (`fox_grace_hound_count`) on top of
`active_caller_qso_count`, so a CQ started while some are finishing cannot be
followed by a full fresh set of Hounds. Together these hold TX-live
Fox-admitted QSOs to `fox.max_streams + 1` (at most 8), the same as a single
engage.

**PAN-78.** PAN-78's admission cap decides whether a new QSO may open; PAN-76
decides which open QSOs keep their TX slots. While Hounds finish, a new
ordinary QSO is refused at admission (with PAN-78) or held at TX by the
coalescer rule (without it), so an ordinary pileup stays capped either way.

Tests: `coordinator::tx::coalesce_tests::pan76_*`,
`coordinator::tx::fox_admitted_ceiling_tests`,
`coordinator::tx::supersede_rekey_tests::pan76_supersede_refresh_of_one_hound_bundles_with_the_other_two`,
`coordinator::qso::fox_admitted_tracking_tests`, and the real-component
`coordinator::qso::respond_to_caller_admission_tests::pan76_fox_disengage_keeps_admitted_hounds_on_air`
(AC1), `pan76_unanswered_fox_cq_leaves_the_fox_set_on_disengage`,
`pan76_never_fox_manual_pileup_is_still_capped` (AC2),
`pan76_fox_reengage_starts_no_cq_while_finishing_hounds_fill_the_cap`,
`pan76_fox_reengage_counts_hounds_in_their_completed_grace`,
`fox_admitted_tracking_tests::pan76_hounds_in_their_completed_grace_fill_the_fox_cap`,
`pan76_stale_fox_keys_do_not_fill_the_fox_cap`
`supersede_rekey_tests::pan76_supersede_refresh_of_a_hound_in_a_mixed_bundle_keeps_every_stream`
and `pan76_fox_off_and_straight_on_starts_a_fresh_cq`.

## UTC re-anchoring after sleep/wake and clock steps (PAN-114, 2026-10-10)

PAN-72 had already replaced the autonomous loop's one-shot `interval_at` anchor with a 250 ms poll
that re-reads `chrono::Utc::now()`, so a **forward** jump re-anchored on the next poll. A
**backward** step still broke three components. Measured on `0823dd2` with scratch reproductions:
0 autonomous slot ticks in 599 s after a 600 s step (expected ~40); the next DSP decode window
613 s away; the TX worker's Step 6 sleeping 600.08 s with PTT keyed; and after a forward jump
during the pre-PTT wait, a frame keyed 4.50 s into its 12.64 s waveform (5 s jump) or skipped as an
"internal scheduling error" (1 h jump).

**One invariant, one classifier.** Every slot deadline is "the next boundary strictly after some
`now`", so a healthy deadline is never more than one period ahead of the wall clock and a consumer
polling it is never more than one period late. Anything outside that band is a wall-clock jump.
`pancetta-core/src/slot_clock.rs::classify_deadline_jump` returns `WallClockJump::Backward`/`Forward`
outside the band and `None` inside it, so no-jump timing stays byte-identical (FT8 invariant). The
same module holds `ClockStepDetector` (wall-vs-monotonic divergence ≥ 250 ms; NTP slew is ≤ 0.5 ms/s)
used by the clock-skew monitor.

- **Backward threshold is exactly one period** (D2). A healthy deadline is ≤ one period ahead, so the
  check never fires without a jump. Re-firing a wall-clock label after a backward step is harmless:
  `AutonomousOperator::decide()` re-reads `Utc::now()` and the slot manager only checks parity.
- **Autonomous tick** (`coordinator/autonomous.rs`): `poll_slot_deadline_mode_aware` re-anchors a
  deadline more than one *tracked* period ahead (after the existing PAN-72 mismatch branch).
  `SlotTicker` replaces the two loose locals and classifies against the deadline's own period, so a
  FT8 → FT4 switch is never reported as a jump. On a forward jump the tick clears `slot_messages`
  before the cqdx spot report and the operator feed (D3): those decodes are at least a slot old.
- **DSP decode trigger** (`coordinator/dsp.rs::reanchor_decode_window`): re-anchors on either jump.
  A forward jump also flushes `ft8_buffer` like a band change (D4), because the buffer spans the
  discontinuity; `bin_history` is kept. A backward step only re-anchors (the audio is continuous).
  Because the window (12.64 s FT8, 5.04 s FT4) is always shorter than the slot, a buffer refill after
  a flush can never be misread as a jump; an audio start-up delay of more than (slot − window) can
  cost one extra flush of a window whose boundary is already more than a slot stale.
- **TX worker pre-PTT wait** (`coordinator/tx.rs::wait_for_ptt_instant` / `ptt_wait_step`, both
  arms' Step 4): re-reads the wall clock every ≤ 50 ms chunk (D5). It drops the frame only if the
  PTT instant is now more than two periods away (backward step; `schedule_tx` never targets
  further), or if the wait started before the PTT instant and finished more than
  `tx_late_max_ms_effective` past `target_slot` (forward jump). A request that was already a late
  start when the wait began keeps the old path exactly; a forward jump that lands inside the
  late-start policy ships as a policy late start.
- **PTT hold bound** (`ptt_hold_exceeds_bound`, both arms' Step 6, D6): if `to_slot` exceeds
  `ptt_lead_ms` + `PTT_HOLD_SLACK_MS` (1000 ms) with PTT keyed, PTT is released first (the
  `AbortedByDisarm` pattern, including the pivot-tombstone cleanup) and the frame is dropped.
  Nothing reassigns `schedule` between Step 4 and Step 6, so the bound only trips on a step.
- **Drop reporting** (`drop_frames_for_clock_jump`, D7): target `tx.policy`, Warn, text
  "dropping stale TX after a {±secs} s clock jump: '…' — next frame uses the current UTC slot", so it
  counts in the TUI's session TX-drops tally next to the existing stale-TX drops, plus one failed
  `TransmitComplete` per frame (through `report_transmit_complete`) so a self-CQ rolls back and the
  frame's keep-call accounting is reversed (PAN-75) while the attempt is inside the four-slot
  rollback window; a longer forward jump leaves it counted. The QSO engine re-emits on its own timers.

Out of scope: no QSO teardown on a clock step (the engine's timeouts retire stalled QSOs); no
queued-call TTL change (it is `std::time::Instant`-based and immune to NTP steps); no station-wide
median-DT clock estimate (it mixes our clock error with audio latency and other stations' errors);
no TX inhibit on large skew (a product decision for its own ticket). The in-session skew monitor is
recorded in `config-and-platform.md`, its title-bar chip in `tui.md`.
