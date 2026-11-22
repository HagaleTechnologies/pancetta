//! Moving a QSO's frequency.
//!
//! `apply_tx_offset_switch` moves our TX offset for one QSO, after a stall or
//! an operator nudge. It rechecks its guards while holding `ptt_sync_gate`
//! and bumps the QSO's relocation epoch. `maybe_confirm_frequency_drift` and
//! `maybe_confirm_frequency_drift_at` relatch a partner's frequency only after
//! a second sighting at the new frequency. `tx_separation_warning` reports a
//! relatched partner within `MIN_TX_SEPARATION_HZ` of our TX offset. The
//! epoch type, `TxRelocationEpochs`, stays in `qso_manager.rs`.
//!
//! PAN-375 moved these methods here from `qso_manager.rs` unchanged. A method
//! that another file calls is `pub(super)`, the scope it had as a private
//! method of `qso_manager.rs`.

use super::*;
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;

impl QsoManager {
    /// PAN-15 item 3: returns `Some(separation)` when a candidate relatched
    /// `partner_freq` has drifted within [`MIN_TX_SEPARATION_HZ`] of our own
    /// TX offset — the caller should warn, since we may now be keying
    /// directly on top of the station we're trying to hear. Returns `None`
    /// when the separation is still adequate. Pure so the boundary can be
    /// unit-tested without a `QsoManager` instance.
    pub(super) fn tx_separation_warning(new_partner_freq: f64, our_tx_freq: f64) -> Option<f64> {
        let separation = (new_partner_freq - our_tx_freq).abs();
        (separation < MIN_TX_SEPARATION_HZ).then_some(separation)
    }

    /// Two-strike confirm-before-relatch at the current time. See
    /// [`Self::maybe_confirm_frequency_drift_at`].
    pub(super) async fn maybe_confirm_frequency_drift(
        &self,
        message_type: &MessageType,
        frequency: f64,
    ) {
        self.maybe_confirm_frequency_drift_at(message_type, frequency, self.now_utc())
            .await;
    }

    /// Two-strike confirm-before-relatch at an explicit time (for testability,
    /// mirroring [`Self::check_timeouts_at`]'s pattern): track a pending off-latch
    /// frequency candidate per QSO, and only relatch once a SECOND message from the
    /// same identified partner repeats at the same new frequency AT LEAST 5 real
    /// seconds after the candidate was FIRST noted. Ordinary Tx=Rx QSOs relatch both
    /// `metadata.frequency` and the active state's embedded `frequency`; non-Hound
    /// split-TX QSOs relatch only `metadata.partner_freq`, preserving our deliberate
    /// TX offset. Runs BEFORE the existing
    /// `find_qsos_for_message`/`is_message_relevant` routing, which stays completely
    /// unmodified — see
    /// `docs/superpowers/specs/2026-07-27-qso-frequency-relatch-v2-design.md`.
    ///
    /// A single off-frequency identity-matching message can't be safely distinguished
    /// from a spoofed frame claiming the partner's callsign (FT8 decoded text carries
    /// no cryptographic identity) — see
    /// `adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`. Only
    /// a REPEATED match at the same new frequency is trusted — and "repeated" must mean
    /// a genuinely separate transmission, not a second decode-pipeline copy of the same
    /// one. The hb-091 scoped fast-path decodes the same audio window twice and forwards
    /// both copies here (this pre-pass runs before the dedup that
    /// `is_message_relevant` used to provide), landing within milliseconds of each
    /// other, so a bare "matches an existing candidate" check is satisfiable by ONE
    /// physical transmission. The 5s floor is comfortably below FT4's 7.5s slot period
    /// (the shortest slot this app supports), so two genuinely separate transmissions
    /// always clear it while two decode-pipeline copies of one transmission never do.
    ///
    /// A same-frequency sighting that arrives again WITHIN the 5s gap does NOT reset
    /// the pending candidate's timestamp — it is presumed a duplicate delivery of the
    /// same transmission, and the ORIGINAL timestamp is preserved untouched. If the
    /// timestamp were reset on every near-duplicate, repeated fast deliveries (spoofed
    /// or an artifact of the decode pipeline) could push confirmation out indefinitely;
    /// a real DX repeating naturally will eventually land >=5s after the ORIGINAL
    /// sighting and correctly confirm.
    pub(super) async fn maybe_confirm_frequency_drift_at(
        &self,
        message_type: &MessageType,
        frequency: f64,
        now: UtcTimestamp,
    ) {
        // Reuses the module-level `ESTABLISHED_FREQ_TOLERANCE_HZ`/
        // `FREQ_TOLERANCE_HZ` shared with `is_message_relevant`/
        // `classify_relevance` (PAN-15 item 6 hoisted all three sites onto
        // one definition — previously each redeclared its own local copy of
        // the same values). `DRIFT_CONFIRM_TOLERANCE_HZ` keeps its own name
        // here since it means something distinct ("counts as the same spot"
        // for two-strike confirmation) even though the value matches
        // `FREQ_TOLERANCE_HZ`.
        const DRIFT_CONFIRM_TOLERANCE_HZ: f64 = FREQ_TOLERANCE_HZ;
        // Confirming sighting must land >=5 real seconds after the ORIGINAL candidate
        // timestamp — comfortably below FT4's 7.5s slot period, so two genuinely
        // separate transmissions always clear it while two decode-pipeline copies of
        // the SAME transmission (milliseconds apart) never do.
        const DRIFT_CONFIRM_MIN_GAP: SignedDuration = SignedDuration::from_secs(5);
        // Wide RX-plausibility sanity bound for candidate ELIGIBILITY -- deliberately
        // NOT the same as TX_OFFSET_MIN_HZ/MAX_HZ (which govern where we autonomously
        // PICK a fresh TX offset, not where we're willing to consider a real decoded
        // signal a real DX). Responding to a CQ already ties our reply to wherever we
        // decoded it, unclamped, for any DX up to ~2900 Hz -- this must preserve that,
        // not narrow it. The constraint that actually matters here is the modulator's
        // transmittable envelope (`pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` =
        // 3100.0, verified to cover a 2900 Hz base plus the widest FT2 tone spread) --
        // NOT `frequency.rs`/`autonomous.rs`'s own convention, which is actually
        // 200-2800 (narrower on the top end) and governs a different concern (where
        // the autonomous allocator prefers to place a fresh CQ/answer).
        const DRIFT_CANDIDATE_MIN_HZ: f64 = 200.0;
        const DRIFT_CANDIDATE_MAX_HZ: f64 = 2900.0;

        let Some(sender) = message_type.sender_callsign() else {
            return;
        };
        if !message_type.is_addressed_to(&self.config.our_callsign) {
            return;
        }

        let mut qsos = self.qsos.write().await;
        for (&qso_id, progress) in qsos.iter_mut() {
            if !progress.state.is_active() {
                continue;
            }
            if progress.metadata.hound || progress.metadata.fox {
                // PAN-386: a Fox-role stream is pinned to its slot, so its TX
                // offset and `partner_freq` never relatch either.
                // Genuine Hound/Fox only. The Fox's RX offset is protocol-fixed for
                // the QSO's life and Hound has its own one-shot QSY mechanism.
                // `partner_freq` alone is not a Hound discriminator: ordinary manual
                // QSOs also set it after an offset hold, collision nudge, or clamp.
                continue;
            }
            let Some(their_callsign) = progress.state.their_callsign().map(|s| s.to_string())
            else {
                continue; // Pre-establishment (CallingCq/Idle) — not this mechanism's scope.
            };
            if !Self::is_partner(sender, &their_callsign) {
                continue;
            }
            let Some(qso_freq) = progress.state.frequency() else {
                continue;
            };
            // Where we expect to hear the DX. This is deliberately identical to
            // is_message_relevant's baseline so both gates agree on their location.
            let split_tx = progress.metadata.partner_freq;
            let match_freq = split_tx.unwrap_or(qso_freq);
            let distance = (match_freq - frequency).abs();

            if distance <= ESTABLISHED_FREQ_TOLERANCE_HZ {
                // PAN-142 (Codex P1 on PR #371): only emit when there was
                // actually a candidate to clear. This branch is the ordinary
                // in-tolerance path every normal exchange takes, so emitting
                // unconditionally here would flood the event stream with a
                // same-state StateChanged on nearly every decoded frame.
                if progress.metadata.pending_freq_drift.take().is_some() {
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                }
                continue;
            }

            if !(DRIFT_CANDIDATE_MIN_HZ..=DRIFT_CANDIDATE_MAX_HZ).contains(&frequency) {
                continue; // Out-of-band decode — don't let noise reset a real candidate.
            }

            let confirmed = progress.metadata.pending_freq_drift.is_some_and(|(f, t)| {
                (f - frequency).abs() <= DRIFT_CONFIRM_TOLERANCE_HZ
                    && now - t >= DRIFT_CONFIRM_MIN_GAP
            });

            if confirmed {
                progress.metadata.pending_freq_drift = None;
                if split_tx.is_some() {
                    progress.metadata.partner_freq = Some(frequency);
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_partner_freq = match_freq,
                        new_partner_freq = frequency,
                        our_tx_freq = qso_freq,
                        "confirmed partner drift on a split-TX QSO (2 consistent sightings, \
                         >=5s apart) — relatching partner_freq; our TX offset is unchanged"
                    );
                    // PAN-15 item 3: the relatched partner_freq is bounded only
                    // by the RX-plausibility window above — it's never checked
                    // against our own TX offset (`metadata.frequency`) or other
                    // active TX offsets. A DX that drifts onto (or within
                    // MIN_TX_SEPARATION_HZ of) our latched TX offset would have
                    // us keying directly on top of the station we're trying to
                    // hear, silently defeating whatever collision nudge/clamp
                    // separated them originally. No re-deconfliction is
                    // performed (stretch goal); this is the minimum-bar warn.
                    if let Some(tx_separation) =
                        Self::tx_separation_warning(frequency, progress.metadata.frequency)
                    {
                        warn!(
                            target: "qso.freq_gate",
                            partner = %their_callsign,
                            new_partner_freq = frequency,
                            our_tx_freq = progress.metadata.frequency,
                            separation_hz = tx_separation,
                            "relatched partner_freq drifted within MIN_TX_SEPARATION_HZ of our \
                             own TX offset — we may now be keying on top of the station we're \
                             trying to hear; no re-deconfliction performed"
                        );
                    }
                    // The QSO state itself is deliberately untouched here (only
                    // where we HEAR the DX moved; our TX offset did not), so
                    // old_state == new_state. We still emit, because the
                    // coordinator refreshes its scoped/AP decoder hint
                    // (`active_qso_freq_hz`) only in its `StateChanged` handler,
                    // re-reading `metadata.partner_freq` there. Without this the
                    // decoder stays centred on the obsolete partner offset while
                    // the relevance gate has already moved to the new one —
                    // a confirmation arriving on a frame that leaves the state
                    // unchanged (e.g. a repeated `SignalReport` in
                    // `SendingReport`) has no other transition to piggyback on,
                    // so weak replies at the relatched frequency would lose
                    // narrow-band/AP recovery until some unrelated transition.
                    // The handler is idempotent for a same-state event: it
                    // re-inserts into `active_tx_qsos`/`active_tx_offsets` (both
                    // keyed on the unchanged TX offset), recomputes AP context,
                    // and re-pushes the TUI snapshot. It triggers no TX.
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                } else {
                    let old_state = progress.state.clone();
                    progress.metadata.frequency = frequency;
                    match &mut progress.state {
                        QsoState::RespondingToCq {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingReport {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::WaitingForConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::SendingConfirmation {
                            frequency: state_freq,
                            ..
                        }
                        | QsoState::Contest(ContestState::ExchangingInfo {
                            frequency: state_freq,
                            ..
                        })
                        | QsoState::Contest(ContestState::ContestCompleted {
                            frequency: state_freq,
                            ..
                        }) => {
                            *state_freq = frequency;
                        }
                        _ => {
                            debug_assert!(
                                false,
                                "confirmed drift relatch has no frequency-field mutation arm for \
                                 this QsoState variant — metadata.frequency and the state's own \
                                 frequency field are now desynced"
                            );
                        }
                    }
                    info!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        old_freq = qso_freq,
                        new_freq = frequency,
                        "confirmed frequency drift (2 consistent sightings, >=5s apart) — \
                         relatching QSO"
                    );
                    self.emit_state_change(qso_id, old_state, progress.state.clone())
                        .await;
                }
            } else {
                // Only start (or leave untouched) a pending candidate — see the
                // "duplicate delivery" doc comment above for why an existing
                // candidate's timestamp is never bumped forward here.
                if progress
                    .metadata
                    .pending_freq_drift
                    .is_none_or(|(f, _)| (f - frequency).abs() > DRIFT_CONFIRM_TOLERANCE_HZ)
                {
                    progress.metadata.pending_freq_drift = Some((frequency, now));
                    debug!(
                        target: "qso.freq_gate",
                        partner = %their_callsign,
                        candidate_freq = frequency,
                        latched_freq = match_freq,
                        "identity-verified message outside tolerance — noting drift candidate \
                         (needs 1 more confirming sighting >=5s later)"
                    );
                    // PAN-142 (Codex P1 on PR #371): a new/changed candidate
                    // is the ONE event the operator-visible drift indicator
                    // exists to surface — without this, `pending_freq_drift`
                    // is set here but nothing re-triggers the TUI snapshot
                    // that carries it (this mechanism's own state-machine
                    // state is deliberately untouched, so no OTHER event
                    // naturally fires), and the indicator built for exactly
                    // this "am I stuck or about to self-heal" moment would
                    // never actually render it. Mirrors the confirmed-split-TX
                    // branch's own same-state emit above.
                    let state = progress.state.clone();
                    self.emit_state_change(qso_id, state.clone(), state).await;
                }
            }
        }
    }

    /// External commit point for PAN-72's stall-switch/revert and the manual
    /// nudge keystroke — the only way outside code may change an
    /// already-active QSO's TX offset. Mirrors the mutation the removed
    /// stuck-DX hop used to perform inline; the caller (the coordinator) has
    /// already resolved WHAT the new offset should be (via the smart
    /// allocator for a Switch, or `last_known_good_offset_hz` for a Revert —
    /// `QsoManager` itself never calls the allocator, see the single-scorer
    /// invariant). Does not force an immediate retransmission — the next
    /// naturally-scheduled send picks up the new value since message
    /// construction reads `metadata.frequency` fresh each time.
    ///
    /// The requested offset is clamped defensively to
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]..=[`ACTIVE_QSO_TX_OFFSET_MAX_HZ`] —
    /// this is a `pub` method documented as the sole external mutator of an
    /// active QSO's TX offset, so it must not be able to park a QSO outside
    /// the transmittable envelope from outside the crate. Every current
    /// caller already supplies an in-band value.
    ///
    /// NOT [`TX_OFFSET_MIN_HZ`]..=[`TX_OFFSET_MAX_HZ`] (300–2700), the band
    /// the removed stuck-DX hop used: those govern where we autonomously
    /// *pick a fresh* offset, not where an in-progress QSO is allowed to sit.
    /// A `Revert`'s `target_hz` is a `last_known_good_offset_hz` that may be
    /// an unclamped reply offset up to ~2900 Hz (answering a CQ ties our reply
    /// to wherever we decoded the DX), and narrowing that to 2700 would move
    /// the QSO off the very offset it demonstrably worked on. See
    /// [`ACTIVE_QSO_TX_OFFSET_MIN_HZ`]'s doc comment.
    ///
    /// Returns the offset that was **actually applied** (post-clamp), so the
    /// coordinator can mirror it into its own `active_tx_offsets` snapshot
    /// without re-deriving the clamp.
    ///
    /// These guards run BEFORE anything is mutated. Guards 1–3 are staleness
    /// guards, all because the coordinator only drains its request mailbox
    /// once per 15-second slot, so a lot can happen between an action being
    /// raised and committed; guard 4 rejects a move that isn't one:
    ///
    /// 1. **Terminal QSO** (Codex round 1 on PR #350, finding 3). Completed
    ///    entries are retained in the map, and completion deliberately keeps
    ///    the QSO's id and offset in the coordinator's active snapshots for a
    ///    45-second trailing-73 grace window — so a late action still *finds*
    ///    its QSO. `QsoState::set_frequency` already refuses terminal states,
    ///    but `metadata.frequency` has no such guard: writing it would leave
    ///    the metadata disagreeing with the logged state, and the drain would
    ///    mirror that phantom offset into `active_tx_offsets` while the queued
    ///    final frame still goes out on the original one. Refuse outright.
    /// 2. **Superseded by a DX advance** (finding 8). If the QSO's
    ///    [`crate::states::QsoMetadata::advance_generation`] has moved since
    ///    the origin's `raised_at_generation` was captured, the DX answered in
    ///    the meantime: `stall_cycles` is already reset and the CURRENT offset
    ///    is already recorded known-good, so applying the queued action would
    ///    move us off the offset that just demonstrably worked.
    ///    [`OffsetRelocationOrigin::OperatorForced`] carries no generation —
    ///    an ordinary advance must never discard the nudge this way; guard 2a
    ///    covers the advances that do supersede it.
    ///
    ///    **2a. Operator nudge superseded by DX progress** (PAN-131, Codex
    ///    round 13 on PR #350, P2). An `OperatorForced` commit is refused when
    ///    the QSO's current state fails [`operator_nudge_retransmits`]: the DX
    ///    moved it, after the keypress, into a state neither
    ///    `rearm_manual_calls_at` nor this method's one-shot retransmission
    ///    sends anything from, so committing would move the offset with no
    ///    frame ever sent on it. An advance that stays inside the
    ///    nudge-eligible states (`RespondingToCq` → `SendingReport`) still
    ///    commits — a generation token would have refused that too.
    /// 3. **Outside the live Hound region** (Codex round 4, finding 2). A
    ///    Hound's TX offset is procedurally pinned to the calling region before
    ///    the Fox answers and to the response region after the QSY, and the
    ///    coordinator picks that window from a `get_qso` snapshot taken before
    ///    the allocator runs. The QSY can land in between — and for an
    ///    operator-forced `u` nudge, which carries no generation, guard 2
    ///    cannot catch it — so the window is re-derived here from the locked
    ///    `progress` (through the same [`hound_switch_range_hz`] the
    ///    coordinator resolved with) and an out-of-region offset is refused
    ///    rather than committed. Refusing, not re-clamping: the stall evidence
    ///    survives and the detector re-raises against the correct region on its
    ///    own, whereas a clamp would park us on a region edge nothing ranked.
    /// 4. **No-op relocation** (PAN-79; Codex round 3, finding 2). The
    ///    allocator's "no valid relocation exists" signal is `avoid_hz`
    ///    returned unchanged, which reaches this method as a requested offset
    ///    equal (within [`TX_OFFSET_NOOP_TOLERANCE_HZ`]) to the QSO's current
    ///    one. Nothing relocates, so nothing may be committed: clearing
    ///    `stall_cycles` here would throw away valid accumulated stall evidence
    ///    — the operator's `u` nudge over a partial streak is the live case —
    ///    and push automatic recovery out by another full
    ///    `qso_stall_switch_after` window, while `TxOffsetApplied` would
    ///    announce a move that never happened.
    /// 5. **Hold mode, re-validated at commit time** (Codex round 10, thread
    ///    on `autonomous.rs:988`). The caller's own Hold checks run before its
    ///    `.await` for this method's write lock, so a contended lock leaves a
    ///    window in which the operator's `f` press (Auto → Hold) lands after
    ///    the caller's last check but before this commit — re-reading
    ///    `tx_freq_mode` here, inside the same locked section, closes it.
    ///
    /// All these refusals are expected outcomes, not faults — see
    /// [`QsoManagerError::is_expected_offset_action_refusal`].
    ///
    /// `origin` is also what the known-good crediting site reads back off
    /// [`crate::states::PreSwitchOffset::operator_forced`] when the next
    /// forward advance arrives — see that field's doc comment (round 6,
    /// finding 2).
    pub async fn apply_tx_offset_switch(
        &self,
        qso_id: QsoId,
        new_offset_hz: f64,
        origin: OffsetRelocationOrigin,
    ) -> Result<f64, QsoManagerError> {
        let raised_at_generation = origin.raised_at_generation();
        let applied_hz =
            new_offset_hz.clamp(ACTIVE_QSO_TX_OFFSET_MIN_HZ, ACTIVE_QSO_TX_OFFSET_MAX_HZ);
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        if !progress.state.is_active() {
            return Err(QsoManagerError::QsoNotActive { qso_id });
        }
        if let Some(raised_at) = raised_at_generation {
            let current = progress.metadata.advance_generation;
            if current != raised_at {
                return Err(QsoManagerError::OffsetActionStale {
                    qso_id,
                    raised_at,
                    current,
                });
            }
            // PAN-75: a rollback since the action was raised may have taken
            // the streak back below the threshold that justified it.
            if progress.metadata.stall_cycles < self.config.timeouts.qso_stall_switch_after
                && self
                    .keep_call_ledger
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&qso_id)
                    .is_some_and(|entry| entry.stall_action_revoked)
            {
                return Err(QsoManagerError::OffsetActionStallRolledBack { qso_id });
            }
        }
        // 2a. **Operator nudge superseded by DX progress** (PAN-131, Codex
        //    round 13 on PR #350, P2). The `u` nudge was queued while its QSO
        //    was nudge-eligible, but it carries no `raised_at_generation`, so
        //    guard 2 cannot see the DX moving the QSO on before this drain. If
        //    the QSO now sits in a state nothing retransmits from
        //    (`WaitingForConfirmation`, an answered Auto self-CQ's
        //    `WaitingForReport`, …), committing would move the offset with no
        //    frame ever sent on it. Revalidating the STATE rather than adding
        //    a token keeps the nudge alive across an advance that stays
        //    retransmittable (`RespondingToCq` → `SendingReport` bumps the
        //    generation too). Ahead of guard 6, so the drain discards this
        //    refusal instead of requeuing it.
        if origin.is_operator_forced() && !operator_nudge_retransmits(progress) {
            return Err(QsoManagerError::OffsetActionNudgeSuperseded { qso_id });
        }
        // 3. **Outside the live Hound region** (Codex round 4 on PR #350,
        //    finding 2). The coordinator picks the window from a `get_qso`
        //    snapshot taken BEFORE the allocator runs, and the Fox's first
        //    report performs the mandatory calling-region -> response-region
        //    QSY (`process_message_for_qso`'s Hound block, which flips
        //    `hound_qsyed`). An operator-forced `u` nudge deliberately carries
        //    no `raised_at_generation`, so guard 2 above cannot catch that
        //    advance — the low, calling-region offset would commit and undo the
        //    QSY, putting our R+report where the Fox is not listening.
        //    Re-deriving the window HERE, from the same locked `progress` this
        //    method is about to mutate and through the same
        //    `hound_switch_range_hz` the coordinator resolved with, makes the
        //    check and the commit atomic by construction.
        if let Some((min_hz, max_hz)) = hound_switch_range_for(&progress.metadata, &self.config) {
            if applied_hz < min_hz || applied_hz > max_hz {
                return Err(QsoManagerError::OffsetActionOutsideHoundRegion {
                    qso_id,
                    offset_hz: applied_hz,
                    min_hz,
                    max_hz,
                });
            }
        }
        let old_off = progress.metadata.frequency;
        // 4. **Nothing to relocate** (PAN-79; Codex round 3 on PR #350, finding
        //    2). The allocator returns `avoid_hz` unchanged when every
        //    candidate is excluded, and the drain passes that straight here, so
        //    a "switch" can resolve back onto the offset the QSO is already on.
        //    Committing it would clear `stall_cycles` — valid evidence, and on
        //    an operator `u` nudge over a partial streak the ONLY evidence, so
        //    automatic recovery would be pushed out by another full
        //    `qso_stall_switch_after` window — and emit a `TxOffsetApplied`
        //    announcing a move that never happened. Refuse before mutating.
        if (applied_hz - old_off).abs() <= TX_OFFSET_NOOP_TOLERANCE_HZ {
            return Err(QsoManagerError::OffsetActionNoOp {
                qso_id,
                offset_hz: old_off,
            });
        }
        // 4a. **Trigger frame's outcome unknown** (PAN-182). The frame whose
        //    emission raised this stall action may still be waiting on the
        //    TX worker, and the commit below erases the ledger evidence a
        //    later not-sent report needs to undo the streak. Hold until the
        //    worker reports the frame aired (`confirm_stall_trigger_aired`)
        //    or unsent (`roll_back_replaced_attempt_at`, after which guard 2
        //    refuses it as `OffsetActionStallRolledBack`). After
        //    `STALL_TRIGGER_HOLD_SLOTS` with no report the action is dropped,
        //    never committed; the still-standing streak re-raises it with a
        //    fresh trigger. Scoped to stall origins: an operator `u` nudge is
        //    never held. After guard 4, so a no-op still reports as one.
        let pending_trigger_at = raised_at_generation.and_then(|_| {
            self.keep_call_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&qso_id)
                .and_then(|entry| entry.pending_stall_trigger.as_ref())
                .map(|trigger| trigger.emitted_at)
        });
        if let Some(emitted_at) = pending_trigger_at {
            return Err(
                if stall_trigger_hold_expired(emitted_at, self.now_utc(), self.rearm_slot_millis())
                {
                    QsoManagerError::OffsetActionTriggerOutcomeExpired { qso_id }
                } else {
                    QsoManagerError::OffsetActionTriggerOutcomePending { qso_id }
                },
            );
        }
        // 5. **Hold mode, re-validated inside this commit** (Codex round 10 on
        //    PR #350, thread on `autonomous.rs:988`; closed for real by
        //    PAN-143 — see the `ptt_sync_gate` field's doc comment). The
        //    caller's own Hold checks (`drain_pending_qso_offset_requests`'s
        //    batch-level and per-request re-reads) run BEFORE the `.await`
        //    for this method's write lock — a contended lock leaves a real
        //    window in which the operator's `f`/`o`-modal action (Auto ↔
        //    Hold) lands after the caller's last check but before this
        //    commit runs. Re-checking here is not enough by itself (`tx_freq_
        //    mode` is written by a completely independent task with no lock
        //    shared with `self.qsos` at all) — `ptt_sync_gate` below is what
        //    actually closes the window, by making this recheck and that
        //    task's store mutually exclusive.
        //
        // 6. **PTT already keyed, re-validated inside this commit** (PAN-140,
        //    Codex P1 on PR #369; closed for real by PAN-143). Same shape as
        //    guard 5, one line down: the caller's own pre-commit check
        //    (`drain_pending_qso_offset_requests`'s top-of-loop guard) runs
        //    BEFORE the `.await` for this method's write lock, and a bare
        //    recheck of the independently-written `ptt_active` atomic here
        //    can only narrow, not close, the window against the TX worker's
        //    `PttGuard::new` store. `ptt_sync_gate` closes it.
        //
        // Both guards, the mutation they gate, AND the resulting
        // `TxOffsetApplied`/one-shot-retransmit `MessageToSend` emission
        // near the end of this function all run under one acquisition of
        // `ptt_sync_gate` — acquired AFTER `self.qsos.write()` above
        // (never before: see the design spec's lock-ordering argument),
        // held all the way to the end of the function (including across
        // this method's own `.await` emit points — an async-aware
        // `tokio::sync::Mutex`, not a blocking one, exists precisely so
        // this is safe), and dropped only on function return (every early
        // return above this point releases it too, via normal scope
        // exit). Round 3 (Codex P1): releasing it right after the
        // mutation, before the resulting intent was published, left a
        // gap in which the TX worker could key PTT for an already-queued,
        // now-stale request — reopening the exact race this exists to
        // close, just moved later in the function.
        let _ptt_sync = self.ptt_sync_gate.lock().await;
        if !pancetta_core::TxFreqMode::from_u8(
            self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_auto_change()
        {
            return Err(QsoManagerError::OffsetActionHeld { qso_id });
        }
        if self.ptt_active.load(std::sync::atomic::Ordering::Acquire) {
            return Err(QsoManagerError::OffsetActionPttInFlight { qso_id });
        }
        // PAN-145: invalidate snapshots before committing, under the PTT gate.
        self.relocation_epochs.bump(&qso_id.to_string());
        progress.metadata.frequency = applied_hz;
        progress.metadata.pending_freq_drift = None;
        progress.metadata.stall_cycles = 0;
        self.clear_recorded_stall_increments(qso_id);
        // PAN-72 (Codex round 4 on PR #350, finding 3): remember the offset we
        // are vacating, and when. The frame that triggered this move went out
        // on it — `rearm_manual_calls_at` re-sends and trips the stall
        // threshold in the same pass, and this commit only happens on the
        // coordinator's next drain — so any answer to that frame arrives at
        // `old_off` AFTER this mutation. For `PRE_SWITCH_OFFSET_GRACE` the
        // vacated offset therefore stays a valid RX baseline
        // (`is_message_relevant`) and stays a CANDIDATE for the offset a
        // forward advance credits as known-good (`process_message_for_qso`).
        //
        // Round 6, finding 2: the provenance travels with it. On any split —
        // including the one the latch below creates for an established simplex
        // QSO (PAN-132) — the DX's reply frequency cannot discriminate our two
        // offsets, and the right fallback there depends entirely on whether the
        // stall detector or the operator moved us. See
        // `QsoMetadata::pre_switch_offset` and
        // `PreSwitchOffset::operator_forced`.
        progress.metadata.pre_switch_offset = Some(crate::states::PreSwitchOffset {
            offset_hz: old_off,
            left_at: self.now_utc(),
            operator_forced: origin.is_operator_forced(),
        });
        // Mirror the Hound QSY block's state write (see `process_message_for_
        // qso`'s `QsoState::SendingReport` fix-up): several transitions —
        // `Completed` above all — are built from the PRECEDING state's own
        // `frequency` field, so updating only `metadata.frequency` would log
        // the pre-switch offset. `set_frequency` is a no-op on terminal and
        // `Idle` states by design.
        progress.state.set_frequency(applied_hz);
        // ...and because `QsoState::frequency()` is dual-purposed, that write
        // alone is not enough. Whenever `metadata.partner_freq` is `None`, the
        // state's own frequency is ALSO the RX-side baseline every relevance/
        // routing and drift gate keys on (`partner_freq.unwrap_or(qso_freq)` in
        // `is_message_relevant`, `classify_relevance` and
        // `maybe_confirm_frequency_drift_at`). This switch moves only OUR side:
        // the DX is still transmitting on the old offset. Latching the offset
        // we moved AWAY from into `partner_freq` keeps those gates pointed at
        // the DX while `metadata.frequency`/the state frequency track our new
        // TX offset — the same bookkeeping `compute_manual_tx_offset` performs
        // for the analogous case (`partner = (tx_off != dx_freq).then_some(
        // dx_freq)`), and it borrows that site's float-noise tolerance
        // ([`TX_OFFSET_NOOP_TOLERANCE_HZ`]) so a no-op switch never manufactures
        // a split — belt and braces now that guard 3 above refuses such a
        // switch outright. Without it the DX's
        // real replies fall outside `ESTABLISHED_FREQ_TOLERANCE_HZ` of our new
        // offset and stop routing to this QSO altogether, and the drift gate
        // then reads the DX's unchanged frequency as a confirmed drift and
        // relatches us straight back onto the offset the switch existed to
        // escape.
        //
        // An already-`Some` `partner_freq` (Hound, an offset hold, a collision
        // nudge, a passband clamp) is deliberately left alone: it is already
        // the correct "where we hear the DX", and this is purely a TX-side
        // move. The crediting block in `process_message_for_qso` treats the
        // split this latch creates exactly like an already-`Some` one, so its
        // provenance rule decides the known-good offset (PAN-132).
        //
        // The latch also requires an ESTABLISHED DX identity
        // (`their_callsign().is_some()` — the same discriminator
        // `is_message_relevant`/`classify_relevance` use to pick between
        // `FREQ_TOLERANCE_HZ` and `ESTABLISHED_FREQ_TOLERANCE_HZ`). On an
        // unanswered `CallingCq` there is no DX at the old offset at all: it
        // was OUR abandoned CQ frequency. Latching it would aim the RX gates
        // at dead air, and — being pre-establishment — they would judge a
        // caller answering us on our REAL (new) offset against it with the
        // tight 15 Hz bound, rejecting every answer for the life of the QSO
        // and spawning a duplicate QSO object in its place.
        if progress.metadata.partner_freq.is_none()
            && progress.state.their_callsign().is_some()
            && (applied_hz - old_off).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ
        {
            progress.metadata.partner_freq = Some(old_off);
        }
        // `info!`, not `warn!`: the removed hop fired only on an identical-
        // repeat trigger and was genuinely rare, but the silence-based
        // detector this replaced it with can fire roughly once a minute per
        // stalled QSO. That is routine adaptive operation, not a warning.
        info!(
            target: "tx.freq",
            qso_id = %qso_id,
            dx = progress.metadata.their_callsign.as_deref().unwrap_or("?"),
            "Adaptive TX-offset action: {:.0} Hz -> {:.0} Hz",
            old_off, applied_hz
        );
        // PAN-72 Fix A (Codex round 10, thread on `autonomous.rs:1829`): an
        // operator-forced nudge of an autonomous self-CQ's `CallingCq` needs
        // an IMMEDIATE one-shot retransmission at the new offset.
        // `rearm_manual_calls_at`'s periodic cadence deliberately never
        // re-sends a `CallingCq` for `CallInitiation::Auto` (its own
        // `is_manual` guard — a separate, intentional design decision this
        // fix does NOT touch), so without this the commit above would
        // silently relocate the QSO with nothing ever sent at the new
        // offset while `AutonomousOperator::current_cq_offset_hz` stays on
        // the old one. This is a narrow, one-shot exception for a
        // deliberate, operator-supervised action — never for a
        // `StallDetected` origin, which is gated out below exactly like the
        // silence-driven stall detector's own dormancy for an Auto
        // `CallingCq` remains.
        //
        // Deliberately keyed on the CURRENT state only, not
        // `CallInitiation`: for a MANUAL `CallingCq` this is a harmless
        // redundant resend layered on top of the existing periodic cadence
        // (the double-send guard below keeps it harmless).
        //
        // PAN-72 round 14 (Codex round 14 on PR #350, thread on
        // `tui_relay.rs:3116`): the SAME gap, one state later. An
        // Auto-initiated QSO in `RespondingToCq`/`SendingReport` that has
        // ALREADY exhausted `AUTO_RESEND_MAX_CALLS` still passes
        // `resolve_nudge_tx_offset`'s eligibility check (state + initiation
        // only, no cap awareness) — the relay queues the nudge and reports
        // success, but `rearm_manual_calls_at`'s own
        // `if progress.metadata.call_count >= max_calls { continue; }` cap
        // gate then skips re-arming it forever, so the metadata updates and
        // nothing is ever retransmitted. Extending the SAME one-shot
        // mechanism (not a second one) to those two states closes it —
        // narrower than the CallingCq arm above: this only fires for an
        // Auto QSO ACTUALLY at/past the cap (below the cap, the very next
        // periodic rearm pass already retransmits on its own, so firing
        // here too would be a harmless but redundant double-send) and never
        // for a Manual QSO in these states (Manual's own
        // `manual_call_max_calls`-bounded watchdog handling here is
        // untouched — hitting IT is expected watchdog-retirement territory,
        // not a "should retry" scenario).
        let cq_retransmit = if origin.is_operator_forced() {
            let is_auto_past_resend_cap = progress.metadata.initiated_by == CallInitiation::Auto
                && progress.metadata.call_count >= AUTO_RESEND_MAX_CALLS;
            let message = match &progress.state {
                QsoState::CallingCq { .. } => Some(MessageType::Cq {
                    callsign: self.config.our_callsign.clone(),
                    grid: self.config.our_grid.clone(),
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::RespondingToCq { target_callsign, .. }` arm uses
                // — see that match block.
                QsoState::RespondingToCq {
                    target_callsign, ..
                } if is_auto_past_resend_cap => Some(MessageType::CqResponse {
                    calling_station: target_callsign.clone(),
                    responding_station: self.config.our_callsign.clone(),
                    grid: self.config.our_grid.clone(),
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::SendingReport { their_report: None, .. }` arm
                // uses — see that match block.
                QsoState::SendingReport {
                    their_callsign,
                    their_report: None,
                    our_report,
                    ..
                } if is_auto_past_resend_cap => Some(MessageType::SignalReport {
                    to_station: their_callsign.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: *our_report,
                }),
                // Reuse EXACTLY the same construction
                // `rearm_manual_calls_at`'s own
                // `QsoState::SendingReport { their_report: Some(_), .. }` arm
                // uses — see that match block.
                QsoState::SendingReport {
                    their_callsign,
                    their_report: Some(_),
                    our_report,
                    ..
                } if is_auto_past_resend_cap => Some(MessageType::ReportAck {
                    to_station: their_callsign.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: *our_report,
                }),
                _ => None,
            };
            message.map(|message| {
                let now = self.now_utc();
                // Double-send guard (Fix A, step 4; extended round 14):
                // bump call_count/last_call_at exactly as
                // `rearm_manual_calls_at` would after a normal resend, so
                // the VERY NEXT rearm pass sees "at least one slot hasn't
                // elapsed yet" and does not immediately re-fire a second
                // resend on top of this one. This matters for a Manual
                // `CallingCq` (which DOES get a periodic rearm); it applies
                // uniformly to whichever arm above fired — no separate
                // bookkeeping needed per state.
                let prev_last_call_at = progress.metadata.last_call_at;
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);
                let raw_text = self.render_sent_text(&message);
                progress.messages.push(QsoMessage {
                    timestamp: now,
                    direction: MessageDirection::Sent,
                    message_type: message.clone(),
                    raw_text: raw_text.clone(),
                    signal_strength: None,
                    frequency: progress.metadata.frequency,
                });
                let attempt_id = self.next_attempt_id();
                self.record_keep_call_attempt(
                    &mut self
                        .keep_call_ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                    qso_id,
                    KeepCallAttempt {
                        attempt_id,
                        emitted_at: now,
                        raw_text,
                        prev_last_call_at,
                        bumped_stall: false,
                        tx_slot: None,
                        recorded_sent: true,
                        frequency_hz: progress.metadata.frequency,
                        remote_origin: progress.metadata.remote_origin,
                        remote_client_key_id: progress.metadata.remote_client_key_id.clone(),
                    },
                );
                (
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                    progress.metadata.remote_origin,
                    progress.metadata.remote_client_key_id.clone(),
                    attempt_id,
                )
            })
        } else {
            None
        };
        // PAN-72 (Codex round 1 on PR #350, finding 7): announce the applied
        // move so the coordinator can rebuild the `ActiveQsosSnapshot` the TUI
        // banner and the remote gateway render from. That snapshot is
        // otherwise rebuilt only on a state transition, and a stalled exchange
        // is precisely the case where no further transition arrives — the UI
        // would show the pre-switch offset through several switch/revert
        // cycles. Emitted AFTER the write guard is dropped, matching every
        // other emit site in this file.
        drop(qsos);
        self.emit_event(QsoEvent::TxOffsetApplied {
            qso_id,
            offset_hz: applied_hz,
        })
        .await;
        // The one-shot CQ retransmission (if any), also emitted after the
        // lock is released — same collect-then-emit-after-lock-drop pattern
        // `rearm_manual_calls_at`'s own `to_recall` uses.
        if let Some((
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id,
        )) = cq_retransmit
        {
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
                tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id: Some(attempt_id),
            })
            .await;
        }
        Ok(applied_hz)
    }
}
