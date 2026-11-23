use super::*;

/// Outcome of checking whether a newly-arrived request should supersede
/// (abort + re-key) an in-flight transmission. See
/// docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingDuringTx {
    /// Not a genuine new request — either an exact-content repeat of what's
    /// already in flight, or a stale pivot-tombstone duplicate. Discard.
    Drop,
    /// PAN-73 round 2 (Codex): a request for a QSO that is NOT any of the
    /// in-flight items. Neither "kill the good in-flight TX for this" (the
    /// original bug) nor "discard it forever" (round-1's own regression —
    /// see the doc comment below) is safe, since today's data model cannot
    /// tell an operator's deliberate action for a not-currently-keyed QSO
    /// apart from another QSO's own routine autonomous message — both are a
    /// `TransmitRequest`/`MultiTransmitRequest` with `qso_id: Some(..)` and
    /// no other provenance. Siphon it (same mechanism as a non-candidate
    /// message, e.g. a `TuneRequest`) so it's put back at the front of the
    /// worker's inbox in arrival order once this wait ends and picked up on
    /// the worker's very next dequeue — not lost, just not instant.
    Requeue,
    /// A genuinely different request. Abort the in-flight transmission and
    /// attempt to re-key with this content.
    Supersede {
        text: String,
        frequency_offset: f64,
        qso_id: Option<String>,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    },
}

/// Manual-trigger / same-QSO classifier: a request arriving while another is
/// in flight supersedes it only when it is a genuine manual/free-text
/// request (`qso_id == None` — operator CQ, tune, test-TX) or a fresher
/// message for one of the QSOs already being transmitted (single in-flight
/// item for the single-TX arm, or any item of the in-flight bundle for the
/// multi-TX arm). It is dropped (discarded) when it is an exact duplicate
/// (identical text for that QSO) or a recognized pivot-tombstone
/// (`is_pivot_duplicate`), and REQUEUED (not discarded, not instant — see
/// `IncomingDuringTx::Requeue`) when it targets a QSO that isn't any of the
/// in-flight items.
///
/// PAN-73: with `max_concurrent_qsos > 1`, every active QSO's autonomous
/// auto-sequence loop independently enqueues its next `TransmitRequest`
/// whenever its own turn comes up — with no operator involved at all. Before
/// this fix, any such message arriving in the few-microsecond window right
/// after a DIFFERENT QSO keyed PTT for the current slot was classified as a
/// supersede candidate purely because *a* new request existed, tearing down
/// a perfectly good in-flight transmission for content that frequently turns
/// out to be unschedulable this slot anyway (see
/// `supersede_and_rekey_or_bundle`'s unconditional PTT-off) — killing BOTH
/// QSOs' turns and leaving the slot silent until some third QSO's unrelated
/// retry timer opportunistically keyed into the leftover dead air. Confirmed
/// live 2026-09-05 (VP2MAA/JF1RDH/JA5GYU).
///
/// Round-1 review (Codex, PR #346) found this classifier's first version
/// wrong in three further ways, all fixed here: (1) it treated ANY
/// `Some(qso_id)` candidate not matching the in-flight QSO as autonomous and
/// dropped it, but `coordinator/qso.rs`'s `MessageToSend` → `TransmitRequest`
/// conversion is identical for operator-triggered actions (start a tracked
/// call/CQ, answer a caller, request a resend) and routine auto-sequence
/// ticks — both carry `Some(qso_id)` with no other marker, so there is no
/// data-model way to tell them apart today; real disambiguation is Phase 2
/// (priority-tier gating / explicit provenance), not built by this plan —
/// `Requeue` is the honest interim answer: never silently discard (which
/// would apply equally to a real operator override), but never let an
/// unconfirmed candidate kill a good in-flight transmission either; (2) the
/// multi-TX arm's two call sites compared only against `items.first()`
/// instead of every in-flight bundle item, so a fresher message for the
/// bundle's 2nd+ QSO was wrongly treated as unrelated; (3) the
/// `MultiTransmitRequest` arm always superseded regardless of whether the
/// incoming bundle actually overlapped the in-flight QSO(s) at all.
pub fn classify_incoming_during_tx(
    candidate: &MessageType,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    pivoted_once: &std::collections::HashMap<String, (String, f64)>,
    // Codex P1, PR #362 round 21: the in-flight side's own authorization —
    // mirrors `tx_pivot_target`'s `current_authorization` (round 5) and
    // `supersede_and_rekey_or_bundle`'s `in_flight_origin`/
    // `in_flight_remote_client_key_id`. Without this, a same-QSO candidate
    // whose rendered text/frequency happen to be byte-identical to the
    // in-flight item (e.g. a local operator takes over an A-bound remote QSO
    // and repeats its current text) was classified as an exact duplicate and
    // dropped — even though it carries a DIFFERENT authorization that has
    // never been checked against this slot's arm. A disarm of the OLD
    // (in-flight) client then aborts the transmission while the dropped
    // candidate — the one actually entitled to transmit — is gone for good.
    in_flight_origin: crate::message_bus::TxOrigin,
    in_flight_remote_client_key_id: Option<&str>,
) -> IncomingDuringTx {
    // M1: normalize through `active_tx_qso_key` before comparing, matching
    // every sibling qso_id comparison (`tx_pivot_target`, `is_pivot_duplicate`,
    // `tx_qso_is_live`).
    let find_in_flight = |qso_id: Option<&str>| {
        qso_id.map(super::super::active_tx_qso_key).and_then(|key| {
            in_flight_items.iter().find(|it| {
                it.qso_id.as_deref().map(super::super::active_tx_qso_key) == Some(key.clone())
            })
        })
    };

    // Codex P1, round 21: a candidate whose rendered content matches the
    // in-flight item byte-for-byte but carries DIFFERENT authorization is
    // not a duplicate — it's a fresh, differently-authorized request that
    // just happens to render the same text (e.g. a local takeover repeating
    // an in-flight remote QSO's current closing frame). Only an
    // authorization match (same origin, same client) plus matching content
    // is a genuine no-op.
    let authorization_differs = |cand_origin: crate::message_bus::TxOrigin,
                                 cand_client: Option<&str>| {
        cand_origin != in_flight_origin || cand_client != in_flight_remote_client_key_id
    };
    // PAN-136 (Codex P2 on PR #456): since PAN-136 every in-flight item
    // carries its own binding, and a bundle's scalars are only the fold of
    // its items (`fold_bundle_identity` keeps the first remote channel).
    // Two bundles can therefore fold to the same binding while one matching
    // item's binding changed — e.g. a local takeover of one of two Remote-A
    // QSOs. Comparing only the folded scalars treats that refresh as an
    // unchanged repeat; if A is then disarmed the old bundle aborts and the
    // entitled refresh is gone. Every content match below must also match
    // the in-flight item's own binding.
    let item_binding_differs = |in_flight_item: &crate::message_bus::TransmitRequestItem,
                                cand_origin: crate::message_bus::TxOrigin,
                                cand_client: Option<&str>| {
        in_flight_item.origin != cand_origin
            || in_flight_item.remote_client_key_id.as_deref() != cand_client
    };

    match candidate {
        MessageType::TransmitRequest {
            attempt_id: _,
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
            remote_client_key_id,
        } => {
            // Codex P1, round 22: the tombstone match alone isn't enough —
            // it's keyed only on qso_id/text/frequency, with no authorization
            // in it. If control has since transferred (this candidate's
            // authorization differs from the in-flight frame's), it's not
            // the stale second copy of the pivot; it's a fresh, differently-
            // authorized request that happens to render the pivoted text.
            // Fall through to the normal per-QSO comparison below rather
            // than trusting the tombstone.
            // PAN-136: also the matching in-flight item's own binding (the
            // pivot swapped it in with the payload).
            if super::super::is_pivot_duplicate(
                qso_id.as_deref(),
                message_text,
                *frequency_offset,
                pivoted_once,
            ) && !authorization_differs(*origin, remote_client_key_id.as_deref())
                && !find_in_flight(qso_id.as_deref()).is_some_and(|item| {
                    item_binding_differs(item, *origin, remote_client_key_id.as_deref())
                })
            {
                return IncomingDuringTx::Drop;
            }
            // Genuine manual/free-text/tune/test-TX (no qso_id at all) is the
            // one unambiguous operator-triggered case — supersedes UNLESS
            // it's an exact duplicate (text AND frequency) of an in-flight
            // untracked item (round 2, Codex): an unchanged re-send of the
            // same manual frame is a no-op, not a fresh override, and
            // treating it as one needlessly deasserts PTT and restarts (or,
            // past `tx_late_max_ms`, abandons) the very frame already on the
            // air. Frequency is part of the identity check (round 3, Codex):
            // comparing text alone would also match a deliberate same-text
            // frequency hop.
            if qso_id.is_none() {
                let is_untracked_duplicate =
                    in_flight_items.iter().any(|it| {
                        it.qso_id.is_none()
                            && it.message_text == *message_text
                            && it.frequency_offset == *frequency_offset
                            && !item_binding_differs(it, *origin, remote_client_key_id.as_deref())
                    }) && !authorization_differs(*origin, remote_client_key_id.as_deref());
                if is_untracked_duplicate {
                    return IncomingDuringTx::Drop;
                }
                return IncomingDuringTx::Supersede {
                    text: message_text.clone(),
                    frequency_offset: *frequency_offset,
                    qso_id: qso_id.clone(),
                    tx_parity: *tx_parity,
                };
            }
            match find_in_flight(qso_id.as_deref()) {
                // PAN-73 round 3 (Codex): text alone isn't a safe duplicate
                // check — a stuck-DX QSO can re-render the SAME text at a
                // deliberately hopped frequency to dodge a collision
                // (qso_manager.rs's stuck-DX offset hop). Matching on text
                // alone would drop that hop and leave the old-offset signal
                // transmitting into the collision it was trying to avoid.
                // Round 21: authorization must ALSO match, or this is a
                // fresh (differently-authorized) request wearing the
                // in-flight item's own content.
                Some(item)
                    if item.message_text == *message_text
                        && item.frequency_offset == *frequency_offset
                        && !authorization_differs(*origin, remote_client_key_id.as_deref())
                        && !item_binding_differs(
                            item,
                            *origin,
                            remote_client_key_id.as_deref(),
                        ) =>
                {
                    IncomingDuringTx::Drop
                }
                Some(_) => IncomingDuringTx::Supersede {
                    text: message_text.clone(),
                    frequency_offset: *frequency_offset,
                    qso_id: qso_id.clone(),
                    tx_parity: *tx_parity,
                },
                // Some(qso_id) that isn't any in-flight item: another QSO's
                // request (autonomous or operator — indistinguishable today).
                // See the doc comment above.
                None => IncomingDuringTx::Requeue,
            }
        }
        // A bundle only supersedes when it actually carries FRESHER content
        // for a QSO already in flight — a matching qso_id alone isn't
        // enough (PAN-73 round 6, Codex): `autonomous.rs` can emit a bundle
        // holding an UNCHANGED item for the currently-keyed QSO alongside
        // items for entirely other QSOs. `supersede_and_rekey_or_bundle`
        // can't merge an incoming `MultiTransmitRequest` at all (it returns
        // `NotViable` for that shape unconditionally), so superseding on
        // that coincidental ID-only match would still deassert PTT and
        // abandon a perfectly good transmission for a bundle with nothing
        // fresher to offer it — the exact PAN-73 dead-air failure this
        // whole guard exists to close, just reached through a bundle
        // instead of a single `TransmitRequest`. A bundle with NO changed
        // overlap (including no overlap at all) gets the same Requeue
        // treatment as a single-item candidate (PAN-73 round 2).
        MessageType::MultiTransmitRequest {
            items,
            origin,
            remote_client_key_id,
            ..
        } => {
            // Round 21: same authorization-aware duplicate check as the
            // single-`TransmitRequest` arm above — a bundle re-carrying an
            // in-flight item's exact content under DIFFERENT authorization
            // is a fresh, differently-authorized request, not the unchanged
            // repeat this overlap check exists to filter out.
            let authorization_changed =
                authorization_differs(*origin, remote_client_key_id.as_deref());
            let has_changed_overlap =
                items
                    .iter()
                    .any(|it| match find_in_flight(it.qso_id.as_deref()) {
                        Some(in_flight_item) => {
                            in_flight_item.message_text != it.message_text
                                || in_flight_item.frequency_offset != it.frequency_offset
                                || authorization_changed
                                || item_binding_differs(
                                    in_flight_item,
                                    it.origin,
                                    it.remote_client_key_id.as_deref(),
                                )
                        }
                        None => false,
                    });
            if has_changed_overlap {
                IncomingDuringTx::Supersede {
                    text: String::new(),
                    frequency_offset: 0.0,
                    qso_id: None,
                    tx_parity: None,
                }
            } else {
                IncomingDuringTx::Requeue
            }
        }
        _ => IncomingDuringTx::Drop,
    }
}

/// A frame a pivot replaced: its text, its payload (PAN-75) and its ledger
/// attempt (PAN-183).
pub(super) type ReplacedFrame = (
    String,
    pancetta_qso::UnsentAttemptPayload,
    Option<pancetta_qso::TxAttemptId>,
);

/// Pivot sources cleared from a refused frame or bundle, keyed by
/// `active_tx_qso_key` (PAN-75).
pub(super) type PivotReplaced = std::collections::HashMap<String, ReplacedFrame>;

/// A bundle refused before it aired: clear the tombstones its Step 4b pivot
/// inserted and return the frames those pivots replaced, so each item's
/// failure rolls back the replaced frame (see [`report_unaired_frame`]).
/// Clearing a tombstone hands the QSO's Autonomous outcome to the queued
/// request that produced the pivoted payload (ownership rule, PAN-185).
pub(super) fn unwind_bundle_pivots(
    keys: &[String],
    pivoted_once: &mut std::collections::HashMap<String, (String, f64)>,
    pivoted_from: &mut PivotReplaced,
) -> PivotReplaced {
    keys.iter()
        .filter_map(|key| {
            pivoted_once.remove(key);
            pivoted_from
                .remove(key)
                .map(|replaced| (key.clone(), replaced))
        })
        .collect()
}

/// The replaced frame recorded for `qso_id`'s pivot, if any.
pub(super) fn take_bundle_replaced(
    replaced: &mut PivotReplaced,
    qso_id: Option<&str>,
) -> Option<ReplacedFrame> {
    qso_id.and_then(|id| replaced.remove(&super::super::active_tx_qso_key(id)))
}

/// Outcome of `supersede_and_rekey_or_bundle` — replaces Task 6's bare
/// `bool`. A viable mid-TX supersede can now either fold the new request into
/// a multi-TX bundle alongside the in-flight content (`Bundle`, when
/// `max_concurrent_qsos > 1`; the actual per-item ~75 Hz frequency-separation
/// check runs in the CALLER via `encode_and_modulate_multi_tx`), or fully
/// replace the in-flight single frame (`Replace` — Task 6's original
/// behavior, and the single-TX caller's fallback when a bundle attempt
/// collides on frequency).
#[derive(Debug)]
pub(super) enum SupersedeOutcome {
    /// Not viable this slot (arrived too late, or the superseding message
    /// isn't a re-keyable single `TransmitRequest`). PTT was already
    /// deasserted. The superseding message comes back intact: the caller
    /// requeues it at the front of the TX inbox so it airs in its own next
    /// slot, never drops it (PAN-279).
    NotViable { superseder: Box<MessageType> },
    /// Single-item replace. The working `message_text`/`frequency_offset`/
    /// `schedule` have been mutated in place to the new item; the single-TX
    /// caller's `'key_and_send` retry re-encodes and re-keys with it.
    Replace {
        /// The NEW superseding request's OWN origin. The single-TX arm's
        /// in-place `'key_and_send` retry MUST re-gate (Step 4b-arm) and emit
        /// the re-keyed frame with THIS origin — never the aborted in-flight
        /// transmission's original origin. (C1: a `Remote` request superseding
        /// a `Local` in-flight frame otherwise re-keyed under the stale `Local`
        /// origin and skipped the key-time arm gate entirely; the reverse —
        /// `Local` superseding `Remote` — wrongly kept gating a local frame.)
        origin: crate::message_bus::TxOrigin,
        /// The NEW superseding request's OWN `remote_client_key_id` — carried
        /// alongside `origin` for the same reason (PAN-91): the re-keyed
        /// frame's arm-gate check at key-time must bind to THIS request's
        /// client, not the aborted in-flight frame's.
        remote_client_key_id: Option<String>,
        /// PAN-38 round 4 (Codex): the NEW superseding request's OWN
        /// `qso_id` — `message_text`/`frequency_offset`/`schedule` are
        /// mutated in place to the new request, but the caller's working
        /// `qso_id` local is a SEPARATE variable this function never
        /// touches. Without this, the caller's in-place retry paired the
        /// superseding frame's text with the ABORTED frame's `qso_id`: a
        /// successful re-key would clear the WRONG QSO's
        /// `pending_self_cq_qsos` entry (if the aborted frame was a
        /// tracked self-CQ) as though it had transmitted, while a
        /// downstream failure would roll back the wrong attempt. The
        /// caller must overwrite its `qso_id` local with this value
        /// alongside `origin`.
        qso_id: Option<String>,
        /// Codex P1, PR #362 round 18: `Replace` is returned for TWO
        /// unrelated reasons — a genuine over-capacity/frequency-collision
        /// fallback (single-TX callers and the multi-TX over-cap path both
        /// correctly abandon the in-flight side and key only the new
        /// request), and a multi-TX identity conflict (the in-flight bundle
        /// and the new request are both Remote but bound to DIFFERENT
        /// clients, so folding them would authorize one client's frame via
        /// the other's bundle-level check). The multi-TX caller
        /// (`supersede_multi_reenqueue`) previously treated every `Replace`
        /// as over-capacity and PRESERVED the in-flight bundle while
        /// dropping the new request — correct for over-capacity (the new
        /// request's own cadence retries later), but wrong for an identity
        /// conflict caused by a legitimate control transfer: the stale
        /// bundle is re-enqueued only to be rejected later against the new
        /// arm anyway, while the fresher, currently-authorized new request
        /// (e.g. a closing frame) is discarded and never retried. This flag
        /// lets that caller tell the two cases apart; single-TX callers
        /// ignore it since they always key the new request either way.
        identity_conflict: bool,
        /// Codex P1, PR #362 round 19: the NEW superseding request's OWN
        /// `tx_parity`. Single-TX callers re-key via the already-mutated
        /// `schedule` (resolved from this same value earlier in this
        /// function) and don't need it separately, but the multi-TX arm's
        /// identity-conflict path builds a fresh `TransmitRequest` from
        /// scratch to re-enqueue — hardcoding `None` there let the next
        /// dequeue's `TxSelfParity::Auto` recompute the nearest slot instead
        /// (normally the opposite slot, since this fires mid-playback inside
        /// the abandoned bundle's own slot), transmitting in sequential
        /// windows over the DX's expected reply instead of the QSO's own
        /// latched parity.
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        /// PAN-183: the NEW superseding request's OWN ledger attempt, for the
        /// same reason as `qso_id`: the re-keyed frame, or the request
        /// rebuilt to re-enqueue it, reports this attempt if it never airs.
        attempt_id: Option<pancetta_qso::TxAttemptId>,
    },
    /// Bundle-add is viable. The caller encodes `items` via
    /// `encode_and_modulate_multi_tx`; on success it re-enqueues a
    /// `MultiTransmitRequest` for them (picked up by the unmodified multi-TX
    /// arm), on a frequency collision it falls back to a single-item replace.
    /// `items.last()` is always the new request, and the working single-item
    /// state was ALSO mutated to it, so the single-TX caller can retry the
    /// replace with no extra plumbing.
    Bundle {
        items: Vec<crate::message_bus::TransmitRequestItem>,
        /// Fail-safe FOLDED origin for the re-enqueued `MultiTransmitRequest`:
        /// `Remote` if EITHER the in-flight stream OR the new request is
        /// `Remote`, so a mixed-origin fold is still gated by the bundle arm
        /// gate. Mirrors the coalescer's fail-safe origin fold.
        bundle_origin: crate::message_bus::TxOrigin,
        /// Folded client identity mirroring `bundle_origin` (PAN-91) — see
        /// `coalesce_backlog_into`'s `bundle_remote_client_key_id` for the
        /// same fold logic and its defense-in-depth caveat.
        bundle_remote_client_key_id: Option<String>,
        /// The NEW request's OWN origin — used when the caller falls back to a
        /// single-item replace on a frequency collision (that fallback drops
        /// the in-flight item and transmits ONLY the new one, so it must gate
        /// with the new request's own origin, not the folded bundle origin).
        new_origin: crate::message_bus::TxOrigin,
        /// The NEW request's OWN `remote_client_key_id`, mirroring `new_origin`.
        new_remote_client_key_id: Option<String>,
        /// The NEW request's OWN ledger attempt, for the single-item replace
        /// fallback (PAN-183). Each item in `items` keeps its own.
        new_attempt_id: Option<pancetta_qso::TxAttemptId>,
    },
}

/// The frame or bundle a supersede abandons.
pub(super) struct SupersedeInFlight<'a> {
    pub(super) items: &'a [crate::message_bus::TransmitRequestItem],
    pub(super) origin: crate::message_bus::TxOrigin,
    pub(super) remote_client_key_id: Option<String>,
    /// The in-flight request's own parity (single: its `TransmitRequest`;
    /// bundle: its `MultiTransmitRequest`).
    pub(super) tx_parity: Option<pancetta_core::slot::SlotParity>,
}

/// The single-TX arm's working frame, which a viable supersede rewrites.
pub(super) struct WorkingFrame<'a> {
    pub(super) message_text: &'a mut String,
    pub(super) frequency_offset: &'a mut f64,
    pub(super) schedule: &'a mut TxSchedule,
}

/// Worker state a supersede handler sends through.
pub(super) struct SupersedeIo<'a> {
    pub(super) message_bus: &'a MessageBus,
    pub(super) inbox: &'a TxInbox,
    pub(super) encoder: &'a mut Ft8Encoder,
    pub(super) ptt_guard: &'a mut PttGuard,
    /// The coordinator's Fox-admitted QSO keys, so a superseding bundle's
    /// encode uses the same Fox spacing and pairing as the worker's own
    /// (PAN-386).
    pub(super) fox_admitted_qsos: &'a std::sync::RwLock<std::collections::HashSet<String>>,
}

/// Recompute scheduling for a superseding request against *now* and, if
/// still viable within `timing.late_cap_ms()`, deassert PTT (clean stop of the
/// aborted transmission), and mutate `message_text`/`frequency_offset`/
/// `schedule` in place so the caller's retry loop re-runs Steps 1-10 with the
/// new content (the caller re-encodes at Step 1, and requests an audio-buffer
/// flush at Step 7 via the `is_rekey` flag so no stale samples from the
/// aborted transmission bleed into the re-key).
///
/// What the caller does next is carried by the returned [`SupersedeOutcome`].
///
/// Note: only `message_text`/`frequency_offset`/`schedule` are mutated —
/// `qso_id` intentionally tracks the ORIGINAL in-flight request (Phase 1
/// manual override; a cross-QSO supersede re-keys the new text but keeps the
/// original request's identity for liveness/tombstone bookkeeping; see the
/// design spec's "Known side effect" note on this cross-QSO characteristic).
///
/// The NEW request's `origin`, by contrast, is NOT discarded: it is returned
/// via `SupersedeOutcome::Replace{origin}` / `Bundle{new_origin}` so the
/// caller re-gates and emits the re-keyed frame with the superseding request's
/// OWN origin, never the aborted transmission's (the C1 fix — an in-place
/// re-key that reused the original `origin` would let a `Remote` supersede of a
/// `Local` frame skip the key-time arm gate). `in_flight.origin` is the origin
/// of the transmission being aborted, needed only to fail-safe-fold the
/// `bundle_origin` (Remote if EITHER stream is Remote).
pub(super) async fn supersede_and_rekey_or_bundle(
    new_request: MessageType,
    WorkingFrame {
        message_text,
        frequency_offset,
        schedule,
    }: WorkingFrame<'_>,
    ptt_guard: &mut PttGuard,
    timing: &SlotTimingContext,
    max_concurrent_qsos: u32,
    in_flight: &SupersedeInFlight<'_>,
) -> SupersedeOutcome {
    // Deassert PTT immediately and UNCONDITIONALLY — before we even inspect the
    // superseding message's type or its schedulability. EVERY return path out of
    // this function (viable re-key, too-late defer, or a non-re-keyable
    // `MultiTransmitRequest`) must unkey, because both callers assume it.
    // Doing this up front (rather than after the `TransmitRequest`
    // destructure) is what fixes the stuck-PTT bug when the superseding message
    // is a `MultiTransmitRequest`. (The aborted transmission's audio may still
    // be draining from the ring buffer; PTT-off means that's harmless — the
    // flush happens next, before new audio is pushed in Step 7 of the retry.)
    //
    // PAN-273: the unkey goes through the caller's guard. A delivered
    // PTT-off stands the guard down (stamps `last_ptt_off_ms`, clears
    // `ptt_active`); an undelivered one leaves it armed so its `Drop`
    // retries when the caller's arm exits. Callers never `disarm()` after
    // this returns. A `Replace`/`Bundle` re-key after an undelivered
    // release is serialized behind that retry: the re-key's
    // `PttGuard::new` waits for it before keying (Codex P2, PR #587).
    ptt_guard.release("Supersede").await;

    let (
        new_attempt_id,
        new_text,
        new_freq,
        new_tx_parity,
        new_qso_id,
        new_origin,
        new_remote_client_key_id,
    ) = match new_request {
        MessageType::TransmitRequest {
            attempt_id,
            message_text,
            frequency_offset,
            tx_parity,
            qso_id,
            origin,
            remote_client_key_id,
        } => (
            attempt_id,
            message_text,
            frequency_offset,
            tx_parity,
            qso_id,
            origin,
            remote_client_key_id,
        ),
        // A `MultiTransmitRequest` arriving as the superseding message isn't
        // re-keyable content here (Phase 1 manual triggers are single
        // requests) — not viable. PTT was already released above through the
        // caller's guard. Both callers requeue the returned bundle at the
        // front of the TX inbox (PAN-279).
        other => {
            return SupersedeOutcome::NotViable {
                superseder: Box::new(other),
            }
        }
    };

    let now = timing.slot_clock.now();
    let required_parity =
        resolve_required_parity(new_tx_parity, timing.tx_self_parity, now, timing.slot_ns);
    let new_schedule = schedule_tx(
        now,
        required_parity,
        timing.late_cap_ms(),
        timing.sample_rate,
        timing.slot_ns,
    );

    if new_schedule.deferred {
        info!(
            target: "pancetta::tx.pivot",
            "supersede: '{}' arrived too late to re-key this slot; requeued for its own next slot",
            new_text
        );
        return SupersedeOutcome::NotViable {
            superseder: Box::new(MessageType::TransmitRequest {
                attempt_id: new_attempt_id,
                message_text: new_text,
                frequency_offset: new_freq,
                tx_parity: new_tx_parity,
                qso_id: new_qso_id,
                origin: new_origin,
                remote_client_key_id: new_remote_client_key_id,
            }),
        };
    }

    info!(
        target: "pancetta::tx.pivot",
        "supersede: aborting in-flight TX, re-keying with '{}' @{:.0}Hz",
        new_text, new_freq
    );

    // Mutate the working single-item state to the new request unconditionally.
    // This IS the `Replace` result, and it also serves as the single-TX
    // caller's fallback if the `Bundle` attempt below collides on frequency —
    // the caller can retry the single item with no extra plumbing.
    *message_text = new_text.clone();
    *frequency_offset = new_freq;
    *schedule = new_schedule;

    // Prefer folding the new request into a multi-TX bundle alongside the
    // in-flight content when the station runs concurrent QSOs. The actual
    // frequency-separation check (>= ~75 Hz for FT8) lives inside
    // `encode_and_modulate_multi_tx`; the caller runs it and falls back to the
    // single-item `Replace` mutation above on a collision.
    let mut replace_is_identity_conflict = false;
    if max_concurrent_qsos > 1 {
        // PAN-73 round 5 (Codex): REPLACE the matching in-flight item, don't
        // append alongside it. A same-QSO, different-frequency candidate
        // (e.g. a stuck-DX collision-avoidance hop, now reachable here since
        // round 3 made the duplicate check frequency-aware) shares its
        // qso_id with an existing `in_flight_items` entry — appending a
        // second entry for that QSO would transmit BOTH the stale
        // old-frequency frame and the fresh new-frequency frame at once,
        // defeating the hop's entire purpose of moving off the colliding
        // frequency. `qso_id == None` (untracked/manual) never matches an
        // existing item, so it's still a plain append, unchanged.
        let new_key = new_qso_id.as_deref().map(super::super::active_tx_qso_key);
        let mut candidate_items: Vec<crate::message_bus::TransmitRequestItem> = in_flight
            .items
            .iter()
            .filter(|it| {
                new_key.is_none()
                    || it.qso_id.as_deref().map(super::super::active_tx_qso_key) != new_key
            })
            .cloned()
            .collect();
        candidate_items.push(crate::message_bus::TransmitRequestItem {
            attempt_id: new_attempt_id,
            message_text: new_text,
            frequency_offset: new_freq,
            qso_id: new_qso_id.clone(),
            origin: new_origin,
            remote_client_key_id: new_remote_client_key_id.clone(),
        });

        // Enforce the configured concurrent-QSO ceiling on the QSO-KEYED
        // stream count only — a manual/free-text item (`qso_id == None`) is
        // not a concurrent QSO and must never count against
        // `max_concurrent_qsos` (it's still bounded by the separate hard
        // `MAX_RETAINED_TX_STREAMS` check below). A same-QSO refresh (the
        // filter above already folded it into its existing slot) never
        // grows the QSO count, so it's never blocked here — only a
        // genuinely NEW QSO stream that would push the count over
        // `max_concurrent_qsos` falls through to `Replace` below
        // (abandoning the whole in-flight bundle in favor of just the new
        // request, the same fallback already used for a frequency
        // collision).
        let qso_count = candidate_items
            .iter()
            .filter(|it| it.qso_id.is_some())
            .count();
        // PAN-91 review follow-up: if BOTH sides are Remote and bound to
        // DIFFERENT clients, folding them into one bundle would let a
        // single bundle-level identity check authorize the other side's
        // frame by proxy — exactly the gap the coalescer's own per-entry
        // filter closes for a drained backlog. This function only ever
        // sees TWO identities (the whole in-flight side's, and the new
        // item's own), so a genuine conflict is a straight inequality
        // check. PAN-98: `None` (the WSJT-X UDP bridge) and `Some` (the
        // station agent) are also a conflict — they are different channels
        // with independent consent, and the bundle's single folded identity
        // would make its key-time gate check only ONE channel's consent.
        // Two `None` sides share a channel and are never a conflict. On a
        // real conflict, refuse to bundle — fall through to the existing
        // over-capacity path below, which abandons the in-flight side and
        // replaces it with just the new request; the retry loop's own
        // key-time gate re-verifies the new request's channel and identity
        // independently before it actually keys.
        let identity_conflict = super::super::remote_identities_conflict(
            (in_flight.origin, in_flight.remote_client_key_id.as_deref()),
            (new_origin, new_remote_client_key_id.as_deref()),
        );
        if identity_conflict {
            warn!(
                target: "pancetta::tx.policy",
                "supersede: not bundling — in-flight stream is bound to a different \
                 remote client ({:?}) than the new request ({:?}); replacing the \
                 in-flight transmission with just the new request instead",
                in_flight.remote_client_key_id, new_remote_client_key_id
            );
            replace_is_identity_conflict = true;
        } else if qso_count <= max_concurrent_qsos as usize
            && candidate_items.len() <= MAX_RETAINED_TX_STREAMS
        {
            // Fail-safe origin fold: the bundle carries BOTH the in-flight
            // stream and the new one, so it must be gated if EITHER is Remote.
            let bundle_origin = if in_flight.origin == crate::message_bus::TxOrigin::Remote
                || new_origin == crate::message_bus::TxOrigin::Remote
            {
                crate::message_bus::TxOrigin::Remote
            } else {
                crate::message_bus::TxOrigin::Local
            };
            // PAN-91: mirror the origin fold — prefer whichever side is
            // actually Remote. Safe to pick either non-`None` identity now
            // that `identity_conflict` above has already ruled out the two
            // sides disagreeing.
            let bundle_remote_client_key_id = in_flight
                .remote_client_key_id
                .clone()
                .or_else(|| new_remote_client_key_id.clone());
            return SupersedeOutcome::Bundle {
                items: candidate_items,
                bundle_origin,
                bundle_remote_client_key_id,
                new_origin,
                new_remote_client_key_id,
                new_attempt_id,
            };
        } else {
            info!(
                target: "pancetta::tx.policy",
                "supersede: not bundling — {} QSO-keyed stream(s) of {} total would exceed \
                 max_concurrent_qsos={} (hard cap {}); replacing the in-flight transmission \
                 with just the new request instead",
                qso_count,
                candidate_items.len(),
                max_concurrent_qsos,
                MAX_RETAINED_TX_STREAMS
            );
        }
    }

    SupersedeOutcome::Replace {
        origin: new_origin,
        remote_client_key_id: new_remote_client_key_id,
        qso_id: new_qso_id,
        identity_conflict: replace_is_identity_conflict,
        tx_parity: new_tx_parity,
        attempt_id: new_attempt_id,
    }
}

/// In-flight bundle items a `NotViable` multi-TX supersede really abandons:
/// every item, except those `reenqueued` (the incoming bundle put back on the
/// queue) carries unchanged for the same QSO under the same authorization,
/// which will still air (PAN-75). An authorization change (origin or bound
/// client) carries nothing: the old-authority attempt is abandoned. Since
/// PAN-136 that holds per item too: a same-content item re-bound to another
/// authority under an unchanged fold is not carried (Codex P2 on PR #456).
pub(super) fn items_abandoned_by_supersede<'a>(
    in_flight_items: &'a [crate::message_bus::TransmitRequestItem],
    in_flight_auth: (crate::message_bus::TxOrigin, Option<&str>),
    reenqueued: Option<&MessageType>,
) -> Vec<&'a crate::message_bus::TransmitRequestItem> {
    let carried: &[crate::message_bus::TransmitRequestItem] = match reenqueued {
        Some(MessageType::MultiTransmitRequest {
            items,
            origin,
            remote_client_key_id,
            ..
        }) if (*origin, remote_client_key_id.as_deref()) == in_flight_auth => items,
        _ => &[],
    };
    in_flight_items
        .iter()
        .filter(|item| !item_carried_unchanged(item, carried))
        .collect()
}

/// PAN-279: settle the single-TX arm's abandoned in-flight frame, unless
/// the requeued successor carries it unchanged (`carried`), so it airs under
/// its own attempt. A successor whose QSO keys (`successor_keys`) include the
/// frame's QSO owns the Autonomous outcome; the QSO engine always rolls back
/// the frame's own attempt.
pub(super) async fn settle_abandoned_single(
    message_bus: &MessageBus,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    successor_keys: &std::collections::HashSet<String>,
    carried: &[crate::message_bus::TransmitRequestItem],
) {
    if let Some(abandoned) = in_flight_items
        .first()
        .filter(|item| !item_carried_unchanged(item, carried))
    {
        settle_unaired(
            message_bus,
            TxFrame {
                qso_id: abandoned.qso_id.clone(),
                message_text: abandoned.message_text.clone(),
                attempt_id: abandoned.attempt_id,
            },
            None,
            tx_outcome::autonomous_outcome_for(abandoned.qso_id.as_deref(), successor_keys),
        )
        .await;
    }
}

/// The bundle items a requeued successor carries (none for a single request).
pub(super) fn carried_items(successor: &MessageType) -> &[crate::message_bus::TransmitRequestItem] {
    match successor {
        MessageType::MultiTransmitRequest { items, .. } => items,
        _ => &[],
    }
}

/// Whether `candidates` carries `item` unchanged: same QSO, text, offset
/// (within `PIVOT_OFFSET_EPSILON_HZ`), authorization and ledger attempt. A
/// carried item airs under its own attempt, so a supersede does not settle
/// it (PAN-75, PAN-279).
pub(super) fn item_carried_unchanged(
    item: &crate::message_bus::TransmitRequestItem,
    candidates: &[crate::message_bus::TransmitRequestItem],
) -> bool {
    candidates.iter().any(|c| {
        c.qso_id == item.qso_id
            && c.message_text == item.message_text
            && (c.frequency_offset - item.frequency_offset).abs()
                < super::super::PIVOT_OFFSET_EPSILON_HZ
            && c.origin == item.origin
            && c.remote_client_key_id == item.remote_client_key_id
            && c.attempt_id == item.attempt_id
    })
}

/// PAN-136 (Codex P2 on PR #456): whether `reenqueued` carries `item`'s exact
/// content for the same QSO under a different per-item binding. Such an item
/// is abandoned under its own (old) binding, which is what tells its attempt
/// apart from the re-bound same-text one in the PAN-75 rollback.
pub(super) fn item_rebound_by_supersede(
    item: &crate::message_bus::TransmitRequestItem,
    reenqueued: Option<&MessageType>,
) -> bool {
    let Some(MessageType::MultiTransmitRequest { items, .. }) = reenqueued else {
        return false;
    };
    items.iter().any(|c| {
        c.qso_id == item.qso_id
            && c.message_text == item.message_text
            && (c.frequency_offset - item.frequency_offset).abs()
                < super::super::PIVOT_OFFSET_EPSILON_HZ
            && (c.origin != item.origin || c.remote_client_key_id != item.remote_client_key_id)
    })
}

/// Settle an in-flight bundle item a supersede abandoned. The QSO engine gets
/// the item's own offset (and, when known, the bundle's binding) so the
/// rollback cannot land on a newer same-text attempt at another offset or
/// authority (PAN-75). The item's own ledger attempt decides the match when
/// it has one, so a folded bundle binding cannot miss or mis-pick it
/// (PAN-183). `autonomous` comes from [`tx_outcome::autonomous_outcome_for`]:
/// a successor that carries the item's QSO reports to Autonomous instead
/// (PAN-279).
pub(super) async fn report_abandoned_bundle_item(
    message_bus: &MessageBus,
    item: &crate::message_bus::TransmitRequestItem,
    remote_binding: Option<(bool, Option<String>)>,
    autonomous: AutonomousOutcome,
) {
    settle_unaired(
        message_bus,
        TxFrame {
            qso_id: item.qso_id.clone(),
            message_text: item.message_text.clone(),
            attempt_id: item.attempt_id,
        },
        Some(pancetta_qso::UnsentAttemptPayload {
            frequency_hz: item.frequency_offset,
            remote_binding,
        }),
        autonomous,
    )
    .await;
}

/// Settle every in-flight item a supersede abandoned for `successor`, which
/// is already requeued: items `successor` carries unchanged are left to air
/// under their own attempt; the rest settle under rule D6 (PAN-279).
pub(super) async fn settle_items_abandoned_for(
    message_bus: &MessageBus,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    successor: &MessageType,
    remote_binding: Option<(bool, Option<String>)>,
) {
    let carried = carried_items(successor);
    let keys = tx_outcome::successor_qso_keys(successor);
    for item in in_flight_items
        .iter()
        .filter(|item| !item_carried_unchanged(item, carried))
    {
        report_abandoned_bundle_item(
            message_bus,
            item,
            remote_binding.clone(),
            tx_outcome::autonomous_outcome_for(item.qso_id.as_deref(), &keys),
        )
        .await;
    }
}

/// Multi-TX arm's mid-TX supersede handler (Task 7 Step 4).
///
/// The multi-TX arm has no `'key_and_send` retry loop (unlike the single-TX
/// arm), so on a qualifying supersede it ALWAYS abandons the current in-flight
/// bundle and re-enqueues the resolved content back to the worker's own
/// channel (picked up next dequeue by the unmodified Steps 1-10) rather than
/// re-keying in place. This runs `supersede_and_rekey_or_bundle` (which
/// deasserts PTT), then:
///
/// - `Bundle` + encode Ok → re-enqueue a `MultiTransmitRequest` folding the
///   new request alongside the in-flight items.
/// - `Bundle` + encode Err (frequency collision) → re-enqueue just the new
///   item (the candidate bundle's last element) as a single `TransmitRequest`.
/// - `Replace` (`max_concurrent_qsos == 1`, an edge for a bundle-in-flight) →
///   re-enqueue the new single item, carrying the new request's own `qso_id`
///   (PAN-38 round 4: previously dropped, re-enqueued as a manual
///   drop-stale-ungated send).
/// - `NotViable` (the superseding message was itself a `MultiTransmitRequest`,
///   or a re-keyable request arrived too late for this slot) → re-enqueue the
///   superseding message unchanged at the front of the inbox, so it airs in
///   its own next slot (PAN-279; dropping it would lose the QSO engine's
///   freshest frame).
///
/// Every abandoned in-flight item settles through
/// [`tx_outcome::settle_unaired_frame`]; an item whose QSO the requeued
/// successor carries leaves the Autonomous outcome to it (PAN-279).
///
/// In every case `supersede_and_rekey_or_bundle` has already released PTT
/// through the caller's `PttGuard` (PAN-273), so the caller `continue`s after
/// this returns.
pub(super) async fn supersede_multi_reenqueue(
    new_request: MessageType,
    in_flight: &SupersedeInFlight<'_>,
    timing: &SlotTimingContext,
    max_concurrent_qsos: u32,
    io: SupersedeIo<'_>,
) {
    let SupersedeIo {
        message_bus,
        inbox,
        encoder,
        ptt_guard,
        fox_admitted_qsos,
    } = io;
    let in_flight_items = in_flight.items;
    let origin = in_flight.origin;
    let remote_client_key_id = &in_flight.remote_client_key_id;
    // The in-flight bundle's OWN `tx_parity` (from its `MultiTransmitRequest`
    // message) — threaded through so a `Replace` outcome that preserves this
    // bundle unchanged (see that arm below) carries the SAME parity forward,
    // not `None`. `None` would let `resolve_required_parity` re-derive an
    // "Auto" parity on the next dequeue, which can pick the wrong slot for an
    // explicit-parity bundle (PR #348 review round 3).
    let bundle_tx_parity = in_flight.tx_parity;
    let active_protocol = timing.active_protocol;
    let tx_params = &pancetta_ft8::ProtocolParams::from_protocol(active_protocol);
    // Scratch single-item working state: the multi-TX arm never reads it back
    // (it requeues rather than retrying in place), so these locals only
    // exist to satisfy `supersede_and_rekey_or_bundle`'s `&mut` contract and
    // to recover the new item on the `Replace` edge case.
    let mut scratch_text = String::new();
    let mut scratch_freq = 0.0f64;
    let mut scratch_schedule = TxSchedule {
        target_slot: timing.slot_clock.now(),
        silent_pad_samples: 0,
        cursor_offset_samples: 0,
        deferred: false,
    };
    match supersede_and_rekey_or_bundle(
        new_request,
        WorkingFrame {
            message_text: &mut scratch_text,
            frequency_offset: &mut scratch_freq,
            schedule: &mut scratch_schedule,
        },
        ptt_guard,
        timing,
        max_concurrent_qsos,
        in_flight,
    )
    .await
    {
        SupersedeOutcome::NotViable { superseder } => {
            // PTT already off; the in-flight bundle is abandoned by the caller.
            // Put the superseding message, whatever its type, at the front of
            // the worker's inbox (mirroring the `Bundle` requeue below) so the
            // next dequeue transmits it in its own next slot via the
            // unmodified Steps 1-10 (PAN-279). The newer message supersedes
            // the abandoned in-flight bundle, so exactly one stays pending
            // (no duplicate). The put-back cannot fail, so the reports below
            // can always lean on the requeued message (Codex P2 on PR #419).
            let reenqueued = *superseder;
            inbox.requeue_front(vec![self_message(reenqueued.clone())]);
            // PAN-75 (Codex P2 on PR #418): report each abandoned item, as the
            // `Replace` arm does, so its keep-call accounting is reversed. Items
            // the requeued bundle carries over unchanged will still air, so
            // they are not reported (Codex P1 on PR #419). Each is reported
            // with its own offset, and with the bundle's binding when the
            // supersede changed authorization (Codex on PR #419). An item
            // whose QSO the requeued message carries leaves the Autonomous
            // outcome to it (PAN-279).
            let auth_changed = matches!(
                &reenqueued,
                MessageType::MultiTransmitRequest { origin: o, remote_client_key_id: k, .. }
                    if (*o, k.as_deref()) != (origin, remote_client_key_id.as_deref())
            );
            let successor_keys = tx_outcome::successor_qso_keys(&reenqueued);
            let binding = auth_changed.then(|| {
                (
                    origin == crate::message_bus::TxOrigin::Remote,
                    remote_client_key_id.clone(),
                )
            });
            for item in items_abandoned_by_supersede(
                in_flight_items,
                (origin, remote_client_key_id.as_deref()),
                Some(&reenqueued),
            ) {
                let item_binding =
                    if binding.is_none() && item_rebound_by_supersede(item, Some(&reenqueued)) {
                        Some((
                            item.origin == crate::message_bus::TxOrigin::Remote,
                            item.remote_client_key_id.clone(),
                        ))
                    } else {
                        binding.clone()
                    };
                report_abandoned_bundle_item(
                    message_bus,
                    item,
                    item_binding,
                    tx_outcome::autonomous_outcome_for(item.qso_id.as_deref(), &successor_keys),
                )
                .await;
            }
        }
        SupersedeOutcome::Bundle {
            items,
            bundle_origin,
            bundle_remote_client_key_id,
            new_origin,
            new_remote_client_key_id,
            new_attempt_id,
        } => {
            let bundle_outcome = encode_and_modulate_multi_tx_with_fox(
                encoder,
                active_protocol,
                tx_params,
                &items,
                &fox_stream_predicate(fox_admitted_qsos),
            );
            let successor = if bundle_outcome.samples.is_ok() {
                MessageType::MultiTransmitRequest {
                    items,
                    // PR #348 review round 4 (Codex P1): carry the established
                    // in-flight bundle's own parity forward, not `None` — the
                    // same reasoning as the `Replace` arm's `bundle_tx_parity`
                    // fix above applies here too (an "Auto" re-resolution on
                    // the next dequeue can pick the wrong slot for an
                    // explicit-parity bundle).
                    tx_parity: bundle_tx_parity,
                    // Fail-safe folded origin (Remote if either the in-flight
                    // bundle or the new request was Remote), NOT the in-flight
                    // bundle's origin — otherwise a Remote item folded onto a
                    // Local in-flight bundle would re-enter ungated.
                    origin: bundle_origin,
                    remote_client_key_id: bundle_remote_client_key_id,
                }
            } else {
                // Frequency collision: fall back to requeueing only the new
                // item (always the last element of the candidate bundle). This
                // transmits ONLY the new stream, so it re-enters with the NEW
                // request's own origin, not the in-flight bundle's.
                let new_item = items
                    .last()
                    .cloned()
                    .expect("candidate bundle always contains the new item");
                MessageType::TransmitRequest {
                    attempt_id: new_attempt_id,
                    message_text: new_item.message_text,
                    frequency_offset: new_item.frequency_offset,
                    qso_id: new_item.qso_id,
                    tx_parity: None,
                    origin: new_origin,
                    remote_client_key_id: new_remote_client_key_id,
                }
            };
            inbox.requeue_front(vec![self_message(successor.clone())]);
            // PAN-279: in-flight items the requeued bundle carries unchanged
            // air under their own attempt; every other one (the new request's
            // QSO's replaced frame, or all of them on the collision fallback)
            // settles here, leaving the Autonomous outcome to a same-QSO
            // successor.
            settle_items_abandoned_for(message_bus, in_flight_items, &successor, None).await;
        }
        SupersedeOutcome::Replace {
            origin: new_origin,
            remote_client_key_id: new_remote_client_key_id,
            qso_id: new_qso_id,
            identity_conflict,
            tx_parity: new_tx_parity,
            attempt_id: new_attempt_id,
        } => {
            // Codex P1, PR #362 round 18: an `identity_conflict` Replace
            // means control legitimately transferred (the in-flight bundle
            // and the new request are both Remote but bound to DIFFERENT
            // clients) — NOT over-capacity. Re-enqueuing the stale in-flight
            // bundle here is actively harmful in that case: it will only be
            // rejected later against the NEW client's arm anyway, and
            // meanwhile the fresher, currently-authorized new request (e.g.
            // a closing frame from the client that now legitimately holds
            // the arm) would be the one silently discarded. Treat this
            // exactly like the single-TX arm's ordinary Replace: abandon the
            // in-flight side and key only the new request. This can't
            // recreate the round-6 livelock (below) — nothing gets
            // re-enqueued for the worker's next dequeue to immediately
            // collide with.
            if identity_conflict {
                let successor = MessageType::TransmitRequest {
                    attempt_id: new_attempt_id,
                    message_text: scratch_text,
                    frequency_offset: scratch_freq,
                    qso_id: new_qso_id,
                    tx_parity: new_tx_parity,
                    origin: new_origin,
                    remote_client_key_id: new_remote_client_key_id,
                };
                inbox.requeue_front(vec![self_message(successor.clone())]);
                // PAN-183 (PR #419 thread 4244657620): each abandoned item
                // reports its own attempt, so the rollback cannot pick the
                // newer, differently bound same-text request keyed instead.
                // PAN-279: the item for the requeued request's QSO leaves the
                // Autonomous outcome to it.
                let successor_keys = tx_outcome::successor_qso_keys(&successor);
                for item in in_flight_items {
                    settle_unaired(
                        message_bus,
                        TxFrame {
                            qso_id: item.qso_id.clone(),
                            message_text: item.message_text.clone(),
                            attempt_id: item.attempt_id,
                        },
                        None,
                        tx_outcome::autonomous_outcome_for(item.qso_id.as_deref(), &successor_keys),
                    )
                    .await;
                }
                return;
            }

            // PR #348 review round 2 (Codex P2): the multi-TX arm's in-flight
            // item is ALWAYS a real bundle (`in_flight_items.len() >= 2` —
            // this function is only reached from the multi-TX arm), so a
            // non-identity-conflict `Replace` outcome here can only mean the
            // candidate bundle would have exceeded `max_concurrent_qsos` (a
            // frequency collision instead produces a `Bundle` whose caller
            // falls back to a single-item re-enqueue in the arm above, never
            // `Replace`). Silently dropping every previously-admitted bundle
            // item to make room for one new request would burn all of their
            // recorded attempts for nothing. Preserve the in-flight bundle
            // unchanged (re-encoding happens fresh at pickup time, so
            // there's no staleness concern).
            if !in_flight_items.is_empty() {
                inbox.requeue_front(vec![self_message(MessageType::MultiTransmitRequest {
                    items: in_flight_items.to_vec(),
                    tx_parity: bundle_tx_parity,
                    origin,
                    remote_client_key_id: remote_client_key_id.clone(),
                })]);
            }
            // PR #348 review round 6 (Codex P1): do NOT re-enqueue the
            // over-cap new request behind the just-preserved bundle — the
            // worker dequeues that bundle next, and its own pre-PTT/pre-audio
            // wait (`interruptible_sleep_or_supersede`) would immediately
            // find this exact request sitting right behind it and treat it
            // as a fresh supersede of the bundle it hasn't even started
            // transmitting yet, re-entering this same arm and re-enqueueing
            // the identical pair forever — a livelock where the bundle never
            // actually transmits. Drop it instead, matching this file's
            // established "excluded, not gone" semantics for parity/
            // frequency conflicts and cap truncation elsewhere: the QSO's
            // own keep-call/rearm cadence produces a fresh request next
            // cycle, evaluated fresh against whatever the in-flight bundle
            // looks like by then.
            info!(
                target: "pancetta::tx.policy",
                "supersede (multi-TX): dropping over-cap request '{}' rather than requeueing it \
                 behind the preserved bundle (would livelock); its own cadence will retry next cycle",
                scratch_text
            );
            if let Err(e) = report_transmit_complete(
                message_bus,
                false,
                scratch_text,
                0,
                new_qso_id,
                new_attempt_id,
            )
            .await
            {
                warn!("Failed to send TransmitComplete: {}", e);
            }
            let _ = scratch_freq; // written by supersede_and_rekey_or_bundle, unused on this path
        }
    }
}
