//! Decode routing: which active QSO a decoded frame belongs to.
//!
//! `process_message_with_parity` in `qso_manager.rs` calls
//! `find_qsos_for_message`. That method runs `classify_relevance` over the
//! active QSOs, and `classify_relevance` applies `is_message_relevant`.
//! `is_us` and `is_partner` are the callsign checks that routing and the
//! state machine in `transition.rs` share. `is_hound_fox_frame` decides
//! whether a frame from a Fox may move a Hound QSO to a new Fox stream.
//!
//! PAN-375 moved these methods here from `qso_manager.rs` unchanged. A method
//! that another file calls is `pub(super)`, the scope it had as a private
//! method of `qso_manager.rs`.

use super::*;

impl QsoManager {
    /// Forward position of a state on the responder's FT8 QSO ladder:
    /// RespondingToCq → SendingReport → WaitingForConfirmation → Completed.
    /// Higher means later in the conversation. Used only to detect a manual
    /// state *regression* (a transition whose rank decreased). States off this
    /// ladder (CallingCq, Idle, Failed, Contest, …) return `None` so they never
    /// register as a regression.
    /// Does `call` refer to *our* station, allowing a compound form of our own
    /// callsign (e.g. we operate as `K5ARH/P`)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`] against `our_callsign`. Used in the
    /// `to == us` / `calling_station == us` halves of sender verification so a
    /// message directed at our compound call is not rejected as "not for us".
    pub(super) fn is_us(&self, call: &str) -> bool {
        crate::exchange::callsigns_match(call, &self.config.our_callsign)
    }

    /// Is `from` the same station as our latched QSO partner `partner`, allowing
    /// a compound↔base change mid-QSO (catalog C18 / peer D4)? Thin wrapper over
    /// [`crate::exchange::callsigns_match`]. Used in the `from == DX` half of
    /// sender verification so an established QSO does not stall when the DX's
    /// displayed call gains or loses a portable prefix/suffix between frames.
    /// Deliberately conservative: genuinely different calls (`K5ARH`/`K5ARG`)
    /// still mismatch — see `callsigns_match` docs.
    /// PAN-259: whether `message_type` is a report or RR73 addressed to us by
    /// this Hound QSO's latched Fox. Only such a frame may move a Hound to a
    /// new Fox stream.
    pub(super) fn is_hound_fox_frame(
        &self,
        metadata: &QsoMetadata,
        message_type: &MessageType,
    ) -> bool {
        let (from, to) = match message_type {
            MessageType::SignalReport {
                from_station,
                to_station,
                ..
            }
            | MessageType::FinalConfirmation {
                from_station,
                to_station,
            } => (from_station, to_station),
            _ => return false,
        };
        metadata
            .their_callsign
            .as_deref()
            .is_some_and(|fox| Self::is_partner(from, fox))
            && self.is_us(to)
    }

    pub(super) fn is_partner(from: &str, partner: &str) -> bool {
        crate::exchange::callsigns_match(from, partner)
    }

    pub(super) async fn find_qsos_for_message(
        &self,
        message_type: &MessageType,
        frequency: f64,
    ) -> Vec<QsoId> {
        let mut matching_qsos = Vec::new();
        let mut rejections = Vec::new();
        {
            let qsos = self.qsos.read().await;

            // PAN-14 / SM-F7 (docs/qso-tx-deep-review-2026-07-18.md §A.3):
            // a station that already has an ESTABLISHED active QSO (i.e. its
            // `their_callsign` is latched — every state past CallingCq/Idle)
            // must not ALSO be accepted as a fresh answerer by an UNRELATED
            // CallingCq QSO's "any station" relevance arms. Without this, the
            // same decoded frame (e.g. that station's report of us) can
            // independently satisfy both QSOs' relevance arms —
            // `find_qsos_for_message` then advances BOTH, producing two
            // simultaneously-active QSO objects partnered with the same
            // real-world station. Each generates its own TX cadence
            // (possibly at a different audio offset), which is the "two
            // very-high-amplitude signals for one station in one TX window"
            // failure mode — see AGENTS.md "at most one QSO object exists
            // per (callsign, band)". Computed once per incoming message
            // (not per-QSO) since it doesn't depend on the candidate QSO.
            //
            // P2 (Codex round-1 review of PR #250): also treat a QSO that
            // COMPLETED with this exact sender within
            // `COMPLETED_QSO_REWORK_GRACE` as "already accounted for" —
            // mirrors `has_active_or_recent_qso_with`'s active-or-recent
            // pattern (and the coordinator's matching 45 s completed-TX
            // grace). Without this, the `is_active()`-only predicate goes
            // false the instant a QSO completes, so a station whose contact
            // with us just finished could be immediately re-claimed by an
            // unrelated CallingCq QSO's "any station" arm on a stray/
            // duplicate frame, opening a second QSO object for a station we
            // just worked seconds ago.
            //
            // PAN-25 (Codex round-2 review of PR #250): the completed arm must
            // also match band, not just callsign — the uniqueness invariant
            // (AGENTS.md) is per (callsign, band). Without this, a station
            // worked on band A stays "accounted for" against a fresh CQ reply
            // from the same call on band B for the whole grace window, so a
            // legitimate new QSO on the new band is silently dropped. The
            // active arm above is intentionally left unscoped by band — an
            // active QSO can only exist on the band currently being decoded.
            //
            // PAN-25 round 1 (Codex): `frequency` here is an audio offset (a
            // few hundred/thousand Hz), and so -- pre-fix -- was the stored
            // `p.metadata.frequency` for a Completed QSO; comparing two audio
            // offsets via `frequency_to_band` always yields the same "0MHZ"
            // bucket regardless of the real RF band, silently defeating this
            // check. A Completed QSO's `metadata.frequency` is now stamped
            // with the true RF frequency (dial + offset) at completion time
            // (see `process_message_for_qso`), so `frequency` must be
            // adjusted the same way here for a fair comparison.
            let now = self.now_utc();
            let want_band = crate::utils::frequency_to_band(self.current_rf_frequency(frequency));
            let sender_has_other_active_or_recent_partner =
                message_type.sender_callsign().is_some_and(|sender| {
                    qsos.values().any(|p| {
                        let same_sender = p
                            .metadata
                            .their_callsign
                            .as_deref()
                            .is_some_and(|c| crate::exchange::callsigns_match(c, sender));
                        if !same_sender {
                            return false;
                        }
                        if p.state.is_active() {
                            return true;
                        }
                        if let QsoState::Completed { completed_at, .. } = &p.state {
                            let completed_band = crate::utils::frequency_to_band(
                                p.metadata
                                    .completed_rf_frequency_hz
                                    .unwrap_or(p.metadata.frequency),
                            );
                            return completed_band == want_band
                                && now.signed_duration_since(*completed_at).num_seconds()
                                    <= COMPLETED_QSO_REWORK_GRACE.num_seconds();
                        }
                        false
                    })
                });

            for (&qso_id, progress) in qsos.iter() {
                if !progress.state.is_active() {
                    continue;
                }

                let verdict = self.classify_relevance(
                    &progress.state,
                    &progress.metadata,
                    message_type,
                    frequency,
                    sender_has_other_active_or_recent_partner,
                );
                if verdict.relevant {
                    matching_qsos.push(qso_id);
                } else if let Some(reason) = verdict.reason {
                    warn!(
                        target: "qso.security",
                        %qso_id,
                        reason = reason.as_str(),
                        got_from = ?message_type.sender_callsign(),
                        "frame entangled with an active QSO failed sender verification — not routed"
                    );
                    rejections.push(QsoEvent::MessageRejected {
                        qso_id,
                        reason,
                        from_callsign: message_type.sender_callsign().map(str::to_string),
                        to_callsign: message_type.addressee_callsign().map(str::to_string),
                    });
                }
            }

            // P1 (Codex round-1 review of PR #250): a reply can independently
            // satisfy MULTIPLE unpartnered `CallingCq` QSOs' relevance arms —
            // e.g. via repeated `StartCq`/`start_cq_manual` calls, or Fox
            // mode engaging while a CQ is already live, both of which insert
            // another `CallingCq` QSO with no guard against one already
            // existing. Every `CallingCq` QSO has `metadata.their_callsign ==
            // None`, so the guard above (keyed on an ESTABLISHED partner)
            // cannot see a conflict between two still-unpartnered `CallingCq`
            // QSOs — both independently pass their "any station" arm and
            // both would advance, again leaving one real station partnered
            // with two `qso_id`s. Restrict to the single earliest-created
            // (`metadata.start_time`) `CallingCq` match — first CQ up, first
            // CQ answered — dropping any others so only ONE advances.
            let calling_cq_matches: Vec<QsoId> = matching_qsos
                .iter()
                .copied()
                .filter(|id| {
                    matches!(
                        qsos.get(id).map(|p| &p.state),
                        Some(QsoState::CallingCq { .. })
                    )
                })
                .collect();
            if calling_cq_matches.len() > 1 {
                let earliest = calling_cq_matches
                    .iter()
                    .copied()
                    .min_by_key(|id| qsos.get(id).map(|p| p.metadata.start_time))
                    .expect("calling_cq_matches is non-empty");
                matching_qsos.retain(|id| *id == earliest || !calling_cq_matches.contains(id));
            }
        }

        // A frame that matched any active QSO is legitimate traffic for this
        // manager.  Do not report it as an impostor against every other
        // concurrent QSO whose state happens to accept the same message kind.
        if matching_qsos.is_empty() {
            for event in rejections {
                self.emit_event(event).await;
            }
        }

        matching_qsos
    }

    pub(super) fn classify_relevance(
        &self,
        state: &QsoState,
        metadata: &QsoMetadata,
        message_type: &MessageType,
        frequency: f64,
        sender_has_other_active_or_recent_partner: bool,
    ) -> Relevance {
        let relevant = self.is_message_relevant(
            state,
            metadata,
            message_type,
            frequency,
            sender_has_other_active_or_recent_partner,
        );
        if relevant {
            return Relevance {
                relevant: true,
                reason: None,
            };
        }

        // Routing-stage diagnostics are only meaningful when the decode is
        // close enough to be entangled with this QSO.  Otherwise ordinary
        // traffic elsewhere in the passband would be classified against every
        // active QSO.
        // Mirrors `is_message_relevant`'s gate exactly, including the
        // recently-vacated-offset baseline (PAN-72, round 4, finding 3) — the
        // two must agree on what counts as "entangled with this QSO".
        let within_frequency_gate = state.frequency().is_none_or(|qso_frequency| {
            let match_frequency = metadata.partner_freq.unwrap_or(qso_frequency);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            let matches_baseline = |baseline: f64| (baseline - frequency).abs() <= tolerance;
            // PAN-386: same Fox-slot baseline as `is_message_relevant`.
            let fox_slot_baseline = metadata.fox
                && state.their_callsign().is_some()
                && matches_baseline(metadata.frequency);
            matches_baseline(match_frequency)
                || fox_slot_baseline
                || live_pre_switch_offset_at(
                    metadata,
                    self.now_utc(),
                    self.pre_switch_offset_grace(),
                )
                .is_some_and(|pre| matches_baseline(pre.offset_hz))
        });
        if !within_frequency_gate {
            return Relevance {
                relevant: false,
                reason: None,
            };
        }

        let verify = |from: &str, partner: &str, to: &str| {
            RejectionReason::classify(Self::is_partner(from, partner), self.is_us(to))
                // A partner working a third party is routine band traffic, not
                // a security event.  At routing time only an impostor sending
                // to us is distinguishable from ordinary traffic.
                .filter(|reason| *reason == RejectionReason::SenderNotPartner)
        };
        let reason = match (state, message_type) {
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                }
                | MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport {
                    from_station,
                    to_station,
                    ..
                },
            ) => verify(from_station, target_callsign, to_station),
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => verify(responding_station, target_callsign, calling_station),
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    from_station,
                    to_station,
                    ..
                }
                | MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation {
                    from_station,
                    to_station,
                }
                | MessageType::SeventyThree {
                    from_station,
                    to_station,
                },
            ) => verify(from_station, their_callsign, to_station),
            _ => None,
        };
        Relevance {
            relevant: false,
            reason,
        }
    }

    pub(super) fn is_message_relevant(
        &self,
        state: &QsoState,
        metadata: &QsoMetadata,
        message_type: &MessageType,
        frequency: f64,
        sender_has_other_active_or_recent_partner: bool,
    ) -> bool {
        // Tolerances are module-level shared consts (`FREQ_TOLERANCE_HZ`,
        // `ESTABLISHED_FREQ_TOLERANCE_HZ`) — see their doc comments for the
        // security-review rationale (C-1) and the B15 established-QSO
        // widening rationale. `classify_relevance` and
        // `maybe_confirm_frequency_drift_at` must match these values; PAN-15
        // item 6 hoisted all three sites onto one definition.

        let matched = match (state, message_type) {
            // We're calling CQ. The responder's callsign is whoever is in the
            // `responding_station` field; the message must be addressed to us.
            // PAN-14/SM-F7: a CallingCq QSO has no established partner yet,
            // so it would otherwise accept a frame from ANY station — even
            // one that already has a different, established active QSO with
            // us. Excluding that case here stops the same real station from
            // becoming the partner of two simultaneously-active QSO objects.
            (
                QsoState::CallingCq { .. },
                MessageType::CqResponse {
                    calling_station, ..
                },
            ) => self.is_us(calling_station) && !sender_has_other_active_or_recent_partner,

            // A4 (routing half): a caller answered our CQ with a bare signal
            // report (grid skipped) — "<us> <them> -NN". Route it to this
            // CallingCq QSO so the transition arm can step CQ → report. Only
            // addressed-to-us reports qualify (any from_station, since we don't
            // yet know who will answer) — UNLESS that from_station already has
            // a different established active QSO (PAN-14/SM-F7, see above).
            (QsoState::CallingCq { .. }, MessageType::SignalReport { to_station, .. }) => {
                self.is_us(to_station) && !sender_has_other_active_or_recent_partner
            }

            // CQer flow: we called CQ, the caller answered, and we sent our
            // report (now WaitingForReport). The caller's R-report (ReportAck)
            // is the next message — route it to this QSO so it can close.
            // Verify both directions: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // A5 (routing half): the caller closed early with RR73 / 73 from
            // WaitingForReport (before sending their R-report). Route the close
            // to this QSO so the transition arm can complete it. Both directions
            // verified: from THEM, to US.
            (
                QsoState::WaitingForReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // We responded to a CQ from `target_callsign` and are waiting for
            // their report. Verify both directions: from THEM, to US.
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::SignalReport {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, target_callsign) && self.is_us(to_station),

            // STUCK-AT-GRID FIX (routing half): the DX answered our grid by
            // returning our call (bare "<us> <DX>" or "<us> <DX> <grid>") — a
            // CqResponse directed at us. Route it to this QSO so the transition
            // arm can step grid -> report. Verify both directions: from THEM
            // (responding_station), to US (calling_station).
            (
                QsoState::RespondingToCq {
                    target_callsign, ..
                },
                MessageType::CqResponse {
                    calling_station,
                    responding_station,
                    ..
                },
            ) => {
                Self::is_partner(responding_station, target_callsign) && self.is_us(calling_station)
            }

            // We sent the report and are waiting for the report-ack. Same check.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::ReportAck {
                    to_station,
                    from_station,
                    ..
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // FIX 2: the DX may close directly from our R-report with RR73
            // (or a plain 73) instead of acking first — accept it here so it
            // routes to this QSO. Both directions verified.
            (
                QsoState::SendingReport { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            // Awaiting RR73 — verify both directions. Accept a plain 73 too
            // (DX skipped RR73).
            (
                QsoState::WaitingForConfirmation { their_callsign, .. },
                MessageType::FinalConfirmation {
                    to_station,
                    from_station,
                }
                | MessageType::SeventyThree {
                    to_station,
                    from_station,
                },
            ) => Self::is_partner(from_station, their_callsign) && self.is_us(to_station),

            _ => {
                // Anything else: only relevant if addressed to us.
                message_type.is_addressed_to(&self.config.our_callsign)
            }
        };

        if !matched {
            return false;
        }

        // Apply the frequency gate AFTER the callsign/to/state match (B15). A
        // matched message from an ESTABLISHED QSO's partner is allowed the
        // wider drift bound; everything else uses the tight default. An
        // established QSO is one where we already know the contra callsign
        // (i.e. not CallingCq/Idle) — `their_callsign()` is Some.
        //
        // Split-TX: when `metadata.partner_freq` is `Some`, the DX transmits on
        // a different frequency than we do. This includes Hound/Fox as well as
        // ordinary manual offset holds, collision nudges, and passband clamps.
        // Match incoming frames against where we hear the DX (`partner_freq`),
        // not our TX offset. When it is `None`, `unwrap_or(qso_freq)` preserves
        // the ordinary Tx=Rx path byte-for-byte.
        //
        // PAN-72 (Codex round 4 on PR #350, finding 3): an offset we moved away
        // from moments ago is a SECOND valid baseline for the length of
        // `PRE_SWITCH_OFFSET_GRACE`. The frame that triggered the relocation was
        // transmitted there, so its answer necessarily arrives after the move —
        // and for an unanswered `CallingCq`, where `partner_freq` is
        // deliberately `None` and the gate is only 15 Hz wide, judging that
        // answer against the new offset rejects it outright and
        // `maybe_answer_caller` spawns a duplicate QSO in its place. The window
        // is bounded in time, uses the SAME tolerance, and is on top of (never
        // instead of) the current baseline; the callsign/state match above still
        // gates who may use it.
        // PAN-259: a Fox runs several streams and may answer us on any of
        // them, more than the established tolerance away from where we last
        // heard it. For an active Hound, a report or RR73 that is addressed
        // to us AND sent by the latched Fox (checked here, because some
        // state/message pairs above fall through to an addressee-only arm)
        // is relevant anywhere inside the Fox response envelope.
        if metadata.hound
            && self.is_hound_fox_frame(metadata, message_type)
            && hound_report_offset(frequency, &self.config.hound).is_some()
        {
            return true;
        }
        if let Some(qso_freq) = state.frequency() {
            let match_freq = metadata.partner_freq.unwrap_or(qso_freq);
            let tolerance = if state.their_callsign().is_some() {
                ESTABLISHED_FREQ_TOLERANCE_HZ
            } else {
                FREQ_TOLERANCE_HZ
            };
            let matches_baseline = |baseline: f64| (baseline - frequency).abs() <= tolerance;
            // PAN-386: a WSJT-X Hound answers on the audio frequency where it
            // decoded the Fox's report, which is the Fox-role QSO's own TX
            // slot, not the Hound's calling frequency in `partner_freq`. Once
            // the partner call is known, that slot is one more baseline; the
            // callsign and state match above still decide who may use it.
            let fox_slot_baseline = metadata.fox
                && state.their_callsign().is_some()
                && matches_baseline(metadata.frequency);
            if !matches_baseline(match_freq)
                && !fox_slot_baseline
                && !live_pre_switch_offset_at(
                    metadata,
                    self.now_utc(),
                    self.pre_switch_offset_grace(),
                )
                .is_some_and(|pre| matches_baseline(pre.offset_hz))
            {
                return false;
            }
        }

        true
    }
}
