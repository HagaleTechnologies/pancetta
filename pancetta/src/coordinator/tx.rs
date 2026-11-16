//! FT8 transmitter component.
//!
//! Owns the encode → modulate → slot-aligned audio output → PTT key/unkey
//! sequence. Runs on the message-bus channel for `MessageType::TransmitRequest`
//! arriving from the QSO state machine, the autonomous operator, or the
//! TUI command-forwarding loop.
//!
//! The `PttGuard` RAII helper ensures the radio is keyed back to RX even
//! if the transmitter task is cancelled mid-transmission — without it a
//! panic in the audio output path would leave the rig stuck on TX.

use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{
    AckState, ComponentId, ComponentMessage, DeliveryAck, MessageBus, MessageType,
    PlaybackStartAck, PlaybackStartState, PlaybackStartWaiter, SendIfOutcome,
};

use super::tx_log::{TxFrameLog, TxFrameRecord, TX_LOG_ACK_WAIT};
use super::tx_outcome::{self, AutonomousOutcome, TxFrame};

/// FT8 nominal pre-roll: audio starts 500ms past the slot boundary.
const DELAY_MS: u64 = 500;

/// Total `TransmitRequest`/`MultiTransmitRequest`/`TuneRequest` messages this
/// worker has received this session, incremented before any policy gating
/// (docs/observability-diagnostics-plan.md Layer 3 health panel). Process-
/// global, matching the existing `DECODE_PANIC_COUNT`
/// (`coordinator/ft8.rs`) / `PANIC_COUNT` (`main.rs`) counter pattern — no
/// new locking, no new message-passing.
static TX_ATTEMPTS_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Total single-TX requests deferred to a later slot because the request
/// arrived too late to make the current one
/// (`schedule.deferred`, single-TX `TransmitRequest` arm only — multi-TX has
/// no equivalent defer path).
static TX_DEFERS_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read-only accessor for the Layer 3 health panel (`coordinator/tui_relay.rs`).
pub(crate) fn tx_attempts_count() -> u64 {
    TX_ATTEMPTS_COUNT.load(Ordering::Relaxed)
}

/// Read-only accessor for the Layer 3 health panel (`coordinator/tui_relay.rs`).
pub(crate) fn tx_defers_count() -> u64 {
    TX_DEFERS_COUNT.load(Ordering::Relaxed)
}

/// Multi-TX coalesce collection window.
///
/// Fix for the "slow-start" bug: when the operator manually starts several
/// same-window QSOs in quick succession, each opening is a separate keypress
/// that crosses two async hops before its `TransmitRequest` reaches this worker.
/// The worker used to coalesce the instant the FIRST request arrived (~10ms),
/// committing the slot to a single stream; the sibling openings landed after the
/// batch was formed and only joined on the next slot via keep-call rearm — so the
/// streams trickled in one-per-cycle instead of all firing in the first window.
///
/// On popping a `TransmitRequest` head we now wait this brief window before
/// coalescing, so same-parity openings emitted close together batch into one
/// `MultiTransmitRequest`. The window is **absorbed by the subsequent
/// slot-wait** (Step 6 keys PTT then sleeps to the slot boundary; audio still
/// goes out at the boundary), so it adds no real latency in the common
/// single-QSO case. Kept short so a request arriving in the final fraction
/// before its slot is rarely pushed to the next slot.
///
/// This is the FT8 baseline — see [`coalesce_collect_window_ms`] for the
/// protocol-scaled value actually used at the call site.
const COALESCE_COLLECT_WINDOW_MS: u64 = 800;

/// Scale the coalesce-collection wait to the active protocol's slot period.
///
/// 800ms was tuned against FT8's 15s slot, which has ~2s of decode-phase
/// margin to spare before the next TX boundary. FT4 (7.5s slot, ~1s margin)
/// and FT2 (3.2s slot) can't absorb the same fixed wait for free — it's a
/// meaningful fraction of their whole slot, not "absorbed by the slot-wait"
/// the way the FT8 comment above describes. Investigation 2026-07-05
/// (operator-reported FT4 TX truncated to ~1-2s) found FT4 replies were
/// consistently scheduled 3+ seconds late into their target slot, causing
/// `schedule_tx`'s late-skip-ahead cursor to consume most of the 5.04s
/// waveform. This 800ms wait is a fixed, purely artificial slice of that
/// lateness we control directly — scaling it down proportionally claws back
/// real margin with no decode-quality tradeoff (unlike widening the
/// decode-phase margin itself, which trades against audio-capture
/// completeness and needs its own design pass — see
/// `docs/superpowers/specs/` for the follow-up). FT8 is byte-identical
/// (`cycle_duration` = 15.0 → ratio 1.0 → 800ms, unchanged).
fn coalesce_collect_window_ms(protocol: pancetta_ft8::Protocol) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((COALESCE_COLLECT_WINDOW_MS as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}

/// Mode-scaled `tx_late_max_ms` cap. Mirrors `coalesce_collect_window_ms`'s
/// cycle-ratio scaling exactly (see docs/superpowers/specs/2026-07-22-
/// mid-tx-abort-restart-design.md "tx_late_max_ms mode-scaling"). FT8 stays
/// byte-identical; FT4/FT2 get a proportionally tighter late-viability cap
/// since their slots are shorter. Closes the gap flagged in
/// `COALESCE_MAX_EXTENSION_MS`'s doc comment: `tx_late_max_ms` itself was
/// previously unscaled, so it exceeded FT4's whole 7.5s slot and the "too
/// late, defer" branch of `schedule_tx` could never fire for FT4.
fn tx_late_max_ms_effective(protocol: pancetta_ft8::Protocol, tx_late_max_ms: u64) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((tx_late_max_ms as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}

/// FT8-baseline cap on total EXTENSION time (beyond the mandatory base
/// `COALESCE_COLLECT_WINDOW_MS` wait) the Symptom-C adaptive coalesce window
/// may add. Scaled by the same cycle-ratio as `coalesce_collect_window_ms`
/// for FT4/FT2. Independent of `tx_late_max_ms` — the remaining-headroom cap
/// computed in `adaptive_coalesce_cap_ms` already bounds against that; this
/// is a second, protocol-proportionate ceiling so a busy pileup can't
/// monopolize an outsized fraction of a short FT4/FT2 slot even when
/// tx_late_max_ms headroom alone would allow it. See
/// docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §1.
const COALESCE_MAX_EXTENSION_MS: u64 = 3000;

/// Safety margin subtracted from remaining `tx_late_max_ms` headroom before
/// it's used as the adaptive window's extension cap. Covers ONLY Step 1's
/// encode/modulate time and other fixed per-message overhead AFTER the
/// coalesce point — the mandatory base collection window
/// (`coalesce_collect_window_ms`, always elapsed before any extension) is
/// subtracted from the headroom explicitly in `adaptive_coalesce_cap_ms`
/// (PAN-117). Without both, a request that arrives with headroom just barely
/// covering the extension alone could still get pushed past the
/// `tx_late_max_ms` cliff the key-time final gate enforces.
const COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// Protocol-scaled `COALESCE_MAX_EXTENSION_MS` — see that constant's doc.
fn coalesce_max_extension_ms(protocol: pancetta_ft8::Protocol) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((COALESCE_MAX_EXTENSION_MS as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}

/// PAN-117 (PR #450 review): the cap the QUEUE-TIME current-slot
/// acceptance decision (`schedule_tx`'s `use_current`) is decided
/// against. Between the frozen `request_received_at` timestamp and the
/// key-time final gate, the unconditional base coalesce window
/// (`coalesce_collect_window_ms`, 800ms FT8) ALWAYS elapses first, and
/// the encode/modulate + gate-await overhead the safety margin covers
/// follows it. Deciding `use_current` against the raw effective cap
/// therefore accepted a request arriving 2.2-3.0s into its FT8 slot
/// (1.1-1.5s FT4) into the CURRENT slot while the same fresh-clock gate
/// was guaranteed to drop it — scheduled, dropped, never deferred.
/// Reserving both quantities below the cap makes such a request DEFER
/// to the next slot (30s) instead; the key-time gate keeps the raw
/// effective cap as its unconditional backstop, and
/// `adaptive_coalesce_cap_ms`'s probe passes this same value so its
/// deferred branch matches the decision the worker actually makes.
fn current_slot_acceptance_cap_ms(protocol: pancetta_ft8::Protocol, tx_late_max_ms: u64) -> u64 {
    tx_late_max_ms_effective(protocol, tx_late_max_ms)
        .saturating_sub(coalesce_collect_window_ms(protocol))
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS)
}

/// PAN-117 (PR #450 review round 2): the multi-TX arm's queue-time
/// acceptance cap. Only a bundle COALESCED from a `TransmitRequest` head
/// sat through the unconditional base coalesce window after its frozen
/// `request_received_at`; a `MultiTransmitRequest` dequeued directly (an
/// autonomous bundle, or a mid-TX supersede re-enqueue) skips that wait.
/// Reserving the base window for it too would defer a bundle that can
/// still key before the raw cap, so it reserves only the safety margin.
fn multi_tx_acceptance_cap_ms(
    protocol: pancetta_ft8::Protocol,
    tx_late_max_ms: u64,
    waited_base_window: bool,
) -> u64 {
    if waited_base_window {
        current_slot_acceptance_cap_ms(protocol, tx_late_max_ms)
    } else {
        tx_late_max_ms_effective(protocol, tx_late_max_ms)
            .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS)
    }
}

/// Remaining `tx_late_max_ms` headroom, in ms, available to extend the
/// Symptom-C adaptive coalesce window for the given head request — bounds
/// the window so it can never push a request past the late-skip cliff.
/// The headroom subtracts the mandatory base collection window
/// (`coalesce_collect_window_ms`) explicitly: the base wait always runs in
/// full before any extension, so extension headroom that ignored it would
/// deliberately stretch a request past the very cap the key-time final
/// gate enforces (PAN-117). The headroom is measured against whichever
/// slot the head resolves to, current or DEFERRED: a deferred target that
/// begins a whole slot later leaves headroom far above the protocol-scaled
/// `COALESCE_MAX_EXTENSION_MS` ceiling (so the ceiling applies), but one
/// that begins moments away (a head arriving at :14.5 for the :15 slot)
/// is bounded by its own cliff too (PAN-117, PR #450 Codex review), or the
/// stretch would carry it past the key-time cap. Returns `0` if `head`
/// isn't a `TransmitRequest` (defensive — the only caller checks this
/// first).
fn adaptive_coalesce_cap_ms(
    head: &MessageType,
    request_received_at: chrono::DateTime<chrono::Utc>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
    protocol: pancetta_ft8::Protocol,
) -> u64 {
    let MessageType::TransmitRequest { tx_parity, .. } = head else {
        return 0;
    };
    let required_parity =
        resolve_required_parity(*tx_parity, tx_self_parity, request_received_at, slot_ns);
    let tx_late_max_ms_eff = tx_late_max_ms_effective(protocol, tx_late_max_ms);
    // PAN-117 (PR #450 review): probe with the SAME acceptance cap the
    // worker's queue-time decision uses, so `probe.target_slot` is the
    // slot the real decision will pick — a head in the reserved band
    // below the raw cap defers at the decision, so the headroom must be
    // measured against that deferred slot, not a current-slot target it
    // will never get.
    let probe = schedule_tx(
        request_received_at,
        required_parity,
        current_slot_acceptance_cap_ms(protocol, tx_late_max_ms),
        sample_rate,
        slot_ns,
    );
    let protocol_ceiling = coalesce_max_extension_ms(protocol);
    // Signed: negative when the target slot (a deferred one) has not begun
    // yet, which ADDS the time until it starts to the headroom. A current
    // slot target is never ahead of `request_received_at`.
    let elapsed_in_slot_ms = (request_received_at - probe.target_slot).num_milliseconds();
    let headroom = (tx_late_max_ms_eff as i64)
        .saturating_sub(elapsed_in_slot_ms)
        .saturating_sub(coalesce_collect_window_ms(protocol) as i64)
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS as i64)
        .max(0) as u64;
    headroom.min(protocol_ceiling)
}

/// Output of `schedule_tx`: where to TX, how much silence to pad in
/// front, and how far into the modulated waveform to start emitting.
#[derive(Debug, Clone, Copy)]
pub struct TxSchedule {
    /// UTC time of the slot boundary we're targeting.
    pub target_slot: chrono::DateTime<chrono::Utc>,
    /// Number of zero samples to emit before the modulated waveform.
    pub silent_pad_samples: usize,
    /// Sample offset into the waveform — caller emits `waveform[cursor..]`.
    pub cursor_offset_samples: usize,
    /// `true` when we could NOT use the current slot and deferred to a later
    /// slot of the required parity (the "too late" / wrong-parity branch). The
    /// caller surfaces this to the TUI strip so a deferred item shows
    /// "deferred 30s" instead of looking dead.
    pub deferred: bool,
}

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
        qso_id.map(super::active_tx_qso_key).and_then(|key| {
            in_flight_items
                .iter()
                .find(|it| it.qso_id.as_deref().map(super::active_tx_qso_key) == Some(key.clone()))
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
            if super::is_pivot_duplicate(
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

/// WSJT-X-style late-start TX scheduler.
///
/// Picks the slot to TX in (current slot if parity matches and we're
/// within `tx_late_max_ms`, otherwise next slot of `required_parity`),
/// then decides how to align audio relative to that slot's boundary:
///
/// - **Early or just-arrived** (`mstr < DELAY_MS`): pad `(DELAY_MS - mstr)`
///   ms of zeros in front, emit the full 12.64s waveform starting at
///   slot+500ms (the FT8 pre-roll).
/// - **Late but viable** (`DELAY_MS <= mstr <= tx_late_max_ms`): skip
///   `(mstr - DELAY_MS)` ms into the waveform. WSJT-X's `m_ic` analogue.
/// - **Too late** (`mstr > tx_late_max_ms`): defer to the next slot of
///   the required parity (30s away), recompute as the early case.
///
/// `slot_ns` is the active slot period (FT8 = 15e9, FT4 = 7.5e9). All
/// slot-grid math is computed against it; passing
/// `pancetta_core::slot::SLOT_NS` is byte-identical to the FT8 behavior.
///
/// PAN-117 (PR #450 review): the worker arms pass an acceptance cap here —
/// not the raw effective cap — so the current slot is only accepted when
/// the request can absorb the coalesce/encode overhead that runs before
/// the key: `current_slot_acceptance_cap_ms` for a request that took the
/// base coalesce wait, `multi_tx_acceptance_cap_ms` (margin only) for a
/// directly dequeued bundle that did not. The key-time final gate keeps
/// enforcing the raw effective cap.
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
) -> TxSchedule {
    use pancetta_core::slot::{
        current_slot_start_with_period, next_slot_with_parity_with_period, SlotParity,
    };

    let cur_start = current_slot_start_with_period(now, slot_ns);
    let cur_parity = SlotParity::of_with_period(cur_start, slot_ns);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let use_current = cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms;
    let target = if use_current {
        cur_start
    } else {
        next_slot_with_parity_with_period(now, required_parity, slot_ns)
    };
    let deferred = !use_current;

    let (silent_pad_samples, cursor_offset_samples) =
        pad_and_cursor_for_target(now, target, sample_rate);

    TxSchedule {
        target_slot: target,
        silent_pad_samples,
        cursor_offset_samples,
        deferred,
    }
}

/// Silent-pad / cursor-skip math for a FIXED target slot boundary, given the
/// instant audio is about to actually ship. Split out of `schedule_tx` so a
/// later, more accurate clock read can refresh the pad/cursor WITHOUT
/// re-deciding which slot to target — that decision (the `use_current` check
/// above) must only ever be made once, off the frozen pre-coalesce
/// `request_received_at` timestamp (see
/// docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md
/// §2). Re-running the full slot-selection logic with a later timestamp
/// risks flipping `deferred` after downstream gates/pivots already assumed
/// the original decision; this function can't do that — it only computes
/// how far into (or before) an already-chosen `target_slot` `now` falls.
fn pad_and_cursor_for_target(
    now: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    sample_rate: u32,
) -> (usize, usize) {
    // mstr relative to the target. When target is in the future,
    // (now - target) is negative; clamp so we hit the early branch.
    let mstr_signed = (now - target_slot).num_milliseconds();
    let mstr_unsigned = mstr_signed.max(0) as u64;

    let (silent_pad_ms, cursor_ms) = if mstr_unsigned < DELAY_MS {
        (DELAY_MS - mstr_unsigned, 0)
    } else {
        (0, mstr_unsigned - DELAY_MS)
    };

    (
        (silent_pad_ms as usize) * (sample_rate as usize) / 1000,
        (cursor_ms as usize) * (sample_rate as usize) / 1000,
    )
}

/// Sleep for `total` duration, but wake early (return `true`) if EITHER
/// the shutdown flag or the abort-current-tx flag flips while we wait.
/// Polled in 50ms chunks so worst-case wake latency is ~50ms.
///
/// Used inside the TX worker's per-message arm to guarantee both Ctrl-Q
/// (whole-app shutdown) and F8 (abort current TX, keep app running)
/// take effect within ~50ms. Without this, each `sleep().await` was
/// uninterruptible and the worker could continue driving PTT and audio
/// for ~13 seconds after the operator asked it to stop.
///
/// The caller checks `shutdown.load()` after wake to distinguish:
/// - shutdown set → break the outer worker loop
/// - shutdown clear, abort set → reset abort, send TransmitComplete
///   failure, `continue` to the next message
async fn interruptible_sleep(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    use std::sync::atomic::Ordering;
    if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
        return true;
    }
    let chunk = Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + total;
    while tokio::time::Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(remaining.min(chunk)).await;
    }
    false
}

/// PAN-114 (lane5 H-16): one evaluation of the pre-PTT wait, re-derived
/// from a fresh wall-clock read each chunk. The old single monotonic sleep
/// keyed a frame 4.5 s into its waveform after a 5 s forward jump, and held
/// PTT 600 s after a 600 s backward step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttWaitStep {
    Sleep(Duration),
    Reached,
    ClockJumped(pancetta_core::slot_clock::WallClockJump),
}

/// Pure decision behind [`wait_for_ptt_instant`]. Drops the frame only if
/// (a) the PTT instant is now more than two periods away (a backward step:
/// `schedule_tx` never targets further ahead than that), or (b) the wait
/// started before the PTT instant and `now` is more than `late_max_ms` past
/// `target_slot` (a forward jump past the late-start policy). A request that
/// was already late when the wait began keeps today's path exactly.
fn ptt_wait_step(
    now: chrono::DateTime<chrono::Utc>,
    ptt_target: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    waited_from_before_target: bool,
    late_max_ms: u64,
    slot_ns: i64,
) -> PttWaitStep {
    use pancetta_core::slot_clock::WallClockJump;
    let remaining = ptt_target - now;
    // schedule_tx never targets more than two slots ahead.
    if remaining > chrono::Duration::nanoseconds(2 * slot_ns) {
        return PttWaitStep::ClockJumped(WallClockJump::Backward(remaining));
    }
    if remaining > chrono::Duration::zero() {
        let chunk = remaining
            .to_std()
            .unwrap_or(Duration::ZERO)
            .min(Duration::from_millis(50));
        return PttWaitStep::Sleep(chunk);
    }
    if waited_from_before_target
        && now - target_slot > chrono::Duration::milliseconds(late_max_ms as i64)
    {
        return PttWaitStep::ClockJumped(WallClockJump::Forward(now - ptt_target));
    }
    PttWaitStep::Reached
}

/// PAN-117 (six-lane review L5 H-7): `true` when `now` falls more than
/// `late_max_ms` past `target_slot` — keying then would put so little of
/// the waveform on the air that no receiver could decode it. Applied at
/// the last instant before PTT in both worker arms (the Step 4d final
/// gate), because everything after the queue-time `schedule_tx` decision
/// (coalesce waits, encode/modulate, gate awaits) adds real delay the
/// queue-time check never sees. The boundary is inclusive on the key side
/// (`elapsed == cap` keys), mirroring `schedule_tx`'s
/// `mstr <= tx_late_max_ms`; a deferred target (target_slot in the
/// future) makes `now - target_slot` negative, so this only bites
/// current-slot late starts. Mid-wait clock jumps stay covered by
/// PAN-114's `ptt_wait_step` above.
fn key_time_too_late(
    now: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    late_max_ms: u64,
) -> bool {
    now - target_slot > chrono::Duration::milliseconds(late_max_ms as i64)
}

/// Outcome of [`wait_for_ptt_instant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttWait {
    /// The wall clock reached the PTT instant (or the frame ships as a
    /// policy late start).
    Reached,
    /// Shutdown or F8 abort flipped; same caller handling as
    /// `interruptible_sleep` returning `true`.
    Interrupted,
    /// A wall-clock jump moved the PTT instant out of reach; drop the frame.
    ClockJumped(pancetta_core::slot_clock::WallClockJump),
}

/// PAN-114: wait for the PTT engage instant on the WALL clock (re-read via
/// `wall_now` every <= 50 ms chunk) instead of one monotonic sleep computed
/// up front. Checks `shutdown`/`abort` before each evaluation, exactly like
/// [`interruptible_sleep`], so wake latency stays ~50 ms. With no clock
/// jump this returns `Reached` at the same wall instant the old monotonic
/// sleep ended.
async fn wait_for_ptt_instant(
    ptt_target: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    late_max_ms: u64,
    slot_ns: i64,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    wall_now: impl Fn() -> chrono::DateTime<chrono::Utc>,
) -> PttWait {
    use std::sync::atomic::Ordering;
    let mut waited_from_before_target: Option<bool> = None;
    loop {
        if shutdown.load(Ordering::Acquire) || abort.load(Ordering::Acquire) {
            return PttWait::Interrupted;
        }
        let now = wall_now();
        let waited = *waited_from_before_target.get_or_insert(ptt_target > now);
        match ptt_wait_step(now, ptt_target, target_slot, waited, late_max_ms, slot_ns) {
            PttWaitStep::Sleep(chunk) => sleep(chunk).await,
            PttWaitStep::Reached => return PttWait::Reached,
            PttWaitStep::ClockJumped(jump) => return PttWait::ClockJumped(jump),
        }
    }
}

/// How a shared TX-worker step ends the current request (PAN-322). A helper
/// cannot jump to a loop label, so each call site turns this into its own
/// jump: `break 'worker` / `continue 'worker` in the single-frame arm,
/// `break` / `continue` in the bundle arm.
#[derive(Debug, PartialEq, Eq)]
enum TxExit {
    /// Shutdown: leave the worker loop.
    StopWorker,
    /// The request is settled: take the next one.
    NextRequest,
}

/// Which worker arm a shared step runs for (PAN-322). The arms differ on
/// purpose (log text, whether the TX strip is cleared), and the helpers
/// pick by shape instead of folding one copy into the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TxShape {
    /// The single-frame `TransmitRequest` arm.
    Single,
    /// The multi-TX bundle arm.
    Bundle,
}

/// Step 4 of both worker arms (PAN-322): act on a [`wait_for_ptt_instant`]
/// outcome. `Reached` continues to PTT. `ClockJumped` drops every frame
/// through [`drop_frames_for_clock_jump`]. `Interrupted` stops the worker on
/// shutdown, sending nothing; on an F8 abort it reports every frame as a
/// failed `TransmitComplete`. `frames` yields `(text, qso_id, attempt_id)`
/// per frame in the arm's order, and runs only for a non-`Reached` outcome.
async fn on_ptt_wait(
    wait: PttWait,
    shape: TxShape,
    frames: impl FnOnce() -> Vec<(String, Option<String>, Option<pancetta_qso::TxAttemptId>)>,
    message_bus: &MessageBus,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::ops::ControlFlow<TxExit> {
    use std::ops::ControlFlow;
    use std::sync::atomic::Ordering;
    match wait {
        PttWait::Reached => ControlFlow::Continue(()),
        PttWait::ClockJumped(jump) => {
            // One drop per frame (per bundle item), like the F8 abort below.
            drop_frames_for_clock_jump(message_bus, &frames(), jump, &mut PivotReplaced::new())
                .await;
            ControlFlow::Break(TxExit::NextRequest)
        }
        PttWait::Interrupted => {
            if shutdown.load(Ordering::Acquire) {
                match shape {
                    TxShape::Single => info!("TX aborted before PTT engage by shutdown"),
                    TxShape::Bundle => info!("Multi-TX aborted before PTT by shutdown"),
                }
                return ControlFlow::Break(TxExit::StopWorker);
            }
            match shape {
                TxShape::Single => {
                    info!("TX aborted before PTT engage by operator (F8)");
                    // This abort happens BEFORE the TxStatusGuard is
                    // constructed, so its Drop-based clear never runs. Clear
                    // the strip explicitly so the QUEUED row doesn't sit
                    // stale until the next status push.
                    send_tx_queue_status(message_bus, None, Vec::new()).await;
                }
                // The bundle arm has never cleared the strip here; keep it
                // that way (PAN-322 decision 9).
                TxShape::Bundle => info!("Multi-TX aborted before PTT by operator (F8)"),
            }
            // PAN-38 round 3 (Codex): an F8 abort here is the one path in
            // this worker that previously sent NO TransmitComplete at all --
            // for a self-CQ whose QSO was already opened (AutonomousCqOpened
            // registered it in the coordinator's pending_self_cq_qsos map),
            // that left the entry permanently leaked and the speculative
            // "+1" streak (and, for a bundle, offset) never rolled back,
            // since nothing ever told the autonomous operator this attempt
            // did not actually transmit. Report every frame (every bundle
            // item) exactly like the drop-stale-TX case.
            for (text, qso_id, attempt_id) in frames() {
                let reported =
                    report_transmit_complete(message_bus, false, text, 0, qso_id, attempt_id).await;
                // The single-frame arm discarded this result; the bundle arm
                // logged a failure.
                if let (TxShape::Bundle, Err(e)) = (shape, reported) {
                    warn!("Failed to send TransmitComplete: {}", e);
                }
            }
            ControlFlow::Break(TxExit::NextRequest)
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/on_ptt_wait_tests.rs"]
mod on_ptt_wait_tests;

/// PAN-114 (D6): slack over `ptt_lead_ms` allowed between PTT-on (Step 5)
/// and the slot boundary (Step 6) before the hold is treated as a clock step.
const PTT_HOLD_SLACK_MS: u64 = 1000;

/// `true` when Step 6 would hold PTT keyed (for `hold`, the Step 6
/// `to_slot`) longer than `ptt_lead_ms` + [`PTT_HOLD_SLACK_MS`]. Nothing
/// between Step 4 and Step 6 reassigns the schedule, so after a normal
/// Step 4 the hold is <= `ptt_lead_ms`; anything past the bound means the
/// wall clock stepped back in between.
fn ptt_hold_exceeds_bound(hold: Duration, ptt_lead_ms: u64) -> bool {
    hold > Duration::from_millis(ptt_lead_ms + PTT_HOLD_SLACK_MS)
}

/// PAN-114 (D7): drop frame(s) a wall-clock jump made unsendable. Mirrors
/// the neighbouring stale-TX drops: one `tx.policy` tracing line and Warn
/// diagnostic per frame (text starts "dropping stale TX" so it counts in the
/// TUI's TX-drops tally), clear the TX strip, then one failed
/// `TransmitComplete` per frame (via `report_transmit_complete`) so a
/// tracked self-CQ rolls back and the QSO engine reverses the frame's
/// keep-call accounting (PAN-75) while the attempt is still inside its
/// four-slot rollback window; a longer forward jump leaves it counted.
/// Callers that hold PTT must release it BEFORE calling this (it awaits).
async fn drop_frames_for_clock_jump(
    message_bus: &MessageBus,
    frames: &[(String, Option<String>, Option<pancetta_qso::TxAttemptId>)],
    jump: pancetta_core::slot_clock::WallClockJump,
    pivot_replaced: &mut PivotReplaced,
) {
    let secs = jump.signed_secs();
    for (message_text, qso_id, _) in frames {
        warn!(
            target: "pancetta::tx.policy",
            "dropping stale TX for QSO {} after a {secs:+.1} s clock jump: '{message_text}' — next frame uses the current UTC slot",
            qso_id.as_deref().unwrap_or("-")
        );
        emit_diagnostic(
            message_bus,
            "tx.policy",
            pancetta_core::DiagnosticLevel::Warn,
            format!(
                "dropping stale TX after a {secs:+.1} s clock jump: '{message_text}' — next frame uses the current UTC slot"
            ),
            qso_id.as_deref(),
        )
        .await;
    }
    send_tx_queue_status(message_bus, None, Vec::new()).await;
    for (message_text, qso_id, attempt_id) in frames {
        if let Err(e) = report_unaired_frame(
            message_bus,
            message_text.clone(),
            qso_id.clone(),
            *attempt_id,
            take_bundle_replaced(pivot_replaced, qso_id.as_deref()),
        )
        .await
        {
            warn!("Failed to send TransmitComplete: {}", e);
        }
    }
}

/// Messages the TX worker took out of its own `Ft8Transmitter` channel and
/// has not handled yet (PAN-73 follow-up, PR #346 round-4 finding). Held
/// here instead of being sent back through the bus: a re-send queued behind
/// any producer that sent in the meantime, and dropped silently when the
/// channel was full. Only the worker reads or writes it. The coordinator
/// owns it so a supervised worker restart (PAN-119) keeps it.
#[derive(Clone, Default)]
pub(crate) struct TxRequeue(
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ComponentMessage>>>,
);

/// Above this many held messages the worker logs a warning. Nothing is
/// dropped; real traffic holds a handful.
const TX_REQUEUE_HIGH_WATER: usize = 1024;

/// The TX worker's inbox: its [`TxRequeue`] front, then its channel.
/// Everything in the front was received before anything still in the
/// channel, so reading the front first keeps arrival order without any
/// synchronization with producers.
pub(crate) struct TxInbox {
    rx: crossbeam_channel::Receiver<ComponentMessage>,
    requeue: TxRequeue,
}

impl TxInbox {
    pub(crate) fn new(
        rx: crossbeam_channel::Receiver<ComponentMessage>,
        requeue: TxRequeue,
    ) -> Self {
        Self { rx, requeue }
    }

    fn front(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<ComponentMessage>> {
        // Only the worker locks this and it never panics while holding it;
        // the queue is still valid if a panic elsewhere poisoned it.
        self.requeue
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The oldest unhandled message: the front first, then the channel.
    pub(crate) fn try_recv(&self) -> Result<ComponentMessage, crossbeam_channel::TryRecvError> {
        let held = self.front().pop_front();
        match held {
            Some(message) => Ok(message),
            None => self.rx.try_recv(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.front().len() + self.rx.len()
    }

    /// Put `messages` back ahead of everything else, in the given order.
    /// Synchronous and infallible: no await, no bus, no capacity.
    pub(crate) fn requeue_front(&self, messages: Vec<ComponentMessage>) {
        if messages.is_empty() {
            return;
        }
        let mut front = self.front();
        let before = front.len();
        let mut restored = std::collections::VecDeque::from(messages);
        restored.append(&mut front);
        *front = restored;
        if before <= TX_REQUEUE_HIGH_WATER && front.len() > TX_REQUEUE_HIGH_WATER {
            warn!(
                target: "pancetta::tx.policy",
                "TX worker is holding {} unhandled messages",
                front.len()
            );
        }
    }
}

/// A message from the TX worker to itself, for [`TxInbox::requeue_front`].
fn self_message(message_type: MessageType) -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Ft8Transmitter,
        message_type,
        Instant::now(),
    )
}

#[cfg(test)]
#[path = "tx/tests/tx_inbox_tests.rs"]
mod tx_inbox_tests;

/// Outcome of `interruptible_sleep_or_supersede`.
///
/// Deliberately does NOT derive `PartialEq`/`Eq`: `Superseded` carries the
/// full `MessageType`, and `MessageType` itself does not (and should not,
/// without a much wider cross-crate change — see
/// docs/superpowers/sdd/task-5-report.md) derive `PartialEq`. Tests that
/// need to assert on a `Superseded` payload pattern-match and compare
/// individual fields instead of using `assert_eq!` on the whole enum.
// `Superseded(MessageType)` is intentionally not boxed: this keeps the
// public shape exactly as specified (a later task's re-key consumer wants
// the full `MessageType`, including `MultiTransmitRequest`'s `items` list —
// see task-5-report.md), at the cost of a size-difference lint between
// variants. Silenced rather than changed.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SleepOutcome {
    /// The full duration elapsed with no abort, shutdown, or supersede.
    Completed,
    AbortedByShutdown,
    /// F8 (or any other existing abort_current_tx setter) fired with no
    /// stashed replacement request.
    AbortedByOperator,
    /// PAN-92: a Remote in-flight frame's arm gate stopped permitting TX
    /// mid-transmission (explicit disarm, dead-man/heartbeat loss, TTL
    /// expiry, or local-kill) — the caller must stop keying, never re-key.
    /// Whatever the wait had already siphoned is back in the inbox front
    /// (a local splice, no await before the caller's PTT-off).
    AbortedByDisarm,
    /// A qualifying request arrived; abort_current_tx was set by this
    /// function itself. Caller should attempt to re-key with the contained
    /// message.
    ///
    /// Everything the wait siphoned is already back in the inbox front. A
    /// caller that enqueues supersede-derived content (a folded bundle)
    /// puts it at the front with [`TxInbox::requeue_front`], ahead of that
    /// leftover traffic (PAN-73 round 4).
    Superseded(MessageType),
}

/// Like `interruptible_sleep`, but also polls `inbox` for a qualifying
/// incoming request (Task 4's `classify_incoming_during_tx`) on every 50ms
/// tick. A qualifying request sets `abort` itself (mirroring the operator
/// F8 path) and is returned via `SleepOutcome::Superseded` for the caller to
/// re-key. A non-qualifying `TransmitRequest`/`MultiTransmitRequest` (exact
/// duplicate or pivot tombstone) is silently consumed — same as it would have
/// been had it reached the main dequeue loop naturally — and the sleep keeps
/// waiting. One for a QSO that isn't any of `in_flight_items` (PAN-73 round
/// 2) is siphoned and put back exactly like a non-candidate message
/// below, rather than either superseding or being discarded.
///
/// I1: a message that is NEITHER a `TransmitRequest` NOR a
/// `MultiTransmitRequest` (e.g. an operator `TuneRequest`) is NOT a supersede
/// candidate and must NOT be swallowed — before this feature it would sit in
/// the channel until the worker's next dequeue and be handled by its own
/// top-level arm. `crossbeam_channel::Receiver` has no peek, so we must consume
/// such a message to inspect its type; it is then SIPHONED aside and
/// put back (in arrival order) at the front of the worker's inbox when the
/// sleep ends, so the main loop processes it after the current TX arm returns.
///
/// See docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md,
/// "Abort + re-key mechanics."
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn interruptible_sleep_or_supersede(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    inbox: &TxInbox,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    pivoted_once: &std::collections::HashMap<String, (String, f64)>,
    // PAN-92: the in-flight frame's own origin/identity, re-checked against
    // the live arm on every poll tick (below) so an explicit disarm — or the
    // dead-man/heartbeat/TTL/local-kill gates ArmState already enforces —
    // interrupts a transmission ALREADY keyed, not just future ones. `None`
    // for a `TxOrigin::Local` in-flight frame, which is never gated.
    remote_tx_arm: Option<(
        &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
        Option<&str>,
    )>,
) -> SleepOutcome {
    interruptible_sleep_or_supersede_with_clock(
        total,
        shutdown,
        abort,
        inbox,
        in_flight_items,
        pivoted_once,
        remote_tx_arm,
        &pancetta_agent::arm::ArmTime::now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn interruptible_sleep_or_supersede_with_clock(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    inbox: &TxInbox,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    pivoted_once: &std::collections::HashMap<String, (String, f64)>,
    // PAN-92: the in-flight frame's own origin/identity, re-checked against
    // the live arm on every poll tick (below) so an explicit disarm — or the
    // dead-man/heartbeat/TTL/local-kill gates ArmState already enforces —
    // interrupts a transmission ALREADY keyed, not just future ones. `None`
    // for a `TxOrigin::Local` in-flight frame, which is never gated.
    remote_tx_arm: Option<(
        &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
        Option<&str>,
    )>,
    arm_clock: &(dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync),
) -> SleepOutcome {
    use std::sync::atomic::Ordering;

    // Non-supersede-candidate messages pulled off the channel during a poll
    // (I1). Held here rather than bounced straight back so a single try_recv
    // per tick can't keep re-reading the same one; put back at the inbox
    // front, in order, once the sleep concludes.
    let mut siphoned: Vec<ComponentMessage> = Vec::new();

    // PAN-73 round 7 (Codex): a requeued bundle can carry an item that's
    // already byte-identical (text + frequency) to its matching in-flight
    // counterpart alongside genuinely unrelated items (e.g. a closing 73
    // for the currently-keyed QSO, riding along with content for other
    // QSOs). Requeuing it verbatim risks that already-sent frame being
    // keyed AGAIN once the multi-TX arm's next natural dequeue picks it up
    // — the multi-TX arm bypasses single-request coalescing, so nothing
    // else catches this. Strip any such item before requeuing; if nothing
    // is left, drop the candidate entirely rather than requeuing an empty
    // bundle.
    let strip_already_sent_items =
        |message_type: MessageType,
         in_flight_items: &[crate::message_bus::TransmitRequestItem]|
         -> Option<MessageType> {
            let MessageType::MultiTransmitRequest {
                items,
                tx_parity,
                origin,
                remote_client_key_id,
            } = message_type
            else {
                return Some(message_type);
            };
            let filtered: Vec<_> = items
                .into_iter()
                .filter(|it| {
                    let key = it.qso_id.as_deref().map(super::active_tx_qso_key);
                    !in_flight_items.iter().any(|fi| {
                        fi.qso_id.as_deref().map(super::active_tx_qso_key) == key
                            && fi.message_text == it.message_text
                            && fi.frequency_offset == it.frequency_offset
                            // PAN-136 (Codex P2 on PR #456): a same-content item
                            // re-bound to a different authority was never sent
                            // under that authority — not "already sent".
                            && fi.origin == it.origin
                            && fi.remote_client_key_id == it.remote_client_key_id
                    })
                })
                .collect();
            if filtered.is_empty() {
                None
            } else {
                Some(MessageType::MultiTransmitRequest {
                    items: filtered,
                    tx_parity,
                    origin,
                    remote_client_key_id,
                })
            }
        };

    // PAN-73 round 7 (Codex): loop until the channel is genuinely empty (or
    // a terminal outcome fires), not just once per call. Returning after a
    // single Requeue/siphon left an already-queued, genuinely fresher
    // same-QSO request sitting untouched until the NEXT 50ms sleep-and-poll
    // tick — and if that tick happened to land past `deadline`, the wait
    // ended as `Completed` with the fresher request never even examined,
    // keying the stale in-flight frame for the whole slot. Draining
    // everything currently available before going back to the caller (and
    // ultimately to `sleep()`) closes that window.
    let check_once =
        |inbox: &TxInbox, siphoned: &mut Vec<ComponentMessage>| -> Option<SleepOutcome> {
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return Some(SleepOutcome::AbortedByShutdown);
                }
                if abort.load(Ordering::Acquire) {
                    return Some(SleepOutcome::AbortedByOperator);
                }
                // PAN-92: re-check the remote-TX arm gate on every tick for a
                // Remote in-flight frame — a disarm (explicit, dead-man,
                // heartbeat-lost, TTL-expired, or local-kill) must stop a
                // transmission ALREADY keyed, not just block the next one.
                if let Some((arm, client_key_id)) = remote_tx_arm {
                    if !remote_tx_permitted_for(arm, arm_clock, client_key_id) {
                        return Some(SleepOutcome::AbortedByDisarm);
                    }
                }
                let Ok(message) = inbox.try_recv() else {
                    return None;
                };
                let is_supersede_candidate = matches!(
                    message.message_type,
                    MessageType::TransmitRequest { .. } | MessageType::MultiTransmitRequest { .. }
                );
                if is_supersede_candidate {
                    // The in-flight side's own authorization: `remote_tx_arm`
                    // being `Some` means the in-flight frame is Remote (with
                    // this client_key_id); `None` means Local (see this
                    // function's own param doc above).
                    let (in_flight_origin, in_flight_remote_client_key_id) = match remote_tx_arm {
                        Some((_, client_key_id)) => {
                            (crate::message_bus::TxOrigin::Remote, client_key_id)
                        }
                        None => (crate::message_bus::TxOrigin::Local, None),
                    };
                    match classify_incoming_during_tx(
                        &message.message_type,
                        in_flight_items,
                        pivoted_once,
                        in_flight_origin,
                        in_flight_remote_client_key_id,
                    ) {
                        // Silent to Autonomous: the keyed pivoted frame owns the
                        // QSO's outcome (ownership rule: `report_unaired_frame`,
                        // PAN-185).
                        IncomingDuringTx::Drop => {}
                        // PAN-73 round 2: not a match for any in-flight QSO —
                        // requeue rather than discard (see IncomingDuringTx::Requeue).
                        IncomingDuringTx::Requeue => {
                            if let Some(message_type) =
                                strip_already_sent_items(message.message_type, in_flight_items)
                            {
                                siphoned.push(ComponentMessage {
                                    message_type,
                                    ..message
                                });
                            }
                        }
                        IncomingDuringTx::Supersede { .. } => {
                            abort.store(true, Ordering::Release);
                            return Some(SleepOutcome::Superseded(message.message_type));
                        }
                    }
                } else {
                    // Not a supersede candidate (e.g. TuneRequest). Siphon it so
                    // it is put back after the sleep instead of being lost.
                    siphoned.push(message);
                }
            }
        };

    let outcome = 'sleep: {
        if let Some(outcome) = check_once(inbox, &mut siphoned) {
            break 'sleep outcome;
        }

        let chunk = Duration::from_millis(50);
        let deadline = tokio::time::Instant::now() + total;
        while tokio::time::Instant::now() < deadline {
            if let Some(outcome) = check_once(inbox, &mut siphoned) {
                break 'sleep outcome;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            sleep(remaining.min(chunk)).await;
        }
        SleepOutcome::Completed
    };

    // Put the siphoned messages back at the inbox front for every outcome.
    // They were received before anything still in the channel, so the front
    // keeps true arrival order without draining the channel (PAN-73 round 3)
    // and without a bus re-send that a concurrent producer could overtake or
    // a full channel could drop (PAN-73 follow-up). The splice is
    // synchronous and bounded by what this wait already holds, so it adds
    // no await and no channel drain ahead of a disarm's PTT-off (rounds 2
    // and 9). A `Superseded` caller puts its own content in front of these.
    inbox.requeue_front(siphoned);
    outcome
}

/// How a shared TX-worker sleep step ends early (PAN-322): leave by a
/// [`TxExit`], or hand a superseding request back to the call site's own
/// supersede handler.
/// The request is boxed so the enum stays small (`MessageType` is large).
enum SleepBreak {
    Exit(TxExit),
    Superseded(Box<MessageType>),
}

/// What [`on_sleep_outcome`] returns.
type SleepFlow = std::ops::ControlFlow<SleepBreak>;

/// Which of the two pre-air waits a [`SleepOutcome`] came from (PAN-322).
#[derive(Clone, Copy, Debug)]
enum SleepPhase {
    /// Step 6: PTT is keyed, the slot has not started, no audio was routed.
    PreSlot,
    /// Step 8: audio is playing; `playback_end` bounds an unconfirmed disarm
    /// flush (PAN-135).
    Playback { playback_end: tokio::time::Instant },
}

/// The frames a sleep step was guarding (PAN-322). The single arm's frame is
/// borrowed field by field, so the sleep call still builds its own
/// in-flight item exactly as before.
enum SleepInFlight<'a> {
    Single {
        message_text: &'a str,
        frequency_offset: f64,
        qso_id: Option<&'a str>,
        attempt_id: Option<pancetta_qso::TxAttemptId>,
        pivoted_this_key: &'a mut Option<String>,
    },
    Bundle {
        items: &'a [crate::message_bus::TransmitRequestItem],
        pivoted_this_bundle_keys: &'a [String],
    },
}

impl SleepInFlight<'_> {
    fn shape(&self) -> TxShape {
        match self {
            SleepInFlight::Single { .. } => TxShape::Single,
            SleepInFlight::Bundle { .. } => TxShape::Bundle,
        }
    }

    /// `(text, qso_id, attempt_id)` per frame, in the arm's order.
    fn frames(&self) -> Vec<(String, Option<String>, Option<pancetta_qso::TxAttemptId>)> {
        match self {
            SleepInFlight::Single {
                message_text,
                qso_id,
                attempt_id,
                ..
            } => vec![(
                (*message_text).to_string(),
                qso_id.map(str::to_owned),
                *attempt_id,
            )],
            SleepInFlight::Bundle { items, .. } => items
                .iter()
                .map(|item| {
                    (
                        item.message_text.clone(),
                        item.qso_id.clone(),
                        item.attempt_id,
                    )
                })
                .collect(),
        }
    }
}

/// The worker state an aborted sleep step touches (PAN-322).
struct AbortCtx<'a> {
    message_bus: &'a MessageBus,
    audit_log: &'a pancetta_agent::audit::AuditLog,
    display_feed_enabled: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    remote_tx_arm: &'a std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&'a str>,
    ptt_guard: &'a mut PttGuard,
    pivoted_once: &'a mut std::collections::HashMap<String, (String, f64)>,
    pivoted_from: &'a mut PivotReplaced,
    shutdown: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    sample_rate: u32,
}

/// Why a sleep step was cut short, for [`sleep_abort_log_line`].
#[derive(Clone, Copy, Debug)]
enum SleepAbort {
    Shutdown,
    Operator,
    Disarm,
}

/// The log line for an aborted Step 6/8 sleep (PAN-322). These strings are
/// what operators and log scrapers have always seen; keep them byte-identical.
fn sleep_abort_log_line(shape: TxShape, phase: SleepPhase, abort: SleepAbort) -> &'static str {
    match (shape, phase, abort) {
        (TxShape::Single, SleepPhase::PreSlot, SleepAbort::Shutdown) => {
            "TX aborted between PTT and slot by shutdown"
        }
        (TxShape::Single, SleepPhase::PreSlot, SleepAbort::Operator) => {
            "TX aborted between PTT and slot by operator (F8)"
        }
        (TxShape::Single, SleepPhase::PreSlot, SleepAbort::Disarm) => {
            "TX aborted between PTT and slot: remote arm no longer permits it"
        }
        (TxShape::Single, SleepPhase::Playback { .. }, SleepAbort::Shutdown) => {
            "TX aborted during playback by shutdown"
        }
        (TxShape::Single, SleepPhase::Playback { .. }, SleepAbort::Operator) => {
            "TX aborted during playback by operator (F8)"
        }
        (TxShape::Single, SleepPhase::Playback { .. }, SleepAbort::Disarm) => {
            "TX aborted during playback: remote arm no longer permits it"
        }
        (TxShape::Bundle, SleepPhase::PreSlot, SleepAbort::Shutdown) => {
            "Multi-TX aborted between PTT and slot by shutdown"
        }
        (TxShape::Bundle, SleepPhase::PreSlot, SleepAbort::Operator) => {
            "Multi-TX aborted between PTT and slot by operator (F8)"
        }
        (TxShape::Bundle, SleepPhase::PreSlot, SleepAbort::Disarm) => {
            "Multi-TX aborted between PTT and slot: remote arm no longer permits it"
        }
        (TxShape::Bundle, SleepPhase::Playback { .. }, SleepAbort::Shutdown) => {
            "Multi-TX aborted during playback by shutdown"
        }
        (TxShape::Bundle, SleepPhase::Playback { .. }, SleepAbort::Operator) => {
            "Multi-TX aborted during playback by operator (F8)"
        }
        (TxShape::Bundle, SleepPhase::Playback { .. }, SleepAbort::Disarm) => {
            "Multi-TX aborted during playback: remote arm no longer permits it"
        }
    }
}

/// The remote-arm gate a sleep re-checks every tick (PAN-92): the live arm
/// and the frame's key for a `TxOrigin::Remote` frame, `None` for a local
/// frame, which is never gated.
fn remote_arm_gate<'a>(
    origin: crate::message_bus::TxOrigin,
    remote_tx_arm: &'a std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&'a str>,
) -> Option<(
    &'a std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    Option<&'a str>,
)> {
    if origin == crate::message_bus::TxOrigin::Remote {
        Some((remote_tx_arm, remote_client_key_id))
    } else {
        None
    }
}

/// Steps 6 and 8 of both worker arms (PAN-322): act on an
/// [`interruptible_sleep_or_supersede_with_clock`] outcome. `Completed`
/// continues. Shutdown only logs and stops the worker; the call site's
/// `PttGuard` drop unkeys, as before. An F8 abort reports every frame as a
/// failed `TransmitComplete`. A disarm releases PTT first, then settles every
/// frame (see the arm). `Superseded` is handed back unchanged for the call
/// site's supersede handler.
async fn on_sleep_outcome(
    outcome: SleepOutcome,
    phase: SleepPhase,
    mut in_flight: SleepInFlight<'_>,
    ctx: AbortCtx<'_>,
) -> SleepFlow {
    use std::ops::ControlFlow;
    let shape = in_flight.shape();
    match outcome {
        SleepOutcome::Completed => ControlFlow::Continue(()),
        SleepOutcome::AbortedByShutdown => {
            info!(
                "{}",
                sleep_abort_log_line(shape, phase, SleepAbort::Shutdown)
            );
            ControlFlow::Break(SleepBreak::Exit(TxExit::StopWorker))
        }
        SleepOutcome::AbortedByOperator => {
            info!(
                "{}",
                sleep_abort_log_line(shape, phase, SleepAbort::Operator)
            );
            // PAN-38 round 4 (Codex): report the failed completion here too,
            // for every frame (every bundle item) -- see `on_ptt_wait`'s F8
            // abort for the full leak this closes.
            for (text, qso_id, attempt_id) in in_flight.frames() {
                if let Err(e) =
                    report_transmit_complete(ctx.message_bus, false, text, 0, qso_id, attempt_id)
                        .await
                {
                    warn!("Failed to send TransmitComplete: {}", e);
                }
            }
            ControlFlow::Break(SleepBreak::Exit(TxExit::NextRequest))
        }
        SleepOutcome::AbortedByDisarm => {
            warn!("{}", sleep_abort_log_line(shape, phase, SleepAbort::Disarm));
            // PAN-91 review: release PTT FIRST, before any of this helper's
            // awaits (diagnostic, audit-log I/O, relay) — a disarmed
            // transmission must not stay keyed a moment longer than necessary.
            // PAN-135: stand the guard down only after Hamlib accepts PTT-off.
            ctx.ptt_guard.release("Remote disarm mid-TX").await;
            let mut single_replaced: Option<ReplacedFrame> = None;
            let mut bundle_replaced = PivotReplaced::new();
            match phase {
                // Round-7 review (Codex P2): this is a PRE-SLOT abort — PTT
                // was keyed but Step 7 never routed any audio, so nothing
                // actually reached the air. If Step 4c (single) or Step 4b
                // (bundle) pivoted this cycle, its tombstone must go too,
                // exactly like the pre-PTT Step 4d-arm denial — else the
                // genuinely fresh, still-queued request is wrongly discarded
                // as an already-sent pivot duplicate.
                SleepPhase::PreSlot => match &mut in_flight {
                    SleepInFlight::Single {
                        pivoted_this_key, ..
                    } => {
                        single_replaced = pivoted_this_key.take().and_then(|key| {
                            ctx.pivoted_once.remove(&key);
                            ctx.pivoted_from.remove(&key)
                        });
                    }
                    SleepInFlight::Bundle {
                        pivoted_this_bundle_keys,
                        ..
                    } => {
                        bundle_replaced = unwind_bundle_pivots(
                            pivoted_this_bundle_keys,
                            ctx.pivoted_once,
                            ctx.pivoted_from,
                        );
                    }
                },
                // Round-4 review (Codex P1): flush the already-queued remote
                // waveform before the worker resumes pending work — a pending
                // local TuneRequest or a fresh TX both queue with
                // `flush_first: false`, so without this the disarmed client's
                // remaining audio could still reach the air riding behind the
                // next request's samples. PAN-135 waits for the audio thread
                // to confirm the flush before proceeding.
                SleepPhase::Playback { playback_end } => {
                    flush_disarmed_audio_confirmed(
                        ctx.message_bus,
                        ctx.sample_rate,
                        playback_end,
                        ctx.shutdown,
                    )
                    .await;
                }
            }
            let operator_callsign =
                tx_denied_operator_attribution(ctx.remote_tx_arm, ctx.remote_client_key_id);
            let (detail, signal_qso_id) = match &in_flight {
                SleepInFlight::Single {
                    message_text,
                    frequency_offset,
                    qso_id,
                    ..
                } => (
                    format!(
                        "disarmed mid-transmission: '{message_text}' at {frequency_offset:.0} Hz"
                    ),
                    *qso_id,
                ),
                SleepInFlight::Bundle { .. } => (
                    "disarmed mid-transmission: '<multi-TX bundle>' at 0 Hz".to_string(),
                    None,
                ),
            };
            emit_disarm_interrupt_signals(
                ctx.message_bus,
                ctx.audit_log,
                ctx.display_feed_enabled,
                operator_callsign,
                "interrupted",
                detail,
                signal_qso_id,
            )
            .await;
            match phase {
                // Nothing aired: settle through `report_unaired_frame` so a
                // pivot's replaced frame rolls back.
                SleepPhase::PreSlot => match &in_flight {
                    SleepInFlight::Single {
                        message_text,
                        qso_id,
                        attempt_id,
                        ..
                    } => {
                        if let Err(e) = report_unaired_frame(
                            ctx.message_bus,
                            (*message_text).to_string(),
                            qso_id.map(str::to_owned),
                            *attempt_id,
                            single_replaced,
                        )
                        .await
                        {
                            warn!("Failed to send TransmitComplete: {}", e);
                        }
                    }
                    SleepInFlight::Bundle { items, .. } => {
                        for item in items.iter() {
                            if let Err(e) = report_unaired_frame(
                                ctx.message_bus,
                                item.message_text.clone(),
                                item.qso_id.clone(),
                                item.attempt_id,
                                take_bundle_replaced(&mut bundle_replaced, item.qso_id.as_deref()),
                            )
                            .await
                            {
                                warn!("Failed to send TransmitComplete: {}", e);
                            }
                        }
                    }
                },
                SleepPhase::Playback { .. } => {
                    for (text, qso_id, attempt_id) in in_flight.frames() {
                        if let Err(e) = report_transmit_complete(
                            ctx.message_bus,
                            false,
                            text,
                            0,
                            qso_id,
                            attempt_id,
                        )
                        .await
                        {
                            warn!("Failed to send TransmitComplete: {}", e);
                        }
                    }
                }
            }
            ControlFlow::Break(SleepBreak::Exit(TxExit::NextRequest))
        }
        SleepOutcome::Superseded(new_request) => {
            ControlFlow::Break(SleepBreak::Superseded(Box::new(new_request)))
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/on_sleep_outcome_tests.rs"]
mod on_sleep_outcome_tests;

#[cfg(test)]
#[path = "tx/tests/interruptible_sleep_tests.rs"]
mod interruptible_sleep_tests;

/// PAN-135: bounded retry for a safety send that must not be silently
/// dropped. Uses the PAN-43/45 schedule: 4 attempts over about 75 ms.
pub(super) const SAFETY_SEND_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(5),
    Duration::from_millis(20),
    Duration::from_millis(50),
];

fn ptt_off_message() -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    )
}

/// What [`PttGuard::release`] achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttRelease {
    /// PTT-off is enqueued on Hamlib's channel: handed off.
    Delivered,
    /// Every attempt dropped; the guard stays armed.
    Undelivered,
    /// PAN-273: no Hamlib channel is registered (`--replay`), so no PTT-on
    /// was ever enqueued; the guard stood down without sending.
    NoRig,
}

/// PAN-135: the shortest a disarm flush waits for its ack, and how far
/// past the revoked waveform's scheduled end an unconfirmed flush waits
/// (relay poll + audio-thread latency between Step 7's enqueue and the
/// samples reaching the output ring).
const FLUSH_ACK_MIN_WAIT: Duration = Duration::from_millis(250);
const FLUSH_PLAYBACK_SLACK: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisarmFlush {
    /// The audio thread confirmed the flush reached the output ring.
    Applied,
    /// No confirmation: waited until the revoked audio had played out.
    WaitedOutPlayback,
    /// Shutdown ended the wait.
    Shutdown,
}

/// PAN-135 (PR #362 rounds 4/6, Codex P1): flush the revoked remote
/// waveform before the worker resumes held or queued work that may key
/// again. Wait for the audio thread to confirm the flush, the revoked
/// playback window to elapse, or shutdown. The playback bound applies
/// when confirmation cannot arrive, including `--no-audio` and a stalled
/// audio thread. The flush rides the same Audio FIFO as the frame's samples,
/// so it can never clear the buffer ahead of them.
async fn flush_disarmed_audio_confirmed(
    message_bus: &MessageBus,
    sample_rate: u32,
    playback_end: tokio::time::Instant,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> DisarmFlush {
    let deadline = std::cmp::max(
        playback_end + FLUSH_PLAYBACK_SLACK,
        tokio::time::Instant::now() + FLUSH_ACK_MIN_WAIT,
    );
    'attempts: for attempt in 0..=SAFETY_SEND_RETRY_DELAYS.len() {
        let (ack, mut waiter) = DeliveryAck::new();
        let flush = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Audio,
            MessageType::AudioFlush { sample_rate, ack },
            Instant::now(),
        );
        if matches!(message_bus.send_message_checked(flush).await, Ok(true)) {
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return DisarmFlush::Shutdown;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break 'attempts;
                }
                match waiter
                    .wait_for((deadline - now).min(Duration::from_millis(50)))
                    .await
                {
                    AckState::Applied => return DisarmFlush::Applied,
                    AckState::Pending => {}
                    AckState::NotApplied => break 'attempts,
                    AckState::Dropped => break, // lost in transit: send a fresh one
                }
            }
        }
        if let Some(delay) = SAFETY_SEND_RETRY_DELAYS.get(attempt) {
            sleep(*delay).await;
        }
    }
    error!(
        target: "pancetta::tx.ptt",
        "Remote disarm: audio flush not confirmed by the audio thread; holding the next request for {:?} until the revoked waveform has played out",
        deadline.saturating_duration_since(tokio::time::Instant::now())
    );
    while tokio::time::Instant::now() < deadline {
        if shutdown.load(Ordering::Acquire) {
            return DisarmFlush::Shutdown;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        sleep(remaining.min(Duration::from_millis(50))).await;
    }
    DisarmFlush::WaitedOutPlayback
}

/// PAN-273 (Codex P2, PR #587): the retrying PTT-off an armed
/// [`PttGuard`]'s `Drop` spawned, while it may still be outstanding.
/// Coordinator-owned, so a restarted TX worker shares it. [`PttGuard::new`]
/// waits for it before reserving the next key-down, so a fallback unkey can
/// never reach Hamlib's channel after a later PTT-on from the TX worker
/// (for example the re-key a supersede starts right after an undelivered
/// release).
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingPttOff(
    std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
);

impl PendingPttOff {
    /// Record a fallback PTT-off so the next [`Self::settle`] waits for it
    /// and for any fallback already recorded. Both send independently.
    fn push(&self, unkey: tokio::task::JoinHandle<()>) {
        let mut slot = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(match slot.take() {
            None => unkey,
            Some(earlier) => tokio::task::spawn(async move {
                let _ = earlier.await;
                let _ = unkey.await;
            }),
        });
    }

    /// Wait until every fallback PTT-off recorded so far has finished
    /// (delivered or given up after its bounded retries). If this wait is
    /// cancelled (worker abort or restart), the unfinished handle goes back
    /// in the slot, so the next key-down still waits for it.
    /// Shutdown also awaits this before stopping the Hamlib drain (PAN-277).
    pub(crate) async fn settle(&self) {
        struct Waiting<'a> {
            slot: &'a PendingPttOff,
            unkey: Option<tokio::task::JoinHandle<()>>,
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                if let Some(unkey) = self.unkey.take() {
                    self.slot.push(unkey);
                }
            }
        }
        let outstanding = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut waiting = Waiting {
            slot: self,
            unkey: outstanding,
        };
        if let Some(unkey) = waiting.unkey.as_mut() {
            let _ = unkey.await;
        }
        waiting.unkey = None;
    }
}

/// Guard that sends PTT-off when dropped, ensuring PTT is released
/// even if the transmitter task is cancelled mid-transmission.
struct PttGuard {
    message_bus: MessageBus,
    armed: bool,
    /// Mirrors the keyed state for the SWR poll / TUI. Set true on construct,
    /// cleared on drop (RAII — clears on every exit path incl. abort/panic).
    ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_ptt_off_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Where `Drop` records its fallback PTT-off for the next `new` to await.
    pending_ptt_off: PendingPttOff,
}

impl PttGuard {
    /// `ptt_sync_gate` (PAN-143) makes the `ptt_active` store below mutually
    /// exclusive with `pancetta_qso::QsoManager::apply_tx_offset_switch`'s
    /// recheck-through-publish span — closing, not just narrowing, the race
    /// PAN-140 round 2 found (and round 3's finding that releasing too early
    /// reopens it further down the same function). See that gate's doc
    /// comment and `docs/superpowers/specs/
    /// 2026-09-12-pan-143-ptt-shared-synchronization-design.md`. An
    /// async-aware `tokio::sync::Mutex` — round 3 requires the QSO-manager
    /// side to hold the gate across its own `.await` event-emission points,
    /// which a blocking `std::sync::Mutex` cannot safely do — so this
    /// constructor is `async` accordingly; the wait here is expected to be
    /// negligible (the QSO-manager side never does I/O or hardware calls
    /// while holding it, only channel sends).
    ///
    /// PAN-273 (Codex P2, PR #587): first waits for any fallback PTT-off a
    /// previous armed guard's `Drop` left outstanding (at most its bounded
    /// retry schedule), so that unkey is serialized ahead of this guard's
    /// key-down. The TX-worker arms already settle before their audio
    /// refresh (single, multi) or hard-mute check (tune), so this wait is
    /// normally a no-op backstop.
    async fn new(
        message_bus: MessageBus,
        ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ptt_sync_gate: &std::sync::Arc<tokio::sync::Mutex<()>>,
        last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        last_ptt_off_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        pending_ptt_off: &PendingPttOff,
    ) -> Self {
        pending_ptt_off.settle().await;
        {
            let _ptt_sync = ptt_sync_gate.lock().await;
            // Reserve the relocation interlock even if gain currently blocks
            // hardware key-down. Gain may recover during status publication;
            // send_ptt_command still refuses key-down while gain is blocked.
            ptt_active.store(true, std::sync::atomic::Ordering::Release);
        }
        last_ptt_on_ms.store(super::now_epoch_ms(), std::sync::atomic::Ordering::Release);
        Self {
            message_bus,
            armed: true,
            ptt_active,
            last_ptt_off_ms: last_ptt_off_ms.clone(),
            pending_ptt_off: pending_ptt_off.clone(),
        }
    }

    /// PAN-135: unkey with checked, bounded-retry delivery, and stand the
    /// RAII fallback down only once PTT-off is enqueued on Hamlib's channel.
    /// Hamlib applies that channel in order, so any later PTT-on from this
    /// worker lands after this unkey. An enqueued PTT-off is handed off.
    /// If every attempt drops, the guard stays armed (its `Drop` retries on
    /// scope exit, and the next [`Self::new`] waits for that retry before
    /// keying) and `ptt_active` stays true until then; Hamlib's 14 s
    /// PTT watchdog is the remaining backstop.
    ///
    /// PAN-273: every TX-worker unkey after a key-down goes through here
    /// (Step 9, multi-TX, tune, the clock-step hold guard and supersede),
    /// never through a best-effort `send_message` followed by `disarm()`.
    /// With no Hamlib channel registered nothing was keyed, so the guard
    /// stands down at once and returns [`PttRelease::NoRig`].
    async fn release(&mut self, context: &str) -> PttRelease {
        if !self.message_bus.has_rig_route() {
            debug!(
                target: "pancetta::tx.ptt",
                "{context}: no Hamlib channel registered; nothing to unkey"
            );
            self.stand_down();
            return PttRelease::NoRig;
        }
        match self
            .message_bus
            .send_message_retrying(ptt_off_message, &SAFETY_SEND_RETRY_DELAYS)
            .await
        {
            Ok(true) => {
                self.stand_down();
                PttRelease::Delivered
            }
            Ok(false) | Err(_) => {
                error!(
                    target: "pancetta::tx.ptt",
                    "{context}: PTT OFF not delivered to Hamlib after {} attempts; PTT guard stays armed and retries on drop; Hamlib's PTT watchdog is the backstop",
                    SAFETY_SEND_RETRY_DELAYS.len() + 1
                );
                PttRelease::Undelivered
            }
        }
    }

    /// The unkey is handed off (or nothing was keyed): stamp the PTT-off
    /// time, clear the keyed flag, and stand the `Drop` fallback down.
    fn stand_down(&mut self) {
        self.last_ptt_off_ms
            .store(super::now_epoch_ms(), Ordering::Release);
        self.ptt_active
            .store(false, std::sync::atomic::Ordering::Release);
        self.armed = false;
    }

    /// Stand the guard down when no PTT-on was enqueued (before the key-down
    /// send, or after a `Denied` one). Every unkey after a key-down uses
    /// [`Self::release`] (PAN-273).
    fn disarm(&mut self) {
        self.armed = false;
    }
}

/// Observer-only RAII guard for the TUI's TX-active badge (Batch 93).
///
/// Constructed right after `PttGuard` when PTT is asserted; its `Drop`
/// sends `MessageType::TxStatus { active: false }` to the TUI relay.
/// Because every exit from a TX arm — normal completion, operator abort
/// (F8 / Shift+Q `continue`), or shutdown `break` — drops the guard,
/// the badge clears on abort paths too, not just clean completion.
///
/// Strictly observational: it never touches PTT, audio, or scheduling.
/// The corresponding `active: true` is sent explicitly via
/// `send_tx_status` at PTT assert (async context is available there;
/// `Drop` is not async, hence the spawned fire-and-forget task).
struct TxStatusGuard {
    message_bus: MessageBus,
}

impl TxStatusGuard {
    fn new(message_bus: MessageBus) -> Self {
        Self { message_bus }
    }
}

impl Drop for TxStatusGuard {
    fn drop(&mut self) {
        let bus = self.message_bus.clone();
        // rationale: intentional fire-and-forget detach — `spawn` runs the task
        // independently; the dropped JoinHandle is the canonical detach idiom.
        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn(async move {
            let msg = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::TxStatus { active: false },
                Instant::now(),
            );
            if let Err(e) = bus.send_message(msg).await {
                tracing::debug!("TxStatus(false) relay failed (no TUI?): {}", e);
            }
            // Also clear the richer NOW-SENDING / QUEUED view so every
            // exit path (complete / abort / shutdown) returns the TX
            // panel to idle, mirroring the boolean badge.
            let idle = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::TxQueueStatus {
                    sending: None,
                    queued: Vec::new(),
                },
                Instant::now(),
            );
            if let Err(e) = bus.send_message(idle).await {
                tracing::debug!("TxQueueStatus(idle) relay failed (no TUI?): {}", e);
            }
        });
    }
}

/// Notify the TUI of TX activity. Best-effort: failure (e.g. headless,
/// no TUI channel) is logged at debug and never affects the TX path.
async fn send_tx_status(message_bus: &MessageBus, active: bool) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxStatus { active },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("TxStatus({}) relay failed (no TUI?): {}", active, e);
    }
}

/// PAN-71: bounded playback confirmation before the existing TX-log wait.
/// Step 8 still times playback from enqueue, so this does not extend key-down.
pub(crate) const TX_PLAYBACK_START_WAIT: Duration = Duration::from_millis(100);

/// Use the audio thread's playback-start report, falling back to the enqueue
/// instant if confirmation is unavailable. Called only after authorized enqueue.
async fn frame_log_instant(
    waiter: &mut PlaybackStartWaiter,
    enqueued_at: chrono::DateTime<chrono::Utc>,
    text: &str,
) -> chrono::DateTime<chrono::Utc> {
    match waiter.wait_for(TX_PLAYBACK_START_WAIT).await {
        PlaybackStartState::Started(at) => at,
        PlaybackStartState::Pending => {
            warn!("TX audio start not confirmed within {} ms for '{text}'; logging the frame at its enqueue time", TX_PLAYBACK_START_WAIT.as_millis());
            enqueued_at
        }
        PlaybackStartState::NotStarted => {
            warn!("TX audio for '{text}' was not played (queue error, flushed, or output stream replaced); logging the frame at its enqueue time");
            enqueued_at
        }
        PlaybackStartState::Dropped => {
            debug!("TX audio playback-start ack dropped for '{text}' (no audio thread); logging the frame at its enqueue time");
            enqueued_at
        }
    }
}

/// Log one actually-keyed frame for Band Activity's own-TX history (#172).
/// The caller supplies the playback-start instant from `frame_log_instant`,
/// or its enqueue-time fallback, also used by the durable TX-log record.
async fn log_tx_frame(
    message_bus: &MessageBus,
    text: String,
    freq_hz: f64,
    qso_id: Option<String>,
    timestamp: chrono::DateTime<chrono::Utc>,
) {
    let log_msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxFrameLogged {
            text,
            freq_hz,
            qso_id,
            timestamp,
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(log_msg).await {
        tracing::debug!("TxFrameLogged relay failed (no TUI?): {}", e);
    }
}

/// PAN-122: the effective (split-aware) TX dial for a TX-log record; 0 = unknown.
fn tx_log_dial_hz(
    operating: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    split: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> u64 {
    pancetta_qso::qso_manager::effective_tx_dial(
        operating.load(Ordering::Relaxed),
        split.load(Ordering::Relaxed),
    )
}

/// Push a richer TX-queue snapshot (NOW-SENDING + QUEUED) to the TUI.
/// Best-effort, observation-only: never touches PTT/audio/scheduling.
///
/// Does NOT emit `TxFrameLogged`: that fires separately at Step 7 after
/// playback confirmation or its bounded fallback (see [`log_tx_frame`]). This
/// function's "NOW-SENDING" snapshot legitimately reflects Step 5 (PTT
/// asserted now); Band Activity's history needs the later, more accurate
/// timestamp instead.
async fn send_tx_queue_status(
    message_bus: &MessageBus,
    sending: Option<crate::message_bus::TxItem>,
    queued: Vec<crate::message_bus::TxItem>,
) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxQueueStatus { sending, queued },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("TxQueueStatus relay failed (no TUI?): {}", e);
    }
}

/// Report every TX outcome to Autonomous, preserving its self-CQ correlation.
/// PAN-75 also mirrors failed QSO frames to the QSO engine, which reverses
/// their keep-call accounting. Covers cap truncation, F8, supersede, denial,
/// hard mute, PAN-141 hold, PAN-114 clock-jump drops, and encode/PTT failure
/// through the same path.
/// Shutdown branches deliberately send nothing: the QSO loop stops on the
/// same flag and its in-memory accounting is discarded at exit.
/// `attempt_id` is the frame's own ledger attempt (PAN-183), carried by both
/// halves (PAN-279). Delegates to [`super::tx_outcome`], which sends both
/// halves on the durable tier (PAN-185); always `Ok(())`.
async fn report_transmit_complete(
    message_bus: &MessageBus,
    success: bool,
    message_text: String,
    duration_ms: u64,
    qso_id: Option<String>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> Result<()> {
    let frame = TxFrame {
        qso_id,
        message_text,
        attempt_id,
    };
    if success {
        tx_outcome::settle_aired_frame(message_bus, frame, duration_ms, None).await;
    } else {
        settle_unaired(message_bus, frame, None, AutonomousOutcome::Report).await;
    }
    Ok(())
}

/// [`tx_outcome::settle_unaired_frame`] from the TX worker.
async fn settle_unaired(
    message_bus: &MessageBus,
    frame: TxFrame,
    replaced: Option<pancetta_qso::UnsentAttemptPayload>,
    autonomous: AutonomousOutcome,
) {
    tx_outcome::settle_unaired_frame(
        message_bus,
        ComponentId::Ft8Transmitter,
        frame,
        replaced,
        autonomous,
    )
    .await;
}

/// Report a frame after PTT is released — one whose audio played out
/// (`success` from a Step-7 `Enqueued`), or one whose audio delivery
/// failed (`Dropped`/error: no samples were ever enqueued, so the frame
/// never aired and routes through the PAN-75 not-sent path). On success it
/// also tells the QSO engine the frame aired, with the offset and binding
/// it aired on, so a stall offset action held on that frame can commit
/// (PAN-182). Never called before or during key-down (PAN-202).
async fn report_played_transmit_complete(
    message_bus: &MessageBus,
    success: bool,
    message_text: String,
    duration_ms: u64,
    qso_id: Option<String>,
    aired: pancetta_qso::UnsentAttemptPayload,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> Result<()> {
    let frame = TxFrame {
        qso_id,
        message_text,
        attempt_id,
    };
    if success {
        tx_outcome::settle_aired_frame(message_bus, frame, duration_ms, Some(aired)).await;
    } else {
        settle_unaired(message_bus, frame, None, AutonomousOutcome::Report).await;
    }
    Ok(())
}

/// PAN-223: tell the QSO engine which slot this frame was scheduled into.
/// Takes the `TxSchedule` itself so the reported slot cannot differ from
/// the one the worker waits for. Durable (PAN-185); its only await is the
/// channel table's read lock, and callers send it before Step 4's PTT wait.
async fn report_frame_scheduled(
    message_bus: &MessageBus,
    qso_id: Option<&str>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
    schedule: &TxSchedule,
) {
    let (Some(qso_id), Some(attempt_id)) = (qso_id, attempt_id) else {
        return;
    };
    message_bus
        .send_message_durable(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameScheduled {
                qso_id: qso_id.to_string(),
                attempt_id,
                target_slot: schedule.target_slot,
            }),
            Instant::now(),
        ))
        .await;
}

/// The offset and binding a transmitted frame carried (PAN-182).
fn aired_payload(
    frequency_hz: f64,
    origin: crate::message_bus::TxOrigin,
    remote_client_key_id: Option<String>,
) -> pancetta_qso::UnsentAttemptPayload {
    pancetta_qso::UnsentAttemptPayload {
        frequency_hz,
        remote_binding: Some((
            origin == crate::message_bus::TxOrigin::Remote,
            remote_client_key_id,
        )),
    }
}

/// A frame a pivot replaced: its text, its payload (PAN-75) and its ledger
/// attempt (PAN-183).
type ReplacedFrame = (
    String,
    pancetta_qso::UnsentAttemptPayload,
    Option<pancetta_qso::TxAttemptId>,
);

/// Pivot sources cleared from a refused frame or bundle, keyed by
/// `active_tx_qso_key` (PAN-75).
type PivotReplaced = std::collections::HashMap<String, ReplacedFrame>;

/// A bundle refused before it aired: clear the tombstones its Step 4b pivot
/// inserted and return the frames those pivots replaced, so each item's
/// failure rolls back the replaced frame (see [`report_unaired_frame`]).
/// Clearing a tombstone hands the QSO's Autonomous outcome to the queued
/// request that produced the pivoted payload (ownership rule, PAN-185).
fn unwind_bundle_pivots(
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
fn take_bundle_replaced(
    replaced: &mut PivotReplaced,
    qso_id: Option<&str>,
) -> Option<ReplacedFrame> {
    qso_id.and_then(|id| replaced.remove(&super::active_tx_qso_key(id)))
}

/// A frame that never aired after Step 4c may have pivoted to it.
///
/// Pivot ownership rule (PAN-185): the pivot tombstone (`pivoted_once`)
/// decides which request reports the QSO's outcome to Autonomous, so a
/// tracked self-CQ settles exactly once.
/// - Tombstone kept (the pivoted frame keyed): the frame reports, and the
///   queued request whose payload it absorbed is dropped silently (Step
///   0-dup, the backlog pivot-duplicate pass, mid-TX `IncomingDuringTx::Drop`).
/// - Tombstone cleared (the pivoted frame was refused before air): the frame
///   sends Autonomous nothing. The request that produced the pivoted payload
///   is still queued (its intent was only pivotable once queued) and reports
///   its own outcome.
///
/// Either way the QSO engine rolls back the frame the pivot replaced
/// (`pivot_replaced`), which never aired as itself (PAN-75). `attempt_id` is
/// the unaired frame's own attempt, reported only when no pivot replaced it;
/// a pivot reports the replaced frame's (PAN-183).
async fn report_unaired_frame(
    message_bus: &MessageBus,
    message_text: String,
    qso_id: Option<String>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
    pivot_replaced: Option<ReplacedFrame>,
) -> Result<()> {
    match pivot_replaced {
        None => {
            settle_unaired(
                message_bus,
                TxFrame {
                    qso_id,
                    message_text,
                    attempt_id,
                },
                None,
                AutonomousOutcome::Report,
            )
            .await;
        }
        Some(replaced) => report_pivot_duplicate_dropped(message_bus, qso_id, Some(replaced)).await,
    }
    Ok(())
}

/// Step 0-dup: this request's payload already aired through an earlier
/// request's pivot. Autonomous gets nothing: the pivoted frame keyed and
/// already reported (ownership rule: [`report_unaired_frame`], PAN-185). The
/// QSO engine keeps this request's attempt and instead rolls back the frame
/// the pivot replaced (`pivoted_from`: its text and payload), which never
/// aired as itself (PAN-75). The payload tells the two apart when the pivot
/// changed only the offset or the remote binding.
async fn report_pivot_duplicate_dropped(
    message_bus: &MessageBus,
    qso_id: Option<String>,
    pivoted_from: Option<ReplacedFrame>,
) {
    if let (Some(qso_id), Some((pivoted_from_text, replaced, replaced_attempt_id))) =
        (qso_id, pivoted_from)
    {
        settle_unaired(
            message_bus,
            TxFrame {
                qso_id: Some(qso_id),
                message_text: pivoted_from_text,
                attempt_id: replaced_attempt_id,
            },
            Some(replaced),
            AutonomousOutcome::Withheld,
        )
        .await;
    }
}

/// PAN-38 round 4 (Codex): report a failed `TransmitComplete` for a
/// single-item `TransmitRequest` that's being abandoned before it was ever
/// sent to the radio (operator F8 abort, shutdown-adjacent abandonment,
/// etc.). A tracked self-CQ's `pending_self_cq_qsos` entry (see
/// `coordinator/autonomous.rs`) is only ever cleared by a `TransmitComplete`
/// or an explicit `AutonomousCqDispatchFailed` -- an abort path that
/// silently `continue`s instead leaks that entry forever and never rolls
/// back the speculative streak/offset mutation the attempt made. No-op for
/// any other `MessageType` (Tune/Multi have their own completion paths).
async fn emit_failed_transmit_complete_for_request(
    message_bus: &MessageBus,
    message_type: &MessageType,
) {
    if let MessageType::TransmitRequest {
        message_text,
        qso_id,
        attempt_id,
        ..
    } = message_type
    {
        settle_unaired(
            message_bus,
            TxFrame {
                qso_id: qso_id.clone(),
                message_text: message_text.clone(),
                attempt_id: *attempt_id,
            },
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }
}

/// Surface a genuine TX-attempt failure (encode/modulate error, invalid
/// frequency, etc.) as a RETAINED diagnostic so the operator can see "TX
/// failed: <reason>" instead of a QSO silently sitting until the watchdog
/// times out — indistinguishable, from the operator's view, from the DX
/// simply not answering. Deliberate policy/security skips (TxPolicy
/// Disabled, stale-QSO drops, poisoned-lock fail-closed) are NOT routed
/// through this — those already have their own operator-visible signal
/// (TX-policy status, the emergency-stop toggle) and aren't the "invisible
/// failure" gap this closes. Best-effort: never blocks or fails the TX path.
async fn emit_tx_failure_diagnostic(
    message_bus: &MessageBus,
    qso_id: Option<&str>,
    message_text: &str,
    reason: &str,
) {
    emit_diagnostic(
        message_bus,
        "tx.encode",
        pancetta_core::DiagnosticLevel::Warn,
        format!("TX failed for '{}': {}", message_text, reason),
        qso_id,
    )
    .await;
}

/// Send a fully attributed `DiagnosticEvent` to the TUI's retained Diagnostics
/// overlay. Best-effort: diagnostics never block or fail the caller's path.
pub(crate) async fn emit_diagnostic_full(
    message_bus: &MessageBus,
    source: ComponentId,
    target: &'static str,
    level: pancetta_core::DiagnosticLevel,
    text: String,
    qso_id: Option<&str>,
    callsign: Option<&str>,
) {
    let msg = ComponentMessage::new(
        source,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            qso_id: qso_id.map(|s| s.to_string()),
            callsign: callsign.map(|s| s.to_string()),
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("DiagnosticEvent({}) relay failed (no TUI?): {}", target, e);
    }
}

/// Send a `DiagnosticEvent` to the TUI's retained Diagnostics overlay
/// (observability-diagnostics-plan.md Layer 1, "Emission"). `target` reuses
/// the same vocabulary already used by the co-located `tracing` call at each
/// site (e.g. `"tx.policy"`) — this is a SEPARATE field from that macro's own
/// `target:` (the tracing one gates file-log visibility via `EnvFilter`; this
/// one is just a TUI-side label used for filtering the Diagnostics panel).
/// Best-effort: never blocks or fails the TX path.
pub(crate) async fn emit_diagnostic(
    message_bus: &MessageBus,
    target: &'static str,
    level: pancetta_core::DiagnosticLevel,
    text: String,
    qso_id: Option<&str>,
) {
    emit_diagnostic_full(
        message_bus,
        ComponentId::Ft8Transmitter,
        target,
        level,
        text,
        qso_id,
        None,
    )
    .await;
}

/// Read the current global TX policy from the shared atomic.
fn current_tx_policy(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
) -> pancetta_core::TxPolicy {
    pancetta_core::TxPolicy::from_u8(tx_policy.load(Ordering::Acquire))
}

/// PAN-19 round-7 review (Codex P1): `hamlib_loop_ready` is checked as an
/// ADDITIONAL, orthogonal condition alongside `restart_inhibit` -- not a
/// replacement for it. `restart_inhibit` (`tx_restart_inhibit`) reflects
/// the coordinator's restart-supervision bookkeeping (`TxInhibitGuard`),
/// which releases the instant `start_hamlib_component` RETURNS -- even on
/// a `LoopReadyOutcome::TimedOut`, which must stay non-bailing so a slow
/// rig never hard-fails startup (the HIGH-fix invariant). `hamlib_loop_ready`
/// instead reflects whether the message loop has genuinely confirmed it's
/// consuming commands, set independently by `start_hamlib_component`
/// (`coordinator/hamlib.rs`) and never touched by the restart-supervision
/// counter machinery at all -- so a `TimedOut` startup still blocks PTT
/// here even after `TxInhibitGuard` has already released.
///
/// PAN-19 round-12 review (Codex P1): "apply loop readiness to direct PTT
/// commands". This is now `pub(crate)` and is THE single shared gate every
/// PTT-on call site in the coordinator must route through -- not just the
/// automated TX worker's own `schedule_tx`/multi-TX call sites below.
/// Round 7 through 11 only ever wired this into the TX worker; the direct,
/// operator-triggered `TogglePtt` path (`coordinator/tui_relay.rs`) had its
/// own separate, duplicated `tx_restart_inhibit`-only check that never
/// learned about `hamlib_command_loop_ready` at all, so a manual PTT toggle
/// during a slow Hamlib startup (`LoopReadyOutcome::TimedOut`, restart
/// counter already released) could still queue a key-up the loop consumes
/// later -- unexpectedly keying the radio well after the operator's actual
/// keypress. Routing `TogglePtt` through this SAME function (rather than
/// re-deriving the same `restart_inhibit && hamlib_loop_ready` condition a
/// second time) is deliberate: a third PTT-on call site added later gets
/// this gate for free instead of silently omitting it again.
///
/// PAN-19 round-14 review (Codex P1): "keep TX muted until pending rig
/// state is delivered". `hamlib_command_loop_ready` correctly reflects
/// "the message loop can consume commands" -- but that's not the same as
/// "the rig's state is actually correct". If a prior generation's
/// `SetSplit`/`SetFrequency` failed its first delivery attempt (channel
/// momentarily full) it's requeued into `hamlib_pending_frequency`/
/// `hamlib_pending_split` (round 10) for the polling task's own ~500ms
/// retry -- but the loop IS ready to consume commands during that gap, so
/// without this check PTT-on could slip through and transmit with the
/// rig still holding its stale, pre-crash split/frequency configuration.
/// `hamlib_pending_frequency`/`hamlib_pending_split` are checked here (the
/// SAME single shared choke point, not a third condition scattered
/// elsewhere) and, like `restart_inhibit`/`hamlib_loop_ready` above, fail
/// CLOSED (treated as still-pending, i.e. still muted) on a poisoned
/// lock -- see [`has_undelivered_pending_hamlib_state`].
///
/// PAN-19 round-16 review (Codex P1): "keep restored rig state pending
/// through CAT application". Round 14's pending-slot check alone still had
/// a gap: `deliver_pending_hamlib_state` clears a pending slot as soon as
/// the command is successfully handed off onto the channel -- NOT once the
/// rig has actually accepted it. A PTT-on gated through here while the
/// message loop is still awaiting the underlying `set_frequency`/
/// `set_split_freq`/`set_split` CAT call would see an already-empty
/// pending slot and be permitted, then get queued behind that in-flight
/// command and key the rig regardless of whether the CAT call succeeds or
/// fails. `hamlib_command_in_flight` closes that window: the message
/// loop's arms bump it for the CAT call's exact duration (see
/// `HamlibCommandInFlightGuard` in `coordinator/hamlib.rs`), and any
/// nonzero count is treated the same as "pending state undelivered" here.
///
/// PAN-19 round-19 review (Codex P1): "count every pending command
/// handoff". `hamlib_command_in_flight` is now a count (`AtomicU32`), not
/// a boolean -- see that field's doc comment in `coordinator/mod.rs` for
/// why a boolean under-reported when two handoffs (frequency + split)
/// were outstanding at once.
///
/// PAN-115: two hardware-health inputs, checked right after the two
/// restart/readiness reasons (hardware faults explain more than CAT
/// bookkeeping; the operator's own `Disabled` stays last). Like PAN-5's
/// restart inhibit they are health-owned safety state ANDed in here, never
/// folded into `TxPolicy`, and both clear on their own once the link or
/// device recovers:
/// - `rig_conn_state` ([`super::hamlib::RigConnState`], written by the Hamlib
///   poll loop) mutes only on `PollingFailed` ("was connected, polls have
///   failed for 5 s"). `NotConnected` is overloaded (mock rig, rig control
///   disabled, not tried yet, initial connect failed), so it does NOT mute;
///   a real rig that never connects still reaches `PollingFailed` after 5 s.
///   The poll loop redials rigctld while disconnected, which is what clears
///   this state again (PTT itself used to be the only redial).
/// - `audio_output_alive` (written by the real audio thread's
///   `OutputWatchdog`) is `false` from an output stream error, or 2 s without
///   an output callback, until a (re)opened stream calls back again. It stays
///   `true` in no-device modes (`--no-audio`, stub, replay), which have no
///   output device to judge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_hard_mute_reason(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    restart_inhibit: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    hamlib_loop_ready: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    rig_conn_state: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    audio_output_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    receive_gain: &std::sync::Arc<super::receive_overload::GainSafety>,

    data_mode: &pancetta_hamlib::DataModeReadiness,
) -> Option<&'static str> {
    if restart_inhibit.load(Ordering::Acquire) != 0 {
        Some("rig control is restarting")
    } else if !hamlib_loop_ready.load(Ordering::Acquire) {
        Some("Hamlib command loop is not yet ready")
    } else if super::hamlib::RigConnState::from_u8(rig_conn_state.load(Ordering::Acquire))
        == super::hamlib::RigConnState::PollingFailed
    {
        Some("rig is disconnected")
    } else if !audio_output_alive.load(Ordering::Acquire) {
        Some("audio output device is dead")
    } else if has_undelivered_pending_hamlib_state(hamlib_pending_frequency, hamlib_pending_split) {
        Some("pending rig frequency/split state has not been delivered yet")
    } else if hamlib_command_in_flight.load(Ordering::Acquire) > 0 {
        Some("a rig frequency/split command is still being applied")
    } else if receive_gain.blocked() {
        Some("receive gain change is in progress or unconfirmed")
    } else if !data_mode.is_ready() {
        Some("rig data mode is not ready")
    } else if current_tx_policy(tx_policy) == pancetta_core::TxPolicy::Disabled {
        Some("TX policy is Disabled")
    } else {
        None
    }
}

/// `true` when a prior generation's `SetFrequency`/`SetSplit` is still
/// sitting undelivered in either pending slot (round 10's requeue-on-full
/// -channel mechanism), waiting for the polling task's next retry. Fails
/// CLOSED (`true`, i.e. "treat as still pending") on a poisoned lock --
/// unlike some other pending-state checks in this codebase that fail open,
/// this one gates a TX-safety-critical decision (avoiding an off-frequency
/// transmission), so an unknown state must never be treated as "safe to
/// key".
fn has_undelivered_pending_hamlib_state(
    pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
) -> bool {
    // PAN-35: any VFO with an undelivered pending command must still mute
    // TX -- not just "the shared slot happens to be occupied".
    pending_frequency
        .lock()
        .map(|slots| !slots.is_empty())
        .unwrap_or(true)
        || pending_split
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(true)
}

/// PAN-118: frequency-aware sibling of [`tx_hard_mute_reason`]. That
/// function answers "may the station key at all right now" from global,
/// frequency-independent state; this one answers "may THIS transmission
/// key on THIS absolute RF frequency" from the configured band plan
/// (`rig.frequency.band_plan`; `edge_warnings` is the single master
/// switch for both the TUI edge warning and this hard refusal, so
/// `edge_warnings = false` remains the operator's documented escape
/// hatch). Per-request by construction (two bundle items can differ),
/// so it is NOT folded into `tx_hard_mute_reason` — whose one
/// frequency-less consumer (the QSO stall-evidence closure) must not
/// grow a frequency parameter. The boolean itself is
/// `pancetta_tui::app::tx_rf_out_of_band_plan`, the single source of
/// truth (region tables + `custom_bands` override) shared with the
/// TUI's entry-time warning. Refusals are rare; allocation cost is
/// fine. `pub` + re-exported from `coordinator` so the `coord_sim`
/// rig-simulation fixture mirrors the worker's gate with the REAL gate
/// functions, exactly as it already does for the coalescer, the arm
/// gate, and the liveness predicate.
///
/// `tx_rf_hz` is the LOWEST emitted tone (dial + audio offset, from
/// [`tx_rf_for_gate`]); `signal_span_hz` is how far above it the highest
/// emitted tone sits ([`tx_signal_span_hz`] for a protocol frame, `0.0`
/// for a single-tone tune or a raw manual-PTT carrier). The whole occupied
/// interval, low edge to high edge, must fit inside ONE allowed range of
/// the band plan (PR #446 Codex review: an FT8 frame whose base tone is
/// in-band can still put its top tone up to 43.75 Hz past an upper band
/// edge; PAN-208: a multi-range `custom_bands` entry must not be straddled
/// across the gap between two ranges —
/// `pancetta_tui::app::tx_interval_out_of_band_plan`).
pub fn tx_band_plan_mute_reason(
    tx_rf_hz: f64,
    signal_span_hz: f64,
    band_plan: &pancetta_config::BandPlanConfig,
) -> Option<String> {
    // Conservative integer edges computed from the exact f64 RF (PR #446
    // Codex review): the low edge rounds DOWN and the high edge rounds UP,
    // each from the unrounded base, so a fractional offset or span (FT4's
    // 62.4999 Hz) can never be checked inside the band while the real tone
    // sits a fraction of a Hz outside it. A non-finite RF fails closed; a
    // non-finite / negative span degrades to the base-tone-only check.
    if !tx_rf_hz.is_finite() {
        return Some(format!(
            "TX frequency {tx_rf_hz} Hz is not a valid RF frequency"
        ));
    }
    let span_hz = if signal_span_hz.is_finite() && signal_span_hz > 0.0 {
        signal_span_hz
    } else {
        0.0
    };
    let low_hz = tx_rf_hz.max(0.0).floor() as u64;
    let high_hz = (tx_rf_hz.max(0.0) + span_hz).ceil() as u64;
    let low_out = pancetta_tui::app::tx_rf_out_of_band_plan(low_hz, band_plan);
    // PAN-208: the whole occupied interval must fit inside ONE allowed range,
    // not just have each edge in some range.
    let interval_out = high_hz != low_hz
        && pancetta_tui::app::tx_interval_out_of_band_plan(low_hz, high_hz, band_plan);
    if low_out {
        Some(format!(
            "TX frequency {low_hz} Hz is outside the configured amateur band plan"
        ))
    } else if interval_out {
        Some(format!(
            "TX signal {low_hz}-{high_hz} Hz extends outside the configured \
             amateur band plan"
        ))
    } else {
        None
    }
}

/// PAN-118: distance in Hz from the lowest to the highest tone the
/// modulator emits for one `protocol` frame — `(num_tones - 1) *
/// tone_spacing`, exactly the upper bound `Ft8Modulator::
/// modulate_symbols_protocol` emits above its base frequency (FT8 43.75,
/// FT4 62.5, FT2 175). The band-plan gate adds it to the low edge to get
/// the occupied high edge.
pub fn tx_signal_span_hz(protocol: pancetta_ft8::Protocol) -> f64 {
    let params = pancetta_ft8::ProtocolParams::from_protocol(protocol);
    (params.num_tones as f64 - 1.0) * params.tone_spacing
}

/// PAN-118: absolute TX RF for gating: effective TX dial (split when
/// active, else RX dial — `pancetta_qso::qso_manager::effective_tx_dial`)
/// plus the request's absolute audio offset. `None` while the effective
/// TX dial reads 0 (no split set and RX dial 0) — the codebase's
/// "frequency not yet known" sentinel (TUI
/// 14.074 fallback, `--replay` seed, mock-rig dev modes) — in which case
/// the caller skips the band-plan check; every frequency-independent
/// hard-mute input still applies. Also the dial resolver for the TUI
/// relay's manual-PTT sibling gate (`tui_relay::ptt_band_plan_refusal`),
/// so the split-vs-RX-dial rule has exactly one implementation. `pub` +
/// re-exported for the same `coord_sim` reason as
/// `tx_band_plan_mute_reason` above.
pub fn tx_rf_for_gate(
    operating_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    split_tx_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    audio_offset_hz: f64,
) -> Option<f64> {
    let rx_dial = operating_frequency_hz.load(Ordering::Relaxed);
    let split = split_tx_frequency_hz.load(Ordering::Relaxed);
    // Resolve the effective TX dial FIRST (PR #446 Codex review): a known
    // split dial with the RX dial still 0 is a real TX frequency and must
    // be gated; only an effective dial of 0 means "not yet known".
    let tx_dial = pancetta_qso::qso_manager::effective_tx_dial(rx_dial, split);
    if tx_dial == 0 {
        return None;
    }
    // Exact f64 RF — NOT rounded here; `tx_band_plan_mute_reason` derives
    // conservative integer edges from it (PR #446 Codex review).
    Some(tx_dial as f64 + audio_offset_hz)
}

#[cfg(test)]
#[path = "tx/tests/pan118_band_plan_gate_tests.rs"]
mod pan118_band_plan_gate_tests;

/// PAN-208: a `custom_bands` entry with two or more `ranges` leaves gaps
/// inside the band. The occupied signal `[floor(low), ceil(low + span)]`
/// must fit inside ONE allowed range; checking the low and high edges
/// independently let a frame whose base tone sat in one range and top
/// tone in the next key across the excluded gap (PR #446 Codex review).
#[cfg(test)]
#[path = "tx/tests/pan208_band_plan_gap_tests.rs"]
pub(crate) mod pan208_band_plan_gap_tests;

/// PAN-89: one TX hard-mute cause, display-only. Declared in
/// [`tx_hard_mute_reason`]'s priority order; [`TxMuteCause::as_reason`]
/// returns the gate's string for each, byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxMuteCause {
    RigRestarting,
    HamlibLoopNotReady,
    RigDisconnected,
    AudioOutputDead,
    RigStatePending,
    RigCommandInFlight,
    ReceiveGainUnconfirmed,
    RigDataModeNotReady,
    PolicyDisabled,
}

impl TxMuteCause {
    /// The exact string [`tx_hard_mute_reason`] returns for this cause.
    pub(crate) fn as_reason(self) -> &'static str {
        match self {
            TxMuteCause::RigRestarting => "rig control is restarting",
            TxMuteCause::HamlibLoopNotReady => "Hamlib command loop is not yet ready",
            TxMuteCause::RigDisconnected => "rig is disconnected",
            TxMuteCause::AudioOutputDead => "audio output device is dead",
            TxMuteCause::RigStatePending => {
                "pending rig frequency/split state has not been delivered yet"
            }
            TxMuteCause::RigCommandInFlight => {
                "a rig frequency/split command is still being applied"
            }
            TxMuteCause::ReceiveGainUnconfirmed => {
                "receive gain change is in progress or unconfirmed"
            }
            TxMuteCause::RigDataModeNotReady => "rig data mode is not ready",
            TxMuteCause::PolicyDisabled => "TX policy is Disabled",
        }
    }
}

/// PAN-89: every active TX hard-mute cause, in [`tx_hard_mute_reason`]'s
/// priority order. Display-only (the station-readiness strip): it evaluates
/// every predicate instead of stopping at the first, and never gates TX
/// itself. Its first element always equals the gate's verdict (parity-tested
/// over every input combination), and it is empty exactly when the gate is
/// open.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_mute_causes(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    restart_inhibit: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    hamlib_loop_ready: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    rig_conn_state: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    audio_output_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    receive_gain: &std::sync::Arc<super::receive_overload::GainSafety>,

    data_mode: &pancetta_hamlib::DataModeReadiness,
) -> Vec<TxMuteCause> {
    let mut causes = Vec::new();
    if restart_inhibit.load(Ordering::Acquire) != 0 {
        causes.push(TxMuteCause::RigRestarting);
    }
    if !hamlib_loop_ready.load(Ordering::Acquire) {
        causes.push(TxMuteCause::HamlibLoopNotReady);
    }
    if super::hamlib::RigConnState::from_u8(rig_conn_state.load(Ordering::Acquire))
        == super::hamlib::RigConnState::PollingFailed
    {
        causes.push(TxMuteCause::RigDisconnected);
    }
    if !audio_output_alive.load(Ordering::Acquire) {
        causes.push(TxMuteCause::AudioOutputDead);
    }
    if has_undelivered_pending_hamlib_state(hamlib_pending_frequency, hamlib_pending_split) {
        causes.push(TxMuteCause::RigStatePending);
    }
    if hamlib_command_in_flight.load(Ordering::Acquire) > 0 {
        causes.push(TxMuteCause::RigCommandInFlight);
    }
    if receive_gain.blocked() {
        causes.push(TxMuteCause::ReceiveGainUnconfirmed);
    }
    if !data_mode.is_ready() {
        causes.push(TxMuteCause::RigDataModeNotReady);
    }
    if current_tx_policy(tx_policy) == pancetta_core::TxPolicy::Disabled {
        causes.push(TxMuteCause::PolicyDisabled);
    }
    causes
}

#[cfg(test)]
#[path = "tx/tests/tx_hard_mute_reason_tests.rs"]
mod tx_hard_mute_reason_tests;

/// Whether a TX item belonging to `qso_id` is still allowed on the air, given
/// the shared active-QSO set. Thin wrapper over [`super::tx_qso_is_live`] that
/// takes the read lock. A poisoned lock fails *open* (returns `true`) — a stuck
/// lock should never silently mute legitimate TX; the worst case reverts to the
/// pre-fix behavior for one cycle, which the operator-facing emergency stop
/// (Shift+Q → cancel-all + Disabled) still covers.
fn tx_qso_is_live(
    qso_id: Option<&str>,
    active: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
) -> bool {
    match active.read() {
        Ok(set) => super::tx_qso_is_live(qso_id, &set),
        Err(_) => true,
    }
}

/// Multi-TX key-time liveness check (Step 4b fast-path gate).
///
/// 2026-07-17 operator finding: a multi-TX bundle's waveform is summed
/// once, up front (Step 1), from whichever items survived Step 0b's
/// per-item liveness filter. The bundle then waits (Step 4) for the PTT
/// engage instant, and that wait CAN span an operator abort ('k') of one
/// of the bundled QSOs. Because the waveform is already a single summed
/// buffer at that point, it can't be partially re-filtered in place — so
/// the OLD key-time gate only checked "did EVERY item go stale" and, if
/// even one item was still live, transmitted the WHOLE pre-summed audio,
/// including the now-cancelled item's already-baked-in call.
/// Operator-observed symptom: killing a QSO to one DX station while a
/// second, still-live QSO shared the same bundle still transmitted a
/// call to the killed one.
///
/// This function answers only "did NOTHING go stale" (the common-case
/// fast path — reuse the already-built audio unchanged). When something
/// DID go stale, the caller re-encodes just the still-live subset via
/// [`encode_and_modulate_multi_tx`] and re-applies the already-computed
/// `TxSchedule` (timing doesn't depend on which items are in the bundle)
/// instead of either transmitting the stale audio or dropping the whole
/// bundle outright.
fn multi_tx_bundle_still_fully_live(
    qso_ids: &[Option<String>],
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
) -> bool {
    qso_ids
        .iter()
        .all(|id| tx_qso_is_live(id.as_deref(), active_tx_qsos))
}

/// PAN-141: the freshness window for [`dx_parity_conflict`] — self-scaling
/// with the live protocol (`2 × active_slot_ns`: FT8 30s, FT4 15s, FT2
/// 6.4s), matching the incident-measured ~20-30s gap between a manual call's
/// parity latch and its actual key-up. Deliberately independent of
/// `CrossTimeState::a7_recent_calls`'s own 30s `max_age` — that value is
/// tuned for hb-048's decoder-correlation purpose, not TX-collision
/// avoidance, and `get()` doesn't filter by age on read (eviction is lazy,
/// on the next `record()`), so a caller must apply its own window regardless.
fn dx_parity_freshness_window(slot_ns: i64) -> std::time::Duration {
    std::time::Duration::from_nanos(slot_ns.max(0) as u64 * 2)
}

/// PAN-146 (Codex P2 on PR #370, round 1): the band and slot grid the frame
/// about to key belongs to. An `a7_recent_calls` entry counts as DX-parity
/// evidence only if it was observed in this context — a parity heard on
/// another band says nothing about this one, and a parity computed on
/// another protocol's slot grid describes different slot boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DxParityContext {
    /// Live RX dial (Hz) from `operating_frequency_hz`; `0` = unknown.
    dial_hz: u64,
    /// Slot period (ns) `required_parity` was computed on. Also sets the
    /// freshness window ([`dx_parity_freshness_window`]).
    slot_ns: i64,
}

impl DxParityContext {
    /// Reads the live dial now — call at the gate, not earlier.
    fn live(operating_frequency_hz: &std::sync::atomic::AtomicU64, slot_ns: i64) -> Self {
        Self {
            dial_hz: operating_frequency_hz.load(Ordering::Relaxed),
            slot_ns,
        }
    }

    /// Same slot grid, and same band per [`super::is_band_change`] — the rule
    /// the DSP band-change flush and the C9 QSO teardown use. The band check
    /// is skipped while either dial is the unknown-0 sentinel (as PAN-118's
    /// band-plan gate does): with no dial known there is no band switch to
    /// detect, so the PAN-141 hold keeps working on a station without CAT.
    fn observed_in(self, entry: &pancetta_qso::A7ExpectedCall) -> bool {
        entry.slot_ns == self.slot_ns
            && (entry.captured_dial_hz == 0
                || self.dial_hz == 0
                || !super::is_band_change(entry.captured_dial_hz, self.dial_hz))
    }
}

/// PAN-141: does the DX's most recently observed OWN-TX slot parity (from
/// [`pancetta_qso::CrossTimeState::a7_recent_calls`] — populated from every
/// decode this station makes, regardless of QSO relevance) conflict with
/// `required_parity`, the parity we are about to key into?
///
/// A "conflict" means: within the last `freshness_window`, `their_callsign`
/// was observed transmitting on `required_parity` itself. Since two stations
/// in QSO must transmit on OPPOSITE parities, a DX transmitting on the exact
/// slot we're about to use means they're mid-exchange with someone else in
/// that window — keying now would collide with that exchange, not reach
/// them. Returns `false` (proceed) on anything short of that positive fresh
/// signal: no entry for this callsign, an entry older than the freshness
/// window, or an unreadable (poisoned) table — this gate only ever blocks a
/// TX on POSITIVE fresh evidence of a collision, never on missing data,
/// mirroring `tx_qso_is_live`'s own fail-open posture on a poisoned lock.
///
/// Codex P2 on PR #370 (round 2, superseded by round 3): the lookup must
/// use the repo's established callsign-equivalence rules, not an exact
/// `HashMap` key match — `a7_recent_calls` is populated from on-air decodes
/// (whatever compound form the DX happened to sign with), while
/// `their_callsign` here comes from `QsoMetadata`, which can latch a
/// DIFFERENT compound form of the same station (round 2: also case, e.g. a
/// remote `callStation` command supplying lowercase; round 3: also compound
/// spelling, e.g. `EA8/G8BCG` latched but a later frame signs bare
/// `G8BCG`). Both are exactly what `pancetta_core::callsign::callsigns_match`
/// already exists to handle, so this scans every fresh matching entry (via
/// `entries_with_parity`, an existing public iterator, called once per
/// parity to cover the whole table with no exact-key lookup) using that
/// same shared matcher instead of hand-rolling a second, narrower
/// equivalence rule.
///
/// Local review gate, round 3: a compound and bare spelling of the same
/// station are separate table entries with independent timestamps and
/// parities — scanning only the REQUESTED parity's subset could match a
/// stale alias while a fresher entry (a different spelling, opposite
/// parity — i.e. NOT a conflict) exists for the same station. Selecting the
/// single NEWEST fresh matching entry across both parities first, then
/// comparing only its parity, uses the DX's actual latest known state
/// rather than whichever alias happens to match the parity being tested.
///
/// PAN-146: only entries observed in `context` (same band, same slot grid —
/// [`DxParityContext::observed_in`]) are candidates, and the context filter
/// runs before the newest-entry pick, so an out-of-context alias can neither
/// hold the frame nor hide an in-context entry.
fn dx_parity_conflict(
    their_callsign: &str,
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    cross_time_state: &pancetta_qso::CrossTimeState,
    now: std::time::SystemTime,
) -> bool {
    let freshness_window = dx_parity_freshness_window(context.slot_ns);
    let Ok(a7) = cross_time_state.a7_recent_calls.read() else {
        return false;
    };
    let newest = a7
        .entries_with_parity(0)
        .chain(a7.entries_with_parity(1))
        .filter(|entry| pancetta_core::callsign::callsigns_match(&entry.callsign, their_callsign))
        .filter(|entry| context.observed_in(entry))
        .filter(|entry| {
            now.duration_since(entry.decoded_at)
                .is_ok_and(|age| age <= freshness_window)
        })
        .max_by_key(|entry| entry.decoded_at);
    let Some(entry) = newest else {
        return false;
    };
    let observed = match entry.slot_parity {
        0 => pancetta_core::slot::SlotParity::Even,
        _ => pancetta_core::slot::SlotParity::Odd,
    };
    observed == required_parity
}

/// PAN-141: resolves `qso_id` to its established DX callsign via the
/// restart-safe `qso_manager_watch` handle (re-borrowed fresh here, never
/// cached across the pre-PTT wait — see PAN-72's `qso_manager_watch` amendment
/// for why a spawn-time-captured clone would go stale across a supervised Qso
/// restart). Fails open (`None`) at every step short of a resolved,
/// established partner: no `qso_id` (manual/tune sends never carry a target
/// to check), the Qso component not up yet, an unparseable id, the QSO not
/// found, or a QSO with no established `their_callsign` yet (e.g. a still-
/// unpartnered `CallingCq`).
///
/// PAN-148: this is the ONLY inherently-async half of the DX-parity check —
/// `manager.get_qso(id).await` is a real yield point; `their_callsign` is a
/// stable property of an established QSO once set, so resolving it here and
/// consulting it in a later, purely SYNCHRONOUS read (see
/// [`dx_parity_conflict`], over `a7_recent_calls` — a `std::sync::RwLock`,
/// not async) does not reintroduce staleness. Split out specifically so a
/// caller checking MULTIPLE items (`bundle_live_mask`) can resolve every
/// item's callsign first (the only awaits) and then read the actual
/// freshness-sensitive state for all of them back-to-back with zero
/// intervening awaits — see that function's doc comment for why a single
/// combined async-then-sync-per-item loop is NOT equivalent.
async fn resolve_their_callsign(
    qso_id: Option<&str>,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
) -> Option<String> {
    let id_str = qso_id?;
    let id = id_str.parse::<pancetta_qso::QsoId>().ok()?;
    let manager = qso_manager_watch.borrow().clone()?;
    let progress = manager.get_qso(id).await.ok()?;
    progress.metadata.their_callsign
}

/// PAN-141: resolves `qso_id` to its established DX callsign, then applies
/// [`dx_parity_conflict`]. See [`resolve_their_callsign`] for the fail-open
/// conditions.
///
/// PAN-146: the live dial is read AFTER the resolver's await, alongside the
/// clock, so a band switch landing during that await is seen.
async fn dx_parity_conflict_for_qso(
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    operating_frequency_hz: &std::sync::atomic::AtomicU64,
    slot_ns: i64,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> bool {
    let Some(their_callsign) = resolve_their_callsign(qso_id, qso_manager_watch).await else {
        return false;
    };
    dx_parity_conflict(
        &their_callsign,
        required_parity,
        DxParityContext::live(operating_frequency_hz, slot_ns),
        cross_time_state,
        std::time::SystemTime::now(),
    )
}

/// PAN-148: reasons the Step 4d final gate can deny a single-TX key.
/// Distinct variants only so the denial's log/diagnostic text can name the
/// actual reason, mirroring Step 4b/4b-parity's separate messages for the
/// same two conditions re-evaluated here at the true last instant.
/// PAN-117 adds the third: the frame's elapsed time in its target slot
/// exceeded `tx_late_max_ms` by the time the gate ran — keying it would
/// put an undecodable fragment on the air.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalGateDenial {
    StaleQso,
    DxParityConflict,
    TooLateToDecode { elapsed_ms: u64, late_max_ms: u64 },
}

/// PAN-141 (Step 4b) / PAN-148 (Step 4d): resolves every id's DX callsign —
/// the batch form of [`resolve_their_callsign`] for a multi-TX bundle.
/// Callers running this for the Step 4d final gate MUST call it BEFORE
/// Step 3's final audio-trim (see that call site's comment: round-2 review,
/// Codex P1 — awaiting this lookup AFTER the trim risked keying `SetPtt`
/// against a now-stale alignment cursor if `QsoManager`'s internal lock
/// were ever contended).
async fn resolve_callsigns(
    ids: &[Option<String>],
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(resolve_their_callsign(id.as_deref(), qso_manager_watch).await);
    }
    out
}

/// PAN-148 Step 4d: the single-TX path's final, atomic pre-PTT gate —
/// SYNCHRONOUS, using ONLY state read at THIS call — never a value cached
/// from Step 4b/4b-parity earlier in the cycle, which can have gone stale
/// during Step 4c's pivot/remodulation or Step 5's status-announce awaits
/// (`send_tx_status`/`send_tx_queue_status`). The caller MUST NOT `.await`
/// anything else between this call returning and the `SetPtt{true}` send it
/// gates — see the call site's own comment.
///
/// `their_callsign` must be resolved SEPARATELY and EARLY, via
/// [`resolve_their_callsign`] called before Step 3's final audio-trim — see
/// that function's doc comment. Round-1 review (Codex P1) found an earlier
/// version checking liveness before awaiting the DX-parity lookup inline
/// here (a cancellation landing during that await went uncaught); round-2
/// review (Codex P1) found that simply reordering those two checks still
/// left an unbounded `QsoManager`-lock-contention wait sitting AFTER Step
/// 3's timing-sensitive trim, risking a stale audio-alignment cursor at key
/// time. Resolving the callsign early and passing it in here — leaving this
/// function with NO internal `.await` at all — fixes both: liveness and the
/// actual freshness read (`dx_parity_conflict`) are the true last reads
/// before `SetPtt`, and nothing async can suspend between Step 3's trim and
/// the key.
///
/// PAN-117's slot-timing re-check is deliberately NOT here: it reads the
/// clock, so it runs as its own [`key_time_gate_denial`] call after every
/// other synchronous pre-PTT gate (this one, the hard mute, the remote arm
/// recheck), immediately before the `SetPtt{true}` send — PR #450 Codex
/// review: a clock read taken here could go stale while this function's
/// `RwLock` reads and the later gates ran.
fn final_single_tx_gate_denial(
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    their_callsign: Option<&str>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> Option<FinalGateDenial> {
    if !tx_qso_is_live(qso_id, active_tx_qsos) {
        return Some(FinalGateDenial::StaleQso);
    }
    let conflict = their_callsign.is_some_and(|callsign| {
        dx_parity_conflict(
            callsign,
            required_parity,
            context,
            cross_time_state,
            std::time::SystemTime::now(),
        )
    });
    if conflict {
        return Some(FinalGateDenial::DxParityConflict);
    }
    None
}

/// PAN-145: compare raw QsoId epochs after `PttGuard::new` has set
/// `ptt_active` under the shared gate. Later offset switches now refuse, so
/// these reads need no async lock or repeated DSP. None is manual/test TX.
fn relocation_epoch_stale(
    epochs: &pancetta_qso::qso_manager::TxRelocationEpochs,
    snapshots: &[(Option<String>, u64)],
) -> bool {
    snapshots
        .iter()
        .any(|(id, before)| id.as_deref().is_some_and(|id| epochs.epoch(id) != *before))
}

/// PAN-117: the slot-timing half of the Step 4d final gate, shared by both
/// worker arms. `now` must be the caller's TRUE last clock read — taken
/// after every other synchronous pre-PTT gate, immediately before the
/// `SetPtt{true}` send — and if more than `late_max_ms` of `target_slot`
/// has elapsed by then the frame is denied with `TooLateToDecode`: keyed
/// that late it is mathematically undecodable for any receiver, so it is
/// dropped rather than keyed (the QSO engine's existing retry timers own
/// the re-send).
fn key_time_gate_denial(
    now: chrono::DateTime<chrono::Utc>,
    target_slot: chrono::DateTime<chrono::Utc>,
    late_max_ms: u64,
) -> Option<FinalGateDenial> {
    key_time_too_late(now, target_slot, late_max_ms).then(|| FinalGateDenial::TooLateToDecode {
        elapsed_ms: (now - target_slot).num_milliseconds().max(0) as u64,
        late_max_ms,
    })
}

/// Log + diagnostic text for a single-TX Step 4d denial, `(log, diagnostic)`.
fn final_gate_denial_text(
    denial: FinalGateDenial,
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    message_text: &str,
) -> (String, String) {
    match denial {
        FinalGateDenial::StaleQso => (
            format!(
                "dropping stale TX at final pre-PTT check for ended QSO {}: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!("dropping stale TX at final pre-PTT check for ended QSO: '{message_text}'"),
        ),
        FinalGateDenial::DxParityConflict => (
            format!(
                "holding TX at final pre-PTT check for QSO {}: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!(
                "holding TX at final pre-PTT check: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'"
            ),
        ),
        FinalGateDenial::TooLateToDecode {
            elapsed_ms,
            late_max_ms,
        } => (
            format!(
                "dropping stale TX at final pre-PTT check for QSO {}: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={late_max_ms}ms — no receiver could decode it: '{message_text}'",
                qso_id.unwrap_or("?")
            ),
            format!(
                "dropping stale TX at final pre-PTT check: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={late_max_ms}ms — no receiver could decode it: '{message_text}'"
            ),
        ),
    }
}

/// PAN-141 (Step 4b) / PAN-148 (Step 4d): per-item liveness+DX-parity mask
/// for a multi-TX bundle — SYNCHRONOUS. Extracted so both call sites —
/// Step 4b's original key-time gate and the new Step 4d final gate — run
/// the textually identical check against fresh state; Step 4d's caller
/// MUST NOT `.await` anything else between this call returning and the
/// `SetPtt{true}` send it gates.
///
/// `callsigns` (one entry per `encoded_qso_ids`, same order) must be
/// resolved SEPARATELY via [`resolve_callsigns`] — round-1 review (Codex
/// P1) found an earlier version awaiting `dx_parity_conflict_for_qso` per
/// item in one sequential loop let item 1's synchronous parity read go
/// stale while item 2..N's lookups were still pending; round-2 review
/// (Codex P1) found that even a two-pass split awaiting INSIDE this
/// function still left an unbounded `QsoManager`-lock-contention wait
/// sitting after Step 3's timing-sensitive trim when called from the Step
/// 4d final gate. Splitting the resolution out entirely — leaving this
/// function with NO internal `.await` — fixes both: callers resolve
/// `callsigns` before Step 3's trim (bounding the misalignment risk to
/// wherever that resolution already had to happen), and this function's
/// own reads (`tx_qso_is_live`, `dx_parity_conflict`, both synchronous) run
/// for every item back-to-back with no yield point between them, so
/// nothing can interleave a change for one item's evidence while another's
/// is being read.
///
/// Returns `(live_mask, parity_hold_mask)`: `live_mask` is the "should this
/// item transmit" mask every caller ultimately wants; `parity_hold_mask` is
/// returned alongside it only so a caller that reports per-item drop
/// reasons (Step 4b's partial-staleness branch) can distinguish "ended" from
/// "held for colliding DX parity" without a second pass.
fn bundle_live_mask(
    encoded_qso_ids: &[Option<String>],
    callsigns: &[Option<String>],
    required_parity: pancetta_core::slot::SlotParity,
    context: DxParityContext,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
) -> (Vec<bool>, Vec<bool>) {
    let now = std::time::SystemTime::now();
    let parity_hold_mask: Vec<bool> = callsigns
        .iter()
        .map(|callsign| {
            callsign.as_deref().is_some_and(|cs| {
                dx_parity_conflict(cs, required_parity, context, cross_time_state, now)
            })
        })
        .collect();
    let live_mask = encoded_qso_ids
        .iter()
        .zip(parity_hold_mask.iter())
        .map(|(id, &held)| tx_qso_is_live(id.as_deref(), active_tx_qsos) && !held)
        .collect();
    (live_mask, parity_hold_mask)
}

/// Build the TX-strip status items for a multi-TX bundle, tagging every item
/// with the SAME `deferred` flag — a bundle either defers as a whole or
/// doesn't (all its items share one parity/slot, per the bundling logic).
/// Shared by the initial "QUEUED" status (`deferred: false`) and the
/// Step-2b defer-time status refresh (`deferred: true`, TX-F8) so the two
/// call sites can't drift out of sync.
fn multi_tx_status_items(
    items: &[crate::message_bus::TransmitRequestItem],
    deferred: bool,
) -> Vec<crate::message_bus::TxItem> {
    items
        .iter()
        .map(|it| crate::message_bus::TxItem {
            text: it.message_text.clone(),
            freq_hz: it.frequency_offset,
            qso_id: it.qso_id.clone(),
            deferred,
        })
        .collect()
}

/// Result of [`encode_and_modulate_multi_tx`].
struct MultiTxEncodeOutcome {
    /// `Ok(samples)` — the summed waveform — when at least one item
    /// encoded and `modulate_multi_tx` succeeded. `Err(reason)` covers
    /// both "nothing encoded" and "modulation of the survivors failed";
    /// the caller only needs to diagnose the latter (when `item_texts`
    /// is non-empty — encode failures are already reported via
    /// `encode_failed`).
    samples: Result<Vec<f32>, String>,
    /// The original `TransmitRequestItem`s that encoded successfully —
    /// parallel to `item_texts`/`encoded_qso_ids`, in the same order
    /// they were summed into `samples`.
    ///
    /// 2026-07-17: added after an independent review of an earlier
    /// version of this fix found that the CALLER was zipping the
    /// unfiltered input `items` list against this function's (possibly
    /// shorter, when any item failed to encode) `encoded_qso_ids` —
    /// silently misaligning positions and reintroducing the exact class
    /// of bug this feature exists to prevent (in the worst case,
    /// transmitting a cancelled QSO's call while dropping the genuinely
    /// still-live one). Callers MUST rebind their `items` to this field
    /// immediately after calling this function, rather than continuing
    /// to use their original input list.
    encoded_items: Vec<crate::message_bus::TransmitRequestItem>,
    /// Message texts of items that encoded successfully — parallel to
    /// `encoded_items`/`encoded_qso_ids`, in the same order they were
    /// summed into `samples`.
    item_texts: Vec<String>,
    /// QSO ids of items that encoded successfully — parallel to
    /// `item_texts`. `None` for manual/tune items (never gated by
    /// liveness), matching `tx_qso_is_live`'s contract.
    encoded_qso_ids: Vec<Option<String>>,
    /// Items whose `encode_for_protocol` call itself failed (never
    /// reached modulation) — the caller reports each of these
    /// individually via `emit_tx_failure_diagnostic` + `TransmitComplete`.
    encode_failed: Vec<crate::message_bus::TransmitRequestItem>,
}

/// Encode + modulate a set of multi-TX items into one summed waveform.
///
/// 2026-07-17: extracted from the original Step 1 inline block so the
/// SAME encode/modulate logic can run twice — once normally (the full
/// bundle), and again at Step 4b's key-time gate for just the
/// still-live subset when the bundle's item set shrank during the
/// pre-PTT wait — instead of either transmitting stale audio for a
/// cancelled QSO baked into an already-summed waveform, or unconditionally
/// dropping a still-live item's cycle for no reason.
///
/// This extraction also fixes a pre-existing index-misalignment bug in
/// the original inline version: it built each `MultiTxItem`'s frequency
/// via `items[i].frequency_offset` AFTER the encode loop had already
/// skipped failed items from `symbol_sets` — so if any item other than
/// the last failed to encode, every later surviving item was summed at
/// the WRONG (unrelated, earlier-indexed) frequency. Here, each item's
/// frequency offset is pushed in the SAME loop iteration as its
/// successful encode, so nothing can misalign.
fn encode_and_modulate_multi_tx(
    encoder: &mut Ft8Encoder,
    active_protocol: pancetta_ft8::Protocol,
    tx_params: &pancetta_ft8::ProtocolParams,
    items: &[crate::message_bus::TransmitRequestItem],
) -> MultiTxEncodeOutcome {
    // TransmitRequestItem.frequency_offset is the ABSOLUTE audio
    // frequency (matching TransmitRequest semantics). modulate_multi_tx
    // wants per-item OFFSETS from a shared base. Use base=200 (lowest
    // valid) so any audio frequency in the 200-2500 Hz FT8 passband maps
    // to a non-negative per-item offset.
    const MULTI_TX_BASE_HZ: f64 = 200.0;

    let mut symbol_sets: Vec<Vec<u8>> = Vec::new();
    let mut encoded_items: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();
    let mut item_texts: Vec<String> = Vec::new();
    let mut encoded_qso_ids: Vec<Option<String>> = Vec::new();
    let mut freq_offsets: Vec<f64> = Vec::new();
    let mut encode_failed: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();

    for item in items {
        match encode_for_protocol(encoder, active_protocol, &item.message_text) {
            Ok(symbols) => {
                encoded_items.push(item.clone());
                item_texts.push(item.message_text.clone());
                encoded_qso_ids.push(item.qso_id.clone());
                freq_offsets.push(item.frequency_offset - MULTI_TX_BASE_HZ);
                symbol_sets.push(symbols);
            }
            Err(e) => {
                warn!("Encoding failed for '{}': {}", item.message_text, e);
                encode_failed.push(item.clone());
            }
        }
    }

    if symbol_sets.is_empty() {
        return MultiTxEncodeOutcome {
            samples: Err("no items to modulate".to_string()),
            encoded_items,
            item_texts,
            encoded_qso_ids,
            encode_failed,
        };
    }

    let multi_items: Vec<_> = symbol_sets
        .iter()
        .zip(freq_offsets.iter())
        .map(|(symbols, &frequency_offset)| pancetta_ft8::MultiTxItem {
            symbols: symbols.as_slice(),
            frequency_offset,
            params: tx_params,
        })
        .collect();

    let samples = match pancetta_ft8::modulate_multi_tx(&multi_items, 12000, MULTI_TX_BASE_HZ, 0.5)
    {
        Ok(samples) => {
            info!(
                "Multi-TX: {} messages -> {} samples ({:.2}s)",
                multi_items.len(),
                samples.len(),
                samples.len() as f64 / 12000.0
            );
            Ok(samples)
        }
        Err(e) => {
            warn!("Multi-TX modulation failed: {}", e);
            Err(format!("{e}"))
        }
    };

    MultiTxEncodeOutcome {
        samples,
        encoded_items,
        item_texts,
        encoded_qso_ids,
        encode_failed,
    }
}

/// Whether a **remote-originated** TX request from `channel` is permitted to
/// key PTT right now, per the shared arm gate.
///
/// This is the safety gate for `TxOrigin::Remote` requests only — the worker
/// never calls it for `TxOrigin::Local` (local TX is byte-identical). It reads
/// the shared [`ArmState`](pancetta_agent::arm::ArmState) and returns
/// `arm.tx_permitted(now(), channel)` — `channel`'s own consent bit, never
/// another channel's. A diagnostic entry point: production gates go through
/// [`remote_tx_permitted_for`], which derives the channel from the frame.
///
/// **Fail-CLOSED on a poisoned lock** — the OPPOSITE of [`tx_qso_is_live`]'s
/// fail-open. This is a safety gate: if the arm mutex is poisoned we can no
/// longer prove the station consented, so remote TX is DENIED. `now` is the
/// one clock read (`ArmState` itself is pure); it runs only AFTER the lock is
/// acquired, so a lock wait cannot make the sample stale. Production passes
/// [`ArmTime::now`](pancetta_agent::arm::ArmTime::now).
pub fn remote_tx_permitted(
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    channel: pancetta_agent::arm::ConsentChannel,
) -> bool {
    match arm.lock() {
        Ok(state) => state.tx_permitted(now(), channel),
        Err(_) => false,
    }
}

/// The remote-TX channel a frame originated on, derived from its bound client
/// identity: every station-agent producer binds `Some(peer)`
/// (`tx_kind_to_qso_message`), while the WSJT-X UDP bridge — which has no
/// client identity concept — binds `None`. Selects which channel's own
/// consent bit the TX-time gate requires.
fn consent_channel_for(remote_client_key_id: Option<&str>) -> pancetta_agent::arm::ConsentChannel {
    match remote_client_key_id {
        Some(_) => pancetta_agent::arm::ConsentChannel::StationAgent,
        None => pancetta_agent::arm::ConsentChannel::WsjtxUdp,
    }
}

/// Like [`remote_tx_permitted`], but additionally binds the check to a
/// specific client (PAN-91).
///
/// The plain boolean arm gate above only asks "is *some* client currently
/// armed?" — it has no way to tell whether the frame in hand is the one that
/// specific client actually requested. Without this, a peer without TX-arm
/// eligibility could get a `TxRequest`-originated QSO queued while the
/// controller slot is free (creating the QSO is deliberately never gated —
/// only transmission is), and if a DIFFERENT, legitimate peer later takes
/// control and arms, the worker would authorize the pending frame from the
/// FIRST peer, because the old gate only checked "is the shared `ArmState`
/// armed", never "armed by the same client this frame is bound to".
///
/// `remote_client_key_id` also names the ORIGINATING channel, whose own
/// current consent is required (PAN-98; one channel's consent never permits
/// another channel's TX):
///
/// - `None` — the WSJT-X UDP bridge (no client identity concept): requires
///   the WSJT-X UDP channel's consent ([`ConsentChannel::WsjtxUdp`]) plus the
///   rest of the boolean arm gate, with no identity binding.
/// - `Some(id)` — the station-agent channel: requires the station-agent
///   channel's consent ([`ConsentChannel::StationAgent`]) plus the rest of the
///   boolean arm gate, AND that `id` is the client
///   `ArmState::armed_client_key_id()` currently reports — a mismatch
///   (including "armed, but by someone else") denies fail-closed.
///
/// [`ConsentChannel::WsjtxUdp`]: pancetta_agent::arm::ConsentChannel::WsjtxUdp
/// [`ConsentChannel::StationAgent`]: pancetta_agent::arm::ConsentChannel::StationAgent
pub fn remote_tx_permitted_for(
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    remote_client_key_id: Option<&str>,
) -> bool {
    match arm.lock() {
        Ok(state) => {
            if !state.tx_permitted(now(), consent_channel_for(remote_client_key_id)) {
                return false;
            }
            match remote_client_key_id {
                None => true,
                Some(id) => state.armed_client_key_id() == Some(id),
            }
        }
        Err(_) => false,
    }
}

/// PAN-134: authorization for all four safety-critical sends. Evaluated
/// immediately before the enqueue: for audio inside MessageBus's read lock by
/// `send_message_if`, after the lock wait; for PTT-on by
/// `send_rig_command_if`, with no lock (PAN-202).
/// Never check this and then use plain `send_message`: that restores the gap.
fn tx_send_authorized(
    origin: crate::message_bus::TxOrigin,
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&str>,
) -> bool {
    origin != crate::message_bus::TxOrigin::Remote
        || remote_tx_permitted_for(arm, pancetta_agent::arm::ArmTime::now, remote_client_key_id)
}

/// PAN-202: the one path every PTT command takes to the rig from the TX
/// worker (single TX, bundle, tune) and the TUI's manual PTT. Synchronous:
/// [`MessageBus::send_rig_command_if`] takes no lock and no await, so the
/// last pre-PTT gate's verdict still holds when the command reaches Hamlib's
/// queue, even while another component registers a bus channel.
/// `authorize` follows `send_message_if`'s synchronous-leaf rule.
struct GainTxIntent(std::sync::Arc<std::sync::atomic::AtomicU32>);
impl GainTxIntent {
    fn new(counter: std::sync::Arc<std::sync::atomic::AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}
impl Drop for GainTxIntent {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn send_ptt_command(
    message_bus: &MessageBus,
    source: ComponentId,
    state: bool,
    authorize: impl FnOnce() -> bool,
) -> SendIfOutcome {
    // One CAS arbitrates gain admission and every key-down enqueue, without
    // introducing a lock or await into the last-gate-to-enqueue path.
    if state {
        match message_bus
            .gain_tx_gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                if value & 1 == 0 {
                    value.checked_add(4)
                } else {
                    None
                }
            }) {
            Ok(_) => (),
            Err(_) => return SendIfOutcome::Denied,
        }
    }
    let mut admitted = false;
    let outcome = message_bus.send_rig_command_if(
        ComponentMessage::new(
            source,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state }),
            Instant::now(),
        ),
        || {
            if !authorize() {
                return false;
            }
            if !state {
                return true;
            }
            if !message_bus.data_mode.is_ready() {
                return false;
            }
            admitted = message_bus
                .receive_gain
                .admit_ptt(chrono::Utc::now().timestamp_millis());
            admitted
        },
    );
    if admitted && outcome != SendIfOutcome::Enqueued {
        message_bus.receive_gain.ptt_command_consumed();
    }
    if state {
        if outcome == SendIfOutcome::Enqueued {
            message_bus.gain_tx_gate.fetch_or(2, Ordering::AcqRel);
            message_bus.gain_tx_epoch.fetch_add(1, Ordering::AcqRel);
        }
        // Bits 2+ count synchronous enqueue attempts, bit 1 persists until
        // Hamlib acknowledges PTT-off, and bit 0 belongs to gain transitions.
        message_bus.gain_tx_gate.fetch_sub(4, Ordering::AcqRel);
    }
    outcome
}

/// PAN-202 test seam: when armed, parks the worker once between its last
/// pre-PTT gate and the key-down, so a test can hold the bus table across
/// that instant. Unarmed it returns without yielding.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct KeyDownProbe {
    pub(crate) armed: std::sync::atomic::AtomicBool,
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(test)]
impl KeyDownProbe {
    async fn pause_if_armed(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

/// One-time snapshot of the remote-TX arm's permission state, taken under a
/// single lock acquisition, so a BATCH of entries (e.g. a coalesced
/// backlog) can be evaluated against one consistent view instead of each
/// entry re-locking `ArmState` separately (round-3 review, Codex P1).
/// Per-entry re-locking left a TOCTOU window: if control transfers A→B→A
/// between two entries' individual lock acquisitions within the same
/// batch, both entries can pass their own check yet the batch's later
/// single-identity fold only ever records one of them, silently
/// authorizing the other under a mismatched identity.
#[derive(Clone)]
struct ArmSnapshot {
    /// The boolean arm gate for a station-agent-originated frame (`Some`
    /// client identity) — reads the station-agent channel's consent.
    station_agent_permitted: bool,
    /// The boolean arm gate for a WSJT-X-UDP-originated frame (`None` client
    /// identity) — reads the WSJT-X UDP channel's consent.
    wsjtx_permitted: bool,
    armed_client_key_id: Option<String>,
    /// Round-10 review (Codex P2): captured under the SAME lock as the
    /// other two fields, so audit attribution for a denial this snapshot
    /// decided can use this frozen value instead of re-locking the LIVE
    /// `ArmState` later — the denial reporting this snapshot feeds is now
    /// a detached `tokio::spawn`'d task, which can run arbitrarily later,
    /// by which point a different operator (or the same one re-arming)
    /// may have taken the arm. Re-locking at that point would attribute a
    /// durable `TxDenied` record to whoever happens to be armed when the
    /// task finally runs, not whoever was (or wasn't) armed when the
    /// denial was actually decided.
    operator_callsign: Option<String>,
    /// The wall-clock instant (unix ms) the permission was decided at, taken
    /// with the lock held — the timestamp for any audit this snapshot feeds.
    decided_at_unix_ms: i64,
}

impl ArmSnapshot {
    /// `now` is sampled only after the lock is acquired (see
    /// [`remote_tx_permitted`]).
    fn take(
        arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
        now: impl FnOnce() -> pancetta_agent::arm::ArmTime,
    ) -> Self {
        match arm.lock() {
            Ok(state) => {
                let now = now();
                Self {
                    station_agent_permitted: state
                        .tx_permitted(now, pancetta_agent::arm::ConsentChannel::StationAgent),
                    wsjtx_permitted: state
                        .tx_permitted(now, pancetta_agent::arm::ConsentChannel::WsjtxUdp),
                    armed_client_key_id: state.armed_client_key_id().map(str::to_string),
                    operator_callsign: state.operator_callsign().map(str::to_string),
                    decided_at_unix_ms: now.unix_ms,
                }
            }
            Err(_) => Self {
                station_agent_permitted: false,
                wsjtx_permitted: false,
                armed_client_key_id: None,
                operator_callsign: None,
                decided_at_unix_ms: now().unix_ms,
            },
        }
    }

    /// The boolean arm gate (no identity binding) for the channel
    /// `remote_client_key_id` originated on — see [`consent_channel_for`].
    fn channel_permitted(&self, remote_client_key_id: Option<&str>) -> bool {
        match consent_channel_for(remote_client_key_id) {
            pancetta_agent::arm::ConsentChannel::StationAgent => self.station_agent_permitted,
            pancetta_agent::arm::ConsentChannel::WsjtxUdp => self.wsjtx_permitted,
        }
    }

    /// Mirrors [`remote_tx_permitted_for`]'s logic against this frozen
    /// snapshot instead of a fresh lock.
    fn permits(&self, remote_client_key_id: Option<&str>) -> bool {
        if !self.channel_permitted(remote_client_key_id) {
            return false;
        }
        match remote_client_key_id {
            None => true,
            Some(id) => self.armed_client_key_id.as_deref() == Some(id),
        }
    }

    /// Round-3 review (Codex P2): true only when the frame would otherwise
    /// be permitted (arm is up, heartbeat/TTL/consent/kill all clear) but
    /// is bound to a DIFFERENT client than the one currently armed — as
    /// opposed to every other denial cause (nobody armed, heartbeat lost,
    /// TTL expired, local consent/kill), which is not an identity problem
    /// at all and must not be reported as one.
    fn is_identity_mismatch(&self, remote_client_key_id: Option<&str>) -> bool {
        self.channel_permitted(remote_client_key_id)
            && remote_client_key_id
                .is_some_and(|id| self.armed_client_key_id.as_deref() != Some(id))
    }

    /// Round-10 review (Codex P2): mirrors [`tx_denied_operator_attribution`]'s
    /// logic against this frozen snapshot instead of a fresh lock — see
    /// this struct's `operator_callsign` field doc for why that matters
    /// for detached denial reporting.
    ///
    /// Round-13 review (Codex P1): deliberately does NOT reuse
    /// `is_identity_mismatch` — that predicate is gated on `tx_permitted`
    /// (it exists to pick DENIAL-REASON WORDING: don't say "bound to a
    /// different client" when the real cause is heartbeat/TTL/kill).
    /// Attribution is a different question — "did A's frame get denied
    /// while it's actually B who is bound here" — and must hold
    /// independent of WHY the frame was denied: if nobody is currently
    /// armed AND the frame is bound to a specific client, that client's
    /// identity still differs from "nobody", so this must still refuse to
    /// attribute the denial to whoever the (unrelated) armed_client_key_id
    /// happens to be.
    fn operator_attribution(&self, remote_client_key_id: Option<&str>) -> Option<String> {
        let identity_differs =
            remote_client_key_id.is_some_and(|id| self.armed_client_key_id.as_deref() != Some(id));
        if identity_differs {
            None
        } else {
            self.operator_callsign.clone()
        }
    }
}

/// Attribute a `TxDenied` audit record's `operator_callsign`, never to the
/// CURRENTLY-armed operator when the denied frame is bound to a DIFFERENT
/// client (PAN-91 identity mismatch) — that would misattribute an
/// unauthorized request as if it came from whoever else happens to be
/// armed, which is actively misleading in a durable security audit record.
/// Falls back to the pre-PAN-91 attribution (whoever is armed, if anyone)
/// for every other denial reason (not armed, heartbeat lost, TTL expired,
/// local consent/kill) and for every frame with no bound identity at all.
fn tx_denied_operator_attribution(
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    remote_client_key_id: Option<&str>,
) -> Option<String> {
    remote_tx_arm.lock().ok().and_then(|s| {
        let mismatched_identity =
            remote_client_key_id.is_some_and(|id| s.armed_client_key_id() != Some(id));
        if mismatched_identity {
            None
        } else {
            s.operator_callsign().map(str::to_string)
        }
    })
}

/// PAN-92: shared diagnostic/audit/relay for a Remote transmission stopped
/// by an arm re-check — mirrors the Step 0a pickup-time drop's dispensa
/// Q-0051 Phase A/B/C signals (TUI diagnostic, `AuditKind::TxDenied`, and a
/// client-visible `error` event) so the interruption is exactly as visible
/// as a pickup-time one, whether it happens mid-flight
/// ([`SleepOutcome::AbortedByDisarm`]) or at the last-instant pre-PTT
/// recheck (round-2 review: the arm can go stale in the gap between
/// Step 4b-arm and the actual `SetPtt` send).
///
/// `diagnostic_verb` and `denial_reason` are caller-supplied rather than
/// built in here, since the wording differs by call site ("interrupted" for
/// a transmission already on the air vs. "denied" for one PTT hasn't
/// asserted for yet — reusing "disarmed mid-transmission" verbatim for the
/// latter would be factually wrong).
///
/// PAN-91 review follow-up: for an in-flight interruption, the CALLER must
/// send PTT off and disarm its `PttGuard` BEFORE calling this — every step
/// here awaits (diagnostic, synchronous audit-log I/O, relay), and delaying
/// PTT release behind that work leaves a disarmed remote transmission keyed
/// longer than necessary. Not applicable to a pre-PTT denial (PTT was never
/// asserted).
#[allow(clippy::too_many_arguments)]
async fn emit_disarm_interrupt_signals(
    message_bus: &MessageBus,
    audit_log: &pancetta_agent::audit::AuditLog,
    display_feed_enabled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Round-12 review (Codex P2): the CALLER captures this at the denial
    // check itself — before this function's own `emit_diagnostic` await —
    // rather than this function re-locking the live `ArmState` after that
    // await. If the arm is absent/expired at the denial point and a
    // client then arms (or takes control) while `emit_diagnostic` is
    // pending, re-locking here would attribute the durable `TxDenied`
    // record to that LATER arm state, not the one that actually rejected
    // the frame. Mirrors `ArmSnapshot::operator_callsign`'s identical
    // reasoning for the coalescer's own (already detached) denial path.
    operator_callsign: Option<String>,
    diagnostic_verb: &str,
    denial_reason: String,
    qso_id: Option<&str>,
) {
    emit_diagnostic(
        message_bus,
        "agent.tx",
        pancetta_core::DiagnosticLevel::Warn,
        format!("Remote TX {diagnostic_verb} ({denial_reason})"),
        qso_id,
    )
    .await;
    audit_log.record(&pancetta_agent::audit::AuditEvent {
        ts_unix_ms: chrono::Utc::now().timestamp_millis(),
        kind: pancetta_agent::audit::AuditKind::TxDenied,
        operator_callsign,
        detail: denial_reason.clone(),
    });
    super::remote_gateway::relay_to_gateway(
        message_bus,
        display_feed_enabled,
        ComponentId::Ft8Transmitter,
        MessageType::TxDenied {
            reason: denial_reason,
            qso_id: qso_id.map(str::to_string),
        },
    )
    .await;
}

/// Encode a text message to transmission symbols for the active protocol.
///
/// **FT8 is byte-identical to the legacy path**: `Protocol::Ft8` calls the exact
/// `encoder.encode_message(text, None)` the worker always called and returns the
/// 79-symbol array as a `Vec<u8>` (`.to_vec()` — no value change, only the
/// container). FT4/FT2 call the protocol-aware `encode_message_protocol`, which
/// emits the correct symbol count for the mode (FT4 → 105, 4-GFSK). The encoder
/// must have been constructed with the matching protocol (see the worker's
/// `Ft8Encoder::with_protocol` for the non-FT8 case) so `encode_message_protocol`
/// applies the right sync/XOR/Gray mapping.
fn encode_for_protocol(
    encoder: &mut Ft8Encoder,
    protocol: pancetta_ft8::Protocol,
    text: &str,
) -> pancetta_ft8::Ft8Result<Vec<u8>> {
    match protocol {
        pancetta_ft8::Protocol::Ft8 => encoder.encode_message(text, None).map(|s| s.to_vec()),
        _ => encoder.encode_message_protocol(text, None),
    }
}

/// Modulate transmission symbols into audio samples for the active protocol.
///
/// **FT8 is byte-identical to the legacy path**: `Protocol::Ft8` calls
/// `modulator.modulate_symbols(&[u8; NUM_SYMBOLS], offset)` — the same FT8 GFSK
/// shaping the worker always used (the 79-length slice is copied back into the
/// fixed-size array the FT8 entry point requires). FT4/FT2 call
/// `modulate_symbols_protocol(symbols, offset, &params)` with the active
/// protocol's params, which carry the correct on-air shaping (FT4 → `Gfsk { bt:
/// 1.0 }`) and symbol geometry.
fn modulate_for_protocol(
    modulator: &mut Ft8Modulator,
    protocol: pancetta_ft8::Protocol,
    symbols: &[u8],
    offset: f64,
) -> pancetta_ft8::Ft8Result<Vec<f32>> {
    match protocol {
        pancetta_ft8::Protocol::Ft8 => {
            // The FT8 entry point requires the fixed-size array; the symbols came
            // from `encode_message` (exactly NUM_SYMBOLS long) so this conversion
            // never fails on the FT8 path.
            let arr: [u8; pancetta_ft8::NUM_SYMBOLS] =
                symbols
                    .try_into()
                    .map_err(|_| pancetta_ft8::Ft8Error::InvalidDataSize {
                        expected: pancetta_ft8::NUM_SYMBOLS,
                        actual: symbols.len(),
                    })?;
            modulator.modulate_symbols(&arr, offset)
        }
        _ => modulate_for_protocol_params(
            modulator,
            symbols,
            offset,
            &pancetta_ft8::ProtocolParams::from_protocol(protocol),
        ),
    }
}

/// Protocol-aware modulate for the non-FT8 path (small indirection so the branch
/// above and any future caller share one call site).
fn modulate_for_protocol_params(
    modulator: &mut Ft8Modulator,
    symbols: &[u8],
    offset: f64,
    params: &pancetta_ft8::ProtocolParams,
) -> pancetta_ft8::Ft8Result<Vec<f32>> {
    modulator.modulate_symbols_protocol(symbols, offset, params)
}

/// Full encode → modulate for one message under the active protocol, returning
/// the audio samples. Constructs a fresh encoder/modulator for the protocol so
/// it is self-contained and unit-testable. `base_offset` is passed straight to
/// the modulator's `frequency_offset` (the worker instead pre-sets the base
/// frequency and passes 0.0; both paths land at the same audio frequency).
///
/// **FT8 regression guarantee:** for `Protocol::Ft8` this calls the exact same
/// `Ft8Encoder::new()` / `Ft8Modulator::new_default()` / `encode_message` /
/// `modulate_symbols` sequence the worker used before the FT4 wiring, so its
/// output is byte-identical to the legacy path for a given message and offset.
#[cfg_attr(not(test), allow(dead_code))]
fn encode_and_modulate(
    protocol: pancetta_ft8::Protocol,
    text: &str,
    base_offset: f64,
) -> pancetta_ft8::Ft8Result<Vec<f32>> {
    let mut encoder = match protocol {
        pancetta_ft8::Protocol::Ft8 => Ft8Encoder::new(),
        _ => Ft8Encoder::with_protocol(pancetta_ft8::ProtocolParams::from_protocol(protocol)),
    };
    let mut modulator = Ft8Modulator::new_default()?;
    let symbols = encode_for_protocol(&mut encoder, protocol, text)?;
    modulate_for_protocol(&mut modulator, protocol, &symbols, base_offset)
}

/// Upper bound on the number of distinct TX streams the worker will retain
/// when coalescing a backlog. Mirrors the "max simultaneous TX in one slot"
/// ceiling: a single FT8 slot can only carry a handful of summed signals
/// cleanly, so retaining more than this serves no purpose and only risks
/// over-summing the waveform. There is no shared multi-TX cap constant in the
/// TX-worker scope (the QSO engine's `max_concurrent_qsos` lives in config and
/// is out of bounds here), so we pick a small, safe constant.
const MAX_RETAINED_TX_STREAMS: usize = 8;

/// One drained `TransmitRequest`, reduced to the fields coalescing needs.
/// Pulled out of `MessageType::TransmitRequest` so the coalesce logic is a
/// pure function testable without the message bus.
#[derive(Debug, Clone, PartialEq)]
pub struct CoalesceEntry {
    pub message_text: String,
    pub frequency_offset: f64,
    pub qso_id: Option<String>,
    pub tx_parity: Option<pancetta_core::slot::SlotParity>,
    /// Origin of the drained request (`Local`/`Remote`). Threaded through the
    /// coalescer so a folded bundle preserves it — if ANY folded entry is
    /// `Remote`, the emitted request/bundle is `Remote` (the arm gate applies
    /// to the whole bundle; fail-safe). Defaults to `Local`.
    pub origin: crate::message_bus::TxOrigin,
    /// The client this entry's request is bound to, iff `origin == Remote`
    /// (PAN-91). Threaded through the coalescer alongside `origin` — see
    /// `MessageType::TransmitRequest::remote_client_key_id`.
    pub remote_client_key_id: Option<String>,
    /// The entry's own ledger attempt (PAN-183), kept on every request or
    /// bundle item rebuilt from it.
    pub attempt_id: Option<pancetta_qso::TxAttemptId>,
}

/// Result of draining + coalescing a backlog of `TransmitRequest`s.
#[derive(Debug, Default, PartialEq)]
pub struct CoalesceOutcome {
    /// The requests to actually transmit, after coalescing per `qso_id`
    /// (newest wins), dropping terminal-QSO requests, and capping the
    /// distinct-stream count. QSO-keyed streams Fox mode opened come first
    /// (PAN-76, see [`coalesce_transmit_requests_with_fox_priority`]), then
    /// the other QSO-keyed streams, then manual sends; each group keeps the
    /// order its keys were first seen, so with no Fox-admitted stream present
    /// the head entry is the oldest-surviving stream.
    pub retained: Vec<CoalesceEntry>,
    /// How many requests were superseded by a newer request for the SAME
    /// `qso_id` (i.e. older keep-call frames dropped in favor of the latest).
    pub coalesced: usize,
    /// The entries counted in `coalesced` (PAN-279). The caller rolls back
    /// each one's own ledger attempt; its QSO's surviving entry reports to
    /// Autonomous.
    pub coalesced_entries: Vec<CoalesceEntry>,
    /// How many requests were dropped because their `qso_id` is no longer in
    /// the active set (terminal / cancelled / completed-past-grace QSO).
    pub dropped_terminal: usize,
    /// The entries counted in `dropped_terminal` (PAN-185). The caller
    /// reports each to Autonomous as a failed completion: a pivot can leave
    /// a QSO's queued request as the one that owns a tracked self-CQ's
    /// outcome, and dropping it silently would leave that self-CQ pending.
    pub dropped_terminal_entries: Vec<CoalesceEntry>,
    /// How many distinct streams were dropped because the retained set hit
    /// either the operator-configured concurrent-QSO cap
    /// (`max_concurrent_qsos`, QSO-keyed streams only) or the hard
    /// [`MAX_RETAINED_TX_STREAMS`] backstop (silent truncation made visible).
    pub truncated: usize,
    /// The actual entries dropped by either truncation cap counted in
    /// `truncated` above — unlike `dropped_terminal` (a QSO that already
    /// ended, nothing to roll back) or `parity_excluded`/`freq_excluded`
    /// (temporary, the QSO's own cadence retries it next cycle), a truncated
    /// stream represents an ADMITTED, still-live QSO whose call-bookkeeping
    /// (call_count, last_call_at, Sent timeline) already advanced as if this
    /// frame would transmit. The caller must report a failed
    /// completion through `report_transmit_complete`, which sends
    /// `TxFrameNotSent` to `QsoManager::roll_back_unsent_attempt`, so it does not drift
    /// from reality (a stream that's always last in FIFO order would
    /// otherwise burn through its retry budget or time out having never
    /// actually gone on air).
    pub truncated_entries: Vec<CoalesceEntry>,
    /// The effective concurrent-QSO cap actually applied to QSO-keyed
    /// streams this cycle (`max_concurrent_qsos`, raised to the number of
    /// Fox-admitted streams present in this backlog (PAN-76), clamped into
    /// `1..=MAX_RETAINED_TX_STREAMS`) — exposed so a truncation diagnostic
    /// can report the cap that actually did the truncating instead of always
    /// citing the (possibly much larger) hard backstop.
    pub qso_cap: usize,
    /// How many retained streams were excluded from THIS cycle's bundle
    /// because their concrete `tx_parity` disagreed with the bundle anchor's
    /// (the first-seen retained stream's parity — see
    /// [`coalesce_transmit_requests`]'s parity-conflict check). Folding a
    /// disagreeing stream into the bundle would silently put it on the wrong
    /// slot window, which is exactly the "every concurrent active QSO
    /// transmits on the same parity" invariant this excludes to protect.
    /// Excluded streams are NOT retained here, but they are not dropped
    /// permanently either — the QSO's own TX cadence (keep-call/rearm) will
    /// produce a fresh request for it next slot, sent individually on its own
    /// parity.
    pub parity_excluded: usize,
    /// FQ-F4/TX-F6 defense-in-depth: how many retained streams were excluded
    /// from THIS cycle's bundle because their `frequency_offset` fell within
    /// [`pancetta_qso::MIN_TX_SEPARATION_HZ`] of an already-retained stream.
    /// `modulate_multi_tx`'s pairwise separation check (bandwidth + 25 Hz
    /// guard) fails the ENTIRE bundle — not just the colliding pair — if two
    /// folded streams are too close, so this excludes the later one instead
    /// of letting the whole bundle's TX silently vanish. Fix 5's own
    /// de-confliction at QSO-open time (`compute_manual_tx_offset` in
    /// `coordinator/qso.rs`) should make this rare in practice; this is
    /// cheap insurance against any other path that still produces
    /// close-together streams. Same "exclude, don't coerce, it retries
    /// individually next cycle" semantics as `parity_excluded` above.
    pub freq_excluded: usize,
    /// The entries counted in `parity_excluded` and `freq_excluded`
    /// (PAN-185). Reported to Autonomous only, like `dropped_terminal_entries`;
    /// their ledger accounting is unchanged.
    pub excluded_entries: Vec<CoalesceEntry>,
}

impl CoalesceOutcome {
    /// `true` when nothing was reduced — a single request (or a backlog that
    /// happened to be one fresh frame per distinct live QSO with no overflow).
    /// Used only for the log-suppression decision in the worker.
    fn is_noop(&self) -> bool {
        self.coalesced == 0
            && self.dropped_terminal == 0
            && self.truncated == 0
            && self.parity_excluded == 0
            && self.freq_excluded == 0
    }
}

/// The effective concurrent-QSO ceiling for THIS cycle: the operator's
/// configured `autonomous.max_concurrent_qsos`, raised to Fox mode's own
/// `fox_max_streams + 1` (the `+1` for Fox's independent CQ stream, which
/// occupies a slot alongside — not carved out of — the Hound-answer count;
/// see `maybe_answer_caller`'s "Capacity bound" comment in `coordinator/qso.rs`)
/// whenever Fox mode is active.
///
/// Both `fox_mode` and `fox_max_streams` are live `Atomic`s the operator/
/// engine can flip at runtime, so this must be recomputed per cycle rather
/// than folded into the `max_concurrent_qsos` snapshot the worker captures
/// once at startup — Fox mode staying capped at the normal single/dual-QSO
/// limit would truncate its own intended multi-stream Hound-answer bundle
/// down to one signal.
///
/// Hound exchanges still running after `SetFoxMode(false)` (PAN-76): a floor
/// at `active_tx_qsos.len()` was tried and reverted in PR #348 rounds 4-5,
/// because `active_tx_qsos` cannot tell a Fox-admitted Hound from an
/// ordinary over-cap manual QSO, so that floor let a manual pileup through.
/// The QSO component now records the QSOs Fox mode opened in the
/// coordinator's `fox_admitted_qsos`. The backlog coalescer gets this
/// UNRAISED value plus that set, and
/// [`coalesce_transmit_requests_with_fox_priority`] raises its cap only to
/// the Fox-admitted streams actually present in the backlog — never the
/// set's size, which would lend the slots of Hounds idling in their 45 s
/// completed grace to ordinary QSOs. The mid-TX supersede sites use
/// [`supersede_max_concurrent_qsos`] over [`live_fox_admitted_count`].
pub(crate) fn effective_max_concurrent_qsos(
    max_concurrent_qsos: u32,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
) -> u32 {
    if fox_mode.load(Ordering::Relaxed) {
        let fox_total = fox_max_streams.load(Ordering::Relaxed) as u32 + 1;
        max_concurrent_qsos.max(fox_total)
    } else {
        max_concurrent_qsos
    }
}

/// PAN-76: Fox-admitted QSOs that are still TX-live (in `active_tx_qsos`).
/// Either lock poisoned → 0, i.e. no raise over today's ceiling.
pub(crate) fn live_fox_admitted_count(
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
    active_tx_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> u32 {
    let (Ok(fox), Ok(active)) = (fox_admitted_qsos.read(), active_tx_qsos.read()) else {
        return 0;
    };
    fox.iter().filter(|k| active.contains(*k)).count() as u32
}

/// PAN-76: ceiling for the mid-TX supersede paths: the live Fox-admitted
/// streams on top of the normal cap, the same shape as the coalescer's rule
/// (Fox-admitted streams are never truncated; ordinary ones are capped). A
/// mixed in-flight bundle of Hounds plus ordinary QSOs therefore stays under
/// it, so a Hound's fresh frame folds in instead of collapsing the bundle
/// (PR #421 review). A supersede never adds a QSO stream
/// (`classify_incoming_during_tx` only supersedes on an in-flight QSO or a
/// manual send), so this never admits more than the coalescer already did.
pub(crate) fn supersede_max_concurrent_qsos(effective: u32, live_fox_admitted: u32) -> u32 {
    effective.saturating_add(live_fox_admitted)
}

#[cfg(test)]
#[path = "tx/tests/effective_max_concurrent_qsos_tests.rs"]
mod effective_max_concurrent_qsos_tests;

#[cfg(test)]
#[path = "tx/tests/fox_admitted_ceiling_tests.rs"]
mod fox_admitted_ceiling_tests;

/// Pure backlog coalescer for the single-threaded TX worker.
///
/// Given the requests drained from the channel (oldest first — the channel is
/// FIFO) and a predicate for whether a `qso_id` is still live, collapse the
/// backlog to "current intent":
///
/// 1. **Coalesce per `qso_id`, newest wins.** Two requests sharing a non-`None`
///    `qso_id` are the same stream's keep-call cadence; only the latest matters
///    (a newer keep-call frame supersedes the older). The older one is counted
///    in `coalesced` and discarded.
/// 2. **Never coalesce manual / free-text / tune sends.** A request with
///    `qso_id == None` is its own non-coalescable stream — every such entry is
///    retained verbatim, so a flood of keep-calls can never swallow an
///    operator's manual send.
/// 3. **Drop terminal QSOs.** A request whose `qso_id` is no longer live (same
///    predicate Step 4b uses) is dropped during the drain and counted in
///    `dropped_terminal`. `None`-keyed requests are never gated.
/// 4. **Bound the retained set.** At most [`MAX_RETAINED_TX_STREAMS`] distinct
///    streams survive; the rest are counted in `truncated` and dropped. The
///    earliest-seen streams are kept (FIFO fairness).
/// 5. **Never coerce a bundle-parity conflict.** When ≥2 distinct streams
///    survive, the caller folds them into one `MultiTransmitRequest` stamped
///    with a single `tx_parity` (the first-seen/oldest-retained stream's — the
///    "bundle anchor"). Any later stream whose OWN concrete `tx_parity`
///    disagrees with the anchor is excluded here (counted in
///    `parity_excluded`) rather than silently forced onto the anchor's
///    parity — every concurrent active QSO must transmit on the same parity
///    (see CLAUDE.md), and coercing would put the disagreeing stream in the
///    wrong slot window. Excluded streams are not gone permanently: the QSO's
///    own cadence produces a fresh request for it next slot.
///
/// The single-request, no-backlog case returns `retained == [that request]`
/// with all counters zero, so the worker's normal path is unchanged.
///
/// `max_concurrent_qsos` bounds the number of distinct QSO-keyed streams
/// (never manual/`None`-keyed sends, which aren't concurrent QSOs) folded
/// into this cycle's bundle — the same operator-configured limit the mid-TX
/// supersede path (`supersede_and_rekey_or_bundle`) enforces. Without this,
/// several manual QSOs opened in the same pre-PTT window bundled unconditionally
/// up to [`MAX_RETAINED_TX_STREAMS`], silently exceeding the configured cap.
///
/// Equivalent to [`coalesce_transmit_requests_with_fox_priority`] with no
/// Fox-admitted QSOs.
pub fn coalesce_transmit_requests(
    drained: Vec<CoalesceEntry>,
    max_concurrent_qsos: u32,
    qso_is_live: impl FnMut(Option<&str>) -> bool,
) -> CoalesceOutcome {
    coalesce_transmit_requests_with_fox_priority(drained, max_concurrent_qsos, qso_is_live, |_| {
        false
    })
}

/// [`coalesce_transmit_requests`] plus one rule for QSOs Fox mode opened:
///
/// 6. **Fox-admitted streams go first and are never cap-truncated (PAN-76).**
///    `is_fox_admitted` receives the canonical [`super::active_tx_qso_key`]
///    of each QSO-keyed stream. Fox-admitted streams are ordered before the
///    other QSO-keyed streams (each group keeps first-seen order), so they
///    anchor the bundle parity and win frequency-conflict tie-breaks, and the
///    QSO cap is raised to the number of Fox-admitted streams that survive
///    into this cycle's bundle. Only ordinary streams are truncated, and only
///    to the room `max_concurrent_qsos` leaves. A Fox-admitted QSO with no
///    frame in this backlog (e.g. a completed Hound in its grace window)
///    lends no slot to anyone. With no Fox-admitted stream present the
///    outcome is identical to [`coalesce_transmit_requests`].
pub fn coalesce_transmit_requests_with_fox_priority(
    drained: Vec<CoalesceEntry>,
    max_concurrent_qsos: u32,
    mut qso_is_live: impl FnMut(Option<&str>) -> bool,
    mut is_fox_admitted: impl FnMut(&str) -> bool,
) -> CoalesceOutcome {
    use std::collections::HashMap;

    let mut outcome = CoalesceOutcome::default();
    // Insertion-ordered map keyed by qso_id (uppercased to match the
    // active-set canonicalization). `None`-keyed entries bypass the map and go
    // straight to `manual` so they're never coalesced.
    let mut order: Vec<String> = Vec::new();
    let mut by_qso: HashMap<String, CoalesceEntry> = HashMap::new();
    // Retained manual/None entries, kept in drain order.
    let mut manual: Vec<CoalesceEntry> = Vec::new();

    for entry in drained {
        // Drop terminal-QSO requests (None is never gated).
        if !qso_is_live(entry.qso_id.as_deref()) {
            outcome.dropped_terminal += 1;
            outcome.dropped_terminal_entries.push(entry);
            continue;
        }
        match entry.qso_id.as_deref() {
            None => manual.push(entry),
            Some(id) => {
                let key = super::active_tx_qso_key(id);
                if let Some(older) = by_qso.insert(key.clone(), entry) {
                    // Superseded an older frame for the same QSO.
                    outcome.coalesced += 1;
                    outcome.coalesced_entries.push(older);
                } else {
                    order.push(key);
                }
            }
        }
    }

    // Assemble QSO-keyed streams (first-seen order) followed by manual sends
    // — UNCAPPED for now. The concurrent-QSO cap is applied further below,
    // AFTER the parity/frequency conflict passes: capping first can truncate
    // away a stream that would have survived while keeping one that gets
    // excluded moments later for a conflict, missing an otherwise-valid
    // bundle combination entirely (e.g. cap=2 with streams A/B/C where B
    // conflicts with A — capping first keeps [A, B] then drops B to a
    // conflict, leaving just A; filtering conflicts first correctly leaves
    // [A, C] as the surviving two-stream bundle). Manual sends go last so a
    // single-stream QSO backlog keeps the QSO as the headline item.
    // Fox-admitted streams go before the other QSO-keyed streams (PAN-76) so
    // the cap below truncates only ordinary streams and a Hound anchors the
    // bundle parity whenever one is present.
    let mut retained: Vec<CoalesceEntry> = Vec::with_capacity(order.len() + manual.len());
    let mut ordinary: Vec<CoalesceEntry> = Vec::new();
    for key in order {
        if let Some(e) = by_qso.remove(&key) {
            if is_fox_admitted(&key) {
                retained.push(e);
            } else {
                ordinary.push(e);
            }
        }
    }
    retained.append(&mut ordinary);
    retained.append(&mut manual);

    // Bundle-parity conflict check. When ≥2 distinct streams survive, they may
    // get folded into a single `MultiTransmitRequest` by the caller, stamped
    // with ONE `tx_parity` (the first-seen/oldest-retained stream's — the
    // "bundle anchor"). A later stream whose OWN concrete `tx_parity` disagrees
    // with the anchor must never silently ride that bundle: every concurrent
    // active QSO must transmit on the same parity (CLAUDE.md invariant), and
    // coercing a disagreeing stream onto the bundle's parity would put it in
    // the wrong slot window for its actual partner. `None` (no preference) is
    // NOT a disagreement — only two concrete, differing `Some` values are a
    // genuine conflict. Excluded streams are dropped from `retained` and
    // reported to the QSO engine as not sent (PAN-279): the rollback returns
    // a keep-call frame's call to the budget, so the engine's rearm can
    // re-emit it on its own parity. They are not requeued: a requeued entry
    // would anchor the next bundle on the other parity.
    if retained.len() > 1 {
        let anchor = retained[0].tx_parity;
        let mut kept = Vec::with_capacity(retained.len());
        for (idx, entry) in retained.into_iter().enumerate() {
            let disagrees = match (anchor, entry.tx_parity) {
                (Some(a), Some(p)) => a != p,
                _ => false,
            };
            if idx > 0 && disagrees {
                warn!(
                    target: "pancetta::tx.policy",
                    "TX bundle parity conflict: stream qso_id={:?} requested parity {:?} \
                     but this cycle's bundle is anchored to {:?} (first-seen stream); \
                     excluding it from this bundle and reporting it to the QSO engine as not sent",
                    entry.qso_id, entry.tx_parity, anchor
                );
                outcome.parity_excluded += 1;
                outcome.excluded_entries.push(entry);
            } else {
                kept.push(entry);
            }
        }
        retained = kept;
    }

    // Split QSO-keyed streams from manual/tune sends for the remaining
    // passes. The frequency-conflict check and the concurrent-QSO cap must
    // be resolved between QSO streams FIRST, entirely independent of manual
    // sends — otherwise a manual stream can be wrongly excluded for
    // conflicting with a QSO stream that turns out to be cap-truncated
    // anyway (PR #348 review round 2: cap=1, QSO A/B where B conflicts with
    // a manual send — checking the manual send against B before the cap
    // decides B is truncated wrongly sacrifices a manual send that was
    // actually compatible with the surviving QSO set). `partition`
    // preserves each side's relative (first-seen / drain) order.
    let (qso_entries, manual_entries): (Vec<CoalesceEntry>, Vec<CoalesceEntry>) =
        retained.into_iter().partition(|e| e.qso_id.is_some());

    // FQ-F4/TX-F6 defense-in-depth: pairwise frequency-separation check among
    // QSO-keyed streams only. `modulate_multi_tx` fails the WHOLE bundle (not
    // just the colliding pair) if any two folded streams' `frequency_offset`s
    // are closer than its minimum separation (signal bandwidth + 25 Hz
    // guard). Fix 5's own de-confliction at QSO-open time should make a
    // collision here rare, but this is cheap insurance against any other
    // path that still produces close-together streams. Greedy,
    // order-preserving: the earliest-seen (already-kept) stream at a given
    // offset wins; a later stream too close to ANY already-kept stream is
    // excluded from this cycle's bundle, not coerced onto a different offset.
    // It is reported to the QSO engine as not sent (PAN-279): the rollback
    // returns a keep-call frame's call to the budget, so the engine's rearm
    // can re-emit it.
    let mut qso_survivors: Vec<CoalesceEntry> = Vec::with_capacity(qso_entries.len());
    for entry in qso_entries {
        let too_close = qso_survivors.iter().any(|k: &CoalesceEntry| {
            (k.frequency_offset - entry.frequency_offset).abs() < pancetta_qso::MIN_TX_SEPARATION_HZ
        });
        if too_close {
            warn!(
                target: "pancetta::tx.policy",
                "TX bundle frequency conflict: stream qso_id={:?} at {:.0} Hz is within \
                 {:.0} Hz of an already-retained stream this cycle; excluding it from \
                 this bundle and reporting it to the QSO engine as not sent",
                entry.qso_id, entry.frequency_offset, pancetta_qso::MIN_TX_SEPARATION_HZ
            );
            outcome.freq_excluded += 1;
            outcome.excluded_entries.push(entry);
        } else {
            qso_survivors.push(entry);
        }
    }

    // Enforce the operator's configured concurrent-QSO cap on the QSO-keyed
    // survivors — now that QSO-vs-QSO conflicts are already resolved, cap
    // truncation only ever drops a genuinely bundleable stream. A truncated
    // entry represents an ADMITTED, still-live QSO whose call-bookkeeping
    // has already advanced as if this frame would transmit, so its
    // `CoalesceEntry` is retained in `truncated_entries` for the caller to
    // use `report_transmit_complete`, which routes `TxFrameNotSent` to
    // `QsoManager::roll_back_unsent_attempt` (PAN-75).
    // PAN-76: raise the cap to the Fox-admitted streams present — they sit at
    // the front, so `split_off` only ever truncates ordinary streams.
    let fox_present = qso_survivors
        .iter()
        .filter(|e| {
            e.qso_id
                .as_deref()
                .is_some_and(|id| is_fox_admitted(&super::active_tx_qso_key(id)))
        })
        .count();
    let qso_cap = (max_concurrent_qsos.max(1) as usize)
        .max(fox_present)
        .min(MAX_RETAINED_TX_STREAMS);
    outcome.qso_cap = qso_cap;
    if qso_survivors.len() > qso_cap {
        let overflow = qso_survivors.split_off(qso_cap);
        outcome.truncated += overflow.len();
        outcome.truncated_entries.extend(overflow);
    }

    // Check manual/tune sends against the FINAL surviving QSO set (post
    // conflict-filter, post-cap) plus each other — never against a QSO
    // stream that was excluded or truncated above. Manual sends are never
    // subject to the concurrent-QSO cap themselves.
    let mut kept: Vec<CoalesceEntry> = qso_survivors;
    for entry in manual_entries {
        let too_close = kept.iter().any(|k: &CoalesceEntry| {
            (k.frequency_offset - entry.frequency_offset).abs() < pancetta_qso::MIN_TX_SEPARATION_HZ
        });
        if too_close {
            warn!(
                target: "pancetta::tx.policy",
                "TX bundle frequency conflict: stream qso_id={:?} at {:.0} Hz is within \
                 {:.0} Hz of an already-retained stream this cycle; excluding it from \
                 this bundle and reporting it to the QSO engine as not sent",
                entry.qso_id, entry.frequency_offset, pancetta_qso::MIN_TX_SEPARATION_HZ
            );
            outcome.freq_excluded += 1;
            outcome.excluded_entries.push(entry);
        } else {
            kept.push(entry);
        }
    }
    let mut retained = kept;

    // Final hard safety cap across everything retained (QSO streams + manual
    // sends together), independent of the concurrent-QSO setting.
    if retained.len() > MAX_RETAINED_TX_STREAMS {
        let overflow = retained.split_off(MAX_RETAINED_TX_STREAMS);
        outcome.truncated += overflow.len();
        outcome.truncated_entries.extend(overflow);
    }

    outcome.retained = retained;
    outcome
}

/// The frame a coalescer entry stands for (PAN-279).
fn coalesce_entry_frame(entry: &CoalesceEntry) -> TxFrame {
    TxFrame {
        qso_id: entry.qso_id.clone(),
        message_text: entry.message_text.clone(),
        attempt_id: entry.attempt_id,
    }
}

/// Drain the queued backlog behind a head `TransmitRequest` and collapse it to
/// current intent, returning the `MessageType` the worker should actually
/// process this cycle.
///
/// `head` MUST be a `MessageType::TransmitRequest` (the caller checks). The
/// channel is drained non-blockingly: every additional `TransmitRequest` is
/// folded into the coalesce buffer; the FIRST non-`TransmitRequest`
/// (`MultiTransmitRequest` / `TuneRequest` / anything else) stops the drain and
/// is put back at the front of the worker's inbox so it is never reordered
/// relative to other non-TX messages, coalesced, or dropped.
///
/// Returns:
/// - the original single `TransmitRequest` when nothing was queued (normal,
///   no-backlog path — byte-for-byte unchanged),
/// - a single `TransmitRequest` carrying the freshest retained frame when the
///   backlog collapsed to one distinct live stream,
/// - a `MultiTransmitRequest` folding the freshest frame of each distinct live
///   stream when several survived (reuses the existing multi-TX path).
///
/// A `tx.policy` warning is logged whenever anything was coalesced, dropped, or
/// truncated, so silent backlog reduction is always operator-visible.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn coalesce_backlog_into(
    head: MessageType,
    inbox: &TxInbox,
    message_bus: &MessageBus,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    fox_admitted_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    max_concurrent_qsos: u32,
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    audit_log: &pancetta_agent::audit::AuditLog,
    display_feed_enabled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    pivots: Option<(
        &mut std::collections::HashMap<String, (String, f64)>,
        &mut PivotReplaced,
    )>,
) -> MessageType {
    // Decompose the head into a CoalesceEntry. (Caller guarantees the variant.)
    let head_entry = match head {
        MessageType::TransmitRequest {
            attempt_id,
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
            remote_client_key_id,
        } => CoalesceEntry {
            attempt_id,
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
            remote_client_key_id,
        },
        // Defensive: not a TransmitRequest — hand it back unchanged.
        other => return other,
    };

    // Drain queued TransmitRequests behind the head; stop at the first
    // non-TransmitRequest and put it back so it is processed next cycle.
    let mut drained = vec![head_entry];
    while let Ok(msg) = inbox.try_recv() {
        match msg.message_type {
            MessageType::TransmitRequest {
                attempt_id,
                message_text,
                frequency_offset,
                qso_id,
                tx_parity,
                origin,
                remote_client_key_id,
            } => {
                drained.push(CoalesceEntry {
                    attempt_id,
                    message_text,
                    frequency_offset,
                    qso_id,
                    tx_parity,
                    origin,
                    remote_client_key_id,
                });
            }
            _ => {
                // Non-TX message: put it back at the inbox front and stop
                // draining, so Tune/Multi keeps its place relative to TX
                // intent.
                inbox.requeue_front(vec![msg]);
                break;
            }
        }
    }

    // PAN-91 review follow-up: filter out any drained entry that is
    // Remote-origin and bound to a DIFFERENT client than the one currently
    // armed, BEFORE `coalesce_transmit_requests` applies its cap/dedup
    // selection — NOT after (round 2, Codex P2). Filtering after allowed a
    // stale, no-longer-authorized entry (e.g. from a controller that has
    // since been disarmed) to occupy a cap slot ahead of the currently
    // -authorized entry in FIFO order, only for the identity filter to
    // remove it afterward — discarding the authorized entry for no reason,
    // since the capacity it needed was never really unavailable to it.
    // Filtering here, on the raw drained list, means `coalesce_transmit_requests`
    // only ever competes authorized entries against each other.
    //
    // Without this filter at all, control transferring from client A to
    // client B mid-session (A's still-active QSO keeps rearming while B
    // starts new ones) could put both A-bound and B-bound entries in the
    // same backlog; the bundle fold picks ONE representative identity for
    // the whole bundle, so a B-bound entry landing first while B is armed
    // would silently authorize A's frame too. Filtering per-entry here —
    // using each entry's OWN bound identity, exactly like the TX worker's
    // real per-frame gate — closes that gap.
    // Round-3 review (Codex P1): ONE arm snapshot for the WHOLE partition,
    // not a fresh lock per entry — see [`ArmSnapshot`]'s doc for the TOCTOU
    // this closes.
    // PAN-75 (Codex P2 on PR #418): consume pivot duplicates BEFORE
    // newest-per-QSO coalescing. A request whose payload already aired via an
    // earlier pivot could otherwise be discarded here in favour of a newer
    // same-QSO request, so Step 0-dup never sees it: its tombstone goes stale
    // and the frame the pivot replaced, which never aired, stays counted.
    // The newest entry is left for Step 0-dup when every entry is a duplicate.
    if let Some((pivoted_once, pivoted_from)) = pivots {
        if drained.len() > 1 {
            let mut duplicates = Vec::new();
            let mut kept = Vec::with_capacity(drained.len());
            let last = drained.len() - 1;
            for (i, entry) in drained.into_iter().enumerate() {
                let spare = i < last || !kept.is_empty();
                if spare
                    && super::is_pivot_duplicate(
                        entry.qso_id.as_deref(),
                        &entry.message_text,
                        entry.frequency_offset,
                        pivoted_once,
                    )
                {
                    // Silent to Autonomous: the pivoted frame keyed and
                    // reported (ownership rule: `report_unaired_frame`).
                    let replaced = entry.qso_id.as_deref().and_then(|id| {
                        let key = super::active_tx_qso_key(id);
                        pivoted_once.remove(&key);
                        pivoted_from.remove(&key)
                    });
                    duplicates.push((entry, replaced));
                } else {
                    kept.push(entry);
                }
            }
            drained = kept;
            for (entry, replaced) in duplicates {
                info!(
                    target: "pancetta::tx.policy",
                    "dropping stale TX for '{}' before coalescing — already sent via pivot",
                    entry.message_text
                );
                report_pivot_duplicate_dropped(message_bus, entry.qso_id, replaced).await;
            }
        }
    }

    let arm_snapshot = ArmSnapshot::take(remote_tx_arm, pancetta_agent::arm::ArmTime::now);
    let now_ms = arm_snapshot.decided_at_unix_ms;
    let (admitted, identity_denied): (Vec<_>, Vec<_>) = drained.into_iter().partition(|e| {
        e.origin != crate::message_bus::TxOrigin::Remote
            || arm_snapshot.permits(e.remote_client_key_id.as_deref())
    });
    // (round 2, Codex P2): route through the SAME diagnostic + durable
    // audit + client-visible relay every other arm-gate rejection uses
    // (dispensa Q-0051), not just a log line + TransmitComplete — an
    // attempted cross-client transmission must stay visible in the
    // security audit trail exactly like a pickup-time or mid-flight one.
    //
    // Round-9 review (Codex P2): this reporting is now DETACHED
    // (`tokio::spawn`), not awaited inline — a large backlog left behind
    // by a since-disarmed controller could otherwise force this function
    // to await a diagnostic + synchronous audit-file write + relay + bus
    // send PER denied entry before ever returning the admitted frame to
    // its caller, delaying that frame's own key-time gates by however
    // long the stale backlog happened to be. The admitted result no
    // longer waits on how much (or how slowly) denied traffic there is to
    // report.
    if !identity_denied.is_empty() {
        let message_bus = message_bus.clone();
        let audit_log = audit_log.clone();
        let display_feed_enabled = display_feed_enabled.clone();
        let arm_snapshot = arm_snapshot.clone();
        tokio::spawn(async move {
            for entry in &identity_denied {
                // Round-3 review (Codex P2): only say "bound to a
                // different client" when that's actually why this entry
                // was denied — every other gate failure (nobody armed,
                // heartbeat lost, TTL expired, local consent/kill) is a
                // real safety event but not an identity problem, and
                // reporting it as one obscures what actually happened in
                // a durable security audit record.
                let denial_reason = if arm_snapshot
                    .is_identity_mismatch(entry.remote_client_key_id.as_deref())
                {
                    format!(
                            "bound to a different client than the one currently armed: '{}' at {:.0} Hz",
                            entry.message_text, entry.frequency_offset
                        )
                } else {
                    format!(
                        "remote TX not currently permitted: '{}' at {:.0} Hz",
                        entry.message_text, entry.frequency_offset
                    )
                };
                emit_diagnostic(
                    &message_bus,
                    "agent.tx",
                    pancetta_core::DiagnosticLevel::Warn,
                    format!("Remote TX denied ({denial_reason})"),
                    entry.qso_id.as_deref(),
                )
                .await;
                audit_log.record(&pancetta_agent::audit::AuditEvent {
                    // Round-11 review (Codex P2): the denial-time instant
                    // (the SAME `now_ms` `arm_snapshot` was taken under),
                    // not a fresh timestamp read whenever this detached
                    // task happens to actually run — a task delayed behind
                    // executor load or a large earlier backlog would
                    // otherwise timestamp entries AFTER later, unrelated
                    // arm/disarm events actually occurred, corrupting the
                    // audit trail's own chronological order.
                    ts_unix_ms: now_ms,
                    kind: pancetta_agent::audit::AuditKind::TxDenied,
                    // Round-10 review (Codex P2): attribute from the
                    // FROZEN snapshot, not a fresh lock — see
                    // `ArmSnapshot::operator_callsign`'s doc for why this
                    // detached task must not re-lock the live `ArmState`.
                    operator_callsign: arm_snapshot
                        .operator_attribution(entry.remote_client_key_id.as_deref()),
                    detail: denial_reason.clone(),
                });
                super::remote_gateway::relay_to_gateway(
                    &message_bus,
                    &display_feed_enabled,
                    ComponentId::Ft8Transmitter,
                    MessageType::TxDenied {
                        reason: denial_reason,
                        qso_id: entry.qso_id.clone(),
                    },
                )
                .await;
                if let Err(e) = report_transmit_complete(
                    &message_bus,
                    false,
                    entry.message_text.clone(),
                    0,
                    entry.qso_id.clone(),
                    entry.attempt_id,
                )
                .await
                {
                    warn!(
                        "Failed to send TransmitComplete for an identity-denied stream: {}",
                        e
                    );
                }
            }
        });
    }

    // Every drained entry was identity-denied. Hand back an empty
    // MultiTransmitRequest, matching the existing "every drained request
    // belonged to an ended QSO" empty-backlog outcome below.
    if admitted.is_empty() {
        return MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        };
    }

    // Fast path: only one entry survived identity filtering — nothing left
    // to coalesce.
    if admitted.len() == 1 {
        let e = admitted.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        };
    }

    let backlog_total = admitted.len();
    // PAN-76: snapshot the Fox-admitted set (poisoned → empty, i.e. today's
    // cap) so the coalescer keeps Hound exchanges Fox mode already opened.
    let fox_admitted: std::collections::HashSet<String> = fox_admitted_qsos
        .read()
        .map(|s| s.clone())
        .unwrap_or_default();
    let mut outcome = coalesce_transmit_requests_with_fox_priority(
        admitted,
        max_concurrent_qsos,
        |id| tx_qso_is_live(id, active_tx_qsos),
        |key| fox_admitted.contains(key),
    );

    if !outcome.is_noop() {
        let text = format!(
            "TX backlog coalesced: drained {} request(s) → {} retained; \
             coalesced {} stale (newest-per-QSO wins), dropped {} for ended QSOs, \
             truncated {} over the effective {}-QSO cap (hard backstop {}), excluded {} for a \
             bundle-parity conflict, excluded {} for a bundle-frequency conflict",
            backlog_total,
            outcome.retained.len(),
            outcome.coalesced,
            outcome.dropped_terminal,
            outcome.truncated,
            outcome.qso_cap,
            MAX_RETAINED_TX_STREAMS,
            outcome.parity_excluded,
            outcome.freq_excluded,
        );
        warn!(target: "pancetta::tx.policy", "{}", text);
        emit_diagnostic(
            message_bus,
            "tx.policy",
            pancetta_core::DiagnosticLevel::Warn,
            text,
            None,
        )
        .await;
    }

    // A truncated entry is an ADMITTED, still-live QSO whose call-bookkeeping
    // (call_count, last_call_at, Sent timeline) already advanced as if this
    // frame would transmit. Its settlement mirrors the failure as
    // TxFrameNotSent to QsoManager::roll_back_unsent_attempt (PAN-75).
    for entry in &outcome.truncated_entries {
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }

    // PAN-185: a terminal or parity/frequency-excluded QSO request may be the
    // one that owns a tracked self-CQ's outcome (a refused pivot leaves it as
    // the owner, see `report_unaired_frame`), so Autonomous hears a failed
    // completion for each. PAN-279: the QSO engine also rolls back each one's
    // own attempt, which returns a keep-call frame's call to the budget so the
    // engine's rearm can re-emit it.
    for entry in outcome
        .dropped_terminal_entries
        .iter()
        .chain(&outcome.excluded_entries)
    {
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }

    // PAN-279: a request coalesced into a newer one for the same QSO rolls
    // back its own attempt; the newer one owns the Autonomous outcome. An
    // attempt-less folded frame stays silent: a text match could roll back
    // the survivor's attempt instead.
    let surviving_attempts: std::collections::HashSet<(String, pancetta_qso::TxAttemptId)> =
        outcome
            .retained
            .iter()
            .chain(&outcome.excluded_entries)
            .chain(&outcome.truncated_entries)
            .filter_map(|e| {
                Some((
                    super::active_tx_qso_key(e.qso_id.as_deref()?),
                    e.attempt_id?,
                ))
            })
            .collect();
    for entry in &outcome.coalesced_entries {
        let (Some(qso_id), Some(attempt_id)) = (entry.qso_id.as_deref(), entry.attempt_id) else {
            continue;
        };
        if surviving_attempts.contains(&(super::active_tx_qso_key(qso_id), attempt_id)) {
            continue;
        }
        settle_unaired(
            message_bus,
            coalesce_entry_frame(entry),
            None,
            AutonomousOutcome::Withheld,
        )
        .await;
    }

    // A bundle has only one remote identity and therefore one consent channel.
    // Keep the first retained remote channel and defer the other, preserving
    // its identity for the next worker pass and the final key-time check.
    // Do this after newest-per-QSO selection so an older frame cannot survive
    // just because a newer request for that QSO came from the other channel.
    let bundle_channel = outcome
        .retained
        .iter()
        .find(|e| e.origin == crate::message_bus::TxOrigin::Remote)
        .map(|e| consent_channel_for(e.remote_client_key_id.as_deref()));
    let (retained, deferred): (Vec<_>, Vec<_>) = outcome.retained.into_iter().partition(|e| {
        e.origin != crate::message_bus::TxOrigin::Remote
            || Some(consent_channel_for(e.remote_client_key_id.as_deref())) == bundle_channel
    });
    outcome.retained = retained;
    // Ahead of the non-TX message the drain stopped at: these arrived first.
    inbox.requeue_front(
        deferred
            .into_iter()
            .map(|entry| {
                self_message(MessageType::TransmitRequest {
                    attempt_id: entry.attempt_id,
                    message_text: entry.message_text,
                    frequency_offset: entry.frequency_offset,
                    qso_id: entry.qso_id,
                    tx_parity: entry.tx_parity,
                    origin: entry.origin,
                    remote_client_key_id: entry.remote_client_key_id,
                })
            })
            .collect(),
    );

    // Every drained request belonged to an ended QSO (rare: needs ≥2 queued
    // requests, all terminal). Hand back an empty MultiTransmitRequest; the
    // multi-TX arm's "empty after dropping stale items" branch consumes and
    // skips it without keying PTT. These QSOs already transitioned terminal;
    // their Autonomous completions were reported above (PAN-185).
    if outcome.retained.is_empty() {
        return MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
            remote_client_key_id: None,
        };
    }

    // Single retained distinct stream → single TransmitRequest (unchanged arm).
    if outcome.retained.len() == 1 {
        let e = outcome.retained.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        };
    }

    // Several distinct live streams survived → fold into the existing multi-TX
    // path. All bundle items share one slot, so the bundle parity is the
    // FIRST-SEEN/oldest-retained stream's parity (not the freshest — order
    // here is first-seen order from the coalescer), which the existing arm
    // resolves via resolve_required_parity. Any stream that disagreed with
    // this anchor has already been excluded from `outcome.retained` above
    // (see the parity-conflict check in `coalesce_transmit_requests`), so
    // every remaining entry's `tx_parity` is either `None` or equal to this
    // value.
    let bundle_parity = outcome.retained[0].tx_parity;
    // PAN-136: each item keeps the binding of the entry it came from, so a
    // later key-time pivot can compare and swap it per item.
    let items: Vec<crate::message_bus::TransmitRequestItem> = outcome
        .retained
        .into_iter()
        .map(|e| crate::message_bus::TransmitRequestItem {
            attempt_id: e.attempt_id,
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            origin: e.origin,
            remote_client_key_id: e.remote_client_key_id,
        })
        .collect();
    // Fail-safe origin fold: if ANY folded stream is Remote, the whole bundle is
    // Remote so the arm gate applies. (In practice a coalesced backlog is one
    // origin; this is defense-in-depth for a future mixed-origin backlog.)
    // All retained remote entries now share one consent channel, and the
    // snapshot already checked every bound identity against the armed client.
    // Folding that identity preserves the channel for the final key-time gate.
    // PAN-136: the fold is shared with the TX worker's post-pivot re-fold.
    let (bundle_origin, bundle_remote_client_key_id) = super::fold_bundle_identity(items.iter());
    MessageType::MultiTransmitRequest {
        items,
        tx_parity: bundle_parity,
        origin: bundle_origin,
        remote_client_key_id: bundle_remote_client_key_id,
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        // Always clear keyed state, on every exit path (normal / abort / panic).
        if self.ptt_active.load(Ordering::Acquire) {
            self.last_ptt_off_ms
                .store(super::now_epoch_ms(), Ordering::Release);
        }
        self.ptt_active
            .store(false, std::sync::atomic::Ordering::Release);
        if self.armed {
            let bus = self.message_bus.clone();
            // Spawn a task to send PTT-off. This runs even if the parent task
            // was cancelled. PAN-273 (Codex P2, PR #587): its handle goes in
            // the shared slot so the TX worker's next key-down waits for it.
            self.pending_ptt_off.push(tokio::task::spawn(async move {
                match bus
                    .send_message_retrying(ptt_off_message, &SAFETY_SEND_RETRY_DELAYS)
                    .await
                {
                    Ok(true) => {
                        tracing::warn!("PTT GUARD: queued a forced PTT-off for Hamlib after task cancellation")
                    }
                    Ok(false) => tracing::error!(
                        target: "pancetta::tx.ptt",
                        "PTT GUARD: PTT OFF not delivered to Hamlib after {} attempts on drop; Hamlib's PTT watchdog, or at shutdown the direct rigctld PTT-off, is the remaining backstop",
                        SAFETY_SEND_RETRY_DELAYS.len() + 1
                    ),
                    Err(e) => tracing::error!("PTT GUARD: failed to force PTT off on drop: {}", e),
                }
            }));
        }
    }
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
enum SupersedeOutcome {
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

/// Slot and PTT timing for one TX request (PAN-322). Built at Step 2 of each
/// arm, where `slot_ns` is loaded, so a single-frame re-key pass rebuilds it.
#[derive(Clone)]
struct SlotTimingContext {
    utc_clock: pancetta_core::time::UtcClock,
    active_protocol: pancetta_ft8::Protocol,
    /// Raw `station.tx_late_max_ms`; [`Self::late_cap_ms`] scales it per mode.
    tx_late_max_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    ptt_lead_ms: u64,
    sample_rate: u32,
    slot_ns: i64,
    /// Frozen at dequeue, before the coalesce wait.
    request_received_at: chrono::DateTime<chrono::Utc>,
}

impl SlotTimingContext {
    /// The mode-scaled late cap, [`tx_late_max_ms_effective`].
    fn late_cap_ms(&self) -> u64 {
        tx_late_max_ms_effective(
            self.active_protocol,
            self.tx_late_max_ms.load(Ordering::Acquire),
        )
    }

    /// The PTT engage instant: `target_slot` minus `ptt_lead_ms`.
    fn ptt_target(
        &self,
        target_slot: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        target_slot - chrono::Duration::milliseconds(self.ptt_lead_ms as i64)
    }
}

/// The frame or bundle a supersede abandons.
struct SupersedeInFlight<'a> {
    items: &'a [crate::message_bus::TransmitRequestItem],
    origin: crate::message_bus::TxOrigin,
    remote_client_key_id: Option<String>,
    /// The in-flight request's own parity (single: its `TransmitRequest`;
    /// bundle: its `MultiTransmitRequest`).
    tx_parity: Option<pancetta_core::slot::SlotParity>,
}

/// The single-TX arm's working frame, which a viable supersede rewrites.
struct WorkingFrame<'a> {
    message_text: &'a mut String,
    frequency_offset: &'a mut f64,
    schedule: &'a mut TxSchedule,
}

/// Worker state a supersede handler sends through.
struct SupersedeIo<'a> {
    message_bus: &'a MessageBus,
    inbox: &'a TxInbox,
    encoder: &'a mut Ft8Encoder,
    ptt_guard: &'a mut PttGuard,
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
async fn supersede_and_rekey_or_bundle(
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

    let now = (timing.utc_clock)();
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
        let new_key = new_qso_id.as_deref().map(super::active_tx_qso_key);
        let mut candidate_items: Vec<crate::message_bus::TransmitRequestItem> = in_flight
            .items
            .iter()
            .filter(|it| {
                new_key.is_none() || it.qso_id.as_deref().map(super::active_tx_qso_key) != new_key
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
        let identity_conflict = super::remote_identities_conflict(
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
fn items_abandoned_by_supersede<'a>(
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
async fn settle_abandoned_single(
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
fn carried_items(successor: &MessageType) -> &[crate::message_bus::TransmitRequestItem] {
    match successor {
        MessageType::MultiTransmitRequest { items, .. } => items,
        _ => &[],
    }
}

/// Whether `candidates` carries `item` unchanged: same QSO, text, offset
/// (within `PIVOT_OFFSET_EPSILON_HZ`), authorization and ledger attempt. A
/// carried item airs under its own attempt, so a supersede does not settle
/// it (PAN-75, PAN-279).
fn item_carried_unchanged(
    item: &crate::message_bus::TransmitRequestItem,
    candidates: &[crate::message_bus::TransmitRequestItem],
) -> bool {
    candidates.iter().any(|c| {
        c.qso_id == item.qso_id
            && c.message_text == item.message_text
            && (c.frequency_offset - item.frequency_offset).abs() < super::PIVOT_OFFSET_EPSILON_HZ
            && c.origin == item.origin
            && c.remote_client_key_id == item.remote_client_key_id
            && c.attempt_id == item.attempt_id
    })
}

/// PAN-136 (Codex P2 on PR #456): whether `reenqueued` carries `item`'s exact
/// content for the same QSO under a different per-item binding. Such an item
/// is abandoned under its own (old) binding, which is what tells its attempt
/// apart from the re-bound same-text one in the PAN-75 rollback.
fn item_rebound_by_supersede(
    item: &crate::message_bus::TransmitRequestItem,
    reenqueued: Option<&MessageType>,
) -> bool {
    let Some(MessageType::MultiTransmitRequest { items, .. }) = reenqueued else {
        return false;
    };
    items.iter().any(|c| {
        c.qso_id == item.qso_id
            && c.message_text == item.message_text
            && (c.frequency_offset - item.frequency_offset).abs() < super::PIVOT_OFFSET_EPSILON_HZ
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
async fn report_abandoned_bundle_item(
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
async fn settle_items_abandoned_for(
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
async fn supersede_multi_reenqueue(
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
        target_slot: (timing.utc_clock)(),
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
            let bundle_outcome =
                encode_and_modulate_multi_tx(encoder, active_protocol, tx_params, &items);
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

impl super::ApplicationCoordinator {
    /// PAN-122: the coordinator's durable local TX log, created on first use
    /// and reused on every later call — the supervisor's restart of the TX
    /// worker re-runs `start_transmitter_component`, and the file must never
    /// have two writers. Under `--replay` the log is disabled (no file).
    pub(crate) fn ensure_tx_frame_log(&mut self) -> std::sync::Arc<TxFrameLog> {
        if let Some(log) = &self.tx_frame_log {
            return log.clone();
        }
        let log = if self.replay_mode() {
            info!("Replay mode: TX log disabled; replayed frames are not recorded");
            TxFrameLog::disabled()
        } else {
            let path = super::tx_log::tx_log_path(self.pancetta_home_override.as_deref());
            info!(
                "TX log: appending every transmitted frame to {}",
                path.display()
            );
            TxFrameLog::open(path)
        };
        let log = std::sync::Arc::new(log);
        self.tx_frame_log = Some(log.clone());
        log
    }

    /// Start FT8 transmitter component
    pub(crate) async fn start_transmitter_component(&mut self) -> Result<()> {
        self.start_transmitter_component_with_clock(
            std::sync::Arc::new(pancetta_agent::arm::ArmTime::now),
            pancetta_core::time::system_utc_clock(),
        )
        .await
    }

    /// The injected clock is sampled under the arm lock during playback.
    /// Production supplies ArmTime::now; tests share the accepted arm's timeline.
    pub(super) async fn start_transmitter_component_with_clock(
        &mut self,
        arm_clock: std::sync::Arc<dyn Fn() -> pancetta_agent::arm::ArmTime + Send + Sync>,
        utc_clock: pancetta_core::time::UtcClock,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_transmitter");
        let _enter = span.enter();

        info!("Starting FT8 transmitter component");

        // `get_or_create_channel` (not the strict `create_channel`): the
        // supervisor restarts this component after a panic (PAN-119), and a
        // re-invocation of this method must not fail on the channel that
        // the first run already registered — the same idempotency every
        // other restartable component's `start_*` relies on.
        let (_tx_sender, tx_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::Ft8Transmitter)
            .await?;
        let message_bus = self.message_bus.clone();

        // Hardware bindings remain restart-only. Timing preferences are read per attempt.
        let (sample_rate, max_concurrent_qsos, band_plan) = {
            let cfg = self.config.read().await;
            (
                12000u32, // FT8 sample rate
                // Mid-TX supersede bundle-add (Task 7): when the station runs
                // concurrent QSOs, a superseding manual request folds into the
                // current window's multi-TX bundle alongside the in-flight
                // content instead of fully replacing it. `1` (the default)
                // keeps the single-item replace path byte-identical to Task 6.
                cfg.autonomous.max_concurrent_qsos,
                // PAN-118: band-plan snapshot for the frequency-aware TX
                // gate (`tx_band_plan_mute_reason`). Band-plan changes require
                // restart, keeping the last-instant check synchronous.
                cfg.rig.frequency.band_plan.clone(),
            )
        };

        let tx_handle = {
            let live_config = self.config.clone();
            let live_late_cap = self.tx_late_max_ms_live.clone();
            let autonomous_config_enabled = self.autonomous_config_enabled.clone();
            let autonomous_cq_attempts = self.autonomous_cq_attempts.clone();
            let autonomous_self_cq_dispatches = self.autonomous_self_cq_dispatches.clone();
            let shutdown = self.shutdown_signal.clone();
            let abort_current_tx = self.abort_current_tx.clone();
            // Tri-state TX policy. The TX worker only enforces the hard
            // mute: when policy == Disabled it consumes a request without
            // keying PTT / playing audio / modulating, then reports the
            // block to the TUI. RespondOnly is gated upstream (at the
            // initiation sources) so in-progress QSOs keep flowing here.
            let tx_policy = self.tx_policy.clone();
            let tx_restart_inhibit = self.tx_restart_inhibit.clone();
            // PAN-19 round-7 (Codex P1): orthogonal to `tx_restart_inhibit`
            // above -- see `tx_hard_mute_reason`'s doc comment.
            let hamlib_command_loop_ready = self.hamlib_command_loop_ready.clone();
            // PAN-19 round-14 review (Codex P1): "keep TX muted until
            // pending rig state is delivered" -- see `tx_hard_mute_reason`'s
            // doc comment.
            let hamlib_pending_frequency = self.hamlib_pending_frequency.clone();
            let hamlib_pending_split = self.hamlib_pending_split.clone();
            // PAN-19 round-16 review (Codex P1): "keep restored rig state
            // pending through CAT application" -- see `tx_hard_mute_reason`'s
            // doc comment.
            let hamlib_command_in_flight = self.hamlib_command_in_flight.clone();
            // PAN-115 -- see `tx_hard_mute_reason`'s doc comment.
            let rig_conn_state = self.rig_conn_state.clone();
            let audio_output_alive = self.audio_output_alive.clone();
            // PAN-118 -- see `tx_band_plan_mute_reason`'s doc comment. The
            // shared dial atomics (updated by the Hamlib poll loop / split
            // writers) give the frequency-aware gate the live effective TX
            // dial at every check, including the last-instant re-check.
            let operating_frequency_hz = self.operating_frequency_hz.clone();
            let split_tx_frequency_hz = self.split_tx_frequency_hz();
            // Drop-stale-TX gate: the QSO component keeps this set in sync;
            // the worker refuses to key PTT for a request whose `qso_id` is no
            // longer present (superseded / cancelled / completed-past-grace).
            let active_tx_qsos = self.active_tx_qsos.clone();
            // PAN-76: QSOs Fox mode opened keep their TX slots after Fox mode is turned off.
            let fox_admitted_qsos = self.fox_admitted_qsos.clone();
            // PAN-141: DX-parity freshness check (Step 4b-adjacent, see
            // `dx_parity_conflict_for_qso`). `cross_time_state` is coordinator-
            // owned and outlives a supervised Qso-component restart, so a plain
            // `Arc` clone is safe here (unlike `qso_manager_watch` below).
            let cross_time_state = self.cross_time_state.clone();
            // PAN-141: restart-safe `QsoManager` handle, re-subscribed here and
            // re-borrowed fresh at each Step 4b check (never cached across the
            // pre-PTT wait) — the same pattern PAN-72 introduced for the
            // Autonomous task, for the identical reason: this closure's other
            // captures are spawn-time snapshots, but `Qso` is independently
            // supervised-restarted, so a spawn-time `QsoManager` clone would
            // silently point at a dead manager's QSO map after a restart.
            let qso_manager_watch = self.qso_manager_watch.subscribe();
            // Newest-TX-intent map: at key-time the worker pivots to the
            // freshest message for this QSO if a later decode advanced the
            // exchange while this frame waited out the pre-PTT sleep.
            let latest_tx_intent = self.latest_tx_intent.clone();
            // Keyed-state flag for the SWR poll / TUI (set by PttGuard).
            let ptt_active = self.ptt_active.clone();
            // PAN-143: shared with QsoManager's apply_tx_offset_switch and
            // the TUI-relay task's tx_freq_mode writers so PttGuard::new's
            // ptt_active store is genuinely atomic against those sites'
            // recheck/store, not just narrowly raced against them. See
            // PttGuard::new's doc comment.
            let ptt_sync_gate = self.ptt_sync_gate.clone();
            let relocation_epochs = self.relocation_epochs.clone();
            // PAN-73 follow-up: coordinator-owned so a restart keeps what the
            // previous worker had taken off the channel but not handled.
            let tx_requeue = self.tx_requeue.clone();
            // PAN-122: one writer per coordinator, reused across restarts.
            let tx_frame_log = self.ensure_tx_frame_log();
            #[cfg(test)]
            let tx_before_ptt_gate = self.tx_before_ptt_gate.clone();
            #[cfg(test)]
            let key_down_probe = self.key_down_probe.clone();
            // Timestamp of the most recent PTT-on, read by the FT8 decode
            // loop's TX-adjacent-desense diagnostic (see `ft8.rs`).
            let last_ptt_on_ms = self.last_ptt_on_ms.clone();
            let last_ptt_off_ms = self.last_ptt_off_ms.clone();
            // PAN-273: the armed-guard drop fallback each key-down waits for.
            let pending_ptt_off = self.pending_ptt_off.clone();
            // Active slot period (FT8 = 15e9 ns, FT4 = 7.5e9 ns), set once at
            // startup from `[rig].mode`. The TX scheduler keys against this so
            // FT4 lands on the 7.5s grid; FT8 (15e9) is byte-identical.
            let active_slot_ns = self.active_slot_ns();
            // Active digital-mode protocol from `[rig].mode`. The encode+modulate
            // steps branch on this: `Ft8` runs the exact legacy calls
            // (byte-identical), `Ft4`/`Ft2` emit the correct on-air waveform
            // (FT4 → 4-GFSK, 105 symbols, GFSK BT=1.0). Without this the station
            // would DECODE FT4 but TRANSMIT an FT8 waveform onto the 7.5s grid.
            // Live mode atomic — re-checked at the top of every request-
            // processing cycle below so a runtime FT8/FT4/FT2 switch
            // (Shift+M) takes effect on the very next TX, not just at
            // coordinator startup.
            let active_protocol_mode_atomic = self.active_protocol_mode();
            let active_protocol = self.active_protocol();
            // Station-agent remote-TX arm gate. Consulted ONLY for
            // `TxOrigin::Remote` requests before keying PTT; `TxOrigin::Local`
            // requests skip it entirely (byte-identical). Fail-CLOSED on a
            // poisoned lock (safety gate). Inert in P0–P2 (nothing arms it, no
            // remote request is constructed).
            let remote_tx_arm = self.remote_tx_arm();
            // Shared station-agent audit log (dispensa Q-0051 Phase B) — the
            // Step 0a remote-TX arm-gate drop appends `AuditKind::TxDenied`,
            // matching the existing `Arm`-rejection audit pattern.
            let audit_log = self.audit_log();
            // Gates the additive Step 0a relay send below (Phase C): mirrors
            // every other `relay_to_gateway` call site (hamlib.rs,
            // autonomous.rs, qso.rs) — zero-cost no-op when neither the
            // localhost gateway nor the station agent's read stream is live.
            let display_feed_enabled = self.display_feed_enabled.clone();
            // Fox mode raises the effective concurrent-stream cap above the
            // operator's configured `max_concurrent_qsos` — see
            // `effective_max_concurrent_qsos`'s doc comment and
            // `maybe_answer_caller`'s "Capacity bound" comment in qso.rs for
            // why these can't just be folded into the `max_concurrent_qsos`
            // snapshot above (both are live Atomics the operator/engine can
            // flip at runtime, not a startup-only config value).
            let fox_mode = self.fox_mode();
            let fox_max_streams = self.fox_max_streams();

            tokio::spawn(async move {
                info!(
                    "FT8 transmitter component ready (protocol {})",
                    active_protocol
                );

                // Shadowed as mutable: rebuilt in-loop below when the live
                // mode atomic diverges from this startup snapshot.
                let mut active_protocol = active_protocol;

                // For FT8 keep the exact legacy `Ft8Encoder::new()`; for FT4/FT2
                // build the encoder with the mode's protocol params so
                // `encode_message_protocol` applies the right sync/XOR/Gray map.
                let mut encoder = match active_protocol {
                    pancetta_ft8::Protocol::Ft8 => Ft8Encoder::new(),
                    _ => Ft8Encoder::with_protocol(pancetta_ft8::ProtocolParams::from_protocol(
                        active_protocol,
                    )),
                };
                let mut modulator = match Ft8Modulator::new_default() {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Failed to create modulator: {}", e);
                        return Err(anyhow::anyhow!("Modulator init failed: {}", e));
                    }
                };

                // Double-PTT fix (docs/qso-tx-deep-review-2026-07-18.md):
                // pivot consume-once tombstone. Step 4c below (the late
                // pivot) can swap the in-flight frame's text for a fresher
                // `LatestTxIntent` and key PTT with it — but the newer
                // `MessageToSend` that produced that fresher intent ALSO
                // already enqueued its own separate `TransmitRequest`, still
                // sitting behind the pivoted one in this worker's channel.
                // Record every successful pivot here (qso_key -> text) so
                // that request, once dequeued, is recognized as an
                // already-sent duplicate and dropped instead of keying PTT
                // a second time. Worker-local (this loop is the single
                // consumer; no concurrent access), so a plain `HashMap`
                // suffices — no new `Arc`/lock. See `is_pivot_duplicate`
                // (coordinator/mod.rs) for the pure membership check. The
                // tombstone also decides which request reports the QSO's
                // outcome to Autonomous (ownership rule:
                // `report_unaired_frame`, PAN-185).
                let mut pivoted_once: std::collections::HashMap<String, (String, f64)> =
                    std::collections::HashMap::new();
                // PAN-75: the text and payload each tombstoned pivot replaced.
                // Written with every `pivoted_once` insert and read only on a
                // Step 0-dup hit, so entries left behind when a tombstone is
                // cleared are inert.
                let mut pivoted_from = PivotReplaced::new();

                // One live read of the supersede bundle cap (PAN-76 Fox raise),
                // evaluated at each mid-TX supersede call.
                let live_supersede_max_concurrent = || {
                    supersede_max_concurrent_qsos(
                        effective_max_concurrent_qsos(
                            max_concurrent_qsos,
                            &fox_mode,
                            &fox_max_streams,
                        ),
                        live_fox_admitted_count(&fox_admitted_qsos, &active_tx_qsos),
                    )
                };

                let inbox = TxInbox::new(tx_rx, tx_requeue);
                'worker: while !shutdown.load(Ordering::Acquire) {
                    // Reset the per-message abort flag at the start of every
                    // try_recv cycle. Keeps a stale F8 from earlier (when no
                    // TX was in flight) from killing the next legitimate TX.
                    // (A mid-TX supersede sets this flag too; the 'key_and_send
                    // retry re-clears it before re-keying — see Steps 6/8.)
                    abort_current_tx.store(false, Ordering::Release);

                    // Re-check the live mode atomic every cycle. Encoder
                    // construction is cheap (no per-request TX happens more
                    // than once every several seconds in FT8/FT4/FT2
                    // operation) so rebuilding on a detected change, rather
                    // than every single request, keeps the common case
                    // (no change) a plain atomic load.
                    let live_protocol =
                        super::protocol_from_mode(pancetta_config::OperatingMode::from_u8(
                            active_protocol_mode_atomic.load(Ordering::Relaxed),
                        ));
                    if live_protocol != active_protocol {
                        info!(
                            "TX worker: protocol changed {} -> {} — rebuilding encoder",
                            active_protocol, live_protocol
                        );
                        active_protocol = live_protocol;
                        encoder = match active_protocol {
                            pancetta_ft8::Protocol::Ft8 => Ft8Encoder::new(),
                            _ => Ft8Encoder::with_protocol(
                                pancetta_ft8::ProtocolParams::from_protocol(active_protocol),
                            ),
                        };
                    }

                    match inbox.try_recv() {
                        Ok(mut message) => {
                            let (tx_late_max_ms, tx_self_parity) = {
                                let config = live_config.read().await;
                                (
                                    live_late_cap.load(Ordering::Acquire),
                                    config.station.tx_self_parity,
                                )
                            };
                            // PAN-302: only a self-CQ the autonomous engine
                            // actually dispatched (marked at send) is refused
                            // by the pre-PTT permission recheck below.
                            // `source == Autonomous` alone is not evidence of
                            // an engine dispatch — any bus participant can
                            // name that source (PAN-277's shutdown fixture
                            // sends exactly the self-CQ shape), and refusing
                            // its synthetic request silently broke keying.
                            let autonomous_request = message.source == ComponentId::Autonomous
                                && super::autonomous::is_marked_autonomous_self_cq_dispatch(
                                    &autonomous_self_cq_dispatches,
                                    message.id,
                                );
                            let is_autonomous_cq = |id: Option<&str>, attempt, text: &str| {
                                text.starts_with("CQ ")
                                    && (autonomous_request
                                        || id.is_some_and(|id| {
                                            autonomous_cq_attempts
                                                .lock()
                                                .map(|set| set.contains(&(id.to_string(), attempt)))
                                                .unwrap_or(true)
                                        }))
                            };
                            let _gain_tx_intent = matches!(
                                &message.message_type,
                                MessageType::TransmitRequest { .. }
                                    | MessageType::MultiTransmitRequest { .. }
                                    | MessageType::TuneRequest { .. }
                            )
                            .then(|| GainTxIntent::new(message_bus.gain_tx_intents.clone()));
                            // 2026-07-18 operator finding: the emit_event(MessageToSend)
                            // trace added in pancetta-qso proved the QSO state machine
                            // sends a QSO's final 73 exactly once, yet PTT keys twice
                            // (confirmed live with YO6BHN, UT7UJ, KF6VPA) — so the
                            // duplicate is downstream of qso.rs, inside this worker or
                            // its channel. `message.id` is a fresh, globally unique id
                            // assigned by `generate_message_id()` at every
                            // `ComponentMessage::new()` call, so logging it on every
                            // dequeue answers the open question directly: the SAME id
                            // appearing twice means the channel/bus redelivered one
                            // message (a bug here or in `MessageBus`); two DIFFERENT
                            // ids with identical `TransmitRequest` content mean a
                            // second, distinct send this worker was never told about
                            // by anything already ruled out (all 4 `TransmitRequest`
                            // construction sites, the backlog coalescer, the manual
                            // keep-call timer, the bounded auto-73 resend, the
                            // autonomous engine's gated-closed TX path). Log-only; no
                            // behavior change; remove once root-caused.
                            let msg_summary = match &message.message_type {
                                MessageType::TransmitRequest {
                                    message_text,
                                    qso_id,
                                    ..
                                } => {
                                    format!(
                                        "TransmitRequest(text='{message_text}', qso={qso_id:?})"
                                    )
                                }
                                MessageType::MultiTransmitRequest { items, .. } => {
                                    format!("MultiTransmitRequest({} items)", items.len())
                                }
                                other => format!("{other:?}"),
                            };
                            info!(
                                target: "pancetta::tx.recv_diag",
                                "tx_rx dequeued: msg_id={} from={} {}",
                                message.id, message.source, msg_summary
                            );

                            // qso-state-machine-analysis Symptom B fix: capture
                            // "now" HERE, at pickup, before the collection sleep
                            // below (and any coalescing). Both scheduling sites
                            // downstream (TransmitRequest and MultiTransmitRequest
                            // arms) reuse this SAME timestamp instead of re-reading
                            // the clock after the sleep. Without this, the
                            // COALESCE_COLLECT_WINDOW_MS sleep itself could push a
                            // request that was genuinely viable for the CURRENT
                            // slot (arrived with, say, 7.5s left before
                            // tx_late_max_ms) past the late-cap purely by the
                            // scheduling decision being made ~800ms later than the
                            // request actually arrived — an unforced ~30s defer.
                            let request_received_at = utc_clock();
                            // PAN-117 (PR #450 review round 2): only a
                            // `TransmitRequest` head takes the base coalesce wait
                            // below. Captured before coalescing can fold it into a
                            // `MultiTransmitRequest`, so the multi-TX arm can tell
                            // a coalesced bundle (waited) from a directly dequeued
                            // one (did not) when it picks its acceptance cap.
                            let head_takes_base_wait =
                                matches!(message.message_type, MessageType::TransmitRequest { .. });

                            // --- Backpressure / staleness coalescing ---
                            // The worker processes one request at a time and a
                            // single transmit spans ~13-28s, while keep-call +
                            // repeated operator actions enqueue a new request
                            // every ~5-15s. Under load the channel backs up
                            // unboundedly and we'd replay STALE frames slot
                            // after slot. So when the head is a TransmitRequest,
                            // drain the rest of the queued TransmitRequests now
                            // and coalesce to current intent: newest-per-qso_id
                            // wins, terminal-QSO requests are dropped, manual
                            // (qso_id == None) sends are preserved, and the
                            // distinct-stream count is bounded. The drain stops
                            // at the first non-TransmitRequest (Tune / Multi),
                            // which is re-enqueued so it's never reordered or
                            // dropped. Single-request (no-backlog) case rewrites
                            // back to exactly that one request — normal path
                            // unchanged.
                            if head_takes_base_wait {
                                // Adaptive collection window (Symptom C fix): take
                                // the base wait once, unconditionally (same as
                                // before — this is the byte-identical baseline for
                                // the common lone-request case), then extend in
                                // further base-length increments ONLY while the
                                // channel's queued-message count keeps growing,
                                // capped by remaining tx_late_max_ms headroom and a
                                // protocol-scaled ceiling. Never modifies
                                // coalesce_backlog_into/coalesce_transmit_requests
                                // — this only decides how long to wait before that
                                // existing, unmodified drain runs once. See
                                // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §1.
                                let base_wait_ms = coalesce_collect_window_ms(active_protocol);
                                let queue_len_before_base = inbox.len();
                                if interruptible_sleep(
                                    Duration::from_millis(base_wait_ms),
                                    &shutdown,
                                    &abort_current_tx,
                                )
                                .await
                                {
                                    if shutdown.load(Ordering::Acquire) {
                                        info!("TX aborted during collection window by shutdown");
                                        break;
                                    }
                                    info!("TX aborted during collection window by operator (F8)");
                                    send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                    emit_failed_transmit_complete_for_request(
                                        &message_bus,
                                        &message.message_type,
                                    )
                                    .await;
                                    continue;
                                }

                                let extension_cap_ms = adaptive_coalesce_cap_ms(
                                    &message.message_type,
                                    request_received_at,
                                    tx_self_parity,
                                    tx_late_max_ms,
                                    sample_rate,
                                    active_slot_ns.load(Ordering::Relaxed),
                                    active_protocol,
                                );
                                let mut extended_ms: u64 = 0;
                                let mut prev_len = inbox.len();
                                // `aborted` is set instead of `continue`/`break`-ing
                                // directly inside the loop: an unlabeled `continue`
                                // here would target this `while`, not the outer
                                // worker loop, which would wrongly resume
                                // extending after an operator abort instead of
                                // abandoning this TX attempt. Checking the flag
                                // once after the loop, exactly as done for every
                                // other single-sleep abort site in this worker,
                                // avoids that.
                                let mut aborted = false;
                                while prev_len > queue_len_before_base
                                    && extended_ms < extension_cap_ms
                                {
                                    let this_wait =
                                        base_wait_ms.min(extension_cap_ms - extended_ms);
                                    if interruptible_sleep(
                                        Duration::from_millis(this_wait),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        aborted = true;
                                        break;
                                    }
                                    extended_ms += this_wait;
                                    let new_len = inbox.len();
                                    if new_len <= prev_len {
                                        // Nothing new arrived this increment — stop
                                        // extending, nothing left to wait for.
                                        break;
                                    }
                                    prev_len = new_len;
                                }
                                if aborted {
                                    if shutdown.load(Ordering::Acquire) {
                                        info!(
                                            "TX aborted during collection window extension by shutdown"
                                        );
                                        break;
                                    }
                                    info!(
                                        "TX aborted during collection window extension by operator (F8)"
                                    );
                                    send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                    emit_failed_transmit_complete_for_request(
                                        &message_bus,
                                        &message.message_type,
                                    )
                                    .await;
                                    continue;
                                }

                                message.message_type = coalesce_backlog_into(
                                    message.message_type,
                                    &inbox,
                                    &message_bus,
                                    &active_tx_qsos,
                                    &fox_admitted_qsos,
                                    // Deliberately the UNRAISED ceiling (PAN-76): the
                                    // coalescer raises it only to the Fox-admitted
                                    // streams present in this backlog.
                                    effective_max_concurrent_qsos(
                                        max_concurrent_qsos,
                                        &fox_mode,
                                        &fox_max_streams,
                                    ),
                                    &remote_tx_arm,
                                    &audit_log,
                                    &display_feed_enabled,
                                    Some((&mut pivoted_once, &mut pivoted_from)),
                                )
                                .await;

                                // Same 2026-07-18 diagnostic: show what coalescing
                                // produced, unconditionally (not just when the
                                // "TX backlog coalesced" warning already fires),
                                // so a duplicate that appears ONLY after this step
                                // (and not at the dequeue log above) would point
                                // at this function specifically.
                                let post_coalesce_summary = match &message.message_type {
                                    MessageType::TransmitRequest {
                                        message_text,
                                        qso_id,
                                        ..
                                    } => {
                                        format!(
                                            "TransmitRequest(text='{message_text}', qso={qso_id:?})"
                                        )
                                    }
                                    MessageType::MultiTransmitRequest { items, .. } => {
                                        format!("MultiTransmitRequest({} items)", items.len())
                                    }
                                    other => format!("{other:?}"),
                                };
                                info!(
                                    target: "pancetta::tx.recv_diag",
                                    "post-coalesce: msg_id={} {}",
                                    message.id, post_coalesce_summary
                                );
                            }

                            match message.message_type {
                                MessageType::TransmitRequest {
                                    // `mut`: re-pointed with the payload by a key-time pivot and by a
                                    // `Replace` supersede, so a failure reports the attempt actually
                                    // keyed (PAN-183).
                                    mut attempt_id,
                                    mut message_text,
                                    mut frequency_offset,
                                    // `mut`: a mid-TX supersede on the `Replace` path re-keys
                                    // in place and reassigns this to the SUPERSEDING
                                    // request's OWN qso_id too (PAN-38 round 4), same
                                    // reasoning as `origin` below.
                                    mut qso_id,
                                    tx_parity,
                                    // `mut`: a mid-TX supersede on the `Replace` path re-keys
                                    // in place and reassigns this to the SUPERSEDING request's
                                    // origin, so Step 4b-arm's key-time gate re-evaluates
                                    // against the frame actually about to transmit (C1 fix).
                                    mut origin,
                                    // `mut`: mirrors `origin` above (PAN-91) — re-pointed
                                    // alongside it on a `Replace` supersede.
                                    mut remote_client_key_id,
                                } => {
                                    info!(
                                        "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                        message_text, frequency_offset, qso_id
                                    );
                                    TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);

                                    // --- Step 0-dup: pivot-tombstone duplicate gate ---
                                    // Double-PTT fix (docs/qso-tx-deep-review-2026-07-18.md).
                                    // A prior TransmitRequest for this SAME qso_id may
                                    // already have been pivoted (Step 4c below) to this
                                    // exact text and physically keyed, while THIS request
                                    // — the one that produced that newer text in the first
                                    // place — was still sitting behind it in the channel.
                                    // If so, this is a stale duplicate of an already-sent
                                    // frame: drop it exactly like the Step 4b stale-QSO
                                    // drop (no PTT, no schedule), and clear the tombstone so
                                    // a genuinely later legitimate re-send of the same text
                                    // (e.g. a keep-call rearm) is never wrongly suppressed.
                                    // Autonomous hears nothing: the pivoted frame keyed and
                                    // reported (ownership rule: `report_unaired_frame`,
                                    // PAN-185).
                                    if super::is_pivot_duplicate(
                                        qso_id.as_deref(),
                                        &message_text,
                                        frequency_offset,
                                        &pivoted_once,
                                    ) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "dropping stale TX for {}: '{}' — already sent via pivot",
                                            qso_id.as_deref().unwrap_or("?"),
                                            message_text
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "dropping stale TX for '{message_text}' — already sent via pivot"
                                            ),
                                            qso_id.as_deref(),
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let replaced = qso_id.as_deref().and_then(|id| {
                                            let key = super::active_tx_qso_key(id);
                                            pivoted_once.remove(&key);
                                            pivoted_from.remove(&key)
                                        });
                                        report_pivot_duplicate_dropped(
                                            &message_bus,
                                            qso_id.clone(),
                                            replaced,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // --- Step 0: TX-policy hard mute ---
                                    // If the global policy is Disabled (RX-only),
                                    // do NOT key PTT / play audio / modulate. Consume
                                    // the request, tell the TUI it was blocked, and
                                    // report a failed TransmitComplete so any awaiting
                                    // QSO state machine doesn't hang. This is the
                                    // catch-all hard gate for every TX source.
                                    if let Some(reason) = tx_hard_mute_reason(
                                        &tx_policy,
                                        &tx_restart_inhibit,
                                        &hamlib_command_loop_ready,
                                        &hamlib_pending_frequency,
                                        &hamlib_pending_split,
                                        &hamlib_command_in_flight,
                                        &rig_conn_state,
                                        &audio_output_alive,
                                        &message_bus.receive_gain,
                                        &message_bus.data_mode,
                                    ) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({}): '{}' at {:.0} Hz (qso: {:?})",
                                            reason, message_text, frequency_offset, qso_id
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): '{message_text}' at {frequency_offset:.0} Hz"
                                            ),
                                            qso_id.as_deref(),
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let _ = report_transmit_complete(
                                            &message_bus,
                                            false,
                                            message_text,
                                            0,
                                            qso_id.clone(),
                                            attempt_id,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // --- Step 0-band: PAN-118 band-plan gate ---
                                    // Frequency-aware sibling of the hard mute
                                    // above: refuses THIS transmission when its
                                    // absolute TX RF (effective split-aware dial
                                    // + this request's absolute audio offset)
                                    // falls outside the configured amateur band
                                    // plan — before PTT is ever asserted. Same
                                    // unwind as the hard-mute branch above. The
                                    // check is skipped while the dial is the
                                    // unknown-0 sentinel (dev/mock modes);
                                    // `tx_band_plan_mute_reason` has the detail.
                                    if let Some(reason) = tx_rf_for_gate(
                                        &operating_frequency_hz,
                                        &split_tx_frequency_hz,
                                        frequency_offset,
                                    )
                                    .and_then(|rf| {
                                        tx_band_plan_mute_reason(
                                            rf,
                                            tx_signal_span_hz(active_protocol),
                                            &band_plan,
                                        )
                                    }) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({reason}): '{}' at {:.0} Hz (qso: {:?})",
                                            message_text, frequency_offset, qso_id
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): '{message_text}' at {frequency_offset:.0} Hz"
                                            ),
                                            qso_id.as_deref(),
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let _ = report_transmit_complete(
                                            &message_bus,
                                            false,
                                            message_text,
                                            0,
                                            qso_id.clone(),
                                            attempt_id,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // --- Step 0a: Remote-TX arm gate ---
                                    // Remote-originated requests must pass the
                                    // station-agent arm gate (armed ∧ tx-scope ∧
                                    // unexpired ∧ heartbeat-fresh ∧ local-consent ∧
                                    // ¬local-kill). Local requests skip this entirely
                                    // (byte-identical). This ANDs UNDER the TxPolicy
                                    // hard-mute above (Disabled already dropped). Fail
                                    // CLOSED on a poisoned arm lock. In P0–P2 nothing
                                    // arms it and no Remote request is constructed, so
                                    // this branch is never taken.
                                    if origin == crate::message_bus::TxOrigin::Remote
                                        && !remote_tx_permitted_for(
                                            &remote_tx_arm,
                                            pancetta_agent::arm::ArmTime::now,
                                            remote_client_key_id.as_deref(),
                                        )
                                    {
                                        warn!(
                                            target: "agent.tx",
                                            "dropping remote TX — not armed/permitted: '{}' at {:.0} Hz (qso: {:?})",
                                            message_text, frequency_offset, qso_id
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "agent.tx",
                                            pancetta_core::DiagnosticLevel::Warn,
                                            format!(
                                                "Remote TX denied (not armed/permitted): '{message_text}' at {frequency_offset:.0} Hz"
                                            ),
                                            qso_id.as_deref(),
                                        )
                                        .await;
                                        // dispensa Q-0051 Phase B/C: audit the
                                        // drop (matches the existing `Arm`-
                                        // rejection `TxDenied` pattern) and
                                        // relay a client-visible signal to any
                                        // connected remote client.
                                        let denial_reason = format!(
                                            "not armed/permitted: '{message_text}' at {frequency_offset:.0} Hz"
                                        );
                                        audit_log.record(&pancetta_agent::audit::AuditEvent {
                                            ts_unix_ms: utc_clock().timestamp_millis(),
                                            kind: pancetta_agent::audit::AuditKind::TxDenied,
                                            operator_callsign: tx_denied_operator_attribution(
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            ),
                                            detail: denial_reason.clone(),
                                        });
                                        super::remote_gateway::relay_to_gateway(
                                            &message_bus,
                                            &display_feed_enabled,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TxDenied {
                                                reason: denial_reason,
                                                qso_id: qso_id.clone(),
                                            },
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let _ = report_transmit_complete(
                                            &message_bus,
                                            false,
                                            message_text,
                                            0,
                                            qso_id.clone(),
                                            attempt_id,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // Report this item as QUEUED (dequeued and now
                                    // scheduling, but not yet on the air).
                                    send_tx_queue_status(
                                        &message_bus,
                                        None,
                                        vec![crate::message_bus::TxItem {
                                            text: message_text.clone(),
                                            freq_hz: frequency_offset,
                                            qso_id: qso_id.clone(),
                                            deferred: false,
                                        }],
                                    )
                                    .await;

                                    // Mid-TX supersede state (Phase 1 abort/restart).
                                    // On a qualifying request arriving mid-TX (Steps 6/8
                                    // below), `supersede_and_rekey` recomputes the schedule
                                    // against *now*; `rekey_schedule` carries it across the
                                    // 'key_and_send retry, and `is_rekey` drives Step 7's
                                    // flush-stale-audio flag. Both stay inert (None/false)
                                    // on the normal first pass, keeping it byte-identical.
                                    let mut rekey_schedule: Option<TxSchedule> = None;
                                    let mut is_rekey = false;

                                    // 'key_and_send wraps Steps 1-10 so a mid-TX supersede
                                    // can abort the in-flight frame and re-drive these steps
                                    // with the new content (re-encoding at Step 1) instead
                                    // of dropping the request. A normal transmission runs the
                                    // body exactly once and `break 'key_and_send`s at the end.
                                    // Every pre-existing "abandon this message" continue/break
                                    // inside the body now targets 'worker explicitly, since
                                    // 'key_and_send is the innermost loop.
                                    'key_and_send: loop {
                                        // --- Step 1: Encode + modulate up front ---
                                        // Do this BEFORE any timing-critical work so encoding
                                        // latency can't push us past the slot boundary.
                                        //
                                        // TransmitRequest.frequency_offset is the ABSOLUTE audio
                                        // frequency in Hz (200-4000), not a delta. The modulator
                                        // adds its base_frequency to whatever we pass to
                                        // modulate_symbols, so to honor the request we set the
                                        // base to the requested frequency and pass 0 as the
                                        // additional offset.
                                        if let Err(e) =
                                            modulator.set_base_frequency(frequency_offset)
                                        {
                                            warn!(
                                                "Invalid TX frequency {} Hz for '{}': {}",
                                                frequency_offset, message_text, e
                                            );
                                            emit_tx_failure_diagnostic(
                                                &message_bus,
                                                qso_id.as_deref(),
                                                &message_text,
                                                &format!(
                                                    "invalid frequency {frequency_offset} Hz ({e})"
                                                ),
                                            )
                                            .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }
                                        // Encode + modulate under the active protocol.
                                        // For FT8 this dispatches to the exact legacy
                                        // `encode_message` + `modulate_symbols` calls
                                        // (byte-identical); for FT4/FT2 it uses the
                                        // protocol-aware `encode_message_protocol` +
                                        // `modulate_symbols_protocol` so the on-air
                                        // waveform matches the mode.
                                        let (samples, _duration_ms) = match encode_for_protocol(
                                            &mut encoder,
                                            active_protocol,
                                            &message_text,
                                        )
                                        .and_then(|symbols| {
                                            modulate_for_protocol(
                                                &mut modulator,
                                                active_protocol,
                                                &symbols,
                                                0.0,
                                            )
                                        }) {
                                            Ok(s) => {
                                                let dur =
                                                    (s.len() as f64 / 12000.0 * 1000.0) as u64;
                                                info!(
                                                    "TX: '{}' -> {} samples ({:.2}s)",
                                                    message_text,
                                                    s.len(),
                                                    dur as f64 / 1000.0
                                                );
                                                (s, dur)
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Encode/modulate failed for '{}': {}",
                                                    message_text, e
                                                );
                                                emit_tx_failure_diagnostic(
                                                    &message_bus,
                                                    qso_id.as_deref(),
                                                    &message_text,
                                                    &format!("encode/modulate error ({e})"),
                                                )
                                                .await;
                                                let _ = report_transmit_complete(
                                                    &message_bus,
                                                    false,
                                                    message_text,
                                                    0,
                                                    qso_id.clone(),
                                                    attempt_id,
                                                )
                                                .await;
                                                continue 'worker;
                                            }
                                        };

                                        // --- Step 2: Resolve required parity ---
                                        let (tx_late_max_ms, tx_self_parity, ptt_lead_ms) = {
                                            let config = live_config.read().await;
                                            (
                                                live_late_cap.load(Ordering::Acquire),
                                                config.station.tx_self_parity,
                                                config.station.ptt_lead_ms,
                                            )
                                        };
                                        let timing = SlotTimingContext {
                                            utc_clock: utc_clock.clone(),
                                            active_protocol,
                                            tx_late_max_ms: live_late_cap.clone(),
                                            tx_self_parity,
                                            ptt_lead_ms,
                                            sample_rate,
                                            slot_ns: active_slot_ns.load(Ordering::Relaxed),
                                            request_received_at,
                                        };
                                        let slot_ns = timing.slot_ns;
                                        let required_parity = resolve_required_parity(
                                            tx_parity,
                                            tx_self_parity,
                                            timing.request_received_at,
                                            slot_ns,
                                        );

                                        // PAN-117 (PR #450 review): decide
                                        // `use_current` against the acceptance
                                        // cap, not the raw effective cap — the
                                        // base coalesce window plus the safety
                                        // margin's overhead always elapse
                                        // between the frozen
                                        // `request_received_at` and the
                                        // key-time gate, so a head accepted on
                                        // the raw cap in the 2.2-3.0s band
                                        // (FT8, 3000ms default) could only
                                        // ever be dropped there. Reserving the
                                        // overhead makes it defer 30s instead.
                                        let mut schedule = schedule_tx(
                                            request_received_at,
                                            required_parity,
                                            current_slot_acceptance_cap_ms(
                                                active_protocol,
                                                tx_late_max_ms,
                                            ),
                                            sample_rate,
                                            slot_ns,
                                        );
                                        // On a mid-TX re-key, discard the first-pass schedule
                                        // recomputed above (keyed to the frozen
                                        // `request_received_at` and the original parity) and use
                                        // the schedule `supersede_and_rekey` computed against
                                        // *now* for the superseding request. Never deferred here:
                                        // supersede_and_rekey returns false (breaks the loop) if
                                        // the re-key can't make this slot, so a retry never
                                        // carries a deferred schedule. `None` on the first pass
                                        // leaves the normal path byte-identical.
                                        if let Some(s) = rekey_schedule {
                                            schedule = s;
                                        }

                                        info!(
                                        "TX scheduled: parity={:?} target_slot={} pad={} samples cursor={} samples deferred={}",
                                        required_parity,
                                        schedule.target_slot.format("%H:%M:%S%.3f UTC"),
                                        schedule.silent_pad_samples,
                                        schedule.cursor_offset_samples,
                                        schedule.deferred,
                                    );

                                        // If we missed the current slot and deferred to a
                                        // later one (~30s), refresh the QUEUED strip with
                                        // the deferred flag so it shows "deferred 30s"
                                        // instead of looking dead during the long wait.
                                        if schedule.deferred {
                                            TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);
                                            // Re-check active-status at defer time: a
                                            // terminal QSO's request must not be re-
                                            // deferred 30s into the future (that is
                                            // exactly the "stale frames every cycle"
                                            // loop the operator hit).
                                            if !tx_qso_is_live(qso_id.as_deref(), &active_tx_qsos) {
                                                info!(
                                                    target: "pancetta::tx.policy",
                                                    "dropping stale TX for ended QSO {} at defer time: '{}'",
                                                    qso_id.as_deref().unwrap_or("?"),
                                                    message_text
                                                );
                                                emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "dropping stale TX for ended QSO at defer time: '{message_text}'"
                                                ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                                send_tx_queue_status(
                                                    &message_bus,
                                                    None,
                                                    Vec::new(),
                                                )
                                                .await;
                                                let _ = report_transmit_complete(
                                                    &message_bus,
                                                    false,
                                                    message_text,
                                                    0,
                                                    qso_id.clone(),
                                                    attempt_id,
                                                )
                                                .await;
                                                continue 'worker;
                                            }
                                            send_tx_queue_status(
                                                &message_bus,
                                                None,
                                                vec![crate::message_bus::TxItem {
                                                    text: message_text.clone(),
                                                    freq_hz: frequency_offset,
                                                    qso_id: qso_id.clone(),
                                                    deferred: true,
                                                }],
                                            )
                                            .await;
                                        }

                                        // PAN-223: the stall ledger keys this frame's
                                        // keep-call attempt by the slot chosen here, not
                                        // the rearm's prediction. Every `'key_and_send`
                                        // pass reports, so a re-key reports its
                                        // superseding attempt and schedule.
                                        report_frame_scheduled(
                                            &message_bus,
                                            qso_id.as_deref(),
                                            attempt_id,
                                            &schedule,
                                        )
                                        .await;

                                        // --- Step 4: Wait until PTT engage instant ---
                                        // PAN-114: waits on the wall clock (re-read every
                                        // 50 ms chunk), so a clock jump during the wait is
                                        // seen before PTT instead of keying a late or
                                        // garbage frame — see `wait_for_ptt_instant`.
                                        let ptt_target_utc =
                                            timing.ptt_target(schedule.target_slot);
                                        let ptt_wait = wait_for_ptt_instant(
                                            ptt_target_utc,
                                            schedule.target_slot,
                                            timing.late_cap_ms(),
                                            slot_ns,
                                            &shutdown,
                                            &abort_current_tx,
                                            || utc_clock(),
                                        )
                                        .await;
                                        match on_ptt_wait(
                                            ptt_wait,
                                            TxShape::Single,
                                            || {
                                                vec![(
                                                    message_text.clone(),
                                                    qso_id.clone(),
                                                    attempt_id,
                                                )]
                                            },
                                            &message_bus,
                                            &shutdown,
                                        )
                                        .await
                                        {
                                            std::ops::ControlFlow::Continue(()) => {}
                                            std::ops::ControlFlow::Break(TxExit::StopWorker) => {
                                                break 'worker
                                            }
                                            std::ops::ControlFlow::Break(TxExit::NextRequest) => {
                                                continue 'worker
                                            }
                                        }

                                        // --- Step 4b-parity: DX-parity freshness gate (PAN-141) ---
                                        // Runs BEFORE the plain liveness check below
                                        // (Codex P1 on PR #370): this check's own
                                        // `dx_parity_conflict_for_qso` await (a QsoManager
                                        // lookup) can itself span a cancellation the way
                                        // any other await in this worker can — so the
                                        // liveness check MUST be the very last thing before
                                        // Step 4b-arm/PTT, not this one. A manually-latched
                                        // `tx_parity` reflects the DX's LAST-observed decode
                                        // at QSO-open time; the ~20-30s wait for the next
                                        // opposite-parity slot is long enough for a
                                        // fast-cycling pileup DX to have moved to a
                                        // different caller on the exact parity we're about
                                        // to key into. Hold this cycle rather than transmit
                                        // into a probable collision — never mutates
                                        // `tx_parity` (the half-duplex single-shared-TX-side
                                        // invariant across concurrent QSOs stays intact) and
                                        // never touches QSO state; the QSO's own existing
                                        // retry/watchdog cadence (`rearm_manual_calls_at`,
                                        // `manual_call_max_calls`, `report_timeout`) decides
                                        // what happens next.
                                        if dx_parity_conflict_for_qso(
                                            qso_id.as_deref(),
                                            required_parity,
                                            &operating_frequency_hz,
                                            slot_ns,
                                            &qso_manager_watch,
                                            &cross_time_state,
                                        )
                                        .await
                                        {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "holding TX for QSO {}: DX observed transmitting on the colliding parity {:?} within the freshness window: '{}'",
                                                qso_id.as_deref().unwrap_or("?"),
                                                required_parity,
                                                message_text
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                "holding TX: DX observed transmitting on the colliding parity {required_parity:?} within the freshness window: '{message_text}'"
                                            ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // --- Step 4b: Drop-stale-TX gate ---
                                        // The slot wait above (AND the parity check just
                                        // above, whose own QsoManager lookup is itself an
                                        // await) can span the moment a QSO ends (superseded
                                        // by a newer call, cancelled, or completed-past-
                                        // grace). Re-check active status at the last instant
                                        // before keying: if this request's QSO is no longer
                                        // live, do NOT key PTT / build+send audio — clear the
                                        // strip, report a failed TransmitComplete, and skip.
                                        // Requests with no qso_id (manual / tune) are never
                                        // gated.
                                        if !tx_qso_is_live(qso_id.as_deref(), &active_tx_qsos) {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "dropping stale TX for ended QSO {}: '{}'",
                                                qso_id.as_deref().unwrap_or("?"),
                                                message_text
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                "dropping stale TX for ended QSO: '{message_text}'"
                                            ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            // Bound `pivoted_once`: a QSO ending via normal
                                            // drop-stale cleanup also clears its own
                                            // pivot-tombstone entry so the worker-local map
                                            // can't grow unboundedly across a long-running
                                            // process.
                                            if let Some(id) = qso_id.as_deref() {
                                                let key = super::active_tx_qso_key(id);
                                                pivoted_once.remove(&key);
                                                pivoted_from.remove(&key);
                                            }
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // --- Step 4b-arm: re-check the remote-TX arm at
                                        // the last instant before keying. The slot wait
                                        // above can span up to ~30s — the dead-man
                                        // heartbeat window. A Remote request admitted with
                                        // a valid arm at pickup must NOT key PTT if, during
                                        // the wait, the heartbeat lapsed, the TTL expired,
                                        // local consent was revoked, or the local kill was
                                        // engaged. Mirrors Step-4b's stale-QSO re-check so
                                        // the dead-man/TTL/local-kill guarantees hold across
                                        // the pre-PTT sleep. Local requests are never gated.
                                        if origin == crate::message_bus::TxOrigin::Remote
                                            && !remote_tx_permitted_for(
                                                &remote_tx_arm,
                                                pancetta_agent::arm::ArmTime::now,
                                                remote_client_key_id.as_deref(),
                                            )
                                        {
                                            // Round-3 review (Codex P2): route through the
                                            // shared denial-signaling helper — a log line +
                                            // TransmitComplete alone left this key-time
                                            // cross-client rejection with no TxDenied audit
                                            // record, diagnostic, or client relay, unlike
                                            // every other arm-gate denial.
                                            let denial_reason = format!(
                                                "arm went stale during slot wait: '{message_text}' at {frequency_offset:.0} Hz"
                                            );
                                            let operator_callsign = tx_denied_operator_attribution(
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            );
                                            emit_disarm_interrupt_signals(
                                                &message_bus,
                                                &audit_log,
                                                &display_feed_enabled,
                                                operator_callsign,
                                                "denied",
                                                denial_reason,
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        if let Some(reason) = tx_hard_mute_reason(
                                            &tx_policy,
                                            &tx_restart_inhibit,
                                            &hamlib_command_loop_ready,
                                            &hamlib_pending_frequency,
                                            &hamlib_pending_split,
                                            &hamlib_command_in_flight,
                                            &rig_conn_state,
                                            &audio_output_alive,
                                            &message_bus.receive_gain,
                                            &message_bus.data_mode,
                                        ) {
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!("TX re-key blocked ({reason}): '{message_text}'"),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // PAN-118: the band-plan sibling of the
                                        // re-key mute above — the dial may have
                                        // moved (CAT / split change) during the
                                        // pre-PTT wait. Same unwind as that
                                        // branch.
                                        if let Some(reason) = tx_rf_for_gate(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                            frequency_offset,
                                        )
                                        .and_then(|rf| {
                                            tx_band_plan_mute_reason(
                                                rf,
                                                tx_signal_span_hz(active_protocol),
                                                &band_plan,
                                            )
                                        }) {
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "TX re-key blocked ({reason}): '{message_text}'"
                                                ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // PAN-148: resolve the DX callsign for the Step 4d
                                        // final gate's parity re-check NOW, BEFORE Step 3's
                                        // timing-sensitive trim below — round-2 review (Codex
                                        // P1) found that awaiting this `QsoManager` lookup
                                        // AFTER the trim risked keying `SetPtt` against a
                                        // now-stale audio-alignment cursor if the lookup's
                                        // internal lock were ever contended. `their_callsign`
                                        // is a stable property of an established QSO, so
                                        // resolving it here and consulting it again in Step
                                        // 4d's synchronous read (further below) doesn't
                                        // reintroduce staleness — see
                                        // `final_single_tx_gate_denial`'s doc comment.
                                        let final_gate_callsign = resolve_their_callsign(
                                            qso_id.as_deref(),
                                            &qso_manager_watch,
                                        )
                                        .await;

                                        // --- Step 3 (moved): build the audio buffer,
                                        // refreshed against real time ---
                                        // request_received_at (Step 2) already decided WHICH
                                        // slot to target and whether to defer — that decision
                                        // is never re-made here (Symptom-B's protection, see
                                        // Global Constraints in the implementation plan). But
                                        // real time has moved on since Step 2 (through the
                                        // Symptom-C adaptive coalesce window, encoding, and the
                                        // gates above), and Step 6 below is a no-op for the
                                        // common current-slot case (target_slot is already in
                                        // the past), so audio actually ships at whatever "now"
                                        // is by the time we reach Step 7 — not at the "now"
                                        // schedule_tx originally saw. Refresh just the pad/cursor
                                        // math against the SAME schedule.target_slot so the
                                        // transmitted waveform stays correctly aligned to the
                                        // real FT8 slot grid regardless of how long the steps
                                        // above took. See
                                        // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2.
                                        // PAN-273: wait out an outstanding drop-fallback
                                        // PTT-off (e.g. after an undelivered supersede
                                        // release) before the refresh, so the wait cannot
                                        // misalign the audio.
                                        pending_ptt_off.settle().await;
                                        let (fresh_pad_samples, fresh_cursor_samples) =
                                            pad_and_cursor_for_target(
                                                utc_clock(),
                                                schedule.target_slot,
                                                sample_rate,
                                            );
                                        schedule.silent_pad_samples = fresh_pad_samples;
                                        schedule.cursor_offset_samples = fresh_cursor_samples;

                                        // Pad zeros in front (early branch); skip cursor into
                                        // waveform (late branch); never both at the same time.
                                        let mut audio_out: Vec<f32> = Vec::with_capacity(
                                            schedule.silent_pad_samples + samples.len(),
                                        );
                                        audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                        if schedule.cursor_offset_samples < samples.len() {
                                            audio_out.extend_from_slice(
                                                &samples[schedule.cursor_offset_samples..],
                                            );
                                        } else {
                                            // Defensive: if cursor outran the waveform (shouldn't
                                            // happen because too-late defers), emit nothing and
                                            // skip TX.
                                            warn!("schedule_tx cursor exceeded waveform length at key-time; skipping TX");
                                            emit_tx_failure_diagnostic(
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            "internal scheduling error (cursor exceeded waveform at key-time)",
                                        )
                                        .await;
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                message_text,
                                                0,
                                                qso_id.clone(),
                                                attempt_id,
                                            )
                                            .await;
                                            continue 'worker;
                                        }
                                        let audio_duration_ms =
                                            (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                                as u64;

                                        // Round-6 review (Codex P2): tracks the
                                        // `pivoted_once` key Step 4c inserts below (if it
                                        // pivots this cycle) so Step 4d-arm's denial branch
                                        // can remove it — see that branch for why. Every
                                        // `pivoted_this_key.take()` refusal hands the QSO's
                                        // Autonomous outcome to the queued request
                                        // (ownership rule: `report_unaired_frame`, PAN-185).
                                        let mut pivoted_this_key: Option<String> = None;

                                        // --- Step 4c: late pivot to the freshest message ---
                                        // Our decoder finishes ~1.8s BEFORE the slot
                                        // boundary, but a fresher decode for THIS QSO can
                                        // still land while this frame waited out the (up to
                                        // ~30s) pre-PTT sleep. If the QSO component has since
                                        // produced a newer message for this qso_id, swap to
                                        // it now and re-modulate. "Newer" covers a changed
                                        // OFFSET as well as changed text (PAN-72 round 5 —
                                        // an adaptive TX-offset switch relocates the QSO
                                        // without re-rendering the frame), which is why the
                                        // current `frequency_offset` is passed in alongside
                                        // `message_text`. We're at the slot boundary
                                        // — comfortably inside the ~1.5s switch budget — and
                                        // re-modulation is <100ms. tx_parity is unchanged (a
                                        // QSO holds one parity for its whole exchange), so the
                                        // schedule (pad/cursor) stays valid, and every FT8
                                        // frame is the same 79-symbol length so audio_out's
                                        // length (and audio_duration_ms) are unchanged.
                                        // Snapshot BEFORE the async mirror read. An offset switch
                                        // during the pivot or gate wait invalidates this frame,
                                        // even if its MessageToSend has not reached the mirror.
                                        let relocation_snapshot = [(
                                            qso_id.clone(),
                                            qso_id
                                                .as_deref()
                                                .map(|id| relocation_epochs.epoch(id))
                                                .unwrap_or(0),
                                        )];
                                        if let Some(intent) =
                                            latest_tx_intent.read().ok().and_then(|m| {
                                                super::tx_pivot_target(
                                                    qso_id.as_deref(),
                                                    &message_text,
                                                    frequency_offset,
                                                    // Round-5 review (Codex P1): an
                                                    // authorization-only rebind (identical
                                                    // resend text/offset, different
                                                    // origin/client) must count as a pivot
                                                    // too — see `tx_pivot_target`'s doc.
                                                    Some((origin, remote_client_key_id.as_deref())),
                                                    &m,
                                                )
                                            })
                                        {
                                            let new_text = intent.message_text;
                                            let new_freq = intent.frequency_offset;
                                            let new_origin = intent.origin;
                                            let new_remote_client_key_id =
                                                intent.remote_client_key_id;
                                            let new_attempt_id = intent.attempt_id;
                                            // TX-F4: protocol-aware re-encode/re-modulate
                                            // (mirrors Step 1's `encode_for_protocol` /
                                            // `modulate_for_protocol` call above) — the
                                            // legacy FT8-only `encode_message` /
                                            // `modulate_symbols` pair used here previously
                                            // would emit an FT8-shaped (151,680-sample)
                                            // waveform onto the FT4/FT2 grid on a pivot in
                                            // those modes (wrong length, wrong symbol
                                            // timing). FT8 is unaffected: `encode_for_protocol`
                                            // /`modulate_for_protocol` dispatch to the exact
                                            // legacy calls for `Protocol::Ft8`.
                                            let remod = match modulator.set_base_frequency(new_freq)
                                            {
                                                Ok(()) => encode_for_protocol(
                                                    &mut encoder,
                                                    active_protocol,
                                                    &new_text,
                                                )
                                                .and_then(|s| {
                                                    modulate_for_protocol(
                                                        &mut modulator,
                                                        active_protocol,
                                                        &s,
                                                        0.0,
                                                    )
                                                })
                                                .ok(),
                                                Err(_) => None,
                                            };
                                            match remod {
                                                Some(new_samples)
                                                    if schedule.cursor_offset_samples
                                                        < new_samples.len() =>
                                                {
                                                    let mut rebuilt = Vec::with_capacity(
                                                        schedule.silent_pad_samples
                                                            + new_samples.len(),
                                                    );
                                                    rebuilt.resize(
                                                        schedule.silent_pad_samples,
                                                        0.0f32,
                                                    );
                                                    rebuilt.extend_from_slice(
                                                        &new_samples
                                                            [schedule.cursor_offset_samples..],
                                                    );
                                                    info!(
                                                        target: "pancetta::tx.pivot",
                                                        "TX pivot: '{}' -> '{}' @{:.0}Hz for qso {} (fresher message arrived during pre-PTT wait)",
                                                        message_text,
                                                        new_text,
                                                        new_freq,
                                                        qso_id.as_deref().unwrap_or("-")
                                                    );
                                                    let replaced =
                                                        pancetta_qso::UnsentAttemptPayload {
                                                            frequency_hz: frequency_offset,
                                                            remote_binding: Some((
                                                                origin
                                                                    == crate::message_bus::TxOrigin::Remote,
                                                                remote_client_key_id.clone(),
                                                            )),
                                                        };
                                                    let pivoted_from_text = std::mem::replace(
                                                        &mut message_text,
                                                        new_text,
                                                    );
                                                    frequency_offset = new_freq;
                                                    audio_out = rebuilt;
                                                    // Round-4 review (Codex P1): the
                                                    // authorization binding is replaced
                                                    // ATOMICALLY with the payload above — never
                                                    // partially, e.g. adopting a newer REMOTE
                                                    // payload while leaving `origin` at its
                                                    // stale `Local` value, which would skip the
                                                    // arm gate entirely rather than merely
                                                    // misattribute it. See `LatestTxIntent`'s
                                                    // doc.
                                                    origin = new_origin;
                                                    remote_client_key_id = new_remote_client_key_id;
                                                    // PAN-183: the keyed frame is now the
                                                    // intent's attempt; the replaced one's is
                                                    // kept below for its not-sent report.
                                                    let replaced_attempt_id = std::mem::replace(
                                                        &mut attempt_id,
                                                        new_attempt_id,
                                                    );
                                                    // Double-PTT fix: record this pivot so the
                                                    // newer request that PRODUCED `message_text`
                                                    // — still queued behind this one — is
                                                    // recognized as an already-sent duplicate
                                                    // (Step 0-dup above) instead of keying PTT a
                                                    // second time for the same text. `qso_id` is
                                                    // guaranteed `Some` here: `tx_pivot_target`
                                                    // (mod.rs) only returns `Some` when the
                                                    // `qso_id` argument is `Some` (manual/tune/
                                                    // test-TX with `qso_id == None` are never
                                                    // pivoted), so this `if let` always matches
                                                    // for a `None` id; guarded defensively anyway.
                                                    if let Some(id) = qso_id.as_deref() {
                                                        let key = super::active_tx_qso_key(id);
                                                        pivoted_once.insert(
                                                            key.clone(),
                                                            (
                                                                message_text.clone(),
                                                                frequency_offset,
                                                            ),
                                                        );
                                                        pivoted_from.insert(
                                                            key.clone(),
                                                            (
                                                                pivoted_from_text,
                                                                replaced,
                                                                replaced_attempt_id,
                                                            ),
                                                        );
                                                        pivoted_this_key = Some(key);
                                                    }
                                                }
                                                _ => {
                                                    warn!(
                                                    "TX pivot re-modulate failed for '{}' — keeping original '{}'",
                                                    new_text, message_text
                                                );
                                                }
                                            }
                                        }

                                        // --- Step 5: Assert PTT ---
                                        #[cfg(test)]
                                        tx_before_ptt_gate.notify_one();
                                        let mut ptt_guard = PttGuard::new(
                                            message_bus.clone(),
                                            ptt_active.clone(),
                                            &ptt_sync_gate,
                                            &last_ptt_on_ms,
                                            &last_ptt_off_ms,
                                            &pending_ptt_off,
                                        )
                                        .await;
                                        // TX badge on; guard drop clears it on every
                                        // exit path (complete / abort / shutdown).
                                        let _tx_status_guard =
                                            TxStatusGuard::new(message_bus.clone());
                                        send_tx_status(&message_bus, true).await;
                                        // NOW-SENDING: this message is keyed and on the air.
                                        send_tx_queue_status(
                                            &message_bus,
                                            Some(crate::message_bus::TxItem {
                                                text: message_text.clone(),
                                                freq_hz: frequency_offset,
                                                qso_id: qso_id.clone(),
                                                deferred: false,
                                            }),
                                            Vec::new(),
                                        )
                                        .await;

                                        let live_timing_guard = ptt_sync_gate.lock().await;
                                        if is_autonomous_cq(
                                            qso_id.as_deref(),
                                            attempt_id,
                                            &message_text,
                                        ) && !autonomous_config_enabled.load(Ordering::Acquire)
                                        {
                                            ptt_guard.disarm();
                                            ptt_active.store(false, Ordering::Release);
                                            drop(live_timing_guard);
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }
                                        // --- Step 4d: final atomic pre-PTT gate ---
                                        // PAN-148: Step 4b/4b-parity's liveness/DX-parity
                                        // evidence is read BEFORE Step 4c's pivot/remodulation
                                        // and the two status-announce awaits just above — any
                                        // of which can span the moment this QSO ends or its DX
                                        // moves onto the exact parity we're about to key into
                                        // (Codex P2, PR #370 round 4). Re-check both, here,
                                        // against fresh state, immediately before the arm
                                        // recheck and the PTT-on send — nothing else runs
                                        // between this call and the key. SYNCHRONOUS (uses
                                        // `final_gate_callsign`, resolved before Step 3's
                                        // trim above — see that call site's comment).
                                        // PAN-117: the slot-timing re-check is NOT part of
                                        // this gate — it reads the clock, so it runs last,
                                        // just before the PTT-on send below (see
                                        // `key_time_gate_denial`).
                                        // PAN-145: the gate can have yielded to a relocation
                                        // after Step 4c read the intent. Refuse its old waveform
                                        // before any PTT-on send; do not rebuild under the gate.
                                        if relocation_epoch_stale(
                                            &relocation_epochs,
                                            &relocation_snapshot,
                                        ) {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            let diagnostic = format!(
                                                "TX blocked at final pre-PTT check (QSO relocated during gate wait): '{message_text}'"
                                            );
                                            info!(target: "pancetta::tx.policy", "{diagnostic}");
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                diagnostic,
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        if let Some(denial) = final_single_tx_gate_denial(
                                            qso_id.as_deref(),
                                            required_parity,
                                            DxParityContext::live(&operating_frequency_hz, slot_ns),
                                            &active_tx_qsos,
                                            final_gate_callsign.as_deref(),
                                            &cross_time_state,
                                        ) {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            // Same tombstone cleanup as the arm-denial branch
                                            // below — nothing reached the air.
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            let (log_msg, diag_msg) = final_gate_denial_text(
                                                denial,
                                                qso_id.as_deref(),
                                                required_parity,
                                                &message_text,
                                            );
                                            info!(target: "pancetta::tx.policy", "{log_msg}");
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                diag_msg,
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // PAN-115: re-run the TX hard mute here too. The
                                        // post-wait check above runs before Step 4c and the
                                        // status awaits, so a rig CAT loss or dead audio
                                        // output (or any other hard-mute reason) landing in
                                        // that window would otherwise still key. SYNCHRONOUS
                                        // (atomics + mutex), so "nothing else runs between
                                        // this call and the key" still holds. Same unwind as
                                        // Step 4d: nothing reached the air.
                                        if let Some(reason) = tx_hard_mute_reason(
                                            &tx_policy,
                                            &tx_restart_inhibit,
                                            &hamlib_command_loop_ready,
                                            &hamlib_pending_frequency,
                                            &hamlib_pending_split,
                                            &hamlib_command_in_flight,
                                            &rig_conn_state,
                                            &audio_output_alive,
                                            &message_bus.receive_gain,
                                            &message_bus.data_mode,
                                        ) {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "TX blocked at final pre-PTT check ({reason}): '{message_text}' (qso: {:?})",
                                                qso_id
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "TX blocked at final pre-PTT check ({reason}): '{message_text}'"
                                                ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // PAN-118: the band-plan sibling of the
                                        // final pre-PTT mute above — nothing else
                                        // runs between this synchronous check
                                        // (atomics + a local struct snapshot) and
                                        // the key. Same unwind: nothing reached
                                        // the air, so the frame is reported
                                        // unaired, not failed-complete.
                                        if let Some(reason) = tx_rf_for_gate(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                            frequency_offset,
                                        )
                                        .and_then(|rf| {
                                            tx_band_plan_mute_reason(
                                                rf,
                                                tx_signal_span_hz(active_protocol),
                                                &band_plan,
                                            )
                                        }) {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "TX blocked at final pre-PTT check ({reason}): '{message_text}' (qso: {:?})",
                                                qso_id
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "TX blocked at final pre-PTT check ({reason}): '{message_text}'"
                                                ),
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        // PAN-117 (PR #450 Codex review): the slot-timing
                                        // half of Step 4d, run LAST among the pre-send
                                        // gates — after the liveness/DX-parity gate, the
                                        // hard mute and the band-plan gate above — so
                                        // `utc_clock()` here is the last clock read
                                        // before the PTT-on send. A frame more than
                                        // `tx_late_max_ms` into its target slot is dropped
                                        // here instead of keyed (no receiver could decode
                                        // the surviving fragment). SYNCHRONOUS through the
                                        // key-down enqueue below: `send_ptt_command` takes
                                        // no lock and no await (PAN-202).
                                        if let Some(denial) = key_time_gate_denial(
                                            utc_clock(),
                                            schedule.target_slot,
                                            timing.late_cap_ms(),
                                        ) {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            let (log_msg, diag_msg) = final_gate_denial_text(
                                                denial,
                                                qso_id.as_deref(),
                                                required_parity,
                                                &message_text,
                                            );
                                            info!(target: "pancetta::tx.policy", "{log_msg}");
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                diag_msg,
                                                qso_id.as_deref(),
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                message_text,
                                                qso_id.clone(),
                                                attempt_id,
                                                pivot_replaced,
                                            )
                                            .await;
                                            continue 'worker;
                                        }

                                        #[cfg(test)]
                                        key_down_probe.pause_if_armed().await;

                                        // --- Step 4d-arm: last-instant pre-PTT arm recheck (PAN-134)
                                        // Authorization runs immediately before the enqueue, with no lock
                                        // or await between it, the gates above and the enqueue (PAN-134,
                                        // PAN-202). The mid-TX poll remains a backstop.
                                        // PTT hardware is not asserted yet. Denial only unwinds the local
                                        // PTT flag, pivot tombstones, and the announced TX/queue status.
                                        let outcome = send_ptt_command(
                                            &message_bus,
                                            ComponentId::Ft8Transmitter,
                                            true,
                                            || {
                                                tx_send_authorized(
                                                    origin,
                                                    &remote_tx_arm,
                                                    remote_client_key_id.as_deref(),
                                                )
                                            },
                                        );
                                        drop(live_timing_guard);
                                        match outcome {
                                            SendIfOutcome::Enqueued => {
                                                info!(
                                                    target: "pancetta::tx.ptt",
                                                    "PTT ON (scheduled TX) sent to rig: '{}' @{:.0}Hz qso={}",
                                                    message_text,
                                                    frequency_offset,
                                                    qso_id.as_deref().unwrap_or("-")
                                                );
                                            }
                                            SendIfOutcome::Denied => {
                                                last_ptt_off_ms.store(
                                                    super::now_epoch_ms(),
                                                    Ordering::Release,
                                                );
                                                ptt_active.store(false, Ordering::Release);
                                                ptt_guard.disarm();
                                                // Round-6 review (Codex P2): if Step 4c pivoted
                                                // this cycle, remove the tombstone it inserted —
                                                // nothing actually reached the air, so the
                                                // genuinely fresh request still queued behind
                                                // this one must NOT be discarded by Step 0-dup as
                                                // an already-sent duplicate of a pivot that never
                                                // transmitted.
                                                let pivot_replaced =
                                                    pivoted_this_key.take().and_then(|key| {
                                                        pivoted_once.remove(&key);
                                                        pivoted_from.remove(&key)
                                                    });
                                                let denial_reason = format!(
                                                    "arm went stale in the pre-PTT gap: '{message_text}' at {frequency_offset:.0} Hz"
                                                );
                                                let operator_callsign =
                                                    tx_denied_operator_attribution(
                                                        &remote_tx_arm,
                                                        remote_client_key_id.as_deref(),
                                                    );
                                                emit_disarm_interrupt_signals(
                                                    &message_bus,
                                                    &audit_log,
                                                    &display_feed_enabled,
                                                    operator_callsign,
                                                    "denied",
                                                    denial_reason,
                                                    qso_id.as_deref(),
                                                )
                                                .await;
                                                send_tx_queue_status(
                                                    &message_bus,
                                                    None,
                                                    Vec::new(),
                                                )
                                                .await;
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    message_text,
                                                    qso_id.clone(),
                                                    attempt_id,
                                                    pivot_replaced,
                                                )
                                                .await;
                                                continue 'worker;
                                            }
                                            // PAN-278: the bus dropped the key-down (full or
                                            // disconnected Hamlib channel), so the rig was
                                            // never keyed. Unwind like a pre-PTT refusal:
                                            // nothing entered Hamlib's queue, so there is no
                                            // PTT-off to send. Report the frame as not sent and
                                            // play no audio. Without a Hamlib route (`--replay`)
                                            // every PTT-on is dropped by design and the audio still goes out,
                                            // so that case keeps the arm below.
                                            SendIfOutcome::Dropped
                                                if message_bus.has_rig_route() =>
                                            {
                                                last_ptt_off_ms.store(
                                                    super::now_epoch_ms(),
                                                    Ordering::Release,
                                                );
                                                ptt_active.store(false, Ordering::Release);
                                                ptt_guard.disarm();
                                                let pivot_replaced =
                                                    pivoted_this_key.take().and_then(|key| {
                                                        pivoted_once.remove(&key);
                                                        pivoted_from.remove(&key)
                                                    });
                                                warn!(
                                                    target: "pancetta::tx.ptt",
                                                    "PTT-on dropped by the bus (rig not keyed): '{}' not sent",
                                                    message_text
                                                );
                                                emit_diagnostic(
                                                    &message_bus,
                                                    "tx.ptt",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    format!(
                                                        "PTT-on dropped (rig not keyed): '{message_text}' not sent"
                                                    ),
                                                    qso_id.as_deref(),
                                                )
                                                .await;
                                                send_tx_queue_status(
                                                    &message_bus,
                                                    None,
                                                    Vec::new(),
                                                )
                                                .await;
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    message_text,
                                                    qso_id.clone(),
                                                    attempt_id,
                                                    pivot_replaced,
                                                )
                                                .await;
                                                continue 'worker;
                                            }
                                            SendIfOutcome::Dropped => {
                                                warn!("PTT ON not sent: no Hamlib route registered (replay mode)");
                                            }
                                        }

                                        // --- Step 6: Sleep precisely until target slot start ---
                                        // (audio_out itself includes any silent_pad needed past
                                        // the slot boundary; we send it at the boundary.)
                                        let to_slot = pancetta_core::slot::duration_until(
                                            schedule.target_slot,
                                            utc_clock(),
                                        );
                                        // PAN-114 (D6): a backward clock step between Step 4
                                        // and here would hold PTT keyed for the size of the
                                        // step (measured: 600 s). Never hold it past
                                        // `ptt_lead_ms` + PTT_HOLD_SLACK_MS; release PTT
                                        // FIRST, exactly like `SleepOutcome::AbortedByDisarm`.
                                        if ptt_hold_exceeds_bound(to_slot, ptt_lead_ms) {
                                            ptt_guard.release("Clock-step PTT hold guard").await;
                                            // Pre-slot abort: nothing reached the air, so a
                                            // Step 4c pivot tombstone must go too (same as
                                            // the AbortedByDisarm branch below).
                                            let pivot_replaced =
                                                pivoted_this_key.take().and_then(|key| {
                                                    pivoted_once.remove(&key);
                                                    pivoted_from.remove(&key)
                                                });
                                            drop_frames_for_clock_jump(
                                                &message_bus,
                                                &[(
                                                    message_text.clone(),
                                                    qso_id.clone(),
                                                    attempt_id,
                                                )],
                                                pancetta_core::slot_clock::WallClockJump::Backward(
                                                    chrono::Duration::from_std(to_slot)
                                                        .unwrap_or(chrono::Duration::MAX),
                                                ),
                                                &mut qso_id
                                                    .as_deref()
                                                    .zip(pivot_replaced)
                                                    .map(|(id, r)| {
                                                        (super::active_tx_qso_key(id), r)
                                                    })
                                                    .into_iter()
                                                    .collect(),
                                            )
                                            .await;
                                            continue 'worker;
                                        }
                                        // ptt_guard in scope — drop on any loop exit fires
                                        // PTT-off. A qualifying request arriving here supersedes
                                        // the in-flight frame (aborts + re-keys).
                                        let outcome = interruptible_sleep_or_supersede_with_clock(
                                            to_slot,
                                            &shutdown,
                                            &abort_current_tx,
                                            &inbox,
                                            std::slice::from_ref(
                                                &crate::message_bus::TransmitRequestItem {
                                                    attempt_id,
                                                    message_text: message_text.clone(),
                                                    frequency_offset,
                                                    qso_id: qso_id.clone(),
                                                    origin,
                                                    remote_client_key_id: remote_client_key_id
                                                        .clone(),
                                                },
                                            ),
                                            &pivoted_once,
                                            remote_arm_gate(
                                                origin,
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            ),
                                            arm_clock.as_ref(),
                                        )
                                        .await;
                                        match on_sleep_outcome(
                                            outcome,
                                            SleepPhase::PreSlot,
                                            SleepInFlight::Single {
                                                message_text: &message_text,
                                                frequency_offset,
                                                qso_id: qso_id.as_deref(),
                                                attempt_id,
                                                pivoted_this_key: &mut pivoted_this_key,
                                            },
                                            AbortCtx {
                                                message_bus: &message_bus,
                                                audit_log: &audit_log,
                                                display_feed_enabled: &display_feed_enabled,
                                                remote_tx_arm: &remote_tx_arm,
                                                remote_client_key_id: remote_client_key_id
                                                    .as_deref(),
                                                ptt_guard: &mut ptt_guard,
                                                pivoted_once: &mut pivoted_once,
                                                pivoted_from: &mut pivoted_from,
                                                shutdown: &shutdown,
                                                sample_rate,
                                            },
                                        )
                                        .await
                                        {
                                            SleepFlow::Continue(()) => {}
                                            SleepFlow::Break(SleepBreak::Exit(
                                                TxExit::StopWorker,
                                            )) => {
                                                break 'worker;
                                            }
                                            SleepFlow::Break(SleepBreak::Exit(
                                                TxExit::NextRequest,
                                            )) => {
                                                continue 'worker;
                                            }
                                            SleepFlow::Break(SleepBreak::Superseded(boxed)) => {
                                                let new_request = *boxed;
                                                // The current in-flight single item — passed to the
                                                // helper so a bundle-add (max_concurrent_qsos > 1)
                                                // can fold the new request alongside it.
                                                let in_flight_items =
                                                    vec![crate::message_bus::TransmitRequestItem {
                                                        attempt_id,
                                                        message_text: message_text.clone(),
                                                        frequency_offset,
                                                        qso_id: qso_id.clone(),
                                                        origin,
                                                        remote_client_key_id: remote_client_key_id
                                                            .clone(),
                                                    }];
                                                match supersede_and_rekey_or_bundle(
                                                    new_request,
                                                    WorkingFrame {
                                                        message_text: &mut message_text,
                                                        frequency_offset: &mut frequency_offset,
                                                        schedule: &mut schedule,
                                                    },
                                                    &mut ptt_guard,
                                                    &timing,
                                                    live_supersede_max_concurrent(),
                                                    &SupersedeInFlight {
                                                        items: &in_flight_items,
                                                        origin,
                                                        remote_client_key_id: remote_client_key_id
                                                            .clone(),
                                                        tx_parity,
                                                    },
                                                )
                                                .await
                                                {
                                                    SupersedeOutcome::NotViable { superseder } => {
                                                        // Too late to re-key this slot — PTT already
                                                        // off; abandon THIS message (not the worker):
                                                        // leave the retry loop and fall through to
                                                        // the next dequeue. The armed PttGuard's
                                                        // drop is the PTT-off safety net if the
                                                        // supersede's release was not delivered.
                                                        // PAN-75: the abandoned frame gets the same
                                                        // failed completion as the `Replace` arm's
                                                        // below, so the QSO engine hands back its
                                                        // keep-call bookkeeping.
                                                        // PAN-279: the superseding request goes back on the front of the inbox (after the
                                                        // siphoned traffic was restored) and airs in its own next slot. The abandoned frame
                                                        // settles first, leaving the Autonomous outcome to a same-QSO superseder.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::successor_qso_keys(
                                                                &superseder,
                                                            ),
                                                            carried_items(&superseder),
                                                        )
                                                        .await;
                                                        inbox.requeue_front(vec![self_message(
                                                            *superseder,
                                                        )]);
                                                        break 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Replace {
                                                        origin: new_origin,
                                                        remote_client_key_id:
                                                            new_remote_client_key_id,
                                                        qso_id: new_qso_id,
                                                        identity_conflict: _,
                                                        tx_parity: _,
                                                        attempt_id: new_attempt_id,
                                                    } => {
                                                        // Viable single-item re-key (Task 6): carry
                                                        // the recomputed schedule into the retry,
                                                        // flush stale audio at Step 7, and clear the
                                                        // abort flag the supersede set. The supersede
                                                        // already released this iteration's PttGuard
                                                        // (PAN-273; Step 5 re-asserts on retry).
                                                        // Re-point `origin` at the SUPERSEDING
                                                        // request's origin so Step 4b-arm gates the
                                                        // frame that is actually about to transmit,
                                                        // not the aborted one (C1 fix). PAN-38
                                                        // round 4: re-point `qso_id` too, for the
                                                        // same reason -- otherwise the retry pairs
                                                        // the superseding frame's text with the
                                                        // ABORTED frame's qso_id.
                                                        // PAN-38 round 5 (Codex), PAN-279: the ABANDONED
                                                        // frame (in_flight_items[0], captured before
                                                        // this supersede) settles now, before the
                                                        // qso_id local is overwritten below. It always
                                                        // rolls back its own attempt with the QSO
                                                        // engine, and fails to Autonomous only when
                                                        // the re-keyed request is for another QSO or
                                                        // none: a same-QSO re-key owns the outcome.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::qso_keys([
                                                                new_qso_id.as_deref()
                                                            ]),
                                                            &[],
                                                        )
                                                        .await;
                                                        origin = new_origin;
                                                        remote_client_key_id =
                                                            new_remote_client_key_id;
                                                        qso_id = new_qso_id;
                                                        attempt_id = new_attempt_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        continue 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Bundle {
                                                        items,
                                                        bundle_origin,
                                                        bundle_remote_client_key_id,
                                                        new_origin,
                                                        new_remote_client_key_id,
                                                        new_attempt_id,
                                                    } => {
                                                        // Prefer a multi-TX bundle: encode the
                                                        // in-flight + new item together. On success,
                                                        // hand it back to the worker as a fresh
                                                        // MultiTransmitRequest (picked up by the
                                                        // unmodified multi-TX arm) and abandon this
                                                        // single-TX retry. On a frequency collision,
                                                        // fall back to the single-item replace — the
                                                        // working state was already mutated to the
                                                        // new item by the helper.
                                                        let tx_params =
                                                            pancetta_ft8::ProtocolParams::from_protocol(
                                                                active_protocol,
                                                            );
                                                        let bundle_outcome =
                                                            encode_and_modulate_multi_tx(
                                                                &mut encoder,
                                                                active_protocol,
                                                                &tx_params,
                                                                &items,
                                                            );
                                                        if bundle_outcome.samples.is_ok() {
                                                            let successor =
                                                                MessageType::MultiTransmitRequest {
                                                                    items,
                                                                    // PR #348 review round 6 (Codex
                                                                    // P1): carry the in-flight
                                                                    // single item's own established
                                                                    // parity forward, not None --
                                                                    // same reasoning as
                                                                    // supersede_multi_reenqueue's
                                                                    // bundle_tx_parity fix.
                                                                    tx_parity,
                                                                    // Fail-safe folded origin, not
                                                                    // the aborted frame's origin.
                                                                    origin: bundle_origin,
                                                                    remote_client_key_id:
                                                                        bundle_remote_client_key_id,
                                                                };
                                                            // The supersede winner goes ahead of the siphoned traffic already on the front (PAN-73 round 4).
                                                            inbox.requeue_front(vec![
                                                                self_message(successor.clone()),
                                                            ]);
                                                            // PAN-279: the in-flight frame the bundle replaced (its QSO's
                                                            // older frame) settles; one it carries unchanged airs as itself.
                                                            settle_items_abandoned_for(
                                                                &message_bus,
                                                                &in_flight_items,
                                                                &successor,
                                                                None,
                                                            )
                                                            .await;
                                                            break 'key_and_send;
                                                        }
                                                        // Frequency collision: single-item replace
                                                        // of the NEW item only, so gate with the new
                                                        // request's own origin (C1 fix). PAN-38
                                                        // round 5 (Codex): carry its qso_id too --
                                                        // `items.last()` is always the new request
                                                        // (per this variant's doc comment), and the
                                                        // working `message_text`/`frequency_offset`
                                                        // were already mutated to it by
                                                        // `supersede_and_rekey_or_bundle` -- without
                                                        // this, the retry paired the new item's text
                                                        // with the ABORTED frame's qso_id, same
                                                        // failure shape as the `Replace` arm's fix.
                                                        // PAN-38 round 5: report the abandoned
                                                        // in-flight frame's TransmitComplete before
                                                        // overwriting qso_id -- same reasoning as
                                                        // the Replace arm's fix.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::qso_keys([items
                                                                .last()
                                                                .and_then(|item| {
                                                                    item.qso_id.as_deref()
                                                                })]),
                                                            &[],
                                                        )
                                                        .await;
                                                        origin = new_origin;
                                                        remote_client_key_id =
                                                            new_remote_client_key_id;
                                                        qso_id = items
                                                            .last()
                                                            .and_then(|item| item.qso_id.clone());
                                                        attempt_id = new_attempt_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        continue 'key_and_send;
                                                    }
                                                }
                                            }
                                        }

                                        // --- Step 7: Route audio to output ---
                                        // PAN-134: recheck authorization after the bus lock wait, with no
                                        // suspension before audio enqueue. PTT is already asserted, so
                                        // denial releases it first. The mid-TX poll remains a backstop.
                                        // Round-9 review: denied frames must never appear in Band Activity
                                        // as genuine own-transmissions. Log only on Enqueued, so dropped
                                        // audio cannot appear as an aired frame either.
                                        let (playback_ack, mut playback_waiter) =
                                            PlaybackStartAck::new();
                                        let audio_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Audio,
                                            MessageType::AudioOutput {
                                                samples: audio_out,
                                                sample_rate,
                                                // On a re-key, flush the aborted transmission's
                                                // still-buffered samples before queuing the new
                                                // content (Task 2/3). `false` on the normal first
                                                // pass keeps that path byte-identical.
                                                flush_first: is_rekey,
                                                playback_started: Some(playback_ack),
                                            },
                                            Instant::now(),
                                        );
                                        let outcome = message_bus
                                            .send_message_if(audio_msg, || {
                                                tx_send_authorized(
                                                    origin,
                                                    &remote_tx_arm,
                                                    remote_client_key_id.as_deref(),
                                                )
                                            })
                                            .await;
                                        // PR #468 review: playback is timed from the enqueue, so the
                                        // bounded TX-log wait below never extends key-down past the audio.
                                        let audio_enqueued_at = tokio::time::Instant::now();
                                        // PAN-182 (PR #507 review): only an `Enqueued` outcome put
                                        // samples on the Audio route. A `Dropped`/error delivery keyed
                                        // no audio, so Step 10 must report the frame as not sent.
                                        let audio_enqueued =
                                            matches!(outcome, Ok(SendIfOutcome::Enqueued));
                                        match outcome {
                                            Ok(SendIfOutcome::Enqueued) => {
                                                // PAN-71: report the callback's first sample, with a bounded
                                                // wait and enqueue-time fallback. Only authorized, enqueued
                                                // frames are logged (PAN-134). The durable record precedes
                                                // the TUI event and uses the same instant (PAN-122).
                                                let enqueued_utc = utc_clock();
                                                let tx_logged_at = frame_log_instant(
                                                    &mut playback_waiter,
                                                    enqueued_utc,
                                                    &message_text,
                                                )
                                                .await;
                                                let _ = tx_frame_log
                                                    .append(
                                                        vec![TxFrameRecord::new(
                                                            tx_logged_at,
                                                            active_protocol,
                                                            tx_log_dial_hz(
                                                                &operating_frequency_hz,
                                                                &split_tx_frequency_hz,
                                                            ),
                                                            frequency_offset,
                                                            message_text.clone(),
                                                            origin,
                                                            qso_id.clone(),
                                                        )],
                                                        TX_LOG_ACK_WAIT,
                                                    )
                                                    .await;
                                                log_tx_frame(
                                                    &message_bus,
                                                    message_text.clone(),
                                                    frequency_offset,
                                                    qso_id.clone(),
                                                    tx_logged_at,
                                                )
                                                .await;
                                            }
                                            Ok(SendIfOutcome::Denied) => {
                                                // PAN-135: stand the guard down only after
                                                // Hamlib accepts PTT-off (checked, bounded
                                                // retry — see `PttGuard::release`).
                                                ptt_guard
                                                    .release("Remote disarm before audio delivery")
                                                    .await;
                                                let pivot_replaced =
                                                    pivoted_this_key.take().and_then(|key| {
                                                        pivoted_once.remove(&key);
                                                        pivoted_from.remove(&key)
                                                    });
                                                let denial_reason = format!(
                                                    "arm went stale before audio delivery: '{message_text}' at {frequency_offset:.0} Hz"
                                                );
                                                let operator_callsign =
                                                    tx_denied_operator_attribution(
                                                        &remote_tx_arm,
                                                        remote_client_key_id.as_deref(),
                                                    );
                                                emit_disarm_interrupt_signals(
                                                    &message_bus,
                                                    &audit_log,
                                                    &display_feed_enabled,
                                                    operator_callsign,
                                                    "denied",
                                                    denial_reason,
                                                    qso_id.as_deref(),
                                                )
                                                .await;
                                                send_tx_queue_status(
                                                    &message_bus,
                                                    None,
                                                    Vec::new(),
                                                )
                                                .await;
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    message_text,
                                                    qso_id.clone(),
                                                    attempt_id,
                                                    pivot_replaced,
                                                )
                                                .await;
                                                continue 'worker;
                                            }
                                            Ok(SendIfOutcome::Dropped) => {
                                                debug!("Audio output routing: message dropped");
                                            }
                                            Err(e) => {
                                                debug!("Audio output routing: {}", e);
                                            }
                                        }

                                        // --- Step 8: Wait for audio playback to complete ---
                                        // Playback is now on the air; a qualifying request here
                                        // supersedes it (aborts + re-keys), flushing the still-
                                        // playing samples on the retry's Step 7.
                                        // PAN-135: expected playback end bounds an unconfirmed disarm flush.
                                        let playback_end = audio_enqueued_at
                                            + Duration::from_millis(audio_duration_ms);
                                        let outcome = interruptible_sleep_or_supersede_with_clock(
                                            playback_end.saturating_duration_since(
                                                tokio::time::Instant::now(),
                                            ),
                                            &shutdown,
                                            &abort_current_tx,
                                            &inbox,
                                            std::slice::from_ref(
                                                &crate::message_bus::TransmitRequestItem {
                                                    attempt_id,
                                                    message_text: message_text.clone(),
                                                    frequency_offset,
                                                    qso_id: qso_id.clone(),
                                                    origin,
                                                    remote_client_key_id: remote_client_key_id
                                                        .clone(),
                                                },
                                            ),
                                            &pivoted_once,
                                            remote_arm_gate(
                                                origin,
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            ),
                                            arm_clock.as_ref(),
                                        )
                                        .await;
                                        match on_sleep_outcome(
                                            outcome,
                                            SleepPhase::Playback { playback_end },
                                            SleepInFlight::Single {
                                                message_text: &message_text,
                                                frequency_offset,
                                                qso_id: qso_id.as_deref(),
                                                attempt_id,
                                                pivoted_this_key: &mut pivoted_this_key,
                                            },
                                            AbortCtx {
                                                message_bus: &message_bus,
                                                audit_log: &audit_log,
                                                display_feed_enabled: &display_feed_enabled,
                                                remote_tx_arm: &remote_tx_arm,
                                                remote_client_key_id: remote_client_key_id
                                                    .as_deref(),
                                                ptt_guard: &mut ptt_guard,
                                                pivoted_once: &mut pivoted_once,
                                                pivoted_from: &mut pivoted_from,
                                                shutdown: &shutdown,
                                                sample_rate,
                                            },
                                        )
                                        .await
                                        {
                                            SleepFlow::Continue(()) => {}
                                            SleepFlow::Break(SleepBreak::Exit(
                                                TxExit::StopWorker,
                                            )) => {
                                                break 'worker;
                                            }
                                            SleepFlow::Break(SleepBreak::Exit(
                                                TxExit::NextRequest,
                                            )) => {
                                                continue 'worker;
                                            }
                                            SleepFlow::Break(SleepBreak::Superseded(boxed)) => {
                                                let new_request = *boxed;
                                                // Same bundle-add-or-replace decision as Step 6.
                                                let in_flight_items =
                                                    vec![crate::message_bus::TransmitRequestItem {
                                                        attempt_id,
                                                        message_text: message_text.clone(),
                                                        frequency_offset,
                                                        qso_id: qso_id.clone(),
                                                        origin,
                                                        remote_client_key_id: remote_client_key_id
                                                            .clone(),
                                                    }];
                                                match supersede_and_rekey_or_bundle(
                                                    new_request,
                                                    WorkingFrame {
                                                        message_text: &mut message_text,
                                                        frequency_offset: &mut frequency_offset,
                                                        schedule: &mut schedule,
                                                    },
                                                    &mut ptt_guard,
                                                    &timing,
                                                    live_supersede_max_concurrent(),
                                                    &SupersedeInFlight {
                                                        items: &in_flight_items,
                                                        origin,
                                                        remote_client_key_id: remote_client_key_id
                                                            .clone(),
                                                        tx_parity,
                                                    },
                                                )
                                                .await
                                                {
                                                    SupersedeOutcome::NotViable { superseder } => {
                                                        // Too late to re-key this slot — abandon
                                                        // THIS message (not the worker): leave the
                                                        // retry loop and fall through to next
                                                        // dequeue.
                                                        // PAN-75: report the abandoned frame as
                                                        // the Step 6 `NotViable` arm does.
                                                        // PAN-279: the superseding request goes back on the front of the inbox (after the
                                                        // siphoned traffic was restored) and airs in its own next slot. The abandoned frame
                                                        // settles first, leaving the Autonomous outcome to a same-QSO superseder.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::successor_qso_keys(
                                                                &superseder,
                                                            ),
                                                            carried_items(&superseder),
                                                        )
                                                        .await;
                                                        inbox.requeue_front(vec![self_message(
                                                            *superseder,
                                                        )]);
                                                        break 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Replace {
                                                        origin: new_origin,
                                                        remote_client_key_id:
                                                            new_remote_client_key_id,
                                                        qso_id: new_qso_id,
                                                        identity_conflict: _,
                                                        tx_parity: _,
                                                        attempt_id: new_attempt_id,
                                                    } => {
                                                        // Re-point `origin` at the superseding
                                                        // request's origin so the retry's Step 4b-arm
                                                        // gates the frame actually transmitting (C1).
                                                        // PAN-38 round 4: re-point `qso_id` too.
                                                        // PAN-38 round 5 (Codex), PAN-279: the ABANDONED
                                                        // frame (in_flight_items[0], captured before
                                                        // this supersede) settles now, before the
                                                        // qso_id local is overwritten below. It always
                                                        // rolls back its own attempt with the QSO
                                                        // engine, and fails to Autonomous only when
                                                        // the re-keyed request is for another QSO or
                                                        // none: a same-QSO re-key owns the outcome.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::qso_keys([
                                                                new_qso_id.as_deref()
                                                            ]),
                                                            &[],
                                                        )
                                                        .await;
                                                        origin = new_origin;
                                                        remote_client_key_id =
                                                            new_remote_client_key_id;
                                                        qso_id = new_qso_id;
                                                        attempt_id = new_attempt_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        continue 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Bundle {
                                                        items,
                                                        bundle_origin,
                                                        bundle_remote_client_key_id,
                                                        new_origin,
                                                        new_remote_client_key_id,
                                                        new_attempt_id,
                                                    } => {
                                                        let tx_params =
                                                            pancetta_ft8::ProtocolParams::from_protocol(
                                                                active_protocol,
                                                            );
                                                        let bundle_outcome =
                                                            encode_and_modulate_multi_tx(
                                                                &mut encoder,
                                                                active_protocol,
                                                                &tx_params,
                                                                &items,
                                                            );
                                                        if bundle_outcome.samples.is_ok() {
                                                            let successor =
                                                                MessageType::MultiTransmitRequest {
                                                                    items,
                                                                    // PR #348 review round 6 (Codex
                                                                    // P1): carry the in-flight
                                                                    // single item's own established
                                                                    // parity forward, not None.
                                                                    tx_parity,
                                                                    // Fail-safe folded origin.
                                                                    origin: bundle_origin,
                                                                    remote_client_key_id:
                                                                        bundle_remote_client_key_id,
                                                                };
                                                            // The supersede winner goes ahead of the siphoned traffic already on the front (PAN-73 round 4).
                                                            inbox.requeue_front(vec![
                                                                self_message(successor.clone()),
                                                            ]);
                                                            // PAN-279: the in-flight frame the bundle replaced (its QSO's
                                                            // older frame) settles; one it carries unchanged airs as itself.
                                                            settle_items_abandoned_for(
                                                                &message_bus,
                                                                &in_flight_items,
                                                                &successor,
                                                                None,
                                                            )
                                                            .await;
                                                            break 'key_and_send;
                                                        }
                                                        // Frequency collision: single-item replace
                                                        // of the new item — gate with its own origin.
                                                        // PAN-38 round 5: carry its qso_id too, same
                                                        // reasoning as the other Bundle fallback arm.
                                                        // Also report the abandoned in-flight
                                                        // frame's TransmitComplete before
                                                        // overwriting qso_id.
                                                        settle_abandoned_single(
                                                            &message_bus,
                                                            &in_flight_items,
                                                            &tx_outcome::qso_keys([items
                                                                .last()
                                                                .and_then(|item| {
                                                                    item.qso_id.as_deref()
                                                                })]),
                                                            &[],
                                                        )
                                                        .await;
                                                        origin = new_origin;
                                                        remote_client_key_id =
                                                            new_remote_client_key_id;
                                                        qso_id = items
                                                            .last()
                                                            .and_then(|item| item.qso_id.clone());
                                                        attempt_id = new_attempt_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        continue 'key_and_send;
                                                    }
                                                }
                                            }
                                        }
                                        // PAN-182 (PR #507 review): success means the audio actually
                                        // enqueued at Step 7 — a `Dropped`/error delivery keyed no
                                        // samples, so the frame never aired and Step 10 reports it
                                        // through the PAN-75 not-sent path instead.
                                        let success = audio_enqueued;
                                        let duration_ms = audio_duration_ms;

                                        // --- Step 9: De-assert PTT (with tail delay) ---
                                        // Plain interruptible_sleep (not _or_supersede): the
                                        // frame has already fully played by this point, so
                                        // there's nothing in flight left to supersede — a
                                        // request arriving during this 50ms tail is handled by
                                        // the worker's next natural dequeue.
                                        if interruptible_sleep(
                                            Duration::from_millis(50),
                                            &shutdown,
                                            &abort_current_tx,
                                        )
                                        .await
                                        {
                                            if shutdown.load(Ordering::Acquire) {
                                                info!("TX aborted during tail by shutdown");
                                                break 'worker;
                                            }
                                            info!("TX aborted during tail by operator (F8)");
                                            // PAN-38 round 4 (Codex): the audio already fully
                                            // played out before this trailing guard period, so
                                            // `success`/`duration_ms` already reflect a genuine
                                            // completed (or failed) transmission -- report it
                                            // with those real values now rather than skipping
                                            // TransmitComplete entirely (which would leak
                                            // pending_self_cq_qsos and, if `success` is false,
                                            // never roll back the streak) or fabricating a
                                            // failure for a CQ that may have transmitted fine.
                                            if let Err(e) = report_played_transmit_complete(
                                                &message_bus,
                                                success,
                                                message_text.clone(),
                                                duration_ms,
                                                qso_id.clone(),
                                                aired_payload(
                                                    frequency_offset,
                                                    origin,
                                                    remote_client_key_id.clone(),
                                                ),
                                                attempt_id,
                                            )
                                            .await
                                            {
                                                warn!("Failed to send TransmitComplete: {}", e);
                                            }
                                            continue 'worker;
                                        }
                                        ptt_guard.release("Step 9 unkey").await;

                                        // --- Step 10: Send TransmitComplete ---
                                        if let Err(e) = report_played_transmit_complete(
                                            &message_bus,
                                            success,
                                            message_text,
                                            duration_ms,
                                            qso_id.clone(),
                                            aired_payload(
                                                frequency_offset,
                                                origin,
                                                remote_client_key_id.clone(),
                                            ),
                                            attempt_id,
                                        )
                                        .await
                                        {
                                            warn!("Failed to send TransmitComplete: {}", e);
                                        }

                                        // Normal completion: leave the retry loop and let the
                                        // worker dequeue the next message.
                                        break 'key_and_send;
                                    } // end 'key_and_send
                                }

                                MessageType::MultiTransmitRequest {
                                    mut items,
                                    tx_parity,
                                    origin,
                                    remote_client_key_id,
                                } => {
                                    info!("Multi-TX request: {} messages", items.len());
                                    TX_ATTEMPTS_COUNT
                                        .fetch_add(items.len() as u64, Ordering::Relaxed);

                                    // --- Step 0-dup (bundle): pivot-tombstone duplicate
                                    // gate ---
                                    // Double-PTT fix (docs/qso-tx-deep-review-2026-07-18.md).
                                    // Mirrors the single-TX arm's Step 0-dup gate above: an
                                    // item in THIS bundle may be a stale duplicate of a
                                    // frame that was ALREADY physically transmitted via a
                                    // previous cycle's bundle-arm pivot (Step 4b-pivot
                                    // below) — the newer request that produced that
                                    // pivoted text is the one bundled here, still behind
                                    // the frame that already carried it out over the air.
                                    // Drop any such item now, before Step 0's TX-policy
                                    // mute or Step 1's encode; if every item in the bundle
                                    // turns out to be such a duplicate, skip the whole
                                    // message exactly like the other "all dropped" paths
                                    // in this arm.
                                    {
                                        let mut kept = Vec::with_capacity(items.len());
                                        for item in items.into_iter() {
                                            if super::is_pivot_duplicate(
                                                item.qso_id.as_deref(),
                                                &item.message_text,
                                                item.frequency_offset,
                                                &pivoted_once,
                                            ) {
                                                info!(
                                                    target: "pancetta::tx.policy",
                                                    "dropping stale multi-TX item for {}: '{}' — already sent via pivot",
                                                    item.qso_id.as_deref().unwrap_or("?"),
                                                    item.message_text
                                                );
                                                emit_diagnostic(
                                                    &message_bus,
                                                    "tx.policy",
                                                    pancetta_core::DiagnosticLevel::Info,
                                                    format!(
                                                        "dropping stale multi-TX item for '{}' — already sent via pivot",
                                                        item.message_text
                                                    ),
                                                    item.qso_id.as_deref(),
                                                )
                                                .await;
                                                let replaced =
                                                    item.qso_id.as_deref().and_then(|id| {
                                                        let key = super::active_tx_qso_key(id);
                                                        pivoted_once.remove(&key);
                                                        pivoted_from.remove(&key)
                                                    });
                                                report_pivot_duplicate_dropped(
                                                    &message_bus,
                                                    item.qso_id.clone(),
                                                    replaced,
                                                )
                                                .await;
                                            } else {
                                                kept.push(item);
                                            }
                                        }
                                        items = kept;
                                        if items.is_empty() {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "multi-TX bundle empty after dropping pivot-duplicate items — skipping"
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                "multi-TX bundle empty after dropping pivot-duplicate items — skipping"
                                                    .to_string(),
                                                None,
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            continue;
                                        }
                                    }

                                    // --- Step 0: TX-policy hard mute ---
                                    // Disabled (RX-only): never key PTT / play audio /
                                    // modulate. Consume the bundle, clear the TUI TX
                                    // view, and report each item failed so any awaiting
                                    // state doesn't hang.
                                    if let Some(reason) = tx_hard_mute_reason(
                                        &tx_policy,
                                        &tx_restart_inhibit,
                                        &hamlib_command_loop_ready,
                                        &hamlib_pending_frequency,
                                        &hamlib_pending_split,
                                        &hamlib_command_in_flight,
                                        &rig_conn_state,
                                        &audio_output_alive,
                                        &message_bus.receive_gain,
                                        &message_bus.data_mode,
                                    ) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({}): multi-TX bundle of {} items",
                                            reason, items.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): multi-TX bundle of {} items",
                                                items.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                item.message_text.clone(),
                                                0,
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // --- Step 0-band: PAN-118 band-plan gate
                                    // (bundle, per item) ---
                                    // Bundled items sit at independent audio
                                    // offsets, so band-plan compliance is
                                    // evaluated PER ITEM: an out-of-privilege
                                    // item is dropped and reported here
                                    // (mirroring the per-item failure loop of
                                    // the whole-bundle refusal above and the
                                    // Step 0b stale-drop below) while
                                    // in-privilege siblings proceed. The
                                    // whole bundle is refused only when every
                                    // item is out of privilege. Skipped while
                                    // the dial is the unknown-0 sentinel.
                                    {
                                        let mut kept = Vec::with_capacity(items.len());
                                        let mut dropped_any = false;
                                        for item in items.into_iter() {
                                            match tx_rf_for_gate(
                                                &operating_frequency_hz,
                                                &split_tx_frequency_hz,
                                                item.frequency_offset,
                                            )
                                            .and_then(|rf| {
                                                tx_band_plan_mute_reason(
                                                    rf,
                                                    tx_signal_span_hz(active_protocol),
                                                    &band_plan,
                                                )
                                            }) {
                                                Some(reason) => {
                                                    dropped_any = true;
                                                    info!(
                                                        target: "pancetta::tx.policy",
                                                        "dropping out-of-band multi-TX item: '{}' ({reason})",
                                                        item.message_text
                                                    );
                                                    emit_diagnostic(
                                                        &message_bus,
                                                        "tx.policy",
                                                        pancetta_core::DiagnosticLevel::Info,
                                                        format!(
                                                            "dropping out-of-band multi-TX item: '{}' ({reason})",
                                                            item.message_text
                                                        ),
                                                        item.qso_id.as_deref(),
                                                    )
                                                    .await;
                                                    let _ = report_transmit_complete(
                                                        &message_bus,
                                                        false,
                                                        item.message_text.clone(),
                                                        0,
                                                        item.qso_id.clone(),
                                                        item.attempt_id,
                                                    )
                                                    .await;
                                                }
                                                None => kept.push(item),
                                            }
                                        }
                                        items = kept;
                                        if dropped_any && items.is_empty() {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "multi-TX bundle empty after dropping out-of-band items — skipping"
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                "multi-TX bundle empty after dropping out-of-band items — skipping"
                                                    .to_string(),
                                                None,
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            continue;
                                        }
                                    }

                                    // --- Step 0a: Remote-TX arm gate (bundle) ---
                                    // A Remote-origin bundle must pass the station-agent
                                    // arm gate before ANY item keys PTT. Local bundles
                                    // skip it (byte-identical). ANDs under the TxPolicy
                                    // hard-mute above. Fail CLOSED on a poisoned lock.
                                    // Inert in P0–P2 (no Remote bundle is constructed).
                                    if origin == crate::message_bus::TxOrigin::Remote
                                        && !remote_tx_permitted_for(
                                            &remote_tx_arm,
                                            pancetta_agent::arm::ArmTime::now,
                                            remote_client_key_id.as_deref(),
                                        )
                                    {
                                        warn!(
                                            target: "agent.tx",
                                            "dropping remote TX — not armed/permitted: multi-TX bundle of {} items",
                                            items.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "agent.tx",
                                            pancetta_core::DiagnosticLevel::Warn,
                                            format!(
                                                "Remote TX denied (not armed/permitted): multi-TX bundle of {} items",
                                                items.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        // dispensa Q-0051 Phase B/C: audit the
                                        // drop and relay a client-visible
                                        // signal, mirroring the single-TX site
                                        // above. No single qso_id to attribute
                                        // a bundle drop to.
                                        let denial_reason = format!(
                                            "not armed/permitted: multi-TX bundle of {} items",
                                            items.len()
                                        );
                                        audit_log.record(&pancetta_agent::audit::AuditEvent {
                                            ts_unix_ms: utc_clock().timestamp_millis(),
                                            kind: pancetta_agent::audit::AuditKind::TxDenied,
                                            operator_callsign: tx_denied_operator_attribution(
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            ),
                                            detail: denial_reason.clone(),
                                        });
                                        super::remote_gateway::relay_to_gateway(
                                            &message_bus,
                                            &display_feed_enabled,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TxDenied {
                                                reason: denial_reason,
                                                qso_id: None,
                                            },
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                item.message_text.clone(),
                                                0,
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // --- Step 0b: Drop-stale-TX gate ---
                                    // Drop bundle items whose QSO has ended (the
                                    // waveform is summed up front, so we must filter
                                    // before encoding). Items with no qso_id are
                                    // never gated. Each dropped item gets a failed
                                    // TransmitComplete; if the whole bundle drops,
                                    // skip it.
                                    {
                                        let mut kept = Vec::with_capacity(items.len());
                                        for item in items.into_iter() {
                                            if tx_qso_is_live(
                                                item.qso_id.as_deref(),
                                                &active_tx_qsos,
                                            ) {
                                                kept.push(item);
                                            } else {
                                                info!(
                                                    target: "pancetta::tx.policy",
                                                    "dropping stale multi-TX item for ended QSO {}: '{}'",
                                                    item.qso_id.as_deref().unwrap_or("?"),
                                                    item.message_text
                                                );
                                                emit_diagnostic(
                                                    &message_bus,
                                                    "tx.policy",
                                                    pancetta_core::DiagnosticLevel::Info,
                                                    format!(
                                                        "dropping stale multi-TX item for ended QSO: '{}'",
                                                        item.message_text
                                                    ),
                                                    item.qso_id.as_deref(),
                                                )
                                                .await;
                                                let _ = report_transmit_complete(
                                                    &message_bus,
                                                    false,
                                                    item.message_text.clone(),
                                                    0,
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                )
                                                .await;
                                            }
                                        }
                                        items = kept;
                                        if items.is_empty() {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "multi-TX bundle empty after dropping stale items — skipping"
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                "multi-TX bundle empty after dropping stale items — skipping"
                                                    .to_string(),
                                                None,
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            continue;
                                        }
                                    }

                                    // Report the bundle items as QUEUED.
                                    send_tx_queue_status(
                                        &message_bus,
                                        None,
                                        multi_tx_status_items(&items, false),
                                    )
                                    .await;

                                    // --- Step 1: Encode + modulate up front ---
                                    // Per-item params for the active protocol.
                                    // `Ft8` → `ProtocolParams::ft8()` (byte-identical
                                    // to the previous hardcoded value);
                                    // `Ft4`/`Ft2` → the mode's params (correct on-air
                                    // shaping, e.g. FT4 GFSK BT=1.0), and encoding
                                    // via `encode_message_protocol` (105 FT4 symbols).
                                    //
                                    // 2026-07-17: delegates to `encode_and_modulate_multi_tx`
                                    // (shared with Step 4b's key-time re-encode) instead of an
                                    // inline loop — also fixes a pre-existing index-misalignment
                                    // bug where a partial encode failure caused later items to
                                    // inherit an unrelated, earlier item's frequency offset.
                                    let tx_params = pancetta_ft8::ProtocolParams::from_protocol(
                                        active_protocol,
                                    );
                                    let outcome = encode_and_modulate_multi_tx(
                                        &mut encoder,
                                        active_protocol,
                                        &tx_params,
                                        &items,
                                    );

                                    for item in &outcome.encode_failed {
                                        warn!(
                                            "Encoding failed for '{}' (multi-TX)",
                                            item.message_text
                                        );
                                        // This item would otherwise get NO TransmitComplete
                                        // at all, ever — worse than the delayed-watchdog
                                        // case, since even that path eventually sends one.
                                        emit_tx_failure_diagnostic(
                                            &message_bus,
                                            item.qso_id.as_deref(),
                                            &item.message_text,
                                            "encoding error",
                                        )
                                        .await;
                                        let _ = report_transmit_complete(
                                            &message_bus,
                                            false,
                                            item.message_text.clone(),
                                            0,
                                            item.qso_id.clone(),
                                            item.attempt_id,
                                        )
                                        .await;
                                    }

                                    // 2026-07-17 (post-review): rebind `items` itself to
                                    // ONLY the successfully-encoded subset, right here,
                                    // before ANY downstream code (Step 4b's liveness zip,
                                    // Step 5's "now sending" display, the failure-cleanup
                                    // loops) reads `items` again. An earlier version of
                                    // this fix left `items` as the full, unfiltered input
                                    // list while `item_texts`/`encoded_qso_ids` only
                                    // covered the successfully-encoded subset — so whenever
                                    // a Step-1 encode failure occurred anywhere in the
                                    // bundle, `items.iter().zip(encoded_qso_ids.iter())` (or
                                    // anything zipped against it) silently paired each item
                                    // with an UNRELATED item's liveness verdict for every
                                    // index after the failure. In the worst case this could
                                    // transmit a call the operator had just cancelled while
                                    // silently dropping the genuinely still-live one — the
                                    // exact bug class this whole feature exists to prevent,
                                    // reintroduced one layer up. `encoded_items` is built in
                                    // the SAME loop iteration as `item_texts`/
                                    // `encoded_qso_ids` inside `encode_and_modulate_multi_tx`,
                                    // so positional correspondence is guaranteed by
                                    // construction from this point on.
                                    let items = outcome.encoded_items;
                                    let item_texts = outcome.item_texts;
                                    let encoded_qso_ids = outcome.encoded_qso_ids;

                                    let samples = match outcome.samples {
                                        Ok(s) => s,
                                        Err(reason) => {
                                            // Only diagnose "modulation of the survivors
                                            // failed" (item_texts non-empty) — the
                                            // "nothing encoded" case has nothing left to
                                            // diagnose beyond the per-item encode_failed
                                            // reports already sent above.
                                            if !item_texts.is_empty() {
                                                for (text, qso_id) in
                                                    item_texts.iter().zip(encoded_qso_ids.iter())
                                                {
                                                    emit_tx_failure_diagnostic(
                                                        &message_bus,
                                                        qso_id.as_deref(),
                                                        text,
                                                        &format!(
                                                            "multi-TX modulation error ({reason})"
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                            for ((text, qso_id), item) in item_texts
                                                .into_iter()
                                                .zip(encoded_qso_ids)
                                                .zip(&items)
                                            {
                                                let _ = report_transmit_complete(
                                                    &message_bus,
                                                    false,
                                                    text,
                                                    0,
                                                    qso_id,
                                                    item.attempt_id,
                                                )
                                                .await;
                                            }
                                            continue;
                                        }
                                    };

                                    // --- Step 2: Resolve required parity ---
                                    let (tx_late_max_ms, tx_self_parity, ptt_lead_ms) = {
                                        let config = live_config.read().await;
                                        (
                                            live_late_cap.load(Ordering::Acquire),
                                            config.station.tx_self_parity,
                                            config.station.ptt_lead_ms,
                                        )
                                    };
                                    let timing = SlotTimingContext {
                                        utc_clock: utc_clock.clone(),
                                        active_protocol,
                                        tx_late_max_ms: live_late_cap.clone(),
                                        tx_self_parity,
                                        ptt_lead_ms,
                                        sample_rate,
                                        slot_ns: active_slot_ns.load(Ordering::Relaxed),
                                        request_received_at,
                                    };
                                    let slot_ns = timing.slot_ns;
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        timing.request_received_at,
                                        slot_ns,
                                    );

                                    // PAN-117 (PR #450 review): a bundle frozen
                                    // at `request_received_at` must not be
                                    // accepted into a slot the key-time gate is
                                    // guaranteed to drop. A bundle coalesced
                                    // from a single request already waited the
                                    // base window, so it reserves that plus the
                                    // margin like the single-TX arm's Step 2; a
                                    // directly dequeued bundle never waited, so
                                    // it reserves only the margin (round 2).
                                    let mut schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        multi_tx_acceptance_cap_ms(
                                            active_protocol,
                                            tx_late_max_ms,
                                            head_takes_base_wait,
                                        ),
                                        sample_rate,
                                        slot_ns,
                                    );

                                    info!(
                                        "Multi-TX scheduled: parity={:?} target_slot={} pad={} samples cursor={} samples ({} items)",
                                        required_parity,
                                        schedule.target_slot.format("%H:%M:%S%.3f UTC"),
                                        schedule.silent_pad_samples,
                                        schedule.cursor_offset_samples,
                                        item_texts.len(),
                                    );

                                    // --- Step 2b: defer-time liveness recheck (TX-F8) ---
                                    // Mirrors the single-TX arm's defer-time recheck (the
                                    // `if schedule.deferred` block right after its own
                                    // Step 2, before building audio): a bundle either
                                    // defers as a whole or doesn't (all items share one
                                    // parity/slot), so if we missed the current slot and
                                    // deferred to a later one (~30s), we (a) count it in
                                    // `TX_DEFERS_COUNT` the same way the single-TX arm
                                    // does, (b) re-check liveness for every item in the
                                    // bundle NOW — reusing the exact live_mask
                                    // partial-liveness mechanism Step 4b's key-time gate
                                    // uses below — instead of silently waiting out the
                                    // full ~30s defer for a bundle that's already
                                    // (partially or wholly) dead, and (c) refresh the
                                    // TUI-visible per-item TX-strip status with
                                    // `deferred: true` (previously always `false`, so a
                                    // deferred bundle was indistinguishable from a dead
                                    // one for up to 30s).
                                    let (items, samples, item_texts, encoded_qso_ids) = if schedule
                                        .deferred
                                    {
                                        TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);

                                        let live_mask: Vec<bool> = encoded_qso_ids
                                            .iter()
                                            .map(|id| {
                                                tx_qso_is_live(id.as_deref(), &active_tx_qsos)
                                            })
                                            .collect();

                                        if !live_mask.iter().any(|&live| live) {
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "dropping stale multi-TX bundle at defer time: all {} item(s) already ended",
                                                items.len()
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "dropping stale multi-TX bundle at defer time: all {} item(s) already ended",
                                                    items.len()
                                                ),
                                                None,
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for item in &items {
                                                let _ = report_transmit_complete(
                                                    &message_bus,
                                                    false,
                                                    item.message_text.clone(),
                                                    0,
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                )
                                                .await;
                                            }
                                            continue;
                                        }

                                        let (items, samples, item_texts, encoded_qso_ids) =
                                            if live_mask.iter().all(|&live| live) {
                                                (items, samples, item_texts, encoded_qso_ids)
                                            } else {
                                                for (item, &live) in
                                                    items.iter().zip(live_mask.iter())
                                                {
                                                    if !live {
                                                        info!(
                                                            target: "pancetta::tx.policy",
                                                            "dropping stale multi-TX item at defer time for ended QSO {}: '{}'",
                                                            item.qso_id.as_deref().unwrap_or("?"),
                                                            item.message_text
                                                        );
                                                        emit_diagnostic(
                                                            &message_bus,
                                                            "tx.policy",
                                                            pancetta_core::DiagnosticLevel::Info,
                                                            format!(
                                                                "dropping stale multi-TX item at defer time for ended QSO: '{}'",
                                                                item.message_text
                                                            ),
                                                            item.qso_id.as_deref(),
                                                        )
                                                        .await;
                                                        let _ = report_transmit_complete(
                                                            &message_bus,
                                                            false,
                                                            item.message_text.clone(),
                                                            0,
                                                            item.qso_id.clone(),
                                                            item.attempt_id,
                                                        )
                                                        .await;
                                                    }
                                                }

                                                let live_items: Vec<
                                                    crate::message_bus::TransmitRequestItem,
                                                > = items
                                                    .iter()
                                                    .zip(live_mask.iter())
                                                    .filter(|(_, &live)| live)
                                                    .map(|(item, _)| item.clone())
                                                    .collect();

                                                let rebuild = encode_and_modulate_multi_tx(
                                                    &mut encoder,
                                                    active_protocol,
                                                    &tx_params,
                                                    &live_items,
                                                );

                                                for item in &rebuild.encode_failed {
                                                    emit_tx_failure_diagnostic(
                                                        &message_bus,
                                                        item.qso_id.as_deref(),
                                                        &item.message_text,
                                                        "defer-time re-encode error",
                                                    )
                                                    .await;
                                                    let _ = report_transmit_complete(
                                                        &message_bus,
                                                        false,
                                                        item.message_text.clone(),
                                                        0,
                                                        item.qso_id.clone(),
                                                        item.attempt_id,
                                                    )
                                                    .await;
                                                }

                                                // Rebind to `rebuild.encoded_items`, not the
                                                // `live_items` passed IN to the rebuild —
                                                // `encode_and_modulate_multi_tx`'s own doc
                                                // comment on `encoded_items` requires this:
                                                // a `live_items` entry whose re-encode failed
                                                // (reported via `encode_failed` above) has no
                                                // audio in `new_samples`, so returning it as
                                                // `items` would log a Band Activity frame for
                                                // a message that was never actually sent.
                                                let rebuilt_items = rebuild.encoded_items;
                                                let rebuilt_texts = rebuild.item_texts;
                                                let rebuilt_qso_ids = rebuild.encoded_qso_ids;

                                                let new_samples = match rebuild.samples {
                                                    Ok(s) => s,
                                                    Err(reason) => {
                                                        if !rebuilt_texts.is_empty() {
                                                            warn!(
                                                                "Defer-time re-modulation failed: {}",
                                                                reason
                                                            );
                                                            for (text, qso_id) in rebuilt_texts
                                                                .iter()
                                                                .zip(rebuilt_qso_ids.iter())
                                                            {
                                                                emit_tx_failure_diagnostic(
                                                                    &message_bus,
                                                                    qso_id.as_deref(),
                                                                    text,
                                                                    &format!(
                                                                        "defer-time re-modulation error ({reason})"
                                                                    ),
                                                                )
                                                                .await;
                                                            }
                                                        }
                                                        send_tx_queue_status(
                                                            &message_bus,
                                                            None,
                                                            Vec::new(),
                                                        )
                                                        .await;
                                                        for ((text, qso_id), item) in rebuilt_texts
                                                            .into_iter()
                                                            .zip(rebuilt_qso_ids)
                                                            .zip(&rebuilt_items)
                                                        {
                                                            let _ = report_transmit_complete(
                                                                &message_bus,
                                                                false,
                                                                text,
                                                                0,
                                                                qso_id,
                                                                item.attempt_id,
                                                            )
                                                            .await;
                                                        }
                                                        continue;
                                                    }
                                                };

                                                (
                                                    rebuilt_items,
                                                    new_samples,
                                                    rebuilt_texts,
                                                    rebuilt_qso_ids,
                                                )
                                            };

                                        // Refresh the TUI TX strip: this bundle is
                                        // deferred (waiting for its slot), not dead.
                                        send_tx_queue_status(
                                            &message_bus,
                                            None,
                                            multi_tx_status_items(&items, true),
                                        )
                                        .await;

                                        (items, samples, item_texts, encoded_qso_ids)
                                    } else {
                                        (items, samples, item_texts, encoded_qso_ids)
                                    };

                                    // PAN-223: report each item that survived the
                                    // defer-time liveness filter at the bundle's slot.
                                    for item in &items {
                                        report_frame_scheduled(
                                            &message_bus,
                                            item.qso_id.as_deref(),
                                            item.attempt_id,
                                            &schedule,
                                        )
                                        .await;
                                    }

                                    // --- Step 3 (deferred): the audio buffer is now
                                    // built once, fresh, immediately before Step 5 —
                                    // see the new block right before "--- Step 5:
                                    // Assert PTT ---" below. `samples` (Step 1's raw,
                                    // untrimmed multi-tone waveform) is carried forward
                                    // as `raw_samples` for the fast path in the
                                    // Step 4b/4b-pivot resolution below; the "cursor
                                    // exceeded waveform" defensive check now happens
                                    // once, at the final trim point, against whichever
                                    // buffer (fast-path or rebuilt) is actually about
                                    // to be sent.
                                    let raw_samples = samples;

                                    // --- Step 4: Wait until PTT engage instant ---
                                    // PAN-114: wall-clock wait, same as the single-TX
                                    // arm — see `wait_for_ptt_instant`.
                                    let ptt_target_utc = timing.ptt_target(schedule.target_slot);
                                    let ptt_wait = wait_for_ptt_instant(
                                        ptt_target_utc,
                                        schedule.target_slot,
                                        timing.late_cap_ms(),
                                        slot_ns,
                                        &shutdown,
                                        &abort_current_tx,
                                        || utc_clock(),
                                    )
                                    .await;
                                    match on_ptt_wait(
                                        ptt_wait,
                                        TxShape::Bundle,
                                        || {
                                            item_texts
                                                .clone()
                                                .into_iter()
                                                .zip(encoded_qso_ids.clone())
                                                .zip(&items)
                                                .map(|((text, qso_id), item)| {
                                                    (text, qso_id, item.attempt_id)
                                                })
                                                .collect()
                                        },
                                        &message_bus,
                                        &shutdown,
                                    )
                                    .await
                                    {
                                        std::ops::ControlFlow::Continue(()) => {}
                                        std::ops::ControlFlow::Break(TxExit::StopWorker) => break,
                                        std::ops::ControlFlow::Break(TxExit::NextRequest) => {
                                            continue
                                        }
                                    }

                                    // --- Step 4b: Drop-stale-TX gate (key-time) ---
                                    // The slot wait can span the moment a QSO ends.
                                    // The audio built at Step 3 can't be partially
                                    // re-filtered in place, so: if EVERYTHING went
                                    // stale, drop the bundle outright. If NOTHING
                                    // went stale, transmit the already-built audio
                                    // unchanged (fast path). If SOME items went
                                    // stale, re-encode + re-modulate just the
                                    // still-live subset via
                                    // `encode_and_modulate_multi_tx` and re-apply the
                                    // ALREADY-COMPUTED `schedule` — timing (target
                                    // slot / padding / cursor) doesn't depend on
                                    // which items are in the bundle, only Step 1's
                                    // encode/modulate needs redoing — instead of
                                    // either transmitting a cancelled QSO's call
                                    // baked into the same audio as a still-live one
                                    // (the original bug), or unconditionally losing
                                    // the still-live item's cycle for no reason
                                    // (2026-07-17's first, more conservative fix).
                                    // Checked against `encoded_qso_ids` (what
                                    // actually made it into the summed waveform),
                                    // not the pre-encode `items` list.
                                    // PAN-141: alongside plain liveness, hold any item
                                    // whose DX was freshly observed transmitting on the
                                    // exact parity this bundle is about to key into (see
                                    // `dx_parity_conflict_for_qso`). All items in a bundle
                                    // share one parity (bundling invariant), so
                                    // `required_parity` applies uniformly. `parity_hold_mask`
                                    // is tracked only so the per-item log below can name
                                    // the right reason; `live_mask` itself keeps its
                                    // existing "should this item transmit" meaning so
                                    // every downstream consumer needs no further changes.
                                    //
                                    // Codex P1 on PR #370 (round 1), refined by the local
                                    // review gate: `tx_qso_is_live` must be checked AFTER
                                    // every parity lookup's `QsoManager` await has already
                                    // completed for the WHOLE bundle, with no further await
                                    // between the last liveness check and PTT — not merely
                                    // after each item's OWN lookup. Checking item N's
                                    // liveness right after item N's lookup (the first
                                    // attempt at this fix) still left a window: item N-1's
                                    // cached `is_live = true` could go stale while item N's
                                    // lookup was still pending, and nothing re-validates
                                    // it afterward. Two passes closes that: pass 1 runs
                                    // every parity lookup (the only awaits in this
                                    // computation); pass 2 is a plain, non-async loop that
                                    // checks every item's liveness back-to-back, immediately
                                    // before this mask is used — mirrors this file's own
                                    // established "recheck immediately before the
                                    // irreversible step" pattern, generalized to a whole
                                    // bundle instead of one QSO. PAN-148: extracted into
                                    // `bundle_live_mask` (synchronous) plus `resolve_callsigns`
                                    // (the async resolution pass) so the new Step 4d final
                                    // gate below runs the textually identical check.
                                    let step_4b_callsigns =
                                        resolve_callsigns(&encoded_qso_ids, &qso_manager_watch)
                                            .await;
                                    let (live_mask, parity_hold_mask) = bundle_live_mask(
                                        &encoded_qso_ids,
                                        &step_4b_callsigns,
                                        required_parity,
                                        DxParityContext::live(&operating_frequency_hz, slot_ns),
                                        &active_tx_qsos,
                                        &cross_time_state,
                                    );

                                    // PAN-118: re-evaluate the band plan per item
                                    // against the CURRENT dial/split (it may have
                                    // moved via CAT or a split change since Step
                                    // 0's partition). An item now out of privilege
                                    // is folded into `live_mask` as not-live, so
                                    // the existing per-item drop + re-encode
                                    // machinery below excludes exactly those items
                                    // and still keys the in-privilege siblings —
                                    // the same partial-liveness semantics a stale
                                    // item gets at this site. `band_drop_mask`
                                    // only steers the per-item reason text below.
                                    let mut live_mask = live_mask;
                                    let mut band_drop_mask = vec![false; live_mask.len()];
                                    for (item, (live, band_dropped)) in items
                                        .iter()
                                        .zip(live_mask.iter_mut().zip(band_drop_mask.iter_mut()))
                                    {
                                        if !*live {
                                            continue;
                                        }
                                        if tx_rf_for_gate(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                            item.frequency_offset,
                                        )
                                        .and_then(|rf| {
                                            tx_band_plan_mute_reason(
                                                rf,
                                                tx_signal_span_hz(active_protocol),
                                                &band_plan,
                                            )
                                        })
                                        .is_some()
                                        {
                                            *live = false;
                                            *band_dropped = true;
                                            info!(
                                                target: "pancetta::tx.policy",
                                                "dropping out-of-band multi-TX item at key-time: '{}' (TX frequency outside the configured amateur band plan)",
                                                item.message_text
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "dropping out-of-band multi-TX item at key-time: '{}'",
                                                    item.message_text
                                                ),
                                                item.qso_id.as_deref(),
                                            )
                                            .await;
                                        }
                                    }

                                    if !live_mask.iter().any(|&live| live) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "dropping stale multi-TX bundle: all {} item(s) ended or held (colliding DX parity) during the pre-PTT wait",
                                            encoded_qso_ids.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "dropping stale multi-TX bundle: all {} item(s) ended or held (colliding DX parity) during the pre-PTT wait",
                                                encoded_qso_ids.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                item.message_text.clone(),
                                                0,
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // --- Step 4b-pivot: bundle-arm late pivot ---
                                    // Double-PTT fix (docs/qso-tx-deep-review-2026-07-18.md),
                                    // bundle-arm analogue of the single-TX arm's Step 4c
                                    // (see `pivot_bundle_items` in `coordinator/mod.rs`).
                                    // The same pre-PTT wait that can leave a bundle item's
                                    // QSO ENDED (handled just above) can also leave a
                                    // still-LIVE item's TEXT stale: e.g. the DX sent RR73
                                    // and this QSO advanced to its closing 73 while its
                                    // frame was already bundled alongside a different,
                                    // still-in-progress QSO. Swap in the freshest
                                    // `LatestTxIntent` for every still-live item before
                                    // deciding whether a rebuild is needed — a pivot alone
                                    // (even with every item otherwise still live) must
                                    // force the rebuild path below, since the item list
                                    // now holds different text than what Step 1 encoded.
                                    // PAN-136: a pivot also swaps each item's binding, so
                                    // Step 4b-rebind below re-folds the bundle's gate
                                    // binding before any gate reads it.
                                    // PAN-145: capture each live QSO before the intent
                                    // mirror. The authoritative bump can precede its event's
                                    // delivery to that map while we build or wait for PTT.
                                    let mut relocation_snapshot: Vec<(Option<String>, u64)> = items
                                        .iter()
                                        .zip(live_mask.iter())
                                        .filter(|(_, live)| **live)
                                        .map(|(item, _)| {
                                            (
                                                item.qso_id.clone(),
                                                item.qso_id
                                                    .as_deref()
                                                    .map(|id| relocation_epochs.epoch(id))
                                                    .unwrap_or(0),
                                            )
                                        })
                                        .collect();
                                    let latest_snapshot: std::collections::HashMap<
                                        String,
                                        super::LatestTxIntent,
                                    > = latest_tx_intent
                                        .read()
                                        .ok()
                                        .map(|guard| guard.clone())
                                        .unwrap_or_default();
                                    let live_items_pre: Vec<
                                        crate::message_bus::TransmitRequestItem,
                                    > = items
                                        .iter()
                                        .zip(live_mask.iter())
                                        .filter(|(_, &live)| live)
                                        .map(|(item, _)| item.clone())
                                        .collect();
                                    // PAN-136: each item carries its own binding, recorded
                                    // on the replaced frame exactly as single-TX Step 4c
                                    // does, so the PAN-75 rollback can tell it apart from
                                    // a fresher same-text frame re-bound by the pivot.
                                    let pre_pivot_frames: std::collections::HashMap<
                                        String,
                                        ReplacedFrame,
                                    > = live_items_pre
                                        .iter()
                                        .filter_map(|item| {
                                            item.qso_id.as_deref().map(|id| {
                                                (
                                                    super::active_tx_qso_key(id),
                                                    (
                                                        item.message_text.clone(),
                                                        pancetta_qso::UnsentAttemptPayload {
                                                            frequency_hz: item.frequency_offset,
                                                            remote_binding: Some((
                                                                item.origin
                                                                    == crate::message_bus::TxOrigin::Remote,
                                                                item.remote_client_key_id.clone(),
                                                            )),
                                                        },
                                                        item.attempt_id,
                                                    ),
                                                )
                                            })
                                        })
                                        .collect();
                                    let (pivoted_live_items, mut pivots) =
                                        super::pivot_bundle_items(live_items_pre, &latest_snapshot);
                                    for (qso_key, new_text, new_freq) in &pivots {
                                        info!(
                                            target: "pancetta::tx.pivot",
                                            "TX pivot (bundle): qso {} -> '{}' @{:.0}Hz (fresher message arrived during pre-PTT wait)",
                                            qso_key,
                                            new_text,
                                            new_freq
                                        );
                                    }
                                    // PAN-75: a refusal before the tombstones below are installed leaves each
                                    // pivoted item's own request queued, so it reports the frame the pivot
                                    // replaced instead (Codex P2 on PR #418).
                                    let mut early_replaced: PivotReplaced = pivots
                                        .iter()
                                        .filter_map(|(key, _, _)| {
                                            pre_pivot_frames
                                                .get(key)
                                                .map(|frame| (key.clone(), frame.clone()))
                                        })
                                        .collect();
                                    let mut piv_iter = pivoted_live_items.into_iter();
                                    let items: Vec<crate::message_bus::TransmitRequestItem> = items
                                        .into_iter()
                                        .zip(live_mask.iter())
                                        .map(|(item, &live)| {
                                            if live {
                                                piv_iter.next().expect(
                                                    "pivoted_live_items has exactly one \
                                                         entry per live item, in order",
                                                )
                                            } else {
                                                item
                                            }
                                        })
                                        .collect();

                                    // --- Step 4b-rebind (PAN-136): re-fold the bundle's gate binding ---
                                    // A pivot now swaps each item's binding with its payload. The bundle's
                                    // `origin`/`remote_client_key_id` were folded at coalesce time, so after a
                                    // pivot they can be stale in BOTH directions: a Local bundle carrying a
                                    // newly Remote-bound frame (no arm gate at all), or client A's bundle
                                    // carrying a frame now bound to client B (B authorized under A's gate).
                                    // Hold any item whose new binding conflicts with the bundle's anchor (the
                                    // existing per-item drop + re-encode below; items are still separable
                                    // here), drop the held items' pivots so their queued requests are not
                                    // tombstoned, and re-fold the binding every later gate, poll, supersede and
                                    // denial attribution in this arm reads. Nothing pivoted → byte-identical.
                                    let pivoted_any = !pivots.is_empty();
                                    let identity_hold_mask = if pivoted_any {
                                        super::bundle_identity_conflict_mask(
                                            &items,
                                            &live_mask,
                                            (origin, remote_client_key_id.as_deref()),
                                        )
                                    } else {
                                        vec![false; items.len()]
                                    };
                                    let mut held_keys: Vec<String> = Vec::new();
                                    for ((item, live), &held) in items
                                        .iter()
                                        .zip(live_mask.iter_mut())
                                        .zip(identity_hold_mask.iter())
                                    {
                                        if held {
                                            *live = false;
                                            if let Some(id) = item.qso_id.as_deref() {
                                                held_keys.push(super::active_tx_qso_key(id));
                                            }
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.policy",
                                                pancetta_core::DiagnosticLevel::Info,
                                                format!(
                                                    "holding multi-TX item: its remote binding changed during the pre-PTT wait and conflicts with the rest of the bundle: '{}'",
                                                    item.message_text
                                                ),
                                                item.qso_id.as_deref(),
                                            )
                                            .await;
                                        }
                                    }
                                    pivots.retain(|(key, _, _)| !held_keys.contains(key));
                                    let (origin, remote_client_key_id) = if pivoted_any {
                                        let refolded = super::fold_bundle_identity(
                                            items
                                                .iter()
                                                .zip(live_mask.iter())
                                                .filter(|(_, &live)| live)
                                                .map(|(item, _)| item),
                                        );
                                        if (refolded.0, refolded.1.as_deref())
                                            != (origin, remote_client_key_id.as_deref())
                                        {
                                            info!(
                                                target: "agent.tx",
                                                "multi-TX bundle re-bound at key-time after a pivot: {:?}/{:?} -> {:?}/{:?}",
                                                origin, remote_client_key_id, refolded.0, refolded.1
                                            );
                                        }
                                        refolded
                                    } else {
                                        (origin, remote_client_key_id)
                                    };

                                    // `encoded_qso_ids` is rebound alongside the rest as
                                    // `encoded_qso_ids_final`; it IS consulted again — the
                                    // final trim's defensive "cursor exceeded waveform"
                                    // check (right before Step 5) emits per-item failure
                                    // diagnostics keyed by these ids.
                                    let (items, raw_samples, item_texts, encoded_qso_ids_final) =
                                        if live_mask.iter().all(|&live| live) && pivots.is_empty() {
                                            // Fast path: nothing went stale, nothing
                                            // pivoted — carry the ORIGINAL Step-1
                                            // waveform forward untrimmed; the final
                                            // trim happens once, fresh, right before
                                            // Step 5 below.
                                            (items, raw_samples, item_texts, encoded_qso_ids)
                                        } else {
                                            // Partial staleness: report the dropped item(s), then
                                            // re-encode just the still-live subset.
                                            for (
                                                ((item, &live), &held_for_binding),
                                                (&parity_held, &band_dropped),
                                            ) in items
                                                .iter()
                                                .zip(live_mask.iter())
                                                .zip(identity_hold_mask.iter())
                                                .zip(
                                                    parity_hold_mask
                                                        .iter()
                                                        .zip(band_drop_mask.iter()),
                                                )
                                            {
                                                if !live {
                                                    // PAN-141: distinguish a genuine
                                                    // stale/ended QSO from a live one held
                                                    // this cycle for a fresh DX-parity
                                                    // conflict — same drop mechanics
                                                    // either way, different reason.
                                                    // PAN-118: a band-plan drop is a
                                                    // third reason with the same
                                                    // mechanics; its diagnostic was
                                                    // already emitted when the mask
                                                    // was folded, so this branch only
                                                    // logs.
                                                    if held_for_binding {
                                                        // PAN-136: diagnostic already emitted when the hold mask was folded.
                                                        info!(
                                                            target: "pancetta::tx.policy",
                                                            "holding multi-TX item at key-time for QSO {}: its remote binding changed during the pre-PTT wait and conflicts with the bundle's: '{}'",
                                                            item.qso_id.as_deref().unwrap_or("?"),
                                                            item.message_text
                                                        );
                                                    } else if band_dropped {
                                                        info!(
                                                            target: "pancetta::tx.policy",
                                                            "dropping out-of-band multi-TX item at key-time for QSO {}: '{}'",
                                                            item.qso_id.as_deref().unwrap_or("?"),
                                                            item.message_text
                                                        );
                                                    } else if parity_held {
                                                        info!(
                                                            target: "pancetta::tx.policy",
                                                            "holding multi-TX item at key-time for QSO {}: DX observed transmitting on the colliding parity {:?} within the freshness window: '{}'",
                                                            item.qso_id.as_deref().unwrap_or("?"),
                                                            required_parity,
                                                            item.message_text
                                                        );
                                                        emit_diagnostic(
                                                            &message_bus,
                                                            "tx.policy",
                                                            pancetta_core::DiagnosticLevel::Info,
                                                            format!(
                                                                "holding multi-TX item: DX observed transmitting on the colliding parity {required_parity:?} within the freshness window: '{}'",
                                                                item.message_text
                                                            ),
                                                            item.qso_id.as_deref(),
                                                        )
                                                        .await;
                                                    } else {
                                                        info!(
                                                            target: "pancetta::tx.policy",
                                                            "dropping stale multi-TX item at key-time for ended QSO {}: '{}'",
                                                            item.qso_id.as_deref().unwrap_or("?"),
                                                            item.message_text
                                                        );
                                                        emit_diagnostic(
                                                            &message_bus,
                                                            "tx.policy",
                                                            pancetta_core::DiagnosticLevel::Info,
                                                            format!(
                                                                "dropping stale multi-TX item at key-time for ended QSO: '{}'",
                                                                item.message_text
                                                            ),
                                                            item.qso_id.as_deref(),
                                                        )
                                                        .await;
                                                    }
                                                    let _ = report_unaired_frame(
                                                        &message_bus,
                                                        item.message_text.clone(),
                                                        item.qso_id.clone(),
                                                        item.attempt_id,
                                                        take_bundle_replaced(
                                                            &mut early_replaced,
                                                            item.qso_id.as_deref(),
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }

                                            let live_items: Vec<
                                                crate::message_bus::TransmitRequestItem,
                                            > = items
                                                .iter()
                                                .zip(live_mask.iter())
                                                .filter(|(_, &live)| live)
                                                .map(|(item, _)| item.clone())
                                                .collect();

                                            let rebuild = encode_and_modulate_multi_tx(
                                                &mut encoder,
                                                active_protocol,
                                                &tx_params,
                                                &live_items,
                                            );

                                            for item in &rebuild.encode_failed {
                                                emit_tx_failure_diagnostic(
                                                    &message_bus,
                                                    item.qso_id.as_deref(),
                                                    &item.message_text,
                                                    "key-time re-encode error",
                                                )
                                                .await;
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    item.message_text.clone(),
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                    take_bundle_replaced(
                                                        &mut early_replaced,
                                                        item.qso_id.as_deref(),
                                                    ),
                                                )
                                                .await;
                                            }

                                            // Rebind to `rebuild.encoded_items`, not the
                                            // `live_items` passed IN to the rebuild — see
                                            // the identical comment at the defer-time rebuild
                                            // above; same reasoning applies here.
                                            let rebuilt_items = rebuild.encoded_items;
                                            let rebuilt_texts = rebuild.item_texts;
                                            let rebuilt_qso_ids = rebuild.encoded_qso_ids;

                                            let new_samples = match rebuild.samples {
                                                Ok(s) => s,
                                                Err(reason) => {
                                                    if !rebuilt_texts.is_empty() {
                                                        warn!(
                                                            "Key-time re-modulation failed: {}",
                                                            reason
                                                        );
                                                        for (text, qso_id) in rebuilt_texts
                                                            .iter()
                                                            .zip(rebuilt_qso_ids.iter())
                                                        {
                                                            emit_tx_failure_diagnostic(
                                                            &message_bus,
                                                            qso_id.as_deref(),
                                                            text,
                                                            &format!(
                                                                "key-time re-modulation error ({reason})"
                                                            ),
                                                        )
                                                        .await;
                                                        }
                                                    }
                                                    send_tx_queue_status(
                                                        &message_bus,
                                                        None,
                                                        Vec::new(),
                                                    )
                                                    .await;
                                                    for ((text, qso_id), item) in rebuilt_texts
                                                        .into_iter()
                                                        .zip(rebuilt_qso_ids)
                                                        .zip(&rebuilt_items)
                                                    {
                                                        let _ = report_unaired_frame(
                                                            &message_bus,
                                                            text,
                                                            qso_id.clone(),
                                                            item.attempt_id,
                                                            take_bundle_replaced(
                                                                &mut early_replaced,
                                                                qso_id.as_deref(),
                                                            ),
                                                        )
                                                        .await;
                                                    }
                                                    continue;
                                                }
                                            };

                                            info!(
                                                target: "pancetta::tx.policy",
                                                "multi-TX bundle re-encoded at key-time: {} of {} item(s) still live",
                                                rebuilt_texts.len(),
                                                items.len()
                                            );

                                            (
                                                rebuilt_items,
                                                new_samples,
                                                rebuilt_texts,
                                                rebuilt_qso_ids,
                                            )
                                        };

                                    // --- Step 4b-arm: re-check the remote-TX arm at the
                                    // last instant before keying (mirrors the single-TX
                                    // arm). The up-to-~30s slot wait can span the dead-man
                                    // heartbeat window / TTL / a consent-revoke / a local
                                    // kill; a Remote bundle admitted at pickup must not key
                                    // PTT if the arm went stale during the wait. Local
                                    // bundles are never gated.
                                    if origin == crate::message_bus::TxOrigin::Remote
                                        && !remote_tx_permitted_for(
                                            &remote_tx_arm,
                                            pancetta_agent::arm::ArmTime::now,
                                            remote_client_key_id.as_deref(),
                                        )
                                    {
                                        // Round-3 review (Codex P2): route through the
                                        // shared denial-signaling helper — mirrors the
                                        // single-TX fix above.
                                        let operator_callsign = tx_denied_operator_attribution(
                                            &remote_tx_arm,
                                            remote_client_key_id.as_deref(),
                                        );
                                        for item in &items {
                                            let denial_reason = format!(
                                                "arm went stale during slot wait: '{}' at {:.0} Hz",
                                                item.message_text, item.frequency_offset
                                            );
                                            emit_disarm_interrupt_signals(
                                                &message_bus,
                                                &audit_log,
                                                &display_feed_enabled,
                                                operator_callsign.clone(),
                                                "denied",
                                                denial_reason,
                                                item.qso_id.as_deref(),
                                            )
                                            .await;
                                        }
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut early_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    if let Some(reason) = tx_hard_mute_reason(
                                        &tx_policy,
                                        &tx_restart_inhibit,
                                        &hamlib_command_loop_ready,
                                        &hamlib_pending_frequency,
                                        &hamlib_pending_split,
                                        &hamlib_command_in_flight,
                                        &rig_conn_state,
                                        &audio_output_alive,
                                        &message_bus.receive_gain,
                                        &message_bus.data_mode,
                                    ) {
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX bundle re-key blocked ({reason}): {} items",
                                                items.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut early_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // PAN-118: the band-plan sibling of the re-key
                                    // mute above. By this point the bundle waveform
                                    // is SUMMED, so an item that is now out of
                                    // privilege cannot be individually extracted —
                                    // its audio energy shares the one waveform with
                                    // its siblings. Mirroring the adjacent hard-mute
                                    // refusal, the WHOLE bundle is held: nothing
                                    // keys, every item is reported as an unaired
                                    // frame. (Per-item drops with siblings still
                                    // keying happened at Step 0's partition and the
                                    // Step 4b key-time mask, where the items were
                                    // still separable.)
                                    if let Some(reason) = items.iter().find_map(|item| {
                                        tx_rf_for_gate(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                            item.frequency_offset,
                                        )
                                        .and_then(|rf| {
                                            tx_band_plan_mute_reason(
                                                rf,
                                                tx_signal_span_hz(active_protocol),
                                                &band_plan,
                                            )
                                        })
                                    }) {
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX bundle re-key blocked ({reason}): {} items",
                                                items.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut early_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // Double-PTT fix: record every pivot from Step
                                    // 4b-pivot now that we're actually about to key this
                                    // bundle, so the newer request that PRODUCED each
                                    // pivoted text — still queued behind this bundle in
                                    // the worker's channel — is recognized as an
                                    // already-sent duplicate (Step 0-dup (bundle) above)
                                    // instead of keying PTT a second time for the same
                                    // text.
                                    // Round-6 review (Codex P2): tracked so Step 4d-arm's
                                    // denial branch below can remove these — see that
                                    // branch, and the single-TX arm's identical fix, for
                                    // why.
                                    let pivoted_this_bundle_keys: Vec<String> =
                                        pivots.iter().map(|(k, _, _)| k.clone()).collect();
                                    for (qso_key, new_text, new_freq) in pivots {
                                        if let Some(frame) = pre_pivot_frames.get(&qso_key) {
                                            pivoted_from.insert(qso_key.clone(), frame.clone());
                                        }
                                        pivoted_once.insert(qso_key, (new_text, new_freq));
                                    }

                                    // PAN-148: resolve every survivor's DX callsign for the
                                    // Step 4d final gate's parity re-check NOW, BEFORE Step
                                    // 3's timing-sensitive trim below — see the single-TX
                                    // arm's identical fix for why (round-2 review, Codex
                                    // P1): awaiting these `QsoManager` lookups AFTER the
                                    // trim risked keying `SetPtt` against a now-stale
                                    // audio-alignment cursor if a lookup's internal lock
                                    // were ever contended.
                                    let final_gate_callsigns = resolve_callsigns(
                                        &encoded_qso_ids_final,
                                        &qso_manager_watch,
                                    )
                                    .await;

                                    // --- Step 3 (final): build the audio buffer,
                                    // refreshed against real time ---
                                    // Mirrors the single-TX arm's equivalent block
                                    // (tx.rs Task 3). request_received_at (Step 2)
                                    // already decided WHICH slot to target and
                                    // whether to defer — never re-derived here.
                                    // raw_samples is whatever the fast-path/rebuild
                                    // resolution above produced (untrimmed); trim it
                                    // ONCE here, against a freshly-read clock and the
                                    // already-decided schedule.target_slot, so the
                                    // transmitted waveform stays aligned to the real
                                    // FT8 slot grid regardless of how long the steps
                                    // above (adaptive coalesce window, encoding,
                                    // possible key-time re-encode) took. See
                                    // docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md §2.
                                    // PAN-273: wait out an outstanding drop-fallback
                                    // PTT-off before the refresh (see the single-TX arm).
                                    pending_ptt_off.settle().await;
                                    let (fresh_pad_samples, fresh_cursor_samples) =
                                        pad_and_cursor_for_target(
                                            utc_clock(),
                                            schedule.target_slot,
                                            sample_rate,
                                        );
                                    schedule.silent_pad_samples = fresh_pad_samples;
                                    schedule.cursor_offset_samples = fresh_cursor_samples;

                                    if schedule.cursor_offset_samples >= raw_samples.len() {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform at key-time; dropping");
                                        for (text, qso_id) in
                                            item_texts.iter().zip(encoded_qso_ids_final.iter())
                                        {
                                            emit_tx_failure_diagnostic(
                                                &message_bus,
                                                qso_id.as_deref(),
                                                text,
                                                "internal scheduling error (cursor exceeded multi-TX waveform at key-time)",
                                            )
                                            .await;
                                        }
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for ((text, qso_id), item) in item_texts
                                            .into_iter()
                                            .zip(encoded_qso_ids_final)
                                            .zip(&items)
                                        {
                                            let _ = report_transmit_complete(
                                                &message_bus,
                                                false,
                                                text,
                                                0,
                                                qso_id,
                                                item.attempt_id,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    let mut audio_out = Vec::with_capacity(
                                        schedule.silent_pad_samples + raw_samples.len()
                                            - schedule.cursor_offset_samples,
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    audio_out.extend_from_slice(
                                        &raw_samples[schedule.cursor_offset_samples..],
                                    );
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;

                                    // --- Step 5: Assert PTT ---
                                    #[cfg(test)]
                                    tx_before_ptt_gate.notify_one();
                                    let mut ptt_guard = PttGuard::new(
                                        message_bus.clone(),
                                        ptt_active.clone(),
                                        &ptt_sync_gate,
                                        &last_ptt_on_ms,
                                        &last_ptt_off_ms,
                                        &pending_ptt_off,
                                    )
                                    .await;
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;
                                    // NOW-SENDING: the whole bundle is keyed and on the
                                    // air CONCURRENTLY in this one slot. Show the first
                                    // item as the headline "now" and the rest as
                                    // non-deferred companions — the strip renders these as
                                    // concurrent ("NOW ×N"), not as future-slot queue.
                                    {
                                        let mut bundle: Vec<crate::message_bus::TxItem> = items
                                            .iter()
                                            .map(|it| crate::message_bus::TxItem {
                                                text: it.message_text.clone(),
                                                freq_hz: it.frequency_offset,
                                                qso_id: it.qso_id.clone(),
                                                deferred: false,
                                            })
                                            .collect();
                                        let head = if bundle.is_empty() {
                                            None
                                        } else {
                                            Some(bundle.remove(0))
                                        };
                                        send_tx_queue_status(&message_bus, head, bundle).await;
                                    }

                                    let live_timing_guard = ptt_sync_gate.lock().await;
                                    // --- Step 4d: final atomic pre-PTT gate ---
                                    // PAN-148 (Finding 1, PR #370 round 2): the
                                    // partial-staleness branch above emits per-item
                                    // diagnostic/`TransmitComplete` awaits AFTER `live_mask`
                                    // was computed; Step 5's status-announce awaits just
                                    // above add more. Any of them can span a survivor QSO
                                    // ending or its DX moving onto the exact parity we're
                                    // about to key into. Re-check every item in
                                    // `encoded_qso_ids_final` (what's actually baked into
                                    // `audio_out`) against fresh state, here, immediately
                                    // before the arm recheck and the PTT-on send.
                                    //
                                    // Unlike Step 4b, a staleness found HERE does not
                                    // trigger a second re-encode of the shrunk survivor
                                    // set — abort the whole bundle instead. See
                                    // docs/superpowers/specs/2026-09-12-pan-148-final-atomic-ptt-gate-design.md
                                    // for why: this window is the handful of near-instant
                                    // in-process channel sends between Step 4b and here (no
                                    // I/O, no sleep), several orders of magnitude narrower
                                    // than the ~20-30s pre-PTT sleep Step 4b's own rebuild
                                    // already covers — trading a vanishingly rare survivor
                                    // waiting one more cycle for never keying stale/held
                                    // evidence, without adding a second synchronous-rebuild
                                    // path through this file's highest-review-churn region.
                                    // SYNCHRONOUS (uses `final_gate_callsigns`, resolved
                                    // before Step 3's trim above).
                                    //
                                    // PAN-117: the slot-timing re-check runs LAST, just
                                    // before the PTT-on send below, not here.
                                    // Rebuilds can reject an item after the snapshot. Only
                                    // QSOs actually present in the summed waveform matter.
                                    relocation_snapshot
                                        .retain(|(id, _)| encoded_qso_ids_final.contains(id));
                                    if relocation_epoch_stale(
                                        &relocation_epochs,
                                        &relocation_snapshot,
                                    ) {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let diagnostic = "dropping multi-TX bundle at final pre-PTT check: QSO relocated during gate wait";
                                        info!(target: "pancetta::tx.policy", "{diagnostic}");
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            diagnostic.to_string(),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    let (final_live_mask, final_parity_hold_mask) =
                                        bundle_live_mask(
                                            &encoded_qso_ids_final,
                                            &final_gate_callsigns,
                                            required_parity,
                                            DxParityContext::live(&operating_frequency_hz, slot_ns),
                                            &active_tx_qsos,
                                            &cross_time_state,
                                        );
                                    if !final_live_mask.iter().all(|&live| live) {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let newly_stale =
                                            final_live_mask.iter().filter(|&&live| !live).count();
                                        let held = final_parity_hold_mask
                                            .iter()
                                            .zip(final_live_mask.iter())
                                            .filter(|(&held, &live)| held && !live)
                                            .count();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "dropping multi-TX bundle at final pre-PTT check: {newly_stale} of {} item(s) ended or held (colliding DX parity) since the key-time check ({held} held)",
                                            final_live_mask.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "dropping multi-TX bundle at final pre-PTT check: {newly_stale} of {} item(s) ended or held (colliding DX parity) since the key-time check",
                                                final_live_mask.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // PAN-115: re-run the TX hard mute here too, mirroring
                                    // the single-TX path. The post-wait check above runs
                                    // before Step 4b's re-encode and the status awaits, so
                                    // a rig CAT loss or dead audio output (or any other
                                    // hard-mute reason) landing in that window would
                                    // otherwise still key. SYNCHRONOUS (atomics + mutex),
                                    // so "nothing else runs between this call and the key"
                                    // still holds. Same unwind as Step 4d.
                                    if let Some(reason) = tx_hard_mute_reason(
                                        &tx_policy,
                                        &tx_restart_inhibit,
                                        &hamlib_command_loop_ready,
                                        &hamlib_pending_frequency,
                                        &hamlib_pending_split,
                                        &hamlib_command_in_flight,
                                        &rig_conn_state,
                                        &audio_output_alive,
                                        &message_bus.receive_gain,
                                        &message_bus.data_mode,
                                    ) {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let n = items.len();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX bundle blocked at final pre-PTT check ({reason}): {n} items"
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX bundle blocked at final pre-PTT check ({reason}): {n} items"
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // PAN-118: the band-plan sibling of the final
                                    // pre-PTT mute above — synchronous (atomics +
                                    // the startup band-plan snapshot), so "nothing
                                    // else runs between this call and the key"
                                    // still holds. Same physics and same unwind as
                                    // the post-wait site: the waveform is summed,
                                    // so ANY out-of-privilege item holds the whole
                                    // bundle (nothing keys; all items unaired).
                                    if let Some(reason) = items.iter().find_map(|item| {
                                        tx_rf_for_gate(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                            item.frequency_offset,
                                        )
                                        .and_then(|rf| {
                                            tx_band_plan_mute_reason(
                                                rf,
                                                tx_signal_span_hz(active_protocol),
                                                &band_plan,
                                            )
                                        })
                                    }) {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let n = items.len();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX bundle blocked at final pre-PTT check ({reason}): {n} items"
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX bundle blocked at final pre-PTT check ({reason}): {n} items"
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    // PAN-117 (six-lane review L5 H-7; PR #450 Codex
                                    // review): re-check slot-timing headroom LAST among the
                                    // pre-send gates — after the liveness mask, the hard
                                    // mute and the band-plan gate above — so this clock
                                    // read is the last one before the PTT-on send. The
                                    // whole bundle shares one slot (per
                                    // `multi_tx_status_items`' doc), so this is a
                                    // bundle-level check. A frame keyed more than
                                    // `tx_late_max_ms` past the slot boundary is
                                    // mathematically undecodable for any receiver; the
                                    // queue-time `schedule_tx` decision never sees the
                                    // coalesce/encode/gate delays, so it is dropped HERE
                                    // through the same whole-bundle unwind as the liveness
                                    // branch above. SYNCHRONOUS through the key-down
                                    // enqueue below: `send_ptt_command` takes no lock and
                                    // no await (PAN-202).
                                    if items.iter().any(|item| {
                                        is_autonomous_cq(
                                            item.qso_id.as_deref(),
                                            item.attempt_id,
                                            &item.message_text,
                                        )
                                    }) && !autonomous_config_enabled.load(Ordering::Acquire)
                                    {
                                        ptt_guard.disarm();
                                        ptt_active.store(false, Ordering::Release);
                                        drop(live_timing_guard);
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }
                                    let key_time_now = utc_clock();
                                    let key_time_late_max_ms = timing.late_cap_ms();
                                    if key_time_too_late(
                                        key_time_now,
                                        schedule.target_slot,
                                        key_time_late_max_ms,
                                    ) {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let elapsed_ms = (key_time_now - schedule.target_slot)
                                            .num_milliseconds()
                                            .max(0)
                                            as u64;
                                        let n = items.len();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "dropping multi-TX bundle at final pre-PTT check: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={key_time_late_max_ms}ms — no receiver could decode it ({n} frame(s))"
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "dropping multi-TX bundle at final pre-PTT check: {elapsed_ms}ms into the slot exceeds tx_late_max_ms={key_time_late_max_ms}ms — no receiver could decode it ({n} frame(s))"
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let _ = report_unaired_frame(
                                                &message_bus,
                                                item.message_text.clone(),
                                                item.qso_id.clone(),
                                                item.attempt_id,
                                                take_bundle_replaced(
                                                    &mut bundle_replaced,
                                                    item.qso_id.as_deref(),
                                                ),
                                            )
                                            .await;
                                        }
                                        continue;
                                    }

                                    #[cfg(test)]
                                    key_down_probe.pause_if_armed().await;

                                    // --- Step 4d-arm: last-instant pre-PTT arm recheck (PAN-134)
                                    // Authorization runs immediately before the enqueue, with no lock
                                    // or await between it, the gates above and the enqueue (PAN-134,
                                    // PAN-202). The mid-TX poll remains a backstop.
                                    // PTT hardware is not asserted yet. Denial only unwinds the local
                                    // PTT flag, pivot tombstones, and the announced TX/queue status.
                                    let outcome = send_ptt_command(
                                        &message_bus,
                                        ComponentId::Ft8Transmitter,
                                        true,
                                        || {
                                            tx_send_authorized(
                                                origin,
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            )
                                        },
                                    );
                                    drop(live_timing_guard);
                                    match outcome {
                                        SendIfOutcome::Enqueued => {
                                            info!(
                                                target: "pancetta::tx.ptt",
                                                "PTT ON (scheduled multi-TX) sent to rig"
                                            );
                                        }
                                        SendIfOutcome::Denied => {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            // Round-6 review (Codex P2): nothing reached the
                                            // air — remove any tombstone this bundle's Step
                                            // 4b-pivot just inserted, so the genuinely fresh
                                            // request still queued behind it isn't discarded by
                                            // Step 0-dup as an already-sent duplicate.
                                            let mut bundle_replaced = unwind_bundle_pivots(
                                                &pivoted_this_bundle_keys,
                                                &mut pivoted_once,
                                                &mut pivoted_from,
                                            );
                                            let operator_callsign = tx_denied_operator_attribution(
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            );
                                            for item in &items {
                                                let denial_reason = format!(
                                                    "arm went stale in the pre-PTT gap: '{}' at {:.0} Hz",
                                                    item.message_text, item.frequency_offset
                                                );
                                                emit_disarm_interrupt_signals(
                                                    &message_bus,
                                                    &audit_log,
                                                    &display_feed_enabled,
                                                    operator_callsign.clone(),
                                                    "denied",
                                                    denial_reason,
                                                    item.qso_id.as_deref(),
                                                )
                                                .await;
                                            }
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for item in &items {
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    item.message_text.clone(),
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                    take_bundle_replaced(
                                                        &mut bundle_replaced,
                                                        item.qso_id.as_deref(),
                                                    ),
                                                )
                                                .await;
                                            }
                                            continue;
                                        }
                                        // PAN-278: the bus dropped the bundle's key-down
                                        // (full or disconnected Hamlib channel), so the rig
                                        // was never keyed. Unwind like the `Denied` arm, with
                                        // no PTT-off (nothing entered Hamlib's queue), and
                                        // settle every item as not sent. Without a Hamlib
                                        // route (`--replay`) the drop is by design and the
                                        // audio still goes out, so that case keeps the arm
                                        // below.
                                        SendIfOutcome::Dropped if message_bus.has_rig_route() => {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let mut bundle_replaced = unwind_bundle_pivots(
                                                &pivoted_this_bundle_keys,
                                                &mut pivoted_once,
                                                &mut pivoted_from,
                                            );
                                            warn!(
                                                target: "pancetta::tx.ptt",
                                                "PTT-on dropped by the bus (rig not keyed): multi-TX bundle of {} items not sent",
                                                items.len()
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.ptt",
                                                pancetta_core::DiagnosticLevel::Warn,
                                                format!(
                                                    "PTT-on dropped (rig not keyed): multi-TX bundle of {} items not sent",
                                                    items.len()
                                                ),
                                                None,
                                            )
                                            .await;
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for item in &items {
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    item.message_text.clone(),
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                    take_bundle_replaced(
                                                        &mut bundle_replaced,
                                                        item.qso_id.as_deref(),
                                                    ),
                                                )
                                                .await;
                                            }
                                            continue;
                                        }
                                        SendIfOutcome::Dropped => {
                                            warn!("PTT ON not sent: no Hamlib route registered (replay mode)");
                                        }
                                    }

                                    // --- Step 6: Sleep precisely until target slot ---
                                    // ptt_guard in scope — drop on any loop exit fires PTT-off.
                                    // A qualifying request arriving here supersedes the in-flight
                                    // bundle: prefer folding it in (bundle-add), else re-enqueue
                                    // a single-item replace — see `supersede_multi_reenqueue`.
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        utc_clock(),
                                    );
                                    // PAN-114 (D6): never hold PTT through a backward clock
                                    // step — see the single-TX arm's identical guard.
                                    if ptt_hold_exceeds_bound(to_slot, ptt_lead_ms) {
                                        ptt_guard.release("Clock-step PTT hold guard").await;
                                        // Pre-slot abort: nothing reached the air yet.
                                        let mut bundle_replaced = unwind_bundle_pivots(
                                            &pivoted_this_bundle_keys,
                                            &mut pivoted_once,
                                            &mut pivoted_from,
                                        );
                                        let frames: Vec<_> = items
                                            .iter()
                                            .map(|item| {
                                                (
                                                    item.message_text.clone(),
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                )
                                            })
                                            .collect();
                                        drop_frames_for_clock_jump(
                                            &message_bus,
                                            &frames,
                                            pancetta_core::slot_clock::WallClockJump::Backward(
                                                chrono::Duration::from_std(to_slot)
                                                    .unwrap_or(chrono::Duration::MAX),
                                            ),
                                            &mut bundle_replaced,
                                        )
                                        .await;
                                        continue;
                                    }
                                    let outcome = interruptible_sleep_or_supersede_with_clock(
                                        to_slot,
                                        &shutdown,
                                        &abort_current_tx,
                                        &inbox,
                                        // PAN-73 round 2 (Codex): every in-flight bundle item,
                                        // not just the first -- a fresher message for the
                                        // bundle's 2nd+ QSO must match here too.
                                        &items,
                                        &pivoted_once,
                                        remote_arm_gate(
                                            origin,
                                            &remote_tx_arm,
                                            remote_client_key_id.as_deref(),
                                        ),
                                        arm_clock.as_ref(),
                                    )
                                    .await;
                                    match on_sleep_outcome(
                                        outcome,
                                        SleepPhase::PreSlot,
                                        SleepInFlight::Bundle {
                                            items: &items,
                                            pivoted_this_bundle_keys: &pivoted_this_bundle_keys,
                                        },
                                        AbortCtx {
                                            message_bus: &message_bus,
                                            audit_log: &audit_log,
                                            display_feed_enabled: &display_feed_enabled,
                                            remote_tx_arm: &remote_tx_arm,
                                            remote_client_key_id: remote_client_key_id.as_deref(),
                                            ptt_guard: &mut ptt_guard,
                                            pivoted_once: &mut pivoted_once,
                                            pivoted_from: &mut pivoted_from,
                                            shutdown: &shutdown,
                                            sample_rate,
                                        },
                                    )
                                    .await
                                    {
                                        SleepFlow::Continue(()) => {}
                                        SleepFlow::Break(SleepBreak::Exit(TxExit::StopWorker)) => {
                                            break;
                                        }
                                        SleepFlow::Break(SleepBreak::Exit(TxExit::NextRequest)) => {
                                            continue;
                                        }
                                        SleepFlow::Break(SleepBreak::Superseded(boxed)) => {
                                            let new_request = *boxed;
                                            supersede_multi_reenqueue(
                                                new_request,
                                                &SupersedeInFlight {
                                                    items: &items,
                                                    origin,
                                                    remote_client_key_id: remote_client_key_id
                                                        .clone(),
                                                    tx_parity,
                                                },
                                                &timing,
                                                live_supersede_max_concurrent(),
                                                SupersedeIo {
                                                    message_bus: &message_bus,
                                                    inbox: &inbox,
                                                    encoder: &mut encoder,
                                                    ptt_guard: &mut ptt_guard,
                                                },
                                            )
                                            .await;
                                            continue;
                                        }
                                    }

                                    // --- Step 7: Route audio to output ---
                                    // PAN-134: recheck authorization after the bus lock wait, with no
                                    // suspension before audio enqueue. PTT is already asserted, so
                                    // denial releases it first. The mid-TX poll remains a backstop.
                                    // Round-9 review: denied frames must never appear in Band Activity
                                    // as genuine own-transmissions. Log only on Enqueued, so dropped
                                    // audio cannot appear as an aired frame either.
                                    let (playback_ack, mut playback_waiter) =
                                        PlaybackStartAck::new();
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                            // Supersede requeues lose in-place rekey history.
                                            // Each complete bundle replaces any queued predecessor.
                                            flush_first: true,
                                            playback_started: Some(playback_ack),
                                        },
                                        Instant::now(),
                                    );
                                    let outcome = message_bus
                                        .send_message_if(audio_msg, || {
                                            tx_send_authorized(
                                                origin,
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            )
                                        })
                                        .await;
                                    // PR #468 review: playback is timed from the enqueue, so the
                                    // bounded TX-log wait below never extends key-down past the audio.
                                    let audio_enqueued_at = tokio::time::Instant::now();
                                    // PAN-182 (PR #507 review): only an `Enqueued` outcome put
                                    // samples on the Audio route. A `Dropped`/error delivery keyed
                                    // no audio, so Step 10 must report every item as not sent.
                                    let audio_enqueued =
                                        matches!(outcome, Ok(SendIfOutcome::Enqueued));
                                    match outcome {
                                        Ok(SendIfOutcome::Enqueued) => {
                                            // PAN-71: report the callback's first sample, with a bounded
                                            // wait and enqueue-time fallback. Only authorized, enqueued
                                            // frames are logged (PAN-134). The durable record precedes
                                            // the TUI event and uses the same instant (PAN-122).
                                            let enqueued_utc = utc_clock();
                                            let bundle_text = items
                                                .iter()
                                                .map(|item| item.message_text.as_str())
                                                .collect::<Vec<_>>()
                                                .join(" | ");
                                            let tx_logged_at = frame_log_instant(
                                                &mut playback_waiter,
                                                enqueued_utc,
                                                &bundle_text,
                                            )
                                            .await;
                                            // PAN-122: one durable TX-log batch (one fsync)
                                            // for every item, written before the TUI events
                                            // with the same timestamp; bounded wait, as in
                                            // the single-frame arm.
                                            let tx_dial_hz = tx_log_dial_hz(
                                                &operating_frequency_hz,
                                                &split_tx_frequency_hz,
                                            );
                                            let records = items
                                                .iter()
                                                .map(|item| {
                                                    TxFrameRecord::new(
                                                        tx_logged_at,
                                                        active_protocol,
                                                        tx_dial_hz,
                                                        item.frequency_offset,
                                                        item.message_text.clone(),
                                                        // Each item's own provenance (PAN-136), not the
                                                        // bundle's fail-safe folded `origin`, which is
                                                        // `Remote` for any mixed bundle (PR #468 review).
                                                        item.origin,
                                                        item.qso_id.clone(),
                                                    )
                                                })
                                                .collect();
                                            let _ =
                                                tx_frame_log.append(records, TX_LOG_ACK_WAIT).await;
                                            for item in &items {
                                                log_tx_frame(
                                                    &message_bus,
                                                    item.message_text.clone(),
                                                    item.frequency_offset,
                                                    item.qso_id.clone(),
                                                    tx_logged_at,
                                                )
                                                .await;
                                            }
                                        }
                                        Ok(SendIfOutcome::Denied) => {
                                            // PAN-135: stand the guard down only after
                                            // Hamlib accepts PTT-off (checked, bounded
                                            // retry — see `PttGuard::release`).
                                            ptt_guard
                                                .release("Remote disarm before audio delivery")
                                                .await;
                                            let mut bundle_replaced = unwind_bundle_pivots(
                                                &pivoted_this_bundle_keys,
                                                &mut pivoted_once,
                                                &mut pivoted_from,
                                            );
                                            let operator_callsign = tx_denied_operator_attribution(
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            );
                                            for item in &items {
                                                let denial_reason = format!(
                                                    "arm went stale before audio delivery: '{}' at {:.0} Hz",
                                                    item.message_text, item.frequency_offset
                                                );
                                                emit_disarm_interrupt_signals(
                                                    &message_bus,
                                                    &audit_log,
                                                    &display_feed_enabled,
                                                    operator_callsign.clone(),
                                                    "denied",
                                                    denial_reason,
                                                    item.qso_id.as_deref(),
                                                )
                                                .await;
                                            }
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for item in &items {
                                                let _ = report_unaired_frame(
                                                    &message_bus,
                                                    item.message_text.clone(),
                                                    item.qso_id.clone(),
                                                    item.attempt_id,
                                                    take_bundle_replaced(
                                                        &mut bundle_replaced,
                                                        item.qso_id.as_deref(),
                                                    ),
                                                )
                                                .await;
                                            }
                                            continue;
                                        }
                                        Ok(SendIfOutcome::Dropped) => {
                                            debug!("Audio output routing: message dropped");
                                        }
                                        Err(e) => {
                                            debug!("Audio output routing: {}", e);
                                        }
                                    }

                                    // --- Step 8: Wait for playback to complete ---
                                    // Playback is now on the air; a qualifying request here
                                    // supersedes the bundle (same bundle-add-or-replace decision
                                    // as Step 6 via `supersede_multi_reenqueue`).
                                    // PAN-135: expected playback end bounds an unconfirmed disarm flush.
                                    let playback_end = audio_enqueued_at
                                        + Duration::from_millis(audio_duration_ms);
                                    let outcome = interruptible_sleep_or_supersede_with_clock(
                                        playback_end
                                            .saturating_duration_since(tokio::time::Instant::now()),
                                        &shutdown,
                                        &abort_current_tx,
                                        &inbox,
                                        // PAN-73 round 2 (Codex): every in-flight bundle item,
                                        // not just the first -- a fresher message for the
                                        // bundle's 2nd+ QSO must match here too.
                                        &items,
                                        &pivoted_once,
                                        remote_arm_gate(
                                            origin,
                                            &remote_tx_arm,
                                            remote_client_key_id.as_deref(),
                                        ),
                                        arm_clock.as_ref(),
                                    )
                                    .await;
                                    match on_sleep_outcome(
                                        outcome,
                                        SleepPhase::Playback { playback_end },
                                        SleepInFlight::Bundle {
                                            items: &items,
                                            pivoted_this_bundle_keys: &pivoted_this_bundle_keys,
                                        },
                                        AbortCtx {
                                            message_bus: &message_bus,
                                            audit_log: &audit_log,
                                            display_feed_enabled: &display_feed_enabled,
                                            remote_tx_arm: &remote_tx_arm,
                                            remote_client_key_id: remote_client_key_id.as_deref(),
                                            ptt_guard: &mut ptt_guard,
                                            pivoted_once: &mut pivoted_once,
                                            pivoted_from: &mut pivoted_from,
                                            shutdown: &shutdown,
                                            sample_rate,
                                        },
                                    )
                                    .await
                                    {
                                        SleepFlow::Continue(()) => {}
                                        SleepFlow::Break(SleepBreak::Exit(TxExit::StopWorker)) => {
                                            break;
                                        }
                                        SleepFlow::Break(SleepBreak::Exit(TxExit::NextRequest)) => {
                                            continue;
                                        }
                                        SleepFlow::Break(SleepBreak::Superseded(boxed)) => {
                                            let new_request = *boxed;
                                            supersede_multi_reenqueue(
                                                new_request,
                                                &SupersedeInFlight {
                                                    items: &items,
                                                    origin,
                                                    remote_client_key_id: remote_client_key_id
                                                        .clone(),
                                                    tx_parity,
                                                },
                                                &timing,
                                                live_supersede_max_concurrent(),
                                                SupersedeIo {
                                                    message_bus: &message_bus,
                                                    inbox: &inbox,
                                                    encoder: &mut encoder,
                                                    ptt_guard: &mut ptt_guard,
                                                },
                                            )
                                            .await;
                                            continue;
                                        }
                                    }
                                    // PAN-182 (PR #507 review): success means the audio actually
                                    // enqueued at Step 7 — a `Dropped`/error delivery keyed no
                                    // samples, so no item aired and Step 10 reports each through
                                    // the PAN-75 not-sent path instead.
                                    let success = audio_enqueued;
                                    let duration_ms = audio_duration_ms;

                                    // --- Step 9: De-assert PTT (with tail delay) ---
                                    if interruptible_sleep(
                                        Duration::from_millis(50),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted during tail by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted during tail by operator (F8)");
                                        // PAN-38 round 4 (Codex): the audio already fully
                                        // played out before this trailing guard period, so
                                        // `success`/`duration_ms` already reflect a genuine
                                        // completed transmission -- report it with those
                                        // real values now, per item, rather than skipping
                                        // TransmitComplete entirely (leaking every bundle
                                        // item's pending_self_cq_qsos entry).
                                        for ((text, qso_id), item) in item_texts
                                            .into_iter()
                                            .zip(encoded_qso_ids_final)
                                            .zip(&items)
                                        {
                                            if let Err(e) = report_played_transmit_complete(
                                                &message_bus,
                                                success,
                                                text,
                                                duration_ms,
                                                qso_id,
                                                aired_payload(
                                                    item.frequency_offset,
                                                    item.origin,
                                                    item.remote_client_key_id.clone(),
                                                ),
                                                item.attempt_id,
                                            )
                                            .await
                                            {
                                                warn!("Failed to send TransmitComplete: {}", e);
                                            }
                                        }
                                        continue;
                                    }
                                    ptt_guard.release("Multi-TX Step 9 unkey").await;

                                    // --- Step 10: Send TransmitComplete for each item ---
                                    for ((text, qso_id), item) in item_texts
                                        .into_iter()
                                        .zip(encoded_qso_ids_final)
                                        .zip(&items)
                                    {
                                        if let Err(e) = report_played_transmit_complete(
                                            &message_bus,
                                            success,
                                            text,
                                            duration_ms,
                                            qso_id,
                                            aired_payload(
                                                item.frequency_offset,
                                                item.origin,
                                                item.remote_client_key_id.clone(),
                                            ),
                                            item.attempt_id,
                                        )
                                        .await
                                        {
                                            warn!("Failed to send TransmitComplete: {}", e);
                                        }
                                    }
                                }

                                MessageType::TuneRequest {
                                    duration_secs,
                                    tone_offset_hz,
                                } => {
                                    info!("Tune: {}s tone at {} Hz", duration_secs, tone_offset_hz);
                                    TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);
                                    // PAN-273: wait out an outstanding drop-fallback
                                    // PTT-off before the gates below, not after them.
                                    pending_ptt_off.settle().await;

                                    // --- TX-policy hard mute ---
                                    // A tune carrier is a transmission. If the
                                    // global policy is Disabled (RX-only), never
                                    // key PTT / emit the tone. This is the
                                    // catch-all gate matching the
                                    // TransmitRequest / MultiTransmitRequest
                                    // arms — defends against any TuneRequest
                                    // source, not just the TUI relay.
                                    if let Some(reason) = tx_hard_mute_reason(
                                        &tx_policy,
                                        &tx_restart_inhibit,
                                        &hamlib_command_loop_ready,
                                        &hamlib_pending_frequency,
                                        &hamlib_pending_split,
                                        &hamlib_command_in_flight,
                                        &rig_conn_state,
                                        &audio_output_alive,
                                        &message_bus.receive_gain,
                                        &message_bus.data_mode,
                                    ) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({}): tune ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): tune ({duration_secs}s @ {tone_offset_hz} Hz)"
                                            ),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // PAN-118: a tune carrier is a transmission
                                    // too — refuse it on the effective dial +
                                    // tone offset before PTT, mirroring the
                                    // band-plan sibling gate at the other
                                    // sites. Same unwind as the hard-mute
                                    // branch above (no TransmitComplete exists
                                    // for a tune).
                                    if let Some(reason) = tx_rf_for_gate(
                                        &operating_frequency_hz,
                                        &split_tx_frequency_hz,
                                        tone_offset_hz,
                                    )
                                    .and_then(|rf| tx_band_plan_mute_reason(rf, 0.0, &band_plan))
                                    {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({}): tune ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): tune ({duration_secs}s @ {tone_offset_hz} Hz)"
                                            ),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // Generate a continuous sine wave
                                    // (single tone, zero-bandwidth on air).
                                    // Amplitude 0.5 — operator manages rig
                                    // power. WSJT-X uses peak amplitude;
                                    // we run gentler so a forgotten rig
                                    // power setting is less likely to
                                    // overdrive.
                                    let n_samples =
                                        (duration_secs as usize) * (sample_rate as usize);
                                    let omega = 2.0 * std::f64::consts::PI * tone_offset_hz
                                        / sample_rate as f64;
                                    let tone_samples: Vec<f32> = (0..n_samples)
                                        .map(|i| ((i as f64) * omega).sin() as f32 * 0.5)
                                        .collect();
                                    let audio_duration_ms = (duration_secs as u64) * 1000;

                                    // Engage PTT immediately. No slot
                                    // scheduling: tune happens NOW.
                                    let mut ptt_guard = PttGuard::new(
                                        message_bus.clone(),
                                        ptt_active.clone(),
                                        &ptt_sync_gate,
                                        &last_ptt_on_ms,
                                        &last_ptt_off_ms,
                                        &pending_ptt_off,
                                    )
                                    .await;
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;

                                    // PAN-118: last-instant band-plan re-check.
                                    // The tone-buffer build, `PttGuard::new`'s
                                    // sync-gate wait and the TX-status send
                                    // above all ran since the admission check,
                                    // and the effective TX dial may have moved
                                    // meanwhile. Nothing but the PTT-on send
                                    // follows. PTT hardware was never asserted
                                    // (`ptt_guard` only flipped the in-process
                                    // flag), so unwind that flag and disarm
                                    // rather than send PTT-off; the TX-badge
                                    // guard's drop clears the badge.
                                    if let Some(reason) = tx_rf_for_gate(
                                        &operating_frequency_hz,
                                        &split_tx_frequency_hz,
                                        tone_offset_hz,
                                    )
                                    .and_then(|rf| tx_band_plan_mute_reason(rf, 0.0, &band_plan))
                                    {
                                        last_ptt_off_ms
                                            .store(super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked at final pre-PTT check ({}): tune ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked at final pre-PTT check ({reason}): tune ({duration_secs}s @ {tone_offset_hz} Hz)"
                                            ),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    }

                                    #[cfg(test)]
                                    key_down_probe.pause_if_armed().await;
                                    let ptt_on = send_ptt_command(
                                        &message_bus,
                                        ComponentId::Ft8Transmitter,
                                        true,
                                        || true,
                                    );
                                    if ptt_on != SendIfOutcome::Enqueued {
                                        // `Denied` here is the receive-gain gate, never
                                        // `authorize` (always true for a tune).
                                        let dropped = ptt_on == SendIfOutcome::Dropped;
                                        // PAN-278: with a Hamlib route the key-down was
                                        // dropped (full or disconnected channel) or
                                        // refused, so the rig was never keyed. Abandon
                                        // the tune rather than play a tone with no
                                        // carrier. Unwind the in-process flag like the
                                        // band-plan refusal above: nothing entered
                                        // Hamlib's queue, so there is no PTT-off to send.
                                        // Without a route (`--replay`) the drop is by design
                                        // and the tone still plays.
                                        if message_bus.has_rig_route() {
                                            last_ptt_off_ms
                                                .store(super::now_epoch_ms(), Ordering::Release);
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let why = if dropped { "dropped" } else { "refused" };
                                            warn!(
                                                target: "pancetta::tx.ptt",
                                                "Tune: PTT-on {} (rig not keyed); tune ({}s @ {} Hz) abandoned",
                                                why, duration_secs, tone_offset_hz
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.ptt",
                                                pancetta_core::DiagnosticLevel::Warn,
                                                format!(
                                                    "Tune: PTT-on {why} (rig not keyed); tune abandoned"
                                                ),
                                                None,
                                            )
                                            .await;
                                            continue;
                                        }
                                        if dropped {
                                            warn!(
                                                "Tune: PTT ON not sent: no Hamlib route registered (replay mode)"
                                            );
                                        } else {
                                            warn!("Tune: PTT ON refused (rig not keyed)");
                                        }
                                    }

                                    // Emit the audio buffer.
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: tone_samples,
                                            sample_rate,
                                            flush_first: false,
                                            // Tune logs no frame.
                                            playback_started: None,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Tune: audio output routing: {}", e);
                                    }

                                    // Wait for the duration. F4-toggle-off
                                    // and F8-halt both flip
                                    // abort_current_tx and wake the sleep
                                    // within 50ms; on wake, ptt_guard's
                                    // Drop fires PTT-off and we exit the
                                    // arm cleanly.
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Tune aborted by shutdown");
                                            break;
                                        }
                                        info!("Tune aborted by operator");
                                        // Drop ptt_guard via continue.
                                        continue;
                                    }

                                    // Natural completion: explicit PTT-off
                                    // (matches the regular TX path).
                                    ptt_guard.release("Tune unkey").await;
                                    info!("Tune: complete");
                                }

                                _ => {} // Ignore other message types
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                info!("FT8 transmitter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Ft8Transmitter, tx_handle));
        info!("FT8 transmitter component started");
        Ok(())
    }
}

/// Resolve the slot parity to use for a given TX request, falling back
/// to the configured self-parity (`Auto` picks whichever next slot is
/// closer to `now`).
pub fn resolve_required_parity(
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    now: chrono::DateTime<chrono::Utc>,
    slot_ns: i64,
) -> pancetta_core::slot::SlotParity {
    use pancetta_config::station::TxSelfParity;
    use pancetta_core::slot::{next_slot_with_parity_with_period, SlotParity};
    if let Some(p) = tx_parity {
        return p;
    }
    match tx_self_parity {
        TxSelfParity::Even => SlotParity::Even,
        TxSelfParity::Odd => SlotParity::Odd,
        TxSelfParity::Auto => {
            let next_even = next_slot_with_parity_with_period(now, SlotParity::Even, slot_ns);
            let next_odd = next_slot_with_parity_with_period(now, SlotParity::Odd, slot_ns);
            if next_even <= next_odd {
                SlotParity::Even
            } else {
                SlotParity::Odd
            }
        }
    }
}

#[cfg(test)]
impl SlotTimingContext {
    /// Test-only FT8 context: the supersede tests' old explicit arguments,
    /// with the default 80 ms `ptt_lead_ms` (unused by the supersede path).
    fn ft8_for_test(
        tx_late_max_ms: u64,
        sample_rate: u32,
        slot_ns: i64,
        tx_self_parity: pancetta_config::station::TxSelfParity,
        request_received_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            active_protocol: pancetta_ft8::Protocol::Ft8,
            tx_late_max_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(tx_late_max_ms)),
            tx_self_parity,
            ptt_lead_ms: 80,
            sample_rate,
            slot_ns,
            request_received_at,
            utc_clock: pancetta_core::time::system_utc_clock(),
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/slot_timing_context_tests.rs"]
mod slot_timing_context_tests;

#[cfg(test)]
#[path = "tx/tests/schedule_tx_tests.rs"]
mod schedule_tx_tests;

/// Regression coverage for the silent-TX-failure gap: an encode/modulate
/// error used to only log a WARN and send a TransmitComplete no consumer
/// reads, so the operator had zero visible signal — the QSO just sat until
/// the watchdog timed out, indistinguishable from the DX not answering.
/// `emit_tx_failure_diagnostic` is the shared helper wired into every
/// genuine TX-attempt-failure site in this file (deliberate policy/security
/// skips are NOT routed through it — see its doc comment). This pins its
/// exact DiagnosticEvent contract directly, independent of the full
/// encode/modulate/PTT pipeline the call sites sit inside.
#[cfg(test)]
#[path = "tx/tests/tx_failure_diagnostic_tests.rs"]
mod tx_failure_diagnostic_tests;

#[cfg(test)]
#[path = "tx/tests/disarm_interrupt_signal_tests.rs"]
mod disarm_interrupt_signal_tests;

#[cfg(test)]
#[path = "tx/tests/supersede_rekey_tests.rs"]
mod supersede_rekey_tests;

#[cfg(test)]
#[path = "tx/tests/tx_frame_logged_tests.rs"]
mod tx_frame_logged_tests;

#[cfg(test)]
#[path = "tx/tests/coalesce_tests.rs"]
mod coalesce_tests;

#[cfg(test)]
#[path = "tx/tests/protocol_tx_tests.rs"]
mod protocol_tx_tests;

/// Unit tests for the remote-TX arm gate helper (`remote_tx_permitted`).
///
/// These lock the safety-critical fail direction: a fresh (unarmed) `ArmState`
/// denies, a fully-armed+consented state permits, and a **poisoned** arm lock
/// **fails CLOSED** (denies) — the opposite of `tx_qso_is_live`'s fail-open.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_tests.rs"]
mod remote_arm_gate_tests;

/// Unit tests for PAN-91's client-identity-bound gate (`remote_tx_permitted_for`).
///
/// These lock the fix's actual invariant: a frame bound to one client must
/// key TX only while THAT client is armed — not merely while *some* client
/// is armed, which is exactly the gap that let a different peer's arm
/// authorize a QSO it never requested.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_identity_tests.rs"]
mod remote_arm_gate_identity_tests;

#[cfg(test)]
#[path = "tx/tests/arm_snapshot_attribution_tests.rs"]
mod arm_snapshot_attribution_tests;

#[cfg(test)]
#[path = "tx/tests/tx_counter_tests.rs"]
mod tx_counter_tests;

#[cfg(test)]
#[path = "tx/tests/classifier_tests.rs"]
mod classifier_tests;

/// PAN-115: the two Gherkin scenarios end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`: a request made
/// while the rig CAT link is lost, or while the audio output is dead, is
/// refused at the key-time gate with no `SetPtt{true}` ever reaching Hamlib.
#[cfg(test)]
#[path = "tx/tests/pan115_worker_gate_tests.rs"]
pub(super) mod pan115_worker_gate_tests;

#[cfg(test)]
#[path = "tx/tests/report_transmit_complete_tests.rs"]
mod report_transmit_complete_tests;

/// PAN-118: the band-plan Gherkin end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`, reusing the
/// PAN-115 harness: a transmission whose absolute TX RF (effective
/// split-aware dial + absolute audio offset) falls outside the configured
/// amateur band plan is refused before `SetPtt{true}` ever reaches Hamlib.
/// The dial-0 "not yet known" sentinel (mock/rig-disabled dev modes,
/// `--test-tx`, replay seed) skips the check, keeping those modes
/// byte-identical.
#[cfg(test)]
#[path = "tx/tests/pan118_worker_band_plan_tests.rs"]
mod pan118_worker_band_plan_tests;

/// PAN-202: key-down reaches Hamlib's queue while another component's
/// channel registration holds the bus table, through the real TX worker over
/// a real `MessageBus`, at every key-down site (single TX, bundle, tune), plus
/// the shared helper the TUI's manual PTT uses.
#[cfg(test)]
#[path = "tx/tests/pan202_key_down_worker_tests.rs"]
mod pan202_key_down_worker_tests;

/// PAN-136: a multi-TX bundle whose item is re-bound during the pre-PTT wait
/// (Step 4b-pivot swaps the item's binding with its payload) is gated under
/// the RE-FOLDED binding, through the real TX worker over a real `MessageBus`.
/// Before PAN-136 the bundle kept its coalesce-time binding: a `Local` bundle
/// keyed a frame now bound to an unarmed remote client, and client A's bundle
/// aired client B's frame under A's gate.
#[cfg(test)]
#[path = "tx/tests/pan136_bundle_binding_worker_tests.rs"]
mod pan136_bundle_binding_worker_tests;

/// PAN-208: the gap-straddling Gherkin end to end, through the real TX
/// worker with a two-range `custom_bands` entry for 20m in its config. A
/// frame whose base tone sits in the first range and top tone in the second
/// is refused before `SetPtt{true}` reaches Hamlib; a frame wholly inside
/// one range still keys.
#[cfg(test)]
#[path = "tx/tests/pan208_worker_band_plan_tests.rs"]
mod pan208_worker_band_plan_tests;

#[cfg(test)]
#[path = "tx/tests/pan135_ptt_release_tests.rs"]
mod pan135_ptt_release_tests;

#[cfg(test)]
#[path = "tx/tests/pan135_disarm_flush_tests.rs"]
mod pan135_disarm_flush_tests;
#[cfg(test)]
#[path = "tx/tests/pan135_worker_disarm_tests.rs"]
mod pan135_worker_disarm_tests;
/// PAN-273: the real TX worker unkeys through `PttGuard::release`, so a
/// PTT-off dropped on a momentarily full Hamlib channel is retried, and a
/// delivered one stands the guard down (no duplicate unkey from its `Drop`).
#[cfg(test)]
#[path = "tx/tests/pan273_worker_unkey_tests.rs"]
mod pan273_worker_unkey_tests;

/// PAN-122: every frame whose audio the real TX worker enqueues gets one
/// durable line in `<home>/.pancetta/tx-log.jsonl` — headless, with the TUI
/// channel full, single or bundled — and nothing else does.
#[cfg(test)]
#[path = "tx/tests/pan122_tx_log_worker_tests.rs"]
mod pan122_tx_log_worker_tests;

/// PAN-146: the DX-parity gate, through the real TX worker, scopes its a7
/// evidence to the live dial's band. A DX kept fresh on 20 m Odd holds an
/// Odd call on 20 m, and does not hold the same call after the operator
/// moved to 15 m.
#[cfg(test)]
#[path = "tx/tests/pan146_dx_parity_context_worker_tests.rs"]
mod pan146_dx_parity_context_worker_tests;

#[cfg(test)]
#[path = "tx/tests/pan182_audio_delivery_tests.rs"]
mod pan182_audio_delivery_tests;

/// PAN-278: a PTT-on that `send_ptt_command` returns `Dropped` while Hamlib's
/// route is registered (here: Hamlib's channel is full) never keyed the rig.
/// The worker must unwind like a pre-PTT refusal, play no audio, and settle
/// the frame as not sent. Without a route (`--replay`) the drop is by
/// design and the frame still plays.
///
/// Hamlib's backlog is read only after the completion: draining it earlier
/// (as `await_completion` does) un-saturates the channel and lets the
/// key-down through, which hides the defect.
#[cfg(test)]
#[path = "tx/tests/pan278_dropped_ptt_on_tests.rs"]
mod pan278_dropped_ptt_on_tests;

/// PAN-223: `report_frame_scheduled` carries the worker's own
/// `TxSchedule::target_slot` to the QSO engine, durably, and only for frames
/// that name both a QSO and a ledger attempt.
#[cfg(test)]
#[path = "tx/tests/pan223_frame_scheduled_report_tests.rs"]
mod pan223_frame_scheduled_report_tests;

/// PAN-223: the real TX worker reports the slot `schedule_tx` picked from its
/// own dequeue instant. Each test ends at the report and never waits for the
/// slot itself.
#[cfg(test)]
#[path = "tx/tests/pan223_scheduled_slot_worker_tests.rs"]
mod pan223_scheduled_slot_worker_tests;

/// PAN-183: each frame's keep-call ledger attempt rides its request through
/// coalescing, pivots and supersedes, and comes back on `TxFrameNotSent`, so
/// the QSO engine rolls back exactly the attempt that failed.
#[cfg(test)]
#[path = "tx/tests/pan183_attempt_identity_tests.rs"]
mod pan183_attempt_identity_tests;
#[cfg(test)]
#[path = "tx/tests/pan71_playback_start_tests.rs"]
mod pan71_playback_start_tests;

#[cfg(test)]
#[path = "tx/tests/receive_overload_ptt_admission_tests.rs"]
mod receive_overload_ptt_admission_tests;

/// PAN-279: every unaired frame settles through `tx_outcome`, driven
/// through the real TX worker.
#[cfg(test)]
#[path = "tx/tests/pan279_settlement_worker_tests.rs"]
mod pan279_settlement_worker_tests;

#[cfg(test)]
mod abort_transcripts;
