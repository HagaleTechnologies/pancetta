//! The QSO state machine: what a routed frame does to one QSO.
//!
//! `process_message_for_qso` applies one frame to one QSO, and
//! `determine_state_transition` is the match on current state and message
//! that picks the next state. `verify_sender`, `verify_addressee`,
//! `reject_sender` and `reject_addressee` check the sender and addressee on
//! the advancing arms. `ladder_rank`, `progress_rank` and `step_ladder_rank`
//! rank positions on the QSO ladder. `advance_existing_qso_to_step` moves an
//! existing manual QSO to the step an operator replied at, instead of
//! opening a new QSO.
//!
//! PAN-375 moved these methods here from `qso_manager.rs` unchanged. A method
//! that another file calls is `pub(super)`, the scope it had as a private
//! method of `qso_manager.rs`.

use super::*;

impl QsoManager {
    pub(super) async fn process_message_for_qso(
        &self,
        qso_id: QsoId,
        message: QsoMessage,
        observed_slot_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<(), QsoManagerError> {
        let mut pending_rejections = Vec::new();
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;

        // Match the relocation commit lock order: QSO map, then PTT gate.
        // Hold through epoch invalidation, mutation and reply publication.
        // Mandatory Hound moves still apply in Hold mode.
        let hound_ptt_sync = if progress.metadata.hound
            && matches!(message.message_type, MessageType::SignalReport { .. })
            && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
        {
            Some(self.ptt_sync_gate.lock().await)
        } else {
            None
        };

        let old_state = progress.state.clone();

        // --- First-decode parity refinement ---
        // This QSO's `tx_parity` may have been latched PROVISIONALLY: when
        // `respond_to_cq_with`/`respond_to_caller` answered a DX-cluster/
        // DX-Hunter spot with no observed live decode yet, there was no real
        // DX parity to oppose, so the latch fell back to a "nearest next
        // slot" guess (`tx_parity_provisional == true`). The first time a
        // frame genuinely FROM that latched partner arrives carrying a
        // determinable slot parity, refine the latch to the TRUE
        // opposite-of-DX parity. Sender verification reuses the same
        // `is_partner` check the rest of this function relies on elsewhere —
        // never weakened for this purpose. This runs exactly once (the flag
        // flip prevents re-running) and emits no event of its own; it must
        // happen BEFORE `qso_tx_parity` is captured just below so a reply
        // this SAME call emits already carries the refined value.
        if progress.metadata.tx_parity_provisional {
            if let (Some(sender), Some(latched), Some(observed_parity)) = (
                message.message_type.sender_callsign(),
                progress.metadata.their_callsign.as_deref(),
                observed_slot_parity,
            ) {
                if Self::is_partner(sender, latched) {
                    progress.metadata.tx_parity = Some(observed_parity.opposite());
                    progress.metadata.tx_parity_provisional = false;
                }
            }
        }

        // Capture per-QSO routing data while we hold the write lock so the
        // reply emission below does not need to re-acquire it (which would
        // deadlock against this guard).
        let mut qso_frequency = progress.metadata.frequency;
        let qso_tx_parity = progress.metadata.tx_parity;
        let qso_remote_origin = progress.metadata.remote_origin;
        let qso_remote_client_key_id = progress.metadata.remote_client_key_id.clone();
        let qso_initiated_by = progress.metadata.initiated_by;
        // PR #344 round-1 Codex P2: a natively-typed ContestReply (PAN-51 --
        // ft8_message_to_qso_type classifies a ReplyWithR decode directly,
        // never routing through the NonStandard reclassification below that
        // used to be the only place this QSO-engagement gate ran) must still
        // be gated the same way, or an ordinary non-contest QSO advances and
        // logs a contest-shaped exchange the moment its real partner happens
        // to send an "R"+grid/report shape.
        let qso_contest_engaged = Self::grid_contest_engaged(&progress.metadata);

        // PAN-259: a Hound answers a Fox report at the Fox's own offset. A
        // report whose offset is outside the response envelope (or not a
        // finite, transmittable offset) cannot be answered there, so it is
        // ignored before it changes any exchange state or schedules an
        // R-report; the QSO keeps waiting for a valid report.
        if progress.metadata.hound
            && matches!(message.message_type, MessageType::SignalReport { .. })
            && matches!(
                old_state,
                QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. }
            )
            && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
            && hound_report_offset(message.frequency, &self.config.hound).is_none()
        {
            warn!(
                target: "hound",
                qso_id = %qso_id,
                "Hound: ignoring Fox report at {:.0} Hz, outside the response envelope {}–{} Hz",
                message.frequency,
                self.config.hound.response_min_hz,
                self.config.hound.response_max_hz
            );
            return Ok(());
        }

        progress.messages.push(message.clone());

        // Compound-callsign equivalence (catalog C18 / peer D4): if this frame
        // came from our latched partner under a MORE-COMPLETE displayed call
        // (same station per `callsigns_match`, but a longer compound form — e.g.
        // we latched bare `G8BCG` and the DX now signs `EA8/G8BCG`), upgrade the
        // logged `their_callsign` to the fuller form. The compound carries DX /
        // portable info worth preserving in the ADIF.
        //
        // SECURITY (deep audit, Fix 1): the upgrade is gated by
        // `is_safe_compound_upgrade`, NOT a bare `callsigns_match` + length
        // check. `callsigns_match` treats two calls as the same station when
        // they share a base, so an RF attacker who knows the partner's on-air
        // base call could otherwise rewrite the LOGGED callsign by signing a
        // state-appropriate frame with arbitrary junk wrapped around that base
        // (e.g. `BOGUS9/G8BCG/MM`). The safe check additionally requires every
        // extra token to be a recognized prefix/suffix (per the same rules
        // `validate_callsign` uses) and forbids replacing a latched affix with
        // a different one — so only a genuine compound completion of the same
        // base is accepted. A rejected-but-matching upgrade is logged at
        // `warn!(target: "qso.security")`.
        if let (Some(sender), Some(latched)) = (
            message.message_type.sender_callsign(),
            progress.metadata.their_callsign.as_deref(),
        ) {
            if crate::exchange::is_safe_compound_upgrade(latched, sender) {
                let upgraded = sender.to_string();
                info!(
                    target: "qso.compound",
                    from = %latched,
                    to = %upgraded,
                    "upgrading logged partner callsign to more-complete compound form (C18)"
                );
                progress.metadata.their_callsign = Some(upgraded);
            } else if sender.len() > latched.len()
                && crate::exchange::callsigns_match(sender, latched)
            {
                // Same base + strictly longer, but NOT a safe compound
                // completion (junk affix token, or a different prefix/suffix
                // than already latched). Do not overwrite the logged call.
                warn!(
                    target: "qso.security",
                    latched = %latched,
                    rejected = %sender,
                    "rejected unsafe compound-callsign upgrade (unrecognized or substituted affix); keeping latched logged callsign"
                );
                pending_rejections.push(QsoEvent::MessageRejected {
                    qso_id,
                    reason: RejectionReason::UnsafeCompoundUpgrade,
                    from_callsign: Some(sender.to_string()),
                    to_callsign: message
                        .message_type
                        .addressee_callsign()
                        .map(str::to_string),
                });
            }
        }

        // Determine state transition based on current state and message.
        // `initiated_by` is threaded through so the manual-only state-regression
        // arms ("back up to where the DX thinks we are") never fire for
        // autonomous QSOs.
        let mut new_state = self
            .determine_state_transition(
                qso_id,
                &old_state,
                &message.message_type,
                message.signal_strength,
                qso_initiated_by,
                qso_contest_engaged,
            )
            .await?;

        // Did this received frame advance the QSO? Computed here (before
        // `old_state`/`new_state` are moved into emit_state_change) for the
        // forward-advance tracking below, which resets `stall_cycles`, bumps
        // `advance_generation` and records `last_known_good_offset_hz` on a
        // real advance.
        //
        // PAN-72 (Codex round 1 on PR #350, finding 2): this uses the
        // role-agnostic `progress_rank`, NOT the Caller-shaped `ladder_rank`
        // the manual context-reply path compares against `ResponseStep`. See
        // `progress_rank`'s doc comment: with `ladder_rank`, the CQer's real
        // `CallingCq -> WaitingForReport` advance read as "no progress", so a
        // stall streak built up over unanswered CQs carried into the
        // established exchange and could trip the switch threshold on the
        // very first missed report slot — with no known-good offset recorded.
        let dx_frame_advanced = Self::progress_rank(&new_state) > Self::progress_rank(&old_state);

        // PAN-72 (Codex round 5 on PR #350, finding 2): the RX tolerance the
        // relevance gate actually judged THIS frame with, captured from the
        // PRE-transition state — `is_message_relevant`/`classify_relevance` ran
        // against `old_state`, and the advance we are about to record may well
        // have just latched a contra callsign, which would widen the bound
        // retroactively. Used below to decide which offset the advance proves.
        let rx_tolerance_hz = if old_state.their_callsign().is_some() {
            ESTABLISHED_FREQ_TOLERANCE_HZ
        } else {
            FREQ_TOLERANCE_HZ
        };

        // Hound QSY gate: computed here while both `old_state` and `new_state`
        // are still in scope (before `old_state` is consumed by `emit_state_change`
        // below). The QSY fires after the state-update block where we can access
        // `progress` again.
        let was_responding_to_cq = matches!(old_state, QsoState::RespondingToCq { .. });
        let advanced_to_sending_report = matches!(new_state, QsoState::SendingReport { .. });

        // Auto-sequence the outbound reply for MANUAL-initiated QSOs only.
        // The reply is generated from the SAME (pre-transition state,
        // received message) pair that drove the transition, so the two never
        // disagree. Autonomous-initiated QSOs are deliberately left UNCHANGED
        // (no auto-reply) — that remains gated for Phase 5.
        //
        // `reply_to_emit` is captured here (under the lock) and emitted after
        // the write guard is released, since `emit_event` only needs the
        // broadcast channel, not the QSO map.
        // Detect a manual state regression: the DX sent an EARLIER-stage
        // message and `determine_state_transition` either backed us up the
        // ladder (rank decreased) or kept us in SendingReport on a repeated
        // report that was already latched. First acquisition (None -> Some)
        // is forward progress and keeps the watchdog reprieve (PAN-84).
        // When the regression queues a re-send (`reply_to_emit`), used
        // to (a) count that re-send against the manual watchdog cap and (b)
        // gate the per-slot rearm so it does not double-send in the same slot;
        // a repeated report in SendingReport queues none, so neither applies
        // (PAN-129). Every regression cancels the C3 reprieve.
        let is_manual_regression = qso_initiated_by == CallInitiation::Manual
            && (Self::ladder_rank(&new_state) < Self::ladder_rank(&old_state)
                || (matches!(
                    old_state,
                    QsoState::SendingReport {
                        their_report: Some(_),
                        ..
                    }
                ) && matches!(new_state, QsoState::SendingReport { .. })
                    && matches!(message.message_type, MessageType::SignalReport { .. })));

        let mut reply_to_emit: Option<MessageType> = None;
        // Phase 5 (autonomous auto-completion): forward auto-sequencing now fires
        // for BOTH Manual and Auto QSOs — an autonomous-opened pounce / CQ-answer
        // must advance its reply ladder (grid → R-report → RR73 → 73) exactly as a
        // manual QSO does, otherwise the autonomous operator opens a QSO and then
        // goes silent after the opening call. Regression handling stays Manual-only:
        // `is_manual_regression` is always false for an Auto QSO, so an Auto QSO
        // emits a reply ONLY on a genuine FORWARD advance (`new_state != old_state`).
        // This activates only while the autonomous operator is running (Auto QSOs
        // are created solely by `respond_to_cq` / `start_cq`), so it is gated behind
        // the autonomous toggle by construction.
        if new_state != old_state || is_manual_regression {
            let exchange = crate::exchange::MessageExchange::new(self.config.our_callsign.clone());
            match exchange.generate_response(
                &old_state,
                &message.message_type,
                message.signal_strength,
            ) {
                Ok(Some(reply)) => reply_to_emit = Some(reply),
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        qso_id = %qso_id,
                        "failed to generate auto-sequence reply: {}",
                        e
                    );
                }
            }
        }
        // PAN-259: in the WSJT-X Fox/Hound procedure the Fox's RR73 ends the
        // contact. A Hound logs it and sends no 73: the Fox has already moved
        // on, and a 73 would only add QRM to the Fox's next Hounds.
        if progress.metadata.hound && hound_suppresses_reply(reply_to_emit.as_ref()) {
            reply_to_emit = None;
        }
        // PAN-386: a WSJT-X Fox logs a Hound when it sends RR73
        // (`mainwindow.cpp` Fox logging at RR73); the Hound sends no 73 back.
        // So for a Fox-role QSO the Hound's R-report completes the contact:
        // the `WaitingForConfirmation` result becomes `Completed`, and the
        // held-completion arm below keeps `QsoCompleted` until the RR73's
        // `TxFrameSent` arrives, reverting to `old_state` if it never airs
        // (PAN-280).
        if progress.metadata.fox
            && matches!(message.message_type, MessageType::ReportAck { .. })
            && matches!(
                old_state,
                QsoState::WaitingForReport { .. } | QsoState::SendingReport { .. }
            )
            && matches!(reply_to_emit, Some(MessageType::FinalConfirmation { .. }))
        {
            if let QsoState::WaitingForConfirmation {
                their_callsign,
                their_report,
                our_report,
                frequency,
                grid_square,
                started_at,
            } = &new_state
            {
                let completed_at = self.now_utc();
                let duration_seconds = (completed_at - *started_at).as_secs().max(0) as u32;
                new_state = QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at,
                    duration_seconds,
                };
            }
        }

        // UX audit Batch 2 #8: latch the DX's grid the moment it arrives in a
        // CqResponse (the opening "<us> <them> <grid>" / "CQ <them> <grid>"
        // exchange). The common close arm (SendingReport → Completed) hard-codes
        // grid_square: None, so without latching here the decoded grid never
        // reaches the logged ADIF GRIDSQUARE. We only overwrite when the
        // incoming grid is present so a later grid-less message can't clear it.
        if let MessageType::CqResponse {
            grid: Some(grid), ..
        } = &message.message_type
        {
            if !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        // PAN-49: latch the grid the moment a contest "R"+grid ack arrives,
        // mirroring the CqResponse grid-latch above — the transition arm's
        // WaitingForConfirmation.grid_square only feeds the Completed-state
        // latch (see the block below), not metadata directly, and ADIF
        // logging (adif.rs) reads metadata.grids.theirs, not QsoState.
        //
        // Guarded by sender verification against `old_state`'s ORIGINAL
        // partner (final review Finding 1): the transition arm's own
        // `reject_sender` check may have rejected this frame (spoofed or
        // third-party sender) and left `new_state == old_state`, but this
        // latch used to run unconditionally regardless of that outcome —
        // writing an unrelated station's grid into the QSO's ADIF-bound
        // metadata. The CqResponse latch above is protected the equivalent
        // way structurally: `(RespondingToCq, CqResponse)` has a dedicated
        // relevance arm requiring `is_partner` before it is ever reached.
        //
        // Also gated by `qso_contest_engaged` (PR #344 round-1 Codex P2):
        // this block is unconditional on the transition arm actually
        // advancing, so without this an ordinary non-contest QSO's
        // `grids.theirs` could still get overwritten by a stray
        // ContestReply-shaped decode from its real partner even after the
        // transition arm itself correctly declines to advance.
        if let MessageType::ContestReply {
            grid, from_station, ..
        } = &message.message_type
        {
            let from_partner = old_state
                .their_callsign()
                .is_some_and(|partner| Self::is_partner(from_station, partner));
            if qso_contest_engaged && from_partner && !grid.is_empty() {
                progress.metadata.grids.theirs = Some(grid.clone());
            }
        }

        // PAN-280: a completion that queues a reply (our 73 to the DX's RR73)
        // holds its QsoCompleted until that reply airs. The reply's attempt
        // id is minted here when it closes a QSO, so the held record and the
        // MessageToSend below carry the same id; `reply_sent_at` is the one
        // timestamp of its `Sent` record, so a revert can find that record.
        let mut reply_attempt_id: Option<TxAttemptId> = None;
        let reply_sent_at = self.now_utc();
        if new_state != old_state {
            // PAN-280: what a dropped closing reply puts back.
            let pre_close_reports = progress.metadata.reports.clone();
            let pre_close_end_time = progress.metadata.end_time;
            let pre_close_rf = progress.metadata.completed_rf_frequency_hz;
            // CQer flow: the QSO was created by start_cq with their_callsign
            // None (we didn't know who would answer). The moment a state
            // advance reveals the contra callsign (caller answered), latch it
            // so the logged ADIF/worked-station record carries the right call,
            // and register the callsign mapping for relevance/supersede.
            if progress.metadata.their_callsign.is_none() {
                if let Some(call) = new_state.their_callsign() {
                    progress.metadata.their_callsign = Some(call.to_string());
                    if progress.metadata.first_answer_at.is_none() {
                        progress.metadata.first_answer_at = Some(message.timestamp);
                    }
                }
            }

            // If transitioning to Completed, update metadata with signal reports and end time
            if let QsoState::Completed {
                their_report,
                our_report,
                grid_square,
                ..
            } = &new_state
            {
                progress.metadata.reports = SignalReports {
                    sent: Some(*our_report),
                    received: Some(*their_report),
                };
                progress.metadata.end_time = Some(self.now_utc());
                // Prefer the grid carried in the Completed state (CQer path
                // threads it through WaitingForConfirmation); otherwise keep
                // whatever was latched from the opening CqResponse above.
                if let Some(grid) = grid_square {
                    progress.metadata.grids.theirs = Some(grid.clone());
                }
            }

            let completed_metadata = if matches!(&new_state, QsoState::Completed { .. }) {
                // `progress.metadata.frequency` is the audio offset within the
                // slot and MUST stay that way for the QSO's entire lifetime --
                // see its own doc comment (PAN-25 round 2: an earlier version
                // of this fix overwrote it in place, which broke
                // `resend_last_tx`/the TX worker's modulation limit for a
                // close-step retry on a completed QSO). The logged RF
                // frequency (dial + offset; WSJT-X logs the actual on-air
                // frequency, not the dial) is stamped separately: into
                // `completed_rf_frequency_hz` (for the recent-completed
                // suppression's band check) and, only in this LOCAL CLONE, into
                // `frequency` for the emitted event (ADIF/DB logging wants the
                // real on-air frequency) -- the stored `progress.metadata`
                // never gets that clone's `frequency` value.
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };

            // C3 fix (watchdog-vs-just-in-time-answer race): mark that this QSO
            // made a FORWARD state advance in the current watchdog cycle. The
            // manual keep-calling watchdog (`check_timeouts_at`) grants a
            // one-pass reprieve to any QSO that advanced since its previous
            // pass, so a just-in-time DX answer arriving in the very slot the
            // call cap trips is NOT thrown away as Failed{Timeout} in the same
            // tick it advanced. This is deliberately NOT a `call_count` reset —
            // the per-QSO cap still bounds total calls across the whole QSO
            // (C12: per-QSO, not per-step), and a QSO that advances once then
            // goes silent still retires at the cap on the NEXT pass (the flag
            // is cleared every watchdog pass). We set it only on a genuine
            // forward advance (not a manual regression — a DX repeating an
            // earlier message must keep counting against the cap so a stuck DX
            // cannot drive an unbounded ping-pong). Auto QSOs do not use the
            // manual watchdog and are unaffected.
            if qso_initiated_by == CallInitiation::Manual && !is_manual_regression {
                progress.metadata.progressed_this_cycle = true;
            }

            // Rearm-coordination fix: a genuine forward advance stamps
            // `last_call_at` like a manual regression that re-sends does
            // (below). Without this, the per-slot keep-call rearm
            // (`rearm_manual_calls_at`) has no way to know a response arrived
            // THIS slot — if the DX's decode lands late in the slot, the 5s
            // watchdog tick can re-emit the OLD rung (rearm fires on stale
            // `last_call_at`) in the same slot the response just advanced us
            // to a NEW rung, producing two TransmitRequests for one slot (a
            // redundant re-TX the operator observed). Stamping here suppresses
            // that slot's rearm at the source, the same way the regression
            // stamp already suppresses a double-send on a repeated DX frame.
            if qso_initiated_by == CallInitiation::Manual {
                progress.metadata.last_call_at = Some(message.timestamp);
            }

            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: message.timestamp,
                reason: TransitionReason::MessageReceived(message.message_type.clone()),
            });

            let closing_reply = completed_metadata
                .as_ref()
                .and(reply_to_emit.as_ref())
                .map(|reply| self.render_sent_text(reply));
            match (completed_metadata, closing_reply) {
                // PAN-280: our 73 is queued below; log only once it airs.
                (Some(metadata), Some(reply_text)) => {
                    let attempt_id = self.next_attempt_id();
                    reply_attempt_id = Some(attempt_id);
                    self.hold_completion(
                        qso_id,
                        PendingCompletion {
                            attempt_ids: vec![attempt_id],
                            raw_text: reply_text.clone(),
                            frequency_hz: qso_frequency,
                            remote_origin: qso_remote_origin,
                            remote_client_key_id: qso_remote_client_key_id.clone(),
                            queued_at: reply_sent_at,
                            completed_state: new_state.clone(),
                            restore: CompletionRestore::Revert {
                                state: old_state.clone(),
                                reports: pre_close_reports,
                                end_time: pre_close_end_time,
                                completed_rf_frequency_hz: pre_close_rf,
                                sent_record: Some((reply_sent_at, reply_text)),
                            },
                            metadata,
                            state_history: progress.state_history.clone(),
                            messages: progress.messages.clone(),
                        },
                    );
                    self.emit_state_change(qso_id, old_state, new_state).await;
                }
                // A completion with no closing frame (the DX's bare 73, a
                // Hound on the Fox's RR73) logs at once, as before.
                (Some(metadata), None) => {
                    self.emit_state_change(qso_id, old_state, new_state).await;
                    self.emit_event(QsoEvent::QsoCompleted {
                        qso_id,
                        metadata,
                        state_history: progress.state_history.clone(),
                        messages: progress.messages.clone(),
                    })
                    .await;
                }
                (None, _) => {
                    self.emit_state_change(qso_id, old_state, new_state).await;
                }
            }
        }

        // Count a manual regression RE-SEND against the keep-calling watchdog
        // so a DX that keeps repeating an earlier message cannot drive an
        // unbounded ping-pong. Only a regression that queued a reply this
        // decode (REGRESSION 1/3) re-sends: it bumps `call_count` and stamps
        // `last_call_at` to `message.timestamp`; the latter also gates
        // `rearm_manual_calls_at` (which only re-emits when ≥1 slot has
        // elapsed since `last_call_at`), so that re-send and the per-slot
        // rearm never both fire in the same slot. A repeated report in
        // SendingReport (REGRESSION 2) sends nothing — the rearm owns that
        // re-send — so it leaves both fields alone, and the rearm keeps its
        // silence cadence and counts the stall (PAN-129). Its old restamp
        // held the rearm back a full slot from every decode, which on FT2
        // (3.2 s slot, 5 s poll) skipped re-sends and delayed the stall
        // switch. `first_call_at` is left untouched — a regression must not
        // reset the watchdog clock.
        if is_manual_regression {
            if let Some(progress) = qsos.get_mut(&qso_id) {
                if reply_to_emit.is_some() {
                    progress.metadata.call_count += 1;
                    progress.metadata.last_call_at = Some(message.timestamp);
                }
                // A regression is the opposite of forward progress (the DX
                // repeated an earlier message), so it cancels any pending C3
                // reprieve from an earlier advance — a stuck DX repeating must
                // still retire at the cap, never earn an extra watchdog pass.
                progress.metadata.progressed_this_cycle = false;
            }
        }

        // PAN-72: a genuine forward advance resets the stall streak and
        // records the offset we were on as "known-good" — the offset a
        // future stall-triggered Switch/Revert decision reasons about. The
        // non-advancing case is no longer driven by inspecting incoming
        // frame content here; `QsoManager::rearm_manual_calls_at` is now the
        // sole site that increments `QsoMetadata::stall_cycles` (on silence
        // — a real per-slot re-send with no DX response) and emits
        // `QsoEvent::TxOffsetActionNeeded` once it trips
        // `TimeoutConfig::qso_stall_switch_after`.
        if let Some(progress) = qsos.get_mut(&qso_id) {
            if dx_frame_advanced {
                progress.metadata.stall_cycles = 0;
                self.clear_recorded_stall_increments(qso_id);
                // PAN-72 (Codex round 4 on PR #350, finding 3): credit the
                // offset the DX actually heard us on. A STALL-triggered
                // relocation is committed AFTER the frame that triggered it has
                // gone out on the old offset, so an advance arriving inside
                // `PRE_SWITCH_OFFSET_GRACE` of such a move is answering the
                // PRE-switch transmission — the new offset has not been
                // transmitted on even once. Crediting it would record an
                // unproven offset as known-good and degrade the next stall's
                // adaptation: with `last_known_good` equal to the offset we are
                // already on, the `>= f64::EPSILON` guard in
                // `rearm_manual_calls_at` skips `Revert` and falls through to a
                // fresh `Switch` — not a hard failure, but the genuinely proven
                // offset is lost and the QSO hops on blindly instead of
                // returning to what worked.
                //
                // Round 5, finding 2: but the clock alone does not identify
                // which frame was answered. An operator-forced `u` nudge is NOT
                // coupled to an old-offset triggering resend — it can land on a
                // QSO that is about to transmit, the next rearm goes out on the
                // NEW offset, and the answer to THAT lands well inside the same
                // grace. So the decisive evidence is where the reply actually
                // decoded: the vacated offset is credited only when the frame
                // matches that baseline, judged with the SAME tolerance the
                // relevance gate admitted it under (`rx_tolerance_hz`, taken
                // from the pre-transition state so a callsign latched by this
                // very advance cannot widen it retroactively). Otherwise we
                // credit the offset we are on now, which is where we are
                // actually transmitting. Where the two baselines overlap (a
                // move smaller than the tolerance) the vacated one still wins:
                // it is the offset with a proven history.
                //
                // Round 5 re-review: that comparison holds the DX's decode
                // frequency against OUR vacated TX offset, so it is evidence
                // ONLY on a Tx=Rx QSO, where those are the same number. A QSO
                // that already carried a `partner_freq` before the switch
                // (Hound, an `o` hold, a de-conflict nudge, a passband clamp)
                // keeps it — `apply_tx_offset_switch` leaves an already-`Some`
                // latch alone — so the DX still transmits at `partner_freq`,
                // which matches neither of our offsets. The check would fail for
                // a reason that says nothing about which frame was answered, and
                // crediting the never-transmitted new offset off the back of it
                // is strictly worse than round 4's behaviour. So the check is
                // applied only when the QSO is Tx=Rx: no `partner_freq` at all.
                //
                // PAN-132 (Codex round 15 on PR #350, P2): that includes a split
                // this switch itself created. `apply_tx_offset_switch`'s
                // established-QSO latch sets `partner_freq` to exactly the
                // offset it vacated, so round 5's re-review counted that QSO as
                // Tx=Rx. But the latch exists because the DX is still
                // transmitting there, whatever moved us, so its reply decodes at
                // the vacated offset whichever of our frames it answered. The
                // relevance gate has already admitted it against that same
                // baseline with the same tolerance, so the check could not fail:
                // an operator nudge credited the offset it had just left, and
                // the next stall `Revert`ed the nudge. A `partner_freq` that
                // merely coincides with the vacated offset (an earlier move
                // landed back on it) is the same case. Within the grace,
                // `partner_freq` is therefore `None` only after a switch on an
                // unanswered `CallingCq`, where the latch deliberately does not
                // fire — the only case where the reply frequency is evidence.
                //
                // Round 6, finding 2: on ANY split — one that predates the
                // switch, or the one its own latch just created (PAN-132) — the
                // fallback is not one answer but two, and the provenance of the
                // relocation picks between them
                // (`PreSwitchOffset::operator_forced`). The DX transmits at
                // `partner_freq` no matter where we key, so its reply frequency
                // is silent about which of our offsets it answered — but the two
                // provenances carry different priors:
                //
                // - a STALL-triggered switch only fires BECAUSE the old offset
                //   demonstrably stopped working, and the triggering resend went
                //   out on it a slot before this commit. The vacated offset is
                //   the one with a proven history and the new one has never been
                //   transmitted on, so credit the vacated offset — round 4/5's
                //   behaviour, unchanged;
                // - an OPERATOR-forced `u` nudge indicts nothing. The operator
                //   chose to move; our very next frame goes out on the NEW
                //   offset, so an advancing reply after it is better evidence
                //   for the new offset than for the one we left deliberately.
                //   Crediting the vacated one there would aim a later `Revert`
                //   straight back at the offset the operator just escaped.
                //
                // PAN-83: provenance is a prior, not evidence — it says why we
                // moved, never which of our transmitted frames the reply
                // answered. On a split the aired-frame record now answers
                // exactly that: the TX worker reports every frame that played
                // out and the offset it aired on
                // (`confirm_stall_trigger_aired`), and the reply answers the
                // frame of ours that aired in the slot just before its own
                // (`answered_frame_offset_hz`). Three outcomes follow: a
                // relocation committed AFTER that frame aired (the ordinary
                // `u`-nudge timing — the keypress queues while our frame is
                // keyed and commits at the next drain) credits the vacated
                // offset; a frame that aired on the new offset before the
                // reply credits the new offset; and across chained
                // relocations the credit is the offset the answered frame
                // actually aired on, whatever the chain did before or after.
                // When no aired frame pins the answer down — no report in the
                // reply's window, no decoded slot parity — the round-6
                // provenance rule above decides, unchanged. The Tx=Rx branch
                // below keeps the decode-frequency rule: a station answering
                // our CQ transmits where it clicked us, so its decode
                // frequency already names the frame it answered, whatever the
                // aired record says.
                //
                // Consumed here either way: the first advance after a switch is
                // the one that resolves the ambiguity, and everything after it
                // is unambiguously about the current offset.
                let vacated = live_pre_switch_offset_at(
                    &progress.metadata,
                    message.timestamp,
                    self.pre_switch_offset_grace(),
                );
                let current_hz = progress.metadata.frequency;
                progress.metadata.last_known_good_offset_hz = Some(match vacated {
                    None => current_hz,
                    Some(pre) => {
                        let tx_equals_rx = progress.metadata.partner_freq.is_none();
                        let (credited_hz, evidence) = if tx_equals_rx {
                            // The decode frequency IS evidence here: it is the
                            // same number as our own TX offset.
                            if (pre.offset_hz - message.frequency).abs() <= rx_tolerance_hz {
                                // PAN-72 round-12 P1 fix (Codex round 12,
                                // thread on `qso_manager.rs:4047`): this
                                // sub-case is exactly a rescued, unanswered
                                // `CallingCq` — the caller's decoded frame
                                // matched the vacated baseline, which is only
                                // possible when there was no DX (and so no
                                // `partner_freq`) to latch at switch time.
                                // `apply_tx_offset_switch` deliberately left
                                // `partner_freq` at `None` then (see its own
                                // `their_callsign().is_some()` guard), but THIS
                                // advance is the establishing one: a caller has
                                // just answered on the offset we vacated. Latch
                                // it now, or the next hint-recompute reads the
                                // QSO's brand-new TX offset instead of where
                                // the caller actually is, and a relocation
                                // bigger than the decoder's narrow window
                                // stalls the exchange right after rescuing it.
                                //
                                // Since PAN-132 `tx_equals_rx` is exactly
                                // `partner_freq.is_none()`, so this guard always
                                // holds here. It stays so that a future
                                // widening of `tx_equals_rx` can never let this
                                // latch overwrite a split an established QSO
                                // already carries.
                                if progress.metadata.partner_freq.is_none() {
                                    progress.metadata.partner_freq = Some(pre.offset_hz);
                                }
                                (pre.offset_hz, "reply frequency")
                            } else {
                                (current_hz, "reply frequency")
                            }
                        } else if let Some(answered_hz) = answered_frame_offset_hz(
                            self.keep_call_ledger
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&qso_id)
                                .and_then(|entry| entry.last_aired),
                            message.timestamp,
                            observed_slot_parity,
                            self.active_slot_ns.load(Ordering::Relaxed),
                        ) {
                            (answered_hz, "aired frame")
                        } else if pre.operator_forced {
                            (current_hz, "operator nudge")
                        } else {
                            (pre.offset_hz, "stall switch")
                        };
                        // PAN-83: one line per grace-window credit, on the
                        // target the on-air `u`-nudge drill reads (the same
                        // `tx.freq` target as the commit line in
                        // `apply_tx_offset_switch`). The evidence label names
                        // which rule decided.
                        info!(
                            target: "tx.freq",
                            qso_id = %qso_id,
                            "Adaptive TX-offset credit: {:.0} Hz known-good after leaving {:.0} Hz (evidence: {})",
                            credited_hz, pre.offset_hz, evidence
                        );
                        credited_hz
                    }
                });
                progress.metadata.pre_switch_offset = None;
                // PAN-72 (Codex round 1 on PR #350, finding 8): bump the
                // staleness token in lockstep with the two fields above, so a
                // TX-offset action queued BEFORE this advance is recognized as
                // stale when the coordinator's once-per-slot drain finally
                // commits it — see `QsoMetadata::advance_generation`.
                progress.metadata.advance_generation =
                    progress.metadata.advance_generation.saturating_add(1);
            }

            // Hound QSY (PAN-259): when the Fox sends us a signal report
            // (RespondingToCq → SendingReport), the Hound moves its TX offset
            // to the Fox's own decoded offset and sends the R+report
            // (`ReportAck`) there — the WSJT-X Hound procedure. A later
            // addressed report from the Fox on another admissible stream
            // re-anchors the Hound onto that stream.
            //
            // Executes INDEPENDENT of `TxFreqMode` (procedure-mandated, not an
            // autonomous optimisation — unlike the Auto-gated silence stall
            // detector above, which only fires in `TxFreqMode::Auto`), so Hold
            // honours it too. The admissibility check already ran before the
            // transition (`hound_report_offset`), so the target is exactly the
            // received offset.
            //
            // This mutates `metadata.frequency` (used as `qso_frequency` on the
            // NEXT process_message call), `qso_frequency` (rides the ReportAck
            // emitted this cycle), `partner_freq` (where we hear the Fox, and
            // the pin `hound_switch_range_hz` holds us to), the known-good
            // anchor, and the frequency inside the `SendingReport` state so
            // the subsequent `Completed` state logs our actual TX offset.
            let hound_fox_hz = if progress.metadata.hound
                && advanced_to_sending_report
                && (was_responding_to_cq || progress.metadata.hound_qsyed)
                && matches!(message.message_type, MessageType::SignalReport { .. })
                && self.is_hound_fox_frame(&progress.metadata, &message.message_type)
            {
                hound_report_offset(message.frequency, &self.config.hound)
            } else {
                None
            };
            if let Some(fox_hz) = hound_fox_hz {
                let fox_call = progress.metadata.their_callsign.as_deref().unwrap_or("");
                let old_off = progress.metadata.frequency;
                let moved = !progress.metadata.hound_qsyed
                    || (fox_hz - old_off).abs() > TX_OFFSET_NOOP_TOLERANCE_HZ;
                if moved {
                    // PAN-145: a frame snapshotted at the old offset must not
                    // key after this move.
                    self.relocation_epochs.bump(&qso_id.to_string());
                }
                progress.metadata.frequency = fox_hz;
                progress.metadata.partner_freq = Some(fox_hz);
                progress.metadata.pending_freq_drift = None;
                progress.metadata.hound_qsyed = true;
                // PAN-72 (Codex round 1 on PR #350, finding 5): re-anchor the
                // known-good offset onto the Fox offset. The
                // `dx_frame_advanced` block a few lines above just recorded
                // the calling offset as known-good, so without this a later
                // `SendingReport` stall would read `frequency !=
                // last_known_good` and emit a `Revert` back to the calling
                // offset, where the Fox is no longer listening.
                progress.metadata.last_known_good_offset_hz = Some(fox_hz);
                // Keep the ReportAck emitted this cycle on the Fox offset.
                qso_frequency = fox_hz;
                if let QsoState::SendingReport {
                    frequency: ref mut state_freq,
                    ..
                } = progress.state
                {
                    *state_freq = fox_hz;
                }
                if moved {
                    info!(
                        target: "hound",
                        qso_id = %qso_id,
                        fox = %fox_call,
                        "Hound: Fox sent our report — QSY TX {:.0} Hz -> {:.0} Hz (Fox offset), sending R+report there",
                        old_off, fox_hz
                    );
                }
            }
        }

        self.emit_event(QsoEvent::MessageReceived { qso_id, message })
            .await;

        // Record the auto-sequenced reply as a Sent message (under the lock)
        // so it is available to `resend_last_tx` and the UI snapshot. Render
        // the FT8 text so the TUI "TX:" line shows what we sent (UX audit
        // Batch 2 — was String::new()).
        if let Some(reply) = reply_to_emit.as_ref() {
            let reply_text = self.render_sent_text(reply);
            if let Some(progress) = qsos.get_mut(&qso_id) {
                progress.messages.push(QsoMessage {
                    timestamp: reply_sent_at,
                    direction: MessageDirection::Sent,
                    message_type: reply.clone(),
                    raw_text: reply_text,
                    signal_strength: None,
                    frequency: qso_frequency,
                });
            }
        }

        // A Hound relocation publishes while both locks still protect the
        // binding and frequency. Do not reacquire the QSO map under the gate.
        if hound_ptt_sync.is_some() {
            if let Some(reply) = reply_to_emit.take() {
                self.emit_event(QsoEvent::MessageToSend {
                    qso_id,
                    message: reply,
                    frequency: qso_frequency,
                    tx_parity: qso_tx_parity,
                    remote_origin: qso_remote_origin,
                    remote_client_key_id: qso_remote_client_key_id.clone(),
                    attempt_id: Some(reply_attempt_id.unwrap_or_else(|| self.next_attempt_id())),
                })
                .await;
            }
        }
        drop(hound_ptt_sync);

        // Release the QSO map write lock before emitting the reply so the
        // emission path holds no locks (and a future change to send_message-
        // style routing cannot deadlock).
        drop(qsos);

        for event in pending_rejections {
            self.emit_event(event).await;
        }

        // Emit the auto-sequenced reply for manual QSOs. We transmit on the
        // QSO's own frequency and reuse the tx_parity latched at QSO start,
        // exactly as the initial-call MessageToSend does.
        if let Some(reply) = reply_to_emit {
            // Re-read the remote binding here rather than trusting the
            // pre-drop snapshot: a concurrent takeover can rebind this QSO's
            // remote identity (`resend_existing_if_permitted`) and publish its own
            // Remote resend in the window since the write lock was released
            // above. Emitting the stale origin/client-id would misattribute
            // this reply (e.g. advertise it as Local) and could let a later
            // pivot/coalesce bypass the new client's arm gate (Codex P1,
            // round 17).
            let (remote_origin, remote_client_key_id) = self
                .current_remote_binding(qso_id, (qso_remote_origin, qso_remote_client_key_id))
                .await;
            self.emit_event(QsoEvent::MessageToSend {
                qso_id,
                message: reply,
                frequency: qso_frequency,
                tx_parity: qso_tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id: Some(reply_attempt_id.unwrap_or_else(|| self.next_attempt_id())),
            })
            .await;
        }

        Ok(())
    }

    fn verify_sender(&self, from: &str, partner: &str, to: &str) -> Option<RejectionReason> {
        RejectionReason::classify(Self::is_partner(from, partner), self.is_us(to))
    }

    fn verify_addressee(&self, to: &str) -> Option<RejectionReason> {
        (!self.is_us(to)).then_some(RejectionReason::AddresseeNotUs)
    }

    async fn reject_sender(&self, qso_id: QsoId, from: &str, partner: &str, to: &str) -> bool {
        let Some(reason) = self.verify_sender(from, partner, to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    async fn reject_addressee(&self, qso_id: QsoId, from: &str, to: &str) -> bool {
        let Some(reason) = self.verify_addressee(to) else {
            return false;
        };
        self.emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason,
            from_callsign: Some(from.to_string()),
            to_callsign: Some(to.to_string()),
        })
        .await;
        true
    }

    /// The **responder-flow** ladder, deliberately aligned 1:1 with
    /// [`Self::step_ladder_rank`]'s [`pancetta_core::ResponseStep`] mapping so
    /// an operator's context reply can be compared against where an existing
    /// manual QSO currently sits. It is Caller-role-shaped by construction —
    /// there is no `ResponseStep` for "I called CQ" or "I sent my report as
    /// the CQer", so `CallingCq`/`WaitingForReport` have no rung here and
    /// must not gain one.
    ///
    /// For "did this QSO make forward progress?" use [`Self::progress_rank`]
    /// instead — see its doc comment for why the two ladders are separate.
    pub(super) fn ladder_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::RespondingToCq { .. } => Some(0),
            QsoState::SendingReport { .. } => Some(1),
            QsoState::WaitingForConfirmation { .. } => Some(2),
            QsoState::SendingConfirmation { .. } => Some(2),
            QsoState::Completed { .. } => Some(3),
            _ => None,
        }
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 2): the **role-agnostic**
    /// forward-progress ladder, covering the CQer flow as well as the Caller
    /// flow. Used solely by `process_message_for_qso`'s `dx_frame_advanced`
    /// predicate, which resets [`QsoMetadata::stall_cycles`] and records
    /// [`QsoMetadata::last_known_good_offset_hz`] on a genuine advance.
    ///
    /// This is deliberately NOT [`Self::ladder_rank`]. That ladder exists to
    /// be compared against a [`pancetta_core::ResponseStep`] in the manual
    /// context-reply path and is Caller-role-shaped for that reason; widening
    /// it would silently change which context replies advance an existing QSO
    /// versus re-send its current outbound. Stall detection needs the
    /// opposite property — every real advance in EITHER role must count —
    /// so it gets its own function.
    ///
    /// The CQer rungs mirror the Caller's stage for stage:
    ///   0. opening sent (`CallingCq` / `RespondingToCq`)
    ///   1. our report is on the air but theirs has not arrived
    ///      (`WaitingForReport` — the CQer's report goes out on the
    ///      `CallingCq -> WaitingForReport` transition — /
    ///      `SendingReport { their_report: None }`)
    ///   2. the partner's report is in hand (`SendingReport { their_report:
    ///      Some(_) }`)
    ///   3. exchange rogered (`WaitingForConfirmation` / `SendingConfirmation`)
    ///   4. `Completed`
    ///
    /// Without rung 0/1 for the CQer, the real `CallingCq -> WaitingForReport`
    /// advance (a caller finally answering an unanswered CQ) read as "no
    /// progress": the stall streak accumulated during the unanswered CQs
    /// carried straight into the established exchange, so a single missed
    /// report slot could trip the threshold immediately — and without ever
    /// recording the just-proven offset as known-good, so the resulting action
    /// was a blind `Switch` rather than the intended anchored ping-pong.
    ///
    /// PAN-72 (Codex round 6 on PR #350, finding 1): `SendingReport` carries a
    /// real sub-rung and must not be collapsed into one. `RespondingToCq +
    /// CqResponse` lands us in `SendingReport { their_report: None }` (the DX
    /// returned our call with no report yet); the DX's first `SignalReport`
    /// then moves us to `SendingReport { their_report: Some(_) }`, which is
    /// what turns our outbound from a plain report into an R-report — the
    /// single most load-bearing advance in the whole exchange. Ranked equal,
    /// that transition read as "no progress": a switch request queued by the
    /// preceding threshold-hitting rearm left `advance_generation` unmoved, so
    /// the commit-time stale-action guard accepted it and relocated the QSO at
    /// the exact moment the DX finally answered; below the threshold, the
    /// partial stall streak carried forward instead of resetting.
    ///
    /// A repeated report (`Some(_) -> Some(_)`, the "the DX never copied our
    /// R" regression arm) still ranks equal, so it correctly reads as no
    /// progress. [`Self::ladder_rank`] is deliberately NOT given this
    /// sub-rung: the manual regression predicate relies on both `SendingReport`
    /// shapes comparing equal there.
    ///
    /// `Contest(ExchangingInfo)` gets the analogous `their_serial`
    /// `None`/`Some` split for the same reason — the contest wiring is not
    /// live yet, and ranking it correctly here keeps it from inheriting the
    /// bug when it is.
    ///
    /// `Failed` and `Idle` stay `None`: `None` is ordered below every `Some`,
    /// so a QSO going terminal-`Failed` can never read as an advance.
    fn progress_rank(state: &QsoState) -> Option<u8> {
        match state {
            QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. } => Some(0),
            QsoState::WaitingForReport { .. }
            | QsoState::SendingReport {
                their_report: None, ..
            } => Some(1),
            QsoState::SendingReport {
                their_report: Some(_),
                ..
            } => Some(2),
            // Not currently constructed by `determine_state_transition`, but
            // ranked here so contest wiring inherits correct stall detection
            // rather than re-introducing this same bug.
            QsoState::Contest(ContestState::ExchangingInfo {
                their_serial: None, ..
            }) => Some(1),
            QsoState::Contest(ContestState::ExchangingInfo {
                their_serial: Some(_),
                ..
            }) => Some(2),
            QsoState::WaitingForConfirmation { .. } | QsoState::SendingConfirmation { .. } => {
                Some(3)
            }
            QsoState::Completed { .. } => Some(4),
            QsoState::Contest(ContestState::ContestCompleted { .. }) => Some(4),
            QsoState::Idle | QsoState::Failed { .. } => None,
        }
    }

    pub(super) async fn determine_state_transition(
        &self,
        qso_id: QsoId,
        current_state: &QsoState,
        message_type: &MessageType,
        signal_strength: Option<f32>,
        initiated_by: CallInitiation,
        contest_engaged: bool,
    ) -> Result<QsoState, QsoManagerError> {
        match (current_state, message_type) {
            // CQ call received response (CQer flow). A station answered our CQ
            // with "<us> <them> <grid>". Verify the response is addressed to us
            // (calling_station == our callsign) before advancing — a spurious
            // CqResponse to another station must not hijack our CQ QSO. We latch
            // their grid here (UX audit Batch 2 #8) so the eventual ADIF carries
            // GRIDSQUARE; the relevance filter already directs only
            // addressed-to-us responses here, but we re-verify for defence.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    grid,
                },
            ) => {
                if self
                    .reject_addressee(qso_id, responding_station, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %calling_station,
                        got_from = %responding_station,
                        "CqResponse not addressed to us ignored — no CQ advance"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: latch the report we're about to send (the reply
                // emitter answers with our SignalReport on this transition)
                // so a repeated CqResponse can re-send an IDENTICAL value —
                // see WaitingForReport::our_report's doc comment. No prior
                // report exists to fall back on here, so the no-SNR default
                // matches every other first-time-computed report in this
                // file (-15).
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: responding_station.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                    their_grid: grid.clone(),
                    our_report,
                })
            }

            // A4 (CQer flow — caller skips the grid): a station answers our CQ
            // with a bare signal report ("<us> <them> -NN") instead of the
            // usual grid frame. On-air this means "I copied you, here's your
            // report" — the caller already has our copy. The protocol-correct
            // next move for us (the CQer) is to send THEM our report, exactly
            // as we would after a grid-bearing CqResponse — so we advance to
            // WaitingForReport (same rung as the CqResponse path) and the reply
            // emitter sends our SignalReport. Without this arm CallingCq had no
            // SignalReport transition and we kept re-CQing forever.
            //
            // Sender-verified like every other arm: the report must be TO us
            // (we don't yet know who will answer our CQ, so any from_station is
            // accepted as the contra). We latch their callsign (from_station)
            // so the QSO carries the right contra call; no grid is available.
            (
                QsoState::CallingCq { frequency, .. },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => {
                if self
                    .reject_addressee(qso_id, from_station, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        got_to = %to_station,
                        got_from = %from_station,
                        "SignalReport not addressed to us ignored — no CQ advance (A4)"
                    );
                    return Ok(current_state.clone());
                }
                // SM-F4: same latch as the CqResponse arm above — the reply
                // emitter answers with our SignalReport here too, so the
                // value must be captured now for anti-jitter re-sends.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForReport {
                    their_callsign: from_station.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                    their_grid: None,
                    our_report,
                })
            }

            // A5 (CQer flow — caller closes early): after we (the CQer) sent our
            // report (now WaitingForReport) the caller fires RR73 / a plain 73
            // instead of acking with their R-report. The caller is done — accept
            // the early close, complete, and log. This is the CQer-side mirror
            // of the FIX-2 early-close arm the Caller flow already has
            // (SendingReport → Completed on RR73/73). Without it a
            // WaitingForReport CQer ignored the close and the QSO never
            // completed.
            //
            // Sender-verified (from == caller && to == us). We never received a
            // numeric report-ack from them, so log with our computed report and
            // a defaulted their_report.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in WaitingForReport ignored (CQer, A5)"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                let duration = (self.now_utc() - *started_at).as_secs().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // SM-F4: we (the CQer) sent our SignalReport on the CallingCq →
            // WaitingForReport transition, but the caller never copied it and
            // repeats their opening CqResponse (their grid again). There was
            // previously NO arm at all for (WaitingForReport, CqResponse) — it
            // fell through to the no-op default and the QSO silently died at
            // the 30s report_timeout even though the caller was still there.
            // STAY in WaitingForReport (do not advance) and re-send the
            // IDENTICAL `our_report` we already latched — mirrors REGRESSION
            // 2's anti-jitter principle (recomputing from this decode's SNR
            // would jitter the report across repeats). exchange.rs has no
            // (WaitingForReport, CqResponse) reply arm, so returning
            // WaitingForReport here is a no-op for the in-slot reply emitter;
            // the rearm (`rearm_manual_calls_at`) owns the actual re-send.
            //
            // Keep the ALREADY-latched grid: we've already answered with a
            // report, not a grid, so a repeat carrying a (possibly different)
            // grid doesn't need to overwrite what we logged the first time.
            // Sender-verified exactly like the original CallingCq + CqResponse
            // arm (calling_station must be us; the responding_station must be
            // the SAME caller we already latched, not a new one hijacking the
            // slot).
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    started_at,
                    their_grid,
                    our_report,
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForReport ignored — no regression (SM-F4)"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "CQer QSO staying in WaitingForReport \
                     (caller repeated their grid; they never copied our report)"
                );
                Ok(QsoState::WaitingForReport {
                    their_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: *started_at,
                    their_grid: their_grid.clone(),
                    our_report: *our_report,
                })
            }

            // CQer flow: we sent our SignalReport (on the CallingCq→
            // WaitingForReport transition) and the caller rogered it with their
            // R-report (ReportAck). Advance to WaitingForConfirmation; the reply
            // emitter answers our FinalConfirmation (RR73). Carry the latched
            // grid into the confirmation state so it reaches Completed/ADIF.
            // Sender-verified (from == DX && to == us) like every other arm.
            (
                QsoState::WaitingForReport {
                    their_callsign,
                    frequency,
                    their_grid,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in WaitingForReport ignored (CQer)"
                    );
                    return Ok(current_state.clone());
                }
                // The caller's R-report is their report OF US. Our report (of
                // them) was computed when we sent it; recover it from SNR or
                // fall back to the report they just acked.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: their_grid.clone(),
                    started_at: self.now_utc(),
                })
            }

            // STUCK-AT-GRID FIX: we answered a CQer with our grid (now in
            // RespondingToCq) and the DX returns OUR call — either a bare
            // "<us> <DX>" or a "<us> <DX> <grid>" (a CqResponse directed at
            // us, carrying no report). On-air this means "I copied you, here
            // I am" — the DX heard our grid. The protocol-correct next move is
            // for us (the answering station) to send the DX a signal report,
            // advancing the contact. Without this arm we re-sent our grid every
            // slot until the manual watchdog timed out — the single
            // highest-frequency stall in the on-air log (N8ME, F5NNN, N9FME,
            // IQ0VT, KB5YNF, KA0NC, first-K9HJZ).
            //
            // A bare-call or grid answer to us parses as a CqResponse with
            // calling_station = us, responding_station = DX. Verify both
            // directions (from DX, to us) before advancing, exactly as on every
            // other arm. We carry no report from the DX yet (their_report:
            // None) and compute OUR report from the SNR.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                if self
                    .reject_sender(qso_id, responding_station, target_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in RespondingToCq ignored — sender/target mismatch"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                info!(
                    target: "qso.advance",
                    their_callsign = %target_callsign,
                    "DX returned our call without a report — advancing grid -> signal report"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: None,
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // Response to CQ, waiting for report
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                // Use received signal strength (SNR) as our report, default to received report
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::SendingReport {
                    their_callsign: target_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // Phase-5 skip-rung: the DX skipped the plain-report rung and sent an
            // R-report (ReportAck) directly while we are still at grid
            // (RespondingToCq). Treat it like the (WaitingForReport, ReportAck)
            // close for the CQer role: advance to WaitingForConfirmation; the
            // reply emitter sends our RR73, and the (WaitingForConfirmation,
            // RR73/73) arm completes + logs on the DX's roger. Sender-verified
            // exactly as the SignalReport arm above.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*report);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: *report,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: self.now_utc(),
                })
            }

            // PAN-49 skip-rung: a state-QSO-party / ARRL Intl Digital
            // partner acks our grid with "R"+grid instead of a numeric
            // report — same close-shape as the ReportAck skip-rung arm
            // above (RespondingToCq -> WaitingForConfirmation), but with no
            // real dB value to carry. `-15` is the established "no real
            // report" sentinel used throughout this file (e.g. lines 3238,
            // 3544, 10260) for exactly this situation. `is_ack: false`
            // (the plain first-exchange grid, already handled by the
            // ordinary CqResponse path) never reaches this arm: both
            // producers of `ContestReply` -- process_message_with_parity's
            // text reclassification (PAN-49) and ft8_message_to_qso_type's
            // direct typed classification (PAN-51) -- only ever set
            // `is_ack: true`.
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::ContestReply {
                    from_station,
                    to_station,
                    grid,
                    is_ack: true,
                },
            ) => {
                // PR #344 round-1 Codex P2: PAN-51 lets a native ReplyWithR
                // decode reach this arm directly, bypassing the QSO-
                // engagement check process_message_with_parity's text-
                // reclassification path applies before ever producing
                // ContestReply. Without this, an ORDINARY non-contest QSO
                // advances and logs a contest-shaped exchange the moment
                // its real partner sends an "R"+grid shape for any reason.
                // `contest_engaged` is this QSO's own
                // `metadata.contest_info.is_some()`, captured by the caller
                // -- scoped per-QSO, matching the reclassification gate's
                // own scoping (final review Finding 2 there).
                if !contest_engaged {
                    debug!(
                        target: "qso.security",
                        qso_id = %qso_id,
                        from = %from_station,
                        "native ContestReply in RespondingToCq ignored — QSO is not contest-engaged"
                    );
                    return Ok(current_state.clone());
                }
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ContestReply in RespondingToCq ignored — sender does not match QSO target"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: Some(grid.clone()),
                    started_at: self.now_utc(),
                })
            }

            // qso-state-machine-analysis GAP-1: the DX skips BOTH remaining
            // rungs and closes directly from our opening grid (RespondingToCq)
            // with RR73/73 — they copied us on the first exchange and are
            // impatient. Mirrors the FIX-2 early-close below (SendingReport)
            // and the A5 early-close (WaitingForReport), which is asymmetric
            // without this arm: RespondingToCq had no early-close, so this
            // exchange previously stalled at grid until the watchdog retired
            // it. their_report is unknown (we never received one) — default to
            // -15 like every other early-close; our_report is best-effort from
            // this closing frame's SNR (we never got a chance to compute it
            // any other way, matching the skip-rung arm above).
            (
                QsoState::RespondingToCq {
                    target_callsign,
                    frequency,
                    ..
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, target_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %target_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in RespondingToCq ignored"
                    );
                    return Ok(current_state.clone());
                }
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(-15);
                Ok(QsoState::Completed {
                    their_callsign: target_callsign.clone(),
                    their_report: -15,
                    our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: 0,
                })
            }

            // Received report acknowledgment
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious ReportAck ignored"
                    );
                    return Ok(current_state.clone());
                }
                // With no report latched yet (we answered with a report, as a
                // Fox answers a Hound, or the DX returned our call bare), the
                // R-report the DX just sent is their report of us; log that,
                // not the -15 placeholder (PAN-386).
                Ok(QsoState::WaitingForConfirmation {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(*report),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    started_at: self.now_utc(),
                })
            }

            // FIX 2: the DX rogered our R-report directly with RR73 (or a
            // plain 73). Real FT8 is a 4-message QSO and RR73 is the close,
            // so we must complete (and the reply emitter answers our 73).
            // Without this arm the QSO stalled one message short — the DX's
            // RR73 was ignored and the contact was never logged. We accept
            // both FinalConfirmation (RR73/RRR-class close) and a bare
            // SeventyThree (73) here.
            (
                QsoState::SendingReport {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious RR73/73 in SendingReport ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (self.now_utc() - *started_at).as_secs().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: their_report.unwrap_or(-15),
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: None,
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // Received final confirmation
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    their_report,
                    our_report,
                    frequency,
                    grid_square,
                    started_at,
                },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious FinalConfirmation ignored"
                    );
                    return Ok(current_state.clone());
                }
                let duration = (self.now_utc() - *started_at).as_secs().max(0) as u32;
                Ok(QsoState::Completed {
                    their_callsign: their_callsign.clone(),
                    their_report: *their_report,
                    our_report: *our_report,
                    frequency: *frequency,
                    grid_square: grid_square.clone(),
                    completed_at: self.now_utc(),
                    duration_seconds: duration,
                })
            }

            // === STATE REGRESSION (manual-initiated QSOs only) ===========
            // Operator principle: "if a DX station re-sends something EARLIER
            // in the conversation, they obviously didn't receive our response —
            // back ourselves up to where THEY think we are."
            //
            // These arms are gated on CallInitiation::Manual so autonomous
            // QSOs are unaffected. Sender verification (from == DX && to == us)
            // is preserved on every regression exactly as on forward arms.

            // REGRESSION 1: we sent RR73 (WaitingForConfirmation) but the DX is
            // still sending us their SignalReport — they never copied our R.
            // Back up two steps to SendingReport and re-send our R-report (the
            // reply emitter answers a ReportAck for this (state, msg) pair).
            // Latch the newest report value the DX sent.
            //
            // SM-F6 safety note: this arm resets `started_at: self.now_utc()` on
            // every fire, which would let a repeatedly-regressing QSO reset
            // its own timeout clock indefinitely. Deliberately left
            // Manual-only (not extended to Auto) rather than adding a bounded
            // counter — WaitingForConfirmation is also outside SM-F6's scope
            // (only RespondingToCq/SendingReport get Auto resend/regression),
            // and it already has a uniform `confirmation_timeout` bound for
            // BOTH Manual and Auto QSOs (it's not in the Manual keep-call
            // watchdog list), so an Auto QSO here is already safely bounded
            // by that timeout without this arm's help.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    our_report,
                    frequency,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) if initiated_by == CallInitiation::Manual => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Recompute our report from the freshest SNR (fall back to the
                // already-latched value), and latch the DX's newest report.
                let our_report = signal_strength
                    .map(|snr| (snr.round() as i8).clamp(-30, 50))
                    .unwrap_or(*our_report);
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → SendingReport \
                     (DX repeated their report; they never copied our R)"
                );
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report,
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // REGRESSION 2: we sent our R (SendingReport) and the DX re-sends
            // their SignalReport — they didn't copy our R. STAY in
            // SendingReport (do not advance); the per-slot rearm
            // (`rearm_manual_calls_at`, FIX 4) keeps re-sending our R-report.
            // We refresh THEIR report to the newest value for the log, but keep
            // OUR sent report LATCHED — the report we give a station must not
            // jitter with per-decode SNR noise when the DX re-requests (observed
            // on-air: R-7 → R-9 → R-6 across repeats of the same DX report).
            // Returning a SendingReport here drives the their-report update
            // without the reply emitter double-sending: exchange.rs has no
            // (SendingReport, SignalReport) response arm, so the in-slot emit
            // path is a no-op and the rearm owns the (now stable-valued) re-send.
            //
            // SM-F6: extended to Auto QSOs too (no `initiated_by` guard) —
            // unlike REGRESSION 1/3, this arm explicitly PRESERVES
            // `started_at` (see below) rather than resetting it, so an Auto
            // QSO stuck here is still bounded by the ordinary 30s
            // report_timeout counted from when SendingReport was first
            // entered; it cannot reset its own clock by looping through this
            // arm. Safe to extend.
            (
                QsoState::SendingReport {
                    their_callsign,
                    our_report,
                    frequency,
                    started_at,
                    ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    report,
                },
            ) => {
                if self
                    .reject_sender(qso_id, from_station, their_callsign, to_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %from_station,
                        got_to = %to_station,
                        "spurious SignalReport in SendingReport ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                // Keep OUR report latched (do NOT recompute from this decode's
                // SNR) — see the comment above; only their_report refreshes.
                Ok(QsoState::SendingReport {
                    their_callsign: their_callsign.clone(),
                    their_report: Some(*report),
                    our_report: *our_report,
                    frequency: *frequency,
                    // Preserve started_at so the manual watchdog keeps measuring
                    // from the original QSO start — a regression must not reset
                    // the keep-calling clock.
                    started_at: *started_at,
                })
            }

            // REGRESSION 3: we sent RR73 (WaitingForConfirmation) but the DX
            // re-sends their original grid/call (CqResponse) — they restarted
            // the whole exchange. Back up to RespondingToCq and re-send our
            // grid/call. Only observable when the repeated message parses as a
            // CqResponse directed appropriately for this QSO.
            //
            // SM-F6 safety note: like REGRESSION 1, this arm resets
            // `started_at: self.now_utc()` on every fire. Deliberately left
            // Manual-only for the same reason as REGRESSION 1 — it operates
            // on WaitingForConfirmation, which is outside SM-F6's scope and
            // already carries a uniform `confirmation_timeout` bound for both
            // Manual and Auto QSOs.
            (
                QsoState::WaitingForConfirmation {
                    their_callsign,
                    frequency,
                    ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) if initiated_by == CallInitiation::Manual => {
                // A "DX K5ARH GRID" repeat parses with calling_station = us,
                // responding_station = DX. Verify both directions before
                // regressing so a spurious station cannot reset our QSO.
                if self
                    .reject_sender(qso_id, responding_station, their_callsign, calling_station)
                    .await
                {
                    warn!(
                        target: "qso.security",
                        expected_from = %their_callsign,
                        got_from = %responding_station,
                        got_to = %calling_station,
                        "spurious CqResponse in WaitingForConfirmation ignored — no regression"
                    );
                    return Ok(current_state.clone());
                }
                info!(
                    target: "qso.regression",
                    %their_callsign,
                    "manual QSO regressing WaitingForConfirmation → RespondingToCq \
                     (DX restarted the exchange)"
                );
                Ok(QsoState::RespondingToCq {
                    target_callsign: their_callsign.clone(),
                    frequency: *frequency,
                    started_at: self.now_utc(),
                })
            }

            // No state change
            _ => Ok(current_state.clone()),
        }
    }

    /// FIX 1: forward position of a [`ResponseStep`] on the responder's FT8 QSO
    /// ladder, aligned with [`Self::ladder_rank`] so the two are directly
    /// comparable. `Grid` → 0 (RespondingToCq), `Report`/`ReportAck` → 1
    /// (SendingReport), `Rr73` → 2 (WaitingForConfirmation), `SeventyThree` → 3
    /// (Completed). Used to decide whether a context reply is AHEAD of an
    /// existing QSO's current stage (→ advance) or at/behind it (→ re-send).
    pub(super) fn step_ladder_rank(step: pancetta_core::ResponseStep) -> Option<u8> {
        use pancetta_core::ResponseStep;
        Some(match step {
            ResponseStep::Grid => 0,
            ResponseStep::Report | ResponseStep::ReportAck => 1,
            ResponseStep::Rr73 => 2,
            ResponseStep::SeventyThree => 3,
        })
    }

    /// FIX 1: advance an EXISTING manual QSO to the state/outbound implied by
    /// `step`, instead of creating a new QSO. Mirrors the (state, message)
    /// mapping in [`Self::respond_to_caller`] but mutates the existing QSO in
    /// place: sets its state, records the outbound as a `Sent` message, emits
    /// the `MessageToSend`, and — when advancing to `SeventyThree` — stamps the
    /// completion metadata and emits `QsoCompleted` so the contact is logged.
    /// The QSO's latched `tx_parity` and `initiated_by` are preserved (we reuse
    /// what was latched at QSO start). Used when an operator context-replies at
    /// a step ahead of where the QSO currently is.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn advance_existing_qso_to_step(
        &self,
        qso_id: QsoId,
        target: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
        our_report: SignalReport,
        their_report_val: SignalReport,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<ManualCallOutcome, QsoManagerError> {
        use pancetta_core::ResponseStep;
        let now = self.now_utc();

        let (new_state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => {
                // Grid never ranks ahead of an active QSO, so the caller never
                // routes here; re-send current as a safe fallback.
                return Ok(self
                    .resend_existing_if_permitted(qso_id, remote_origin, remote_client_key_id)
                    .await);
            }
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.to_string(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.to_string(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.to_string(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let is_completed = matches!(new_state, QsoState::Completed { .. });
        let raw_text = self.render_sent_text(&message);

        // Mutate the existing QSO under the write lock, capturing what we need
        // for the emits after the lock is released.
        let emit = {
            let mut qsos = self.qsos.write().await;
            let Some(progress) = qsos.get_mut(&qso_id) else {
                return Err(QsoManagerError::QsoNotFound { qso_id });
            };
            // PAN-138 / round 13: check, rebind and advance under this one
            // guard, so no other `qsos` holder (QSO processing, a PAN-137
            // retire) runs between the check and the mutation. The check is
            // still a point-in-time read of `ArmState`, not a TX gate: a
            // disarm or expiry after it is caught only by the TX worker at
            // key time. A non-completing advance is not undone then (advances
            // record no PAN-75 attempt); a completing one holds its
            // QsoCompleted until the 73 airs and reverts if it does not
            // (PAN-280).
            if remote_origin && !(self.remote_tx_permitted)(remote_client_key_id.as_deref()) {
                return Ok(ManualCallOutcome::NotPermitted);
            }
            progress.metadata.remote_origin = remote_origin;
            progress.metadata.remote_client_key_id = remote_client_key_id;
            let old_state = progress.state.clone();
            // PAN-280: what a dropped 73 puts back.
            let pre_close_reports = progress.metadata.reports.clone();
            let pre_close_end_time = progress.metadata.end_time;
            let pre_close_rf = progress.metadata.completed_rf_frequency_hz;
            let closing_text = raw_text.clone();
            progress.state = new_state.clone();
            progress.state_history.push(StateTransition {
                from_state: old_state.clone(),
                to_state: new_state.clone(),
                timestamp: now,
                reason: TransitionReason::UserAction,
            });
            progress.messages.push(QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            });
            let tx_parity = progress.metadata.tx_parity;
            let remote_origin = progress.metadata.remote_origin;
            let remote_client_key_id = progress.metadata.remote_client_key_id.clone();

            // On completion, stamp reports/end-time and prepare the completed
            // metadata (with the real RF frequency = dial + offset) to log.
            // PAN-25 round 2: stamp `completed_rf_frequency_hz` (for the
            // suppression band-check) and the LOCAL CLONE's `frequency` (for
            // the emitted event) -- never the stored `progress.metadata.frequency`
            // itself, which must stay the audio offset for its whole
            // lifetime; see the matching comment in `process_message_for_qso`.
            let completed_metadata = if is_completed {
                progress.metadata.reports = SignalReports {
                    sent: Some(our_report),
                    received: Some(their_report_val),
                };
                progress.metadata.end_time = Some(now);
                let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
                let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
                let dial = effective_tx_dial(rx_dial, split);
                let rf_frequency = (dial > 0).then_some(progress.metadata.frequency + dial as f64);
                progress.metadata.completed_rf_frequency_hz = rf_frequency;
                let mut m = progress.metadata.clone();
                if let Some(rf) = rf_frequency {
                    m.frequency = rf;
                }
                Some(m)
            } else {
                None
            };
            // Layer 2 timeline persistence: capture the timeline as of this
            // mutation while the write lock is still held, for the held
            // QsoCompleted.
            let state_history = progress.state_history.clone();
            let messages = progress.messages.clone();
            let attempt_id = self.next_attempt_id();
            // PAN-280: a completing advance holds its QsoCompleted until the
            // 73 airs, stored under this guard and before the MessageToSend.
            if let Some(metadata) = completed_metadata {
                self.hold_completion(
                    qso_id,
                    PendingCompletion {
                        attempt_ids: vec![attempt_id],
                        raw_text: closing_text.clone(),
                        frequency_hz: frequency,
                        remote_origin,
                        remote_client_key_id: remote_client_key_id.clone(),
                        queued_at: now,
                        completed_state: new_state.clone(),
                        restore: CompletionRestore::Revert {
                            state: old_state.clone(),
                            reports: pre_close_reports,
                            end_time: pre_close_end_time,
                            completed_rf_frequency_hz: pre_close_rf,
                            sent_record: Some((now, closing_text)),
                        },
                        metadata,
                        state_history,
                        messages,
                    },
                );
            }
            (
                old_state,
                tx_parity,
                remote_origin,
                remote_client_key_id,
                attempt_id,
            )
        };
        let (old_state, tx_parity, remote_origin, remote_client_key_id, attempt_id) = emit;

        self.emit_state_change(qso_id, old_state, new_state).await;
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
        Ok(ManualCallOutcome::Advanced(attempt_id))
    }
}
