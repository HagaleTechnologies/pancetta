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

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

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
/// it's used as the adaptive window's extension cap. Covers Step 1's
/// encode/modulate time and other fixed per-message overhead between
/// `request_received_at` and the actual coalesce point, none of which is
/// otherwise accounted for in the headroom math — without this margin, a
/// request that arrives with headroom just barely covering the extension
/// alone could still get pushed past the `tx_late_max_ms` cliff by that
/// extra overhead.
const COALESCE_CAP_SAFETY_MARGIN_MS: u64 = 500;

/// Protocol-scaled `COALESCE_MAX_EXTENSION_MS` — see that constant's doc.
fn coalesce_max_extension_ms(protocol: pancetta_ft8::Protocol) -> u64 {
    const FT8_CYCLE_SECS: f64 = 15.0;
    let cycle = pancetta_ft8::ProtocolParams::from_protocol(protocol).cycle_duration;
    ((COALESCE_MAX_EXTENSION_MS as f64) * (cycle / FT8_CYCLE_SECS)).round() as u64
}

/// Remaining `tx_late_max_ms` headroom, in ms, available to extend the
/// Symptom-C adaptive coalesce window for the given head request — bounds
/// the window so it can never push a request past the late-skip cliff.
/// Returns the full (protocol-scaled) `COALESCE_MAX_EXTENSION_MS` when the
/// head has already resolved to a DEFERRED (next-slot) target, since
/// there's no current-slot cliff to protect in that case, and `0` if `head`
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
    let probe = schedule_tx(
        request_received_at,
        required_parity,
        tx_late_max_ms_eff,
        sample_rate,
        slot_ns,
    );
    let protocol_ceiling = coalesce_max_extension_ms(protocol);
    if probe.deferred {
        return protocol_ceiling;
    }
    let elapsed_in_slot_ms = (request_received_at - probe.target_slot)
        .num_milliseconds()
        .max(0) as u64;
    let headroom = tx_late_max_ms_eff
        .saturating_sub(elapsed_in_slot_ms)
        .saturating_sub(COALESCE_CAP_SAFETY_MARGIN_MS);
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
/// message for the SAME QSO already being transmitted. It is dropped when
/// it is an exact duplicate (identical text), a recognized pivot-tombstone
/// (`is_pivot_duplicate`), or — PAN-73 — belongs to a DIFFERENT QSO than the
/// one in flight.
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
/// live 2026-09-05 (VP2MAA/JF1RDH/JA5GYU). Real cross-QSO supersede/bundling
/// during an in-flight transmission is Phase 2 (priority-tier gating), not
/// built by this plan — until then, a different QSO's message during
/// another QSO's TX just waits for that QSO's own next auto-sequence cycle,
/// same as it would have if it had lost the race to be dequeued first.
pub fn classify_incoming_during_tx(
    candidate: &MessageType,
    in_flight_qso_id: Option<&str>,
    in_flight_text: &str,
    pivoted_once: &std::collections::HashMap<String, String>,
) -> IncomingDuringTx {
    match candidate {
        MessageType::TransmitRequest {
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            ..
        } => {
            if super::is_pivot_duplicate(qso_id.as_deref(), message_text, pivoted_once) {
                return IncomingDuringTx::Drop;
            }
            // M1: normalize BOTH sides through `active_tx_qso_key` before
            // comparing, matching every sibling qso_id comparison
            // (`tx_pivot_target`, `is_pivot_duplicate`, `tx_qso_is_live`). QSO
            // ids are already deterministic-lowercase Uuids today so this
            // doesn't change behavior, but it removes a raw-`Option<&str>`
            // comparison that would silently diverge if casing ever drifted.
            let same_target = qso_id.as_deref().map(super::active_tx_qso_key)
                == in_flight_qso_id.map(super::active_tx_qso_key);
            if same_target && message_text == in_flight_text {
                return IncomingDuringTx::Drop;
            }
            // PAN-73: a candidate with its own qso_id that differs from the
            // in-flight QSO is another QSO's routine auto-sequence message,
            // not an operator action — never let it tear down an unrelated
            // in-flight transmission. `qso_id == None` (manual/free-text/
            // tune/test-TX) is the actual operator-triggered case this
            // classifier exists for, and always supersedes as before.
            if qso_id.is_some() && !same_target {
                return IncomingDuringTx::Drop;
            }
            IncomingDuringTx::Supersede {
                text: message_text.clone(),
                frequency_offset: *frequency_offset,
                qso_id: qso_id.clone(),
                tx_parity: *tx_parity,
            }
        }
        // A bundle is always new information (it carries its own set of
        // items, not comparable 1:1 to a single in-flight text) — always
        // supersede. Task 7 (multi-TX bundle-add) refines what happens next;
        // this classifier only decides Drop vs Supersede.
        MessageType::MultiTransmitRequest { .. } => IncomingDuringTx::Supersede {
            text: String::new(),
            frequency_offset: 0.0,
            qso_id: None,
            tx_parity: None,
        },
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
    /// A qualifying request arrived; abort_current_tx was set by this
    /// function itself. Caller should attempt to re-key with the contained
    /// message.
    Superseded(MessageType),
}

/// Like `interruptible_sleep`, but also polls `tx_rx` for a qualifying
/// incoming request (Task 4's `classify_incoming_during_tx`) on every 50ms
/// tick. A qualifying request sets `abort` itself (mirroring the operator
/// F8 path) and is returned via `SleepOutcome::Superseded` for the caller to
/// re-key. A non-qualifying `TransmitRequest`/`MultiTransmitRequest` (exact
/// duplicate or pivot tombstone) is silently consumed — same as it would have
/// been had it reached the main dequeue loop naturally — and the sleep keeps
/// waiting.
///
/// I1: a message that is NEITHER a `TransmitRequest` NOR a
/// `MultiTransmitRequest` (e.g. an operator `TuneRequest`) is NOT a supersede
/// candidate and must NOT be swallowed — before this feature it would sit in
/// the channel until the worker's next dequeue and be handled by its own
/// top-level arm. `crossbeam_channel::Receiver` has no peek, so we must consume
/// such a message to inspect its type; it is then SIPHONED aside and
/// re-enqueued (in arrival order) to the worker's own channel when the sleep
/// ends, so the main loop processes it after the current TX arm returns.
///
/// See docs/superpowers/specs/2026-07-22-mid-tx-abort-restart-design.md,
/// "Abort + re-key mechanics."
#[allow(clippy::too_many_arguments)]
async fn interruptible_sleep_or_supersede(
    total: Duration,
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    abort: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx_rx: &crossbeam_channel::Receiver<ComponentMessage>,
    message_bus: &MessageBus,
    in_flight_qso_id: Option<&str>,
    in_flight_text: &str,
    pivoted_once: &std::collections::HashMap<String, String>,
) -> SleepOutcome {
    use std::sync::atomic::Ordering;

    // Non-supersede-candidate messages pulled off the channel during a poll
    // (I1). Held here rather than bounced straight back so a single try_recv
    // per tick can't keep re-reading the same one; flushed to the worker's own
    // channel, in order, once the sleep concludes.
    let mut siphoned: Vec<ComponentMessage> = Vec::new();

    let check_once = |tx_rx: &crossbeam_channel::Receiver<ComponentMessage>,
                      siphoned: &mut Vec<ComponentMessage>|
     -> Option<SleepOutcome> {
        if shutdown.load(Ordering::Acquire) {
            return Some(SleepOutcome::AbortedByShutdown);
        }
        if abort.load(Ordering::Acquire) {
            return Some(SleepOutcome::AbortedByOperator);
        }
        if let Ok(message) = tx_rx.try_recv() {
            let is_supersede_candidate = matches!(
                message.message_type,
                MessageType::TransmitRequest { .. } | MessageType::MultiTransmitRequest { .. }
            );
            if is_supersede_candidate {
                match classify_incoming_during_tx(
                    &message.message_type,
                    in_flight_qso_id,
                    in_flight_text,
                    pivoted_once,
                ) {
                    IncomingDuringTx::Drop => {}
                    IncomingDuringTx::Supersede { .. } => {
                        abort.store(true, Ordering::Release);
                        return Some(SleepOutcome::Superseded(message.message_type));
                    }
                }
            } else {
                // Not a supersede candidate (e.g. TuneRequest). Siphon it so
                // it is re-enqueued after the sleep instead of being lost.
                siphoned.push(message);
            }
        }
        None
    };

    let outcome = 'sleep: {
        if let Some(outcome) = check_once(tx_rx, &mut siphoned) {
            break 'sleep outcome;
        }

        let chunk = Duration::from_millis(50);
        let deadline = tokio::time::Instant::now() + total;
        while tokio::time::Instant::now() < deadline {
            if let Some(outcome) = check_once(tx_rx, &mut siphoned) {
                break 'sleep outcome;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            sleep(remaining.min(chunk)).await;
        }
        SleepOutcome::Completed
    };

    // Re-enqueue any siphoned non-candidate messages to the worker's own
    // channel, in arrival order, so the main loop dequeues them normally on its
    // next cycle (pre-feature behavior). Done for EVERY outcome — including
    // Superseded — so nothing an operator sent is lost by the supersede path.
    for msg in siphoned {
        let reenqueue = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Ft8Transmitter,
            msg.message_type,
            Instant::now(),
        );
        if let Err(e) = message_bus.send_message(reenqueue).await {
            warn!(
                "supersede sleep: failed to re-enqueue non-candidate message: {}",
                e
            );
        }
    }

    outcome
}

#[cfg(test)]
mod interruptible_sleep_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[tokio::test]
    async fn returns_false_and_waits_full_duration_when_not_aborted() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_millis(100), &shutdown, &abort).await;
        assert!(!aborted);
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn returns_true_promptly_when_abort_flips_mid_sleep() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let abort_clone = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            abort_clone.store(true, Ordering::Release);
        });
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
        assert!(aborted);
        // Should wake within the ~50ms poll granularity of when the flag flipped
        // (flag flips at ~20ms), not wait out the full 5s sleep.
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn returns_true_promptly_when_shutdown_flips_mid_sleep() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shutdown_clone.store(true, Ordering::Release);
        });
        let start = tokio::time::Instant::now();
        let aborted = interruptible_sleep(Duration::from_secs(5), &shutdown, &abort).await;
        assert!(aborted);
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_completes_normally_with_no_incoming_message() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = crossbeam_channel::unbounded();
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let pivoted_once = std::collections::HashMap::new();
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(80),
            &shutdown,
            &abort,
            &rx,
            &bus,
            Some("qso-1"),
            "in flight text",
            &pivoted_once,
        )
        .await;
        assert!(matches!(outcome, super::SleepOutcome::Completed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_detects_a_qualifying_incoming_message() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let pivoted_once = std::collections::HashMap::new();

        let new_request = MessageType::TransmitRequest {
            message_text: "KA1ABC K5ARH RR73".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        tx.send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            new_request.clone(),
            std::time::Instant::now(),
        ))
        .unwrap();

        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(500),
            &shutdown,
            &abort,
            &rx,
            &bus,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        )
        .await;

        match outcome {
            super::SleepOutcome::Superseded(MessageType::TransmitRequest {
                message_text,
                qso_id,
                frequency_offset,
                ..
            }) => {
                assert_eq!(message_text, "KA1ABC K5ARH RR73");
                assert_eq!(qso_id.as_deref(), Some("qso-1"));
                assert_eq!(frequency_offset, 1500.0);
            }
            other => panic!("expected Superseded(TransmitRequest), got {:?}", other),
        }
        assert!(
            abort.load(Ordering::Acquire),
            "should set abort_current_tx itself"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_drops_non_qualifying_message_and_keeps_waiting() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let pivoted_once = std::collections::HashMap::new();

        // Identical content to what's in flight — should be Dropped, not treated
        // as a trigger, and the sleep should complete normally.
        let duplicate = MessageType::TransmitRequest {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        tx.send(crate::message_bus::ComponentMessage::new(
            crate::message_bus::ComponentId::Autonomous,
            crate::message_bus::ComponentId::Ft8Transmitter,
            duplicate,
            std::time::Instant::now(),
        ))
        .unwrap();

        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(80),
            &shutdown,
            &abort,
            &rx,
            &bus,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        )
        .await;

        assert!(matches!(outcome, super::SleepOutcome::Completed));
        assert!(!abort.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_still_honors_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(true));
        let abort = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = crossbeam_channel::unbounded();
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let pivoted_once = std::collections::HashMap::new();
        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_secs(60),
            &shutdown,
            &abort,
            &rx,
            &bus,
            Some("qso-1"),
            "in flight text",
            &pivoted_once,
        )
        .await;
        assert!(matches!(outcome, super::SleepOutcome::AbortedByShutdown));
    }

    /// I1 REGRESSION: a message that is NOT a supersede candidate (here a
    /// `TuneRequest` — a real top-level worker arm) arriving during a supersede
    /// sleep must NOT be silently swallowed. Before the fix it was fed to the
    /// classifier's catch-all `Drop` arm and consumed with no re-enqueue, so an
    /// operator's F4 single-tone tune issued while a TX was on the air simply
    /// vanished instead of being processed once TX finished. The sleep must
    /// complete normally (a TuneRequest is not a supersede trigger) and the
    /// message must be re-enqueued to the worker's own channel for the main
    /// loop's next dequeue.
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_sleep_reenqueues_non_candidate_tune_request() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        // The worker's own channel: the function polls its receiver AND
        // re-enqueues to it (routed by ComponentId::Ft8Transmitter).
        let bus = crate::message_bus::MessageBus::new(16).unwrap();
        let (tx_sender, tx_rx) = bus
            .create_channel(crate::message_bus::ComponentId::Ft8Transmitter)
            .await
            .unwrap();
        let pivoted_once = std::collections::HashMap::new();

        // Operator F4 tune request lands on the TX channel mid-transmission.
        let tune = MessageType::TuneRequest {
            duration_secs: 5,
            tone_offset_hz: 1500.0,
        };
        tx_sender
            .send(crate::message_bus::ComponentMessage::new(
                crate::message_bus::ComponentId::Tui,
                crate::message_bus::ComponentId::Ft8Transmitter,
                tune,
                std::time::Instant::now(),
            ))
            .unwrap();

        let outcome = super::interruptible_sleep_or_supersede(
            Duration::from_millis(120),
            &shutdown,
            &abort,
            &tx_rx,
            &bus,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        )
        .await;

        // A TuneRequest is not a supersede trigger: the sleep runs to completion
        // and never sets the abort flag.
        assert!(
            matches!(outcome, super::SleepOutcome::Completed),
            "a non-candidate TuneRequest must not supersede — got {outcome:?}"
        );
        assert!(
            !abort.load(Ordering::Acquire),
            "a TuneRequest must not set abort_current_tx"
        );

        // The TuneRequest was re-enqueued (siphoned during the sleep, flushed
        // back after) — NOT lost.
        let reenqueued = tx_rx
            .try_recv()
            .expect("the TuneRequest must be re-enqueued, not swallowed");
        assert!(
            matches!(
                reenqueued.message_type,
                MessageType::TuneRequest {
                    duration_secs: 5,
                    ..
                }
            ),
            "expected the re-enqueued TuneRequest, got {:?}",
            reenqueued.message_type
        );
        // Exactly one — no duplication.
        assert!(
            tx_rx.try_recv().is_err(),
            "the TuneRequest is re-enqueued exactly once"
        );
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
}

impl PttGuard {
    fn new(
        message_bus: MessageBus,
        ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
        last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        ptt_active.store(true, std::sync::atomic::Ordering::Release);
        last_ptt_on_ms.store(super::now_epoch_ms(), std::sync::atomic::Ordering::Release);
        Self {
            message_bus,
            armed: true,
            ptt_active,
        }
    }

    /// Disarm the guard after PTT-off has been sent normally.
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

/// Log one actually-keyed frame for Band Activity's own-TX history (#172).
///
/// `timestamp` must be captured AFTER Step 6's slot-boundary wait, not at
/// Step 5's PTT-key time — Step 6's `duration_until` collapses to a no-op
/// once `target_slot` is already past (the late-but-viable cursor-skip
/// case), so a timestamp taken there is `target_slot` on an on-time key and
/// the true late instant on a late-but-viable one. Stamping at Step 5
/// instead would show every late-started frame at its nominal slot
/// boundary even though the audio actually went out seconds later.
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

/// Push a richer TX-queue snapshot (NOW-SENDING + QUEUED) to the TUI.
/// Best-effort, observation-only: never touches PTT/audio/scheduling.
///
/// Does NOT emit `TxFrameLogged` — that fires separately, at Step 7, once
/// the actual audio-start instant is known (see [`log_tx_frame`]). This
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
        ..
    } = message_type
    {
        let complete_msg = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Autonomous,
            MessageType::TransmitComplete {
                success: false,
                message_text: message_text.clone(),
                duration_ms: 0,
                qso_id: qso_id.clone(),
            },
            Instant::now(),
        );
        if let Err(e) = message_bus.send_message(complete_msg).await {
            warn!("Failed to send TransmitComplete: {}", e);
        }
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
pub(crate) fn tx_hard_mute_reason(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
    restart_inhibit: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    hamlib_loop_ready: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &std::sync::Arc<
        std::sync::Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &std::sync::Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &std::sync::Arc<std::sync::atomic::AtomicU32>,
) -> Option<&'static str> {
    if restart_inhibit.load(Ordering::Acquire) != 0 {
        Some("rig control is restarting")
    } else if !hamlib_loop_ready.load(Ordering::Acquire) {
        Some("Hamlib command loop is not yet ready")
    } else if has_undelivered_pending_hamlib_state(hamlib_pending_frequency, hamlib_pending_split) {
        Some("pending rig frequency/split state has not been delivered yet")
    } else if hamlib_command_in_flight.load(Ordering::Acquire) > 0 {
        Some("a rig frequency/split command is still being applied")
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

#[cfg(test)]
mod tx_hard_mute_reason_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8};
    use std::sync::{Arc, Mutex};

    fn policy(p: pancetta_core::TxPolicy) -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(p.as_u8()))
    }

    /// An empty pending slot -- the common case (nothing carried over from
    /// a prior failed teardown replay).
    fn no_pending() -> Arc<Mutex<Option<ComponentMessage>>> {
        Arc::new(Mutex::new(None))
    }

    /// The frequency sibling of `no_pending()` -- PAN-35 keyed the
    /// frequency pending slot by VFO, so its empty state is an empty map
    /// rather than `None`.
    fn no_pending_frequency() -> Arc<Mutex<HashMap<pancetta_hamlib::Vfo, ComponentMessage>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn pending_split_msg() -> ComponentMessage {
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            std::time::Instant::now(),
        )
    }

    /// Not in flight -- the common case (no CAT call currently executing).
    fn not_in_flight() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    #[test]
    fn permits_tx_when_everything_is_ready() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        assert_eq!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &not_in_flight(),
            ),
            None
        );
    }

    #[test]
    fn restart_inhibit_still_mutes_independent_of_loop_readiness() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(1));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        assert!(tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
        )
        .is_some());
    }

    /// PAN-19 round-7 review (Codex P1): the actual bug this guards
    /// against. A `LoopReadyOutcome::TimedOut` startup still returns
    /// `Ok(())` from `start_hamlib_component` (must not bail -- the
    /// HIGH-fix invariant), which releases `TxInhibitGuard`
    /// (`tx_restart_inhibit` back to 0) in `health.rs`. Simulate exactly
    /// that end state -- `tx_restart_inhibit == 0` (already released) but
    /// `hamlib_command_loop_ready == false` (never confirmed, because
    /// `TimedOut` never sets it true) -- and confirm PTT is STILL refused
    /// at this gate, independent of the restart-supervision counter having
    /// already released.
    #[test]
    fn hamlib_loop_not_ready_mutes_tx_even_after_restart_inhibit_has_released() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        // The exact post-`TimedOut` state: the restart-supervision inhibit
        // has ALREADY been released (0), simulating `TxInhibitGuard`
        // having dropped normally once `start_hamlib_component` returned
        // `Ok(())` on a timeout.
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(false));

        let reason = tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
        );
        assert!(
            reason.is_some(),
            "TX must stay muted when the Hamlib command loop hasn't confirmed readiness, even \
             though the restart-supervision inhibit counter has already released"
        );
    }

    /// PAN-19 round-14 review (Codex P1): the actual bug this guards
    /// against. A pending `SetSplit` from a prior generation's failed
    /// teardown replay is requeued (round 10) into `hamlib_pending_split`
    /// after a momentarily-full channel, waiting for the polling task's
    /// next ~500ms retry -- but the message loop IS ready to consume
    /// commands during that gap (`hamlib_command_loop_ready == true`), and
    /// the restart-supervision inhibit has already released
    /// (`restart_inhibit == 0`). Without this check, PTT-on would slip
    /// through here and transmit with the rig still holding its stale,
    /// pre-crash split configuration -- an off-frequency transmission.
    #[test]
    fn undelivered_pending_split_mutes_tx_even_when_the_loop_is_ready() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        let pending_split = Arc::new(Mutex::new(Some(pending_split_msg())));

        let reason = tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &pending_split,
            &not_in_flight(),
        );
        assert!(
            reason.is_some(),
            "TX must stay muted while a pending SetSplit is still undelivered, even though the \
             Hamlib command loop has confirmed readiness and the restart inhibit has released"
        );

        // Once the pending item clears (delivered by the polling task's
        // retry, or applied and the slot drained), PTT-on must be
        // permitted again -- the fix must not become overly conservative.
        *pending_split.lock().unwrap() = None;
        assert_eq!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency(),
                &pending_split,
                &not_in_flight(),
            ),
            None,
            "PTT-on must be permitted again once the pending SetSplit has been delivered"
        );
    }

    /// Same as above, for the frequency slot -- both pending kinds must be
    /// covered, not just split.
    #[test]
    fn undelivered_pending_frequency_also_mutes_tx() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        let mut pending_frequency_map = HashMap::new();
        pending_frequency_map.insert(
            pancetta_hamlib::Vfo::A,
            ComponentMessage::new(
                ComponentId::Hamlib,
                ComponentId::Hamlib,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                    vfo: 0,
                    frequency: 14_074_000,
                }),
                std::time::Instant::now(),
            ),
        );
        let pending_frequency = Arc::new(Mutex::new(pending_frequency_map));

        assert!(tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &pending_frequency,
            &no_pending(),
            &not_in_flight(),
        )
        .is_some());
    }

    /// PAN-35 regression guard: a pending command for ONE VFO must still
    /// mute TX even though the OTHER VFO's slot is empty -- before this
    /// fix, both VFOs shared a single slot, so this distinction didn't
    /// exist; now that the slot is keyed by VFO, `has_undelivered_pending_
    /// hamlib_state` must check the map as a whole (any entry), not
    /// assume a specific VFO's entry.
    #[test]
    fn undelivered_pending_frequency_on_either_vfo_alone_still_mutes_tx() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));

        for vfo in [pancetta_hamlib::Vfo::A, pancetta_hamlib::Vfo::B] {
            let mut pending_frequency_map = HashMap::new();
            pending_frequency_map.insert(
                vfo,
                ComponentMessage::new(
                    ComponentId::Hamlib,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                        vfo: if vfo == pancetta_hamlib::Vfo::A { 0 } else { 1 },
                        frequency: 14_074_000,
                    }),
                    std::time::Instant::now(),
                ),
            );
            let pending_frequency = Arc::new(Mutex::new(pending_frequency_map));

            assert!(
                tx_hard_mute_reason(
                    &policy,
                    &restart_inhibit,
                    &hamlib_loop_ready,
                    &pending_frequency,
                    &no_pending(),
                    &not_in_flight(),
                )
                .is_some(),
                "a pending command for {vfo:?} alone must still mute TX"
            );
        }
    }

    #[test]
    fn tx_policy_disabled_still_mutes_when_everything_else_is_ready() {
        let policy = policy(pancetta_core::TxPolicy::Disabled);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        assert_eq!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &not_in_flight(),
            ),
            Some("TX policy is Disabled")
        );
    }

    /// A poisoned pending-slot lock must fail CLOSED (still muted) -- an
    /// unknown pending state is never "safe to key".
    #[test]
    fn poisoned_pending_lock_fails_closed() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        let pending_split = no_pending();
        let poison_guard = pending_split.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_guard.lock().unwrap();
            panic!("deliberately poison the lock");
        })
        .join();
        assert!(pending_split.is_poisoned());

        assert!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency(),
                &pending_split,
                &not_in_flight(),
            )
            .is_some(),
            "a poisoned pending-slot lock must fail closed (still muted), not be treated as \
             'nothing pending'"
        );
    }

    /// PAN-19 round-16 review (Codex P1) regression guard: "keep restored
    /// rig state pending through CAT application". The exact scenario the
    /// finding describes: the pending slot is ALREADY empty (cleared at
    /// hand-off time by `deliver_pending_hamlib_state`, before the message
    /// loop even started the underlying CAT call), the loop is ready, and
    /// restart isn't inhibiting -- yet a `set_frequency`/`set_split_freq`/
    /// `set_split` call is genuinely in flight right now. PTT must still
    /// be refused: the empty pending slot alone (round 14) doesn't mean
    /// the rig's state is confirmed correct.
    #[test]
    fn in_flight_command_mutes_tx_even_when_the_pending_slot_is_already_empty() {
        let policy = policy(pancetta_core::TxPolicy::Full);
        let restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_loop_ready = Arc::new(AtomicBool::new(true));
        let in_flight = Arc::new(AtomicU32::new(1));

        let reason = tx_hard_mute_reason(
            &policy,
            &restart_inhibit,
            &hamlib_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &in_flight,
        );
        assert!(
            reason.is_some(),
            "TX must stay muted while a rig frequency/split command is in flight, even though \
             both pending slots are already empty"
        );

        // Once the CAT call resolves and the in-flight flag clears, PTT-on
        // must be permitted again -- the fix must not become overly
        // conservative.
        in_flight.store(0, Ordering::Release);
        assert_eq!(
            tx_hard_mute_reason(
                &policy,
                &restart_inhibit,
                &hamlib_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &in_flight,
            ),
            None,
            "PTT-on must be permitted again once the in-flight CAT call has resolved"
        );
    }
}

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

/// Whether a **remote-originated** TX request is permitted to key PTT right now,
/// per the station-agent arm gate.
///
/// This is the safety gate for `TxOrigin::Remote` requests only — the worker
/// never calls it for `TxOrigin::Local` (local TX is byte-identical). It reads
/// the shared [`ArmState`](pancetta_agent::arm::ArmState) and returns
/// `arm.tx_permitted(now_ms)`.
///
/// **Fail-CLOSED on a poisoned lock** — the OPPOSITE of [`tx_qso_is_live`]'s
/// fail-open. This is a safety gate: if the arm mutex is poisoned we can no
/// longer prove the station consented, so remote TX is DENIED. `now_ms` is unix
/// milliseconds (the one clock read; `ArmState` itself is pure).
pub fn remote_tx_permitted(
    arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    now_ms: i64,
) -> bool {
    match arm.lock() {
        Ok(state) => state.tx_permitted(now_ms),
        Err(_) => false,
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
}

/// Result of draining + coalescing a backlog of `TransmitRequest`s.
#[derive(Debug, Default, PartialEq)]
pub struct CoalesceOutcome {
    /// The requests to actually transmit, after coalescing per `qso_id`
    /// (newest wins), dropping terminal-QSO requests, and capping the
    /// distinct-stream count. Order is the order each retained key was first
    /// seen, so the head entry is the oldest-surviving stream.
    pub retained: Vec<CoalesceEntry>,
    /// How many requests were superseded by a newer request for the SAME
    /// `qso_id` (i.e. older keep-call frames dropped in favor of the latest).
    pub coalesced: usize,
    /// How many requests were dropped because their `qso_id` is no longer in
    /// the active set (terminal / cancelled / completed-past-grace QSO).
    pub dropped_terminal: usize,
    /// How many distinct streams were dropped because the retained set hit
    /// the [`MAX_RETAINED_TX_STREAMS`] cap (silent truncation made visible).
    pub truncated: usize,
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
pub fn coalesce_transmit_requests(
    drained: Vec<CoalesceEntry>,
    mut qso_is_live: impl FnMut(Option<&str>) -> bool,
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
            continue;
        }
        match entry.qso_id.as_deref() {
            None => manual.push(entry),
            Some(id) => {
                let key = super::active_tx_qso_key(id);
                if by_qso.insert(key.clone(), entry).is_some() {
                    // Superseded an older frame for the same QSO.
                    outcome.coalesced += 1;
                } else {
                    order.push(key);
                }
            }
        }
    }

    // Assemble retained in a stable order: coalesced QSO streams (first-seen
    // order) followed by manual sends (drain order). Manual sends go last so a
    // single-stream QSO backlog keeps the QSO as the headline item.
    let mut retained: Vec<CoalesceEntry> = Vec::with_capacity(order.len() + manual.len());
    for key in order {
        if let Some(e) = by_qso.remove(&key) {
            retained.push(e);
        }
    }
    retained.append(&mut manual);

    // Enforce the distinct-stream cap.
    if retained.len() > MAX_RETAINED_TX_STREAMS {
        outcome.truncated = retained.len() - MAX_RETAINED_TX_STREAMS;
        retained.truncate(MAX_RETAINED_TX_STREAMS);
    }

    // Bundle-parity conflict check. When ≥2 distinct streams survive, they may
    // get folded into a single `MultiTransmitRequest` by the caller, stamped
    // with ONE `tx_parity` (the first-seen/oldest-retained stream's — the
    // "bundle anchor"). A later stream whose OWN concrete `tx_parity` disagrees
    // with the anchor must never silently ride that bundle: every concurrent
    // active QSO must transmit on the same parity (CLAUDE.md invariant), and
    // coercing a disagreeing stream onto the bundle's parity would put it in
    // the wrong slot window for its actual partner. `None` (no preference) is
    // NOT a disagreement — only two concrete, differing `Some` values are a
    // genuine conflict. Excluded streams are dropped from `retained` for THIS
    // cycle only; they are not gone — the QSO's own cadence produces a fresh
    // request for it next slot, which will then coalesce/bundle normally.
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
                     excluding it from this bundle — it will be retried individually on \
                     its own parity next cycle",
                    entry.qso_id, entry.tx_parity, anchor
                );
                outcome.parity_excluded += 1;
            } else {
                kept.push(entry);
            }
        }
        retained = kept;
    }

    // FQ-F4/TX-F6 defense-in-depth: pairwise frequency-separation check.
    // `modulate_multi_tx` fails the WHOLE bundle (not just the colliding
    // pair) if any two folded streams' `frequency_offset`s are closer than
    // its minimum separation (signal bandwidth + 25 Hz guard). Fix 5's own
    // de-confliction at QSO-open time should make a collision here rare,
    // but this is cheap insurance against any other path that still
    // produces close-together streams. Greedy, order-preserving: the
    // earliest-seen (already-kept) stream at a given offset wins; a later
    // stream too close to ANY already-kept stream is excluded from this
    // cycle's bundle only — not coerced onto a different offset, and not
    // gone permanently (its own TX cadence produces a fresh request next
    // cycle, which will then coalesce/bundle normally).
    if retained.len() > 1 {
        let mut kept: Vec<CoalesceEntry> = Vec::with_capacity(retained.len());
        for entry in retained.into_iter() {
            let too_close = kept.iter().any(|k: &CoalesceEntry| {
                (k.frequency_offset - entry.frequency_offset).abs()
                    < pancetta_qso::MIN_TX_SEPARATION_HZ
            });
            if too_close {
                warn!(
                    target: "pancetta::tx.policy",
                    "TX bundle frequency conflict: stream qso_id={:?} at {:.0} Hz is within \
                     {:.0} Hz of an already-retained stream this cycle; excluding it from \
                     this bundle — it will be retried individually on its own offset next cycle",
                    entry.qso_id, entry.frequency_offset, pancetta_qso::MIN_TX_SEPARATION_HZ
                );
                outcome.freq_excluded += 1;
            } else {
                kept.push(entry);
            }
        }
        retained = kept;
    }

    outcome.retained = retained;
    outcome
}

/// Drain the queued backlog behind a head `TransmitRequest` and collapse it to
/// current intent, returning the `MessageType` the worker should actually
/// process this cycle.
///
/// `head` MUST be a `MessageType::TransmitRequest` (the caller checks). The
/// channel is drained non-blockingly: every additional `TransmitRequest` is
/// folded into the coalesce buffer; the FIRST non-`TransmitRequest`
/// (`MultiTransmitRequest` / `TuneRequest` / anything else) stops the drain and
/// is re-enqueued to the transmitter's own channel so it is never reordered
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
async fn coalesce_backlog_into(
    head: MessageType,
    tx_rx: &crossbeam_channel::Receiver<ComponentMessage>,
    message_bus: &MessageBus,
    active_tx_qsos: &std::sync::Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
) -> MessageType {
    // Decompose the head into a CoalesceEntry. (Caller guarantees the variant.)
    let head_entry = match head {
        MessageType::TransmitRequest {
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
        } => CoalesceEntry {
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
            origin,
        },
        // Defensive: not a TransmitRequest — hand it back unchanged.
        other => return other,
    };

    // Drain queued TransmitRequests behind the head; stop at the first
    // non-TransmitRequest and re-enqueue it so it is processed next cycle.
    let mut drained = vec![head_entry];
    while let Ok(msg) = tx_rx.try_recv() {
        match msg.message_type {
            MessageType::TransmitRequest {
                message_text,
                frequency_offset,
                qso_id,
                tx_parity,
                origin,
            } => {
                drained.push(CoalesceEntry {
                    message_text,
                    frequency_offset,
                    qso_id,
                    tx_parity,
                    origin,
                });
            }
            _ => {
                // Non-TX message: re-enqueue verbatim and stop draining so we
                // never reorder Tune/Multi ahead of, or behind, TX intent.
                if let Err(e) = message_bus.send_message(msg).await {
                    warn!(
                        target: "pancetta::tx.policy",
                        "failed to re-enqueue non-TX message during coalesce drain: {}",
                        e
                    );
                    emit_diagnostic(
                        message_bus,
                        "tx.policy",
                        pancetta_core::DiagnosticLevel::Warn,
                        format!("failed to re-enqueue non-TX message during coalesce drain: {e}"),
                        None,
                    )
                    .await;
                }
                break;
            }
        }
    }

    // Fast path: only the head was present — nothing to coalesce.
    if drained.len() == 1 {
        let e = drained.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
        };
    }

    let backlog_total = drained.len();
    let outcome = coalesce_transmit_requests(drained, |id| tx_qso_is_live(id, active_tx_qsos));

    if !outcome.is_noop() {
        let text = format!(
            "TX backlog coalesced: drained {} request(s) → {} retained; \
             coalesced {} stale (newest-per-QSO wins), dropped {} for ended QSOs, \
             truncated {} over the {}-stream cap, excluded {} for a bundle-parity conflict, \
             excluded {} for a bundle-frequency conflict",
            backlog_total,
            outcome.retained.len(),
            outcome.coalesced,
            outcome.dropped_terminal,
            outcome.truncated,
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

    // Every drained request belonged to an ended QSO (rare: needs ≥2 queued
    // requests, all terminal). Hand back an empty MultiTransmitRequest; the
    // multi-TX arm's "empty after dropping stale items" branch consumes and
    // skips it without keying PTT. These QSOs already transitioned terminal, so
    // there is no live state machine awaiting a TransmitComplete.
    if outcome.retained.is_empty() {
        return MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
    }

    // Single retained distinct stream → single TransmitRequest (unchanged arm).
    if outcome.retained.len() == 1 {
        let e = outcome.retained.into_iter().next().expect("len == 1");
        return MessageType::TransmitRequest {
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
            tx_parity: e.tx_parity,
            origin: e.origin,
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
    // Fail-safe origin fold: if ANY folded stream is Remote, the whole bundle is
    // Remote so the arm gate applies. (In practice a coalesced backlog is one
    // origin; this is defense-in-depth for a future mixed-origin backlog.)
    let bundle_origin = if outcome
        .retained
        .iter()
        .any(|e| e.origin == crate::message_bus::TxOrigin::Remote)
    {
        crate::message_bus::TxOrigin::Remote
    } else {
        crate::message_bus::TxOrigin::Local
    };
    let items = outcome
        .retained
        .into_iter()
        .map(|e| crate::message_bus::TransmitRequestItem {
            message_text: e.message_text,
            frequency_offset: e.frequency_offset,
            qso_id: e.qso_id,
        })
        .collect();
    MessageType::MultiTransmitRequest {
        items,
        tx_parity: bundle_parity,
        origin: bundle_origin,
    }
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        // Always clear keyed state, on every exit path (normal / abort / panic).
        self.ptt_active
            .store(false, std::sync::atomic::Ordering::Release);
        if self.armed {
            let bus = self.message_bus.clone();
            // Spawn a fire-and-forget task to send PTT-off.
            // This runs even if the parent task was cancelled.
            // rationale: intentional detach — `spawn` runs the task independently;
            // the dropped JoinHandle is the canonical fire-and-forget idiom.
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::task::spawn(async move {
                let ptt_off_msg = ComponentMessage::new(
                    ComponentId::Ft8Transmitter,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: false,
                    }),
                    Instant::now(),
                );
                if let Err(e) = bus.send_message(ptt_off_msg).await {
                    tracing::error!("PTT GUARD: failed to force PTT off on drop: {}", e);
                } else {
                    tracing::warn!("PTT GUARD: forced PTT off due to task cancellation");
                }
            });
        }
    }
}

/// Recompute scheduling for a superseding request against *now* and, if
/// still viable within `tx_late_max_ms_eff`, deassert PTT (clean stop of the
/// aborted transmission), and mutate `message_text`/`frequency_offset`/
/// `schedule` in place so the caller's retry loop re-runs Steps 1-10 with the
/// new content (the caller re-encodes at Step 1, and requests an audio-buffer
/// flush at Step 7 via the `is_rekey` flag so no stale samples from the
/// aborted transmission bleed into the re-key).
///
/// Returns `true` if the caller should retry with the mutated state, `false`
/// if re-keying isn't viable this slot (already deasserted PTT; caller
/// should stop and let the request flow through the worker's next natural
/// dequeue cycle for the next slot — this function does NOT re-enqueue it;
/// see the design spec's "Error handling" section for why there's nothing
/// to re-enqueue for a `TransmitRequest`, since dropping it here means it's
/// simply gone — a real gap the design spec accepted for Phase 1, matching
/// today's F8-abort behavior of not resending either).
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
/// `Local` frame skip the key-time arm gate). `in_flight_origin` is the origin
/// of the transmission being aborted, needed only to fail-safe-fold the
/// `bundle_origin` (Remote if EITHER stream is Remote).
#[allow(clippy::too_many_arguments)]
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
    /// isn't a re-keyable single `TransmitRequest`). Caller abandons this
    /// message; PTT was already deasserted.
    NotViable,
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
        /// The NEW request's OWN origin — used when the caller falls back to a
        /// single-item replace on a frequency collision (that fallback drops
        /// the in-flight item and transmits ONLY the new one, so it must gate
        /// with the new request's own origin, not the folded bundle origin).
        new_origin: crate::message_bus::TxOrigin,
    },
}

#[allow(clippy::too_many_arguments)]
async fn supersede_and_rekey_or_bundle(
    new_request: MessageType,
    message_text: &mut String,
    frequency_offset: &mut f64,
    schedule: &mut TxSchedule,
    message_bus: &crate::message_bus::MessageBus,
    ptt_active: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx_late_max_ms_eff: u64,
    sample_rate: u32,
    slot_ns: i64,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    _request_received_at: chrono::DateTime<chrono::Utc>,
    max_concurrent_qsos: u32,
    in_flight_origin: crate::message_bus::TxOrigin,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
) -> SupersedeOutcome {
    // Deassert PTT immediately and UNCONDITIONALLY — before we even inspect the
    // superseding message's type or its schedulability. EVERY return path out of
    // this function (viable re-key, too-late defer, or a non-re-keyable
    // `MultiTransmitRequest`) must leave PTT off, because both callers assume it:
    // the multi-TX arm unconditionally `ptt_guard.disarm()`s (skipping the
    // guard's own Drop PTT-off) right after this returns, and the single-TX arm
    // relies on PTT already being off on its `NotViable`/`Replace`/`Bundle`
    // arms. Doing this up front (rather than after the `TransmitRequest`
    // destructure) is what fixes the stuck-PTT bug when the superseding message
    // is a `MultiTransmitRequest`. (The aborted transmission's audio may still
    // be draining from the ring buffer; PTT-off means that's harmless — the
    // flush happens next, before new audio is pushed in Step 7 of the retry.)
    let ptt_off_msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Hamlib,
        MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt { state: false }),
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
        warn!("supersede: PTT OFF failed: {}", e);
    }
    ptt_active.store(false, Ordering::Release);

    let MessageType::TransmitRequest {
        message_text: new_text,
        frequency_offset: new_freq,
        tx_parity: new_tx_parity,
        qso_id: new_qso_id,
        origin: new_origin,
    } = new_request
    else {
        // A `MultiTransmitRequest` arriving as the superseding message isn't
        // re-keyable content here (Phase 1 manual triggers are single
        // requests) — not viable. PTT was already deasserted above, so the
        // caller can safely disarm its guard and abandon this message. The
        // multi-TX caller re-enqueues the dropped bundle (see
        // `supersede_multi_reenqueue`); the single-TX caller drops it (an
        // already-accepted Phase-1 gap for single-request supersedes).
        return SupersedeOutcome::NotViable;
    };

    let now = chrono::Utc::now();
    let required_parity = resolve_required_parity(new_tx_parity, tx_self_parity, now, slot_ns);
    let new_schedule = schedule_tx(
        now,
        required_parity,
        tx_late_max_ms_eff,
        sample_rate,
        slot_ns,
    );

    if new_schedule.deferred {
        info!(
            target: "pancetta::tx.pivot",
            "supersede: '{}' arrived too late to re-key this slot — deferring to next slot via normal scheduling",
            new_text
        );
        return SupersedeOutcome::NotViable;
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
    let _ = last_ptt_on_ms; // re-stamped by PttGuard when Step 5 re-asserts PTT

    // Prefer folding the new request into a multi-TX bundle alongside the
    // in-flight content when the station runs concurrent QSOs. The actual
    // frequency-separation check (>= ~75 Hz for FT8) lives inside
    // `encode_and_modulate_multi_tx`; the caller runs it and falls back to the
    // single-item `Replace` mutation above on a collision.
    if max_concurrent_qsos > 1 {
        let mut candidate_items: Vec<crate::message_bus::TransmitRequestItem> =
            in_flight_items.to_vec();
        candidate_items.push(crate::message_bus::TransmitRequestItem {
            message_text: new_text,
            frequency_offset: new_freq,
            qso_id: new_qso_id,
        });
        // Fail-safe origin fold: the bundle carries BOTH the in-flight stream
        // and the new one, so it must be gated if EITHER is Remote.
        let bundle_origin = if in_flight_origin == crate::message_bus::TxOrigin::Remote
            || new_origin == crate::message_bus::TxOrigin::Remote
        {
            crate::message_bus::TxOrigin::Remote
        } else {
            crate::message_bus::TxOrigin::Local
        };
        return SupersedeOutcome::Bundle {
            items: candidate_items,
            bundle_origin,
            new_origin,
        };
    }

    SupersedeOutcome::Replace {
        origin: new_origin,
        qso_id: new_qso_id,
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
/// - `NotViable` because the superseding message was itself a
///   `MultiTransmitRequest` → re-enqueue that whole incoming bundle unchanged
///   (it can't be folded by the single-item re-key path, but dropping a
///   concurrent-QSO bundle would lose the slot's transmissions).
/// - `NotViable` because a re-keyable request arrived too late for this slot →
///   nothing re-enqueued; abandoned (a Task-6-accepted Phase-1 drop).
///
/// In every case `supersede_and_rekey_or_bundle` has already deasserted PTT, so
/// the caller safely disarms its `PttGuard` and `continue`s after this returns.
#[allow(clippy::too_many_arguments)]
async fn supersede_multi_reenqueue(
    new_request: MessageType,
    in_flight_items: &[crate::message_bus::TransmitRequestItem],
    origin: crate::message_bus::TxOrigin,
    encoder: &mut Ft8Encoder,
    active_protocol: pancetta_ft8::Protocol,
    tx_params: &pancetta_ft8::ProtocolParams,
    message_bus: &crate::message_bus::MessageBus,
    ptt_active: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    tx_late_max_ms_eff: u64,
    sample_rate: u32,
    slot_ns: i64,
    tx_self_parity: pancetta_config::station::TxSelfParity,
    request_received_at: chrono::DateTime<chrono::Utc>,
    max_concurrent_qsos: u32,
) {
    // Scratch single-item working state: the multi-TX arm never reads it back
    // (it re-enqueues rather than retrying in place), so these locals only
    // exist to satisfy `supersede_and_rekey_or_bundle`'s `&mut` contract and
    // to recover the new item on the `Replace` edge case.
    let mut scratch_text = String::new();
    let mut scratch_freq = 0.0f64;
    let mut scratch_schedule = TxSchedule {
        target_slot: chrono::Utc::now(),
        silent_pad_samples: 0,
        cursor_offset_samples: 0,
        deferred: false,
    };
    // `supersede_and_rekey_or_bundle` returns `NotViable` for two distinct
    // reasons: (a) the superseding message is itself a `MultiTransmitRequest`
    // (the single-item re-key path can't fold it), or (b) a re-keyable
    // `TransmitRequest` arrived too late to make this slot. In case (a) the
    // incoming message is a whole concurrent-QSO bundle the autonomous engine
    // enqueues every slot — silently dropping it (it was already consumed via
    // `try_recv`) would lose that slot's transmissions. So keep a copy to
    // re-enqueue on `NotViable`; case (b) leaves this `None` and remains a
    // (Task-6-accepted) drop. The message type is decided here BEFORE the call
    // moves `new_request`.
    let reenqueue_on_notviable = if matches!(new_request, MessageType::MultiTransmitRequest { .. })
    {
        Some(new_request.clone())
    } else {
        None
    };
    match supersede_and_rekey_or_bundle(
        new_request,
        &mut scratch_text,
        &mut scratch_freq,
        &mut scratch_schedule,
        message_bus,
        ptt_active,
        last_ptt_on_ms,
        tx_late_max_ms_eff,
        sample_rate,
        slot_ns,
        tx_self_parity,
        request_received_at,
        max_concurrent_qsos,
        origin,
        in_flight_items,
    )
    .await
    {
        SupersedeOutcome::NotViable => {
            // PTT already off; the in-flight bundle is abandoned by the caller.
            // If NotViable because the superseding message was itself a
            // `MultiTransmitRequest`, re-enqueue that whole bundle back to the
            // worker's own channel (mirroring the `Bundle` re-enqueue below) so
            // the next dequeue transmits it via the unmodified Steps 1-10 — the
            // newer bundle supersedes the abandoned in-flight one, so exactly
            // one bundle stays pending (no duplicate). A `None` here is the
            // too-late-for-slot case, which stays a drop.
            if let Some(bundle) = reenqueue_on_notviable {
                let reenqueue = ComponentMessage::new(
                    ComponentId::Ft8Transmitter,
                    ComponentId::Ft8Transmitter,
                    bundle,
                    Instant::now(),
                );
                if let Err(e) = message_bus.send_message(reenqueue).await {
                    warn!(
                        "supersede (multi-TX): failed to re-enqueue superseding bundle: {}",
                        e
                    );
                }
            }
        }
        SupersedeOutcome::Bundle {
            items,
            bundle_origin,
            new_origin,
        } => {
            let bundle_outcome =
                encode_and_modulate_multi_tx(encoder, active_protocol, tx_params, &items);
            let reenqueue = if bundle_outcome.samples.is_ok() {
                ComponentMessage::new(
                    ComponentId::Ft8Transmitter,
                    ComponentId::Ft8Transmitter,
                    MessageType::MultiTransmitRequest {
                        items,
                        tx_parity: None,
                        // Fail-safe folded origin (Remote if either the in-flight
                        // bundle or the new request was Remote), NOT the in-flight
                        // bundle's origin — otherwise a Remote item folded onto a
                        // Local in-flight bundle would re-enter ungated.
                        origin: bundle_origin,
                    },
                    Instant::now(),
                )
            } else {
                // Frequency collision: fall back to re-enqueueing only the new
                // item (always the last element of the candidate bundle). This
                // transmits ONLY the new stream, so it re-enters with the NEW
                // request's own origin, not the in-flight bundle's.
                let new_item = items
                    .last()
                    .cloned()
                    .expect("candidate bundle always contains the new item");
                ComponentMessage::new(
                    ComponentId::Ft8Transmitter,
                    ComponentId::Ft8Transmitter,
                    MessageType::TransmitRequest {
                        message_text: new_item.message_text,
                        frequency_offset: new_item.frequency_offset,
                        qso_id: new_item.qso_id,
                        tx_parity: None,
                        origin: new_origin,
                    },
                    Instant::now(),
                )
            };
            if let Err(e) = message_bus.send_message(reenqueue).await {
                warn!(
                    "supersede (multi-TX): failed to re-enqueue bundle-add: {}",
                    e
                );
            }
        }
        SupersedeOutcome::Replace {
            origin: new_origin,
            qso_id: new_qso_id,
        } => {
            let reenqueue = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Ft8Transmitter,
                MessageType::TransmitRequest {
                    message_text: scratch_text,
                    frequency_offset: scratch_freq,
                    // PAN-38 round 4 (Codex): now threaded through
                    // `SupersedeOutcome::Replace` instead of always `None`
                    // ("manual (drop-stale-ungated) send").
                    qso_id: new_qso_id,
                    tx_parity: None,
                    // The re-enqueued replace carries the NEW request's origin so
                    // the pickup-time gate re-evaluates against IT, not the
                    // aborted in-flight bundle's origin (aligns with the C1 fix).
                    origin: new_origin,
                },
                Instant::now(),
            );
            if let Err(e) = message_bus.send_message(reenqueue).await {
                warn!("supersede (multi-TX): failed to re-enqueue replace: {}", e);
            }
        }
    }
}

impl super::ApplicationCoordinator {
    /// Start FT8 transmitter component
    pub(crate) async fn start_transmitter_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_transmitter");
        let _enter = span.enter();

        info!("Starting FT8 transmitter component");

        let (_tx_sender, tx_rx) = self
            .message_bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await?;
        let message_bus = self.message_bus.clone();

        // Capture config snapshot for TX timing parameters.
        let (tx_late_max_ms, tx_self_parity, ptt_lead_ms, sample_rate, max_concurrent_qsos) = {
            let cfg = self.config.read().await;
            (
                cfg.station.tx_late_max_ms,
                cfg.station.tx_self_parity,
                cfg.station.ptt_lead_ms,
                12000u32, // FT8 sample rate
                // Mid-TX supersede bundle-add (Task 7): when the station runs
                // concurrent QSOs, a superseding manual request folds into the
                // current window's multi-TX bundle alongside the in-flight
                // content instead of fully replacing it. `1` (the default)
                // keeps the single-item replace path byte-identical to Task 6.
                cfg.autonomous.max_concurrent_qsos,
            )
        };

        let tx_handle = {
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
            // Drop-stale-TX gate: the QSO component keeps this set in sync;
            // the worker refuses to key PTT for a request whose `qso_id` is no
            // longer present (superseded / cancelled / completed-past-grace).
            let active_tx_qsos = self.active_tx_qsos.clone();
            // Newest-TX-intent map: at key-time the worker pivots to the
            // freshest message for this QSO if a later decode advanced the
            // exchange while this frame waited out the pre-PTT sleep.
            let latest_tx_intent = self.latest_tx_intent.clone();
            // Keyed-state flag for the SWR poll / TUI (set by PttGuard).
            let ptt_active = self.ptt_active.clone();
            // Timestamp of the most recent PTT-on, read by the FT8 decode
            // loop's TX-adjacent-desense diagnostic (see `ft8.rs`).
            let last_ptt_on_ms = self.last_ptt_on_ms.clone();
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
                // (coordinator/mod.rs) for the pure membership check.
                let mut pivoted_once: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();

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

                    match tx_rx.try_recv() {
                        Ok(mut message) => {
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
                            let request_received_at = chrono::Utc::now();

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
                            if matches!(message.message_type, MessageType::TransmitRequest { .. }) {
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
                                let queue_len_before_base = tx_rx.len();
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
                                let mut prev_len = tx_rx.len();
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
                                    let new_len = tx_rx.len();
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
                                    &tx_rx,
                                    &message_bus,
                                    &active_tx_qsos,
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
                                    if super::is_pivot_duplicate(
                                        qso_id.as_deref(),
                                        &message_text,
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
                                        if let Some(id) = qso_id.as_deref() {
                                            pivoted_once.remove(&super::active_tx_qso_key(id));
                                        }
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                                qso_id: qso_id.clone(),
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
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
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                                qso_id: qso_id.clone(),
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
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
                                        && !remote_tx_permitted(
                                            &remote_tx_arm,
                                            chrono::Utc::now().timestamp_millis(),
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
                                        audit_log.append(&pancetta_agent::audit::AuditEvent {
                                            ts_unix_ms: chrono::Utc::now().timestamp_millis(),
                                            kind: pancetta_agent::audit::AuditKind::TxDenied,
                                            operator_callsign: remote_tx_arm.lock().ok().and_then(
                                                |s| s.operator_callsign().map(str::to_string),
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
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                                qso_id: qso_id.clone(),
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text,
                                                        duration_ms: 0,
                                                        qso_id: qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
                                                continue 'worker;
                                            }
                                        };

                                        // --- Step 2: Resolve required parity ---
                                        let slot_ns = active_slot_ns.load(Ordering::Relaxed);
                                        let required_parity = resolve_required_parity(
                                            tx_parity,
                                            tx_self_parity,
                                            request_received_at,
                                            slot_ns,
                                        );

                                        let mut schedule = schedule_tx(
                                            request_received_at,
                                            required_parity,
                                            tx_late_max_ms_effective(
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
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text,
                                                        duration_ms: 0,
                                                        qso_id: qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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

                                        // --- Step 4: Sleep until PTT engage instant ---
                                        let ptt_target_utc = schedule.target_slot
                                            - chrono::Duration::milliseconds(ptt_lead_ms as i64);
                                        let to_ptt = pancetta_core::slot::duration_until(
                                            ptt_target_utc,
                                            chrono::Utc::now(),
                                        );
                                        if interruptible_sleep(to_ptt, &shutdown, &abort_current_tx)
                                            .await
                                        {
                                            if shutdown.load(Ordering::Acquire) {
                                                info!("TX aborted before PTT engage by shutdown");
                                                break 'worker;
                                            }
                                            info!("TX aborted before PTT engage by operator (F8)");
                                            // This abort happens BEFORE the TxStatusGuard is
                                            // constructed, so its Drop-based clear never runs.
                                            // Clear the strip explicitly so the QUEUED row
                                            // doesn't sit stale until the next status push.
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            // PAN-38 round 3 (Codex): an F8 abort here is the
                                            // one path in this worker that previously sent NO
                                            // TransmitComplete at all -- for a self-CQ whose
                                            // QSO was already opened (AutonomousCqOpened
                                            // registered it in the coordinator's
                                            // pending_self_cq_qsos map), that left the entry
                                            // permanently leaked and the speculative "+1"
                                            // streak never rolled back, since nothing ever
                                            // told the autonomous operator this attempt did
                                            // not actually transmit. Report it exactly like
                                            // the drop-stale-TX case above.
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue 'worker;
                                        }

                                        // --- Step 4b: Drop-stale-TX gate ---
                                        // The slot wait above can span the moment a QSO
                                        // ends (superseded by a newer call, cancelled,
                                        // or completed-past-grace). Re-check active
                                        // status at the last instant before keying:
                                        // if this request's QSO is no longer live, do
                                        // NOT key PTT / build+send audio — clear the
                                        // strip, report a failed TransmitComplete, and
                                        // skip. Requests with no qso_id (manual / tune)
                                        // are never gated.
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
                                                pivoted_once.remove(&super::active_tx_qso_key(id));
                                            }
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                            && !remote_tx_permitted(
                                                &remote_tx_arm,
                                                chrono::Utc::now().timestamp_millis(),
                                            )
                                        {
                                            info!(
                                                target: "agent.tx",
                                                "dropping remote TX at key-time — arm went stale during slot wait: '{}' (qso: {:?})",
                                                message_text, qso_id
                                            );
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue 'worker;
                                        }

                                        if let Some(reason) = tx_hard_mute_reason(
                                            &tx_policy,
                                            &tx_restart_inhibit,
                                            &hamlib_command_loop_ready,
                                            &hamlib_pending_frequency,
                                            &hamlib_pending_split,
                                            &hamlib_command_in_flight,
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue 'worker;
                                        }

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
                                        let (fresh_pad_samples, fresh_cursor_samples) =
                                            pad_and_cursor_for_target(
                                                chrono::Utc::now(),
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue 'worker;
                                        }
                                        let audio_duration_ms =
                                            (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                                as u64;

                                        // --- Step 4c: late pivot to the freshest message ---
                                        // Our decoder finishes ~1.8s BEFORE the slot
                                        // boundary, but a fresher decode for THIS QSO can
                                        // still land while this frame waited out the (up to
                                        // ~30s) pre-PTT sleep. If the QSO component has since
                                        // produced a newer message for this qso_id, swap to
                                        // it now and re-modulate. We're at the slot boundary
                                        // — comfortably inside the ~1.5s switch budget — and
                                        // re-modulation is <100ms. tx_parity is unchanged (a
                                        // QSO holds one parity for its whole exchange), so the
                                        // schedule (pad/cursor) stays valid, and every FT8
                                        // frame is the same 79-symbol length so audio_out's
                                        // length (and audio_duration_ms) are unchanged.
                                        if let Some(intent) =
                                            latest_tx_intent.read().ok().and_then(|m| {
                                                super::tx_pivot_target(
                                                    qso_id.as_deref(),
                                                    &message_text,
                                                    &m,
                                                )
                                            })
                                        {
                                            let new_text = intent.message_text;
                                            let new_freq = intent.frequency_offset;
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
                                                    message_text = new_text;
                                                    frequency_offset = new_freq;
                                                    audio_out = rebuilt;
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
                                                        pivoted_once.insert(
                                                            super::active_tx_qso_key(id),
                                                            message_text.clone(),
                                                        );
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
                                        let mut ptt_guard = PttGuard::new(
                                            message_bus.clone(),
                                            ptt_active.clone(),
                                            &last_ptt_on_ms,
                                        );
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
                                        let ptt_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Hamlib,
                                            MessageType::RigControl(
                                                crate::message_bus::RigControlMessage::SetPtt {
                                                    state: true,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(ptt_msg).await {
                                            warn!("PTT ON failed (rig not keyed): {} — if you are transmitting, TX audio may be going to the wrong device", e);
                                        } else {
                                            info!(
                                                target: "pancetta::tx.ptt",
                                                "PTT ON (scheduled TX) sent to rig: '{}' @{:.0}Hz qso={}",
                                                message_text,
                                                frequency_offset,
                                                qso_id.as_deref().unwrap_or("-")
                                            );
                                        }

                                        // --- Step 6: Sleep precisely until target slot start ---
                                        // (audio_out itself includes any silent_pad needed past
                                        // the slot boundary; we send it at the boundary.)
                                        let to_slot = pancetta_core::slot::duration_until(
                                            schedule.target_slot,
                                            chrono::Utc::now(),
                                        );
                                        // ptt_guard in scope — drop on any loop exit fires
                                        // PTT-off. A qualifying request arriving here supersedes
                                        // the in-flight frame (aborts + re-keys).
                                        match interruptible_sleep_or_supersede(
                                            to_slot,
                                            &shutdown,
                                            &abort_current_tx,
                                            &tx_rx,
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            &pivoted_once,
                                        )
                                        .await
                                        {
                                            SleepOutcome::Completed => {}
                                            SleepOutcome::AbortedByShutdown => {
                                                info!(
                                                    "TX aborted between PTT and slot by shutdown"
                                                );
                                                break 'worker;
                                            }
                                            SleepOutcome::AbortedByOperator => {
                                                info!("TX aborted between PTT and slot by operator (F8)");
                                                // PAN-38 round 4 (Codex): report the failed
                                                // completion here too -- see the "aborted before
                                                // PTT engage" comment earlier in this worker for
                                                // the full leak this closes.
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(complete_msg).await
                                                {
                                                    warn!("Failed to send TransmitComplete: {}", e);
                                                }
                                                continue 'worker;
                                            }
                                            SleepOutcome::Superseded(new_request) => {
                                                // The current in-flight single item — passed to the
                                                // helper so a bundle-add (max_concurrent_qsos > 1)
                                                // can fold the new request alongside it.
                                                let in_flight_items =
                                                    vec![crate::message_bus::TransmitRequestItem {
                                                        message_text: message_text.clone(),
                                                        frequency_offset,
                                                        qso_id: qso_id.clone(),
                                                    }];
                                                match supersede_and_rekey_or_bundle(
                                                    new_request,
                                                    &mut message_text,
                                                    &mut frequency_offset,
                                                    &mut schedule,
                                                    &message_bus,
                                                    &ptt_active,
                                                    &last_ptt_on_ms,
                                                    tx_late_max_ms_effective(
                                                        active_protocol,
                                                        tx_late_max_ms,
                                                    ),
                                                    sample_rate,
                                                    slot_ns,
                                                    tx_self_parity,
                                                    request_received_at,
                                                    max_concurrent_qsos,
                                                    origin,
                                                    &in_flight_items,
                                                )
                                                .await
                                                {
                                                    SupersedeOutcome::NotViable => {
                                                        // Too late to re-key this slot — PTT already
                                                        // off; abandon THIS message (not the worker):
                                                        // leave the retry loop and fall through to
                                                        // the next dequeue. The armed PttGuard's
                                                        // drop is the PTT-off safety net if the
                                                        // explicit send failed.
                                                        break 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Replace {
                                                        origin: new_origin,
                                                        qso_id: new_qso_id,
                                                    } => {
                                                        // Viable single-item re-key (Task 6): carry
                                                        // the recomputed schedule into the retry,
                                                        // flush stale audio at Step 7, clear the
                                                        // abort flag the supersede set, and disarm
                                                        // this iteration's PttGuard (PTT-off was
                                                        // already sent; Step 5 re-asserts on retry).
                                                        // Re-point `origin` at the SUPERSEDING
                                                        // request's origin so Step 4b-arm gates the
                                                        // frame that is actually about to transmit,
                                                        // not the aborted one (C1 fix). PAN-38
                                                        // round 4: re-point `qso_id` too, for the
                                                        // same reason -- otherwise the retry pairs
                                                        // the superseding frame's text with the
                                                        // ABORTED frame's qso_id.
                                                        // PAN-38 round 5 (Codex): the ABANDONED
                                                        // frame (in_flight_items[0], captured
                                                        // before this supersede) never gets a
                                                        // TransmitComplete -- only the eventual
                                                        // replacement's new_qso_id is ever
                                                        // reported. If the abandoned frame was a
                                                        // tracked self-CQ, its pending_self_cq_qsos
                                                        // entry deterministically leaks and its
                                                        // untransmitted attempt is never rolled
                                                        // back. Report it now, before the qso_id
                                                        // local is overwritten below.
                                                        if let Some(abandoned) =
                                                            in_flight_items.first()
                                                        {
                                                            let complete_msg =
                                                                ComponentMessage::new(
                                                                    ComponentId::Ft8Transmitter,
                                                                    ComponentId::Autonomous,
                                                                    MessageType::TransmitComplete {
                                                                        success: false,
                                                                        message_text: abandoned
                                                                            .message_text
                                                                            .clone(),
                                                                        duration_ms: 0,
                                                                        qso_id: abandoned
                                                                            .qso_id
                                                                            .clone(),
                                                                    },
                                                                    Instant::now(),
                                                                );
                                                            if let Err(e) = message_bus
                                                                .send_message(complete_msg)
                                                                .await
                                                            {
                                                                warn!(
                                                                    "Failed to send TransmitComplete: {}",
                                                                    e
                                                                );
                                                            }
                                                        }
                                                        origin = new_origin;
                                                        qso_id = new_qso_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        ptt_guard.disarm();
                                                        continue 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Bundle {
                                                        items,
                                                        bundle_origin,
                                                        new_origin,
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
                                                            let bundle_msg = ComponentMessage::new(
                                                                ComponentId::Ft8Transmitter,
                                                                ComponentId::Ft8Transmitter,
                                                                MessageType::MultiTransmitRequest {
                                                                    items,
                                                                    tx_parity: None,
                                                                    // Fail-safe folded origin, not
                                                                    // the aborted frame's origin.
                                                                    origin: bundle_origin,
                                                                },
                                                                Instant::now(),
                                                            );
                                                            if let Err(e) = message_bus
                                                                .send_message(bundle_msg)
                                                                .await
                                                            {
                                                                warn!("supersede: failed to re-enqueue multi-TX bundle: {}", e);
                                                            }
                                                            ptt_guard.disarm();
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
                                                        if let Some(abandoned) =
                                                            in_flight_items.first()
                                                        {
                                                            let complete_msg =
                                                                ComponentMessage::new(
                                                                    ComponentId::Ft8Transmitter,
                                                                    ComponentId::Autonomous,
                                                                    MessageType::TransmitComplete {
                                                                        success: false,
                                                                        message_text: abandoned
                                                                            .message_text
                                                                            .clone(),
                                                                        duration_ms: 0,
                                                                        qso_id: abandoned
                                                                            .qso_id
                                                                            .clone(),
                                                                    },
                                                                    Instant::now(),
                                                                );
                                                            if let Err(e) = message_bus
                                                                .send_message(complete_msg)
                                                                .await
                                                            {
                                                                warn!("Failed to send TransmitComplete: {}", e);
                                                            }
                                                        }
                                                        origin = new_origin;
                                                        qso_id = items
                                                            .last()
                                                            .and_then(|item| item.qso_id.clone());
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        ptt_guard.disarm();
                                                        continue 'key_and_send;
                                                    }
                                                }
                                            }
                                        }

                                        // --- Step 7: Route audio to output ---
                                        // Band Activity's own-TX history logs the actual
                                        // audio-start instant here, not Step 5's PTT-key
                                        // time — see `log_tx_frame`'s doc comment.
                                        log_tx_frame(
                                            &message_bus,
                                            message_text.clone(),
                                            frequency_offset,
                                            qso_id.clone(),
                                            chrono::Utc::now(),
                                        )
                                        .await;
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
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(audio_msg).await {
                                            debug!("Audio output routing: {}", e);
                                        }

                                        // --- Step 8: Wait for audio playback to complete ---
                                        // Playback is now on the air; a qualifying request here
                                        // supersedes it (aborts + re-keys), flushing the still-
                                        // playing samples on the retry's Step 7.
                                        match interruptible_sleep_or_supersede(
                                            Duration::from_millis(audio_duration_ms),
                                            &shutdown,
                                            &abort_current_tx,
                                            &tx_rx,
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            &pivoted_once,
                                        )
                                        .await
                                        {
                                            SleepOutcome::Completed => {}
                                            SleepOutcome::AbortedByShutdown => {
                                                info!("TX aborted during playback by shutdown");
                                                break 'worker;
                                            }
                                            SleepOutcome::AbortedByOperator => {
                                                info!(
                                                    "TX aborted during playback by operator (F8)"
                                                );
                                                // PAN-38 round 4 (Codex): report the failed
                                                // completion here too -- see the "aborted before
                                                // PTT engage" comment earlier in this worker.
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(complete_msg).await
                                                {
                                                    warn!("Failed to send TransmitComplete: {}", e);
                                                }
                                                continue 'worker;
                                            }
                                            SleepOutcome::Superseded(new_request) => {
                                                // Same bundle-add-or-replace decision as Step 6.
                                                let in_flight_items =
                                                    vec![crate::message_bus::TransmitRequestItem {
                                                        message_text: message_text.clone(),
                                                        frequency_offset,
                                                        qso_id: qso_id.clone(),
                                                    }];
                                                match supersede_and_rekey_or_bundle(
                                                    new_request,
                                                    &mut message_text,
                                                    &mut frequency_offset,
                                                    &mut schedule,
                                                    &message_bus,
                                                    &ptt_active,
                                                    &last_ptt_on_ms,
                                                    tx_late_max_ms_effective(
                                                        active_protocol,
                                                        tx_late_max_ms,
                                                    ),
                                                    sample_rate,
                                                    slot_ns,
                                                    tx_self_parity,
                                                    request_received_at,
                                                    max_concurrent_qsos,
                                                    origin,
                                                    &in_flight_items,
                                                )
                                                .await
                                                {
                                                    SupersedeOutcome::NotViable => {
                                                        // Too late to re-key this slot — abandon
                                                        // THIS message (not the worker): leave the
                                                        // retry loop and fall through to next
                                                        // dequeue.
                                                        break 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Replace {
                                                        origin: new_origin,
                                                        qso_id: new_qso_id,
                                                    } => {
                                                        // Re-point `origin` at the superseding
                                                        // request's origin so the retry's Step 4b-arm
                                                        // gates the frame actually transmitting (C1).
                                                        // PAN-38 round 4: re-point `qso_id` too.
                                                        // PAN-38 round 5 (Codex): the ABANDONED
                                                        // frame (in_flight_items[0], captured
                                                        // before this supersede) never gets a
                                                        // TransmitComplete -- only the eventual
                                                        // replacement's new_qso_id is ever
                                                        // reported. If the abandoned frame was a
                                                        // tracked self-CQ, its pending_self_cq_qsos
                                                        // entry deterministically leaks and its
                                                        // untransmitted attempt is never rolled
                                                        // back. Report it now, before the qso_id
                                                        // local is overwritten below.
                                                        if let Some(abandoned) =
                                                            in_flight_items.first()
                                                        {
                                                            let complete_msg =
                                                                ComponentMessage::new(
                                                                    ComponentId::Ft8Transmitter,
                                                                    ComponentId::Autonomous,
                                                                    MessageType::TransmitComplete {
                                                                        success: false,
                                                                        message_text: abandoned
                                                                            .message_text
                                                                            .clone(),
                                                                        duration_ms: 0,
                                                                        qso_id: abandoned
                                                                            .qso_id
                                                                            .clone(),
                                                                    },
                                                                    Instant::now(),
                                                                );
                                                            if let Err(e) = message_bus
                                                                .send_message(complete_msg)
                                                                .await
                                                            {
                                                                warn!(
                                                                    "Failed to send TransmitComplete: {}",
                                                                    e
                                                                );
                                                            }
                                                        }
                                                        origin = new_origin;
                                                        qso_id = new_qso_id;
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        ptt_guard.disarm();
                                                        continue 'key_and_send;
                                                    }
                                                    SupersedeOutcome::Bundle {
                                                        items,
                                                        bundle_origin,
                                                        new_origin,
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
                                                            let bundle_msg = ComponentMessage::new(
                                                                ComponentId::Ft8Transmitter,
                                                                ComponentId::Ft8Transmitter,
                                                                MessageType::MultiTransmitRequest {
                                                                    items,
                                                                    tx_parity: None,
                                                                    // Fail-safe folded origin.
                                                                    origin: bundle_origin,
                                                                },
                                                                Instant::now(),
                                                            );
                                                            if let Err(e) = message_bus
                                                                .send_message(bundle_msg)
                                                                .await
                                                            {
                                                                warn!("supersede: failed to re-enqueue multi-TX bundle: {}", e);
                                                            }
                                                            ptt_guard.disarm();
                                                            break 'key_and_send;
                                                        }
                                                        // Frequency collision: single-item replace
                                                        // of the new item — gate with its own origin.
                                                        // PAN-38 round 5: carry its qso_id too, same
                                                        // reasoning as the other Bundle fallback arm.
                                                        // Also report the abandoned in-flight
                                                        // frame's TransmitComplete before
                                                        // overwriting qso_id.
                                                        if let Some(abandoned) =
                                                            in_flight_items.first()
                                                        {
                                                            let complete_msg =
                                                                ComponentMessage::new(
                                                                    ComponentId::Ft8Transmitter,
                                                                    ComponentId::Autonomous,
                                                                    MessageType::TransmitComplete {
                                                                        success: false,
                                                                        message_text: abandoned
                                                                            .message_text
                                                                            .clone(),
                                                                        duration_ms: 0,
                                                                        qso_id: abandoned
                                                                            .qso_id
                                                                            .clone(),
                                                                    },
                                                                    Instant::now(),
                                                                );
                                                            if let Err(e) = message_bus
                                                                .send_message(complete_msg)
                                                                .await
                                                            {
                                                                warn!(
                                                                    "Failed to send TransmitComplete: {}",
                                                                    e
                                                                );
                                                            }
                                                        }
                                                        origin = new_origin;
                                                        qso_id = items
                                                            .last()
                                                            .and_then(|item| item.qso_id.clone());
                                                        rekey_schedule = Some(schedule);
                                                        is_rekey = true;
                                                        abort_current_tx
                                                            .store(false, Ordering::Release);
                                                        ptt_guard.disarm();
                                                        continue 'key_and_send;
                                                    }
                                                }
                                            }
                                        }
                                        let success = true;
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success,
                                                    message_text: message_text.clone(),
                                                    duration_ms,
                                                    qso_id: qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            if let Err(e) =
                                                message_bus.send_message(complete_msg).await
                                            {
                                                warn!("Failed to send TransmitComplete: {}", e);
                                            }
                                            continue 'worker;
                                        }
                                        let ptt_off_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Hamlib,
                                            MessageType::RigControl(
                                                crate::message_bus::RigControlMessage::SetPtt {
                                                    state: false,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(ptt_off_msg).await
                                        {
                                            warn!(
                                                "PTT OFF failed (rig may be stuck in TX!): {}",
                                                e
                                            );
                                        }
                                        ptt_guard.disarm();

                                        // --- Step 10: Send TransmitComplete ---
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success,
                                                message_text,
                                                duration_ms,
                                                qso_id: qso_id.clone(),
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(complete_msg).await
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
                                                if let Some(id) = item.qso_id.as_deref() {
                                                    pivoted_once
                                                        .remove(&super::active_tx_qso_key(id));
                                                }
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: item.message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: item.qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }

                                    // --- Step 0a: Remote-TX arm gate (bundle) ---
                                    // A Remote-origin bundle must pass the station-agent
                                    // arm gate before ANY item keys PTT. Local bundles
                                    // skip it (byte-identical). ANDs under the TxPolicy
                                    // hard-mute above. Fail CLOSED on a poisoned lock.
                                    // Inert in P0–P2 (no Remote bundle is constructed).
                                    if origin == crate::message_bus::TxOrigin::Remote
                                        && !remote_tx_permitted(
                                            &remote_tx_arm,
                                            chrono::Utc::now().timestamp_millis(),
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
                                        audit_log.append(&pancetta_agent::audit::AuditEvent {
                                            ts_unix_ms: chrono::Utc::now().timestamp_millis(),
                                            kind: pancetta_agent::audit::AuditKind::TxDenied,
                                            operator_callsign: remote_tx_arm.lock().ok().and_then(
                                                |s| s.operator_callsign().map(str::to_string),
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: item.message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: item.qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text: item.message_text.clone(),
                                                duration_ms: 0,
                                                qso_id: item.qso_id.clone(),
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
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
                                            for (text, qso_id) in
                                                item_texts.into_iter().zip(encoded_qso_ids)
                                            {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: text,
                                                        duration_ms: 0,
                                                        qso_id,
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
                                            }
                                            continue;
                                        }
                                    };

                                    // --- Step 2: Resolve required parity ---
                                    let slot_ns = active_slot_ns.load(Ordering::Relaxed);
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        request_received_at,
                                        slot_ns,
                                    );

                                    let mut schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms_effective(active_protocol, tx_late_max_ms),
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
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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
                                                        let complete_msg = ComponentMessage::new(
                                                            ComponentId::Ft8Transmitter,
                                                            ComponentId::Autonomous,
                                                            MessageType::TransmitComplete {
                                                                success: false,
                                                                message_text: item
                                                                    .message_text
                                                                    .clone(),
                                                                duration_ms: 0,
                                                                qso_id: item.qso_id.clone(),
                                                            },
                                                            Instant::now(),
                                                        );
                                                        let _ = message_bus
                                                            .send_message(complete_msg)
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
                                                    let complete_msg = ComponentMessage::new(
                                                        ComponentId::Ft8Transmitter,
                                                        ComponentId::Autonomous,
                                                        MessageType::TransmitComplete {
                                                            success: false,
                                                            message_text: item.message_text.clone(),
                                                            duration_ms: 0,
                                                            qso_id: item.qso_id.clone(),
                                                        },
                                                        Instant::now(),
                                                    );
                                                    let _ = message_bus
                                                        .send_message(complete_msg)
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
                                                        for (text, qso_id) in rebuilt_texts
                                                            .into_iter()
                                                            .zip(rebuilt_qso_ids)
                                                        {
                                                            let complete_msg =
                                                                ComponentMessage::new(
                                                                    ComponentId::Ft8Transmitter,
                                                                    ComponentId::Autonomous,
                                                                    MessageType::TransmitComplete {
                                                                        success: false,
                                                                        message_text: text,
                                                                        duration_ms: 0,
                                                                        qso_id,
                                                                    },
                                                                    Instant::now(),
                                                                );
                                                            let _ = message_bus
                                                                .send_message(complete_msg)
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

                                    // --- Step 4: Sleep until PTT engage instant ---
                                    let ptt_target_utc = schedule.target_slot
                                        - chrono::Duration::milliseconds(ptt_lead_ms as i64);
                                    let to_ptt = pancetta_core::slot::duration_until(
                                        ptt_target_utc,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_ptt, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted before PTT by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted before PTT by operator (F8)");
                                        // PAN-38 round 3 (Codex): same gap as the single-item
                                        // worker's F8-before-PTT path -- no TransmitComplete
                                        // was ever sent for any bundle item, leaking a
                                        // self-CQ's pending_self_cq_qsos entry and never
                                        // rolling back its speculative streak/offset.
                                        for (text, qso_id) in
                                            item_texts.into_iter().zip(encoded_qso_ids)
                                        {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: text,
                                                    duration_ms: 0,
                                                    qso_id,
                                                },
                                                Instant::now(),
                                            );
                                            if let Err(e) =
                                                message_bus.send_message(complete_msg).await
                                            {
                                                warn!("Failed to send TransmitComplete: {}", e);
                                            }
                                        }
                                        continue;
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
                                    let live_mask: Vec<bool> = encoded_qso_ids
                                        .iter()
                                        .map(|id| tx_qso_is_live(id.as_deref(), &active_tx_qsos))
                                        .collect();

                                    if !live_mask.iter().any(|&live| live) {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "dropping stale multi-TX bundle: all {} item(s) ended during the pre-PTT wait",
                                            encoded_qso_ids.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "dropping stale multi-TX bundle: all {} item(s) ended during the pre-PTT wait",
                                                encoded_qso_ids.len()
                                            ),
                                            None,
                                        )
                                        .await;
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: item.message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: item.qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                    let (pivoted_live_items, pivots) =
                                        super::pivot_bundle_items(live_items_pre, &latest_snapshot);
                                    for (qso_key, new_text) in &pivots {
                                        info!(
                                            target: "pancetta::tx.pivot",
                                            "TX pivot (bundle): qso {} -> '{}' (fresher message arrived during pre-PTT wait)",
                                            qso_key,
                                            new_text
                                        );
                                    }
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
                                            for (item, &live) in items.iter().zip(live_mask.iter())
                                            {
                                                if !live {
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
                                                    let complete_msg = ComponentMessage::new(
                                                        ComponentId::Ft8Transmitter,
                                                        ComponentId::Autonomous,
                                                        MessageType::TransmitComplete {
                                                            success: false,
                                                            message_text: item.message_text.clone(),
                                                            duration_ms: 0,
                                                            qso_id: item.qso_id.clone(),
                                                        },
                                                        Instant::now(),
                                                    );
                                                    let _ = message_bus
                                                        .send_message(complete_msg)
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
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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
                                                    for (text, qso_id) in rebuilt_texts
                                                        .into_iter()
                                                        .zip(rebuilt_qso_ids)
                                                    {
                                                        let complete_msg = ComponentMessage::new(
                                                            ComponentId::Ft8Transmitter,
                                                            ComponentId::Autonomous,
                                                            MessageType::TransmitComplete {
                                                                success: false,
                                                                message_text: text,
                                                                duration_ms: 0,
                                                                qso_id,
                                                            },
                                                            Instant::now(),
                                                        );
                                                        let _ = message_bus
                                                            .send_message(complete_msg)
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
                                        && !remote_tx_permitted(
                                            &remote_tx_arm,
                                            chrono::Utc::now().timestamp_millis(),
                                        )
                                    {
                                        info!(
                                            target: "agent.tx",
                                            "dropping remote multi-TX at key-time — arm went stale during slot wait: {} item(s)",
                                            items.len()
                                        );
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        for item in &items {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: item.message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: item.qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: item.message_text.clone(),
                                                    duration_ms: 0,
                                                    qso_id: item.qso_id.clone(),
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                    for (qso_key, new_text) in pivots {
                                        pivoted_once.insert(qso_key, new_text);
                                    }

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
                                    let (fresh_pad_samples, fresh_cursor_samples) =
                                        pad_and_cursor_for_target(
                                            chrono::Utc::now(),
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
                                        for (text, qso_id) in
                                            item_texts.into_iter().zip(encoded_qso_ids_final)
                                        {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: text,
                                                    duration_ms: 0,
                                                    qso_id,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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
                                    let mut ptt_guard = PttGuard::new(
                                        message_bus.clone(),
                                        ptt_active.clone(),
                                        &last_ptt_on_ms,
                                    );
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
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        warn!("PTT ON failed (rig not keyed): {} — if you are transmitting, TX audio may be going to the wrong device", e);
                                    } else {
                                        info!(
                                            target: "pancetta::tx.ptt",
                                            "PTT ON (scheduled multi-TX) sent to rig"
                                        );
                                    }

                                    // --- Step 6: Sleep precisely until target slot ---
                                    // ptt_guard in scope — drop on any loop exit fires PTT-off.
                                    // A qualifying request arriving here supersedes the in-flight
                                    // bundle: prefer folding it in (bundle-add), else re-enqueue
                                    // a single-item replace — see `supersede_multi_reenqueue`.
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    match interruptible_sleep_or_supersede(
                                        to_slot,
                                        &shutdown,
                                        &abort_current_tx,
                                        &tx_rx,
                                        &message_bus,
                                        items.first().and_then(|it| it.qso_id.as_deref()),
                                        items
                                            .first()
                                            .map(|it| it.message_text.as_str())
                                            .unwrap_or(""),
                                        &pivoted_once,
                                    )
                                    .await
                                    {
                                        SleepOutcome::Completed => {}
                                        SleepOutcome::AbortedByShutdown => {
                                            info!(
                                                "Multi-TX aborted between PTT and slot by shutdown"
                                            );
                                            break;
                                        }
                                        SleepOutcome::AbortedByOperator => {
                                            info!("Multi-TX aborted between PTT and slot by operator (F8)");
                                            // PAN-38 round 4 (Codex): report a failed
                                            // completion for every bundle item -- see the
                                            // "Multi-TX aborted before PTT" comment earlier
                                            // in this worker for the full leak this closes.
                                            for item in &items {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(complete_msg).await
                                                {
                                                    warn!("Failed to send TransmitComplete: {}", e);
                                                }
                                            }
                                            continue;
                                        }
                                        SleepOutcome::Superseded(new_request) => {
                                            supersede_multi_reenqueue(
                                                new_request,
                                                &items,
                                                origin,
                                                &mut encoder,
                                                active_protocol,
                                                &tx_params,
                                                &message_bus,
                                                &ptt_active,
                                                &last_ptt_on_ms,
                                                tx_late_max_ms_effective(
                                                    active_protocol,
                                                    tx_late_max_ms,
                                                ),
                                                sample_rate,
                                                slot_ns,
                                                tx_self_parity,
                                                request_received_at,
                                                max_concurrent_qsos,
                                            )
                                            .await;
                                            ptt_guard.disarm();
                                            continue;
                                        }
                                    }

                                    // --- Step 7: Route audio to output ---
                                    // Band Activity's own-TX history logs the actual
                                    // audio-start instant here, not Step 5's PTT-key
                                    // time — see `log_tx_frame`'s doc comment. Every
                                    // bundle item is keyed concurrently in this same
                                    // slot, so all of them share this one timestamp.
                                    let tx_logged_at = chrono::Utc::now();
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
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                            flush_first: false,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for playback to complete ---
                                    // Playback is now on the air; a qualifying request here
                                    // supersedes the bundle (same bundle-add-or-replace decision
                                    // as Step 6 via `supersede_multi_reenqueue`).
                                    match interruptible_sleep_or_supersede(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                        &tx_rx,
                                        &message_bus,
                                        items.first().and_then(|it| it.qso_id.as_deref()),
                                        items
                                            .first()
                                            .map(|it| it.message_text.as_str())
                                            .unwrap_or(""),
                                        &pivoted_once,
                                    )
                                    .await
                                    {
                                        SleepOutcome::Completed => {}
                                        SleepOutcome::AbortedByShutdown => {
                                            info!("Multi-TX aborted during playback by shutdown");
                                            break;
                                        }
                                        SleepOutcome::AbortedByOperator => {
                                            info!(
                                                "Multi-TX aborted during playback by operator (F8)"
                                            );
                                            // PAN-38 round 4 (Codex): report a failed
                                            // completion for every bundle item -- see the
                                            // "Multi-TX aborted before PTT" comment earlier
                                            // in this worker.
                                            for item in &items {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: item.message_text.clone(),
                                                        duration_ms: 0,
                                                        qso_id: item.qso_id.clone(),
                                                    },
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(complete_msg).await
                                                {
                                                    warn!("Failed to send TransmitComplete: {}", e);
                                                }
                                            }
                                            continue;
                                        }
                                        SleepOutcome::Superseded(new_request) => {
                                            supersede_multi_reenqueue(
                                                new_request,
                                                &items,
                                                origin,
                                                &mut encoder,
                                                active_protocol,
                                                &tx_params,
                                                &message_bus,
                                                &ptt_active,
                                                &last_ptt_on_ms,
                                                tx_late_max_ms_effective(
                                                    active_protocol,
                                                    tx_late_max_ms,
                                                ),
                                                sample_rate,
                                                slot_ns,
                                                tx_self_parity,
                                                request_received_at,
                                                max_concurrent_qsos,
                                            )
                                            .await;
                                            ptt_guard.disarm();
                                            continue;
                                        }
                                    }
                                    let success = true;
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
                                        for (text, qso_id) in
                                            item_texts.into_iter().zip(encoded_qso_ids_final)
                                        {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success,
                                                    message_text: text,
                                                    duration_ms,
                                                    qso_id,
                                                },
                                                Instant::now(),
                                            );
                                            if let Err(e) =
                                                message_bus.send_message(complete_msg).await
                                            {
                                                warn!("Failed to send TransmitComplete: {}", e);
                                            }
                                        }
                                        continue;
                                    }
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        warn!("PTT OFF failed (rig may be stuck in TX!): {}", e);
                                    }
                                    ptt_guard.disarm();

                                    // --- Step 10: Send TransmitComplete for each item ---
                                    for (text, qso_id) in
                                        item_texts.into_iter().zip(encoded_qso_ids_final)
                                    {
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success,
                                                message_text: text,
                                                duration_ms,
                                                qso_id,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(complete_msg).await
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
                                        &last_ptt_on_ms,
                                    );
                                    // TX badge on; guard drop clears it on every
                                    // exit path (complete / abort / shutdown).
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
                                    send_tx_status(&message_bus, true).await;
                                    let ptt_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: true,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_msg).await {
                                        warn!("Tune: PTT ON failed (rig not keyed): {}", e);
                                    }

                                    // Emit the audio buffer.
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: tone_samples,
                                            sample_rate,
                                            flush_first: false,
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
                                    let ptt_off_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetPtt {
                                                state: false,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(ptt_off_msg).await {
                                        warn!(
                                            "Tune: PTT OFF failed (rig may be stuck in TX!): {}",
                                            e
                                        );
                                    }
                                    ptt_guard.disarm();
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
mod schedule_tx_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_core::slot::{SlotParity, SLOT_NS};

    /// FT4 slot period in nanoseconds (7.5s). FT8 is `SLOT_NS` (15s).
    const FT4_SLOT_NS: i64 = 7_500_000_000;

    fn at(seconds: f64) -> chrono::DateTime<chrono::Utc> {
        // Reference: 2026-01-01 00:00:00 UTC. timestamp() = 1767225600,
        // divisible by 15. Slot 0 is Even (= 1767225600 / 15 % 2 = 0).
        let base = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        base + chrono::Duration::nanoseconds((seconds * 1_000_000_000.0) as i64)
    }

    #[test]
    fn early_pads_silent_no_skip() {
        // now = :05.0 (Even slot 0). Required = Odd. Current slot is Even
        // (wrong), so we advance to next Odd = :15. mstr_relative_to_target
        // = max(0, :05 - :15) = 0. 0 < 500 → pad 500ms, cursor 0.
        let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12); // 12 samples/ms at 12kHz
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn on_time_no_pad_no_skip() {
        // now = :15.500 (Odd slot 1). Required = Odd. Current slot matches;
        // mstr_in_current_slot = 500ms ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 500 = DELAY_MS → pad 0, cursor 0.
        let s = schedule_tx(at(15.5), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn late_skips_cursor_in_current_slot() {
        // now = :20.0 (Odd slot 1, 5s in). Required = Odd. Current matches;
        // mstr_in_current_slot = 5000 ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 5000 > 500 → cursor = 4500ms × SR.
        let s = schedule_tx(at(20.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 4500 * 12);
    }

    #[test]
    fn too_late_defers_to_next_opposite_slot() {
        // now = :24.5 (Odd slot 1, 9.5s in). Required = Odd. Current
        // matches but mstr_in_current_slot = 9500 > 8000 → too late;
        // advance to next Odd = :45. mstr_relative_to_target = 0 (target
        // is in future) → pad 500ms, cursor 0.
        let s = schedule_tx(at(24.5), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn collision_avoidance_does_not_pick_same_parity() {
        // now = :14.6 (Even slot 0, near end). DX on Even → required Odd.
        // Current parity Even ≠ Odd → advance to next Odd = :15.
        // mstr_relative_to_target = max(0, :14.6 - :15) = 0 → pad 500ms,
        // cursor 0. Most importantly: target is :15, NEVER :30 (the
        // collision case the original bug produced).
        let s = schedule_tx(at(14.6), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_ne!((s.target_slot - at(0.0)).num_milliseconds(), 30_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn at_exact_boundary_targets_current_slot() {
        // now = :15.000 exactly. Required = Odd. The :15 slot is Odd
        // and we're 0ms in — fully viable. Target the current slot,
        // pad 500ms before audio starts.
        let s = schedule_tx(at(15.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn current_slot_correct_parity_but_too_late_defers() {
        // now = :29.0 (Odd slot 1, 14s in — past tx_late_max_ms=8000).
        // Even though parity matches, we're too late for skip-ahead.
        // Defer to next Odd = :45.
        let s = schedule_tx(at(29.0), SlotParity::Odd, 8000, 12_000, SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn ft4_period_targets_7_5s_grid_boundary() {
        // FT4 slot period = 7.5s. On this grid, at(0.0) is Even (slot
        // 235630080 = even), at(7.5) Odd, at(15) Even, at(22.5) Odd.
        // now = :05.0 (Even slot 0 on the FT4 grid). Required = Odd.
        // Current parity Even ≠ Odd → advance to next Odd = :07.5.
        // mstr_relative_to_target = max(0, :05 - :07.5) = 0 < 500 →
        // pad 500ms, cursor 0. The target MUST land on a 7.5s-grid
        // boundary of the requested (Odd) parity — :07.5, NOT :15.
        let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000, FT4_SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 7_500);
        // boundary lands on the 7.5s grid
        assert_eq!(
            (s.target_slot - at(0.0)).num_milliseconds() % 7_500,
            0,
            "target must sit on a 7.5s slot boundary"
        );
        // and it is the requested (Odd) parity
        assert_eq!(
            pancetta_core::slot::SlotParity::of_with_period(s.target_slot, FT4_SLOT_NS),
            SlotParity::Odd
        );
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn ft4_period_late_skips_cursor_in_current_slot() {
        // now = :10.0 (Odd slot at :07.5 on the FT4 grid, 2.5s in).
        // Required = Odd. Current matches; mstr_in_current_slot = 2500
        // ≤ 8000 → target current slot :07.5. mstr_relative_to_target
        // = 2500 > 500 → cursor = 2000ms × SR; no defer to :22.5.
        let s = schedule_tx(at(10.0), SlotParity::Odd, 8000, 12_000, FT4_SLOT_NS);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 7_500);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 2_000 * 12);
    }

    #[test]
    fn ft4_resolve_required_parity_auto_picks_nearest_next_slot() {
        use pancetta_config::station::TxSelfParity;
        // now = :05 (Even slot 0 on FT4 grid). Next Even = :15,
        // next Odd = :07.5. Odd is closer → Auto picks Odd.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0), FT4_SLOT_NS);
        assert_eq!(p, SlotParity::Odd);
    }

    #[test]
    fn coalesce_collect_window_ft8_byte_identical() {
        assert_eq!(
            coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft8),
            COALESCE_COLLECT_WINDOW_MS
        );
    }

    #[test]
    fn coalesce_collect_window_scales_down_for_ft4() {
        // FT4 cycle = 7.5s, half of FT8's 15s → half the wait (400ms).
        assert_eq!(coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft4), 400);
    }

    #[test]
    #[cfg(feature = "ft2")]
    fn coalesce_collect_window_scales_down_for_ft2() {
        // FT2 cycle = 3.2s → 800 * 3.2 / 15 = 170.67, rounds to 171ms.
        assert_eq!(coalesce_collect_window_ms(pancetta_ft8::Protocol::Ft2), 171);
    }

    #[test]
    fn tx_late_max_ms_effective_ft8_byte_identical() {
        assert_eq!(
            tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft8, 8000),
            8000
        );
    }

    #[test]
    fn tx_late_max_ms_effective_scales_down_for_ft4() {
        // FT4 cycle = 7.5s, half of FT8's 15s → half the cap (4000ms).
        assert_eq!(
            tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft4, 8000),
            4000
        );
    }

    #[test]
    #[cfg(feature = "ft2")]
    fn tx_late_max_ms_effective_scales_down_for_ft2() {
        // FT2 cycle = 3.2s → 8000 * 3.2 / 15 = 1706.67, rounds to 1707ms.
        assert_eq!(
            tx_late_max_ms_effective(pancetta_ft8::Protocol::Ft2, 8000),
            1707
        );
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Auto → returns Even
        let p =
            resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Auto, at(5.0), SLOT_NS);
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_explicit_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Odd → tx_parity wins → Even
        let p =
            resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Odd, at(5.0), SLOT_NS);
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_falls_back_to_config_when_none() {
        use pancetta_config::station::TxSelfParity;
        let p = resolve_required_parity(None, TxSelfParity::Even, at(5.0), SLOT_NS);
        assert_eq!(p, SlotParity::Even);
        let p = resolve_required_parity(None, TxSelfParity::Odd, at(5.0), SLOT_NS);
        assert_eq!(p, SlotParity::Odd);
    }

    #[test]
    fn resolve_required_parity_auto_picks_nearest_next_slot() {
        use pancetta_config::station::TxSelfParity;
        // now = :05 (in Even slot 0). Next Even = :30, next Odd = :15.
        // Odd is closer → Auto picks Odd.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0), SLOT_NS);
        assert_eq!(p, SlotParity::Odd);
        // now = :20 (in Odd slot 1). Next Odd = :45, next Even = :30.
        // Even is closer → Auto picks Even.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(20.0), SLOT_NS);
        assert_eq!(p, SlotParity::Even);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_completes_when_no_shutdown() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let interrupted = interruptible_sleep(Duration::from_millis(120), &shutdown, &abort).await;
        assert!(
            !interrupted,
            "should not flag interrupted when both flags stay false"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_returns_immediately_if_already_shutdown() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(true));
        let abort = Arc::new(AtomicBool::new(false));
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        assert!(
            interrupted,
            "must signal interrupted when shutdown is already set"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "should return without sleeping (elapsed={:?})",
            elapsed
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_returns_immediately_if_already_aborted() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(true));
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(60), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        assert!(
            interrupted,
            "must signal interrupted when abort is already set"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "should return without sleeping (elapsed={:?})",
            elapsed
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interruptible_sleep_wakes_within_one_chunk_when_shutdown_fires() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let shutdown = Arc::new(AtomicBool::new(false));
        let abort = Arc::new(AtomicBool::new(false));
        let s2 = shutdown.clone();
        // After 200ms, flip the shutdown flag.
        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            s2.store(true, Ordering::Release);
        });
        let start = std::time::Instant::now();
        let interrupted = interruptible_sleep(Duration::from_secs(30), &shutdown, &abort).await;
        let elapsed = start.elapsed();
        let _ = setter.await;
        assert!(
            interrupted,
            "should signal interrupted when flag flips mid-sleep"
        );
        // Polling chunk is 50ms, so wake latency from flag flip is at most one chunk.
        // Total elapsed ≈ 200ms (when flag flipped) + ≤ 50ms (chunk poll) = ≤ 250ms.
        // Allow 300ms slack for test-runner jitter.
        assert!(
            elapsed < Duration::from_millis(300),
            "wake latency exceeded one chunk (elapsed={:?})",
            elapsed
        );
    }

    /// `current_tx_policy` round-trips the shared atomic. The TuneRequest /
    /// TransmitRequest / MultiTransmitRequest worker arms all hard-mute when
    /// this reads `Disabled`; assert the encoding so a stray atomic value can't
    /// silently un-mute the tune carrier (UX audit Batch 1).
    #[test]
    fn current_tx_policy_reads_disabled_for_tune_mute() {
        use std::sync::atomic::AtomicU8;
        use std::sync::Arc;
        let p = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Disabled.as_u8()));
        assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::Disabled);
        p.store(pancetta_core::TxPolicy::Full.as_u8(), Ordering::Release);
        assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::Full);
        p.store(
            pancetta_core::TxPolicy::RespondOnly.as_u8(),
            Ordering::Release,
        );
        assert_eq!(current_tx_policy(&p), pancetta_core::TxPolicy::RespondOnly);
    }

    /// The worker's drop-decision helper reads the shared active-QSO set
    /// through its RwLock and matches `super::tx_qso_is_live`'s semantics:
    /// live id → transmit, absent id → drop, `None` → always transmit.
    #[test]
    fn worker_tx_qso_is_live_reads_shared_set() {
        use std::collections::HashSet;
        use std::sync::{Arc, RwLock};
        let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        set.write()
            .unwrap()
            .insert(super::super::active_tx_qso_key("qso-live"));

        // Manual / tune (no qso_id) always transmits.
        assert!(super::tx_qso_is_live(None, &set));
        // Live QSO transmits (case-insensitive).
        assert!(super::tx_qso_is_live(Some("QSO-LIVE"), &set));
        // Ended QSO (not in set) is dropped.
        assert!(!super::tx_qso_is_live(Some("qso-ended"), &set));
    }

    /// A poisoned lock fails OPEN — a stuck lock must never silently mute a
    /// legitimate TX. (The operator emergency stop covers the rare worst case.)
    #[test]
    fn worker_tx_qso_is_live_fails_open_on_poison() {
        use std::collections::HashSet;
        use std::sync::{Arc, RwLock};
        let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        // Poison the lock.
        let s2 = set.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = s2.write().unwrap();
            panic!("poison");
        }));
        assert!(set.is_poisoned());
        // Even for a qso_id that would otherwise be "ended", fail-open → true.
        assert!(super::tx_qso_is_live(Some("qso-ended"), &set));
    }

    /// 2026-07-17 fix: the multi-TX key-time gate must require EVERY bundled
    /// item to still be live, not just one — a single still-live item must
    /// NOT be enough to carry a cancelled bundle-mate's stale call onto the
    /// air, since the summed waveform can't be partially re-filtered.
    #[test]
    fn multi_tx_bundle_requires_every_item_live() {
        use std::collections::HashSet;
        use std::sync::{Arc, RwLock};
        let set: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        set.write()
            .unwrap()
            .insert(super::super::active_tx_qso_key("qso-peru"));

        // All live → transmit.
        assert!(super::multi_tx_bundle_still_fully_live(
            &[Some("qso-peru".to_string())],
            &set
        ));

        // One live (Peru), one stale (Kenya, cancelled — not in the set):
        // the operator's exact reported scenario. Must now DROP the whole
        // bundle instead of transmitting Kenya's stale call alongside
        // Peru's live one.
        assert!(!super::multi_tx_bundle_still_fully_live(
            &[Some("qso-kenya".to_string()), Some("qso-peru".to_string())],
            &set
        ));

        // All stale → drop (matches the pre-fix "all ended" case).
        assert!(!super::multi_tx_bundle_still_fully_live(
            &[Some("qso-kenya".to_string())],
            &set
        ));

        // A `None` qso_id (manual/tune item) never gates — mixed with a
        // still-live item, the bundle stays fully live.
        assert!(super::multi_tx_bundle_still_fully_live(
            &[None, Some("qso-peru".to_string())],
            &set
        ));

        // A `None` qso_id mixed with a STALE item: the stale item still
        // fails the gate for the whole bundle.
        assert!(!super::multi_tx_bundle_still_fully_live(
            &[None, Some("qso-kenya".to_string())],
            &set
        ));
    }

    #[test]
    fn pad_and_cursor_for_target_matches_schedule_tx_for_same_now() {
        // Sanity: calling the extracted helper with schedule_tx's own chosen
        // target and "now" must reproduce schedule_tx's own pad/cursor exactly
        // — this is what makes the Task 1 extraction provably behavior-neutral.
        let now = at(5.0);
        let s = schedule_tx(now, SlotParity::Odd, 8000, 12_000, SLOT_NS);
        let (pad, cursor) = pad_and_cursor_for_target(now, s.target_slot, 12_000);
        assert_eq!(pad, s.silent_pad_samples);
        assert_eq!(cursor, s.cursor_offset_samples);
    }

    #[test]
    fn pad_and_cursor_for_target_refreshes_against_a_later_now() {
        // The key new behavior Tasks 3/4 rely on: given the SAME target_slot,
        // a later "now" produces a LARGER cursor (more of the waveform's front
        // trimmed) — because more real time has passed relative to the slot
        // boundary, independent of when target_slot was originally decided.
        let target = at(0.0); // slot boundary itself
        let (pad_early, cursor_early) = pad_and_cursor_for_target(at(0.2), target, 12_000);
        let (pad_late, cursor_late) = pad_and_cursor_for_target(at(3.0), target, 12_000);
        assert!(
            pad_early > 0,
            "200ms in: still inside the DELAY_MS pre-roll, expect padding"
        );
        assert_eq!(pad_late, 0, "3s in: past DELAY_MS, expect no padding");
        assert!(
            cursor_late > cursor_early,
            "later refresh must trim more of the waveform's front"
        );
    }

    #[test]
    fn pad_and_cursor_for_target_stable_within_delay_ms_window() {
        // Two "now" reads a few ms apart, both still inside the DELAY_MS
        // pre-roll, should both land in the padding branch (cursor == 0) —
        // confirms there's no discontinuity right at the DELAY_MS boundary.
        let target = at(10.0);
        let (_, cursor_a) = pad_and_cursor_for_target(at(10.1), target, 12_000);
        let (_, cursor_b) = pad_and_cursor_for_target(at(10.3), target, 12_000);
        assert_eq!(cursor_a, 0);
        assert_eq!(cursor_b, 0);
    }

    fn tx_request(tx_parity: Option<SlotParity>) -> MessageType {
        MessageType::TransmitRequest {
            message_text: "CQ TEST".to_string(),
            frequency_offset: 1500.0,
            qso_id: None,
            tx_parity,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    #[test]
    fn adaptive_cap_shrinks_for_a_late_arriving_head() {
        // Arrives 7.5s into an 8000ms tx_late_max_ms budget: only 500ms of
        // headroom remains before the safety margin (500ms) eats the rest —
        // cap should be at or near zero. at(7.5) falls inside slot 0, which
        // is Even (base timestamp 1767225600 / 15 % 2 == 0, per `at()`'s own
        // doc comment) — so the request must target Even parity for this to
        // be a same-slot late arrival (`use_current = true`) rather than a
        // parity-mismatch defer to the next Odd slot, which would exercise
        // the unrelated `probe.deferred` branch instead of the headroom math
        // this test is for.
        use pancetta_config::station::TxSelfParity;
        let head = tx_request(Some(SlotParity::Even));
        let cap = adaptive_coalesce_cap_ms(
            &head,
            at(7.5),
            TxSelfParity::Auto,
            8000,
            12_000,
            SLOT_NS,
            pancetta_ft8::Protocol::Ft8,
        );
        assert_eq!(cap, 0);
    }

    #[test]
    fn adaptive_cap_has_room_for_an_early_arriving_head() {
        // Arrives 1s into the slot: 7000ms of raw headroom before tx_late_max_ms,
        // well above the protocol ceiling — cap should be the full FT8 ceiling
        // (3000ms), not the raw headroom. at(1.0) falls inside slot 0, which is
        // Even (base timestamp 1767225600 / 15 % 2 == 0, per `at()`'s own doc
        // comment) — so the request must target Even parity for this to be a
        // same-slot early arrival (`use_current = true`, `deferred = false`)
        // and actually exercise `headroom.min(protocol_ceiling)`, rather than
        // parity-mismatch deferring to the next Odd slot and short-circuiting
        // at `if probe.deferred { return protocol_ceiling; }` before that line.
        use pancetta_config::station::TxSelfParity;
        let head = tx_request(Some(SlotParity::Even));
        let cap = adaptive_coalesce_cap_ms(
            &head,
            at(1.0),
            TxSelfParity::Auto,
            8000,
            12_000,
            SLOT_NS,
            pancetta_ft8::Protocol::Ft8,
        );
        assert_eq!(cap, 3000);
    }

    #[test]
    fn adaptive_cap_uses_protocol_ceiling_for_ft4() {
        // Same early-arrival case, but FT4's cycle is half FT8's — the ceiling
        // should scale down proportionally (1500ms), not stay at FT8's 3000ms.
        // at(0.5) on FT4's 7.5s slot grid is also slot 0 = Even, so — same
        // reasoning as adaptive_cap_has_room_for_an_early_arriving_head above
        // — the request must target Even parity to hit `use_current = true`
        // / `deferred = false` and actually exercise
        // `headroom.min(protocol_ceiling)`, instead of deferring to the next
        // Odd slot and short-circuiting before that line.
        use pancetta_config::station::TxSelfParity;
        let head = tx_request(Some(SlotParity::Even));
        let cap = adaptive_coalesce_cap_ms(
            &head,
            at(0.5),
            TxSelfParity::Auto,
            8000,
            12_000,
            FT4_SLOT_NS,
            pancetta_ft8::Protocol::Ft4,
        );
        assert_eq!(cap, 1500);
    }

    #[test]
    fn adaptive_cap_full_ceiling_when_already_deferred() {
        // A head that's already past tx_late_max_ms for the current slot
        // (deferred to the next one) has no current-slot cliff to protect —
        // cap is the full protocol ceiling.
        use pancetta_config::station::TxSelfParity;
        let head = tx_request(Some(SlotParity::Odd));
        let cap = adaptive_coalesce_cap_ms(
            &head,
            at(29.0), // >8000ms into slot 1 (Odd), forces defer
            TxSelfParity::Auto,
            8000,
            12_000,
            SLOT_NS,
            pancetta_ft8::Protocol::Ft8,
        );
        assert_eq!(cap, 3000);
    }

    #[test]
    fn adaptive_cap_zero_for_non_transmit_request() {
        use pancetta_config::station::TxSelfParity;
        let head = MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        let cap = adaptive_coalesce_cap_ms(
            &head,
            at(1.0),
            TxSelfParity::Auto,
            8000,
            12_000,
            SLOT_NS,
            pancetta_ft8::Protocol::Ft8,
        );
        assert_eq!(cap, 0);
    }
}

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
mod tx_failure_diagnostic_tests {
    use super::*;
    use crate::message_bus::MessageBus;

    #[tokio::test]
    async fn emits_a_warn_diagnostic_the_tui_channel_receives() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        emit_tx_failure_diagnostic(
            &bus,
            Some("qso-42"),
            "K5ARH CQ EM12",
            "encode/modulate error (test)",
        )
        .await;

        let msg = receiver
            .try_recv()
            .expect("a DiagnosticEvent should have been sent to the Tui channel");
        match msg.message_type {
            MessageType::DiagnosticEvent {
                target,
                level,
                text,
                qso_id,
                callsign,
            } => {
                assert_eq!(target, "tx.encode");
                assert_eq!(level, pancetta_core::DiagnosticLevel::Warn);
                assert!(text.contains("K5ARH CQ EM12"));
                assert!(text.contains("encode/modulate error (test)"));
                assert_eq!(qso_id.as_deref(), Some("qso-42"));
                assert_eq!(callsign, None);
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tolerates_a_missing_qso_id() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        emit_tx_failure_diagnostic(&bus, None, "CQ K5ARH EM12", "invalid frequency").await;

        let msg = receiver.try_recv().expect("diagnostic should still send");
        match msg.message_type {
            MessageType::DiagnosticEvent { qso_id, .. } => assert_eq!(qso_id, None),
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_diagnostic_full_carries_wide_attribution() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        emit_diagnostic_full(
            &bus,
            ComponentId::Qso,
            "qso.security",
            pancetta_core::DiagnosticLevel::Warn,
            "rejected".into(),
            Some("qso-7"),
            Some("W1AW"),
        )
        .await;
        let msg = receiver.try_recv().expect("diagnostic");
        assert_eq!(msg.source, ComponentId::Qso);
        match msg.message_type {
            MessageType::DiagnosticEvent {
                target,
                qso_id,
                callsign,
                ..
            } => {
                assert_eq!(target, "qso.security");
                assert_eq!(qso_id.as_deref(), Some("qso-7"));
                assert_eq!(callsign.as_deref(), Some("W1AW"));
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_diagnostic_delegates_without_changing_its_own_contract() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        emit_diagnostic(
            &bus,
            "tx.policy",
            pancetta_core::DiagnosticLevel::Info,
            "held".into(),
            None,
        )
        .await;
        let msg = receiver.try_recv().expect("diagnostic");
        assert_eq!(msg.source, ComponentId::Ft8Transmitter);
        match msg.message_type {
            MessageType::DiagnosticEvent { callsign, .. } => assert_eq!(callsign, None),
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    /// `emit_diagnostic` is the generic sibling wired into the 10
    /// `"tx.policy"` sites (observability-diagnostics-plan.md Layer 1) —
    /// unlike `emit_tx_failure_diagnostic`, its `target` and `level` are
    /// caller-supplied. Pins that both vary correctly and independently.
    #[tokio::test]
    async fn emit_diagnostic_carries_caller_supplied_target_and_level() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        emit_diagnostic(
            &bus,
            "tx.policy",
            pancetta_core::DiagnosticLevel::Info,
            "dropping stale TX for ended QSO: 'CQ K5ARH EM12'".to_string(),
            Some("qso-7"),
        )
        .await;

        let msg = receiver.try_recv().expect("diagnostic should send");
        match msg.message_type {
            MessageType::DiagnosticEvent {
                target,
                level,
                text,
                qso_id,
                ..
            } => {
                assert_eq!(target, "tx.policy");
                assert_eq!(level, pancetta_core::DiagnosticLevel::Info);
                assert!(text.contains("CQ K5ARH EM12"));
                assert_eq!(qso_id.as_deref(), Some("qso-7"));
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_diagnostic_full_carries_source_and_callsign() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        emit_diagnostic_full(
            &bus,
            ComponentId::Qso,
            "qso.security",
            pancetta_core::DiagnosticLevel::Warn,
            "rejected frame".into(),
            Some("qso-7"),
            Some("BOGUS9"),
        )
        .await;
        let msg = receiver.try_recv().expect("diagnostic should send");
        assert_eq!(msg.source, ComponentId::Qso);
        match msg.message_type {
            MessageType::DiagnosticEvent {
                target, callsign, ..
            } => {
                assert_eq!(target, "qso.security");
                assert_eq!(callsign.as_deref(), Some("BOGUS9"));
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod supersede_rekey_tests {
    use super::*;
    use crate::message_bus::MessageBus;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// The core Phase-1 re-key decision: a genuinely different superseding
    /// request that can still make this slot must deassert PTT and swap the
    /// working `message_text`/`frequency_offset`/`schedule` in place so the
    /// caller's `'key_and_send` retry re-encodes (Step 1) and re-keys with the
    /// NEW content. Targets the current slot's parity with a >1-slot
    /// `tx_late_max_ms` so the re-key is viable no matter when the test runs.
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_and_rekey_updates_state_when_viable() {
        let bus = MessageBus::new(16).unwrap();
        // Hamlib channel so the helper's PTT-OFF send has a receiver.
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

        let new_request = MessageType::TransmitRequest {
            message_text: "NEW TEXT".to_string(),
            frequency_offset: 1500.0,
            qso_id: Some("qso-1".to_string()),
            tx_parity: Some(cur_parity),
            origin: crate::message_bus::TxOrigin::Local,
        };

        // max_concurrent_qsos == 1 → single-item replace (Task 6 behavior).
        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            &mut message_text,
            &mut frequency_offset,
            &mut schedule,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
            1,
            crate::message_bus::TxOrigin::Local,
            &[],
        )
        .await;

        assert!(
            matches!(outcome, super::SupersedeOutcome::Replace { .. }),
            "re-key targeting the current slot parity with max_concurrent_qsos==1 must Replace"
        );
        // The state swap is what makes the retry re-encode the NEW content:
        // Step 1 re-runs `encode_for_protocol(&message_text)` at the new freq.
        assert_eq!(message_text, "NEW TEXT");
        assert_eq!(frequency_offset, 1500.0);
        assert!(
            !ptt_active.load(Ordering::Acquire),
            "PTT must be deasserted"
        );
        assert!(
            !schedule.deferred,
            "a viable re-key schedule is never deferred"
        );
    }

    /// TASK 9 — the end-to-end audio gap Task 6 explicitly deferred.
    ///
    /// Task 6's `supersede_and_rekey_updates_state_when_viable` proves only the
    /// PRECONDITION of a single-TX supersede: PTT is deasserted and the working
    /// `message_text`/`frequency_offset` are swapped to the new request. It does
    /// NOT prove the crux — that the audio the single-TX arm actually
    /// re-encodes and pushes to the output ring buffer (Step 1 → Step 7)
    /// reflects the NEW message's waveform rather than stale samples from the
    /// aborted one. Task 6's reviewer flagged that as deferred here.
    ///
    /// This test closes it by driving the REAL single-TX supersede helper
    /// (`supersede_and_rekey_or_bundle`, `max_concurrent_qsos == 1` →
    /// `Replace`) over a REAL `MessageBus`, then re-encoding the POST-supersede
    /// working state through `encode_and_modulate` — documented (see its
    /// doc-comment's "FT8 regression guarantee") to be byte-identical to the
    /// arm's Step-1 `encode_for_protocol` + `modulate_for_protocol` sequence for
    /// a given (text, offset). It asserts:
    ///
    ///   1. The abort of the in-flight message A is a REAL
    ///      `SetPtt{false}` observed on a REAL Hamlib channel (not just the
    ///      `ptt_active` flag), and it is the ONLY message on that channel (no
    ///      stray/duplicate keying).
    ///   2. The waveform re-encoded from the mutated working state is
    ///      BIT-IDENTICAL to a fresh encode of B and DIFFERS from A's waveform —
    ///      i.e. the audio the arm's Step 7 would send is B's content, not A's
    ///      stale samples. This is the exact gap Task 6 left open.
    ///   3. That waveform is a full-length FT8 frame (same length as A's), so
    ///      the difference is genuine re-encoded CONTENT, not a
    ///      truncated/empty buffer.
    ///
    /// NOT covered (helper-level scope; see task-9-report.md): PTT going back ON
    /// and the single `TransmitComplete` for B are emitted by the arm's inline
    /// Steps 5/10, which are not separable from `start_transmitter_component`
    /// without a much larger worker harness than this task warrants.
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_rekeys_audio_to_new_message_not_stale() {
        let bus = MessageBus::new(16).unwrap();
        // Real Hamlib channel: the helper's abort PTT-OFF lands here.
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        // Message A is "in flight": these are the working-loop variables the
        // arm's Step 1 would encode into the audio it is currently sending.
        let a_text = "CQ K5ARH EM12";
        let a_freq = 1000.0_f64;
        let mut message_text = a_text.to_string();
        let mut frequency_offset = a_freq;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

        // A genuinely different superseding message B — different text AND
        // frequency, so its encoded waveform is unmistakably distinct from A's.
        let b_text = "W1AW K5ARH R-09";
        let b_freq = 1600.0_f64;
        let new_request = MessageType::TransmitRequest {
            message_text: b_text.to_string(),
            frequency_offset: b_freq,
            qso_id: Some("qso-super".to_string()),
            tx_parity: Some(cur_parity),
            origin: crate::message_bus::TxOrigin::Local,
        };

        // max_concurrent_qsos == 1 → the single-TX arm's `Replace` path.
        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            &mut message_text,
            &mut frequency_offset,
            &mut schedule,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
            1,
            crate::message_bus::TxOrigin::Local,
            &[],
        )
        .await;

        assert!(
            matches!(outcome, super::SupersedeOutcome::Replace { .. }),
            "a viable single-TX supersede must Replace"
        );

        // (1) The abort of A is a REAL SetPtt{false} on the real Hamlib channel.
        assert!(
            !ptt_active.load(Ordering::Acquire),
            "ptt_active flag must be cleared by the abort"
        );
        let ptt_msg = hamlib_rx
            .try_recv()
            .expect("aborting the in-flight message must send a real SetPtt to Hamlib");
        assert!(
            matches!(
                ptt_msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: false
                })
            ),
            "abort must be a real SetPtt{{false}}, got {:?}",
            ptt_msg.message_type
        );
        // Exactly one keying message from the abort — no stray/duplicate PTT.
        assert!(
            hamlib_rx.try_recv().is_err(),
            "the abort emits exactly one SetPtt{{false}} (no duplicate keying)"
        );

        // (2) THE CRUX: encode the POST-supersede working state exactly as the
        // arm's Step 1 would, and prove it is B's waveform, not A's stale one.
        // `arm_encode` reproduces the single-TX arm's Step-1 keying byte for
        // byte: `set_base_frequency(freq)` then `encode_for_protocol` +
        // `modulate_for_protocol(.., 0.0)` (see tx.rs ~2516-2564). Using the
        // arm's real sequence — rather than the `encode_and_modulate` helper,
        // which routes the frequency through the modulator's default base
        // instead of `set_base_frequency` — makes "this is the waveform the arm
        // sends" a faithful claim.
        let protocol = pancetta_ft8::Protocol::Ft8;
        let arm_encode = |text: &str, freq: f64| -> Vec<f32> {
            let mut encoder = Ft8Encoder::new();
            let mut modulator = Ft8Modulator::new_default().expect("modulator");
            modulator
                .set_base_frequency(freq)
                .expect("set base frequency");
            let symbols =
                super::encode_for_protocol(&mut encoder, protocol, text).expect("encode symbols");
            super::modulate_for_protocol(&mut modulator, protocol, &symbols, 0.0).expect("modulate")
        };

        let rekeyed_audio = arm_encode(&message_text, frequency_offset);
        let fresh_b = arm_encode(b_text, b_freq);
        let fresh_a = arm_encode(a_text, a_freq);

        assert_eq!(
            rekeyed_audio, fresh_b,
            "the audio the arm's Step 7 would push must be B's freshly-encoded \
             waveform (the working state re-encodes to the NEW message)"
        );
        assert_ne!(
            rekeyed_audio, fresh_a,
            "the audio must NOT be A's stale waveform — this is the exact \
             end-to-end gap Task 6 deferred to Task 9"
        );

        // (3) The re-encode is a genuine full FT8 frame, not truncated/empty:
        // same length as A's frame, so the mismatch above is CONTENT, not size.
        assert!(
            !rekeyed_audio.is_empty(),
            "re-encoded audio must be a real, non-empty frame"
        );
        assert_eq!(
            rekeyed_audio.len(),
            fresh_a.len(),
            "same full FT8-frame length as A — the A/B waveform difference is \
             re-encoded content, not a truncated or empty buffer"
        );
    }

    /// With `max_concurrent_qsos > 1`, a viable supersede folds the new
    /// request into a bundle alongside the in-flight item: the outcome is
    /// `Bundle` carrying both items (in-flight first, new item last), PTT is
    /// deasserted, and the working state is ALSO mutated to the new item (the
    /// single-TX caller's collision fallback).
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_returns_bundle_when_concurrent() {
        let bus = MessageBus::new(16).unwrap();
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let now = chrono::Utc::now();
        let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
            pancetta_core::slot::current_slot_start_with_period(now, pancetta_core::slot::SLOT_NS),
            pancetta_core::slot::SLOT_NS,
        );

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            now,
            cur_parity,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

        let in_flight = [crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];

        let new_request = MessageType::TransmitRequest {
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1400.0,
            qso_id: Some("qso-2".to_string()),
            tx_parity: Some(cur_parity),
            origin: crate::message_bus::TxOrigin::Local,
        };

        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            &mut message_text,
            &mut frequency_offset,
            &mut schedule,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            now,
            2,
            crate::message_bus::TxOrigin::Local,
            &in_flight,
        )
        .await;

        match outcome {
            super::SupersedeOutcome::Bundle { items, .. } => {
                assert_eq!(items.len(), 2, "bundle = in-flight + new item");
                assert_eq!(items[0].message_text, "KA1ABC K5ARH R-15");
                assert_eq!(items[0].frequency_offset, 1000.0);
                assert_eq!(items[1].message_text, "CQ K5ARH EM12");
                assert_eq!(items[1].frequency_offset, 1400.0);
                assert_eq!(items[1].qso_id.as_deref(), Some("qso-2"));
            }
            other => panic!("expected Bundle, got {other:?}"),
        }
        assert!(
            !ptt_active.load(Ordering::Acquire),
            "PTT must be deasserted"
        );
        // Working state also mutated to the new item (collision fallback).
        assert_eq!(message_text, "CQ K5ARH EM12");
        assert_eq!(frequency_offset, 1400.0);
    }

    /// C1 REGRESSION: the `Replace` outcome must carry the SUPERSEDING request's
    /// OWN `origin`, independent of the aborted in-flight transmission's origin.
    ///
    /// The single-TX arm re-keys a `Replace` in place (`continue 'key_and_send`)
    /// and re-runs Step 4b-arm's key-time arm gate against its `origin` binding.
    /// Before the fix the superseding request's origin was discarded (`..`) and
    /// the arm reused the ORIGINAL loop origin, so:
    ///   - a `Remote` request superseding a `Local` in-flight frame re-keyed
    ///     under the stale `Local` origin and SKIPPED the arm gate entirely
    ///     (remote content transmitted with zero arm gating — the C1 leak), and
    ///   - a `Local` request superseding a `Remote` in-flight frame kept gating
    ///     a purely local frame (wrongly droppable if the arm went stale).
    ///
    /// Proving `Replace{origin}` equals the NEW request's origin in BOTH
    /// directions is exactly what closes both failures: the arm now re-gates and
    /// emits the re-keyed frame under the origin of the frame actually going out.
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_replace_carries_superseding_requests_origin() {
        use crate::message_bus::TxOrigin;

        // (in-flight origin, superseding request origin) — both directions.
        for (in_flight_origin, new_origin) in [
            (TxOrigin::Local, TxOrigin::Remote),
            (TxOrigin::Remote, TxOrigin::Local),
        ] {
            let bus = MessageBus::new(16).unwrap();
            let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

            let now = chrono::Utc::now();
            let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
                pancetta_core::slot::current_slot_start_with_period(
                    now,
                    pancetta_core::slot::SLOT_NS,
                ),
                pancetta_core::slot::SLOT_NS,
            );

            let mut message_text = "OLD TEXT".to_string();
            let mut frequency_offset = 1000.0;
            let mut schedule = super::schedule_tx(
                now,
                cur_parity,
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
            );
            let ptt_active = Arc::new(AtomicBool::new(true));
            let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

            let new_request = MessageType::TransmitRequest {
                message_text: "NEW TEXT".to_string(),
                frequency_offset: 1500.0,
                qso_id: Some("qso-new".to_string()),
                tx_parity: Some(cur_parity),
                origin: new_origin,
            };

            // max_concurrent_qsos == 1 → the single-TX arm's Replace path.
            let outcome = super::supersede_and_rekey_or_bundle(
                new_request,
                &mut message_text,
                &mut frequency_offset,
                &mut schedule,
                &bus,
                &ptt_active,
                &last_ptt_on_ms,
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
                pancetta_config::station::TxSelfParity::Auto,
                now,
                1,
                in_flight_origin,
                &[],
            )
            .await;

            match outcome {
                super::SupersedeOutcome::Replace { origin, qso_id } => {
                    assert_eq!(
                        origin, new_origin,
                        "Replace must carry the SUPERSEDING request's origin \
                         (in_flight={in_flight_origin:?}, new={new_origin:?}), not the \
                         aborted transmission's — otherwise the key-time arm gate is \
                         evaluated against the wrong origin"
                    );
                    // PAN-38 round 4 (Codex): Replace must ALSO carry the
                    // superseding request's own qso_id, not the aborted
                    // in-flight transmission's — otherwise the caller's
                    // in-place retry pairs the new frame's text with the
                    // WRONG QSO's id, misattributing its eventual
                    // TransmitComplete.
                    assert_eq!(
                        qso_id,
                        Some("qso-new".to_string()),
                        "Replace must carry the SUPERSEDING request's own qso_id"
                    );
                }
                other => panic!("expected Replace, got {other:?}"),
            }
        }
    }

    /// C1 CONSISTENCY: a `Bundle` fold must be gated if EITHER the in-flight
    /// stream or the superseding request is `Remote` (fail-safe fold), while the
    /// separately-returned `new_origin` (used by the caller's frequency-collision
    /// fallback, which transmits ONLY the new item) tracks the new request's own
    /// origin. Mirrors the coalescer's fail-safe origin fold so a Remote item
    /// folded onto a Local in-flight bundle can never re-enter ungated.
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_bundle_folds_origin_failsafe() {
        use crate::message_bus::TxOrigin;

        // (in_flight, new) -> (expected bundle_origin, expected new_origin)
        let cases = [
            (
                TxOrigin::Local,
                TxOrigin::Remote,
                TxOrigin::Remote,
                TxOrigin::Remote,
            ),
            (
                TxOrigin::Remote,
                TxOrigin::Local,
                TxOrigin::Remote,
                TxOrigin::Local,
            ),
            (
                TxOrigin::Local,
                TxOrigin::Local,
                TxOrigin::Local,
                TxOrigin::Local,
            ),
            (
                TxOrigin::Remote,
                TxOrigin::Remote,
                TxOrigin::Remote,
                TxOrigin::Remote,
            ),
        ];

        for (in_flight_origin, new_origin, want_bundle, want_new) in cases {
            let bus = MessageBus::new(16).unwrap();
            let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

            let now = chrono::Utc::now();
            let cur_parity = pancetta_core::slot::SlotParity::of_with_period(
                pancetta_core::slot::current_slot_start_with_period(
                    now,
                    pancetta_core::slot::SLOT_NS,
                ),
                pancetta_core::slot::SLOT_NS,
            );

            let mut message_text = "OLD TEXT".to_string();
            let mut frequency_offset = 1000.0;
            let mut schedule = super::schedule_tx(
                now,
                cur_parity,
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
            );
            let ptt_active = Arc::new(AtomicBool::new(true));
            let last_ptt_on_ms = Arc::new(AtomicU64::new(0));
            let in_flight = [crate::message_bus::TransmitRequestItem {
                message_text: "KA1ABC K5ARH R-15".to_string(),
                frequency_offset: 1000.0,
                qso_id: Some("qso-1".to_string()),
            }];

            let new_request = MessageType::TransmitRequest {
                message_text: "CQ K5ARH EM12".to_string(),
                frequency_offset: 1400.0,
                qso_id: Some("qso-2".to_string()),
                tx_parity: Some(cur_parity),
                origin: new_origin,
            };

            let outcome = super::supersede_and_rekey_or_bundle(
                new_request,
                &mut message_text,
                &mut frequency_offset,
                &mut schedule,
                &bus,
                &ptt_active,
                &last_ptt_on_ms,
                20_000,
                12_000,
                pancetta_core::slot::SLOT_NS,
                pancetta_config::station::TxSelfParity::Auto,
                now,
                2,
                in_flight_origin,
                &in_flight,
            )
            .await;

            match outcome {
                super::SupersedeOutcome::Bundle {
                    bundle_origin,
                    new_origin: got_new,
                    ..
                } => {
                    assert_eq!(
                        bundle_origin, want_bundle,
                        "bundle_origin fail-safe fold wrong for \
                         in_flight={in_flight_origin:?}, new={new_origin:?}"
                    );
                    assert_eq!(
                        got_new, want_new,
                        "new_origin must track the superseding request's own origin"
                    );
                }
                other => panic!("expected Bundle, got {other:?}"),
            }
        }
    }

    /// A `MultiTransmitRequest` arriving AS the superseding message isn't
    /// re-keyable content — it must report `NotViable` and leave the working
    /// text untouched (no schedule mutation).
    #[tokio::test(flavor = "current_thread")]
    async fn supersede_declines_multi_transmit_request() {
        let bus = MessageBus::new(16).unwrap();
        let (_hamlib_tx, _hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();

        let mut message_text = "OLD TEXT".to_string();
        let mut frequency_offset = 1000.0;
        let mut schedule = super::schedule_tx(
            chrono::Utc::now(),
            pancetta_core::slot::SlotParity::Even,
            8000,
            12_000,
            pancetta_core::slot::SLOT_NS,
        );
        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

        let new_request = MessageType::MultiTransmitRequest {
            items: Vec::new(),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };

        let outcome = super::supersede_and_rekey_or_bundle(
            new_request,
            &mut message_text,
            &mut frequency_offset,
            &mut schedule,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            8000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
            2,
            crate::message_bus::TxOrigin::Local,
            &[],
        )
        .await;

        assert!(matches!(outcome, super::SupersedeOutcome::NotViable));
        assert_eq!(
            message_text, "OLD TEXT",
            "a declined supersede leaves the working text untouched"
        );
        // Even on the wrong-type NotViable path, PTT must be deasserted (the
        // stuck-PTT bug: PTT-off used to sit AFTER the destructure and never
        // ran here). This is what makes the multi-TX arm's unconditional
        // `ptt_guard.disarm()` safe.
        assert!(
            !ptt_active.load(Ordering::Acquire),
            "PTT must be deasserted even when the supersede is declined"
        );
    }

    /// End-to-end regression for the multi-TX arm's supersede integration and
    /// the stuck-PTT bug it had: when a `MultiTransmitRequest` supersedes an
    /// in-flight multi-TX bundle, `supersede_multi_reenqueue` must (1) actually
    /// deassert PTT — both the `ptt_active` flag AND a real `SetPtt{false}`
    /// message on the Hamlib channel (the multi-TX arm's `ptt_guard.disarm()`
    /// otherwise leaves the rig physically keyed) — and (2) re-enqueue the
    /// superseding bundle rather than silently dropping it.
    #[tokio::test(flavor = "current_thread")]
    async fn multi_reenqueue_deasserts_ptt_and_reenqueues_superseding_bundle() {
        let bus = MessageBus::new(16).unwrap();
        // Hamlib channel receives the PTT-OFF; Ft8Transmitter channel receives
        // the worker's re-enqueue-to-self.
        let (_hamlib_tx, hamlib_rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        let (_tx_tx, tx_rx) = bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await
            .unwrap();

        let mut encoder = super::Ft8Encoder::new();
        let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);

        // The bundle currently on the air.
        let in_flight = [crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];

        // The superseding message is itself a MultiTransmitRequest — the routine
        // concurrent-QSO path the autonomous engine enqueues every slot.
        let superseding = MessageType::MultiTransmitRequest {
            items: vec![
                crate::message_bus::TransmitRequestItem {
                    message_text: "JA1XYZ K5ARH 73".to_string(),
                    frequency_offset: 1300.0,
                    qso_id: Some("qso-2".to_string()),
                },
                crate::message_bus::TransmitRequestItem {
                    message_text: "CQ K5ARH EM12".to_string(),
                    frequency_offset: 1700.0,
                    qso_id: None,
                },
            ],
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };

        let ptt_active = Arc::new(AtomicBool::new(true));
        let last_ptt_on_ms = Arc::new(AtomicU64::new(0));

        super::supersede_multi_reenqueue(
            superseding,
            &in_flight,
            crate::message_bus::TxOrigin::Local,
            &mut encoder,
            pancetta_ft8::Protocol::Ft8,
            &tx_params,
            &bus,
            &ptt_active,
            &last_ptt_on_ms,
            20_000,
            12_000,
            pancetta_core::slot::SLOT_NS,
            pancetta_config::station::TxSelfParity::Auto,
            chrono::Utc::now(),
            2,
        )
        .await;

        // (1a) The in-memory flag is cleared.
        assert!(
            !ptt_active.load(Ordering::Acquire),
            "ptt_active must be cleared"
        );
        // (1b) A real SetPtt{false} was sent to Hamlib — this is the assertion
        // that would have caught the stuck-PTT bug (the flag alone did not).
        let ptt_msg = hamlib_rx
            .try_recv()
            .expect("a PTT-OFF message must be sent to Hamlib");
        assert!(
            matches!(
                ptt_msg.message_type,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: false
                })
            ),
            "expected SetPtt{{false}}, got {:?}",
            ptt_msg.message_type
        );

        // (2) The superseding bundle was re-enqueued to the worker, not dropped.
        let reenqueued = tx_rx
            .try_recv()
            .expect("the superseding MultiTransmitRequest must be re-enqueued");
        match reenqueued.message_type {
            MessageType::MultiTransmitRequest { items, .. } => {
                assert_eq!(items.len(), 2, "the whole superseding bundle is preserved");
                assert_eq!(items[0].message_text, "JA1XYZ K5ARH 73");
                assert_eq!(items[1].message_text, "CQ K5ARH EM12");
            }
            other => panic!("expected re-enqueued MultiTransmitRequest, got {other:?}"),
        }
        // Exactly one thing re-enqueued (no duplicate transmission).
        assert!(
            tx_rx.try_recv().is_err(),
            "no second re-enqueue — the in-flight bundle is abandoned, not duplicated"
        );
    }

    /// Grounding check (brief Step 1/2): the pre-existing multi-TX encode path
    /// that bundle-add reuses succeeds when two items are well clear of the
    /// ~75 Hz FT8 minimum separation, yielding a 2-item summed waveform.
    #[test]
    fn bundle_add_succeeds_when_frequencies_are_well_separated() {
        let mut encoder = super::Ft8Encoder::new();
        let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
        let in_flight = [crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];
        let new_item = crate::message_bus::TransmitRequestItem {
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1400.0, // 400 Hz away — well clear of the ~75 Hz minimum
            qso_id: None,
        };
        let bundled: Vec<_> = in_flight
            .iter()
            .cloned()
            .chain(std::iter::once(new_item))
            .collect();
        let outcome = super::encode_and_modulate_multi_tx(
            &mut encoder,
            pancetta_ft8::Protocol::Ft8,
            &tx_params,
            &bundled,
        );
        assert!(
            outcome.samples.is_ok(),
            "expected bundle-add to succeed: {:?}",
            outcome.samples
        );
        assert_eq!(outcome.encoded_items.len(), 2);
    }

    /// Bundle-add falls back (encode error) when the new item collides on
    /// frequency with the in-flight item (inside the ~75 Hz minimum) — this is
    /// the collision the caller detects to fall back to a single-item replace.
    #[test]
    fn bundle_add_falls_back_when_frequencies_collide() {
        let mut encoder = super::Ft8Encoder::new();
        let tx_params = pancetta_ft8::ProtocolParams::from_protocol(pancetta_ft8::Protocol::Ft8);
        let in_flight = [crate::message_bus::TransmitRequestItem {
            message_text: "KA1ABC K5ARH R-15".to_string(),
            frequency_offset: 1000.0,
            qso_id: Some("qso-1".to_string()),
        }];
        let new_item = crate::message_bus::TransmitRequestItem {
            message_text: "CQ K5ARH EM12".to_string(),
            frequency_offset: 1010.0, // 10 Hz away — well inside the ~75 Hz minimum, must collide
            qso_id: None,
        };
        let bundled: Vec<_> = in_flight
            .iter()
            .cloned()
            .chain(std::iter::once(new_item))
            .collect();
        let outcome = super::encode_and_modulate_multi_tx(
            &mut encoder,
            pancetta_ft8::Protocol::Ft8,
            &tx_params,
            &bundled,
        );
        assert!(
            outcome.samples.is_err(),
            "expected the frequency collision to be rejected"
        );
    }
}

#[cfg(test)]
mod tx_frame_logged_tests {
    use super::*;
    use crate::message_bus::{ComponentId, MessageBus, MessageType};
    use chrono::TimeZone;

    #[tokio::test]
    async fn log_tx_frame_emits_the_given_fields_and_timestamp() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        // A late-but-viable key: the caller passes the ACTUAL audio-start
        // instant, which can be well past the nominal slot boundary — this
        // must round-trip untouched, not get snapped to any slot grid.
        let actual_audio_start = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 22).unwrap();
        log_tx_frame(
            &bus,
            "K5ARH JA1ABC -12".to_string(),
            1500.0,
            Some("qso-1".to_string()),
            actual_audio_start,
        )
        .await;

        let msg = receiver
            .try_recv()
            .expect("a TxFrameLogged message should have been sent");
        match msg.message_type {
            MessageType::TxFrameLogged {
                text,
                freq_hz,
                qso_id,
                timestamp,
            } => {
                assert_eq!(text, "K5ARH JA1ABC -12");
                assert_eq!(freq_hz, 1500.0);
                assert_eq!(qso_id.as_deref(), Some("qso-1"));
                assert_eq!(timestamp, actual_audio_start);
            }
            other => panic!("expected TxFrameLogged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cq_frame_qso_id_is_none() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        log_tx_frame(
            &bus,
            "CQ K5ARH EM10".to_string(),
            1200.0,
            None,
            chrono::Utc::now(),
        )
        .await;

        let msg = receiver
            .try_recv()
            .expect("TxFrameLogged should send for CQ too");
        match msg.message_type {
            MessageType::TxFrameLogged { qso_id, .. } => assert_eq!(qso_id, None),
            other => panic!("expected TxFrameLogged, got {other:?}"),
        }
    }

    /// `send_tx_queue_status` must never emit `TxFrameLogged` itself, for
    /// either a NOW-SENDING or an idle-clear push — Band Activity's history
    /// timestamp comes only from Step 7's `log_tx_frame` call, made once the
    /// actual audio-start instant is known (see `log_tx_frame`'s doc
    /// comment). Guards against re-coupling the two and reintroducing the
    /// stale-timestamp bug this decoupling fixed.
    #[tokio::test]
    async fn send_tx_queue_status_never_emits_tx_frame_logged() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        send_tx_queue_status(
            &bus,
            Some(crate::message_bus::TxItem {
                text: "K5ARH JA1ABC -12".to_string(),
                freq_hz: 1500.0,
                qso_id: Some("qso-1".to_string()),
                deferred: false,
            }),
            Vec::new(),
        )
        .await;
        let msg = receiver.try_recv().expect("TxQueueStatus should send");
        assert!(matches!(
            msg.message_type,
            MessageType::TxQueueStatus {
                sending: Some(_),
                ..
            }
        ));
        assert!(
            receiver.try_recv().is_err(),
            "no TxFrameLogged from a NOW-SENDING push"
        );

        send_tx_queue_status(&bus, None, Vec::new()).await;
        let msg2 = receiver
            .try_recv()
            .expect("TxQueueStatus should still send");
        assert!(matches!(
            msg2.message_type,
            MessageType::TxQueueStatus { sending: None, .. }
        ));
        assert!(
            receiver.try_recv().is_err(),
            "no TxFrameLogged from an idle-clear push"
        );
    }
}

#[cfg(test)]
mod coalesce_tests {
    use super::*;
    use std::collections::HashSet;

    fn entry(text: &str, qso_id: Option<&str>) -> CoalesceEntry {
        CoalesceEntry {
            message_text: text.to_string(),
            frequency_offset: 1000.0,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    /// Like [`entry`] but with an explicit `frequency_offset` — used by tests
    /// that expect ≥2 entries to survive together in `retained` (the
    /// FQ-F4/TX-F6 frequency-separation exclusion pass would otherwise treat
    /// same-frequency retained entries as a bundle-frequency conflict, since
    /// `entry`'s default 1000.0 Hz is shared by every plain `entry(...)`
    /// call in this module).
    fn entry_freq(text: &str, qso_id: Option<&str>, freq: f64) -> CoalesceEntry {
        CoalesceEntry {
            message_text: text.to_string(),
            frequency_offset: freq,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    /// Predicate over a fixed live-set (uppercased+trimmed to match the
    /// production canonicalization). `None` is always live.
    fn live_in(set: &HashSet<String>) -> impl FnMut(Option<&str>) -> bool + '_ {
        move |id: Option<&str>| match id {
            None => true,
            Some(id) => set.contains(&super::super::active_tx_qso_key(id)),
        }
    }

    fn liveset(ids: &[&str]) -> HashSet<String> {
        ids.iter()
            .map(|s| super::super::active_tx_qso_key(s))
            .collect()
    }

    #[test]
    fn single_request_passthrough_unchanged() {
        // The no-backlog case: one request in, one retained out, zero reduced.
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(vec![entry("CQ", Some("qso-a"))], live_in(&live));
        assert_eq!(out.retained, vec![entry("CQ", Some("qso-a"))]);
        assert_eq!(out.coalesced, 0);
        assert_eq!(out.dropped_terminal, 0);
        assert_eq!(out.truncated, 0);
        assert!(out.is_noop());
    }

    #[test]
    fn newest_per_qso_id_wins() {
        // Three keep-calls for the SAME live QSO collapse to the LAST one.
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(
            vec![
                entry("OLD-1", Some("qso-a")),
                entry("OLD-2", Some("qso-a")),
                entry("NEWEST", Some("qso-a")),
            ],
            live_in(&live),
        );
        assert_eq!(out.retained, vec![entry("NEWEST", Some("qso-a"))]);
        assert_eq!(out.coalesced, 2);
        assert_eq!(out.dropped_terminal, 0);
        assert_eq!(out.truncated, 0);
    }

    #[test]
    fn newest_per_qso_id_is_case_insensitive() {
        // Mixed-case ids for the same QSO coalesce together (canonical key).
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(
            vec![entry("OLD", Some("QSO-A")), entry("NEWEST", Some("qso-a"))],
            live_in(&live),
        );
        assert_eq!(out.retained.len(), 1);
        assert_eq!(out.retained[0].message_text, "NEWEST");
        assert_eq!(out.coalesced, 1);
    }

    #[test]
    fn terminal_qso_requests_dropped() {
        // qso-dead is not in the live set → its requests are dropped; the live
        // QSO survives.
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(
            vec![
                entry("DEAD-1", Some("qso-dead")),
                entry("LIVE", Some("qso-a")),
                entry("DEAD-2", Some("qso-dead")),
            ],
            live_in(&live),
        );
        assert_eq!(out.retained, vec![entry("LIVE", Some("qso-a"))]);
        assert_eq!(out.dropped_terminal, 2);
        assert_eq!(out.coalesced, 0);
    }

    #[test]
    fn manual_none_entries_preserved_and_never_coalesced() {
        // Two distinct manual sends (qso_id == None) must BOTH survive — they
        // are never coalesced into each other, and never gated by liveness.
        // Distinct, well-separated frequencies so the FQ-F4/TX-F6 bundle-
        // frequency check (a SEPARATE reduction) doesn't also fire here.
        let live = liveset(&[]); // empty: no QSO is "live"
        let out = coalesce_transmit_requests(
            vec![
                entry_freq("MANUAL-1", None, 1000.0),
                entry_freq("MANUAL-2", None, 3000.0),
            ],
            live_in(&live),
        );
        assert_eq!(out.retained.len(), 2);
        assert_eq!(out.retained[0].message_text, "MANUAL-1");
        assert_eq!(out.retained[1].message_text, "MANUAL-2");
        assert_eq!(out.coalesced, 0);
        assert_eq!(out.dropped_terminal, 0);
    }

    #[test]
    fn manual_send_survives_keepcall_flood() {
        // A flood of keep-calls for one QSO plus an operator manual send: the
        // QSO collapses to its newest frame, and the manual send is retained.
        // MANUAL is on a well-separated frequency so the FQ-F4/TX-F6 bundle-
        // frequency check (a SEPARATE reduction) doesn't also fire here.
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(
            vec![
                entry("KC-1", Some("qso-a")),
                entry("KC-2", Some("qso-a")),
                entry_freq("MANUAL", None, 3000.0),
                entry("KC-3", Some("qso-a")),
            ],
            live_in(&live),
        );
        // QSO stream first (first-seen), manual last.
        assert_eq!(out.retained.len(), 2);
        assert_eq!(out.retained[0].message_text, "KC-3"); // newest for qso-a
        assert_eq!(out.retained[1].message_text, "MANUAL");
        assert_eq!(out.coalesced, 2);
    }

    #[test]
    fn cap_enforced_with_truncation_count() {
        // More distinct live streams than the cap → truncated to the cap,
        // first-seen streams kept, overflow counted. Distinct, well-separated
        // frequencies (100 Hz apart) so the FQ-F4/TX-F6 bundle-frequency
        // check (a SEPARATE reduction from the cap) doesn't also fire here —
        // this test is purely about the cap/truncation count.
        let ids: Vec<String> = (0..MAX_RETAINED_TX_STREAMS + 3)
            .map(|i| format!("qso-{i}"))
            .collect();
        let live: HashSet<String> = ids
            .iter()
            .map(|s| super::super::active_tx_qso_key(s))
            .collect();
        let drained: Vec<CoalesceEntry> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| entry_freq(id, Some(id), 1000.0 + i as f64 * 100.0))
            .collect();
        let out = coalesce_transmit_requests(drained, live_in(&live));
        assert_eq!(out.retained.len(), MAX_RETAINED_TX_STREAMS);
        assert_eq!(out.truncated, 3);
        // First-seen streams kept (FIFO fairness): qso-0..qso-7.
        assert_eq!(out.retained[0].message_text, "qso-0");
        assert_eq!(
            out.retained[MAX_RETAINED_TX_STREAMS - 1].message_text,
            format!("qso-{}", MAX_RETAINED_TX_STREAMS - 1)
        );
    }

    #[test]
    fn distinct_live_qsos_all_retained_under_cap() {
        // Two distinct live QSOs, one frame each, under cap → both retained,
        // nothing reduced (is_noop). Distinct, well-separated frequencies so
        // the FQ-F4/TX-F6 bundle-frequency check doesn't also fire.
        let live = liveset(&["qso-a", "qso-b"]);
        let out = coalesce_transmit_requests(
            vec![
                entry("A", Some("qso-a")),
                entry_freq("B", Some("qso-b"), 3000.0),
            ],
            live_in(&live),
        );
        assert_eq!(out.retained.len(), 2);
        assert!(out.is_noop());
    }

    #[test]
    fn empty_input_yields_empty_retained() {
        let live = liveset(&[]);
        let out = coalesce_transmit_requests(Vec::new(), live_in(&live));
        assert!(out.retained.is_empty());
        assert!(out.is_noop());
    }

    #[test]
    fn all_terminal_yields_empty_retained_with_drop_count() {
        let live = liveset(&[]); // nothing live
        let out = coalesce_transmit_requests(
            vec![entry("D1", Some("qso-x")), entry("D2", Some("qso-y"))],
            live_in(&live),
        );
        assert!(out.retained.is_empty());
        assert_eq!(out.dropped_terminal, 2);
        assert!(!out.is_noop());
    }

    // ── Bundle-parity conflict exclusion (Batch 2, part 2) ───────────────────

    fn entry_with_parity(
        text: &str,
        qso_id: Option<&str>,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
    ) -> CoalesceEntry {
        CoalesceEntry {
            message_text: text.to_string(),
            frequency_offset: 1000.0,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    /// Like [`entry_with_parity`] but with an explicit `frequency_offset` —
    /// see [`entry_freq`]'s doc comment for why some multi-entry tests need
    /// distinct frequencies now that the FQ-F4/TX-F6 bundle-frequency
    /// exclusion pass is a separate reduction alongside the parity one.
    fn entry_with_parity_freq(
        text: &str,
        qso_id: Option<&str>,
        tx_parity: Option<pancetta_core::slot::SlotParity>,
        freq: f64,
    ) -> CoalesceEntry {
        CoalesceEntry {
            message_text: text.to_string(),
            frequency_offset: freq,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    /// Three distinct-QSO streams survive coalescing: two share `Some(Even)`
    /// and one is `Some(Odd)`. The bundle must NOT silently coerce the Odd
    /// outlier onto the Even anchor (that would put it in the wrong slot
    /// window for its actual partner) — it is excluded from `retained` for
    /// this cycle and counted in `parity_excluded`, keeping only the two
    /// agreeing Even entries.
    #[test]
    fn disagreeing_parity_stream_is_excluded_not_coerced() {
        use pancetta_core::slot::SlotParity;

        // A and B (both Even, both expected to survive together) get
        // distinct, well-separated frequencies so the FQ-F4/TX-F6
        // bundle-frequency check (a separate reduction) doesn't also fire.
        // C's frequency doesn't matter — it's excluded by the parity check
        // before the frequency pass ever sees it.
        let live = liveset(&["qso-a", "qso-b", "qso-c"]);
        let out = coalesce_transmit_requests(
            vec![
                entry_with_parity("A", Some("qso-a"), Some(SlotParity::Even)),
                entry_with_parity_freq("B", Some("qso-b"), Some(SlotParity::Even), 3000.0),
                entry_with_parity("C", Some("qso-c"), Some(SlotParity::Odd)),
            ],
            live_in(&live),
        );
        assert_eq!(
            out.retained.len(),
            2,
            "the Odd outlier must be excluded, keeping only the two Even entries"
        );
        assert_eq!(out.retained[0].message_text, "A");
        assert_eq!(out.retained[1].message_text, "B");
        assert!(
            out.retained
                .iter()
                .all(|e| e.tx_parity == Some(SlotParity::Even)),
            "every surviving entry must agree with the bundle anchor"
        );
        assert_eq!(
            out.parity_excluded, 1,
            "exactly one stream excluded for a parity conflict"
        );
        assert!(
            !out.is_noop(),
            "a parity exclusion must not be reported as a no-op"
        );
    }

    /// A `None` (no-preference) `tx_parity` must NOT be treated as
    /// disagreeing with a concrete anchor — it rides along unchanged, exactly
    /// like the pre-existing manual/None-keyed behavior.
    #[test]
    fn none_parity_stream_is_not_treated_as_a_conflict() {
        use pancetta_core::slot::SlotParity;

        // Distinct, well-separated frequencies so the FQ-F4/TX-F6
        // bundle-frequency check doesn't also fire — this test is purely
        // about parity, and both entries are expected to survive together.
        let live = liveset(&["qso-a", "qso-b"]);
        let out = coalesce_transmit_requests(
            vec![
                entry_with_parity("A", Some("qso-a"), Some(SlotParity::Even)),
                entry_with_parity_freq("B", Some("qso-b"), None, 3000.0),
            ],
            live_in(&live),
        );
        assert_eq!(
            out.retained.len(),
            2,
            "a None-parity stream must not be excluded"
        );
        assert_eq!(out.parity_excluded, 0);
        assert!(out.is_noop());
    }
}

#[cfg(test)]
mod protocol_tx_tests {
    //! Coordinator-level FT4 TX coverage (final-review blocker).
    //!
    //! Before this wiring the TX worker hardcoded FT8 encode+modulate, so in
    //! `[rig].mode = "FT4"` the station DECODED FT4 but TRANSMITTED an FT8
    //! waveform (8-GFSK, 79 symbols, 12.64s) onto the 7.5s grid — FT4 QSOs
    //! could never complete. These tests pin the branch: FT4 emits the FT4
    //! waveform (4-GFSK, 105 symbols, ~5.04s / 60_480 samples @ 12 kHz), and
    //! the FT8 branch stays byte-identical to the legacy encode+modulate path.
    use super::*;
    use pancetta_ft8::{Ft8Encoder, Ft8Modulator, Protocol, ProtocolParams};

    /// The message used across the protocol-branch tests. A standard grid
    /// message so both FT8 and FT4 encode via `try_encode_standard`.
    const MSG: &str = "CQ K5ARH EM12";

    #[test]
    fn ft4_encode_produces_105_symbols_not_79() {
        let mut enc = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        let symbols =
            encode_for_protocol(&mut enc, Protocol::Ft4, MSG).expect("FT4 encode should succeed");
        assert_eq!(
            symbols.len(),
            105,
            "FT4 must emit 105 symbols (4-GFSK), not FT8's 79"
        );
        // FT4 is 4-GFSK: every symbol must be in 0..=3.
        assert!(
            symbols.iter().all(|&s| s < 4),
            "FT4 symbols must be 4-ary (0-3): {symbols:?}"
        );
    }

    #[test]
    fn ft4_waveform_is_ft4_length_not_ft8() {
        // End-to-end encode+modulate for FT4 through the extracted helper.
        let samples = encode_and_modulate(Protocol::Ft4, MSG, 0.0)
            .expect("FT4 encode+modulate should succeed");
        // FT4: 105 symbols × 0.048s × 12_000 Hz = 60_480 samples (~5.04s).
        assert_eq!(
            samples.len(),
            60_480,
            "FT4 waveform must be ~5.04s (60_480 samples @ 12 kHz), the 7.5s-grid FT4 length"
        );
        // Must NOT be the FT8 waveform length (79 × 0.16 × 12_000 = 151_680).
        assert_ne!(
            samples.len(),
            151_680,
            "FT4 must not emit an FT8-length (12.64s) waveform"
        );
    }

    /// TX-F4: the Step-4c late-pivot re-encode (`coordinator/tx.rs`, "Step 4c:
    /// late pivot to the freshest message") must use the SAME protocol-aware
    /// `encode_for_protocol` + `modulate_for_protocol` pair Step 1's initial
    /// encode uses, not the legacy FT8-only `encode_message`/`modulate_symbols`
    /// pair. Before this fix, a pivot in FT4/FT2 mode would emit an FT8-shaped
    /// (151_680-sample) waveform onto the FT4/FT2 grid — wrong length, wrong
    /// symbol timing. This test exercises the exact call shape the pivot now
    /// uses and proves it yields the FT4-length waveform (60_480 samples).
    #[test]
    fn ft4_pivot_reencode_produces_ft4_shaped_waveform_not_ft8() {
        let mut enc = Ft8Encoder::with_protocol(ProtocolParams::ft4());
        let mut modu = Ft8Modulator::new_default().unwrap();
        // Mirrors the pivot's `modulator.set_base_frequency(new_freq)` call.
        modu.set_base_frequency(1500.0).unwrap();

        let samples = encode_for_protocol(&mut enc, Protocol::Ft4, MSG)
            .and_then(|s| modulate_for_protocol(&mut modu, Protocol::Ft4, &s, 0.0))
            .expect("FT4 pivot re-encode should succeed");

        assert_eq!(
            samples.len(),
            60_480,
            "a Step-4c pivot re-encode in FT4 mode must produce the FT4-shaped \
             waveform (60_480 samples @ 12 kHz)"
        );
        assert_ne!(
            samples.len(),
            151_680,
            "a Step-4c pivot re-encode in FT4 mode must NOT fall back to the \
             FT8-shaped (151_680-sample) waveform"
        );
    }

    #[test]
    fn ft8_waveform_is_ft8_length() {
        let samples = encode_and_modulate(Protocol::Ft8, MSG, 0.0)
            .expect("FT8 encode+modulate should succeed");
        // FT8: 79 symbols × 0.16s × 12_000 Hz = 151_680 samples (~12.64s).
        assert_eq!(
            samples.len(),
            151_680,
            "FT8 waveform must be the ~12.64s length"
        );
    }

    #[test]
    fn ft8_branch_is_byte_identical_to_legacy_path() {
        // Legacy path exactly as the TX worker called it pre-FT4-wiring:
        // Ft8Encoder::new() + Ft8Modulator::new_default() + encode_message +
        // modulate_symbols.
        let legacy = {
            let mut enc = Ft8Encoder::new();
            let mut modu = Ft8Modulator::new_default().unwrap();
            let symbols = enc.encode_message(MSG, None).unwrap();
            modu.modulate_symbols(&symbols, 0.0).unwrap()
        };
        // New helper path with Protocol::Ft8.
        let via_helper =
            encode_and_modulate(Protocol::Ft8, MSG, 0.0).expect("FT8 helper should succeed");
        assert_eq!(
            legacy.len(),
            via_helper.len(),
            "FT8 helper sample count must match the legacy path"
        );
        assert_eq!(
            legacy, via_helper,
            "FT8 helper output must be byte-identical to the legacy encode+modulate path"
        );
    }

    #[test]
    fn ft8_split_helpers_match_legacy_combined_call() {
        // Prove the split encode_for_protocol + modulate_for_protocol pair (what
        // the worker's single-TX path now calls) is byte-identical to the legacy
        // combined encode_message().and_then(modulate_symbols) call for FT8.
        let legacy = {
            let mut enc = Ft8Encoder::new();
            let mut modu = Ft8Modulator::new_default().unwrap();
            enc.encode_message(MSG, None)
                .and_then(|symbols| modu.modulate_symbols(&symbols, 0.0))
                .unwrap()
        };
        let split = {
            let mut enc = Ft8Encoder::new();
            let mut modu = Ft8Modulator::new_default().unwrap();
            let symbols = encode_for_protocol(&mut enc, Protocol::Ft8, MSG).unwrap();
            modulate_for_protocol(&mut modu, Protocol::Ft8, &symbols, 0.0).unwrap()
        };
        assert_eq!(
            legacy, split,
            "split FT8 encode/modulate helpers must equal the legacy combined call"
        );
    }

    fn tx_item(
        text: &str,
        freq: f64,
        qso_id: Option<&str>,
    ) -> crate::message_bus::TransmitRequestItem {
        crate::message_bus::TransmitRequestItem {
            message_text: text.to_string(),
            frequency_offset: freq,
            qso_id: qso_id.map(|s| s.to_string()),
        }
    }

    /// TX-F8: `multi_tx_status_items` is the single source both the initial
    /// "QUEUED" status and the Step-2b defer-time refresh use to build the
    /// TUI-visible TX-strip items — prove it tags every item with the
    /// requested `deferred` flag (not the previously-unconditional `false`).
    #[test]
    fn multi_tx_status_items_tags_deferred_flag() {
        let items = vec![
            tx_item("CQ K5ARH EM12", 800.0, None),
            tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-1")),
        ];

        let queued = multi_tx_status_items(&items, false);
        assert_eq!(queued.len(), 2);
        assert!(
            queued.iter().all(|it| !it.deferred),
            "the initial QUEUED status must show deferred: false"
        );

        let deferred = multi_tx_status_items(&items, true);
        assert_eq!(deferred.len(), 2);
        assert!(
            deferred.iter().all(|it| it.deferred),
            "the Step-2b defer-time refresh must show deferred: true on every \
             item in the bundle (a bundle defers as a whole)"
        );
        // Content is otherwise preserved (text/freq/qso_id) regardless of
        // the deferred flag.
        assert_eq!(deferred[0].text, "CQ K5ARH EM12");
        assert_eq!(deferred[0].freq_hz, 800.0);
        assert_eq!(deferred[0].qso_id, None);
        assert_eq!(deferred[1].qso_id, Some("qso-1".to_string()));
    }

    #[test]
    fn multi_tx_encode_all_items_succeed_in_order() {
        let mut enc = Ft8Encoder::new();
        let params = ProtocolParams::from_protocol(Protocol::Ft8);
        let items = vec![
            tx_item("CQ K5ARH EM12", 800.0, None),
            tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-1")),
        ];
        let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &items);
        assert!(outcome.encode_failed.is_empty());
        assert_eq!(outcome.item_texts, vec!["CQ K5ARH EM12", "W1AW K5ARH EM12"]);
        assert_eq!(
            outcome.encoded_qso_ids,
            vec![None, Some("qso-1".to_string())]
        );
        let samples = outcome.samples.expect("both items should modulate fine");
        assert_eq!(
            samples.len(),
            151_680,
            "FT8 multi-TX waveform is one fixed-duration 12.64s slot regardless of item count"
        );
    }

    /// 2026-07-17 regression lock: the original inline Step 1 code built each
    /// `MultiTxItem`'s frequency via `items[i].frequency_offset` AFTER the
    /// encode loop had already skipped a failed item from `symbol_sets` — so
    /// a partial encode failure silently misaligned every LATER item's
    /// frequency with an unrelated, earlier item's. This proves the
    /// extracted helper keeps frequency/text/qso_id/symbols correctly
    /// paired per-item even when a middle item fails to encode.
    #[test]
    fn multi_tx_encode_partial_failure_does_not_misalign_frequencies() {
        let mut enc = Ft8Encoder::new();
        let params = ProtocolParams::from_protocol(Protocol::Ft8);
        // A free-text message far too long for FT8's charset/length limit —
        // guaranteed to fail `encode_for_protocol`, unlike the two valid
        // standard-callsign messages bracketing it.
        let bad_text = "THIS FREE TEXT MESSAGE IS WAY TOO LONG FOR FT8 TO EVER ENCODE";
        let items = vec![
            tx_item("CQ K5ARH EM12", 800.0, Some("qso-good-1")),
            tx_item(bad_text, 1000.0, Some("qso-bad")),
            tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-good-2")),
        ];
        let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &items);

        assert_eq!(
            outcome.encode_failed.len(),
            1,
            "exactly the bad middle item should fail to encode"
        );
        assert_eq!(outcome.encode_failed[0].message_text, bad_text);
        assert_eq!(outcome.encode_failed[0].qso_id.as_deref(), Some("qso-bad"));

        // The two GOOD items must keep their OWN frequencies and qso_ids —
        // not silently inherit each other's via index drift.
        assert_eq!(outcome.item_texts, vec!["CQ K5ARH EM12", "W1AW K5ARH EM12"]);
        assert_eq!(
            outcome.encoded_qso_ids,
            vec![
                Some("qso-good-1".to_string()),
                Some("qso-good-2".to_string())
            ]
        );
        assert!(
            outcome.samples.is_ok(),
            "the two good items should still modulate"
        );
        // `encoded_items` (2026-07-17, post-review) must ALSO stay
        // positionally aligned with item_texts/encoded_qso_ids, skipping
        // exactly the failed item — this is what the caller rebinds
        // `items` to, so downstream code (Step 4b's liveness zip) can
        // never see a stale, unfiltered `items` list again.
        assert_eq!(outcome.encoded_items.len(), 2);
        assert_eq!(
            outcome.encoded_items[0].qso_id.as_deref(),
            Some("qso-good-1")
        );
        assert_eq!(
            outcome.encoded_items[1].qso_id.as_deref(),
            Some("qso-good-2")
        );
    }

    /// 2026-07-17 (post-review) critical regression lock: an independent
    /// review of an earlier version of this fix found that the CALLER was
    /// zipping the unfiltered input `items` list against `encoded_qso_ids`
    /// (shorter, whenever any item failed to encode) — silently misaligning
    /// every item after the first encode failure. In the worst case this
    /// could pair a genuinely-cancelled QSO with a "still live" verdict
    /// belonging to an unrelated item, transmitting a call the operator had
    /// just killed. This test proves the FIX PATTERN Step 1/Step 4b now
    /// use — rebind `items = outcome.encoded_items` immediately, THEN zip
    /// against a liveness set derived from `encoded_qso_ids` — keeps every
    /// item paired with its OWN, correct verdict, even with an encode
    /// failure in the middle of the bundle.
    #[test]
    fn multi_tx_items_rebind_keeps_correct_pairing_after_encode_failure() {
        use std::collections::HashSet;

        let mut enc = Ft8Encoder::new();
        let params = ProtocolParams::from_protocol(Protocol::Ft8);
        let bad_text = "THIS FREE TEXT MESSAGE IS WAY TOO LONG FOR FT8 TO EVER ENCODE";
        let original_items = vec![
            tx_item("CQ K5ARH EM12", 800.0, Some("qso-kenya")),
            tx_item(bad_text, 1000.0, Some("qso-bad-encode")),
            tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-peru")),
        ];
        let outcome =
            encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &original_items);
        assert_eq!(outcome.encode_failed.len(), 1);

        // The fix: rebind `items` to the encoded subset BEFORE any liveness
        // check, exactly as the real Step 1 call site now does.
        let items = outcome.encoded_items;
        let encoded_qso_ids = outcome.encoded_qso_ids;
        assert_eq!(items.len(), encoded_qso_ids.len());

        // Simulate: Kenya was cancelled during the pre-PTT wait; Peru is
        // still live. Only Peru is in the "active" set.
        let mut active: HashSet<String> = HashSet::new();
        active.insert(super::super::active_tx_qso_key("qso-peru"));

        let live_mask: Vec<bool> = encoded_qso_ids
            .iter()
            .map(|id| super::super::tx_qso_is_live(id.as_deref(), &active))
            .collect();

        // Zipping the REBOUND `items` (not the original 3-item list) against
        // `live_mask` must pair each item with ITS OWN verdict.
        let live_items: Vec<_> = items
            .iter()
            .zip(live_mask.iter())
            .filter(|(_, &live)| live)
            .map(|(item, _)| item.qso_id.clone())
            .collect();

        assert_eq!(
            live_items,
            vec![Some("qso-peru".to_string())],
            "only Peru must survive — Kenya must be excluded, not swapped in via index drift"
        );

        let dropped: Vec<_> = items
            .iter()
            .zip(live_mask.iter())
            .filter(|(_, &live)| !live)
            .map(|(item, _)| item.qso_id.clone())
            .collect();
        assert_eq!(
            dropped,
            vec![Some("qso-kenya".to_string())],
            "Kenya must be the one reported as dropped, not silently kept alive"
        );
    }

    #[test]
    fn multi_tx_encode_empty_input_yields_no_samples() {
        let mut enc = Ft8Encoder::new();
        let params = ProtocolParams::from_protocol(Protocol::Ft8);
        let outcome = encode_and_modulate_multi_tx(&mut enc, Protocol::Ft8, &params, &[]);
        assert!(outcome.samples.is_err());
        assert!(outcome.item_texts.is_empty());
        assert!(outcome.encode_failed.is_empty());
    }

    /// 2026-07-17: proves the key-time re-encode path (Step 4b, when only
    /// SOME bundle items are still live) reuses the encode helper correctly
    /// on just a subset, producing a shorter but still-valid, correctly
    /// frequency-aligned output — the core building block the Step 4b
    /// rebuild relies on instead of dropping a still-live station's cycle.
    #[test]
    fn multi_tx_encode_subset_reencode_matches_full_encode_of_that_subset() {
        let params = ProtocolParams::from_protocol(Protocol::Ft8);
        let all_items = vec![
            tx_item("CQ K5ARH EM12", 800.0, Some("qso-kenya")),
            tx_item("W1AW K5ARH EM12", 1200.0, Some("qso-peru")),
        ];
        // "Full" bundle result.
        let mut enc_full = Ft8Encoder::new();
        let full = encode_and_modulate_multi_tx(&mut enc_full, Protocol::Ft8, &params, &all_items);
        assert!(full.samples.is_ok());

        // "Rebuilt" result for just the still-live subset (as Step 4b does
        // when qso-kenya went stale during the pre-PTT wait).
        let live_subset: Vec<_> = all_items
            .iter()
            .filter(|it| it.qso_id.as_deref() == Some("qso-peru"))
            .cloned()
            .collect();
        let mut enc_rebuild = Ft8Encoder::new();
        let rebuilt =
            encode_and_modulate_multi_tx(&mut enc_rebuild, Protocol::Ft8, &params, &live_subset);

        assert_eq!(rebuilt.item_texts, vec!["W1AW K5ARH EM12"]);
        assert_eq!(rebuilt.encoded_qso_ids, vec![Some("qso-peru".to_string())]);
        assert!(
            rebuilt.samples.is_ok(),
            "the surviving item alone must still modulate"
        );
        // Same fixed protocol duration regardless of how many items are summed.
        assert_eq!(
            rebuilt.samples.unwrap().len(),
            full.samples.unwrap().len(),
            "re-encoding a subset must not change the waveform's fixed slot duration \
             (the Step 4b rebuild reuses the ALREADY-COMPUTED TxSchedule padding/cursor, \
             which assumes this)"
        );
    }
}

/// Unit tests for the remote-TX arm gate helper (`remote_tx_permitted`).
///
/// These lock the safety-critical fail direction: a fresh (unarmed) `ArmState`
/// denies, a fully-armed+consented state permits, and a **poisoned** arm lock
/// **fails CLOSED** (denies) — the opposite of `tx_qso_is_live`'s fail-open.
#[cfg(test)]
mod remote_arm_gate_tests {
    use super::remote_tx_permitted;
    use pancetta_agent::arm::{ArmState, VerifiedArmGrant};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_000_000;

    fn grant() -> VerifiedArmGrant {
        VerifiedArmGrant {
            operator_callsign: "K5ARH".to_string(),
            ttl_ms: 120_000,
            scope_tx: true,
            jti: "tx-test-arm-jti".to_string(),
            client_key_id: "tx-test-client-key-id".to_string(),
        }
    }

    #[test]
    fn fresh_unarmed_state_denies() {
        let arm = Arc::new(Mutex::new(ArmState::new()));
        assert!(
            !remote_tx_permitted(&arm, NOW),
            "a fresh (unarmed) ArmState must deny remote TX"
        );
    }

    #[test]
    fn armed_consented_fresh_heartbeat_permits() {
        let mut st = ArmState::new();
        st.arm(grant(), NOW);
        st.set_local_consent(true, NOW);
        let arm = Arc::new(Mutex::new(st));
        assert!(
            remote_tx_permitted(&arm, NOW),
            "armed + tx-scope + consent + fresh heartbeat must permit"
        );
    }

    #[test]
    fn armed_without_local_consent_denies() {
        // Consent (remote_tx_enabled) is the LOCAL operator gate: default OFF.
        let mut st = ArmState::new();
        st.arm(grant(), NOW);
        // no set_local_consent(true)
        let arm = Arc::new(Mutex::new(st));
        assert!(
            !remote_tx_permitted(&arm, NOW),
            "armed but no local consent must deny"
        );
    }

    #[test]
    fn poisoned_lock_fails_closed() {
        // Arm + consent so that IF the lock were readable, it would permit.
        let mut st = ArmState::new();
        st.arm(grant(), NOW);
        st.set_local_consent(true, NOW);
        let arm = Arc::new(Mutex::new(st));

        // Poison the mutex by panicking while holding the guard.
        let a2 = arm.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = a2.lock().unwrap();
            panic!("poison the arm mutex");
        }));
        assert!(arm.is_poisoned(), "mutex should be poisoned");

        // Even though the underlying state WOULD permit, the poisoned lock must
        // fail CLOSED (deny) — the opposite of tx_qso_is_live's fail-open.
        assert!(
            !remote_tx_permitted(&arm, NOW),
            "SAFETY: a poisoned arm lock must fail CLOSED (deny remote TX)"
        );
    }
}

#[cfg(test)]
mod tx_counter_tests {
    use super::*;

    /// Process-global counters mean concurrent tests in this binary can also
    /// increment them — assert a delta, not an absolute value (same
    /// discipline as `main.rs`'s `panic_hook_counts_and_survives_via_catch_unwind`).
    #[test]
    fn tx_attempts_count_increments() {
        let before = tx_attempts_count();
        TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);
        assert_eq!(tx_attempts_count(), before + 1);
    }

    #[test]
    fn tx_defers_count_increments() {
        let before = tx_defers_count();
        TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);
        assert_eq!(tx_defers_count(), before + 1);
    }

    /// TX-F8: the multi-TX arm's defer-time recheck must bump the SAME
    /// process-global counter the single-TX arm's defer-time recheck does
    /// (mirrors `tx_defers_count_increments` above — see `coordinator/tx.rs`'s
    /// Step-2b block in the `MultiTransmitRequest` worker arm).
    #[test]
    fn tx_defers_count_increments_for_multi_tx() {
        let before = tx_defers_count();
        TX_DEFERS_COUNT.fetch_add(1, Ordering::Relaxed);
        assert_eq!(tx_defers_count(), before + 1);
    }
}

#[cfg(test)]
mod classifier_tests {
    use super::*;
    use crate::coordinator::active_tx_qso_key;

    fn transmit_request(text: &str, qso_id: Option<&str>) -> MessageType {
        MessageType::TransmitRequest {
            message_text: text.to_string(),
            frequency_offset: 1500.0,
            qso_id: qso_id.map(|s| s.to_string()),
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        }
    }

    #[test]
    fn classify_supersedes_on_different_text_same_qso() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        );
        match outcome {
            super::IncomingDuringTx::Supersede { text, qso_id, .. } => {
                assert_eq!(text, "KA1ABC K5ARH RR73");
                assert_eq!(qso_id.as_deref(), Some("qso-1"));
            }
            super::IncomingDuringTx::Drop => panic!("expected Supersede"),
        }
    }

    #[test]
    fn classify_supersedes_on_manual_free_text_no_qso() {
        // qso_id == None is the genuine manual/free-text/tune/test-TX case —
        // this is the actual "operator trigger" this classifier exists for,
        // and always supersedes.
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("CQ K5ARH EM12", None);
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        );
        assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
    }

    /// PAN-73 regression: reproduces the 2026-09-05 VP2MAA/JF1RDH/JA5GYU
    /// live incident. A DIFFERENT QSO's own routine auto-sequence message
    /// arriving in the channel while another QSO is in flight must NOT
    /// supersede it — that killed a perfectly good in-flight transmission
    /// for content that (in the live incident) wasn't even schedulable this
    /// slot, leaving the slot silent until an unrelated third QSO's retry
    /// timer opportunistically keyed into the dead air ~5s later.
    #[test]
    fn classify_drops_different_qsos_autonomous_message() {
        let pivoted_once = std::collections::HashMap::new();
        // In flight: VP2MAA's QSO. Candidate: JF1RDH's QSO, a distinct
        // qso_id and distinct text — exactly the shape that raced in
        // production.
        let candidate = transmit_request("JF1RDH W5AU EM10", Some("qso-jf1rdh"));
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-vp2maa"),
            "VP2MAA W5AU EM10",
            &pivoted_once,
        );
        assert!(
            matches!(outcome, super::IncomingDuringTx::Drop),
            "a different QSO's own message must not supersede an unrelated in-flight TX, got {outcome:?}"
        );
    }

    #[test]
    fn classify_drops_identical_content() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = transmit_request("KA1ABC K5ARH R-15", Some("qso-1"));
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        );
        assert!(matches!(outcome, super::IncomingDuringTx::Drop));
    }

    #[test]
    fn classify_drops_pivot_tombstone_duplicate() {
        let mut pivoted_once = std::collections::HashMap::new();
        pivoted_once.insert(active_tx_qso_key("qso-1"), "KA1ABC K5ARH RR73".to_string());
        let candidate = transmit_request("KA1ABC K5ARH RR73", Some("qso-1"));
        // in_flight_text is something else — the pivot already sent RR73 via
        // Step 4c, this is the stale second copy of the request that produced it.
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "KA1ABC K5ARH R-15",
            &pivoted_once,
        );
        assert!(matches!(outcome, super::IncomingDuringTx::Drop));
    }

    #[test]
    fn classify_always_supersedes_multi_transmit_request() {
        let pivoted_once = std::collections::HashMap::new();
        let candidate = MessageType::MultiTransmitRequest {
            items: vec![],
            tx_parity: None,
            origin: crate::message_bus::TxOrigin::Local,
        };
        let outcome = super::classify_incoming_during_tx(
            &candidate,
            Some("qso-1"),
            "anything",
            &pivoted_once,
        );
        assert!(matches!(outcome, super::IncomingDuringTx::Supersede { .. }));
    }
}
