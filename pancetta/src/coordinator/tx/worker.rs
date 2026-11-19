use super::*;

/// FT8 nominal pre-roll: audio starts 500ms past the slot boundary.
pub(super) const DELAY_MS: u64 = 500;

/// Total `TransmitRequest`/`MultiTransmitRequest`/`TuneRequest` messages this
/// worker has received this session, incremented before any policy gating
/// (docs/observability-diagnostics-plan.md Layer 3 health panel). Process-
/// global, matching the existing `DECODE_PANIC_COUNT`
/// (`coordinator/ft8.rs`) / `PANIC_COUNT` (`main.rs`) counter pattern — no
/// new locking, no new message-passing.
pub(super) static TX_ATTEMPTS_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Total single-TX requests deferred to a later slot because the request
/// arrived too late to make the current one
/// (`schedule.deferred`, single-TX `TransmitRequest` arm only — multi-TX has
/// no equivalent defer path).
pub(super) static TX_DEFERS_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
pub(super) const COALESCE_COLLECT_WINDOW_MS: u64 = 800;

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
pub(super) fn coalesce_collect_window_ms(protocol: pancetta_ft8::Protocol) -> u64 {
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
pub(super) fn tx_late_max_ms_effective(
    protocol: pancetta_ft8::Protocol,
    tx_late_max_ms: u64,
) -> u64 {
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
pub(super) const COALESCE_MAX_EXTENSION_MS: u64 = 3000;

/// Safety margin subtracted from remaining `tx_late_max_ms` headroom before
/// it's used as the adaptive window's extension cap. Covers ONLY Step 1's
/// encode/modulate time and other fixed per-message overhead AFTER the
/// coalesce point — the mandatory base collection window
/// (`coalesce_collect_window_ms`, always elapsed before any extension) is
/// subtracted from the headroom explicitly in `adaptive_coalesce_cap_ms`
/// (PAN-117). Without both, a request that arrives with headroom just barely
/// covering the extension alone could still get pushed past the
/// `tx_late_max_ms` cliff the key-time final gate enforces.
pub(super) const COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// Protocol-scaled `COALESCE_MAX_EXTENSION_MS` — see that constant's doc.
pub(super) fn coalesce_max_extension_ms(protocol: pancetta_ft8::Protocol) -> u64 {
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
pub(super) fn current_slot_acceptance_cap_ms(
    protocol: pancetta_ft8::Protocol,
    tx_late_max_ms: u64,
) -> u64 {
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
pub(super) fn multi_tx_acceptance_cap_ms(
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
pub(super) fn adaptive_coalesce_cap_ms(
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
pub(super) fn pad_and_cursor_for_target(
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
pub(super) async fn interruptible_sleep(
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

/// How a shared TX-worker step ends the current request (PAN-322). A helper
/// cannot jump to a loop label, so each call site turns this into its own
/// jump: `break 'worker` / `continue 'worker` in the single-frame arm,
/// `break` / `continue` in the bundle arm.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TxExit {
    /// Shutdown: leave the worker loop.
    StopWorker,
    /// The request is settled: take the next one.
    NextRequest,
}

/// Which worker arm a shared step runs for (PAN-322). The arms differ on
/// purpose (log text, whether the TX strip is cleared), and the helpers
/// pick by shape instead of folding one copy into the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TxShape {
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
pub(super) async fn on_ptt_wait(
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

/// PAN-114 (D6): slack over `ptt_lead_ms` allowed between PTT-on (Step 5)
/// and the slot boundary (Step 6) before the hold is treated as a clock step.
pub(super) const PTT_HOLD_SLACK_MS: u64 = 1000;

/// `true` when Step 6 would hold PTT keyed (for `hold`, the Step 6
/// `to_slot`) longer than `ptt_lead_ms` + [`PTT_HOLD_SLACK_MS`]. Nothing
/// between Step 4 and Step 6 reassigns the schedule, so after a normal
/// Step 4 the hold is <= `ptt_lead_ms`; anything past the bound means the
/// wall clock stepped back in between.
pub(super) fn ptt_hold_exceeds_bound(hold: Duration, ptt_lead_ms: u64) -> bool {
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
pub(super) async fn drop_frames_for_clock_jump(
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
    pub(super) std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ComponentMessage>>>,
);

/// Above this many held messages the worker logs a warning. Nothing is
/// dropped; real traffic holds a handful.
pub(super) const TX_REQUEUE_HIGH_WATER: usize = 1024;

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
pub(super) fn self_message(message_type: MessageType) -> ComponentMessage {
    ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Ft8Transmitter,
        message_type,
        Instant::now(),
    )
}

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
pub(super) async fn interruptible_sleep_or_supersede(
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
pub(super) async fn interruptible_sleep_or_supersede_with_clock(
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
                    let key = it.qso_id.as_deref().map(super::super::active_tx_qso_key);
                    !in_flight_items.iter().any(|fi| {
                        fi.qso_id.as_deref().map(super::super::active_tx_qso_key) == key
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
pub(super) enum SleepBreak {
    Exit(TxExit),
    Superseded(Box<MessageType>),
}

/// What [`on_sleep_outcome`] returns.
pub(super) type SleepFlow = std::ops::ControlFlow<SleepBreak>;

/// Which of the two pre-air waits a [`SleepOutcome`] came from (PAN-322).
#[derive(Clone, Copy, Debug)]
pub(super) enum SleepPhase {
    /// Step 6: PTT is keyed, the slot has not started, no audio was routed.
    PreSlot,
    /// Step 8: audio is playing; `playback_end` bounds an unconfirmed disarm
    /// flush (PAN-135).
    Playback { playback_end: tokio::time::Instant },
}

/// The frames a sleep step was guarding (PAN-322). The single arm's frame is
/// borrowed field by field, so the sleep call still builds its own
/// in-flight item exactly as before.
pub(super) enum SleepInFlight<'a> {
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
pub(super) struct AbortCtx<'a> {
    pub(super) message_bus: &'a MessageBus,
    pub(super) audit_log: &'a pancetta_agent::audit::AuditLog,
    pub(super) display_feed_enabled: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) remote_tx_arm: &'a std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    pub(super) remote_client_key_id: Option<&'a str>,
    pub(super) ptt_guard: &'a mut PttGuard,
    pub(super) pivoted_once: &'a mut std::collections::HashMap<String, (String, f64)>,
    pub(super) pivoted_from: &'a mut PivotReplaced,
    pub(super) shutdown: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) sample_rate: u32,
}

/// Why a sleep step was cut short, for [`sleep_abort_log_line`].
#[derive(Clone, Copy, Debug)]
pub(super) enum SleepAbort {
    Shutdown,
    Operator,
    Disarm,
}

/// The log line for an aborted Step 6/8 sleep (PAN-322). These strings are
/// what operators and log scrapers have always seen; keep them byte-identical.
pub(super) fn sleep_abort_log_line(
    shape: TxShape,
    phase: SleepPhase,
    abort: SleepAbort,
) -> &'static str {
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
pub(super) fn remote_arm_gate<'a>(
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
pub(super) async fn on_sleep_outcome(
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
pub(super) fn encode_for_protocol(
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
pub(super) fn modulate_for_protocol(
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
pub(super) fn modulate_for_protocol_params(
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
pub(super) fn encode_and_modulate(
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

/// Slot and PTT timing for one TX request (PAN-322). Built at Step 2 of each
/// arm, where `slot_ns` is loaded, so a single-frame re-key pass rebuilds it.
#[derive(Clone)]
pub(super) struct SlotTimingContext {
    pub(super) utc_clock: pancetta_core::time::UtcClock,
    pub(super) active_protocol: pancetta_ft8::Protocol,
    /// Raw `station.tx_late_max_ms`; [`Self::late_cap_ms`] scales it per mode.
    pub(super) tx_late_max_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(super) tx_self_parity: pancetta_config::station::TxSelfParity,
    pub(super) ptt_lead_ms: u64,
    pub(super) sample_rate: u32,
    pub(super) slot_ns: i64,
    /// Frozen at dequeue, before the coalesce wait.
    pub(super) request_received_at: chrono::DateTime<chrono::Utc>,
}

impl super::super::ApplicationCoordinator {
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
    pub(crate) async fn start_transmitter_component_with_clock(
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
            #[cfg(any(test, feature = "test-harness"))]
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
            let tx_drive = self.tx_drive.clone();
            let tx_drive_session_record = self.session_record.clone();
            let tx_drive_session_path = self.session_state_path.clone();
            let tx_drive_rig_generation = self.hamlib_generation.clone();

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
                        super::super::protocol_from_mode(pancetta_config::OperatingMode::from_u8(
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
                                && super::super::autonomous::is_marked_autonomous_self_cq_dispatch(
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
                                    | MessageType::DriveCalibrationRequest { .. }
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

                                message.message_type = coalesce_backlog_into_with_fox_spacing(
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
                                    // PAN-386: Fox-pair spacing only in FT8.
                                    active_protocol == pancetta_ft8::Protocol::Ft8,
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
                                    if super::super::is_pivot_duplicate(
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
                                            let key = super::super::active_tx_qso_key(id);
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
                                        super::super::remote_gateway::relay_to_gateway(
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
                                                let key = super::super::active_tx_qso_key(id);
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
                                                super::super::tx_pivot_target(
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
                                                        let key =
                                                            super::super::active_tx_qso_key(id);
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
                                        #[cfg(any(test, feature = "test-harness"))]
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
                                        // PAN-439: select drive and prepare the meter attempt before the
                                        // final gates, so the per-band lock can never age their timing
                                        // checks. The band read is under `ptt_sync_gate`, which spot-call
                                        // QSY and band hops also hold.
                                        let drive_snapshot = tx_drive.snapshot_for_band(
                                            pancetta_core::Band::from_frequency(tx_log_dial_hz(
                                                &operating_frequency_hz,
                                                &split_tx_frequency_hz,
                                            )),
                                        );
                                        let drive_context = super::super::tx_drive::DriveContext {
                                            rig_generation: tx_drive_rig_generation
                                                .load(Ordering::Acquire),
                                            ft_mode: active_protocol_mode_atomic
                                                .load(Ordering::Acquire),
                                            stream_count: 1,
                                        };
                                        let drive_attempt = super::super::tx_meter::TxDriveAttempt::new(
                                            drive_snapshot.mode
                                                == pancetta_core::tx_drive_mode::TxDriveMode::Auto,
                                            drive_snapshot.band,
                                        );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                        let outcome = send_ptt_command_with_attempt(
                                            &message_bus,
                                            ComponentId::Ft8Transmitter,
                                            true,
                                            Some(drive_attempt.clone()),
                                            || {
                                                tx_send_authorized(
                                                    origin,
                                                    &remote_tx_arm,
                                                    remote_client_key_id.as_deref(),
                                                )
                                            },
                                        );
                                        drop(live_timing_guard);
                                        let drive_at_key = drive_snapshot.db;
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
                                                    super::super::now_epoch_ms(),
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
                                                    super::super::now_epoch_ms(),
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
                                                        (super::super::active_tx_qso_key(id), r)
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
                                                            encode_and_modulate_multi_tx_with_fox(
                                                                &mut encoder,
                                                                active_protocol,
                                                                &tx_params,
                                                                &items,
                                                                &fox_stream_predicate(
                                                                    &fox_admitted_qsos,
                                                                ),
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
                                        super::super::tx_drive::apply(
                                            &mut audio_out,
                                            drive_at_key,
                                            tx_drive.ceiling_db(),
                                            true,
                                        );
                                        let (playback_ack, mut playback_waiter) =
                                            PlaybackStartAck::new();
                                        let playback_ack = playback_ack
                                            .with_tx_drive_attempt(drive_attempt.clone());
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
                                                            encode_and_modulate_multi_tx_with_fox(
                                                                &mut encoder,
                                                                active_protocol,
                                                                &tx_params,
                                                                &items,
                                                                &fox_stream_predicate(
                                                                    &fox_admitted_qsos,
                                                                ),
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

                                        // PAN-439: playback has ended, so close burst sampling now; a meter
                                        // poll during the unkey tail belongs to no waveform.
                                        drive_attempt.close_sampling();
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
                                        let release = ptt_guard.release("Step 9 unkey").await;
                                        if success && release == PttRelease::Delivered {
                                            // An unconfirmed, unplayed or watchdog-invalidated attempt has no
                                            // summary and must not read as a burst without ALC.
                                            if let Some(samples) = drive_attempt.summary() {
                                                finish_auto_drive_burst(
                                                    &message_bus,
                                                    &tx_drive,
                                                    drive_snapshot,
                                                    &samples,
                                                    drive_context,
                                                    &tx_drive_session_record,
                                                    &tx_drive_session_path,
                                                    &operating_frequency_hz,
                                                    &split_tx_frequency_hz,
                                                    &tx_drive_rig_generation,
                                                    &active_protocol_mode_atomic,
                                                )
                                                .await;
                                            }
                                        }

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
                                            if super::super::is_pivot_duplicate(
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
                                                        let key =
                                                            super::super::active_tx_qso_key(id);
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
                                        super::super::remote_gateway::relay_to_gateway(
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
                                    let outcome = encode_and_modulate_multi_tx_with_fox(
                                        &mut encoder,
                                        active_protocol,
                                        &tx_params,
                                        &items,
                                        &fox_stream_predicate(&fox_admitted_qsos),
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

                                                let rebuild = encode_and_modulate_multi_tx_with_fox(
                                                    &mut encoder,
                                                    active_protocol,
                                                    &tx_params,
                                                    &live_items,
                                                    &fox_stream_predicate(&fox_admitted_qsos),
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
                                        super::super::LatestTxIntent,
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
                                                    super::super::active_tx_qso_key(id),
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
                                        super::super::pivot_bundle_items(
                                            live_items_pre,
                                            &latest_snapshot,
                                        );
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
                                        super::super::bundle_identity_conflict_mask(
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
                                                held_keys.push(super::super::active_tx_qso_key(id));
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
                                        let refolded = super::super::fold_bundle_identity(
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

                                            let rebuild = encode_and_modulate_multi_tx_with_fox(
                                                &mut encoder,
                                                active_protocol,
                                                &tx_params,
                                                &live_items,
                                                &fox_stream_predicate(&fox_admitted_qsos),
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
                                    #[cfg(any(test, feature = "test-harness"))]
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
                                    // PAN-439: select drive and prepare the meter attempt before the
                                    // final gates, so the per-band lock can never age their timing
                                    // checks. The band read is under `ptt_sync_gate`, which spot-call
                                    // QSY and band hops also hold.
                                    let drive_snapshot = tx_drive.snapshot_for_band(
                                        pancetta_core::Band::from_frequency(tx_log_dial_hz(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                        )),
                                    );
                                    let drive_attempt = super::super::tx_meter::TxDriveAttempt::new(
                                        drive_snapshot.mode
                                            == pancetta_core::tx_drive_mode::TxDriveMode::Auto,
                                        drive_snapshot.band,
                                    );
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                    let drive_context = super::super::tx_drive::DriveContext {
                                        rig_generation: tx_drive_rig_generation
                                            .load(Ordering::Acquire),
                                        ft_mode: active_protocol_mode_atomic
                                            .load(Ordering::Acquire),
                                        stream_count: items.len(),
                                    };
                                    let outcome = send_ptt_command_with_attempt(
                                        &message_bus,
                                        ComponentId::Ft8Transmitter,
                                        true,
                                        Some(drive_attempt.clone()),
                                        || {
                                            tx_send_authorized(
                                                origin,
                                                &remote_tx_arm,
                                                remote_client_key_id.as_deref(),
                                            )
                                        },
                                    );
                                    drop(live_timing_guard);
                                    let drive_at_key = drive_snapshot.db;
                                    match outcome {
                                        SendIfOutcome::Enqueued => {
                                            info!(
                                                target: "pancetta::tx.ptt",
                                                "PTT ON (scheduled multi-TX) sent to rig"
                                            );
                                        }
                                        SendIfOutcome::Denied => {
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                                    fox_admitted_qsos: &fox_admitted_qsos,
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
                                    super::super::tx_drive::apply(
                                        &mut audio_out,
                                        drive_at_key,
                                        tx_drive.ceiling_db(),
                                        true,
                                    );
                                    let (playback_ack, mut playback_waiter) =
                                        PlaybackStartAck::new();
                                    let playback_ack =
                                        playback_ack.with_tx_drive_attempt(drive_attempt.clone());
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
                                                    fox_admitted_qsos: &fox_admitted_qsos,
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

                                    // PAN-439: playback has ended, so close burst sampling now; a meter
                                    // poll during the unkey tail belongs to no waveform.
                                    drive_attempt.close_sampling();
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
                                    let release = ptt_guard.release("Multi-TX Step 9 unkey").await;
                                    if success && release == PttRelease::Delivered {
                                        // An unconfirmed, unplayed or watchdog-invalidated attempt has no
                                        // summary and must not read as a burst without ALC.
                                        if let Some(samples) = drive_attempt.summary() {
                                            finish_auto_drive_burst(
                                                &message_bus,
                                                &tx_drive,
                                                drive_snapshot,
                                                &samples,
                                                drive_context,
                                                &tx_drive_session_record,
                                                &tx_drive_session_path,
                                                &operating_frequency_hz,
                                                &split_tx_frequency_hz,
                                                &tx_drive_rig_generation,
                                                &active_protocol_mode_atomic,
                                            )
                                            .await;
                                        }
                                    }

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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
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
                                    let drive_at_key = tx_drive
                                        .snapshot_for_band(pancetta_core::Band::from_frequency(
                                            tx_log_dial_hz(
                                                &operating_frequency_hz,
                                                &split_tx_frequency_hz,
                                            ),
                                        ))
                                        .db;
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
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
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
                                    let mut tone_samples = tone_samples;
                                    super::super::tx_drive::apply(
                                        &mut tone_samples,
                                        drive_at_key,
                                        tx_drive.ceiling_db(),
                                        false,
                                    );
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

                                MessageType::DriveCalibrationRequest { tone_offset_hz } => {
                                    let duration_secs = 12;
                                    info!(
                                        "Drive calibration: {}s tone at {} Hz",
                                        duration_secs, tone_offset_hz
                                    );
                                    TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);
                                    // PAN-273: wait out an outstanding drop-fallback
                                    // PTT-off before the gates below, not after them.
                                    pending_ptt_off.settle().await;

                                    // --- TX-policy hard mute ---
                                    // A calibration carrier is a transmission. If the
                                    // global policy is Disabled (RX-only), never
                                    // key PTT / emit the tone. This is the
                                    // catch-all gate matching the
                                    // TransmitRequest / MultiTransmitRequest
                                    // arms — defends against any Drive calibrationRequest
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
                                            "TX blocked ({}): calibration ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): calibration ({duration_secs}s @ {tone_offset_hz} Hz)"
                                            ),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    }

                                    // PAN-118: a calibration carrier is a transmission
                                    // too — refuse it on the effective dial +
                                    // tone offset before PTT, mirroring the
                                    // band-plan sibling gate at the other
                                    // sites. Same unwind as the hard-mute
                                    // branch above (no TransmitComplete exists
                                    // for a calibration).
                                    if let Some(reason) = tx_rf_for_gate(
                                        &operating_frequency_hz,
                                        &split_tx_frequency_hz,
                                        tone_offset_hz,
                                    )
                                    .and_then(|rf| tx_band_plan_mute_reason(rf, 0.0, &band_plan))
                                    {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked ({}): calibration ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked ({reason}): calibration ({duration_secs}s @ {tone_offset_hz} Hz)"
                                            ),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    }

                                    if !message_bus.has_rig_route()
                                        || !message_bus
                                            .tx_meter_capability
                                            .lock()
                                            .unwrap()
                                            .is_some_and(|c| c.alc)
                                    {
                                        emit_diagnostic(&message_bus, "tx.drive", pancetta_core::DiagnosticLevel::Warn,
                                            "Can't calibrate — this rig does not report ALC over CAT".to_string(), None).await;
                                        continue;
                                    }
                                    if !tx_drive.auto_enabled() {
                                        continue;
                                    }
                                    let Some(_admission_band) =
                                        pancetta_core::Band::from_frequency(tx_log_dial_hz(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                        ))
                                    else {
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.drive",
                                            pancetta_core::DiagnosticLevel::Warn,
                                            "Can't calibrate — unknown TX band".to_string(),
                                            None,
                                        )
                                        .await;
                                        continue;
                                    };
                                    // Generate a continuous sine wave
                                    // (single tone, zero-bandwidth on air).
                                    // Amplitude 0.5 — operator manages rig
                                    // power. WSJT-X uses peak amplitude;
                                    // we run gentler so a forgotten rig
                                    // power setting is less likely to
                                    // overdrive.

                                    let omega = 2.0 * std::f64::consts::PI * tone_offset_hz
                                        / sample_rate as f64;

                                    // Engage PTT immediately. No slot
                                    // scheduling: calibration happens NOW.
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
                                            .store(super::super::now_epoch_ms(), Ordering::Release);
                                        ptt_active.store(false, Ordering::Release);
                                        ptt_guard.disarm();
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX blocked at final pre-PTT check ({}): calibration ({}s @ {} Hz)",
                                            reason, duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX blocked at final pre-PTT check ({reason}): calibration ({duration_secs}s @ {tone_offset_hz} Hz)"
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
                                        // `authorize` (always true for a calibration).
                                        let dropped = ptt_on == SendIfOutcome::Dropped;
                                        // PAN-278: with a Hamlib route the key-down was
                                        // dropped (full or disconnected channel) or
                                        // refused, so the rig was never keyed. Abandon
                                        // the calibration rather than play a tone with no
                                        // carrier. Unwind the in-process flag like the
                                        // band-plan refusal above: nothing entered
                                        // Hamlib's queue, so there is no PTT-off to send.
                                        // Without a route (`--replay`) the drop is by design
                                        // and the tone still plays.
                                        if message_bus.has_rig_route() {
                                            last_ptt_off_ms.store(
                                                super::super::now_epoch_ms(),
                                                Ordering::Release,
                                            );
                                            ptt_active.store(false, Ordering::Release);
                                            ptt_guard.disarm();
                                            let why = if dropped { "dropped" } else { "refused" };
                                            warn!(
                                                target: "pancetta::tx.ptt",
                                                "Drive calibration: PTT-on {} (rig not keyed); calibration ({}s @ {} Hz) abandoned",
                                                why, duration_secs, tone_offset_hz
                                            );
                                            emit_diagnostic(
                                                &message_bus,
                                                "tx.ptt",
                                                pancetta_core::DiagnosticLevel::Warn,
                                                format!(
                                                    "Drive calibration: PTT-on {why} (rig not keyed); calibration abandoned"
                                                ),
                                                None,
                                            )
                                            .await;
                                            continue;
                                        }
                                        if dropped {
                                            warn!(
                                                "Drive calibration: PTT ON not sent: no Hamlib route registered (replay mode)"
                                            );
                                        } else {
                                            warn!(
                                                "Drive calibration: PTT ON refused (rig not keyed)"
                                            );
                                        }
                                    }

                                    let Some(band) =
                                        pancetta_core::Band::from_frequency(tx_log_dial_hz(
                                            &operating_frequency_hz,
                                            &split_tx_frequency_hz,
                                        ))
                                    else {
                                        ptt_guard
                                            .release("Calibration band unknown at key time")
                                            .await;
                                        continue;
                                    };
                                    let drive_at_key = tx_drive.snapshot_for_band(Some(band)).db;
                                    let sink_guard =
                                        drive_calibration::CalibrationSinkGuard::install(
                                            message_bus.active_drive_calibration.clone(),
                                        );
                                    let (floor, ceiling, onset) = tx_drive.calibration_limits();
                                    let mut stepper = drive_calibration::CalibrationStepper::new(
                                        floor, ceiling, onset,
                                    );
                                    let started = Instant::now();
                                    let mut sample_index = 0usize;
                                    let mut saved = None;
                                    let mut missing_alc = false;
                                    let mut audio_dropped = false;
                                    let mut midrun_mute = None;
                                    loop {
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
                                            midrun_mute = Some(reason.to_string());
                                            break;
                                        }
                                        if started.elapsed()
                                            + Duration::from_millis(
                                                drive_calibration::CAL_SEGMENT_MS,
                                            )
                                            >= Duration::from_millis(
                                                drive_calibration::CAL_MAX_KEYDOWN_MS,
                                            )
                                        {
                                            break;
                                        }
                                        let level = stepper.level();
                                        let segment_start = Instant::now();
                                        // Queue the cushion once. Appending it on every step
                                        // would accumulate audio latency and misattribute ALC.
                                        let queued_ms = drive_calibration::CAL_SEGMENT_MS
                                            + if sample_index == 0 {
                                                drive_calibration::CAL_CUSHION_MS
                                            } else {
                                                0
                                            };
                                        let count =
                                            (sample_rate as usize) * queued_ms as usize / 1000;
                                        let mut samples: Vec<f32> = (0..count)
                                            .map(|i| {
                                                (((sample_index + i) as f64) * omega).sin() as f32
                                                    * 0.5
                                            })
                                            .collect();
                                        sample_index += count;
                                        super::super::tx_drive::apply(
                                            &mut samples,
                                            level,
                                            ceiling,
                                            false,
                                        );
                                        let sent = message_bus
                                            .send_message_checked(ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Audio,
                                                MessageType::AudioOutput {
                                                    samples,
                                                    sample_rate,
                                                    flush_first: false,
                                                    playback_started: None,
                                                },
                                                Instant::now(),
                                            ))
                                            .await;
                                        if !matches!(sent, Ok(true)) {
                                            audio_dropped = true;
                                            break;
                                        }
                                        if interruptible_sleep(
                                            Duration::from_millis(
                                                drive_calibration::CAL_SEGMENT_MS,
                                            ),
                                            &shutdown,
                                            &abort_current_tx,
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                        let readings: Vec<Option<f32>> = {
                                            let mut ticks = sink_guard.sink().ticks.lock().unwrap();
                                            let values = ticks
                                                .iter()
                                                .filter(|(t, _)| {
                                                    *t >= segment_start
                                                        + Duration::from_millis(
                                                            drive_calibration::CAL_CUSHION_MS,
                                                        )
                                                })
                                                .map(|(_, alc)| *alc)
                                                .collect();
                                            ticks.clear();
                                            values
                                        };
                                        match stepper.observe(&readings) {
                                            drive_calibration::Decision::StepUp => {}
                                            drive_calibration::Decision::Knee(db) => {
                                                saved = Some((db, false));
                                                break;
                                            }
                                            drive_calibration::Decision::Ceiling(db) => {
                                                saved = Some((db, true));
                                                break;
                                            }
                                            drive_calibration::Decision::Abort => {
                                                missing_alc = true;
                                                break;
                                            }
                                        }
                                    }
                                    drop(sink_guard);
                                    // Clear queued carrier before releasing PTT.
                                    let _ = message_bus
                                        .send_message(ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Audio,
                                            MessageType::AudioOutput {
                                                samples: Vec::new(),
                                                sample_rate,
                                                flush_first: true,
                                                playback_started: None,
                                            },
                                            Instant::now(),
                                        ))
                                        .await;
                                    ptt_guard.release("Drive calibration unkey").await;
                                    if shutdown.load(Ordering::Acquire) {
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.drive",
                                            pancetta_core::DiagnosticLevel::Warn,
                                            "Drive calibration stopped — shutdown; nothing saved"
                                                .to_string(),
                                            None,
                                        )
                                        .await;
                                        break;
                                    }
                                    if abort_current_tx.load(Ordering::Acquire) {
                                        emit_diagnostic(&message_bus, "tx.drive", pancetta_core::DiagnosticLevel::Warn,
                                            "Drive calibration stopped — operator halt; nothing saved".to_string(), None).await;
                                        continue;
                                    }
                                    if let Some((db, no_onset)) = saved {
                                        let selected = tx_drive.calibrate_band(band, db);
                                        let _ = message_bus.send_message(ComponentMessage::new(
                                            ComponentId::Ft8Transmitter, ComponentId::Tui,
                                            MessageType::RigControl(crate::message_bus::RigControlMessage::TxDriveState {
                                                band: Some(band), db: selected.db,
                                                ceiling_db: tx_drive.ceiling_db(), mode: selected.mode,
                                                available: selected.available,
                                            }), Instant::now(),
                                        )).await;
                                        let detail = if no_onset {
                                            format!("Drive calibrated: {} → {} dB — no ALC onset below the ceiling", drive_calibration::display_band(band), drive_calibration::display_db(db))
                                        } else {
                                            format!(
                                                "Drive calibrated: {} → {} dB (was {} dB)",
                                                drive_calibration::display_band(band),
                                                drive_calibration::display_db(db),
                                                drive_calibration::display_db(drive_at_key)
                                            )
                                        };
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.drive",
                                            pancetta_core::DiagnosticLevel::Info,
                                            detail,
                                            None,
                                        )
                                        .await;
                                        if tx_drive.needs_persist() {
                                            let record = tx_drive_session_record.clone();
                                            let path = tx_drive_session_path.clone();
                                            let drive = tx_drive.clone();
                                            let bus = message_bus.clone();
                                            tokio::spawn(async move {
                                                let result = tokio::task::spawn_blocking(move ||
                                                    super::super::session_state::persist_tx_drive_current(&record, &path, &drive)
                                                ).await;
                                                if let Ok(Err(error)) = result {
                                                    emit_diagnostic(&bus, "tx.drive", pancetta_core::DiagnosticLevel::Warn,
                                                        format!("TX drive changed for this session only. Could not save TX drive. {error}"), None).await;
                                                }
                                            });
                                        }
                                    } else {
                                        let detail = if let Some(reason) = midrun_mute.as_ref() {
                                            format!("Drive calibration stopped — {reason}; nothing saved")
                                        } else if audio_dropped {
                                            "Drive calibration stopped — audio output dropped; nothing saved".to_string()
                                        } else if missing_alc {
                                            "Drive calibration stopped — ALC readings stopped mid-run; nothing saved".to_string()
                                        } else {
                                            "Drive calibration stopped — time limit reached; nothing saved".to_string()
                                        };
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.drive",
                                            pancetta_core::DiagnosticLevel::Warn,
                                            detail,
                                            None,
                                        )
                                        .await;
                                    }
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
