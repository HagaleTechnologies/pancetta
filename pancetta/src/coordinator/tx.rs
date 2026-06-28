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
const COALESCE_COLLECT_WINDOW_MS: u64 = 800;

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
pub fn schedule_tx(
    now: chrono::DateTime<chrono::Utc>,
    required_parity: pancetta_core::slot::SlotParity,
    tx_late_max_ms: u64,
    sample_rate: u32,
) -> TxSchedule {
    use pancetta_core::slot::{current_slot_start, next_slot_with_parity, SlotParity};

    let cur_start = current_slot_start(now);
    let cur_parity = SlotParity::of(cur_start);
    let mstr_in_cur_slot = (now - cur_start).num_milliseconds().max(0) as u64;

    // Decide which slot to target. The current slot is viable iff its
    // parity matches AND we haven't burned past tx_late_max_ms.
    let use_current = cur_parity == required_parity && mstr_in_cur_slot <= tx_late_max_ms;
    let target = if use_current {
        cur_start
    } else {
        next_slot_with_parity(now, required_parity)
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
    ) -> Self {
        ptt_active.store(true, std::sync::atomic::Ordering::Release);
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
        } => CoalesceEntry {
            message_text,
            frequency_offset,
            qso_id,
            tx_parity,
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
            } => {
                drained.push(CoalesceEntry {
                    message_text,
                    frequency_offset,
                    qso_id,
                    tx_parity,
                });
            }
            _ => {
                // Non-TX message: re-enqueue verbatim and stop draining so we
                // never reorder Tune/Multi ahead of, or behind, TX intent.
                if let Err(e) = message_bus.send_message(msg).await {
                    warn!(
                        target: "tx.policy",
                        "failed to re-enqueue non-TX message during coalesce drain: {}",
                        e
                    );
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
        };
    }

    let backlog_total = drained.len();
    let outcome = coalesce_transmit_requests(drained, |id| tx_qso_is_live(id, active_tx_qsos));

    if !outcome.is_noop() {
        warn!(
            target: "tx.policy",
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
        };
    }

    // Several distinct live streams survived → fold into the existing multi-TX
    // path. All bundle items share one slot, so the bundle parity is the
    // freshest stream's parity (first retained entry, which the existing arm
    // resolves via resolve_required_parity).
    let bundle_parity = outcome.retained[0].tx_parity;
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

            tokio::spawn(async move {
                info!("FT8 transmitter component ready");

                let mut encoder = Ft8Encoder::new();
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
                    match tx_rx.try_recv() {
                        Ok(mut message) => {
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
                                // See COALESCE_COLLECT_WINDOW_MS.
                                tokio::time::sleep(Duration::from_millis(
                                    COALESCE_COLLECT_WINDOW_MS,
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
                                } => {
                                    info!(
                                        "Transmit request: '{}' at offset {:.0} Hz (qso: {:?})",
                                        message_text, frequency_offset, qso_id
                                    );

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
                                            target: "tx.policy",
                                            "TX DISABLED (RX-only): blocking '{}' at {:.0} Hz (qso: {:?})",
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
                                    let (samples, _duration_ms) = match encoder
                                        .encode_message(&message_text, None)
                                        .and_then(|symbols| {
                                            modulator.modulate_symbols(&symbols, 0.0)
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
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        chrono::Utc::now(),
                                    );

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
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
                                        // Re-check active-status at defer time: a
                                        // terminal QSO's request must not be re-
                                        // deferred 30s into the future (that is
                                        // exactly the "stale frames every cycle"
                                        // loop the operator hit).
                                        if !tx_qso_is_live(qso_id.as_deref(), &active_tx_qsos) {
                                            info!(
                                                target: "tx.policy",
                                                "dropping stale TX for ended QSO {} at defer time: '{}'",
                                                qso_id.as_deref().unwrap_or("?"),
                                                message_text
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
                                            target: "tx.policy",
                                            "dropping stale TX for ended QSO {}: '{}'",
                                            qso_id.as_deref().unwrap_or("?"),
                                            message_text
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
                                    let mut ptt_guard =
                                        PttGuard::new(message_bus.clone(), ptt_active.clone());
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
                                } => {
                                    info!("Multi-TX request: {} messages", items.len());

                                    // --- Step 0: TX-policy hard mute ---
                                    // Disabled (RX-only): never key PTT / play audio /
                                    // modulate. Consume the bundle, clear the TUI TX
                                    // view, and report each item failed so any awaiting
                                    // state doesn't hang.
                                    if current_tx_policy(&tx_policy)
                                        == pancetta_core::TxPolicy::Disabled
                                    {
                                        info!(
                                            target: "tx.policy",
                                            "TX DISABLED (RX-only): blocking multi-TX bundle of {} items",
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
                                                    target: "tx.policy",
                                                    "dropping stale multi-TX item for ended QSO {}: '{}'",
                                                    item.qso_id.as_deref().unwrap_or("?"),
                                                    item.message_text
                                                );
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
                                                target: "tx.policy",
                                                "multi-TX bundle empty after dropping stale items — skipping"
                                            );
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
                                    let ft8_params = pancetta_ft8::ProtocolParams::ft8();
                                    let mut symbol_sets: Vec<Vec<u8>> = Vec::new();
                                    let mut item_texts: Vec<String> = Vec::new();

                                    for item in &items {
                                        match encoder.encode_message(&item.message_text, None) {
                                            Ok(symbols) => {
                                                item_texts.push(item.message_text.clone());
                                                symbol_sets.push(symbols.to_vec());
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Encoding failed for '{}': {}",
                                                    item.message_text, e
                                                );
                                            }
                                        }
                                    }

                                    // TransmitRequestItem.frequency_offset is the ABSOLUTE
                                    // audio frequency (matching TransmitRequest semantics).
                                    // modulate_multi_tx wants per-item OFFSETS from a shared
                                    // base. Use base=200 (lowest valid) so any audio frequency
                                    // in the 200-2500 Hz FT8 passband maps to a non-negative
                                    // per-item offset.
                                    const MULTI_TX_BASE_HZ: f64 = 200.0;
                                    let mut multi_items = Vec::new();
                                    for (i, symbols) in symbol_sets.iter().enumerate() {
                                        multi_items.push(pancetta_ft8::MultiTxItem {
                                            symbols: symbols.as_slice(),
                                            frequency_offset: items[i].frequency_offset
                                                - MULTI_TX_BASE_HZ,
                                            params: &ft8_params,
                                        });
                                    }

                                    let (samples_opt, _duration_ms_encode) = if !multi_items
                                        .is_empty()
                                    {
                                        match pancetta_ft8::modulate_multi_tx(
                                            &multi_items,
                                            12000,
                                            MULTI_TX_BASE_HZ,
                                            0.5,
                                        ) {
                                            Ok(samples) => {
                                                let dur = (samples.len() as f64 / 12000.0 * 1000.0)
                                                    as u64;
                                                info!(
                                                    "Multi-TX: {} messages -> {} samples ({:.2}s)",
                                                    multi_items.len(),
                                                    samples.len(),
                                                    dur as f64 / 1000.0
                                                );
                                                (Some(samples), dur)
                                            }
                                            Err(e) => {
                                                warn!("Multi-TX modulation failed: {}", e);
                                                (None, 0)
                                            }
                                        }
                                    } else {
                                        (None, 0)
                                    };

                                    let samples = match samples_opt {
                                        Some(s) => s,
                                        None => {
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
                                    let required_parity = resolve_required_parity(
                                        tx_parity,
                                        tx_self_parity,
                                        chrono::Utc::now(),
                                    );

                                    let schedule = schedule_tx(
                                        chrono::Utc::now(),
                                        required_parity,
                                        tx_late_max_ms,
                                        sample_rate,
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
                                    // The summed waveform can't be re-filtered now,
                                    // but if EVERY item's QSO went terminal during
                                    // the wait, skip the whole bundle rather than key
                                    // PTT for a dead exchange.
                                    if !items.iter().any(|it| {
                                        tx_qso_is_live(it.qso_id.as_deref(), &active_tx_qsos)
                                    }) {
                                        info!(
                                            target: "tx.policy",
                                            "dropping stale multi-TX bundle: all {} item(s) belong to ended QSOs",
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
                                    let mut ptt_guard =
                                        PttGuard::new(message_bus.clone(), ptt_active.clone());
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
                                            target: "tx.policy",
                                            "TX DISABLED (RX-only): blocking tune ({}s @ {} Hz)",
                                            duration_secs, tone_offset_hz
                                        );
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
                                    let mut ptt_guard =
                                        PttGuard::new(message_bus.clone(), ptt_active.clone());
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
) -> pancetta_core::slot::SlotParity {
    use pancetta_config::station::TxSelfParity;
    use pancetta_core::slot::{next_slot_with_parity, SlotParity};
    if let Some(p) = tx_parity {
        return p;
    }
    match tx_self_parity {
        TxSelfParity::Even => SlotParity::Even,
        TxSelfParity::Odd => SlotParity::Odd,
        TxSelfParity::Auto => {
            let next_even = next_slot_with_parity(now, SlotParity::Even);
            let next_odd = next_slot_with_parity(now, SlotParity::Odd);
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
    use pancetta_core::slot::SlotParity;

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
        let s = schedule_tx(at(5.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12); // 12 samples/ms at 12kHz
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn on_time_no_pad_no_skip() {
        // now = :15.500 (Odd slot 1). Required = Odd. Current slot matches;
        // mstr_in_current_slot = 500ms ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 500 = DELAY_MS → pad 0, cursor 0.
        let s = schedule_tx(at(15.5), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 0);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn late_skips_cursor_in_current_slot() {
        // now = :20.0 (Odd slot 1, 5s in). Required = Odd. Current matches;
        // mstr_in_current_slot = 5000 ≤ 8000 → target current slot :15.
        // mstr_relative_to_target = 5000 > 500 → cursor = 4500ms × SR.
        let s = schedule_tx(at(20.0), SlotParity::Odd, 8000, 12_000);
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
        let s = schedule_tx(at(24.5), SlotParity::Odd, 8000, 12_000);
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
        let s = schedule_tx(at(14.6), SlotParity::Odd, 8000, 12_000);
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
        let s = schedule_tx(at(15.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 15_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn current_slot_correct_parity_but_too_late_defers() {
        // now = :29.0 (Odd slot 1, 14s in — past tx_late_max_ms=8000).
        // Even though parity matches, we're too late for skip-ahead.
        // Defer to next Odd = :45.
        let s = schedule_tx(at(29.0), SlotParity::Odd, 8000, 12_000);
        assert_eq!((s.target_slot - at(0.0)).num_milliseconds(), 45_000);
        assert_eq!(s.silent_pad_samples, 500 * 12);
        assert_eq!(s.cursor_offset_samples, 0);
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Auto → returns Even
        let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Auto, at(5.0));
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_explicit_wins_over_explicit_config() {
        use pancetta_config::station::TxSelfParity;
        // tx_parity = Some(Even), config = Odd → tx_parity wins → Even
        let p = resolve_required_parity(Some(SlotParity::Even), TxSelfParity::Odd, at(5.0));
        assert_eq!(p, SlotParity::Even);
    }

    #[test]
    fn resolve_required_parity_falls_back_to_config_when_none() {
        use pancetta_config::station::TxSelfParity;
        let p = resolve_required_parity(None, TxSelfParity::Even, at(5.0));
        assert_eq!(p, SlotParity::Even);
        let p = resolve_required_parity(None, TxSelfParity::Odd, at(5.0));
        assert_eq!(p, SlotParity::Odd);
    }

    #[test]
    fn resolve_required_parity_auto_picks_nearest_next_slot() {
        use pancetta_config::station::TxSelfParity;
        // now = :05 (in Even slot 0). Next Even = :30, next Odd = :15.
        // Odd is closer → Auto picks Odd.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(5.0));
        assert_eq!(p, SlotParity::Odd);
        // now = :20 (in Odd slot 1). Next Odd = :45, next Even = :30.
        // Even is closer → Auto picks Even.
        let p = resolve_required_parity(None, TxSelfParity::Auto, at(20.0));
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
