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

    // mstr relative to the chosen target. When target is in the future,
    // (now - target) is negative; clamp so we hit the early branch.
    let mstr_signed = (now - target).num_milliseconds();
    let mstr_unsigned = mstr_signed.max(0) as u64;

    let (silent_pad_ms, cursor_ms) = if mstr_unsigned < DELAY_MS {
        (DELAY_MS - mstr_unsigned, 0)
    } else {
        (0, mstr_unsigned - DELAY_MS)
    };

    TxSchedule {
        target_slot: target,
        silent_pad_samples: (silent_pad_ms as usize) * (sample_rate as usize) / 1000,
        cursor_offset_samples: (cursor_ms as usize) * (sample_rate as usize) / 1000,
        deferred,
    }
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

/// Push a richer TX-queue snapshot (NOW-SENDING + QUEUED) to the TUI.
/// Best-effort, observation-only: never touches PTT/audio/scheduling.
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

/// Send a `DiagnosticEvent` to the TUI's retained Diagnostics overlay
/// (observability-diagnostics-plan.md Layer 1, "Emission"). `target` reuses
/// the same vocabulary already used by the co-located `tracing` call at each
/// site (e.g. `"tx.policy"`) — this is a SEPARATE field from that macro's own
/// `target:` (the tracing one gates file-log visibility via `EnvFilter`; this
/// one is just a TUI-side label used for filtering the Diagnostics panel).
/// Best-effort: never blocks or fails the TX path.
async fn emit_diagnostic(
    message_bus: &MessageBus,
    target: &'static str,
    level: pancetta_core::DiagnosticLevel,
    text: String,
    qso_id: Option<&str>,
) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            qso_id: qso_id.map(|s| s.to_string()),
            callsign: None,
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("DiagnosticEvent({}) relay failed (no TUI?): {}", target, e);
    }
}

/// Read the current global TX policy from the shared atomic.
fn current_tx_policy(
    tx_policy: &std::sync::Arc<std::sync::atomic::AtomicU8>,
) -> pancetta_core::TxPolicy {
    pancetta_core::TxPolicy::from_u8(tx_policy.load(Ordering::Acquire))
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
}

impl CoalesceOutcome {
    /// `true` when nothing was reduced — a single request (or a backlog that
    /// happened to be one fresh frame per distinct live QSO with no overflow).
    /// Used only for the log-suppression decision in the worker.
    fn is_noop(&self) -> bool {
        self.coalesced == 0 && self.dropped_terminal == 0 && self.truncated == 0
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
             truncated {} over the {}-stream cap",
            backlog_total,
            outcome.retained.len(),
            outcome.coalesced,
            outcome.dropped_terminal,
            outcome.truncated,
            MAX_RETAINED_TX_STREAMS,
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
    // freshest stream's parity (first retained entry, which the existing arm
    // resolves via resolve_required_parity).
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
        let (tx_late_max_ms, tx_self_parity, ptt_lead_ms, sample_rate) = {
            let cfg = self.config.read().await;
            (
                cfg.station.tx_late_max_ms,
                cfg.station.tx_self_parity,
                cfg.station.ptt_lead_ms,
                12000u32, // FT8 sample rate
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

                while !shutdown.load(Ordering::Acquire) {
                    // Reset the per-message abort flag at the start of every
                    // try_recv cycle. Keeps a stale F8 from earlier (when no
                    // TX was in flight) from killing the next legitimate TX.
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
                                // Brief collection window so same-parity openings
                                // started in quick succession (serial manual
                                // keypresses, each crossing async hops) all arrive
                                // before we coalesce — otherwise the first opening
                                // commits the slot alone and siblings trickle in
                                // one-per-cycle (the "slow-start" bug). Absorbed by
                                // the Step-6 slot-wait, so no real added latency.
                                // See COALESCE_COLLECT_WINDOW_MS /
                                // coalesce_collect_window_ms (FT4/FT2-scaled).
                                tokio::time::sleep(Duration::from_millis(
                                    coalesce_collect_window_ms(active_protocol),
                                ))
                                .await;
                                message.message_type = coalesce_backlog_into(
                                    message.message_type,
                                    &tx_rx,
                                    &message_bus,
                                    &active_tx_qsos,
                                )
                                .await;
                            }

                            match message.message_type {
                                MessageType::TransmitRequest {
                                    mut message_text,
                                    mut frequency_offset,
                                    qso_id,
                                    tx_parity,
                                    origin,
                                } => {
                                    info!(
                                        "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                        message_text, frequency_offset, qso_id
                                    );
                                    TX_ATTEMPTS_COUNT.fetch_add(1, Ordering::Relaxed);

                                    // --- Step 0: TX-policy hard mute ---
                                    // If the global policy is Disabled (RX-only),
                                    // do NOT key PTT / play audio / modulate. Consume
                                    // the request, tell the TUI it was blocked, and
                                    // report a failed TransmitComplete so any awaiting
                                    // QSO state machine doesn't hang. This is the
                                    // catch-all hard gate for every TX source.
                                    if current_tx_policy(&tx_policy)
                                        == pancetta_core::TxPolicy::Disabled
                                    {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX DISABLED (RX-only): blocking '{}' at {:.0} Hz (qso: {:?})",
                                            message_text, frequency_offset, qso_id
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX DISABLED (RX-only): blocking '{message_text}' at {frequency_offset:.0} Hz"
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
                                        info!(
                                            target: "agent.tx",
                                            "dropping remote TX — not armed/permitted: '{}' at {:.0} Hz (qso: {:?})",
                                            message_text, frequency_offset, qso_id
                                        );
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
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
                                    if let Err(e) = modulator.set_base_frequency(frequency_offset) {
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
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
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
                                            let dur = (s.len() as f64 / 12000.0 * 1000.0) as u64;
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
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
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

                                    let schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
                                        slot_ns,
                                    );

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
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text,
                                                    duration_ms: 0,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                            continue;
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

                                    // --- Step 3: Build the audio buffer to ship ---
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
                                        warn!("schedule_tx cursor exceeded waveform length; skipping TX");
                                        emit_tx_failure_diagnostic(
                                            &message_bus,
                                            qso_id.as_deref(),
                                            &message_text,
                                            "internal scheduling error (cursor exceeded waveform)",
                                        )
                                        .await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;

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
                                            break;
                                        }
                                        info!("TX aborted before PTT engage by operator (F8)");
                                        // This abort happens BEFORE the TxStatusGuard is
                                        // constructed, so its Drop-based clear never runs.
                                        // Clear the strip explicitly so the QUEUED row
                                        // doesn't sit stale until the next status push.
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        continue;
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
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
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
                                        send_tx_queue_status(&message_bus, None, Vec::new()).await;
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success: false,
                                                message_text,
                                                duration_ms: 0,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(complete_msg).await;
                                        continue;
                                    }

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
                                        let remod = match modulator.set_base_frequency(new_freq) {
                                            Ok(()) => encoder
                                                .encode_message(&new_text, None)
                                                .and_then(|s| modulator.modulate_symbols(&s, 0.0))
                                                .ok(),
                                            Err(_) => None,
                                        };
                                        match remod {
                                            Some(new_samples)
                                                if schedule.cursor_offset_samples
                                                    < new_samples.len() =>
                                            {
                                                let mut rebuilt = Vec::with_capacity(
                                                    schedule.silent_pad_samples + new_samples.len(),
                                                );
                                                rebuilt.resize(schedule.silent_pad_samples, 0.0f32);
                                                rebuilt.extend_from_slice(
                                                    &new_samples[schedule.cursor_offset_samples..],
                                                );
                                                info!(
                                                    target: "tx.pivot",
                                                    "TX pivot: '{}' -> '{}' @{:.0}Hz for qso {} (fresher message arrived during pre-PTT wait)",
                                                    message_text,
                                                    new_text,
                                                    new_freq,
                                                    qso_id.as_deref().unwrap_or("-")
                                                );
                                                message_text = new_text;
                                                frequency_offset = new_freq;
                                                audio_out = rebuilt;
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
                                    let _tx_status_guard = TxStatusGuard::new(message_bus.clone());
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
                                            target: "tx.ptt",
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
                                    if interruptible_sleep(to_slot, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        // ptt_guard in scope — drop on `break`/`continue`
                                        // fires PTT-off either way.
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted between PTT and slot by shutdown");
                                            break;
                                        }
                                        info!("TX aborted between PTT and slot by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 7: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for audio playback to complete ---
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("TX aborted during playback by shutdown");
                                            break;
                                        }
                                        info!("TX aborted during playback by operator (F8)");
                                        continue;
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
                                            info!("TX aborted during tail by shutdown");
                                            break;
                                        }
                                        info!("TX aborted during tail by operator (F8)");
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

                                    // --- Step 10: Send TransmitComplete ---
                                    let complete_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Autonomous,
                                        MessageType::TransmitComplete {
                                            success,
                                            message_text,
                                            duration_ms,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(complete_msg).await {
                                        warn!("Failed to send TransmitComplete: {}", e);
                                    }
                                }

                                MessageType::MultiTransmitRequest {
                                    mut items,
                                    tx_parity,
                                    origin,
                                } => {
                                    info!("Multi-TX request: {} messages", items.len());
                                    TX_ATTEMPTS_COUNT
                                        .fetch_add(items.len() as u64, Ordering::Relaxed);

                                    // --- Step 0: TX-policy hard mute ---
                                    // Disabled (RX-only): never key PTT / play audio /
                                    // modulate. Consume the bundle, clear the TUI TX
                                    // view, and report each item failed so any awaiting
                                    // state doesn't hang.
                                    if current_tx_policy(&tx_policy)
                                        == pancetta_core::TxPolicy::Disabled
                                    {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX DISABLED (RX-only): blocking multi-TX bundle of {} items",
                                            items.len()
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX DISABLED (RX-only): blocking multi-TX bundle of {} items",
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
                                        info!(
                                            target: "agent.tx",
                                            "dropping remote TX — not armed/permitted: multi-TX bundle of {} items",
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
                                        items
                                            .iter()
                                            .map(|it| crate::message_bus::TxItem {
                                                text: it.message_text.clone(),
                                                freq_hz: it.frequency_offset,
                                                qso_id: it.qso_id.clone(),
                                                deferred: false,
                                            })
                                            .collect(),
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
                                            for text in item_texts {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: text,
                                                        duration_ms: 0,
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

                                    let schedule = schedule_tx(
                                        request_received_at,
                                        required_parity,
                                        tx_late_max_ms,
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

                                    // --- Step 3: Build the audio buffer ---
                                    let mut audio_out: Vec<f32> = Vec::with_capacity(
                                        schedule.silent_pad_samples + samples.len(),
                                    );
                                    audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                    if schedule.cursor_offset_samples < samples.len() {
                                        audio_out.extend_from_slice(
                                            &samples[schedule.cursor_offset_samples..],
                                        );
                                    } else {
                                        warn!("schedule_tx cursor exceeded multi-TX waveform; skipping");
                                        for (text, qso_id) in
                                            item_texts.iter().zip(encoded_qso_ids.iter())
                                        {
                                            emit_tx_failure_diagnostic(
                                                &message_bus,
                                                qso_id.as_deref(),
                                                text,
                                                "internal scheduling error (cursor exceeded multi-TX waveform)",
                                            )
                                            .await;
                                        }
                                        for text in item_texts {
                                            let complete_msg = ComponentMessage::new(
                                                ComponentId::Ft8Transmitter,
                                                ComponentId::Autonomous,
                                                MessageType::TransmitComplete {
                                                    success: false,
                                                    message_text: text,
                                                    duration_ms: 0,
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }
                                    let audio_duration_ms =
                                        (audio_out.len() as f64 / sample_rate as f64 * 1000.0)
                                            as u64;

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
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }

                                    // `encoded_qso_ids` isn't consulted again after this
                                    // point (Step 5+ only needs `items`/`item_texts`), but
                                    // it's rebound alongside the rest for symmetry with the
                                    // pre-Step-4b bindings.
                                    let (
                                        items,
                                        audio_out,
                                        item_texts,
                                        _encoded_qso_ids,
                                        audio_duration_ms,
                                    ) = if live_mask.iter().all(|&live| live) {
                                        // Fast path: nothing went stale.
                                        (
                                            items,
                                            audio_out,
                                            item_texts,
                                            encoded_qso_ids,
                                            audio_duration_ms,
                                        )
                                    } else {
                                        // Partial staleness: report the dropped item(s), then
                                        // re-encode just the still-live subset.
                                        for (item, &live) in items.iter().zip(live_mask.iter()) {
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
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
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
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }

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
                                                for text in rebuilt_texts {
                                                    let complete_msg = ComponentMessage::new(
                                                        ComponentId::Ft8Transmitter,
                                                        ComponentId::Autonomous,
                                                        MessageType::TransmitComplete {
                                                            success: false,
                                                            message_text: text,
                                                            duration_ms: 0,
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

                                        if schedule.cursor_offset_samples >= new_samples.len() {
                                            warn!("schedule_tx cursor exceeded rebuilt multi-TX waveform at key-time; dropping");
                                            for (text, qso_id) in
                                                rebuilt_texts.iter().zip(rebuilt_qso_ids.iter())
                                            {
                                                emit_tx_failure_diagnostic(
                                                    &message_bus,
                                                    qso_id.as_deref(),
                                                    text,
                                                    "internal scheduling error (cursor exceeded rebuilt multi-TX waveform)",
                                                )
                                                .await;
                                            }
                                            send_tx_queue_status(&message_bus, None, Vec::new())
                                                .await;
                                            for text in rebuilt_texts {
                                                let complete_msg = ComponentMessage::new(
                                                    ComponentId::Ft8Transmitter,
                                                    ComponentId::Autonomous,
                                                    MessageType::TransmitComplete {
                                                        success: false,
                                                        message_text: text,
                                                        duration_ms: 0,
                                                    },
                                                    Instant::now(),
                                                );
                                                let _ =
                                                    message_bus.send_message(complete_msg).await;
                                            }
                                            continue;
                                        }

                                        let mut new_audio_out = Vec::with_capacity(
                                            schedule.silent_pad_samples + new_samples.len(),
                                        );
                                        new_audio_out.resize(schedule.silent_pad_samples, 0.0f32);
                                        new_audio_out.extend_from_slice(
                                            &new_samples[schedule.cursor_offset_samples..],
                                        );
                                        let new_audio_duration_ms = (new_audio_out.len() as f64
                                            / sample_rate as f64
                                            * 1000.0)
                                            as u64;

                                        info!(
                                            target: "pancetta::tx.policy",
                                            "multi-TX bundle re-encoded at key-time: {} of {} item(s) still live",
                                            rebuilt_texts.len(),
                                            items.len()
                                        );

                                        (
                                            live_items,
                                            new_audio_out,
                                            rebuilt_texts,
                                            rebuilt_qso_ids,
                                            new_audio_duration_ms,
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
                                                },
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(complete_msg).await;
                                        }
                                        continue;
                                    }

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
                                            target: "tx.ptt",
                                            "PTT ON (scheduled multi-TX) sent to rig"
                                        );
                                    }

                                    // --- Step 6: Sleep precisely until target slot ---
                                    let to_slot = pancetta_core::slot::duration_until(
                                        schedule.target_slot,
                                        chrono::Utc::now(),
                                    );
                                    if interruptible_sleep(to_slot, &shutdown, &abort_current_tx)
                                        .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!(
                                                "Multi-TX aborted between PTT and slot by shutdown"
                                            );
                                            break;
                                        }
                                        info!("Multi-TX aborted between PTT and slot by operator (F8)");
                                        continue;
                                    }

                                    // --- Step 7: Route audio to output ---
                                    let audio_msg = ComponentMessage::new(
                                        ComponentId::Ft8Transmitter,
                                        ComponentId::Audio,
                                        MessageType::AudioOutput {
                                            samples: audio_out,
                                            sample_rate,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(audio_msg).await {
                                        debug!("Audio output routing: {}", e);
                                    }

                                    // --- Step 8: Wait for playback to complete ---
                                    if interruptible_sleep(
                                        Duration::from_millis(audio_duration_ms),
                                        &shutdown,
                                        &abort_current_tx,
                                    )
                                    .await
                                    {
                                        if shutdown.load(Ordering::Acquire) {
                                            info!("Multi-TX aborted during playback by shutdown");
                                            break;
                                        }
                                        info!("Multi-TX aborted during playback by operator (F8)");
                                        continue;
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
                                    for text in item_texts {
                                        let complete_msg = ComponentMessage::new(
                                            ComponentId::Ft8Transmitter,
                                            ComponentId::Autonomous,
                                            MessageType::TransmitComplete {
                                                success,
                                                message_text: text,
                                                duration_ms,
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
                                    if current_tx_policy(&tx_policy)
                                        == pancetta_core::TxPolicy::Disabled
                                    {
                                        info!(
                                            target: "pancetta::tx.policy",
                                            "TX DISABLED (RX-only): blocking tune ({}s @ {} Hz)",
                                            duration_secs, tone_offset_hz
                                        );
                                        emit_diagnostic(
                                            &message_bus,
                                            "tx.policy",
                                            pancetta_core::DiagnosticLevel::Info,
                                            format!(
                                                "TX DISABLED (RX-only): blocking tune ({duration_secs}s @ {tone_offset_hz} Hz)"
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
        let live = liveset(&[]); // empty: no QSO is "live"
        let out = coalesce_transmit_requests(
            vec![entry("MANUAL-1", None), entry("MANUAL-2", None)],
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
        let live = liveset(&["qso-a"]);
        let out = coalesce_transmit_requests(
            vec![
                entry("KC-1", Some("qso-a")),
                entry("KC-2", Some("qso-a")),
                entry("MANUAL", None),
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
        // first-seen streams kept, overflow counted.
        let ids: Vec<String> = (0..MAX_RETAINED_TX_STREAMS + 3)
            .map(|i| format!("qso-{i}"))
            .collect();
        let live: HashSet<String> = ids
            .iter()
            .map(|s| super::super::active_tx_qso_key(s))
            .collect();
        let drained: Vec<CoalesceEntry> = ids.iter().map(|id| entry(id, Some(id))).collect();
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
        // nothing reduced (is_noop).
        let live = liveset(&["qso-a", "qso-b"]);
        let out = coalesce_transmit_requests(
            vec![entry("A", Some("qso-a")), entry("B", Some("qso-b"))],
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
}
