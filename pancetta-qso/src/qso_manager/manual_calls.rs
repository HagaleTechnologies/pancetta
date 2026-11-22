//! Operator-initiated calls: opening them, resolving repeat presses, keeping
//! them on the air every slot, and retiring them.
//!
//! `start_cq_manual`, `respond_to_cq_manual`, `respond_to_caller` and
//! `respond_to_caller_with_outcome` open manual QSOs, and
//! `enroll_manual_contest` enrolls them in an engaged contest.
//! `manual_open_adds_active_qso` says whether an open would add an active
//! QSO. `find_active_manual_qso_for` and
//! `find_recently_completed_manual_qso_for` resolve a repeated press to the
//! QSO that already exists, and `supersede_active_qsos_for` retires other
//! active QSOs with the same station on the same band.
//! `rearm_manual_calls_at` re-sends the frame in flight once per slot, for
//! manual QSOs and, up to `AUTO_RESEND_MAX_CALLS`, for autonomous ones.
//! `watchdog_pass` runs it before `check_timeouts_at`, which retires
//! calls that hit the manual watchdog and applies every QSO's per-state
//! timeouts.
//!
//! PAN-375 moved these methods here from `qso_manager.rs` unchanged. A method
//! that another file calls is `pub(super)`, the scope it had as a private
//! method of `qso_manager.rs`.

use super::*;
use jiff::SignedDuration;
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;

impl QsoManager {
    /// PAN-78: whether a MANUAL open of `callsign` at `step`, on `frequency`'s
    /// band, would ADD an active QSO, as opposed to resolving to one that
    /// already exists. Pass the same `frequency` the open call will receive
    /// (the computed TX offset):
    /// - `SeventyThree` never adds: it advances an active QSO, re-keys a
    ///   recently completed one, or opens directly at `Completed` (not active);
    /// - any active QSO with this callsign on this band → no: FIX 1 continues a
    ///   manual one, FIX 3 ([`Self::supersede_active_qsos_for`]) replaces any
    ///   other one-for-one;
    /// - `Rr73` with a manual QSO completed within
    ///   [`COMPLETED_QSO_REWORK_GRACE`] → no: FIX B re-keys it;
    /// - otherwise → yes.
    ///
    /// Must stay in lockstep with [`Self::respond_to_cq_with`] /
    /// [`Self::respond_to_caller`] (pinned by the
    /// `manual_open_predicate_matches_real_open_effect` test). Read-only: never
    /// takes a write lock.
    pub async fn manual_open_adds_active_qso(
        &self,
        callsign: &str,
        frequency: f64,
        step: pancetta_core::ResponseStep,
    ) -> bool {
        if step == pancetta_core::ResponseStep::SeventyThree {
            return false;
        }
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self
            .qsos_by_callsign
            .read()
            .await
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let has_active_same_band = {
            let qsos = self.qsos.read().await;
            ids.iter().any(|id| {
                qsos.get(id).is_some_and(|p| {
                    p.state.is_active()
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band
                })
            })
        };
        if has_active_same_band {
            return false;
        }
        // FIX B only exists on `respond_to_caller`'s non-Grid path; Grid
        // (ladder rank 0) is routed through `respond_to_cq_with`, which has
        // no completed-QSO re-key, so the rank gate excludes it here too.
        if Self::step_ladder_rank(step) >= Some(2)
            && self
                .find_recently_completed_manual_qso_for(
                    callsign,
                    frequency,
                    COMPLETED_QSO_REWORK_GRACE,
                )
                .await
                .is_some()
        {
            return false;
        }
        true
    }

    /// Start a **manual** (operator-initiated) CQ call as a real QSO.
    ///
    /// This is the engine half of the TUI `c` (StartCq) key. Unlike
    /// [`Self::start_cq`] (which marks the QSO [`CallInitiation::Auto`] for
    /// autonomous CQ), this marks it [`CallInitiation::Manual`] so that:
    ///
    /// 1. **We keep calling CQ every slot** until a station answers or the
    ///    CQ watchdog fires — [`Self::rearm_manual_calls_at`] re-emits a
    ///    `Cq` `MessageToSend` for a manual `CallingCq` QSO once per FT8
    ///    slot, bounded by `manual_call_max_calls` /
    ///    `manual_call_watchdog_minutes` (see [`Self::check_timeouts_at`]).
    /// 2. **When a caller answers, the exchange auto-sequences to
    ///    Completed + logs** — the auto-reply emitter in
    ///    [`Self::process_message`] is gated on `CallInitiation::Manual`, so
    ///    a manual CQer (us) automatically replies with our report → RR73 as
    ///    the caller's CqResponse → ReportAck arrive, exactly like the
    ///    operator-driven Callers path.
    ///
    /// We emit a `StateChanged` (Idle → CallingCq) so the coordinator's
    /// drop-stale-TX gate keys this QSO into `active_tx_qsos` (otherwise the
    /// TX worker would refuse to key PTT for it), and emit the first `Cq`
    /// `MessageToSend` immediately. `last_call_at` is stamped `now` so the
    /// per-slot rearm does not double-send the first CQ within the opening
    /// slot.
    ///
    /// `tx_parity` is the parity we want our CQ to land on, if fixed; `None`
    /// (no preference) is resolved to a CONCRETE parity ONCE here
    /// ([`Self::latch_cq_parity_if_none`]) and held for the life of the QSO —
    /// see that function's doc for the BUG 1 (transmit-every-window) history.
    /// (Calling CQ, we choose our own slot parity — there is no DX parity to
    /// oppose.)
    pub async fn start_cq_manual(
        &self,
        frequency: f64,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }
        let qso_id = Uuid::new_v4();
        let now = self.now_utc();
        // BUG 1 fix (docs/archive/qso-engine-bugs.md): latch a concrete parity ONCE at
        // QSO creation when the caller has no fixed preference (`None`, the
        // Auto-config default) — see `latch_cq_parity_if_none` doc.
        let tx_parity = Self::latch_cq_parity_if_none(tx_parity, now);

        let state = QsoState::CallingCq {
            frequency,
            started_at: now,
            call_count: 1,
        };

        let message = MessageType::Cq {
            callsign: self.config.our_callsign.clone(),
            grid: self.config.our_grid.clone(),
        };

        let raw_text = self.render_sent_text(&message);
        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: None,
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            // Operator pressed `c`: this is a MANUAL CQ. The manual
            // keep-calling watchdog re-arms our CQ every slot, and the
            // CallInitiation::Manual gate turns on the auto-reply emitter so
            // an answering station drives the exchange to completion.
            initiated_by: CallInitiation::Manual,
            // We called CQ → CQer role (drives the role-aware display ladder).
            role: QsoRole::Cqer,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            // `false` for operator-pressed `c` (local); `true` for a remote
            // operator's `startCq` routed via the station agent.
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            // CQ-path latch: resolved from our own preference, not an
            // observed DX parity — always self-consistent, never provisional.
            tx_parity_provisional: false,
        };
        self.enroll_manual_contest(&mut metadata);

        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            // Record the opening CQ as a Sent message so the TUI last-TX line
            // and `resend_last_tx` see it.
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata,
        };

        self.qsos.write().await.insert(qso_id, progress);

        // Emit a state change (Idle → CallingCq) so the coordinator's
        // drop-stale-TX gate keys this QSO into `active_tx_qsos`; without it
        // the TX worker would drop our CQ as "stale TX for an ended QSO".
        self.emit_state_change(qso_id, QsoState::Idle, state).await;

        // Emit the first CQ. Subsequent slots are owned by the per-slot
        // manual keep-call rearm (`rearm_manual_calls_at`).
        self.emit_event(QsoEvent::MessageToSend {
            qso_id,
            message,
            frequency,
            tx_parity,
            remote_origin,
            remote_client_key_id,
            attempt_id: Some(self.next_attempt_id()),
        })
        .await;

        info!("Started manual CQ on {:.1} Hz: {}", frequency, qso_id);

        Ok(qso_id)
    }

    /// Respond to a CQ call as an operator-initiated **manual** call.
    ///
    /// Bypasses the self-duplicate gate (the operator explicitly chose to
    /// call this station, e.g. to re-work it) and marks the QSO so the
    /// manual keep-calling watchdog re-arms a call every TX slot until the
    /// DX answers or the watchdog fires.
    pub async fn respond_to_cq_manual(
        &self,
        target_callsign: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_cq_with(
            target_callsign,
            frequency,
            dx_parity,
            CallInitiation::Manual,
            None,  // partner_freq computed by coordinator (T3); None = Tx=Rx fallback
            false, // TUI/DX-hunter manual call is LOCAL, never remote
            None,
        )
        .await
    }

    /// Enroll new manual attempts only. Exchange activation is separate
    /// from scoring identity: SC's Field Day exchange is not implemented.
    pub(super) fn enroll_manual_contest(&self, metadata: &mut QsoMetadata) {
        if metadata.initiated_by != CallInitiation::Manual {
            return;
        }
        if let Some(context) = self.config.contest_scoring.as_ref() {
            if context.in_period(metadata.start_time) {
                crate::contest::ledger::attach(metadata, context);
            }
        }
    }

    /// Respond to a station **calling us**, opening the exchange at an
    /// operator-chosen [`ResponseStep`] instead of always sending our grid.
    ///
    /// This is the engine half of the TUI "Callers" panel. The operator picks
    /// a caller and pancetta classifies what they sent (their CQ/grid →
    /// `Grid`, their report → `ReportAck`, etc.), with a manual override. We
    /// reuse all of [`respond_to_cq_with`](Self::respond_to_cq_with)'s manual
    /// machinery — self-duplicate-gate bypass, superseding a same-call QSO,
    /// latching `tx_parity = dx_parity.opposite()`, and per-slot keep-calling
    /// under the manual watchdog — but set the *initial* [`QsoState`] and emit
    /// the *first* [`MessageType`] according to `step`:
    ///
    /// | step         | initial state          | first message      |
    /// |--------------|------------------------|--------------------|
    /// | `Grid`       | `RespondingToCq`       | `CqResponse` (grid)|
    /// | `Report`     | `SendingReport`        | `SignalReport`     |
    /// | `ReportAck`  | `SendingReport`        | `ReportAck`        |
    /// | `Rr73`       | `WaitingForConfirmation` | `FinalConfirmation` |
    /// | `SeventyThree` | `Completed`          | `SeventyThree` (+ QsoCompleted) |
    ///
    /// `our_snr_of_them` is our measurement of the caller's signal; it
    /// produces the report we send (rounded, clamped to −30..50, defaulting to
    /// −15 if absent). `their_report` is the report they sent us, if known —
    /// used to populate the `their_report` field of the `SendingReport` /
    /// `WaitingForConfirmation` state for `ReportAck`/`Rr73` opens.
    ///
    /// `partner_freq` — when `Some(f)`, our TX offset (`frequency`) differs from
    /// the DX's RX offset `f`. Set by the coordinator when the operator holds a
    /// custom TX offset or when de-confliction nudges us away from the DX's freq.
    /// `None` = Tx=Rx (legacy behavior, unchanged).
    ///
    /// The `Grid` step is exactly equivalent to `respond_to_cq_manual`, so the
    /// DX-Hunter path (which still uses `StartQso` → `respond_to_cq_manual`) is
    /// unaffected.
    // arg count is intrinsic to the QSO-open signature
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_caller(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<QsoId, QsoManagerError> {
        self.respond_to_caller_with_outcome(
            target,
            frequency,
            dx_parity,
            step,
            our_snr_of_them,
            their_report,
            partner_freq,
            remote_origin,
            remote_client_key_id,
        )
        .await
        .map(|(qso_id, _)| qso_id)
    }

    /// [`Self::respond_to_caller`], also reporting whether a frame was emitted (PAN-138).
    #[allow(clippy::too_many_arguments)]
    pub async fn respond_to_caller_with_outcome(
        &self,
        target: String,
        frequency: f64,
        dx_parity: Option<pancetta_core::slot::SlotParity>,
        step: pancetta_core::ResponseStep,
        our_snr_of_them: Option<f32>,
        their_report: Option<i8>,
        partner_freq: Option<f64>,
        remote_origin: bool,
        remote_client_key_id: Option<String>,
    ) -> Result<(QsoId, ManualCallOutcome), QsoManagerError> {
        use pancetta_core::ResponseStep;

        // PAN-23: refuse FT8's literal unresolved-hash placeholder "<...>"
        // up front, before branching on `step`. This is the exact failure
        // mode observed on-air (production logs 2026-08-20..22): an
        // operator selected "<...>" from the TUI Callers panel at step
        // ReportAck, pancetta queued/started the QSO, and the eventual
        // encode attempt failed with "callsign '<...>' cannot be
        // represented in any FT8 message format". The `step == Grid` branch
        // below would also be caught by the identical guard in
        // `respond_to_cq_with`, but checking here first (a) covers every
        // other step (Report/ReportAck/Rr73/SeventyThree) that bypasses
        // `respond_to_cq_with` entirely, and (b) fails fast before any
        // logging/queueing work happens. Defense in depth: the TUI already
        // filters "<...>" out of the Callers list an operator can select
        // from (PAN-16/PAN-23); this guards any other path — present or
        // future — that might route a `RespondToCaller` command here.
        if target == "<...>" {
            return Err(QsoManagerError::InvalidCallsign { callsign: target });
        }

        // Grid is exactly the historical manual-call behavior; route through
        // the existing path so there is a single source of truth for it.
        if step == ResponseStep::Grid {
            return self
                .respond_to_cq_with_outcome(
                    target,
                    frequency,
                    dx_parity,
                    CallInitiation::Manual,
                    partner_freq,
                    remote_origin,
                    remote_client_key_id,
                )
                .await;
        }

        if self.config.our_callsign == "NOCALL" || self.config.our_callsign == "N0CALL" {
            return Err(QsoManagerError::Configuration {
                message: format!(
                    "Cannot transmit with placeholder callsign '{}'. Configure your callsign first.",
                    self.config.our_callsign
                ),
            });
        }

        // Our report of their signal (the report WE send), same formula the
        // auto-sequencer uses in `MessageExchange::generate_response`, but
        // defaulting to -15 when we have no measurement.
        let our_report: SignalReport = our_snr_of_them
            .map(|s| (s.round() as i8).clamp(-30, 50))
            .unwrap_or(-15);
        // Their report of us (only meaningful for ReportAck/Rr73 opens).
        let their_report_val: SignalReport = their_report.unwrap_or(-15);

        // FIX 1: if we already have an ACTIVE manual QSO with this caller on
        // this band, CONTINUE it instead of superseding/duplicating. Mashing a
        // context reply on a station already in progress must keep ONE QSO per
        // (callsign, band).
        //   - If the requested step is AHEAD of the existing QSO's current
        //     ladder stage (e.g. the DX now sent RR73 → SeventyThree while we
        //     were in SendingReport), advance the EXISTING QSO to emit that
        //     step.
        //   - If it matches (or is behind) the current stage, just re-send the
        //     existing QSO's current outbound (idempotent keep-call).
        // Either way we return the existing id and never create a second QSO.
        if let Some(existing_id) = self.find_active_manual_qso_for(&target, frequency).await {
            let existing_rank = {
                let qsos = self.qsos.read().await;
                qsos.get(&existing_id)
                    .and_then(|p| Self::ladder_rank(&p.state))
            };
            let requested_rank = Self::step_ladder_rank(step);
            match (existing_rank, requested_rank) {
                (Some(cur), Some(req)) if req > cur => {
                    info!(
                        "Context reply to {} at step {:?} — advancing existing QSO {} \
                         (ahead of its current stage)",
                        target, step, existing_id
                    );
                    // Round 13 / PAN-138: advancing can complete and log a
                    // contact. Check permission inside the advance's write
                    // guard before changing identity or exchange state.
                    let outcome = self
                        .advance_existing_qso_to_step(
                            existing_id,
                            &target,
                            frequency,
                            step,
                            our_report,
                            their_report_val,
                            remote_origin,
                            remote_client_key_id,
                        )
                        .await?;
                    return Ok((existing_id, outcome));
                }
                _ => {
                    info!(
                        "Context reply to {} at step {:?} — re-sending existing QSO {} \
                         current outbound (idempotent keep-call)",
                        target, step, existing_id
                    );
                    let outcome = self
                        .resend_existing_if_permitted(
                            existing_id,
                            remote_origin,
                            remote_client_key_id,
                        )
                        .await;
                    return Ok((existing_id, outcome));
                }
            }
        }

        // FIX B (one-QSO-per-(callsign,band) close idempotency,
        // docs/qso-tx-deep-review-2026-07-18.md): no ACTIVE QSO with this
        // caller (FIX 1 above didn't fire) — but if the requested step is a
        // CLOSE step (Rr73/SeventyThree) and we JUST completed a manual QSO
        // with this exact station on this band, within the grace window, this
        // is the operator mashing "send RR73"/"send 73" again after the QSO
        // already finished (observed on-air: 3 SeventyThree presses in 8s
        // produced 4 duplicate ADIF log entries for the same contact). Re-key
        // the EXISTING completed QSO's last frame instead of opening a new
        // one — never spawn a sibling `QsoId`, never emit a second
        // `QsoCompleted`. Grid/Report/ReportAck (ladder rank < 2) fall
        // through to the normal new-QSO path below: a fresh call at those
        // steps within the grace window is the deliberate legitimate-rework
        // case (e.g. working the same station again on a different exchange).
        if Self::step_ladder_rank(step) >= Some(2) {
            if let Some(existing_id) = self
                .find_recently_completed_manual_qso_for(
                    &target,
                    frequency,
                    COMPLETED_QSO_REWORK_GRACE,
                )
                .await
            {
                info!(
                    "Context reply to {} at step {:?} — DX already worked within the \
                     grace window, re-sending existing QSO {}'s last frame",
                    target, step, existing_id
                );
                let outcome = self
                    .resend_existing_if_permitted(existing_id, remote_origin, remote_client_key_id)
                    .await;
                return Ok((existing_id, outcome));
            }
        }

        // Round-16 review (Codex P1): this identity-bound permission check
        // (round-15's fix) MUST run before `supersede_active_qsos_for`
        // below, not after — an autonomous same-call/band QSO is invisible
        // to the manual-QSO lookups above (FIX 1), so a denied close-step
        // request could otherwise reach here, supersede (cancel) that
        // ongoing autonomous exchange, and ONLY THEN get refused itself —
        // destroying a real QSO in progress and replacing it with nothing.
        //
        // Round-15 review (Codex P1): unlike the EXISTING-QSO ladder-advance
        // fix (round 13), a brand-new QSO opened directly at a close step
        // (`SeventyThree`) is created ALREADY `Completed` — the state-build
        // match below sets that unconditionally, and completing immediately
        // emits `QsoCompleted` (ADIF/logbook entry) further down. Doing that
        // for a client the TX layer will deny is worse than the existing-QSO
        // case: the resulting QSO is already terminal, so it can never be
        // retried once that client becomes authorized, and the false log
        // entry cannot be un-logged. Refuse outright rather than create
        // anything — a later, actually-authorized call creates it fresh.
        //
        // PAN-139 (Codex round 23 on PR #362): the same refusal now covers
        // BOTH close steps (`step_ladder_rank >= 2`, the identical predicate
        // FIX B uses above), not just `SeventyThree`. A denied `Rr73` open
        // used to mutate unconditionally: the TX arm gate correctly dropped
        // the unauthorized `FinalConfirmation` frame at key-time, but the
        // QSO was already stranded in `WaitingForConfirmation` with a
        // phantom `Sent` record for a frame that never aired — a state the
        // rearm loop deliberately never retries (`rearm_manual_calls_at`
        // has no keep-call concept past RR73; the DX's own next frame drives
        // that stage), so `confirmation_timeout` retired it as a silently
        // lost QSO. Refusing outright lets a later authorized call open it
        // fresh, and the deferred admission queue already re-queues and
        // retries the refusal within its TTL. `Report`/`ReportAck` opens are
        // NOT covered: their states are rearm-eligible and
        // `frame_reaches_the_air` already refuses to compound a denial, so
        // they self-correct.
        //
        // PAN-280: the check now runs under the SAME `qsos` write guard as the
        // supersede and the insert below (no `qsos` release between them), so
        // no other `qsos` holder can run between the check and the insert.
        // Everything the open needs is built before that guard is taken; the
        // guard's only await is `qsos_by_callsign`, taken in the existing
        // `qsos` -> `qsos_by_callsign` order. A clock-based arm expiry right
        // after the check is still caught only by the TX worker at key time.
        let qso_id = Uuid::new_v4();
        let now = self.now_utc();
        // See the matching comment in `respond_to_cq_with`: a `None`
        // `dx_parity` (no observed decode for this caller yet) falls back to
        // the "nearest next slot" latch and is marked provisional so the
        // first real decode from this partner can refine it.
        let (tx_parity, tx_parity_provisional) = match dx_parity {
            Some(p) => (Some(p.opposite()), false),
            None => (Self::latch_cq_parity_if_none(None, now), true),
        };

        // Build (initial_state, first_message) for the chosen step.
        let (state, message): (QsoState, MessageType) = match step {
            ResponseStep::Grid => unreachable!("Grid handled above"),
            ResponseStep::Report => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: None,
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::SignalReport {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::ReportAck => (
                QsoState::SendingReport {
                    their_callsign: target.clone(),
                    their_report: Some(their_report_val),
                    our_report,
                    frequency,
                    started_at: now,
                },
                MessageType::ReportAck {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                    report: our_report,
                },
            ),
            ResponseStep::Rr73 => (
                QsoState::WaitingForConfirmation {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    started_at: now,
                },
                MessageType::FinalConfirmation {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
            ResponseStep::SeventyThree => (
                QsoState::Completed {
                    their_callsign: target.clone(),
                    their_report: their_report_val,
                    our_report,
                    frequency,
                    grid_square: None,
                    completed_at: now,
                    duration_seconds: 0,
                },
                MessageType::SeventyThree {
                    to_station: target.clone(),
                    from_station: self.config.our_callsign.clone(),
                },
            ),
        };

        let mut metadata = QsoMetadata {
            qso_id,
            our_callsign: self.config.our_callsign.clone(),
            their_callsign: Some(target.clone()),
            frequency,
            completed_rf_frequency_hz: None,
            mode: self.config.active_mode.clone(),
            first_answer_at: None,
            start_time: now,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares {
                ours: self.config.our_grid.clone(),
                theirs: None,
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            adif_field_types: HashMap::new(),
            notes: None,
            tx_parity,
            initiated_by: CallInitiation::Manual,
            // Replying to a station calling us → Caller role.
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            // When our TX offset != the DX's RX offset (Hold mode / de-conflict),
            // the caller supplies `partner_freq = Some(dx_freq)` so the relevance
            // gate routes the DX's reply (which arrives at their audio offset) to
            // this QSO. `None` = Tx=Rx (legacy behavior, unchanged).
            partner_freq,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin,
            remote_client_key_id: remote_client_key_id.clone(),
            tx_parity_provisional,
        };
        self.enroll_manual_contest(&mut metadata);

        // If we are opening at the close (SeventyThree → Completed), stamp the
        // completion reports/end-time so the logged record is well-formed.
        let is_completed_open = matches!(state, QsoState::Completed { .. });
        if is_completed_open {
            metadata.reports = SignalReports {
                sent: Some(our_report),
                received: Some(their_report_val),
            };
            metadata.end_time = Some(now);
        }

        let raw_text = self.render_sent_text(&message);

        // If we are opening at the close, the logged record carries the real
        // on-air RF frequency (dial + offset), mirroring the completion path
        // in `process_message_for_qso`. Only the event's clone gets
        // `frequency` overwritten with the RF value -- the STORED entry's
        // `frequency` must stay the audio offset (PAN-25 round 2: see
        // `QsoMetadata::frequency`'s doc comment for why). The stored entry
        // gets `completed_rf_frequency_hz` instead, for the recent-completed
        // suppression's band check. PAN-280: stamped before the insert, under
        // the same guard, instead of by a second write lock afterwards.
        let mut stored_metadata = metadata.clone();
        if is_completed_open {
            let rx_dial = self.dial_frequency_hz.load(Ordering::Relaxed);
            let split = self.split_tx_frequency_hz.load(Ordering::Relaxed);
            let dial = effective_tx_dial(rx_dial, split);
            if dial > 0 {
                let rf_frequency = metadata.frequency + dial as f64;
                metadata.frequency = rf_frequency;
                stored_metadata.completed_rf_frequency_hz = Some(rf_frequency);
            }
        }
        let closing_text = raw_text.clone();
        let progress = QsoProgress {
            state: state.clone(),
            state_history: vec![],
            messages: vec![QsoMessage {
                timestamp: now,
                direction: MessageDirection::Sent,
                message_type: message.clone(),
                raw_text,
                signal_strength: None,
                frequency,
            }],
            metadata: stored_metadata,
        };
        // Captured before the move into the map below so the held
        // QsoCompleted for an open at the close still has it (Layer 2
        // timeline persistence).
        let opening_state_history = progress.state_history.clone();
        let opening_messages = progress.messages.clone();

        // PAN-280: check, supersede and insert under ONE `qsos` write guard.
        // Round-16 / PAN-139 ordering kept: the permission check runs before
        // any supersede mutation, so a denied open mutates nothing.
        let new_band = crate::utils::frequency_to_band(frequency);
        let (superseded, attempt_id) = {
            let mut qsos = self.qsos.write().await;
            if Self::step_ladder_rank(step) >= Some(2)
                && remote_origin
                && !(self.remote_tx_permitted)(remote_client_key_id.as_deref())
            {
                return Err(QsoManagerError::Internal {
                    message: format!(
                        "refusing to open a brand-new QSO with {target} at close step {step:?}: \
                         the bound remote client is not currently TX-permitted"
                    ),
                });
            }
            // Manual: supersede any same-call QSO on this band, then insert
            // the new one (no duplicate gate — the operator explicitly chose
            // this caller). With FIX 1 above this only fires when no ACTIVE
            // QSO remains (e.g. a lingering terminal record), so it should
            // rarely trigger now.
            let candidate_ids = self
                .qsos_by_callsign
                .read()
                .await
                .get(&target.to_uppercase())
                .cloned()
                .unwrap_or_default();
            let superseded =
                Self::supersede_active_locked(&mut qsos, &candidate_ids, &new_band, self.now_utc());
            qsos.insert(qso_id, progress);
            let attempt_id = self.next_attempt_id();
            // PAN-280: an open at the close holds its QsoCompleted until the
            // 73 airs. Stored under the same guard as the insert and before
            // the MessageToSend below, so its TX report always finds it.
            if is_completed_open {
                self.hold_completion(
                    qso_id,
                    PendingCompletion {
                        attempt_ids: vec![attempt_id],
                        raw_text: closing_text,
                        frequency_hz: frequency,
                        remote_origin,
                        remote_client_key_id: remote_client_key_id.clone(),
                        queued_at: now,
                        completed_state: state.clone(),
                        restore: CompletionRestore::RetireOpen,
                        metadata,
                        state_history: opening_state_history,
                        messages: opening_messages,
                    },
                );
            }
            (superseded, attempt_id)
        };
        self.emit_superseded(&target, &new_band, superseded).await;
        self.add_callsign_mapping(&target, qso_id).await;

        // See the matching comment in `respond_to_cq_with`: emit the initial
        // StateChanged BEFORE the first MessageToSend so the coordinator's
        // `active_tx_qsos` set has this qso_id inserted before the first
        // scheduled TransmitRequest reaches the Step 4b PTT gate. A
        // `Completed` open (SeventyThree) is not an active state, but emitting
        // the StateChanged is still correct/harmless — the gate's post-
        // completion grace window keeps the final-73 TransmitRequest live (and
        // the QsoCompleted event below drives the grace insert+removal anyway).
        self.emit_state_change(qso_id, QsoState::Idle, state.clone())
            .await;

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

        info!(
            "Responding to caller {} on {:.1} Hz at step {:?}: {}",
            target, frequency, step, qso_id
        );

        // PAN-280: an open at the close no longer logs here. Its QsoCompleted,
        // with the RF-stamped metadata built above, is held until the 73's
        // `TxFrameSent` (`confirm_completion_aired`).

        Ok((qso_id, ManualCallOutcome::Opened(attempt_id)))
    }

    /// FIX 1: return the id of an ACTIVE (non-terminal), MANUAL-initiated QSO
    /// with `callsign` on the same band as `frequency`, if one exists. Used by
    /// the operator re-call paths so mashing Call/Space on a station already in
    /// progress CONTINUES the one QSO rather than superseding it / spawning a
    /// duplicate. Case-insensitive callsign match (matching the callsign-map
    /// keying); "same band" derived via [`crate::utils::frequency_to_band`]
    /// exactly like [`Self::supersede_active_qsos_for`]. When several match
    /// (shouldn't happen post-FIX-1, but be robust), the most-recently-started
    /// one wins.
    pub(super) async fn find_active_manual_qso_for(
        &self,
        callsign: &str,
        frequency: f64,
    ) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(frequency);
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| {
                    let matches = p.state.is_active()
                        && p.metadata.initiated_by == CallInitiation::Manual
                        && crate::utils::frequency_to_band(p.metadata.frequency) == want_band;
                    matches.then_some((id, p.metadata.start_time))
                })
            })
            .max_by_key(|(_, started)| *started)
            .map(|(id, _)| id)
    }

    /// FIX B: return the id of the most-recently-COMPLETED, MANUAL-initiated
    /// QSO with `callsign` on the same band as `frequency`, if it completed
    /// within `within` of now. Used by [`Self::respond_to_caller`] so a
    /// close-step (`Rr73`/`SeventyThree`) reply for a station we JUST finished
    /// working — e.g. the operator mashing "send 73" after already
    /// completing — resolves to the SAME QSO object (re-key its last frame)
    /// instead of spawning a sibling with a brand-new `QsoId` (the double-73
    /// duplicate-ADIF-entry bug). Same `qsos_by_callsign` + band-match
    /// approach as [`Self::find_active_manual_qso_for`]; when several
    /// completed matches exist, the newest (`completed_at`) wins.
    ///
    /// Delegates to [`Self::find_recently_completed_manual_qso_for_at`] with
    /// the real clock; see that function for the testable, explicit-`now`
    /// variant (mirroring [`Self::check_timeouts_at`]'s pattern) — a 45s
    /// real-time grace window is awkward to exercise with `tokio::time::sleep`
    /// in a unit test.
    pub(super) async fn find_recently_completed_manual_qso_for(
        &self,
        callsign: &str,
        frequency: f64,
        within: SignedDuration,
    ) -> Option<QsoId> {
        self.find_recently_completed_manual_qso_for_at(callsign, frequency, within, self.now_utc())
            .await
    }

    /// Explicit-`now` variant of [`Self::find_recently_completed_manual_qso_for`]
    /// (for testability — see its doc comment).
    ///
    /// PAN-25 (Codex round-1 review): `frequency` here is an audio offset,
    /// but the COMPLETED QSOs this scans against now have their
    /// `metadata.frequency` stamped with the true RF frequency (dial +
    /// offset) at completion time — `frequency` must be adjusted the same
    /// way for the band comparison below to mean anything.
    pub(super) async fn find_recently_completed_manual_qso_for_at(
        &self,
        callsign: &str,
        frequency: f64,
        within: SignedDuration,
        now: UtcTimestamp,
    ) -> Option<QsoId> {
        let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
        let key = callsign.to_uppercase();
        let ids = self.qsos_by_callsign.read().await.get(&key).cloned()?;
        let qsos = self.qsos.read().await;
        ids.into_iter()
            .filter_map(|id| {
                qsos.get(&id).and_then(|p| match &p.state {
                    QsoState::Completed { completed_at, .. }
                        if p.metadata.initiated_by == CallInitiation::Manual
                            && Self::completed_band(p) == want_band
                            && now - *completed_at <= within =>
                    {
                        Some((id, *completed_at))
                    }
                    _ => None,
                })
            })
            .max_by_key(|(_, completed_at)| *completed_at)
            .map(|(id, _)| id)
    }

    /// FIX 3: retire every active (non-terminal) QSO with `callsign` on the
    /// same band as `frequency`, marking each `Failed{Superseded}` and
    /// clearing its callsign mapping. Emits a `StateChanged` per superseded
    /// QSO (terminal Failed → AP/snapshot clears in the coordinator). Called
    /// just before a new manual QSO is created so only the most-recent one
    /// remains active.
    ///
    /// "Same band" is derived from the QSO frequency via
    /// [`crate::utils::frequency_to_band`]. Within a single operating session
    /// every active QSO shares the RF band, so in practice this collapses to
    /// "same callsign"; deriving the band keeps the rule correct should
    /// per-QSO RF frequencies ever be threaded through.
    pub(super) async fn supersede_active_qsos_for(&self, callsign: &str, frequency: f64) {
        let new_band = crate::utils::frequency_to_band(frequency);
        // Look up case-insensitively (Batch 2 #7) so a case/format mismatch
        // can't leak a duplicate active QSO past supersede.
        let key = callsign.to_uppercase();
        let superseded = {
            let mut qsos = self.qsos.write().await;
            let candidate_ids = self
                .qsos_by_callsign
                .read()
                .await
                .get(&key)
                .cloned()
                .unwrap_or_default();
            Self::supersede_active_locked(&mut qsos, &candidate_ids, &new_band, self.now_utc())
        };
        self.emit_superseded(callsign, &new_band, superseded).await;
    }

    /// PAN-280: the mutating core of [`Self::supersede_active_qsos_for`],
    /// run under a caller's `qsos` write guard so a new QSO's permission
    /// check, supersede and insert share one guard. Marks every active
    /// candidate on `new_band` `Failed{Superseded}` and returns what
    /// [`Self::emit_superseded`] announces once the guard drops.
    fn supersede_active_locked(
        qsos: &mut HashMap<QsoId, QsoProgress>,
        candidate_ids: &[QsoId],
        new_band: &str,
        now: UtcTimestamp,
    ) -> Vec<SupersededQso> {
        let mut superseded = Vec::new();
        for qso_id in candidate_ids {
            let Some(progress) = qsos.get_mut(qso_id) else {
                continue;
            };
            if !(progress.state.is_active()
                && crate::utils::frequency_to_band(progress.metadata.frequency) == *new_band)
            {
                continue;
            }
            let old_state = progress.state.clone();
            progress.state = QsoState::Failed {
                reason: QsoFailureReason::Superseded,
                failed_at: now,
                last_state: Box::new(old_state.clone()),
            };
            // Capture metadata (Batch 4, SM-F5 — needed for QsoFailed) and
            // the timeline (Layer 2 persistence) — superseded QSOs stay in
            // the map and are picked up later by `cleanup_completed_qsos`,
            // but emitting it here too means a persistence subscriber
            // doesn't have to wait up to an hour for it.
            superseded.push(SupersededQso {
                qso_id: *qso_id,
                old_state,
                new_state: progress.state.clone(),
                metadata: progress.metadata.clone(),
                state_history: progress.state_history.clone(),
                messages: progress.messages.clone(),
            });
        }
        superseded
    }

    /// PAN-280: announce what [`Self::supersede_active_locked`] retired, in
    /// the order the supersede always used: `StateChanged`, `QsoFailed`,
    /// then the callsign mapping removal.
    async fn emit_superseded(
        &self,
        callsign: &str,
        new_band: &str,
        superseded: Vec<SupersededQso>,
    ) {
        for s in superseded {
            self.emit_state_change(s.qso_id, s.old_state.clone(), s.new_state)
                .await;
            // SM-F5: also emit QsoFailed alongside StateChanged — see the
            // cancel_qso comment for why (dead priority-scoring backoff
            // consumer; idempotent HashSet::remove on the coordinator
            // side makes emitting both events safe).
            self.emit_event(QsoEvent::QsoFailed {
                qso_id: s.qso_id,
                reason: QsoFailureReason::Superseded,
                last_state: s.old_state,
                metadata: s.metadata,
                state_history: s.state_history,
                messages: s.messages,
            })
            .await;
            self.remove_callsign_mapping(callsign, s.qso_id).await;
            info!(
                "Superseded older active QSO {} with {} on band {} (re-call)",
                s.qso_id, callsign, new_band
            );
        }
    }

    /// For every manual-initiated QSO still in `CallingCq`, `WaitingForReport`,
    /// `RespondingToCq`, or `SendingReport` (waiting for the DX to come
    /// back / copy our last frame), re-emit that frame at most once per FT8
    /// slot so the operator keeps calling every slot until the DX answers or
    /// the manual watchdog fires. The TX scheduler downstream resolves slot
    /// parity from the `tx_parity` latched on the QSO, so re-emitting more
    /// often than a slot is harmless, but we gate to ~one per slot to avoid
    /// flooding the bus.
    ///
    /// SM-F6: autonomous (`CallInitiation::Auto`) QSOs in `RespondingToCq` or
    /// `SendingReport` are ALSO re-armed here now, but under a small,
    /// hardcoded cap (`AUTO_RESEND_MAX_CALLS`) distinct from Manual's
    /// operator-supervised `manual_call_max_calls` — see the constant's doc
    /// comment for the safety rationale. `CallingCq` and `WaitingForReport`
    /// remain Manual-only (an Auto QSO doesn't reach those rearm-eligible
    /// states via a self-initiated CQ opening; extending that is a separate,
    /// out-of-scope initiative).
    ///
    /// Re-arming increments `call_count` and updates `last_call_at`; the
    /// watchdog ([`Self::check_timeouts_at`]) reads `call_count` and
    /// `first_call_at` (Manual) or the per-state `report_timeout` (Auto) to
    /// decide when to stop. The stall streak (`stall_cycles`) counts once
    /// per own-parity TX slot these re-sends feed, not once per emission
    /// (PAN-128).
    /// Failed TX outcomes can reverse accounting via [`Self::roll_back_unsent_attempt_at`].
    /// The ledger emission anchor preserves cadence even when accounting is restored.
    pub async fn rearm_manual_calls_at(&self, now: UtcTimestamp) {
        // Re-arm only when at least one transmit slot has elapsed since the
        // last call, to keep ~one call per slot.
        //
        // PAN-72 (Codex round 7 on PR #350, finding 5): the slot length is the
        // ACTIVE MODE's, not a hardcoded 15 s. FT4 (7.5 s) and FT2 (3.2 s)
        // previously advanced this gate — and with it the `stall_cycles`
        // counter below, which the documented threshold measures in *slots* —
        // only every other (or every fifth) real slot.
        //
        // Round-7 re-review: read the coordinator's LIVE `active_slot_ns`
        // atomic, never the cloned `config.active_mode` string. This loop runs
        // on a `start()`-time clone, and `Clone` copies `config` by value, so
        // a config-derived cadence silently missed every runtime Shift+M
        // switch in both directions. With no injected source the atomic holds
        // `DEFAULT_SLOT_NS`, so mode=FT8 stays byte-identical; see
        // [`QsoManager::set_active_slot_ns_source`].
        let slot_millis = self.rearm_slot_millis();

        // SM-F6: bounded resend cap for AUTONOMOUS QSOs -- see the
        // module-level [`AUTO_RESEND_MAX_CALLS`] const's doc comment (hoisted
        // there in PAN-72 round 14 so `apply_tx_offset_switch` can reference
        // the SAME value).

        // Each entry carries the exact MessageType to re-emit so a
        // RespondingToCq QSO re-sends the call (CqResponse) while a
        // SendingReport QSO re-sends our R-report (ReportAck) — FIX 4.
        type RecallEntry = (
            QsoId,
            MessageType,
            f64,
            Option<pancetta_core::slot::SlotParity>,
            bool,
            Option<String>,
            TxAttemptId,
        );
        let mut to_recall: Vec<RecallEntry> = Vec::new();

        // PAN-72: TX-offset actions (Switch/Revert) a stall-tripped QSO
        // needs, collected here and emitted after the write lock below is
        // dropped — same reason `to_recall` above is collect-then-emit.
        let mut offset_actions_to_emit: Vec<(QsoId, OffsetAction, u32)> = Vec::new();

        // PAN-72 (Codex round 2 on PR #350, finding 5): read the global TX
        // policy ONCE for this pass. Under `Disabled` the coordinator's
        // `tx_hard_mute_reason` blocks every frame this loop re-emits, so
        // nothing goes on the air and the DX's continued silence is not
        // evidence of anything. See the `tx_policy` field's doc comment.
        let tx_muted = !pancetta_core::TxPolicy::from_u8(
            self.tx_policy.load(std::sync::atomic::Ordering::Relaxed),
        )
        .allows_any_tx();

        // PAN-72 (Codex round 7 on PR #350, finding 3): the SECOND, entirely
        // independent gate a rearmed frame must clear — the station-agent
        // armed-TX check the TX worker runs immediately before PTT for every
        // `TxOrigin::Remote` request. `TxPolicy` staying `Full` says nothing
        // about it: an arm expiry or an explicit remote disarm drops every
        // frame a `remote_origin` QSO rearms while the policy above still
        // reads "TX allowed". Read ONCE per pass, like `tx_policy`, and
        // consulted below only for QSOs that are actually remote-origin. See
        // the `remote_tx_permitted` field's doc comment — read-only evidence,
        // never a TX gate. PAN-91 review follow-up: evaluated PER-QSO below
        // (not once per pass) since it now takes the QSO's own bound client
        // identity — a QSO bound to client A must not read as reached-the-air
        // just because client B happens to be armed.
        let remote_tx_permitted = &self.remote_tx_permitted;

        // PAN-72 Fix E (Codex round 10, thread on `qso_manager.rs:6335`): the
        // THIRD independent "did this frame actually reach the air" check —
        // the TX worker's own pre-PTT Hamlib hard-mute (restart, command loop
        // not ready, an undelivered pending frequency/split command, or an
        // in-flight Hamlib command). Read ONCE per pass, like `tx_policy` and
        // `remote_tx_permitted` above. See the `hamlib_hard_muted` field's
        // doc comment.
        let hamlib_hard_muted = (self.hamlib_hard_muted)();

        {
            let mut qsos = self.qsos.write().await;
            let mut ledger = self
                .keep_call_ledger
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cutoff =
                now - SignedDuration::from_millis(UNSENT_ATTEMPT_RETENTION_SLOTS * slot_millis);
            for entry in ledger.values_mut() {
                entry
                    .attempts
                    .retain(|attempt| attempt.emitted_at >= cutoff);
            }
            for (&qso_id, progress) in qsos.iter_mut() {
                let is_manual = progress.metadata.initiated_by == CallInitiation::Manual;
                let message = match &progress.state {
                    // Manual CQ (operator `c`): keep calling CQ every slot
                    // until a station answers (→ WaitingForReport, handled by
                    // the normal sequence) or the watchdog retires us. Manual
                    // only — SM-F6 deliberately does NOT extend autonomous
                    // keep-calling to a self-CQ opening; that's a separate,
                    // out-of-scope initiative (Auto QSOs don't currently
                    // keep-call a CQ this way at all).
                    QsoState::CallingCq { .. } if is_manual => MessageType::Cq {
                        callsign: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SM-F4: the CQer's report was sent on the CallingCq →
                    // WaitingForReport transition; if the caller never copied
                    // it and repeats their grid, re-send the SAME latched
                    // value (no SNR jitter — see the (WaitingForReport,
                    // CqResponse) regression arm's doc comment). Manual only,
                    // for the same reason CallingCq above is Manual only: an
                    // Auto QSO never reaches WaitingForReport via a
                    // rearm-eligible CQ opening.
                    QsoState::WaitingForReport {
                        their_callsign,
                        our_report,
                        ..
                    } if is_manual => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // SM-F6: re-send our call/grid every slot while waiting
                    // for the DX to come back. Now covers BOTH Manual
                    // (keep-calling, long watchdog) and Auto (a single
                    // bounded resend, capped below) — an autonomous pounce
                    // whose CqResponse the DX missed previously got ZERO
                    // re-sends and died silently at the 30s report_timeout.
                    QsoState::RespondingToCq {
                        target_callsign, ..
                    } => MessageType::CqResponse {
                        calling_station: target_callsign.clone(),
                        responding_station: self.config.our_callsign.clone(),
                        grid: self.config.our_grid.clone(),
                    },
                    // SendingReport is entered two ways with DIFFERENT last-sent
                    // frames, so the rearm must re-send the SAME rung we're
                    // actually at, not always escalate to R-report:
                    //   - their_report == None: we're at the plain-report rung
                    //     (entered via the stuck-at-grid arm — RespondingToCq +
                    //     CqResponse — or a manual Report-step answer). We sent
                    //     a plain SignalReport (-NN) and have NOT yet heard the
                    //     DX's report, so keep-calling MUST re-send -NN, never
                    //     R-NN — escalating here was the KJ5NJF bug: we'd
                    //     advance our own TX to R-report with zero DX input.
                    //   - their_report == Some: FIX 4 — we sent R and the DX
                    //     re-sent their report (they did not copy our R) —
                    //     re-send our R-report each slot, under the SAME
                    //     watchdog, until the DX advances (RR73) or the
                    //     watchdog retires us.
                    // SM-F6: also covers Auto (a single bounded resend,
                    // capped below) — the second of the two Auto-eligible
                    // states (RespondingToCq, SendingReport) per this fix.
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: None,
                        our_report,
                        ..
                    } => MessageType::SignalReport {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    QsoState::SendingReport {
                        their_callsign,
                        their_report: Some(_),
                        our_report,
                        ..
                    } => MessageType::ReportAck {
                        to_station: their_callsign.clone(),
                        from_station: self.config.our_callsign.clone(),
                        report: *our_report,
                    },
                    // Any later state: the normal sequence drives the rest.
                    _ => continue,
                };

                // Stop re-arming once the watchdog bound is reached; the
                // watchdog itself will retire the QSO on its own pass.
                // Manual gets the long, operator-supervised bound; Auto gets
                // the small, hardcoded, strictly-bounded cap above (SM-F6).
                let max_calls = if is_manual {
                    self.config.timeouts.manual_call_max_calls
                } else {
                    AUTO_RESEND_MAX_CALLS
                };
                if progress.metadata.call_count >= max_calls {
                    continue;
                }

                // Millisecond resolution so a 7.5 s FT4 slot is expressible;
                // for FT8's whole-second 15_000 ms this is exactly the
                // previous `num_seconds() < 15` test (both truncate toward
                // zero, and the threshold is a whole number of seconds).
                let elapsed_since_last = progress
                    .metadata
                    .last_call_at
                    .max(ledger.get(&qso_id).and_then(|entry| entry.last_emitted_at))
                    .map(|t| (now - t).as_millis_i64())
                    .unwrap_or(i64::MAX);
                if elapsed_since_last < slot_millis {
                    continue;
                }

                // Round-3 review (Codex P2): decide whether this frame will
                // actually reach the air BEFORE touching any attempt
                // bookkeeping (`call_count`/`last_call_at`), not after. The
                // manual-call budget (`max_calls` above) and the on-air
                // timeline (`progress.messages`, below) both exist to bound
                // and record REAL transmission attempts — spending either on
                // a frame the TX-layer arm gate will deny anyway (a
                // `remote_origin` QSO left bound to a client that is no
                // longer the one armed) silently exhausts
                // `manual_call_max_calls` without ever having transmitted,
                // and records a `Sent` timeline entry for something that
                // never went out. THREE independent gates can swallow a
                // frame, and it has to clear ALL of them before an attempt
                // is real:
                //   - the global TX policy hard mute (round 2, finding 5);
                //   - for a `remote_origin` QSO only, the station-agent
                //     armed-TX gate the TX worker applies to every
                //     `TxOrigin::Remote` frame (round 7, finding 3). An arm
                //     expiry, explicit disarm, or a rebind to a different
                //     client leaves `TxPolicy` untouched, so the first check
                //     alone cannot see it;
                //   - the TX worker's own pre-PTT Hamlib hard mute (round 10,
                //     Fix E) — a Hamlib restart, the command loop not ready,
                //     an undelivered pending frequency/split command, or an
                //     in-flight Hamlib command. Neither of the first two
                //     checks can see this either.
                let frame_reaches_the_air = !tx_muted
                    && !hamlib_hard_muted
                    && (!progress.metadata.remote_origin
                        || remote_tx_permitted(progress.metadata.remote_client_key_id.as_deref()));
                if !frame_reaches_the_air {
                    continue;
                }

                let prev_last_call_at = progress.metadata.last_call_at;
                progress.metadata.call_count += 1;
                progress.metadata.last_call_at = Some(now);

                // PAN-72: this real per-slot re-send IS the silence signal —
                // the DX did not advance the QSO since the last slot, so we
                // count it against the stall streak. A forward advance
                // (`process_message_for_qso`) resets this to 0 elsewhere;
                // this is the sole increment site (see
                // `QsoMetadata::stall_cycles`'s doc comment). Reached only
                // when `frame_reaches_the_air` (above), so the existing
                // count-intact-on-mute behavior is preserved by construction.
                //
                // PAN-128: the QSO only transmits on its own parity — every
                // OTHER slot — so an emission counts once per own-parity TX
                // slot it feeds, not once per emission. The first emission
                // feeding a slot no surviving attempt has claimed holds the
                // slot's count (`bumped_stall`); its same-slot successors
                // keep pre-arming the TX worker without inflating the
                // streak, so `qso_stall_switch_after` measures real
                // unanswered TX opportunities. The slot an emission feeds is
                // derived with the TX scheduler's current-slot acceptance
                // rule (see `keep_call_tx_slot`), so an in-window rearm and
                // the off-parity rearm before it — both scheduled into the
                // same real TX slot — share one key (PR review follow-up).
                let tx_slot = keep_call_tx_slot(
                    now,
                    slot_millis,
                    self.tx_late_max_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                    progress.metadata.tx_parity,
                );
                let counts_toward_stall = !ledger.get(&qso_id).is_some_and(|entry| {
                    entry
                        .attempts
                        .iter()
                        .any(|a| a.bumped_stall && a.tx_slot == Some(tx_slot))
                });
                if counts_toward_stall {
                    progress.metadata.stall_cycles =
                        progress.metadata.stall_cycles.saturating_add(1);
                }

                let tx_auto = pancetta_core::TxFreqMode::from_u8(
                    self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
                )
                .allows_auto_change();

                let mut arm_stall_trigger = false;
                if tx_auto
                    && progress.metadata.stall_cycles >= self.config.timeouts.qso_stall_switch_after
                {
                    let current = progress.metadata.frequency;
                    let action = match progress.metadata.last_known_good_offset_hz {
                        Some(known_good) if (current - known_good).abs() >= f64::EPSILON => {
                            OffsetAction::Revert {
                                target_hz: known_good,
                            }
                        }
                        _ => OffsetAction::Switch { avoid_hz: current },
                    };
                    // PAN-72 (Codex round 7 on PR #350, finding 2): the streak
                    // is deliberately NOT cleared here. Raising an action is
                    // not relocating: the coordinator drains this mailbox once
                    // per slot and `apply_tx_offset_switch` can still refuse
                    // the result — most importantly with `OffsetActionNoOp`
                    // when exclusions left the allocator no valid candidate on
                    // a crowded band. Clearing at emission time threw the
                    // evidence away before anything could act on it, so PAN-79's
                    // commit-side refusal preserved a streak that had in fact
                    // already been reset, and recovery was pushed out by
                    // another full `qso_stall_switch_after` window even if a
                    // slot opened up immediately.
                    //
                    // The reset now happens in exactly one place — the
                    // successful commit in `apply_tx_offset_switch` (and, as
                    // before, a genuine DX advance in `process_message_for_qso`).
                    // Until one of those lands, the QSO stays at/above the
                    // threshold and re-raises the action once per slot; the
                    // coordinator's drain coalesces per QSO per batch (round 7,
                    // finding 1), so this costs one retry per slot, not a
                    // pile-up.
                    let entry = ledger.entry(qso_id).or_default();
                    entry.stall_action_revoked = false;
                    // PAN-182: the commit waits on the frame that brought the
                    // streak to the threshold (recorded below). A re-raise on
                    // a later slot leaves that hold alone — its own
                    // rollback cannot revoke the action, and re-arming the
                    // hold every slot could keep it from ever releasing — and
                    // only takes over an expired one.
                    arm_stall_trigger = (counts_toward_stall
                        && progress.metadata.stall_cycles
                            == self.config.timeouts.qso_stall_switch_after)
                        || entry.pending_stall_trigger.as_ref().is_some_and(|trigger| {
                            stall_trigger_hold_expired(trigger.emitted_at, now, slot_millis)
                        });
                    offset_actions_to_emit.push((
                        qso_id,
                        action,
                        // PAN-72 finding 8: the staleness token, captured
                        // here under the same write lock that raised the
                        // action so it can never disagree with it.
                        progress.metadata.advance_generation,
                    ));
                }

                // Record the re-emitted call as a Sent message so the TUI's
                // last-TX line and activity counter advance during keep-calling
                // (UX audit Batch 2 — the panel previously froze because rearm
                // appended nothing, making keep-calling look like a hang). The
                // raw_text is the rendered FT8 text we put on the air.
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
                let attempt = KeepCallAttempt {
                    attempt_id,
                    emitted_at: now,
                    raw_text,
                    prev_last_call_at,
                    bumped_stall: counts_toward_stall,
                    tx_slot: Some(tx_slot),
                    recorded_sent: true,
                    frequency_hz: progress.metadata.frequency,
                    remote_origin: progress.metadata.remote_origin,
                    remote_client_key_id: progress.metadata.remote_client_key_id.clone(),
                };
                if arm_stall_trigger {
                    ledger.entry(qso_id).or_default().pending_stall_trigger =
                        Some(PendingStallTrigger::from_attempt(&attempt));
                }
                self.record_keep_call_attempt(&mut ledger, qso_id, attempt);
                to_recall.push((
                    qso_id,
                    message,
                    progress.metadata.frequency,
                    progress.metadata.tx_parity,
                    progress.metadata.remote_origin,
                    progress.metadata.remote_client_key_id.clone(),
                    attempt_id,
                ));
            }
            ledger.retain(|id, _| qsos.get(id).is_some_and(|p| p.state.is_active()));
        }

        for (
            qso_id,
            message,
            frequency,
            tx_parity,
            snapshot_remote_origin,
            snapshot_remote_client_key_id,
            attempt_id,
        ) in to_recall
        {
            debug!(
                "Manual keep-calling: re-emitting {:?} on {:.1} Hz (qso={})",
                message, frequency, qso_id
            );
            // Round-16 review (Codex P1): re-read the LIVE binding
            // immediately before emitting, rather than trusting the
            // snapshot taken under the write lock above (released before
            // this loop runs) — a concurrent rebind (a remote client
            // taking over this QSO between the snapshot and this emission)
            // would otherwise let a stale `Local`/wrong-identity
            // `MessageToSend` reach `LatestTxIntent`, which the TX worker's
            // pivot mechanism can prefer over the correctly-bound request,
            // bypassing the arm check entirely rather than merely
            // misattributing it. Falls back to the snapshot only if the
            // QSO has since left the active map (shouldn't happen here —
            // `to_recall` was built from this same set moments ago — kept
            // defensive rather than unwrapping).
            let (remote_origin, remote_client_key_id) = match self.qsos.read().await.get(&qso_id) {
                Some(live) => (
                    live.metadata.remote_origin,
                    live.metadata.remote_client_key_id.clone(),
                ),
                None => (snapshot_remote_origin, snapshot_remote_client_key_id),
            };
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

        for (qso_id, action, raised_at_generation) in offset_actions_to_emit {
            self.emit_event(QsoEvent::TxOffsetActionNeeded {
                qso_id,
                action,
                raised_at_generation,
            })
            .await;
        }
    }

    /// One watchdog pass at a fresh slot-clock reading (PAN-296, PAN-358).
    ///
    /// When the clock reported an event (a wall-clock step of 250 ms or
    /// more) since the last applied one, every active QSO's timeout budget
    /// restarts at the latest event's `observed_at`, so the budget runs from
    /// the step whichever reader saw it first. Then manual calls rearm and
    /// timeouts apply at the reading's `now`, in that order.
    pub async fn watchdog_pass(&self) {
        let reading = self.slot_clock.read();
        let applied = self
            .watchdog_clock_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if reading.epoch != applied {
            if let Some(event) = reading.last_event {
                let at = event.observed_at;
                let qsos = self.qsos.write().await;
                let mut anchors = self
                    .watchdog_anchors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                anchors.clear();
                // A state or first call that began after the step (between
                // the event and this pass) runs from its own start, never
                // from an instant before it existed. Anything else restarts
                // at the event, including a pre-step start a backward step
                // left in the future, so no budget ever runs from an instant
                // later than this pass.
                let anchor_for = |start: UtcTimestamp| {
                    if start >= at && start <= reading.now {
                        start
                    } else {
                        at
                    }
                };
                for (&id, progress) in qsos.iter().filter(|(_, p)| p.state.is_active()) {
                    anchors.insert(
                        id,
                        WatchdogAnchors {
                            state: watchdog_state_identity(&progress.state, reading.now)
                                .map(|identity| (identity, anchor_for(identity.1))),
                            first_call: progress
                                .metadata
                                .first_call_at
                                .map(|original| (original, anchor_for(original))),
                        },
                    );
                }
                warn!(
                    skew_ms = event.skew.as_millis_i64(),
                    epoch = event.epoch,
                    "Wall-clock step: reset active QSO watchdog budgets"
                );
            }
            self.watchdog_clock_epoch
                .store(reading.epoch, std::sync::atomic::Ordering::Release);
        }
        // Preserve rearm-before-retirement ordering and independent call caps.
        self.rearm_manual_calls_at(reading.now).await;
        self.check_timeouts_at(reading.now).await;
    }

    /// Watchdog pass at an explicit time (for testability).
    ///
    /// In addition to the standard per-state timeouts, this enforces the
    /// **manual keep-calling watchdog**: a manual-initiated QSO that is
    /// still in `RespondingToCq` is retired (→ `Failed`/idle, callsign
    /// mapping cleared) once it has either transmitted
    /// `manual_call_max_calls` calls OR `manual_call_watchdog_minutes`
    /// have elapsed since the first call — whichever comes first.
    pub async fn check_timeouts_at(&self, now: UtcTimestamp) {
        // PAN-280: a completion whose closing frame never got a TX outcome
        // reverts first, so the exemption below applies to it at once.
        self.expire_pending_completions_at(now).await;
        // PAN-280: a QSO whose dropped 73 put it back in its pre-close state
        // gets one rework grace before any timeout applies again.
        let reworking: std::collections::HashSet<QsoId> = {
            let mut reverted = self
                .completion_reverted_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            reverted.retain(|_, at| now - *at < COMPLETED_QSO_REWORK_GRACE);
            reverted.keys().copied().collect()
        };
        let mut qsos = self.qsos.write().await;
        let mut timeouts = Vec::new();
        self.watchdog_anchors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id, _| qsos.get(id).is_some_and(|p| p.state.is_active()));

        for (&qso_id, progress) in qsos.iter_mut() {
            if reworking.contains(&qso_id) {
                continue;
            }
            let (state_age, first_call_age) = {
                let mut anchors = self
                    .watchdog_anchors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let mut state_age = progress.state.state_duration(now);
                let mut first_call_age = progress.metadata.first_call_at.map(|at| now - at);
                if let Some(anchor) = anchors.get_mut(&qso_id) {
                    if anchor.state.as_ref().is_some_and(|(identity, _)| {
                        Some(*identity) != watchdog_state_identity(&progress.state, now)
                    }) {
                        anchor.state = None;
                    }
                    if anchor.first_call.as_ref().is_some_and(|(original, _)| {
                        Some(*original) != progress.metadata.first_call_at
                    }) {
                        anchor.first_call = None;
                    }
                    if let Some((_, effective)) = anchor.state {
                        state_age = Some(now - effective);
                    }
                    if let Some((_, effective)) = anchor.first_call {
                        first_call_age = Some(now - effective);
                    }
                }
                (state_age, first_call_age)
            };
            let active_tx_state = matches!(
                progress.state,
                QsoState::CallingCq { .. }
                    | QsoState::RespondingToCq { .. }
                    | QsoState::SendingReport { .. }
                    | QsoState::WaitingForReport { .. }
                    | QsoState::WaitingForConfirmation { .. }
                    | QsoState::SendingConfirmation { .. }
            );

            // PAN-17: an outbound message that embeds a callsign the FT8
            // encoder can never represent (neither pack28's fixed 6-char
            // scheme nor the i3=4 nonstandard-callsign hash/58-bit path —
            // see `callsign_is_wire_representable`) would otherwise re-arm
            // and silently fail to transmit every slot for the full
            // watchdog window before self-retiring as a plain Timeout,
            // indistinguishable from "the DX never answered" (the live
            // incident this ticket fixes). Encoding is a pure function of
            // the callsign text, so there is nothing to gain by waiting or
            // retrying — retire on the first watchdog pass that observes
            // it, with a reason distinct from Timeout so the operator sees
            // "cannot transmit this message" rather than a bogus no-reply.
            //
            // Round 2 (Codex review #248, finding 4): checks BOTH sides of
            // the exchange, not just the DX's. Every directed frame also
            // embeds `config.our_callsign` — `StationConfig` permits an
            // arbitrarily long compound call there with no length cap, so a
            // misconfigured `our_callsign` (e.g. `VK9/W1XYZ/MM`, >11 chars)
            // would otherwise leave every QSO against a perfectly normal DX
            // stuck on the generic timeout — the exact PAN-17 symptom just
            // moved to the other callsign.
            //
            // Round 3 (Codex re-review, finding 1): ALSO fast-fails a
            // report-bearing rung (SendingReport / WaitingForReport — see
            // `report_stage_partner_callsign`; PAN-27 finding 3 removed
            // WaitingForConfirmation from that set) whose partner isn't
            // plausibly pack28-standard
            // (`callsign_is_plausibly_pack28_standard`), even though the
            // partner's callsign passes the plain wire-representability
            // check above (fits the i3=4 hash field fine). The round-1
            // encoder fix (`pancetta_ft8::encoder::try_encode_nonstandard`'s
            // doc) makes a numeric report/ack to such a partner fail
            // LOUDLY rather than silently blanking — correct — but nothing
            // stopped the QSO from re-arming that exact doomed encode every
            // slot once it reached this rung, which is the original PAN-17
            // "burns the watchdog window retrying an unencodable message"
            // symptom relocated to the report stage.
            //
            // Round 2 deliberately omitted this check because a naive
            // version (keyed on `metadata.their_callsign`, the "most
            // complete form ever seen") falsely retired healthy QSOs in
            // `adversarial_compound_calls.rs`'s compound-then-base
            // scenarios. That risk doesn't apply here: `report_stage_
            // partner_callsign` reads the STATE's own `their_callsign`
            // field, which is exactly the value `MessageExchange::
            // generate_response` embeds in the real outgoing
            // SignalReport/ReportAck (`to_station`/`from_station` come
            // straight from this same field — see exchange.rs) — so this
            // check fires if and only if the message the QSO is actually
            // about to (re-)send is genuinely unencodable, not on a
            // heuristic guess. `compound_first_then_base_completes` and
            // `cqer_caller_compound_then_base_completes` were updated to
            // assert the (now correct) fast MessageUnencodable retirement
            // — those scenarios need a real numeric report addressed to a
            // compound-form callsign, which round 1's encoder fix already
            // made impossible over real FT8; the tests just never
            // exercised the real encoder before, so they didn't notice.
            if active_tx_state {
                let our = self.config.our_callsign.as_str();
                let their = progress.metadata.their_callsign.as_deref();

                let unencodable_reason = if !callsign_is_wire_representable(our) {
                    Some(format!(
                        "our configured callsign '{}' cannot be represented in any FT8 message format",
                        our
                    ))
                } else if let Some(their) = their {
                    if !callsign_is_wire_representable(their) {
                        Some(format!(
                            "callsign '{}' cannot be represented in any FT8 message format",
                            their
                        ))
                    } else if let Some(report_partner) =
                        report_stage_partner_callsign(&progress.state)
                    {
                        if !callsign_is_plausibly_pack28_standard(report_partner)
                            || !callsign_is_plausibly_pack28_standard(our)
                        {
                            Some(format!(
                                "cannot send a numeric report/ack to '{}' — i3=4 (required \
                                 because a compound callsign is involved) has no field for one",
                                report_partner
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(reason) = unencodable_reason {
                    timeouts.push((qso_id, QsoFailureReason::MessageUnencodable(reason)));
                    continue;
                }
            }

            // Repetitive-TX watchdog (operator request): if a QSO has sat in the
            // same active TX state — i.e. we've been re-sending the SAME message
            // without the DX advancing us — longer than repetitive_tx_timeout_secs,
            // retire it. Applies to BOTH manual and auto QSOs and is checked first
            // (of the time-based watchdogs) so it bounds "stuck sending the same
            // thing" even while the manual keep-call watchdog (below) would
            // otherwise keep re-arming. A forward state advance resets the
            // state's `started_at`, so a healthy, progressing QSO never trips this.
            if active_tx_state {
                if let Some(dur) = state_age {
                    if dur.as_secs() > self.config.timeouts.repetitive_tx_timeout_secs as i64 {
                        timeouts.push((qso_id, QsoFailureReason::Timeout));
                        continue;
                    }
                }
            }

            // Manual keep-calling watchdog. Covers CallingCq (operator `c`:
            // re-calling CQ until someone answers), RespondingToCq
            // (re-calling the DX), SendingReport (FIX 4: re-sending our
            // R-report when the DX repeats their report), and WaitingForReport
            // (SM-F4: re-sending our report when a caller repeats their grid
            // because they never copied it). In all phases the operator is
            // actively keep-calling, and `call_count` / `first_call_at` span
            // the whole QSO, so the 10-calls / 5-min bound applies to the QSO
            // as a whole. Once the DX advances past these states (a caller
            // answers / ReportAck / RR73 received), the normal state timeouts
            // take over.
            if progress.metadata.initiated_by == CallInitiation::Manual
                && matches!(
                    progress.state,
                    QsoState::CallingCq { .. }
                        | QsoState::RespondingToCq { .. }
                        | QsoState::SendingReport { .. }
                        | QsoState::WaitingForReport { .. }
                )
            {
                let max_calls = self.config.timeouts.manual_call_max_calls;
                let watchdog = SignedDuration::from_mins(
                    self.config.timeouts.manual_call_watchdog_minutes as i64,
                );
                let elapsed = first_call_age.unwrap_or(SignedDuration::ZERO);

                // C3 race guard: if this QSO made a forward state advance in the
                // current watchdog cycle (the DX just answered), grant a
                // one-pass reprieve — do NOT retire it in the same tick it
                // advanced. The flag is consumed (cleared) here, so a QSO that
                // advanced once then goes silent is retired on the NEXT pass
                // once it re-hits the cap (per-QSO bound preserved; see C12).
                let progressed = progress.metadata.progressed_this_cycle;
                progress.metadata.progressed_this_cycle = false;

                // The call-count cap exists to stop pounding a DX that never
                // answered our INITIAL call (CallingCq / RespondingToCq). Once
                // the DX has ENGAGED — we received their report and advanced to
                // SendingReport — they can clearly hear us, so abandoning the
                // exchange at the call cap is wrong: it can drop the QSO in the
                // very window the DX's closing RR73 arrives. (Observed on-air
                // 2026-06-22, 9K2MP: the 10-call cap retired the QSO at 00:07:29,
                // one slot before its RR73 at 00:07:44 — so the RR73 had no
                // active QSO to complete and the operator had to close it by
                // hand.) In SendingReport, only the (longer) time watchdog and
                // the DX's own messages drive the QSO; the call cap does not
                // apply.
                //
                // SM-F4: WaitingForReport belongs with SendingReport here, NOT
                // with CallingCq/RespondingToCq — a CQer in WaitingForReport
                // has already been ANSWERED (a station replied to our CQ and
                // we've already sent them our report); this is the identical
                // "DX has engaged us" situation as SendingReport, just on the
                // CQer ladder rather than the Caller ladder. Applying the
                // call-count cap here would risk the same 9K2MP-style
                // premature drop for a CQer whose caller is slow to close, so
                // WaitingForReport is likewise exempt from the cap and
                // governed only by the (longer) time watchdog.
                let in_initial_call = matches!(
                    progress.state,
                    QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. }
                );
                // PAN-75: the frame that reaches the cap must get its TX
                // outcome before the cap retires the QSO, or an unsent final
                // frame could never be given back. Hold retirement while any
                // keep-call attempt is still inside the rollback window; a
                // rollback drops `call_count` below the cap and re-arming
                // resumes. Re-arming stops at the cap, so nothing new airs
                // while we wait, and the time watchdog still applies.
                let attempt_unresolved = {
                    let cutoff = now
                        - SignedDuration::from_millis(
                            UNSENT_ATTEMPT_RETENTION_SLOTS * self.rearm_slot_millis(),
                        );
                    self.keep_call_ledger
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&qso_id)
                        .is_some_and(|entry| entry.attempts.iter().any(|a| a.emitted_at >= cutoff))
                };
                let call_cap_hit = in_initial_call
                    && progress.metadata.call_count >= max_calls
                    && !attempt_unresolved;
                if !progressed && (call_cap_hit || elapsed >= watchdog) {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
                // Manual calls do not use the (much shorter) per-state
                // timeout while keep-calling; the watchdog above governs.
                continue;
            }

            if let Some(duration) = state_age {
                let timeout_seconds = match &progress.state {
                    QsoState::CallingCq { .. } => self.config.timeouts.cq_timeout,
                    QsoState::WaitingForReport { .. } => self.config.timeouts.report_timeout,
                    QsoState::WaitingForConfirmation { .. } => {
                        self.config.timeouts.confirmation_timeout
                    }
                    // Phase 5: an AUTO pounce / CQ-answer that the DX never replies
                    // to must retire so it does not pin `max_concurrent_qsos`
                    // forever. Manual QSOs in these states are governed by the
                    // keep-call watchdog above (which already `continue`d), so these
                    // arms apply only to Auto QSOs. `report_timeout` is the natural
                    // bound — it is how long we wait for the DX's next frame before
                    // giving up on a one-shot autonomous call.
                    QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. } => {
                        self.config.timeouts.report_timeout
                    }
                    _ => continue,
                };

                // Compare as signed seconds: a NEGATIVE elapsed (the state's
                // `started_at` is later than `now`) must never count as a
                // timeout. In production `started_at` and `now` are both the real
                // clock so elapsed is always ≥ 0, but the sim harness anchors its
                // virtual `now` to a slot boundary that can sit slightly behind
                // the real-clock `started_at` the engine stamps — casting a
                // negative `num_seconds()` to `u64` would wrap to a huge value
                // and spuriously retire a just-opened QSO.
                if duration.as_secs() > timeout_seconds as i64 {
                    timeouts.push((qso_id, QsoFailureReason::Timeout));
                }
            }
        }

        for (qso_id, reason) in timeouts {
            if let Some(mut progress) = qsos.remove(&qso_id) {
                self.watchdog_anchors
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&qso_id);
                let old_state = progress.state.clone();
                progress.state = QsoState::Failed {
                    reason: reason.clone(),
                    failed_at: now,
                    last_state: Box::new(old_state.clone()),
                };
                // Capture metadata before it's consumed below (Batch 4,
                // SM-F5 — needed for QsoFailed).
                let metadata = progress.metadata.clone();
                // Layer 2 timeline persistence: this is the most common
                // "QSO leaves the active map" discard site (every
                // watchdog/timeout retirement runs through here) — capture
                // the timeline before `progress` is dropped at loop end.
                let state_history = progress.state_history.clone();
                let messages = progress.messages.clone();

                drop(qsos); // Release lock before emitting events
                self.emit_state_change(qso_id, old_state.clone(), progress.state.clone())
                    .await;
                // SM-F5: this is the most common failure producer (every
                // watchdog/timeout retirement runs through here) and
                // previously never emitted QsoFailed, so the coordinator's
                // priority-scoring failure backoff (`record_failure`) never
                // fired for a timed-out QSO. `reason` here is
                // QsoFailureReason::Timeout for every timing-based push site
                // above, or QsoFailureReason::MessageUnencodable for the
                // PAN-17 unencodable-callsign check.
                self.emit_event(QsoEvent::QsoFailed {
                    qso_id,
                    reason,
                    last_state: old_state,
                    metadata,
                    state_history,
                    messages,
                })
                .await;

                if let Some(callsign) = &progress.metadata.their_callsign {
                    self.remove_callsign_mapping(callsign, qso_id).await;
                }

                warn!("QSO timeout: {}", qso_id);
                qsos = self.qsos.write().await; // Re-acquire lock
            }
        }
    }
}
