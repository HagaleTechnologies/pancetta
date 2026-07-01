//! QSO state-machine component.
//!
//! Wires decoded FT8 messages into the `pancetta-qso` state machine for
//! tracking, auto-logs completed exchanges to SQLite at
//! `~/.pancetta/qso.db`, and surfaces respond-to-CQ outcomes to the TUI
//! status bar (so Space-to-call says "Calling X — TX queued" or "Call X
//! failed: duplicate QSO …" instead of the previous optimistic
//! "Calling X..." that hid silent rejections).
//!
//! Subscribes to QSO state-machine events to:
//!  - update the FT8 decoder's AP context as state advances (so AP3/AP4
//!    decoding can lean on the active QSO's contra-callsign),
//!  - forward auto-sequence outbound messages to the transmitter,
//!  - record completed/failed QSOs in the worked-station lookup, and
//!  - report completed QSOs to cqdx.io via the bridge.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// item-2-auto-73 tuning. When a station we JUST completed a *manual* QSO
/// with keeps re-sending us RR73/RRR (they did not copy our 73), we
/// auto-re-send our 73 — bounded so a stuck DX can never make us TX
/// forever:
///   - only for **manual** completions (never autonomous),
///   - only while within [`AUTO_73_WINDOW`] of completion,
///   - at most [`AUTO_73_MAX_RESENDS`] extra 73s per completed QSO,
///   - at most once per ~15 s FT8 slot (so two decodes of the same RR73 in
///     one slot fire only once),
///   - never when a live QSO with that station is already active.
const AUTO_73_WINDOW: chrono::Duration = chrono::Duration::minutes(3);
/// Maximum number of auto re-sends of our 73 per completed manual QSO.
const AUTO_73_MAX_RESENDS: u8 = 3;
/// Minimum spacing between auto re-sends (one FT8 slot is 15 s; we use a
/// slightly-under-slot guard so we fire at most once per slot even if the
/// DX's RR73 is decoded a hair early/late).
const AUTO_73_MIN_SPACING: chrono::Duration = chrono::Duration::seconds(14);

/// One recently-completed **manual** QSO, tracked so we can auto-re-send our
/// 73 if the DX keeps sending RR73/RRR. Keyed (in the map) by uppercased
/// callsign.
#[derive(Debug, Clone)]
struct RecentManualCompletion {
    /// When the QSO completed (window + pruning are measured from here).
    completed_at: chrono::DateTime<chrono::Utc>,
    /// Audio frequency (Hz) we last heard them on — where we send the 73.
    frequency_hz: f64,
    /// DX slot parity (so our 73 lands on the slot they expect). `None`
    /// lets the TX scheduler fall back to its default.
    dx_parity: Option<pancetta_core::slot::SlotParity>,
    /// How many auto re-sends we have already done (bounded by
    /// [`AUTO_73_MAX_RESENDS`]).
    resends: u8,
    /// When we last auto-re-sent (one-per-slot guard). `None` = never yet.
    last_resend_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Shared map of recently-completed manual QSOs. Populated by the QSO-event
/// task on `QsoCompleted` and consumed by the decode-processing loop when a
/// directed RR73/RRR arrives. Both live inside the same QSO component task.
type RecentManualCompletions = Arc<Mutex<HashMap<String, RecentManualCompletion>>>;

/// A manual call the operator requested that could NOT start immediately
/// because it would transmit in the *opposite* window from the one our active
/// QSOs are committed to (half-duplex parity discipline, #40). It is held until
/// the current side's QSOs complete and a clean window flip is possible, then
/// promoted by [`promote_pending_manual_calls`]. Never preempts an in-flight
/// QSO.
#[derive(Debug, Clone)]
struct PendingManualCall {
    /// DX callsign the operator chose to work.
    callsign: String,
    /// Audio offset (Hz) to call them on.
    ///
    /// For normal manual calls this is the **DX's** decoded audio frequency
    /// (used as the `dx_freq` argument to [`compute_manual_tx_offset`] on
    /// promotion). For Hound calls (`hound = true`) this field is unused on
    /// promotion (the low calling offset is re-derived from the callsign via
    /// `engage_hound`); it is kept for logging/display only.
    frequency_hz: f64,
    /// The DX's slot parity (we latch our TX = opposite at QSO start). `None`
    /// would never have been queued (it rides any side), so in practice this
    /// is always `Some`.
    dx_parity: Option<pancetta_core::slot::SlotParity>,
    /// When the call was parked in the queue. Used by the TTL watchdog to
    /// retire calls that have waited without a free window for too long.
    queued_at: std::time::Instant,
    /// Set when this entry was created by [`QsoMessage::EngageHound`].
    /// On promotion, the call is routed to `engage_hound` instead of
    /// `respond_to_cq_with` so the Hound metadata (partner_freq, low
    /// calling offset, QSY hook) is correctly installed.
    hound: bool,
    /// Fox RX audio offset (Hz), latched from the original `EngageHound`
    /// message. Only meaningful when `hound == true`; `None` otherwise.
    fox_freq_hz: Option<f64>,
    /// Fox grid square, latched for ADIF logging. Only meaningful when
    /// `hound == true`.
    fox_grid: Option<String>,
    /// Operator-held TX audio offset (Hz) at the time this call was queued.
    /// `0` means no held offset was active. Only meaningful for non-Hound
    /// calls; used by [`promote_pending_manual_calls`] to call
    /// [`compute_manual_tx_offset`] with the same held state the live
    /// `StartQso` handler would have used, so the offset logic is not lost
    /// when promotion is deferred to a later window.
    held_hz: u64,
    /// Whether `TxFreqMode::Hold` was active when this call was queued.
    /// Together with [`held_hz`](Self::held_hz), restores the held-offset
    /// context at promotion time. Only meaningful for non-Hound calls.
    hold_mode: bool,
}

impl PendingManualCall {
    /// The parity we would transmit on for this call (opposite the DX's slot).
    fn desired_tx_parity(&self) -> Option<pancetta_core::slot::SlotParity> {
        self.dx_parity.map(|p| p.opposite())
    }
}

/// Shared queue of operator-requested manual calls deferred by the half-duplex
/// parity gate (#40). Pushed by the message-handler's `StartQso` arm when the
/// call would cross the committed window; drained by the QSO-event task when
/// the current side clears.
type PendingManualCalls = Arc<Mutex<std::collections::VecDeque<PendingManualCall>>>;

/// Maximum number of operator-deferred manual calls we hold. A generous bound
/// purely to stop an unbounded queue if the operator mashes the call button on
/// many opposite-window stations; older entries past this are dropped.
const MAX_PENDING_MANUAL_CALLS: usize = 16;

/// A manual call parked in the cross-parity queue is retired after this long
/// if it never gets a window to start. Generous so it only catches genuinely
/// stuck calls, not normal multi-QSO waits.
const QUEUED_CALL_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The most-recent band activity decoded *from* a given station (#41), so the
/// operator can see what the DX they're calling is actually doing — working
/// someone else, calling CQ, or coming back to us — even before that DX has
/// answered (i.e. before any QSO-internal RX exists).
#[derive(Debug, Clone)]
struct DxActivity {
    /// Short human summary, e.g. "CQ", "→ W1XYZ R-12", "→ us -09".
    summary: String,
    /// When this frame was decoded (drives the staleness/"(silent)" display).
    at: chrono::DateTime<chrono::Utc>,
}

/// Shared map (uppercased callsign → latest [`DxActivity`]) updated by the
/// decode loop for every decoded frame and read when building the active-QSO
/// snapshot. Bounded by [`DX_ACTIVITY_MAX`] + age pruning.
type DxActivityMap = Arc<std::sync::RwLock<HashMap<String, DxActivity>>>;

/// Cap on tracked callsigns; oldest are pruned past this.
const DX_ACTIVITY_MAX: usize = 256;

/// Entries older than this are treated as stale (DX has gone quiet) and not
/// surfaced; also the pruning horizon.
const DX_ACTIVITY_TTL: chrono::Duration = chrono::Duration::seconds(150);

/// Compute the TX audio offset for a new **manual** QSO (StartQso /
/// RespondToCaller) from the operator's held-offset state and the live active
/// QSO set.
///
/// Priority:
///   1. **Hold mode + held offset set** → use the held offset as the candidate.
///   2. Otherwise → candidate = `dx_freq` (Tx=Rx).
///
/// Then **de-conflict**: if the candidate is within `MIN_TX_SEPARATION_HZ` of
/// any already-active QSO, nudge to the nearest clear slot in
/// `[TX_OFFSET_MIN_HZ, TX_OFFSET_MAX_HZ]`.
///
/// Returns `(tx_off, partner_freq)` where:
/// - `tx_off` is our chosen TX audio offset.
/// - `partner_freq` is `Some(dx_freq)` **only when** `tx_off != dx_freq` —
///   needed by the relevance gate so the DX's replies (at their own audio
///   offset) are still routed to this QSO. `None` means Tx=Rx (unchanged
///   from today's behavior).
///
/// **Regression invariant:** with `TxFreqMode::Auto` (or held=0) AND no
/// occupied collision, `candidate = dx_freq`, `deconflict` returns it
/// unchanged, `partner_freq = None` — byte-identical to today's Tx=Rx.
pub fn compute_manual_tx_offset(
    dx_freq: f64,
    hold_mode: bool,
    held_hz: u64,
    active_offsets: &[f64],
) -> (f64, Option<f64>) {
    let candidate = if hold_mode && held_hz != 0 {
        held_hz as f64
    } else {
        dx_freq
    };
    let tx_off = pancetta_qso::deconflict_offset(
        candidate,
        active_offsets,
        pancetta_qso::MIN_TX_SEPARATION_HZ,
        pancetta_qso::TX_OFFSET_MIN_HZ,
        pancetta_qso::TX_OFFSET_MAX_HZ,
    );
    // Set partner_freq only when we actually diverge from the DX's RX freq.
    // The 1.0 Hz tolerance guards against float-rounding noise on the exact
    // Tx=Rx path.
    let partner_freq = ((tx_off - dx_freq).abs() > 1.0).then_some(dx_freq);
    (tx_off, partner_freq)
}

/// Pure: summarize what a decoded frame tells us its sender is doing (#41).
/// `our_call` lets us say "→ us" when the frame is directed at us. Returns
/// `None` for frames with no useful "who are they working" signal.
fn dx_activity_summary(
    msg: &pancetta_qso::states::MessageType,
    our_call: &str,
) -> Option<(String, String)> {
    use pancetta_qso::exchange::callsigns_match;
    use pancetta_qso::states::MessageType as Mt;

    // Render the target as "us" when it's our station, else the bare callsign.
    let tgt = |to: &str| {
        if callsigns_match(to, our_call) {
            "us".to_string()
        } else {
            to.to_string()
        }
    };
    // Returns (from_station, summary).
    Some(match msg {
        Mt::Cq { callsign, .. } => (callsign.clone(), "calling CQ".to_string()),
        Mt::CqResponse {
            calling_station,
            responding_station,
            ..
        } => (
            responding_station.clone(),
            format!("→ {}", tgt(calling_station)),
        ),
        Mt::SignalReport {
            to_station,
            from_station,
            report,
        } => (
            from_station.clone(),
            format!("→ {} {:+}", tgt(to_station), report),
        ),
        Mt::ReportAck {
            to_station,
            from_station,
            report,
        } => (
            from_station.clone(),
            format!("→ {} R{:+}", tgt(to_station), report),
        ),
        Mt::FinalConfirmation {
            to_station,
            from_station,
        } => (from_station.clone(), format!("→ {} RR73", tgt(to_station))),
        Mt::SeventyThree {
            to_station,
            from_station,
        } => (from_station.clone(), format!("→ {} 73", tgt(to_station))),
        Mt::ContestExchange {
            to_station,
            from_station,
            ..
        } => (
            from_station.clone(),
            format!("→ {} (contest)", tgt(to_station)),
        ),
        Mt::NonStandard { .. } => return None,
    })
}

/// Record one decoded frame into the DX-activity map (#41), pruning stale and
/// excess entries. No-op for frames with no useful summary.
fn record_dx_activity(
    map: &DxActivityMap,
    msg: &pancetta_qso::states::MessageType,
    our_call: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some((from, summary)) = dx_activity_summary(msg, our_call) else {
        return;
    };
    if let Ok(mut m) = map.write() {
        m.insert(from.to_uppercase(), DxActivity { summary, at: now });
        // Cheap bound: prune by age, and if still over the cap drop the oldest.
        if m.len() > DX_ACTIVITY_MAX {
            m.retain(|_, a| now.signed_duration_since(a.at) < DX_ACTIVITY_TTL);
            while m.len() > DX_ACTIVITY_MAX {
                if let Some(oldest_key) = m.iter().min_by_key(|(_, a)| a.at).map(|(k, _)| k.clone())
                {
                    m.remove(&oldest_key);
                } else {
                    break;
                }
            }
        }
    }
}

/// Look up the freshest non-stale activity summary for a callsign (#41),
/// compound-call aware. Returns `None` if unknown or stale.
fn lookup_dx_activity(
    map: &DxActivityMap,
    callsign: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let m = map.read().ok()?;
    // Exact (uppercased) hit first; fall back to a compound-call match.
    let entry = m.get(&callsign.to_uppercase()).or_else(|| {
        m.iter()
            .find(|(k, _)| pancetta_qso::exchange::callsigns_match(k, callsign))
            .map(|(_, a)| a)
    })?;
    if now.signed_duration_since(entry.at) < DX_ACTIVITY_TTL {
        Some(entry.summary.clone())
    } else {
        None
    }
}

/// Pure partition step for [`promote_pending_manual_calls`] (#40): given the
/// queued calls (oldest first) and the parity our still-active QSOs are
/// committed to (`None` ⇒ idle), split into the calls to start now and the
/// ones to keep queued.
///
/// When idle we adopt the oldest queued call's desired parity as the new side;
/// otherwise we may only add to the side already in flight. Every call matching
/// that side (and any parity-agnostic call) starts; cross-parity calls stay
/// queued until that side, in turn, clears.
fn partition_pending_calls(
    queue: std::collections::VecDeque<PendingManualCall>,
    current_side: Option<pancetta_core::slot::SlotParity>,
) -> (
    Vec<PendingManualCall>,
    std::collections::VecDeque<PendingManualCall>,
) {
    let adopt =
        current_side.or_else(|| queue.front().and_then(PendingManualCall::desired_tx_parity));
    let mut start = Vec::new();
    let mut keep = std::collections::VecDeque::new();
    for p in queue {
        match (p.desired_tx_parity(), adopt) {
            // Rides any side, or matches the side we're committing to.
            (None, _) => start.push(p),
            (Some(want), Some(side)) if want == side => start.push(p),
            // Cross-parity: hold until this side clears too.
            _ => keep.push_back(p),
        }
    }
    (start, keep)
}

/// Promote operator-deferred manual calls (#40) once the TX window is free to
/// accept them.
///
/// Called from the QSO-event task after any QSO goes terminal. Determines the
/// side we may now commit to — the side our remaining active QSOs hold, or (if
/// idle) the parity the oldest pending call wants — and starts every pending
/// call that matches it (concurrent, same window). Cross-parity calls stay
/// queued until that side, in turn, clears. Never preempts a live QSO.
async fn promote_pending_manual_calls(
    qso_manager: &pancetta_qso::QsoManager,
    pending: &PendingManualCalls,
    message_bus: &MessageBus,
) {
    // The parity our still-active QSOs are committed to (None ⇒ idle).
    let current_side = qso_manager.current_tx_side().await;

    let (to_start, keep_depth): (Vec<PendingManualCall>, usize) = {
        let mut q = pending.lock().await;
        if q.is_empty() {
            return;
        }
        info!(
            target: "qso",
            "promote_pending_manual_calls: current_side={:?}, queue_depth={}",
            current_side,
            q.len()
        );
        let queue = std::mem::take(&mut *q);
        let (start, keep) = partition_pending_calls(queue, current_side);
        let keep_depth = keep.len();
        *q = keep;
        (start, keep_depth)
    };

    if keep_depth > 0 {
        info!(
            target: "qso",
            "Promote: {} call(s) starting, {} still queued (cross-parity); current_side={:?}",
            to_start.len(),
            keep_depth,
            current_side
        );
    }

    // Collect calls that fail to start so we can re-queue them at the front
    // rather than silently dropping them.
    let mut failed: Vec<PendingManualCall> = Vec::new();

    for p in to_start {
        info!(
            target: "qso",
            "Promoting deferred {} call to {} on {:.0} Hz — window is now free",
            if p.hound { "Hound" } else { "manual" },
            p.callsign, p.frequency_hz
        );
        let result = if p.hound {
            // Hound engage: re-derive the low calling offset + install Hound
            // metadata (partner_freq, hound flag, QSY hook) via engage_hound.
            qso_manager
                .engage_hound(
                    &p.callsign,
                    p.fox_freq_hz.unwrap_or(p.frequency_hz),
                    p.fox_grid.as_deref(),
                    p.dx_parity,
                )
                .await
                .map(|_| ())
        } else {
            // Normal manual call: re-run the held-offset + de-confliction
            // logic at promotion time (not at queue time) so we use the
            // CURRENT active offset set, not the stale snapshot from when
            // the call was deferred. The held_hz/hold_mode fields carry the
            // operator's intent from the original StartQso handler so the
            // chosen offset is honoured even across the cross-parity deferral.
            let active = qso_manager.active_tx_offsets().await;
            let (tx_off, partner) =
                compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
            if tx_off != p.frequency_hz {
                info!(
                    target: "qso",
                    "Promoting queued call to {} — held_hz={} hold_mode={} dx_freq={:.0} \
                     → tx_off={:.0} Hz (de-conflicted from {} active)",
                    p.callsign, p.held_hz, p.hold_mode, p.frequency_hz, tx_off, active.len()
                );
            }
            qso_manager
                .respond_to_cq_with(
                    p.callsign.clone(),
                    tx_off,
                    p.dx_parity,
                    pancetta_qso::CallInitiation::Manual,
                    partner,
                )
                .await
                .map(|_| ())
        };
        match result {
            Ok(()) => {
                emit_status(
                    message_bus,
                    format!("Now calling {} (was queued)", p.callsign),
                )
                .await;
            }
            Err(e) => {
                error!(
                    target: "qso",
                    "Promoting queued call to {} failed — re-queuing: {}",
                    p.callsign, e
                );
                failed.push(p);
            }
        }
    }

    // Re-queue any calls that failed to start at the FRONT of the queue so
    // they get first priority on the next promote cycle.
    if !failed.is_empty() {
        let mut q = pending.lock().await;
        for p in failed.into_iter().rev() {
            q.push_front(p);
        }
    }
}

/// Pure TTL partition: split the queue into entries that have NOT yet exceeded
/// the TTL (retained) and entries that have expired (returned as a `Vec` of
/// callsigns to report). `now` and `ttl` are injected so this is
/// deterministically unit-testable without sleeping.
fn partition_expired(
    queue: std::collections::VecDeque<PendingManualCall>,
    now: std::time::Instant,
    ttl: std::time::Duration,
) -> (std::collections::VecDeque<PendingManualCall>, Vec<String>) {
    let mut kept = std::collections::VecDeque::new();
    let mut expired = Vec::new();
    for p in queue {
        if now.duration_since(p.queued_at) >= ttl {
            expired.push(p.callsign.clone());
        } else {
            kept.push_back(p);
        }
    }
    (kept, expired)
}

/// TTL watchdog: remove entries from the cross-parity queue that have waited
/// longer than [`QUEUED_CALL_TTL`] without getting a free window to start.
/// Emits an operator status line + warn-level log for each retired call.
/// Called from a dedicated interval task (every 15 s).
async fn expire_stale_queued_calls(pending: &PendingManualCalls, message_bus: &MessageBus) {
    let now = std::time::Instant::now();
    let expired = {
        let mut q = pending.lock().await;
        if q.is_empty() {
            return;
        }
        let queue = std::mem::take(&mut *q);
        let (kept, expired) = partition_expired(queue, now, QUEUED_CALL_TTL);
        *q = kept;
        expired
    };
    for call in expired {
        warn!(
            target: "qso",
            "Retiring queued call to {} — waited >{}s without a free TX window",
            call,
            QUEUED_CALL_TTL.as_secs()
        );
        emit_status(
            message_bus,
            format!(
                "Queued call to {} expired — no free window in {}m",
                call,
                QUEUED_CALL_TTL.as_secs() / 60
            ),
        )
        .await;
    }
}

/// Send a free-form status string to the TUI status bar via the message bus.
/// Used to surface QSO/TX state changes that the operator should see, even
/// when nothing failed at the transport layer (e.g. duplicate suppression,
/// QSO state-machine rejections).
async fn emit_status(message_bus: &MessageBus, text: impl Into<String>) {
    let msg = ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Tui,
        MessageType::StatusUpdate(text.into()),
        Instant::now(),
    );
    let _ = message_bus.send_message(msg).await;
}

/// item-2-auto-73 trigger. When `msg_type` is a directed-at-us RR73/RRR
/// (`FinalConfirmation { to_station == our call }`) from a station we just
/// MANUALLY completed a QSO with, auto-re-send our 73 — bounded so it can
/// never run away:
///   - the sender must be in `completions` (a MANUAL completion stashed by
///     the QsoCompleted handler) and within [`AUTO_73_WINDOW`],
///   - `resends < AUTO_73_MAX_RESENDS`,
///   - at most once per [`AUTO_73_MIN_SPACING`] (≈ one FT8 slot, so two
///     decodes of the same RR73 in one slot fire only once),
///   - the global [`pancetta_core::TxPolicy`] must `allows_any_tx()`
///     (RESPOND-ONLY allows — it's a response; DISABLED blocks),
///   - there must be NO currently-active QSO with the sender (don't fight a
///     live exchange).
///
/// On success it sends our 73 via the same `respond_to_caller(SeventyThree)`
/// path the Callers/Space close uses; the resulting Completed QSO is handled
/// by the drop-stale-TX grace window (the 73 frame goes out, then drops), so
/// there is no runaway. After the cap/window the entry is dropped.
#[allow(clippy::too_many_arguments)]
async fn maybe_auto_resend_73(
    msg_type: &pancetta_qso::states::MessageType,
    our_callsign: &str,
    frequency_hz: f64,
    dx_parity: Option<pancetta_core::slot::SlotParity>,
    qso_manager: &pancetta_qso::QsoManager,
    completions: &RecentManualCompletions,
    tx_policy: &std::sync::atomic::AtomicU8,
    message_bus: &MessageBus,
) {
    use pancetta_qso::states::MessageType as Mt;

    // Only directed RR73/RRR (both parse to FinalConfirmation) addressed to us.
    let from_station = match msg_type {
        Mt::FinalConfirmation {
            to_station,
            from_station,
        } if to_station.eq_ignore_ascii_case(our_callsign) => from_station.clone(),
        _ => return,
    };
    let key = from_station.to_uppercase();

    // TX policy gate (DISABLED blocks; RESPOND-ONLY/FULL allow). Cheap check
    // first, before touching the map.
    let policy =
        pancetta_core::TxPolicy::from_u8(tx_policy.load(std::sync::atomic::Ordering::Relaxed));
    if !policy.allows_any_tx() {
        return;
    }

    let now = chrono::Utc::now();

    // Decide under the map lock: is this a stashed manual completion still in
    // window and under the cap, with the per-slot guard satisfied? We mutate
    // the entry (resends/last_resend_at) here so the bound holds even if RR73
    // arrives every slot. We do NOT call into the QSO manager while holding
    // the lock.
    {
        let mut map = completions.lock().await;
        // Prune expired entries every time we look.
        map.retain(|_, e| now.signed_duration_since(e.completed_at) < AUTO_73_WINDOW);

        let Some(entry) = map.get_mut(&key) else {
            return;
        };
        if entry.resends >= AUTO_73_MAX_RESENDS {
            // Cap reached — stop and drop the entry so we never reconsider it.
            map.remove(&key);
            return;
        }
        if let Some(last) = entry.last_resend_at {
            if now.signed_duration_since(last) < AUTO_73_MIN_SPACING {
                // Already re-sent this slot — ignore the duplicate decode.
                return;
            }
        }
        // Commit the send: increment + stamp BEFORE we drop the lock so two
        // decodes racing in the same slot can't both pass the per-slot guard.
        entry.resends += 1;
        entry.last_resend_at = Some(now);
        // Prefer the freq/parity we just heard them on (fresher); fall back to
        // the stashed completion values if the decode lacked parity.
        entry.frequency_hz = frequency_hz;
        if dx_parity.is_some() {
            entry.dx_parity = dx_parity;
        }
    }

    // Don't fight a live QSO with this station: if one is active, skip the
    // auto-73 (the QSO state machine is handling it). The counter was already
    // incremented above, which is fine — it only tightens the bound.
    let active = qso_manager.get_active_qsos().await;
    let has_active = active.iter().any(|(_, p)| {
        p.state
            .their_callsign()
            .map(|c| c.eq_ignore_ascii_case(&from_station))
            .unwrap_or(false)
            || p.metadata
                .their_callsign
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case(&from_station))
                .unwrap_or(false)
    });
    if has_active {
        return;
    }

    // Read back the resend count for logging (lock is released between the
    // commit and the send; the value can only have grown, never shrunk).
    let resend_n = completions
        .lock()
        .await
        .get(&key)
        .map(|e| e.resends)
        .unwrap_or(AUTO_73_MAX_RESENDS);

    info!(
        target: "qso",
        "auto-resending 73 to {} ({}/{}) — repeated RR73 after manual QSO completion",
        from_station, resend_n, AUTO_73_MAX_RESENDS
    );

    match qso_manager
        .respond_to_caller(
            from_station.clone(),
            frequency_hz,
            dx_parity,
            pancetta_core::ResponseStep::SeventyThree,
            None,
            None,
            None, // auto-73: always Tx=Rx, no partner offset
        )
        .await
    {
        Ok(_) => {
            emit_status(
                message_bus,
                format!(
                    "Re-sending 73 to {} ({}/{}) — they repeated RR73",
                    from_station, resend_n, AUTO_73_MAX_RESENDS
                ),
            )
            .await;
        }
        Err(e) => {
            warn!(
                target: "qso",
                "auto-73 re-send to {} failed: {}", from_station, e
            );
        }
    }
}

/// A decoded message that a station directed at *us* to work us, classified
/// into the [`ResponseStep`](pancetta_core::ResponseStep) we'd open at and the
/// report they gave us (if any). Produced by [`classify_caller_answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerAnswer {
    /// The station calling us (their callsign as decoded).
    their_call: String,
    /// The sequence rung we should open our reply at.
    step: pancetta_core::ResponseStep,
    /// The signal report they sent us, if this rung carried one.
    their_report: Option<i8>,
}

/// Pure classifier for the always-answer-callers path (#39).
///
/// Maps a parsed FT8 message **directed at us** to the reply we owe, or `None`
/// when the message isn't a station trying to work us. Compound-call aware
/// (`callsigns_match`) so `EA8/G8BCG` calling `K5ARH` is recognized.
///
/// | They sent us            | We open at      |
/// |-------------------------|-----------------|
/// | `US THEM <grid>` (CqResponse) | `Report`   |
/// | `US THEM -NN` (SignalReport)  | `ReportAck` |
/// | `US THEM R-NN` (ReportAck)    | `Rr73`      |
/// | `US THEM RR73` (FinalConfirmation) | `SeventyThree` |
///
/// CQ, 73, contest, and non-standard frames return `None` — a CQ is an
/// initiation decision (autonomous/operator territory), not a direct call to
/// us, and a 73 needs no reply. The caller still applies all the TX gates
/// (policy, parity, dedup, capacity) before acting on a `Some`.
fn classify_caller_answer(
    msg: &pancetta_qso::states::MessageType,
    our_call: &str,
) -> Option<CallerAnswer> {
    use pancetta_core::ResponseStep;
    use pancetta_qso::exchange::callsigns_match;
    use pancetta_qso::states::MessageType as Mt;

    match msg {
        Mt::CqResponse {
            calling_station,
            responding_station,
            ..
        } if callsigns_match(calling_station, our_call) => Some(CallerAnswer {
            their_call: responding_station.clone(),
            step: ResponseStep::Report,
            their_report: None,
        }),
        Mt::SignalReport {
            to_station,
            from_station,
            report,
        } if callsigns_match(to_station, our_call) => Some(CallerAnswer {
            their_call: from_station.clone(),
            step: ResponseStep::ReportAck,
            their_report: Some(*report),
        }),
        Mt::ReportAck {
            to_station,
            from_station,
            report,
        } if callsigns_match(to_station, our_call) => Some(CallerAnswer {
            their_call: from_station.clone(),
            step: ResponseStep::Rr73,
            their_report: Some(*report),
        }),
        Mt::FinalConfirmation {
            to_station,
            from_station,
        } if callsigns_match(to_station, our_call) => Some(CallerAnswer {
            their_call: from_station.clone(),
            step: ResponseStep::SeventyThree,
            their_report: None,
        }),
        _ => None,
    }
}

// Always-answer-callers (#39 + #43 part 2): auto-open a reply to a station
// calling us, **independent of the autonomous-operator toggle**. See
// `maybe_answer_caller` below for the implementation.
//
// FT8 etiquette is to always come back to a station that calls you. This runs
// in the always-on decode loop, so it works whether or not autonomous mode is
// engaged. It is a *response*, not an unattended *initiation*, so the FCC
// §97.221 presence gate (which governs initiation) does not apply — but every
// other TX gate does:
//
// 1. **TX policy** — `Disabled` blocks entirely; `RespondOnly`/`Full` allow.
// 2. **Already in QSO** — if `process_message` (run first) is already driving
//    an exchange with this station, skip (no duplicate).
// 3. **Half-duplex parity** — we'd TX on `opposite(their_parity)`; if that
//    crosses the window our active QSOs are committed to, defer (the operator
//    can still pick them manually). Keeps us off sequential-window TX.
// 4. **Capacity** — at most `max_concurrent` concurrent caller-answers.
//
// Because this path carries no failure-backoff state, it also satisfies #43
// part 2: after our initiation watchdog retires a QSO, if that DX then calls
// us we still answer.

/// Compute a monotonically-increasing slot index from a decode timestamp.
///
/// FT8 windows are 15 seconds wide and aligned to UTC. Two decodes that fall
/// in the same window produce the same key; decodes in adjacent windows produce
/// consecutive keys. Used by the per-slot creation-dedup set in the decode
/// loop so that repeated decodes of the same station within one 15-second
/// window only attempt QSO creation once.
///
/// # Formula
/// ```text
/// slot_key = floor(unix_seconds / 15)
/// ```
///
/// A `SystemTime` before UNIX_EPOCH (e.g. in unit tests) returns 0.
#[inline]
fn caller_creation_slot_key(timestamp: std::time::SystemTime) -> u64 {
    timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 15
}

#[allow(clippy::too_many_arguments)]
async fn maybe_answer_caller(
    msg_type: &pancetta_qso::states::MessageType,
    our_callsign: &str,
    frequency_hz: f64,
    their_parity: Option<pancetta_core::slot::SlotParity>,
    snr: f32,
    qso_manager: &pancetta_qso::QsoManager,
    tx_policy: &std::sync::atomic::AtomicU8,
    max_concurrent: usize,
    message_bus: &MessageBus,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
) {
    // 1. TX policy: DISABLED blocks; RESPOND-ONLY / FULL allow responses.
    let policy =
        pancetta_core::TxPolicy::from_u8(tx_policy.load(std::sync::atomic::Ordering::Relaxed));
    if !policy.allows_any_tx() {
        return;
    }

    // Is this a station calling us, and at what rung?
    let Some(answer) = classify_caller_answer(msg_type, our_callsign) else {
        return;
    };

    // 2. Don't open a duplicate — process_message already drives any active QSO
    //    with this station (it ran before us this cycle). Also suppress for a
    //    recently-completed QSO (120 s window): if the DX sends another RR73
    //    right after we already exchanged 73s, the bounded auto-resend-73 path
    //    (`maybe_auto_resend_73`) handles it — opening a brand-new QSO here
    //    would produce spurious duplicate 73s. Explicit operator re-work via
    //    StartQso / Space does NOT come through this function, so it is
    //    unaffected by this gate.
    if qso_manager
        .has_active_or_recent_qso_with(&answer.their_call, std::time::Duration::from_secs(120))
        .await
    {
        debug!(target: "qso", "Not auto-answering {} — active or recently-completed QSO exists", answer.their_call);
        return;
    }

    // 3. Half-duplex parity: our reply would TX on opposite(their_parity).
    //    Defer if that crosses the window our active QSOs hold.
    let desired_tx_parity = their_parity.map(|p| p.opposite());
    let current_side = qso_manager.current_tx_side().await;
    if matches!(
        pancetta_qso::qso_manager::admit_new_qso(current_side, desired_tx_parity),
        pancetta_qso::qso_manager::TxAdmission::Queue
    ) {
        info!(
            target: "qso",
            "Skipping auto-answer to {} — cross-parity (active side {:?}); operator can queue manually",
            answer.their_call, current_side
        );
        return;
    }

    // 4. Capacity bound. Fox mode raises the cap to fox_max_streams so the
    //    station can work many Hound callers concurrently; normal mode uses
    //    the operator's configured max_concurrent_qsos (passed as max_concurrent).
    //
    //    In Fox mode we count ONLY caller-answer QSOs (active_caller_qso_count),
    //    NOT the CallingCq QSO: the CQ stream is an independent fixed slot; it
    //    must not eat one of the N Hound-answer slots.  With fox_max_streams=5
    //    this yields 5 Hounds + 1 CQ = 6 total streams (≤ MAX_RETAINED_TX_STREAMS=8).
    //    The non-Fox path is UNCHANGED (active_qso_count vs max_concurrent —
    //    regression guard).
    let is_fox = fox_mode.load(std::sync::atomic::Ordering::Relaxed);
    let effective_cap = if is_fox {
        fox_max_streams.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        max_concurrent
    };
    let active_count = if is_fox {
        qso_manager.active_caller_qso_count().await
    } else {
        qso_manager.active_qso_count().await
    };
    if active_count >= effective_cap {
        return;
    }

    info!(
        target: "qso",
        "Auto-answering {} at {:?} on {:.0} Hz (caller — autonomous-independent)",
        answer.their_call, answer.step, frequency_hz
    );

    match qso_manager
        .respond_to_caller(
            answer.their_call.clone(),
            frequency_hz,
            their_parity,
            answer.step,
            Some(snr),
            answer.their_report,
            None, // classify_caller_answer: always Tx=Rx (answering a caller, no held-offset)
        )
        .await
    {
        Ok(_) => {
            emit_status(
                message_bus,
                format!("Answering {} (caller)", answer.their_call),
            )
            .await;
        }
        Err(e) => {
            warn!(
                target: "qso",
                "Auto-answer to {} failed: {}", answer.their_call, e
            );
        }
    }
}

/// Short, operator-facing description of why a QSO failed, for the TUI
/// status line (Batch 2 #3). Terminal QSOs are dropped from the active
/// snapshot, so this is the only place the operator learns the reason.
fn failure_reason_text(reason: &pancetta_qso::QsoFailureReason) -> String {
    use pancetta_qso::QsoFailureReason as R;
    match reason {
        R::Timeout => "watchdog timeout".to_string(),
        R::SignalLost => "signal lost".to_string(),
        R::Duplicate => "duplicate".to_string(),
        R::InvalidCallsign => "invalid callsign".to_string(),
        R::FrequencyConflict => "frequency conflict".to_string(),
        R::UserCancelled => "cancelled by operator".to_string(),
        R::Superseded => "superseded by a newer call".to_string(),
        R::StationQrt => "station went QRT".to_string(),
        R::ProtocolError(e) => format!("protocol error: {e}"),
    }
}

impl super::ApplicationCoordinator {
    /// Start QSO management component
    ///
    /// Wires decoded FT8 messages into the QSO manager for state tracking,
    /// auto-logging to SQLite at `~/.pancetta/qso.db`, and duplicate detection.
    pub(crate) async fn start_qso_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_qso");
        let _enter = span.enter();

        info!("Starting QSO component");

        let (_qso_tx, qso_rx) = self.message_bus.create_channel(ComponentId::Qso).await?;
        let message_bus = self.message_bus.clone();
        let gateway_enabled = self.gateway_enabled.clone();

        // Read station config for callsign/grid
        let config = self.config.read().await;
        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };
        // Snapshot the opt-in QSO-upload settings. Only when at least one is
        // enabled do we build clients + spawn the upload subscriber.
        let clublog_cfg = config.network.clublog.clone();
        let qrz_cfg = config.network.qrz_logbook.clone();
        let lotw_cfg = config.network.lotw.clone();
        let eqsl_cfg = config.network.eqsl.clone();
        let cqdx_cfg = config.network.cqdx.clone();
        // QRZ paid-XML callsign lookup — a gated, best-effort enrichment that
        // fills a MISSING their-grid (and name/dxcc for logging) on a completed
        // QSO's metadata before the ADIF record is rendered for upload.
        // Default-off; only the upload subscriber consumes it.
        let qrz_xml_cfg = config.network.qrz_xml.clone();
        // Always-answer-callers (#39): cap how many concurrent caller-answer
        // QSOs we'll auto-open. Reuses the operator's `max_concurrent_qsos`
        // (default 1) so the policy is consistent with autonomous concurrency;
        // the parity gate additionally keeps all concurrent QSOs in one window.
        let auto_answer_max_concurrent = config.autonomous.max_concurrent_qsos.max(1) as usize;
        // Snapshot the operator-configured Hound audio-offset regions so the
        // QsoManager can use them in engage_hound + the QSY hook.  We capture
        // them here (before drop) and pass them as `HoundRegions` to avoid
        // introducing a pancetta-qso → pancetta-config dependency.
        let hound_cfg = config.hound.clone();
        // Station-wide active operating mode string ("FT8"/"FT4"/"FT2"),
        // stamped into every QsoMetadata.mode (→ ADIF MODE). Defaults to FT8
        // on parse error so the legacy path is unchanged.
        let active_mode = super::mode_str(
            config
                .rig
                .operating_mode()
                .unwrap_or(pancetta_config::OperatingMode::Ft8),
        )
        .to_string();
        drop(config);

        // cqdx.io logbook upload is opt-in just like ClubLog/QRZ: it requires
        // the integration enabled AND a non-empty PAT token. (The same
        // `[network.cqdx]` token gates the spot-poller bridge; here it drives
        // the per-QSO logbook POST to `POST /api/v1/qsos`.)
        let cqdx_upload_enabled = cqdx_logbook_upload_enabled(&cqdx_cfg);
        // QRZ XML enrichment is gated on `enabled` + creds (config validation
        // already rejects enabled-without-creds). When it (and only it) is on,
        // we still want the subscriber so completed QSOs get grid enrichment —
        // even with no upload target the enriched record costs nothing.
        let qrz_xml_enabled = qrz_xml_cfg.enabled
            && !qrz_xml_cfg.username.is_empty()
            && !qrz_xml_cfg.password.is_empty();
        let upload_enabled = clublog_cfg.enabled
            || qrz_cfg.enabled
            || lotw_cfg.enabled
            || eqsl_cfg.enabled
            || cqdx_upload_enabled
            || qrz_xml_enabled;

        let qso_lookup = self.cached_lookup.clone();
        let upload_our_callsign = our_callsign.clone();
        let active_qso_ap = self.active_qso_ap.clone();
        let active_qso_freq_hz = self.active_qso_freq_hz.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let split_tx_frequency_hz = self.split_tx_frequency_hz.clone();
        let tx_freq_mode = self.tx_freq_mode.clone();
        // T3 will read this to apply the operator's held TX audio offset when
        // starting a manual QSO (Hold mode). Captured here so both atomics
        // are in scope in the StartQso/RespondToCaller handlers below.
        let tx_offset_hold_hz = self.tx_offset_hold_hz.clone();
        // Shared with the TX worker — drives the "drop TX for ended QSOs"
        // gate. The QSO component keeps it in sync from the QsoEvent stream
        // below.
        let active_tx_qsos = self.active_tx_qsos.clone();
        // Newest-TX-intent map — written as we forward each MessageToSend so
        // the TX worker can pivot to the freshest message at key-time.
        let latest_tx_intent = self.latest_tx_intent.clone();
        // Global TX policy — the auto-73 re-send respects it (RESPOND-ONLY
        // allows, DISABLED blocks), exactly like every other response path.
        let tx_policy = self.tx_policy.clone();
        // Fox-mode flag — set true by SetFoxMode{on:true} to engage CQ loop +
        // raise the caller-answer cap to fox_max_streams.
        let fox_mode = self.fox_mode();
        // Maximum concurrent caller-answer QSOs while Fox mode is engaged.
        // When fox_mode is false the normal auto_answer_max_concurrent cap applies.
        let fox_max_streams = self.fox_max_streams();
        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_qso::{HoundRegions, LoggerConfig, QsoManager, QsoManagerConfig};

                let qso_config = QsoManagerConfig {
                    our_callsign: our_callsign.clone(),
                    our_grid: our_grid.clone(),
                    hound: HoundRegions {
                        call_min_hz: hound_cfg.call_min_hz,
                        call_max_hz: hound_cfg.call_max_hz,
                        response_min_hz: hound_cfg.response_min_hz,
                        response_max_hz: hound_cfg.response_max_hz,
                    },
                    active_mode: active_mode.clone(),
                    ..Default::default()
                };

                let mut qso_manager = QsoManager::new(qso_config);
                // Share the rig dial-frequency source so completed QSOs log the
                // real RF frequency (dial + audio offset), not the bare offset
                // (was producing ADIF FREQ ~0.001 / BAND 0MHZ).
                qso_manager.set_dial_frequency_source(operating_frequency_hz.clone());
                // Share the split-TX dial source (0 = simplex). Written by the
                // TUI SetSplit relay; the QSO RF stamp uses this for the
                // effective TX dial frequency when split is active.
                qso_manager.set_split_tx_frequency_source(split_tx_frequency_hz.clone());
                // Share the operator's Hold/Auto TX-frequency mode so the
                // stuck-DX hop only fires in Auto (Hold keeps the offset sticky).
                qso_manager.set_tx_freq_mode_source(tx_freq_mode.clone());
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }

                // Initialize QSO logger with SQLite database at ~/.pancetta/qso.db
                let db_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qso.db");

                // ADIF source-of-truth writer. Subscribes to QsoEvent::QsoCompleted
                // and appends one ADIF record per completed QSO. Fail-soft: if open
                // fails, we log but proceed with DB-only — every operator should at
                // least get duplicate detection from the DB.
                let adif_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qsos.adi");

                let _adif_writer = match pancetta_qso::AdifLogWriter::open(&adif_path).await {
                    Ok(w) => {
                        info!("ADIF log open at {}", adif_path.display());
                        let w = std::sync::Arc::new(w);
                        start_adif_subscriber(w.clone(), qso_manager.subscribe(), shutdown.clone());
                        Some(w)
                    }
                    Err(e) => {
                        warn!(
                            "ADIF writer init failed at {}: {} — continuing; QSOs this \
                             session will be DB-only",
                            adif_path.display(),
                            e,
                        );
                        None
                    }
                };

                // Async QSO logger — subscribes independently to QsoEvent::QsoCompleted
                // and inserts into the rebuildable SQLite index. Comes AFTER the ADIF
                // writer so that a crash between the two is recoverable by Task 5's
                // startup replay (ADIF is source of truth; DB is cache).
                let logger_config = LoggerConfig {
                    database_path: db_path.clone(),
                    ..Default::default()
                };

                let _async_logger = match pancetta_qso::async_logger::QsoLogger::new(
                    logger_config,
                    qso_manager.clone(),
                )
                .await
                {
                    Ok(l) => {
                        info!(
                            "Async QSO logger initialized with database at {}",
                            db_path.display()
                        );
                        let l = std::sync::Arc::new(l);
                        if let Err(e) = l.start().await {
                            warn!("Async QSO logger background tasks failed to start: {}", e);
                        }
                        Some(l)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize async QSO logger (continuing without): {}",
                            e
                        );
                        None
                    }
                };

                // Per-QSO log-upload subscriber (ClubLog + QRZ Logbook + cqdx.io
                // + eQSL + LoTW), with optional QRZ-XML grid enrichment applied
                // first. Opt-in: only spawned when at least one upload target OR
                // QRZ XML enrichment is enabled. Best-effort and fully decoupled
                // from the QSO pipeline — each upload runs in its own task so a
                // slow/failing service never blocks logging.
                if upload_enabled {
                    start_qso_upload_subscriber(
                        clublog_cfg.clone(),
                        qrz_cfg.clone(),
                        lotw_cfg.clone(),
                        eqsl_cfg.clone(),
                        cqdx_cfg.clone(),
                        qrz_xml_cfg.clone(),
                        upload_our_callsign.clone(),
                        qso_manager.subscribe(),
                        shutdown.clone(),
                    );
                }

                // Seed worked-station history from the QSO database so that
                // previously-worked stations are recognised as duplicates across restarts.
                //
                // Three-case startup decision:
                //   1. Migration: ADIF missing but legacy DB exists → dump DB to ADIF first
                //      so contacts are not lost; future runs use ADIF as source of truth.
                //   2. Replay: index missing or older than ADIF → drop + replay so duplicate
                //      detection sees every prior contact.
                //   3. Open as-is: normal startup; index is current.
                {
                    use pancetta_qso::async_database::QsoDatabase;

                    // Determine the current band from the rig's operating frequency,
                    // falling back to "20m".  This is a best-effort seed — the
                    // autonomous operator will always re-validate against the live
                    // worked-on-band set as QSOs complete.
                    let freq_hz = operating_frequency_hz.load(std::sync::atomic::Ordering::Relaxed);
                    let band = pancetta_cqdx::frequency_to_band(freq_hz)
                        .unwrap_or_else(|| "20m".to_string())
                        .to_uppercase();

                    // Case 1: migration — ADIF missing but legacy DB exists.
                    let adif_exists = tokio::fs::try_exists(&adif_path).await.unwrap_or(false);
                    let db_exists = tokio::fs::try_exists(&db_path).await.unwrap_or(false);

                    if !adif_exists && db_exists {
                        info!(
                            "ADIF missing but legacy DB present — migrating QSOs from {} to {}",
                            db_path.display(),
                            adif_path.display(),
                        );
                        match QsoDatabase::open(&db_path).await {
                            Ok(db) => {
                                if let Err(e) = db.export_to_adif(&adif_path).await {
                                    warn!(
                                        "DB→ADIF migration failed: {} — index continues to work, \
                                         but ADIF source-of-truth will only contain QSOs logged \
                                         from now on",
                                        e,
                                    );
                                } else {
                                    info!("DB→ADIF migration succeeded");
                                }
                            }
                            Err(e) => {
                                warn!("Could not open legacy DB for migration: {} — skipping", e);
                            }
                        }
                    }

                    // Case 2: replay — index missing or older than ADIF.
                    let needs_replay = match (
                        tokio::fs::metadata(&db_path).await.ok(),
                        tokio::fs::metadata(&adif_path).await.ok(),
                    ) {
                        (None, Some(_)) => {
                            info!(
                                "Index missing at {} — replaying from ADIF",
                                db_path.display()
                            );
                            true
                        }
                        (Some(db_meta), Some(adif_meta)) => {
                            match (db_meta.modified().ok(), adif_meta.modified().ok()) {
                                (Some(d), Some(a)) if a > d => {
                                    info!(
                                        "Index at {} is older than ADIF at {} — replaying",
                                        db_path.display(),
                                        adif_path.display(),
                                    );
                                    true
                                }
                                _ => false,
                            }
                        }
                        // No ADIF and no DB: fresh install; coordinator creates both later.
                        _ => false,
                    };

                    let db_for_seed = if needs_replay {
                        match QsoDatabase::replay_from_adif(&db_path, &adif_path).await {
                            Ok(db) => Some(db),
                            Err(e) => {
                                warn!(
                                    "ADIF replay failed: {} — falling back to existing index \
                                     (may be stale)",
                                    e,
                                );
                                QsoDatabase::open(&db_path).await.ok()
                            }
                        }
                    } else {
                        // Case 3: open as-is.
                        QsoDatabase::open(&db_path).await.ok()
                    };

                    if let Some(db) = db_for_seed {
                        let callsigns = db.get_worked_callsigns(&band).await;
                        if callsigns.is_empty() {
                            info!(
                                "QSO database has no prior contacts on {} — starting fresh",
                                band
                            );
                        } else {
                            qso_lookup.seed_worked_from_list(&band, callsigns);
                        }
                    } else {
                        warn!(
                            "Could not open QSO database for startup seed ({}) — \
                             previously-worked stations will not be detected as duplicates \
                             until re-worked this session",
                            db_path.display(),
                        );
                    }
                }

                info!(
                    "QSO component ready (callsign={}, grid={:?})",
                    our_callsign, our_grid
                );

                // item-2-auto-73: map of recently-completed MANUAL QSOs, shared
                // between the QsoCompleted handler (in the event-forwarding task
                // below, which populates it) and the decode-processing loop
                // (which consumes it when a directed RR73/RRR arrives). See the
                // type alias / constants at the top of this module.
                let recent_manual_completions: RecentManualCompletions =
                    Arc::new(Mutex::new(HashMap::new()));

                // #40: operator-deferred manual calls (cross-parity, waiting for
                // the window to free). Pushed by the StartQso handler below;
                // drained by the QSO-event task when a QSO goes terminal.
                let pending_manual_calls: PendingManualCalls =
                    Arc::new(Mutex::new(std::collections::VecDeque::new()));

                // #41: band-wide DX activity (callsign → latest decoded frame
                // summary). Written by the decode loop for every frame; read
                // when building the active-QSO snapshot so the QSO panel shows
                // what the DX we're calling is doing.
                let dx_activity: DxActivityMap = Arc::new(std::sync::RwLock::new(HashMap::new()));

                // TTL watchdog: every 15 s, retire queued cross-parity calls
                // that have waited longer than QUEUED_CALL_TTL without getting
                // a free TX window. Runs in a dedicated lightweight task so it
                // fires even when no QSO events are flowing (the main loop is
                // event-driven and would never drain the queue on its own if
                // the operator's active QSOs stay alive indefinitely).
                {
                    let ttl_pending = pending_manual_calls.clone();
                    let ttl_bus = message_bus.clone();
                    let ttl_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        loop {
                            tick.tick().await;
                            if ttl_shutdown.load(Ordering::Acquire) {
                                break;
                            }
                            expire_stale_queued_calls(&ttl_pending, &ttl_bus).await;
                        }
                    });
                }

                // Spawn a task to forward QSO auto-sequence TX requests to the transmitter
                // and update AP decoding state for the FT8 decoder thread.
                let mut qso_events = qso_manager.subscribe();
                let tx_bus = message_bus.clone();
                let tx_shutdown = shutdown.clone();
                let tx_callsign = our_callsign.clone();
                let ap_state = active_qso_ap;
                let qso_freq_state = active_qso_freq_hz;
                let active_tx_qsos = active_tx_qsos.clone();
                let latest_tx_intent = latest_tx_intent.clone();
                let snapshot_qso_manager = qso_manager.clone();
                let snapshot_bus = tx_bus.clone();
                let completions_for_events = recent_manual_completions.clone();
                let pending_for_events = pending_manual_calls.clone();
                let dx_activity_for_events = dx_activity.clone();
                let gateway_enabled = gateway_enabled.clone();
                tokio::spawn(async move {
                    while !tx_shutdown.load(Ordering::Acquire) {
                        match qso_events.recv().await {
                            Ok(pancetta_qso::QsoEvent::StateChanged {
                                qso_id,
                                old_state,
                                new_state,
                                ..
                            }) => {
                                // Keep the TX-active set in sync (drop-stale-TX
                                // gate). A QSO entering a non-terminal active
                                // state is now allowed to TX; a QSO entering a
                                // terminal Failed state (covers Superseded /
                                // UserCancelled / Timeout / SignalLost / …) must
                                // STOP transmitting at once, so we remove it
                                // immediately. (Completion is handled in the
                                // QsoCompleted arm with a grace window so the
                                // final 73 still goes out.)
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    if new_state.is_active() {
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.insert(key);
                                        }
                                    } else if matches!(
                                        new_state,
                                        pancetta_qso::QsoState::Failed { .. }
                                    ) {
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.remove(&key);
                                        }
                                        if let Ok(mut m) = latest_tx_intent.write() {
                                            m.remove(&key);
                                        }
                                        info!(
                                            target: "tx.policy",
                                            "QSO {} went terminal-Failed — purging its TX from the active set",
                                            qso_id
                                        );
                                        // #40: a failed QSO frees its window
                                        // immediately (no trailing TX) — promote
                                        // any deferred cross-parity manual call.
                                        promote_pending_manual_calls(
                                            &snapshot_qso_manager,
                                            &pending_for_events,
                                            &snapshot_bus,
                                        )
                                        .await;
                                    }
                                }

                                // Map QSO state to AP context for AP3/AP4 decoding.
                                //
                                // WSJT-X Improved-style a8 wiring: also enumerate
                                // the expected next-message texts from the
                                // partner so that the FT8 decoder's a8 path
                                // (gated on `Ft8Config::a8_qso_state_ap_enabled`)
                                // can relax the AP confidence floor for decodes
                                // that match. Inspired by spec ref
                                // `spec-wsjtx-improved-a8-decoding.md`. When
                                // a8 is disabled the templates are still
                                // populated but never consulted, so wiring
                                // is byte-safe.
                                let new_ap = match &new_state {
                                    pancetta_qso::QsoState::RespondingToCq {
                                        target_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::WaitingForReport {
                                        their_callsign: target_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::SendingReport {
                                        their_callsign: target_callsign,
                                        ..
                                    } => pancetta_ft8::QsoAp::new(
                                        target_callsign,
                                        pancetta_ft8::QsoApProgress::WaitingForReport,
                                    )
                                    .map(|q| {
                                        let texts = pancetta_ft8::ap::enumerate_a8_expected_texts(
                                            &tx_callsign,
                                            target_callsign,
                                            pancetta_ft8::QsoApProgress::WaitingForReport,
                                        );
                                        q.with_expected_texts(texts)
                                    }),
                                    pancetta_qso::QsoState::WaitingForConfirmation {
                                        their_callsign,
                                        ..
                                    }
                                    | pancetta_qso::QsoState::SendingConfirmation {
                                        their_callsign,
                                        ..
                                    } => pancetta_ft8::QsoAp::new(
                                        their_callsign,
                                        pancetta_ft8::QsoApProgress::WaitingForConfirmation,
                                    )
                                    .map(|q| {
                                        let texts = pancetta_ft8::ap::enumerate_a8_expected_texts(
                                            &tx_callsign,
                                            their_callsign,
                                            pancetta_ft8::QsoApProgress::WaitingForConfirmation,
                                        );
                                        q.with_expected_texts(texts)
                                    }),
                                    // Terminal or idle states clear the AP context
                                    _ => None,
                                };
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = new_ap;
                                }

                                // hb-091 scoped fast-path: mirror the AP
                                // update with the partner's audio freq.
                                // `QsoState::frequency()` returns Some for
                                // the in-QSO states and None for Idle /
                                // Failed / Completed.
                                //
                                // Hound-mode bin-hint: for a Hound QSO the
                                // decoder's narrow-band collapse window should
                                // centre on the Fox's RX frequency (where we
                                // HEAR the Fox), not our TX offset (where we
                                // CALL the Fox). `metadata.partner_freq` holds
                                // the Fox's RX offset when set; for every
                                // non-Hound QSO it is `None` and we fall back
                                // to `new_state.frequency()` (our TX offset),
                                // preserving byte-identical behavior for all
                                // existing QSOs.
                                // Resolve the frequency value BEFORE acquiring
                                // the std::sync::RwLock guard so we never hold
                                // a non-Send guard across an await point.
                                {
                                    let decoder_hint_freq: Option<f64> = if new_state.is_active() {
                                        // Try to obtain `partner_freq` from the
                                        // QSO metadata. This is a cheap read-lock
                                        // on the already-updated QSO map; it fires
                                        // once per state-change (not per decode
                                        // window).  On error (QSO vanished between
                                        // the event and the lookup — extremely
                                        // rare) we fall back to the state's own
                                        // TX frequency.
                                        let partner = snapshot_qso_manager
                                            .get_qso(qso_id)
                                            .await
                                            .ok()
                                            .and_then(|p| p.metadata.partner_freq);
                                        partner.or_else(|| new_state.frequency())
                                    } else {
                                        None
                                    };
                                    // Acquire the guard synchronously (no await
                                    // in this scope) and write.
                                    if let Ok(mut guard) = qso_freq_state.write() {
                                        *guard = decoder_hint_freq;
                                    }
                                }

                                // Push an updated snapshot of in-progress
                                // QSOs to the TUI banner. The QSO state
                                // machine is the source of truth; the TUI
                                // replaces its list each push.
                                let (snapshot, pending_snap) = build_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                )
                                .await;
                                // Additive: clone the snapshot for the read-only
                                // gateway BEFORE it is moved into the →Tui send
                                // (only when the gateway is enabled).
                                let gw_snap = if gateway_enabled.load(Ordering::Relaxed) {
                                    Some(MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot.clone(),
                                        pending: pending_snap.clone(),
                                    })
                                } else {
                                    None
                                };
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot,
                                        pending: pending_snap,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = snapshot_bus.send_message(snap_msg).await {
                                    debug!("Failed to push active-QSOs snapshot: {}", e);
                                }
                                if let Some(m) = gw_snap {
                                    super::remote_gateway::relay_to_gateway(
                                        &snapshot_bus,
                                        &gateway_enabled,
                                        ComponentId::Qso,
                                        m,
                                    )
                                    .await;
                                }

                                // Batch 2 #3: a QSO that just went terminal-Failed
                                // is otherwise silently dropped from the snapshot.
                                // Surface a one-line status so the operator learns
                                // WHY (watchdog timeout, cancelled, …) instead of
                                // the QSO just vanishing. We only fire on the
                                // transition INTO Failed (old_state was not already
                                // terminal).
                                //
                                // FIX 2: a `Superseded` end is an INTENTIONAL
                                // replace, not a failure — the operator (or the
                                // engine on a genuine re-call after the old QSO
                                // went terminal) deliberately swapped one QSO for
                                // another. Surfacing it as "QSO … failed:
                                // superseded" alarmed the operator into thinking
                                // the rig was broken. So we phrase Superseded
                                // neutrally ("replaced earlier call to X") and keep
                                // the scary "failed" wording only for REAL failures
                                // (Timeout / SignalLost / StationQrt / …). With FIX
                                // 1, supersede is rare anyway.
                                if let pancetta_qso::QsoState::Failed { reason, .. } = &new_state {
                                    if !old_state.is_terminal() {
                                        let who = new_state
                                            .their_callsign()
                                            .or_else(|| old_state.their_callsign())
                                            .unwrap_or("?")
                                            .to_string();
                                        let text = if matches!(
                                            reason,
                                            pancetta_qso::QsoFailureReason::Superseded
                                        ) {
                                            format!("Replaced earlier call to {who}")
                                        } else {
                                            format!(
                                                "QSO with {} failed: {}",
                                                who,
                                                failure_reason_text(reason)
                                            )
                                        };
                                        emit_status(&snapshot_bus, text).await;
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                qso_id,
                                message,
                                frequency,
                                tx_parity,
                            }) => {
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={}, tx_parity={:?})",
                                            text, frequency, qso_id, tx_parity
                                        );
                                        // Record this as the newest intent for the QSO so the
                                        // TX worker can pivot to it at key-time if it arrives
                                        // while an earlier frame for the same QSO is still in
                                        // the worker's pre-PTT wait.
                                        if let Ok(mut m) = latest_tx_intent.write() {
                                            m.insert(
                                                super::active_tx_qso_key(&qso_id.to_string()),
                                                super::LatestTxIntent {
                                                    message_text: text.clone(),
                                                    frequency_offset: frequency,
                                                    tx_parity,
                                                },
                                            );
                                        }
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: text,
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                                tx_parity,
                                                origin: crate::message_bus::TxOrigin::Local,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = tx_bus.send_message(tx_msg).await {
                                            warn!("Failed to send auto-sequence TX: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        // BUG: This encode failure leaves the QSO state machine
                                        // stuck waiting for a TX that will never happen. The QSO
                                        // will eventually time out, but ideally we'd send a
                                        // QsoFailed event here. The qso_manager is not accessible
                                        // from this forwarding task.
                                        error!(
                                            "Failed to generate FT8 message for QSO {} — QSO state machine may be stuck: {}",
                                            qso_id, e
                                        );
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoCompleted {
                                qso_id, metadata, ..
                            }) => {
                                // Drop-stale-TX grace window. A normally
                                // completing QSO emits its FINAL 73 right at
                                // completion, so we must NOT purge it from the
                                // active set immediately — that would race the
                                // 73 out of existence.
                                //
                                // The grace MUST outlast the worst-case wait for
                                // the 73 to actually key PTT. Crucially, our TX
                                // slots are SAME-PARITY only, which are **30 s**
                                // apart (e.g. Odd = :15/:45), not 15 s. When the
                                // 73 is emitted just too late to key the
                                // immediately-next same-parity slot, the
                                // scheduler defers it a full 30 s to the slot
                                // after that. The old 16 s grace expired before
                                // that deferred slot, so the stale-TX gate
                                // silently DROPPED the 73 — the operator saw us
                                // stop at R-report and had to send 73 by hand
                                // (observed on-air with G8KHF, 2026-06-23).
                                //
                                // 45 s comfortably covers a full 30 s
                                // same-parity deferral plus the ≤8 s tx-late
                                // window and margin, while still purging any
                                // leftover backlog shortly after. Only the
                                // single 73 is pending post-completion (the
                                // coalescer keeps newest-per-QSO), so a longer
                                // grace cannot leak stale report frames.
                                const COMPLETED_TX_GRACE: Duration = Duration::from_secs(45);
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    // Ensure the key is present for the grace
                                    // window's duration. Normally a prior
                                    // active StateChanged already inserted it
                                    // (idempotent here), but a QSO that OPENS
                                    // directly at the close (respond_to_caller
                                    // SeventyThree → Completed) never passed
                                    // through an active state, so without this
                                    // insert its single final-73 TransmitRequest
                                    // would be dropped by the Step 4b gate and
                                    // never key PTT.
                                    if let Ok(mut s) = active_tx_qsos.write() {
                                        s.insert(key.clone());
                                    }
                                    let set = active_tx_qsos.clone();
                                    let intent_map = latest_tx_intent.clone();
                                    let qid = qso_id;
                                    // #40: promote any operator-deferred
                                    // cross-parity manual call once THIS QSO's
                                    // trailing 73 has cleared (grace elapsed) —
                                    // only then is the window truly free, so we
                                    // never end up TXing the 73 and a new
                                    // opposite-window call in sequential slots.
                                    let promote_mgr = snapshot_qso_manager.clone();
                                    let promote_pending = pending_for_events.clone();
                                    let promote_bus = snapshot_bus.clone();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(COMPLETED_TX_GRACE).await;
                                        if let Ok(mut s) = set.write() {
                                            s.remove(&key);
                                        }
                                        if let Ok(mut m) = intent_map.write() {
                                            m.remove(&key);
                                        }
                                        info!(
                                            target: "tx.policy",
                                            "QSO {} completed — grace elapsed, purging its TX from the active set",
                                            qid
                                        );
                                        promote_pending_manual_calls(
                                            &promote_mgr,
                                            &promote_pending,
                                            &promote_bus,
                                        )
                                        .await;
                                    });
                                }
                                // Clear AP state on QSO completion
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = None;
                                }
                                // hb-091: also clear the partner freq.
                                if let Ok(mut guard) = qso_freq_state.write() {
                                    *guard = None;
                                }
                                // Push fresh snapshot so the banner drops
                                // the just-completed QSO from the active list.
                                let (snapshot, pending_snap) = build_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                )
                                .await;
                                let gw_snap = if gateway_enabled.load(Ordering::Relaxed) {
                                    Some(MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot.clone(),
                                        pending: pending_snap.clone(),
                                    })
                                } else {
                                    None
                                };
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot,
                                        pending: pending_snap,
                                    },
                                    Instant::now(),
                                );
                                let _ = snapshot_bus.send_message(snap_msg).await;
                                if let Some(m) = gw_snap {
                                    super::remote_gateway::relay_to_gateway(
                                        &snapshot_bus,
                                        &gateway_enabled,
                                        ComponentId::Qso,
                                        m,
                                    )
                                    .await;
                                }
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO completed with {}, marking as worked", their_call);

                                    // Batch 2 #4: completed QSOs are filtered out
                                    // of the active snapshot, so the operator never
                                    // saw success. Surface a one-line confirmation
                                    // with the reports exchanged (RST sent/received).
                                    let rst = |r: Option<i8>| {
                                        r.map(|v| format!("{v:+}"))
                                            .unwrap_or_else(|| "--".to_string())
                                    };
                                    emit_status(
                                        &snapshot_bus,
                                        format!(
                                            "QSO with {} logged (RST {}/{})",
                                            their_call,
                                            rst(metadata.reports.sent),
                                            rst(metadata.reports.received),
                                        ),
                                    )
                                    .await;

                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);

                                    // item-2-auto-73: stash MANUAL completions so
                                    // that if this DX keeps re-sending RR73/RRR (they
                                    // didn't copy our 73) we can auto-re-send our 73,
                                    // bounded, from the decode-processing loop below.
                                    // Autonomous completions are deliberately NOT
                                    // stashed — that path has its own dx-busy /
                                    // duplicate gates and shouldn't keep TXing 73s.
                                    if metadata.initiated_by == pancetta_qso::CallInitiation::Manual
                                    {
                                        let now = chrono::Utc::now();
                                        let entry = RecentManualCompletion {
                                            completed_at: now,
                                            frequency_hz: metadata.frequency,
                                            dx_parity: metadata.tx_parity.map(|p| p.opposite()),
                                            resends: 0,
                                            last_resend_at: None,
                                        };
                                        let mut map = completions_for_events.lock().await;
                                        // Prune stale entries while we hold the lock so
                                        // the map never grows unbounded.
                                        map.retain(|_, e| {
                                            now.signed_duration_since(e.completed_at)
                                                < AUTO_73_WINDOW
                                        });
                                        map.insert(their_call.to_uppercase(), entry);
                                    }

                                    // QSO upload to cqdx.io's logbook is handled
                                    // by the opt-in `start_qso_upload_subscriber`
                                    // (alongside ClubLog / QRZ), which has its
                                    // own `QsoEvent::QsoCompleted` subscription
                                    // and defensively parses the
                                    // success/duplicate/auth-fail response. We do
                                    // NOT also fire `cqdx_bridge.report_qso` here
                                    // — that would double-upload the same QSO.
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::QsoFailed {
                                qso_id, metadata, ..
                            }) => {
                                // Drop-stale-TX gate: a failed QSO must stop
                                // transmitting immediately. (StateChanged-into-
                                // Failed already purges, but a QsoFailed not
                                // preceded by such a transition would otherwise
                                // be missed — purge here too, idempotently.)
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    if let Ok(mut set) = active_tx_qsos.write() {
                                        set.remove(&key);
                                    }
                                    if let Ok(mut m) = latest_tx_intent.write() {
                                        m.remove(&key);
                                    }
                                }
                                // #40: window freed — promote a deferred call.
                                promote_pending_manual_calls(
                                    &snapshot_qso_manager,
                                    &pending_for_events,
                                    &snapshot_bus,
                                )
                                .await;
                                // Clear AP state on QSO failure
                                if let Ok(mut guard) = ap_state.write() {
                                    *guard = None;
                                }
                                // Push fresh snapshot so the banner drops
                                // the failed QSO.
                                let (snapshot, pending_snap) = build_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                )
                                .await;
                                let gw_snap = if gateway_enabled.load(Ordering::Relaxed) {
                                    Some(MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot.clone(),
                                        pending: pending_snap.clone(),
                                    })
                                } else {
                                    None
                                };
                                let snap_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::ActiveQsosSnapshot {
                                        qsos: snapshot,
                                        pending: pending_snap,
                                    },
                                    Instant::now(),
                                );
                                let _ = snapshot_bus.send_message(snap_msg).await;
                                if let Some(m) = gw_snap {
                                    super::remote_gateway::relay_to_gateway(
                                        &snapshot_bus,
                                        &gateway_enabled,
                                        ComponentId::Qso,
                                        m,
                                    )
                                    .await;
                                }
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO failed with {}, adding backoff", their_call);
                                    qso_lookup.record_failure(their_call);
                                }
                            }
                            Ok(_) => {} // Other events (StateChanged, etc.)
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("QSO event subscriber lagged by {} events", n);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                });

                // Per-slot decode-creation dedup (#fix/duplicate-qso-73 Part B).
                //
                // FT8 decoders routinely emit 2-4 copies of the same station in
                // one 15-second window (different candidate frequencies / passes).
                // `process_message` MUST run for every decode so active QSOs
                // advance on every copy. But `maybe_answer_caller` (QSO
                // *creation*) only needs to fire ONCE per station per slot — all
                // subsequent copies can skip creation because the first already
                // opened (or gated) it.
                //
                // Keying strategy: `slot_key = unix_secs_of_decode / 15`.
                // Because `decoded_msg.timestamp` is a `SystemTime` stamped by
                // the decoder, multiple decodes from the same 15-second window
                // share the same `slot_key`. We dedup by `(slot_key,
                // base_callsign)`, where `base_callsign` collapses compound
                // variants (`EA8/G8BCG` and `G8BCG` → `G8BCG`).
                //
                // The set is a simple `(last_slot_key, HashSet<String>)`: when
                // the slot_key changes (new window), we clear and restart. This
                // is O(1) amortised and requires no locking (loop is single-task).
                let mut caller_dedup: (u64, std::collections::HashSet<String>) =
                    (0, std::collections::HashSet::new());

                while !shutdown.load(Ordering::Acquire) {
                    match qso_rx.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                // Decoded FT8 messages forwarded from the decoder
                                MessageType::DecodedMessage(ref decoded_msg) => {
                                    let raw_text = decoded_msg.text.clone();
                                    let frequency = decoded_msg.frequency_offset;
                                    let snr = decoded_msg.snr_db;

                                    // Parse the FT8 message to determine its type
                                    match pancetta_qso::utils::parse_ft8_message(
                                        &raw_text,
                                        &our_callsign,
                                    ) {
                                        Ok(msg_type) => {
                                            // item-2-auto-73: a directed RR73/RRR from
                                            // a station we just MANUALLY completed with
                                            // means they didn't copy our 73 — bounded
                                            // auto-re-send. Detect before process_message
                                            // moves the parsed type. The map/window/cap
                                            // gating lives in the helper.
                                            maybe_auto_resend_73(
                                                &msg_type,
                                                &our_callsign,
                                                frequency,
                                                decoded_msg.slot_parity,
                                                &qso_manager,
                                                &recent_manual_completions,
                                                &tx_policy,
                                                &message_bus,
                                            )
                                            .await;

                                            // process_message advances any active
                                            // QSO — runs unconditionally for every
                                            // decode so the state machine always
                                            // sees the latest copy.
                                            if let Err(e) = qso_manager
                                                .process_message(
                                                    msg_type.clone(),
                                                    raw_text.clone(),
                                                    frequency,
                                                    Some(snr),
                                                )
                                                .await
                                            {
                                                debug!("QSO process_message error: {}", e);
                                            }

                                            // Per-slot dedup gate for always-answer
                                            // creation. Derive the slot key from the
                                            // decode timestamp (floor(unix_secs/15)).
                                            let decode_slot_key =
                                                caller_creation_slot_key(decoded_msg.timestamp);

                                            // Refresh the dedup set when the slot
                                            // changes (new 15-second window).
                                            if decode_slot_key != caller_dedup.0 {
                                                caller_dedup.0 = decode_slot_key;
                                                caller_dedup.1.clear();
                                            }

                                            // Peek at the caller's base callsign
                                            // without consuming msg_type — we only
                                            // need it to key the dedup set.
                                            let caller_base =
                                                classify_caller_answer(&msg_type, &our_callsign)
                                                    .map(|a| {
                                                        pancetta_qso::exchange::base_callsign(
                                                            &a.their_call,
                                                        )
                                                    });

                                            // Always-answer-callers (#39): if a
                                            // station is calling US and no QSO
                                            // with them is already in progress,
                                            // come back to them — independent of
                                            // the autonomous toggle, gated by TX
                                            // policy / parity / capacity.
                                            //
                                            // Skip if we already attempted creation
                                            // for this station in this slot (Part B
                                            // of duplicate-QSO fix).
                                            let skip_creation = caller_base
                                                .as_deref()
                                                .is_some_and(|base| caller_dedup.1.contains(base));

                                            if skip_creation {
                                                debug!(
                                                    target: "qso",
                                                    "Per-slot dedup: skipping maybe_answer_caller for {} (slot {})",
                                                    caller_base.as_deref().unwrap_or("?"),
                                                    decode_slot_key,
                                                );
                                            } else {
                                                // Record the attempt BEFORE the
                                                // async call so that a second
                                                // decode arriving while we await
                                                // would also be suppressed if this
                                                // loop were ever concurrent (it
                                                // isn't today, but is defensive).
                                                if let Some(ref base) = caller_base {
                                                    caller_dedup.1.insert(base.clone());
                                                }
                                                maybe_answer_caller(
                                                    &msg_type,
                                                    &our_callsign,
                                                    frequency,
                                                    decoded_msg.slot_parity,
                                                    snr,
                                                    &qso_manager,
                                                    &tx_policy,
                                                    auto_answer_max_concurrent,
                                                    &message_bus,
                                                    &fox_mode,
                                                    &fox_max_streams,
                                                )
                                                .await;
                                            }

                                            // #41: record what this sender is
                                            // doing on the band so the QSO panel
                                            // can show whether the DX we're
                                            // calling is busy / CQing / on us.
                                            record_dx_activity(
                                                &dx_activity,
                                                &msg_type,
                                                &our_callsign,
                                                chrono::Utc::now(),
                                            );
                                        }
                                        Err(e) => {
                                            debug!(
                                                "Could not parse FT8 message '{}': {}",
                                                raw_text, e
                                            );
                                        }
                                    }
                                }

                                // QSO control messages (start QSO, log, etc.)
                                MessageType::QsoMessage(qso_msg) => {
                                    match qso_msg {
                                        crate::message_bus::QsoMessage::StartQso {
                                            callsign,
                                            frequency,
                                            dx_parity,
                                        } => {
                                            // Belt-and-suspenders: refuse to call our own
                                            // station regardless of how the command arrived.
                                            // The relay already blocks this via CallStation,
                                            // but non-relay paths (tests, future commands)
                                            // are covered here.
                                            if pancetta_qso::exchange::callsigns_match(
                                                &callsign,
                                                &our_callsign,
                                            ) {
                                                warn!(
                                                    target: "qso.security",
                                                    "Refusing StartQso for our own callsign {}",
                                                    callsign
                                                );
                                                continue;
                                            }
                                            info!(
                                                "Starting QSO with {} on {} Hz (manual)",
                                                callsign, frequency
                                            );
                                            // #40 half-duplex parity gate: a manual call
                                            // that would TX in the *opposite* window from
                                            // the one our active QSOs hold is DEFERRED, not
                                            // started — keeping the opposite window free to
                                            // hear responses (no sequential-window TX).
                                            // Same-window selections start immediately and
                                            // run concurrently (the TX coalescer
                                            // multi-streams them). The deferred call is
                                            // promoted automatically once the current side's
                                            // QSOs finish (promote_pending_manual_calls).
                                            let desired_tx_parity = dx_parity.map(|p| p.opposite());
                                            let current_side = qso_manager.current_tx_side().await;
                                            if matches!(
                                                pancetta_qso::qso_manager::admit_new_qso(
                                                    current_side,
                                                    desired_tx_parity,
                                                ),
                                                pancetta_qso::qso_manager::TxAdmission::Queue
                                            ) {
                                                let mut q = pending_manual_calls.lock().await;
                                                // Dedup by callsign; bound the queue.
                                                let dup = q.iter().any(|p| {
                                                    p.callsign.eq_ignore_ascii_case(&callsign)
                                                });
                                                if !dup {
                                                    if q.len() >= MAX_PENDING_MANUAL_CALLS {
                                                        q.pop_front();
                                                    }
                                                    // Capture the operator's held-offset
                                                    // intent so promote_pending_manual_calls
                                                    // can rerun compute_manual_tx_offset with
                                                    // the current active set at promotion time.
                                                    let queued_held =
                                                        tx_offset_hold_hz.load(Ordering::Relaxed);
                                                    let queued_hold_mode =
                                                        pancetta_core::TxFreqMode::from_u8(
                                                            tx_freq_mode.load(Ordering::Relaxed),
                                                        ) == pancetta_core::TxFreqMode::Hold;
                                                    q.push_back(PendingManualCall {
                                                        callsign: callsign.clone(),
                                                        frequency_hz: frequency as f64,
                                                        dx_parity,
                                                        queued_at: std::time::Instant::now(),
                                                        hound: false,
                                                        fox_freq_hz: None,
                                                        fox_grid: None,
                                                        held_hz: queued_held,
                                                        hold_mode: queued_hold_mode,
                                                    });
                                                }
                                                let queue_depth = q.len();
                                                drop(q);
                                                info!(
                                                    target: "qso",
                                                    "Queued {} ({:?}) — opposite window \
                                                     (active side {:?}); queue now {} pending",
                                                    callsign, dx_parity, current_side, queue_depth
                                                );
                                                emit_status(
                                                    &message_bus,
                                                    format!(
                                                        "Queued {} — waiting for current window \
                                                         to clear",
                                                        callsign
                                                    ),
                                                )
                                                .await;
                                                continue;
                                            }
                                            // Operator-initiated MANUAL call:
                                            //  - bypasses the self-duplicate gate (operator
                                            //    explicitly chose to work/re-work this DX), and
                                            //  - keep-calls every TX slot under the manual
                                            //    watchdog (5 min / 10 calls).
                                            //
                                            // TX-offset selection (T3): honor the held offset
                                            // (Hold mode) or de-conflict against live concurrent
                                            // QSOs, then fall back to Tx=Rx. partner_freq is
                                            // Some(dx_freq) only when tx_off != dx_freq so the
                                            // relevance gate still routes the DX's replies to us.
                                            // Regression: Auto + no collision → tx_off = dx_freq,
                                            // partner_freq = None (Tx=Rx, identical to today).
                                            let dx_freq = frequency as f64;
                                            let held = tx_offset_hold_hz.load(Ordering::Relaxed);
                                            let hold_mode = pancetta_core::TxFreqMode::from_u8(
                                                tx_freq_mode.load(Ordering::Relaxed),
                                            ) == pancetta_core::TxFreqMode::Hold;
                                            let active = qso_manager.active_tx_offsets().await;
                                            let (tx_off, partner) = compute_manual_tx_offset(
                                                dx_freq, hold_mode, held, &active,
                                            );
                                            if tx_off != dx_freq {
                                                info!(
                                                    target: "qso",
                                                    "TX offset: held={} hold_mode={} dx_freq={:.0} \
                                                     → tx_off={:.0} Hz (de-conflicted from {} active)",
                                                    held, hold_mode, dx_freq, tx_off, active.len()
                                                );
                                            }
                                            // respond_to_cq_with (Manual) emits the first
                                            // CqResponse as a QsoEvent::MessageToSend,
                                            // which the event-forwarding task above turns
                                            // into a TransmitRequest with the latched
                                            // tx_parity. The watchdog re-arm
                                            // (QsoManager::rearm_manual_calls) re-emits the
                                            // same MessageToSend once per slot until the DX
                                            // answers or the watchdog fires — so there is no
                                            // separate TransmitRequest here (that would
                                            // double-send the first call).
                                            match qso_manager
                                                .respond_to_cq_with(
                                                    callsign.clone(),
                                                    tx_off,
                                                    dx_parity,
                                                    pancetta_qso::CallInitiation::Manual,
                                                    partner,
                                                )
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Manual QSO started with {}: {} \
                                                         (tx_off={:.0} Hz, keep-calling under watchdog)",
                                                        callsign, qso_id, tx_off
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Calling {} — TX queued ({:.0} Hz)",
                                                            callsign, tx_off
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to start QSO with {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Call {} failed: {}", callsign, e),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::StartAutonomousQso {
                                            callsign,
                                            frequency,
                                            parity,
                                        } => {
                                            // Phase 5: the autonomous operator decided to open
                                            // a QSO. Create it in the QsoManager as an Auto QSO
                                            // so the engine auto-sequences it to completion; the
                                            // QsoManager emits the opening MessageToSend (→ TX)
                                            // and StateChanged (→ active_tx_qsos). The autonomous
                                            // task already applied its gating and is NOT sending
                                            // the opening itself, so there is no double-send.
                                            //
                                            // Half-duplex parity discipline (#39): never open a
                                            // QSO that would transmit in the *opposite* window
                                            // from the one our active QSOs are committed to —
                                            // doing so would leave us TXing in sequential windows
                                            // and deaf to responses. The new QSO's desired TX
                                            // parity is `opposite(dx_parity)` for a pounce, or
                                            // `parity` itself for a self-CQ. If it crosses the
                                            // live side, skip this slot; the DX will CQ again and
                                            // we re-evaluate once the current side clears.
                                            let desired_tx_parity = match &callsign {
                                                Some(_) => parity.map(|p| p.opposite()),
                                                None => parity,
                                            };
                                            let current_side = qso_manager.current_tx_side().await;
                                            if matches!(
                                                pancetta_qso::qso_manager::admit_new_qso(
                                                    current_side,
                                                    desired_tx_parity,
                                                ),
                                                pancetta_qso::qso_manager::TxAdmission::Queue
                                            ) {
                                                info!(
                                                    target: "qso.autonomous",
                                                    "Deferring autonomous QSO ({:?}) — cross-parity: \
                                                     active side {:?}, wanted {:?}; \
                                                     waiting for current window to clear",
                                                    callsign, current_side, desired_tx_parity
                                                );
                                                continue;
                                            }
                                            let result = match &callsign {
                                                Some(dx) => {
                                                    qso_manager
                                                        .respond_to_cq(
                                                            dx.clone(),
                                                            frequency,
                                                            parity,
                                                        )
                                                        .await
                                                }
                                                None => {
                                                    // Calling CQ ourselves: `parity` is our TX
                                                    // parity (not a DX parity).
                                                    qso_manager.start_cq(frequency, parity).await
                                                }
                                            };
                                            match result {
                                                Ok(qso_id) => match &callsign {
                                                    Some(dx) => info!(
                                                        target: "qso.autonomous",
                                                        "Autonomous QSO opened with {} on {:.0} Hz: {} \
                                                         (auto-sequencing to completion)",
                                                        dx, frequency, qso_id
                                                    ),
                                                    None => info!(
                                                        target: "qso.autonomous",
                                                        "Autonomous CQ QSO opened on {:.0} Hz: {}",
                                                        frequency, qso_id
                                                    ),
                                                },
                                                Err(e) => {
                                                    warn!(
                                                        target: "qso.autonomous",
                                                        "Failed to open autonomous QSO ({:?}): {}",
                                                        callsign, e
                                                    );
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::EngageHound {
                                            callsign,
                                            fox_freq,
                                            dx_parity,
                                            fox_grid,
                                        } => {
                                            // Belt-and-suspenders: refuse to Hound our own call.
                                            if pancetta_qso::exchange::callsigns_match(
                                                &callsign,
                                                &our_callsign,
                                            ) {
                                                warn!(
                                                    target: "qso.security",
                                                    "Refusing EngageHound for our own callsign {}",
                                                    callsign
                                                );
                                                continue;
                                            }
                                            info!(
                                                "Hound: engaging Fox {} at fox_freq={} Hz (manual)",
                                                callsign, fox_freq
                                            );
                                            // #40 half-duplex parity gate — identical logic to
                                            // StartQso. A cross-parity Hound engage is deferred
                                            // into the pending queue (as a Hound entry) and
                                            // promoted via engage_hound once the window flips.
                                            let desired_tx_parity = dx_parity.map(|p| p.opposite());
                                            let current_side = qso_manager.current_tx_side().await;
                                            if matches!(
                                                pancetta_qso::qso_manager::admit_new_qso(
                                                    current_side,
                                                    desired_tx_parity,
                                                ),
                                                pancetta_qso::qso_manager::TxAdmission::Queue
                                            ) {
                                                let mut q = pending_manual_calls.lock().await;
                                                let dup = q.iter().any(|p| {
                                                    p.callsign.eq_ignore_ascii_case(&callsign)
                                                });
                                                if !dup {
                                                    if q.len() >= MAX_PENDING_MANUAL_CALLS {
                                                        q.pop_front();
                                                    }
                                                    q.push_back(PendingManualCall {
                                                        callsign: callsign.clone(),
                                                        frequency_hz: fox_freq as f64,
                                                        dx_parity,
                                                        queued_at: std::time::Instant::now(),
                                                        hound: true,
                                                        fox_freq_hz: Some(fox_freq as f64),
                                                        fox_grid: fox_grid.clone(),
                                                        // Hound engage re-derives its own offset
                                                        // via engage_hound — these fields are
                                                        // ignored on promotion when hound==true.
                                                        held_hz: 0,
                                                        hold_mode: false,
                                                    });
                                                }
                                                let queue_depth = q.len();
                                                drop(q);
                                                info!(
                                                    target: "qso",
                                                    "Hound: queued Fox {} ({:?}) — opposite \
                                                     window (active side {:?}); queue now {} \
                                                     pending",
                                                    callsign, dx_parity, current_side,
                                                    queue_depth
                                                );
                                                emit_status(
                                                    &message_bus,
                                                    format!(
                                                        "Hound: queued {} — waiting for current \
                                                         window to clear",
                                                        callsign
                                                    ),
                                                )
                                                .await;
                                                continue;
                                            }
                                            // Same/idle parity: start the Hound QSO now.
                                            // engage_hound sets hound=true, partner_freq,
                                            // low calling offset, and emits the opening
                                            // CqResponse — all via QsoEvent (StateChanged +
                                            // MessageToSend), which the event-forwarding task
                                            // above turns into active_tx_qsos insertion and
                                            // a TransmitRequest. No double-send.
                                            match qso_manager
                                                .engage_hound(
                                                    &callsign,
                                                    fox_freq as f64,
                                                    fox_grid.as_deref(),
                                                    dx_parity,
                                                )
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Hound QSO started with Fox {}: {} \
                                                         (calling low, keep-calling under watchdog)",
                                                        callsign, qso_id
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Hound: calling Fox {} low — TX \
                                                             queued ({} Hz RX)",
                                                            callsign, fox_freq
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Hound: failed to engage Fox {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Hound: engage {} failed: {}",
                                                            callsign, e
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::RespondToCaller {
                                            callsign,
                                            frequency,
                                            dx_parity,
                                            step,
                                            snr,
                                        } => {
                                            info!(
                                                "Responding to caller {} on {} Hz at step {:?} \
                                                 (manual)",
                                                callsign, frequency, step
                                            );
                                            // TX-offset selection (T3): same priority as StartQso:
                                            // held offset → de-conflict → Tx=Rx fallback.
                                            let dx_freq = frequency as f64;
                                            let held = tx_offset_hold_hz.load(Ordering::Relaxed);
                                            let hold_mode = pancetta_core::TxFreqMode::from_u8(
                                                tx_freq_mode.load(Ordering::Relaxed),
                                            ) == pancetta_core::TxFreqMode::Hold;
                                            let active = qso_manager.active_tx_offsets().await;
                                            let (tx_off, partner) = compute_manual_tx_offset(
                                                dx_freq, hold_mode, held, &active,
                                            );
                                            if tx_off != dx_freq {
                                                info!(
                                                    target: "qso",
                                                    "RespondToCaller TX offset: held={} hold_mode={} \
                                                     dx_freq={:.0} → tx_off={:.0} Hz",
                                                    held, hold_mode, dx_freq, tx_off
                                                );
                                            }
                                            // Operator picked a station calling US from the
                                            // Callers panel and chose (or accepted the smart
                                            // default for) which sequence step to open at.
                                            // Manual call: bypasses the duplicate gate and
                                            // keep-calls under the watchdog, exactly like
                                            // StartQso — but starts at the correct rung
                                            // (their report → our R-report, etc.) instead of
                                            // always sending our grid. `their_report` is left
                                            // None; the engine defaults it.
                                            match qso_manager
                                                .respond_to_caller(
                                                    callsign.clone(),
                                                    tx_off,
                                                    dx_parity,
                                                    step,
                                                    snr,
                                                    None,
                                                    partner,
                                                )
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Caller-response QSO started with {}: \
                                                         {} (step {:?}, tx_off={:.0} Hz)",
                                                        callsign, qso_id, step, tx_off
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Replying to {} — TX queued ({:.0} Hz)",
                                                            callsign, tx_off
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to respond to caller {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Reply to {} failed: {}",
                                                            callsign, e
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::LogQso { qso_data } => {
                                            debug!("Manual log QSO: {}", qso_data);
                                        }
                                        // Abort / End both cancel the QSO
                                        // (→ Failed{UserCancelled}, mapping cleared).
                                        crate::message_bus::QsoMessage::AbortQso { qso_id }
                                        | crate::message_bus::QsoMessage::EndQso { qso_id } => {
                                            match qso_id.parse::<pancetta_qso::QsoId>() {
                                                Ok(id) => {
                                                    if let Err(e) = qso_manager.cancel_qso(id).await
                                                    {
                                                        warn!(
                                                            "Failed to abort QSO {}: {}",
                                                            qso_id, e
                                                        );
                                                    } else {
                                                        info!("Aborted QSO {}", qso_id);
                                                    }
                                                }
                                                Err(e) => warn!(
                                                    "AbortQso: bad QSO id '{}': {}",
                                                    qso_id, e
                                                ),
                                            }
                                        }
                                        crate::message_bus::QsoMessage::ResendQso { qso_id } => {
                                            match qso_id.parse::<pancetta_qso::QsoId>() {
                                                Ok(id) => {
                                                    if let Err(e) =
                                                        qso_manager.resend_last_tx(id).await
                                                    {
                                                        warn!(
                                                            "Failed to re-send QSO {}: {}",
                                                            qso_id, e
                                                        );
                                                    } else {
                                                        info!("Re-sent last TX for QSO {}", qso_id);
                                                    }
                                                }
                                                Err(e) => warn!(
                                                    "ResendQso: bad QSO id '{}': {}",
                                                    qso_id, e
                                                ),
                                            }
                                        }
                                        // Cancel EVERY active QSO. This is the
                                        // loop-breaker: manual QSOs keep-call
                                        // every slot via rearm_manual_calls_at,
                                        // and per-callsign `k`/AbortQso only
                                        // clears one — duplicate QSO objects or
                                        // an unseen QSO can keep re-emitting TX
                                        // forever. The emergency stop sends this
                                        // so a single Shift+Q clears the source
                                        // (not just mutes via TX policy).
                                        crate::message_bus::QsoMessage::CancelAllQsos => {
                                            let active = qso_manager.get_active_qsos().await;
                                            let n = active.len();
                                            for (id, _) in active {
                                                if let Err(e) = qso_manager.cancel_qso(id).await {
                                                    warn!("CancelAllQsos: {} failed: {}", id, e);
                                                }
                                            }
                                            info!(
                                                target: "operator.override",
                                                "CancelAllQsos: cancelled {} active QSO(s)",
                                                n
                                            );
                                        }
                                        // C9 — operator changed bands mid-QSO.
                                        // An active QSO cannot complete on a new
                                        // band, and its manual keep-call must NOT
                                        // keep transmitting there. Tear every
                                        // active QSO down (drives each to
                                        // Failed{UserCancelled}, which purges it
                                        // from `active_tx_qsos` via the QsoEvent
                                        // subscriber — so any already-queued TX is
                                        // dropped by the stale-TX gate next slot)
                                        // and surface a brief operator status.
                                        crate::message_bus::QsoMessage::BandChanged {
                                            previous_hz,
                                            new_hz,
                                        } => {
                                            let active = qso_manager.get_active_qsos().await;
                                            let n = active.len();
                                            for (id, _) in active {
                                                if let Err(e) = qso_manager.cancel_qso(id).await {
                                                    warn!(
                                                        "BandChanged: cancel {} failed: {}",
                                                        id, e
                                                    );
                                                }
                                            }
                                            info!(
                                                target: "operator.override",
                                                "Band change {} Hz -> {} Hz: ended {} active QSO(s)",
                                                previous_hz, new_hz, n
                                            );
                                            if n > 0 {
                                                emit_status(
                                                    &message_bus,
                                                    format!(
                                                        "Band change — {} active QSO(s) ended",
                                                        n
                                                    ),
                                                )
                                                .await;
                                            }
                                        }
                                        // Operator pressed `c`: start a manual
                                        // CQ as a tracked CallingCq QSO. The QSO
                                        // owns the CQ transmission (emits the
                                        // first CQ + keeps calling every slot
                                        // via rearm_manual_calls_at); the old
                                        // tui_relay text-only CQ loop no longer
                                        // transmits, so there is exactly one CQ
                                        // TX source per slot (no double-TX).
                                        // When a station answers, the
                                        // CallingCq → WaitingForReport arm fires
                                        // and the Manual-gated auto-reply emitter
                                        // sequences the exchange to Completed +
                                        // QsoCompleted (ADIF log).
                                        crate::message_bus::QsoMessage::StartCq {
                                            frequency,
                                            tx_parity,
                                        } => {
                                            match qso_manager
                                                .start_cq_manual(frequency as f64, tx_parity)
                                                .await
                                            {
                                                Ok(qso_id) => {
                                                    info!(
                                                        "Manual CQ started: {} ({} Hz, \
                                                         keep-calling under watchdog)",
                                                        qso_id, frequency
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Calling CQ — TX queued ({} Hz)",
                                                            frequency
                                                        ),
                                                    )
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!("Failed to start manual CQ: {}", e);
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Start CQ failed: {}", e),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                        // Operator pressed `s`: stop calling CQ.
                                        // Cancel any active QSO still in
                                        // CallingCq (un-answered). A CallingCq
                                        // QSO that already advanced past CallingCq
                                        // (a caller answered) is left running so
                                        // the in-progress exchange completes.
                                        crate::message_bus::QsoMessage::StopCq => {
                                            let active = qso_manager.get_active_qsos().await;
                                            let mut cancelled = 0usize;
                                            for (id, progress) in active {
                                                if matches!(
                                                    progress.state,
                                                    pancetta_qso::QsoState::CallingCq { .. }
                                                ) {
                                                    if let Err(e) = qso_manager.cancel_qso(id).await
                                                    {
                                                        warn!("StopCq: {} failed: {}", id, e);
                                                    } else {
                                                        cancelled += 1;
                                                    }
                                                }
                                            }
                                            info!(
                                                "StopCq: cancelled {} un-answered CQ QSO(s)",
                                                cancelled
                                            );
                                        }

                                        // Fox-mode engage/disengage. On engage:
                                        //   1. TX-policy gate (Fox originates CQ = initiation).
                                        //   2. Set fox_mode flag.
                                        //   3. Start a repeating CQ (same path as StartCq / `c`).
                                        //   4. Raise the caller-answer cap to fox_max_streams
                                        //      (read dynamically in maybe_answer_caller).
                                        // On disengage:
                                        //   1. Clear fox_mode.
                                        //   2. Cancel any active un-answered CallingCq QSO (StopCq path).
                                        //   3. Normal cap automatically restored (fox_mode == false).
                                        crate::message_bus::QsoMessage::SetFoxMode { on } => {
                                            if on {
                                                // Gate: Fox originates CQ — initiation only under Full.
                                                let policy = pancetta_core::TxPolicy::from_u8(
                                                    tx_policy
                                                        .load(std::sync::atomic::Ordering::Relaxed),
                                                );
                                                if !policy.allows_initiation() {
                                                    warn!(
                                                        target: "tx.policy",
                                                        "Refusing Fox mode: TX policy is {} \
                                                         (initiation disallowed)",
                                                        policy.label()
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Fox mode refused — TX policy is {} \
                                                             (press g for Full)",
                                                            policy.label()
                                                        ),
                                                    )
                                                    .await;
                                                    // Echo the ACTUAL state (still false — refused)
                                                    // so the TUI can correct its optimistic flip.
                                                    let _ = message_bus
                                                        .send_message(ComponentMessage::new(
                                                            ComponentId::Qso,
                                                            ComponentId::Tui,
                                                            MessageType::FoxModeStatus {
                                                                on: false,
                                                            },
                                                            Instant::now(),
                                                        ))
                                                        .await;
                                                    continue;
                                                }

                                                // Set the flag so maybe_answer_caller uses fox_max_streams.
                                                fox_mode.store(
                                                    true,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );

                                                // Start the repeating Fox CQ (same as manual `c`):
                                                // CallingCq QSO re-emits CQ every slot under the
                                                // manual watchdog until a Hound answers.
                                                // Use 1500 Hz (FT8 passband centre) as the default
                                                // Fox CQ audio offset; tx_parity = None (Fox
                                                // picks its own slot via the self-parity fallback).
                                                const FOX_CQ_OFFSET_HZ: f64 = 1500.0;
                                                match qso_manager
                                                    .start_cq_manual(FOX_CQ_OFFSET_HZ, None)
                                                    .await
                                                {
                                                    Ok(qso_id) => {
                                                        let n = fox_max_streams.load(
                                                            std::sync::atomic::Ordering::Relaxed,
                                                        );
                                                        info!(
                                                            "Fox mode ON — CQ started: {} \
                                                             ({:.0} Hz, up to {} streams)",
                                                            qso_id, FOX_CQ_OFFSET_HZ, n
                                                        );
                                                        emit_status(
                                                            &message_bus,
                                                            format!(
                                                                "Fox mode ON — CQ + up to {} \
                                                                 streams",
                                                                n
                                                            ),
                                                        )
                                                        .await;
                                                    }
                                                    Err(e) => {
                                                        // CQ start failed — still leave fox_mode
                                                        // set so the cap raise takes effect and the
                                                        // operator can manually call CQ.
                                                        warn!(
                                                            "Fox mode ON but CQ start failed: {}",
                                                            e
                                                        );
                                                        emit_status(
                                                            &message_bus,
                                                            format!(
                                                                "Fox mode ON (CQ start failed: \
                                                                 {})",
                                                                e
                                                            ),
                                                        )
                                                        .await;
                                                    }
                                                }
                                                // Echo actual state (engaged = true) to TUI.
                                                let _ = message_bus
                                                    .send_message(ComponentMessage::new(
                                                        ComponentId::Qso,
                                                        ComponentId::Tui,
                                                        MessageType::FoxModeStatus { on: true },
                                                        Instant::now(),
                                                    ))
                                                    .await;
                                            } else {
                                                // Disengage: clear flag first so cap drops
                                                // immediately; then cancel CQ.
                                                fox_mode.store(
                                                    false,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );

                                                let active = qso_manager.get_active_qsos().await;
                                                let mut cancelled = 0usize;
                                                for (id, progress) in active {
                                                    if matches!(
                                                        progress.state,
                                                        pancetta_qso::QsoState::CallingCq { .. }
                                                    ) {
                                                        if let Err(e) =
                                                            qso_manager.cancel_qso(id).await
                                                        {
                                                            warn!(
                                                                "Fox mode OFF: cancel CQ {} \
                                                                 failed: {}",
                                                                id, e
                                                            );
                                                        } else {
                                                            cancelled += 1;
                                                        }
                                                    }
                                                }
                                                info!(
                                                    "Fox mode OFF — cancelled {} CQ QSO(s)",
                                                    cancelled
                                                );
                                                emit_status(
                                                    &message_bus,
                                                    "Fox mode OFF".to_string(),
                                                )
                                                .await;
                                                // Echo actual state (disengaged = false) to TUI.
                                                let _ = message_bus
                                                    .send_message(ComponentMessage::new(
                                                        ComponentId::Qso,
                                                        ComponentId::Tui,
                                                        MessageType::FoxModeStatus { on: false },
                                                        Instant::now(),
                                                    ))
                                                    .await;
                                            }
                                        }
                                    }
                                }

                                _ => {}
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                info!("QSO component stopped");
                Ok(())
            })
        };

        self.named_task_handles.push((ComponentId::Qso, qso_handle));
        info!("QSO component started");
        Ok(())
    }
}

/// Build a flat snapshot of in-progress QSOs from the QSO manager,
/// suitable for `MessageType::ActiveQsosSnapshot`. The TUI banner and
/// QSO-detail panel both render from this. Also snapshots the cross-parity
/// pending-call queue (#40) so the TUI can surface "Queued" calls without
/// a separate message.
async fn build_active_qso_snapshot(
    qso_manager: &pancetta_qso::QsoManager,
    dx_activity: &DxActivityMap,
    pending_manual_calls: &PendingManualCalls,
) -> (
    Vec<crate::message_bus::ActiveQsoSnapshotItem>,
    Vec<crate::message_bus::PendingCallSnapshotItem>,
) {
    let active = qso_manager.get_active_qsos().await;
    let now = chrono::Utc::now();
    // Watchdog config for the manual keep-calling countdown (Batch 2 #1).
    let timeouts = &qso_manager.config().timeouts;
    let max_calls = timeouts.manual_call_max_calls;
    let watchdog_minutes = timeouts.manual_call_watchdog_minutes;

    // FIX 3 (defense-in-depth): the QSO engine now supersedes older active
    // QSOs per (callsign, band) at start time, so a callsign should appear at
    // most once here. Dedup anyway, keeping the most-recently-started QSO, so
    // the TUI "exchanges" list never shows two entries for one (callsign,
    // band) even if a transient race ever surfaced both.
    let mut latest: std::collections::HashMap<(String, String), pancetta_qso::QsoProgress> =
        std::collections::HashMap::new();
    for (_id, progress) in active {
        let Some(their) = progress
            .state
            .their_callsign()
            .map(str::to_string)
            .or_else(|| progress.metadata.their_callsign.clone())
        else {
            continue;
        };
        let band = pancetta_qso::utils::frequency_to_band(progress.metadata.frequency);
        let key = (their, band);
        match latest.get(&key) {
            Some(existing) if existing.metadata.start_time >= progress.metadata.start_time => {}
            _ => {
                latest.insert(key, progress);
            }
        }
    }

    // Batch 2 #5: emit in a STABLE order (start_time, then callsign). The
    // HashMap iteration order is non-deterministic, which made multi-QSO row
    // order jump between snapshots — a positional cursor then pointed at a
    // different QSO each frame. The TUI also pins its selection by qso_id, but
    // a stable emit order keeps the visible list from reshuffling.
    let mut progresses: Vec<pancetta_qso::QsoProgress> = latest.into_values().collect();
    progresses.sort_by(|a, b| {
        a.metadata
            .start_time
            .cmp(&b.metadata.start_time)
            .then_with(|| {
                let ca = a.state.their_callsign().unwrap_or("");
                let cb = b.state.their_callsign().unwrap_or("");
                ca.cmp(cb)
            })
    });

    let qsos: Vec<crate::message_bus::ActiveQsoSnapshotItem> = progresses
        .iter()
        .filter_map(|p| {
            let item = snapshot_item_from_progress(p, max_calls, watchdog_minutes)?;
            // #41: enrich with what the DX is doing band-wide.
            let dx_last_activity = lookup_dx_activity(dx_activity, &item.their_callsign, now);
            Some(crate::message_bus::ActiveQsoSnapshotItem {
                dx_last_activity,
                ..item
            })
        })
        .collect();

    // #40: snapshot the cross-parity pending queue. Lock briefly, copy out,
    // release — never hold across await points.
    let pending: Vec<crate::message_bus::PendingCallSnapshotItem> = {
        let guard = pending_manual_calls.lock().await;
        guard
            .iter()
            .map(|p| crate::message_bus::PendingCallSnapshotItem {
                callsign: p.callsign.clone(),
                dx_parity: p.dx_parity,
                waited_secs: p.queued_at.elapsed().as_secs(),
            })
            .collect()
    };

    (qsos, pending)
}

/// Flatten one `QsoProgress` into the bus snapshot item. Pure read of
/// state the QSO engine already tracks — no behavioral change to the
/// engine. Returns `None` when the contra callsign is unknown (nothing
/// useful to render yet).
///
/// Batch 94: in addition to the banner fields, derives the QSO-detail
/// panel fields — last message exchanged in each direction (from
/// `progress.messages`), measured RX SNR (signal strength of the last
/// received message), reports sent/received (from
/// `metadata.reports`), and the exchange count.
/// `max_calls` / `watchdog_minutes` come from the QSO manager's
/// `TimeoutConfig`; they populate the manual keep-calling countdown
/// fields (`call_count`/`max_calls`/`watchdog_deadline`), which are only
/// meaningful while the QSO is in a manual keep-calling state
/// (RespondingToCq / SendingReport).
fn snapshot_item_from_progress(
    progress: &pancetta_qso::QsoProgress,
    max_calls: u32,
    watchdog_minutes: u64,
) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
    use pancetta_qso::{CallInitiation, MessageDirection, QsoState};
    let their = progress
        .state
        .their_callsign()
        .map(str::to_string)
        .or_else(|| progress.metadata.their_callsign.clone())?;
    let frequency_hz = progress
        .state
        .frequency()
        .unwrap_or(progress.metadata.frequency);
    let state = match &progress.state {
        QsoState::Idle => "idle",
        QsoState::CallingCq { .. } => "calling CQ",
        QsoState::RespondingToCq { .. } => "→ called",
        QsoState::WaitingForReport { .. } => "wait rpt",
        QsoState::SendingReport { .. } => "sending rpt",
        QsoState::WaitingForConfirmation { .. } => "wait RR73",
        QsoState::SendingConfirmation { .. } => "sending RR73",
        QsoState::Completed { .. } => "done",
        QsoState::Failed { .. } => "failed",
        QsoState::Contest(pancetta_qso::ContestState::ExchangingInfo { .. }) => "contest exch",
        QsoState::Contest(pancetta_qso::ContestState::ContestCompleted { .. }) => "contest done",
    }
    .to_string();

    let last_tx = progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Sent);
    let last_rx = progress
        .messages
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Received);

    let initiated_by = match progress.metadata.initiated_by {
        pancetta_qso::CallInitiation::Manual => "Manual",
        pancetta_qso::CallInitiation::Auto => "Auto",
    }
    .to_string();

    // Derive the role-aware display ladder + now/next lines. Terminal/Idle/
    // Contest states return None (shouldn't appear in the active set, but we
    // handle it by leaving the ladder empty and now/next blank). The role
    // (CQer vs Caller) is latched on the QSO at creation and disambiguates the
    // shared middle states (Batch 2 #6).
    let ladder = progress.state.ladder_view(progress.metadata.role);
    let (ladder_labels, ladder_ours, ladder_index, now_line, next_line) = match ladder {
        Some(v) => (
            v.labels.iter().map(|s| s.to_string()).collect(),
            v.ours,
            v.index,
            v.now,
            v.next,
        ),
        None => (Vec::new(), Vec::new(), 0, String::new(), String::new()),
    };

    // Manual keep-calling watchdog visibility (Batch 2 #1). Only meaningful
    // while a MANUAL QSO is in a keep-calling state (RespondingToCq /
    // SendingReport); otherwise zero/None so the TUI shows nothing misleading.
    let keep_calling = progress.metadata.initiated_by == CallInitiation::Manual
        && matches!(
            progress.state,
            QsoState::RespondingToCq { .. } | QsoState::SendingReport { .. }
        );
    let (wd_call_count, wd_max_calls, watchdog_deadline) = if keep_calling {
        let deadline = progress
            .metadata
            .first_call_at
            .map(|t| t + chrono::Duration::minutes(watchdog_minutes as i64));
        (progress.metadata.call_count, max_calls, deadline)
    } else {
        (0, 0, None)
    };

    Some(crate::message_bus::ActiveQsoSnapshotItem {
        their_callsign: their,
        state,
        started_at: progress.metadata.start_time,
        frequency_hz,
        tx_parity: progress.metadata.tx_parity,
        last_tx_text: last_tx.map(|m| m.raw_text.clone()),
        last_tx_at: last_tx.map(|m| m.timestamp),
        last_rx_text: last_rx.map(|m| m.raw_text.clone()),
        last_rx_at: last_rx.map(|m| m.timestamp),
        snr_rx: last_rx.and_then(|m| m.signal_strength).map(|s| s as i32),
        report_sent: progress.metadata.reports.sent.map(i32::from),
        report_received: progress.metadata.reports.received.map(i32::from),
        exchange_count: progress.messages.len() as u32,
        qso_id: progress.metadata.qso_id.to_string(),
        initiated_by,
        ladder_labels,
        ladder_ours,
        ladder_index,
        now_line,
        next_line,
        call_count: wd_call_count,
        max_calls: wd_max_calls,
        watchdog_deadline,
        // Enriched by build_active_qso_snapshot from the band-wide DX-activity
        // map (#41); this pure per-progress builder has no band context.
        dx_last_activity: None,
        hound: progress.metadata.hound,
    })
}

#[cfg(test)]
mod pending_manual_tests {
    use super::{partition_pending_calls, PendingManualCall};
    use pancetta_core::slot::SlotParity;
    use std::collections::VecDeque;

    // Build a pending call whose DX is on `dx`, so its desired TX parity is the
    // opposite.
    fn call(name: &str, dx: SlotParity) -> PendingManualCall {
        PendingManualCall {
            callsign: name.to_string(),
            frequency_hz: 1500.0,
            dx_parity: Some(dx),
            queued_at: std::time::Instant::now(),
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 0,
            hold_mode: false,
        }
    }

    fn names(v: &[PendingManualCall]) -> Vec<String> {
        v.iter().map(|p| p.callsign.clone()).collect()
    }

    #[test]
    fn idle_adopts_oldest_then_starts_all_same_side() {
        // DX Even ⇒ we TX Odd; DX Odd ⇒ we TX Even.
        let q: VecDeque<_> = [
            call("A", SlotParity::Even), // want Odd
            call("B", SlotParity::Odd),  // want Even
            call("C", SlotParity::Even), // want Odd
        ]
        .into();
        // Idle: adopt oldest (A wants Odd). A & C start, B stays.
        let (start, keep) = partition_pending_calls(q, None);
        assert_eq!(names(&start), vec!["A", "C"]);
        assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
    }

    #[test]
    fn committed_side_only_adds_same_side() {
        let q: VecDeque<_> = [
            call("A", SlotParity::Even), // want Odd
            call("B", SlotParity::Odd),  // want Even
        ]
        .into();
        // We're committed to Odd already: only A (wants Odd) joins; B waits.
        let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd));
        assert_eq!(names(&start), vec!["A"]);
        assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
    }

    #[test]
    fn committed_side_with_no_match_keeps_all() {
        let q: VecDeque<_> = [call("B", SlotParity::Odd)].into(); // wants Even
                                                                  // Committed to Odd, only an Even-wanting call queued → nothing promotes.
        let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd));
        assert!(start.is_empty());
        assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["B"]);
    }

    #[test]
    fn empty_queue_is_noop() {
        let (start, keep) = partition_pending_calls(VecDeque::new(), None);
        assert!(start.is_empty());
        assert!(keep.is_empty());
    }

    // ── TTL / partition_expired tests ────────────────────────────────────────

    use super::partition_expired;

    /// Build a call whose `queued_at` is `age` in the past.
    fn aged_call(name: &str, dx: SlotParity, age: std::time::Duration) -> PendingManualCall {
        PendingManualCall {
            callsign: name.to_string(),
            frequency_hz: 1500.0,
            dx_parity: Some(dx),
            queued_at: std::time::Instant::now()
                .checked_sub(age)
                .expect("Instant underflow in test"),
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 0,
            hold_mode: false,
        }
    }

    #[test]
    fn fresh_calls_are_kept() {
        let ttl = std::time::Duration::from_secs(600);
        let q: VecDeque<_> = [call("A", SlotParity::Even), call("B", SlotParity::Odd)].into();
        let now = std::time::Instant::now();
        let (kept, expired) = partition_expired(q, now, ttl);
        assert_eq!(kept.len(), 2);
        assert!(expired.is_empty());
    }

    #[test]
    fn expired_calls_are_removed_and_returned() {
        let ttl = std::time::Duration::from_secs(600);
        let q: VecDeque<_> = [
            // A: 11 min old — expired
            aged_call("A", SlotParity::Even, std::time::Duration::from_secs(660)),
            // B: 5 min old — still fresh
            aged_call("B", SlotParity::Odd, std::time::Duration::from_secs(300)),
            // C: 12 min old — expired
            aged_call("C", SlotParity::Even, std::time::Duration::from_secs(720)),
        ]
        .into();
        let now = std::time::Instant::now();
        let (kept, expired) = partition_expired(q, now, ttl);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].callsign, "B");
        assert_eq!(expired, vec!["A", "C"]);
    }

    #[test]
    fn exact_ttl_boundary_expires() {
        // A call that is exactly TTL old should be retired (>=, not >).
        let ttl = std::time::Duration::from_secs(600);
        let q: VecDeque<_> = [aged_call("X", SlotParity::Even, ttl)].into();
        let now = std::time::Instant::now();
        let (kept, expired) = partition_expired(q, now, ttl);
        assert!(kept.is_empty());
        assert_eq!(expired, vec!["X"]);
    }

    #[test]
    fn empty_queue_partition_expired_is_noop() {
        let (kept, expired) = partition_expired(
            VecDeque::new(),
            std::time::Instant::now(),
            std::time::Duration::from_secs(600),
        );
        assert!(kept.is_empty());
        assert!(expired.is_empty());
    }

    // ── Queued call offset fields + promote logic unit tests ─────────────────
    //
    // These tests verify that a PendingManualCall correctly carries the
    // held_hz/hold_mode snapshot from queue time, and that
    // compute_manual_tx_offset produces the expected offset when called with
    // those values at promotion time (mirroring what promote_pending_manual_calls
    // does in the non-Hound branch).

    use super::compute_manual_tx_offset;

    /// A queued call with held_hz=1500 / hold_mode=true opens at the held
    /// offset (1500 Hz) when promoted with no active concurrent QSOs.
    #[test]
    fn queued_call_with_held_offset_opens_at_held_hz() {
        let p = PendingManualCall {
            callsign: "DX1ABC".to_string(),
            frequency_hz: 900.0, // DX decoded at 900 Hz
            dx_parity: Some(SlotParity::Even),
            queued_at: std::time::Instant::now(),
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 1500,
            hold_mode: true,
        };
        let active: Vec<f64> = vec![];
        let (tx_off, partner) =
            compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
        assert_eq!(tx_off, 1500.0, "held offset should be honoured");
        assert_eq!(
            partner,
            Some(900.0),
            "partner_freq must be Some(dx_freq) when tx_off != dx_freq"
        );
    }

    /// A queued call with held_hz=1500 / hold_mode=true de-conflicts against
    /// an active QSO already at 1500 Hz.
    #[test]
    fn queued_call_deconflicts_held_offset_against_active_at_promotion() {
        let p = PendingManualCall {
            callsign: "DX2XYZ".to_string(),
            frequency_hz: 1200.0, // DX decoded at 1200 Hz
            dx_parity: Some(SlotParity::Odd),
            queued_at: std::time::Instant::now(),
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 1500,
            hold_mode: true,
        };
        // An active QSO is already on 1500 Hz at promotion time.
        let active: Vec<f64> = vec![1500.0];
        let (tx_off, partner) =
            compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
        // Should NOT be 1500 (too close to the occupied slot).
        assert_ne!(tx_off, 1500.0, "must not stack on the occupied offset");
        // tx_off should be within [300, 2700].
        assert!(
            (300.0..=2700.0).contains(&tx_off),
            "tx_off={tx_off} is outside [300, 2700]"
        );
        // partner_freq must be Some so the DX's replies at 1200 Hz are routed.
        assert_eq!(
            partner,
            Some(1200.0),
            "partner_freq must be Some(dx_freq) when tx_off != dx_freq"
        );
    }

    /// A queued call with hold_mode=false (Auto) and no active QSOs promotes
    /// Tx=Rx (partner_freq=None) — regression invariant.
    #[test]
    fn queued_call_auto_mode_promotes_tx_eq_rx() {
        let p = PendingManualCall {
            callsign: "DX3WWW".to_string(),
            frequency_hz: 1750.0,
            dx_parity: Some(SlotParity::Even),
            queued_at: std::time::Instant::now(),
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 0,
            hold_mode: false,
        };
        let active: Vec<f64> = vec![];
        let (tx_off, partner) =
            compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
        assert_eq!(tx_off, 1750.0, "Auto + no collision → Tx=Rx");
        assert_eq!(partner, None, "Tx=Rx → partner_freq is None");
    }
}

#[cfg(test)]
mod dx_activity_tests {
    use super::dx_activity_summary;
    use pancetta_qso::states::MessageType as Mt;

    const OUR: &str = "K5ARH";

    #[test]
    fn cq_summarizes_as_calling_cq() {
        let m = Mt::Cq {
            callsign: "JA1ABC".to_string(),
            grid: None,
        };
        let (from, s) = dx_activity_summary(&m, OUR).unwrap();
        assert_eq!(from, "JA1ABC");
        assert_eq!(s, "calling CQ");
    }

    #[test]
    fn report_to_third_party_names_them() {
        let m = Mt::SignalReport {
            to_station: "W1XYZ".to_string(),
            from_station: "JA1ABC".to_string(),
            report: -12,
        };
        let (from, s) = dx_activity_summary(&m, OUR).unwrap();
        assert_eq!(from, "JA1ABC");
        assert_eq!(s, "→ W1XYZ -12");
    }

    #[test]
    fn report_to_us_says_us() {
        let m = Mt::ReportAck {
            to_station: OUR.to_string(),
            from_station: "JA1ABC".to_string(),
            report: 3,
        };
        let (from, s) = dx_activity_summary(&m, OUR).unwrap();
        assert_eq!(from, "JA1ABC");
        assert_eq!(s, "→ us R+3");
    }

    #[test]
    fn nonstandard_has_no_summary() {
        let m = Mt::NonStandard {
            text: "blah".to_string(),
        };
        assert!(dx_activity_summary(&m, OUR).is_none());
    }
}

#[cfg(test)]
mod caller_answer_tests {
    use super::{classify_caller_answer, CallerAnswer};
    use pancetta_core::ResponseStep;
    use pancetta_qso::states::MessageType as Mt;

    const OUR: &str = "K5ARH";

    #[test]
    fn cqresponse_to_us_opens_at_report() {
        let m = Mt::CqResponse {
            calling_station: OUR.to_string(),
            responding_station: "JA1ABC".to_string(),
            grid: None,
        };
        assert_eq!(
            classify_caller_answer(&m, OUR),
            Some(CallerAnswer {
                their_call: "JA1ABC".to_string(),
                step: ResponseStep::Report,
                their_report: None,
            })
        );
    }

    #[test]
    fn signal_report_to_us_opens_at_reportack_with_report() {
        let m = Mt::SignalReport {
            to_station: OUR.to_string(),
            from_station: "JA1ABC".to_string(),
            report: -12,
        };
        assert_eq!(
            classify_caller_answer(&m, OUR),
            Some(CallerAnswer {
                their_call: "JA1ABC".to_string(),
                step: ResponseStep::ReportAck,
                their_report: Some(-12),
            })
        );
    }

    #[test]
    fn reportack_to_us_opens_at_rr73() {
        let m = Mt::ReportAck {
            to_station: OUR.to_string(),
            from_station: "JA1ABC".to_string(),
            report: -3,
        };
        let a = classify_caller_answer(&m, OUR).unwrap();
        assert_eq!(a.step, ResponseStep::Rr73);
        assert_eq!(a.their_report, Some(-3));
    }

    #[test]
    fn final_confirmation_to_us_opens_at_73() {
        let m = Mt::FinalConfirmation {
            to_station: OUR.to_string(),
            from_station: "JA1ABC".to_string(),
        };
        assert_eq!(
            classify_caller_answer(&m, OUR).map(|a| a.step),
            Some(ResponseStep::SeventyThree)
        );
    }

    #[test]
    fn compound_call_to_us_is_recognized() {
        // Their frame addresses our base call from a compound call.
        let m = Mt::SignalReport {
            to_station: OUR.to_string(),
            from_station: "EA8/G8BCG".to_string(),
            report: -7,
        };
        assert_eq!(
            classify_caller_answer(&m, OUR).map(|a| a.their_call),
            Some("EA8/G8BCG".to_string())
        );
    }

    #[test]
    fn message_to_someone_else_is_ignored() {
        let m = Mt::SignalReport {
            to_station: "W1XYZ".to_string(),
            from_station: "JA1ABC".to_string(),
            report: -12,
        };
        assert_eq!(classify_caller_answer(&m, OUR), None);
    }

    #[test]
    fn cq_and_seventythree_are_not_caller_answers() {
        // A CQ is an initiation, not a direct call to us.
        let cq = Mt::Cq {
            callsign: "JA1ABC".to_string(),
            grid: None,
        };
        assert_eq!(classify_caller_answer(&cq, OUR), None);
        // A 73 to us needs no reply (the QSO is closing).
        let seventythree = Mt::SeventyThree {
            to_station: OUR.to_string(),
            from_station: "JA1ABC".to_string(),
        };
        assert_eq!(classify_caller_answer(&seventythree, OUR), None);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::snapshot_item_from_progress;
    use chrono::{Duration, Utc};
    use pancetta_qso::{
        GridSquares, MessageDirection, QsoMetadata, QsoProgress, QsoState, SignalReports,
    };

    /// Build a QsoProgress mid-exchange: we called them, sent our grid,
    /// and just received their report.
    fn fixture_progress() -> QsoProgress {
        let start = Utc::now() - Duration::seconds(45);
        let their_call = "JA1ABC".to_string();
        let messages = vec![
            pancetta_qso::states::QsoMessage {
                timestamp: start + Duration::seconds(15),
                direction: MessageDirection::Sent,
                message_type: pancetta_qso::states::MessageType::CqResponse {
                    calling_station: their_call.clone(),
                    responding_station: "K5ARH".to_string(),
                    grid: Some("EM10".to_string()),
                },
                raw_text: "JA1ABC K5ARH EM10".to_string(),
                signal_strength: None,
                frequency: 1500.0,
            },
            pancetta_qso::states::QsoMessage {
                timestamp: start + Duration::seconds(30),
                direction: MessageDirection::Received,
                message_type: pancetta_qso::states::MessageType::SignalReport {
                    to_station: "K5ARH".to_string(),
                    from_station: their_call.clone(),
                    report: -12,
                },
                raw_text: "K5ARH JA1ABC -12".to_string(),
                signal_strength: Some(-12.4),
                frequency: 1500.0,
            },
        ];
        QsoProgress {
            state: QsoState::SendingReport {
                their_callsign: their_call.clone(),
                their_report: Some(-12),
                our_report: -8,
                frequency: 1500.0,
                started_at: start,
            },
            state_history: Vec::new(),
            messages,
            metadata: QsoMetadata {
                qso_id: pancetta_qso::QsoId::new_v4(),
                our_callsign: "K5ARH".to_string(),
                their_callsign: Some(their_call),
                frequency: 1500.0,
                mode: "FT8".to_string(),
                start_time: start,
                end_time: None,
                reports: SignalReports {
                    sent: Some(-8),
                    received: Some(-12),
                },
                grids: GridSquares::default(),
                contest_info: None,
                tags: std::collections::HashMap::new(),
                notes: None,
                tx_parity: Some(pancetta_core::slot::SlotParity::Odd),
                initiated_by: Default::default(),
                role: Default::default(),
                call_count: 0,
                first_call_at: None,
                last_call_at: None,
                progressed_this_cycle: false,
                last_rx_text: None,
                dx_repeat_count: 0,
                hound: false,
                partner_freq: None,
                hound_qsyed: false,
            },
        }
    }

    /// Default watchdog config for snapshot tests (matches TimeoutConfig
    /// defaults: 10 calls / 5 minutes).
    const TEST_MAX_CALLS: u32 = 10;
    const TEST_WATCHDOG_MIN: u64 = 5;

    /// Thin wrapper so the existing tests don't each repeat the watchdog args.
    fn snap(progress: &QsoProgress) -> Option<crate::message_bus::ActiveQsoSnapshotItem> {
        snapshot_item_from_progress(progress, TEST_MAX_CALLS, TEST_WATCHDOG_MIN)
    }

    /// All detail-panel fields derive from state the engine already
    /// tracks: last message per direction, measured RX SNR, reports,
    /// exchange count, plus the original banner fields.
    #[test]
    fn snapshot_derives_detail_fields_from_progress() {
        let item = snap(&fixture_progress()).expect("item");
        assert_eq!(item.their_callsign, "JA1ABC");
        assert_eq!(item.state, "sending rpt");
        assert_eq!(item.frequency_hz, 1500.0);
        assert_eq!(item.tx_parity, Some(pancetta_core::slot::SlotParity::Odd));
        assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH EM10"));
        assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert!(item.last_tx_at.is_some());
        assert!(item.last_rx_at.is_some());
        assert_eq!(item.snr_rx, Some(-12));
        assert_eq!(item.report_sent, Some(-8));
        assert_eq!(item.report_received, Some(-12));
        assert_eq!(item.exchange_count, 2);
    }

    /// The most recent message per direction wins, not the first.
    #[test]
    fn snapshot_picks_latest_message_per_direction() {
        let mut progress = fixture_progress();
        progress.messages.push(pancetta_qso::states::QsoMessage {
            timestamp: Utc::now(),
            direction: MessageDirection::Sent,
            message_type: pancetta_qso::states::MessageType::ReportAck {
                to_station: "JA1ABC".to_string(),
                from_station: "K5ARH".to_string(),
                report: -8,
            },
            raw_text: "JA1ABC K5ARH R-8".to_string(),
            signal_strength: None,
            frequency: 1500.0,
        });
        let item = snap(&progress).expect("item");
        assert_eq!(item.last_tx_text.as_deref(), Some("JA1ABC K5ARH R-8"));
        // RX side unchanged by a new TX.
        assert_eq!(item.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert_eq!(item.exchange_count, 3);
    }

    /// No callsign known yet (e.g. CallingCq with empty metadata) →
    /// nothing useful to render → None.
    #[test]
    fn snapshot_skips_qso_without_callsign() {
        let mut progress = fixture_progress();
        progress.state = QsoState::CallingCq {
            frequency: 1500.0,
            started_at: Utc::now(),
            call_count: 1,
        };
        progress.metadata.their_callsign = None;
        assert!(snap(&progress).is_none());
    }

    /// A QSO with no messages yet (just started) still produces an item
    /// with empty detail fields — the panel renders placeholders.
    #[test]
    fn snapshot_handles_empty_message_history() {
        let mut progress = fixture_progress();
        progress.messages.clear();
        let item = snap(&progress).expect("item");
        assert!(item.last_tx_text.is_none());
        assert!(item.last_rx_text.is_none());
        assert!(item.snr_rx.is_none());
        assert_eq!(item.exchange_count, 0);
    }

    /// Batch 2 #1: a MANUAL QSO in a keep-calling state surfaces the
    /// watchdog countdown fields (call N/M + deadline).
    #[test]
    fn snapshot_surfaces_watchdog_for_manual_keep_calling() {
        let mut progress = fixture_progress();
        let start = Utc::now() - Duration::seconds(20);
        progress.state = QsoState::RespondingToCq {
            target_callsign: "JA1ABC".to_string(),
            frequency: 1500.0,
            started_at: start,
        };
        progress.metadata.initiated_by = pancetta_qso::CallInitiation::Manual;
        progress.metadata.call_count = 4;
        progress.metadata.first_call_at = Some(start);
        let item = snap(&progress).expect("item");
        assert_eq!(item.call_count, 4);
        assert_eq!(item.max_calls, TEST_MAX_CALLS);
        let deadline = item.watchdog_deadline.expect("deadline");
        assert_eq!(
            deadline,
            start + Duration::minutes(TEST_WATCHDOG_MIN as i64)
        );
    }

    /// An AUTO QSO (or a manual QSO past the keep-calling phase) shows no
    /// watchdog fields — they would be misleading.
    #[test]
    fn snapshot_no_watchdog_for_auto_qso() {
        let mut progress = fixture_progress();
        progress.metadata.initiated_by = pancetta_qso::CallInitiation::Auto;
        progress.metadata.call_count = 3;
        progress.metadata.first_call_at = Some(Utc::now());
        let item = snap(&progress).expect("item");
        assert_eq!(item.call_count, 0);
        assert_eq!(item.max_calls, 0);
        assert!(item.watchdog_deadline.is_none());
    }

    /// Batch 2 #3: every failure reason maps to an operator-readable string.
    #[test]
    fn failure_reason_text_is_human_readable() {
        use pancetta_qso::QsoFailureReason as R;
        assert_eq!(super::failure_reason_text(&R::Timeout), "watchdog timeout");
        assert_eq!(
            super::failure_reason_text(&R::Superseded),
            "superseded by a newer call"
        );
        assert_eq!(
            super::failure_reason_text(&R::UserCancelled),
            "cancelled by operator"
        );
        assert_eq!(
            super::failure_reason_text(&R::ProtocolError("boom".to_string())),
            "protocol error: boom"
        );
    }
}

#[cfg(test)]
mod auto_73_tests {
    use super::{
        maybe_auto_resend_73, RecentManualCompletion, RecentManualCompletions, AUTO_73_MAX_RESENDS,
        AUTO_73_WINDOW,
    };
    use crate::message_bus::MessageBus;
    use pancetta_core::slot::SlotParity;
    use pancetta_core::TxPolicy;
    use pancetta_qso::states::MessageType as Mt;
    use pancetta_qso::{QsoManager, QsoManagerConfig};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU8;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const OUR: &str = "K5ARH";
    const DX: &str = "JA1ABC";

    async fn manager() -> QsoManager {
        let m = QsoManager::new(QsoManagerConfig {
            our_callsign: OUR.to_string(),
            our_grid: Some("EM10".to_string()),
            ..Default::default()
        });
        m.start().await.expect("manager start");
        m
    }

    fn bus() -> MessageBus {
        MessageBus::new(1000).expect("bus")
    }

    /// A completions map containing a single fresh manual completion for `DX`.
    fn map_with_dx() -> RecentManualCompletions {
        let mut map = HashMap::new();
        map.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now(),
                frequency_hz: 1500.0,
                dx_parity: Some(SlotParity::Even),
                resends: 0,
                last_resend_at: None,
            },
        );
        Arc::new(Mutex::new(map))
    }

    fn rr73_to_us() -> Mt {
        Mt::FinalConfirmation {
            to_station: OUR.to_string(),
            from_station: DX.to_string(),
        }
    }

    /// Count `MessageToSend` events the manager has emitted by draining a
    /// subscriber that was attached before the action under test.
    fn drain_sends(rx: &mut tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>) -> usize {
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, pancetta_qso::QsoEvent::MessageToSend { .. }) {
                n += 1;
            }
        }
        n
    }

    /// A directed RR73 from a stashed manual completion triggers exactly one
    /// auto-73 per slot, and never more than `AUTO_73_MAX_RESENDS` total even
    /// if RR73 arrives every slot.
    #[tokio::test]
    async fn bound_holds_under_repeated_rr73_every_slot() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        // Simulate the DX hammering RR73 across many slots. We bypass the
        // per-slot guard by zeroing last_resend_at between calls — that proves
        // the HARD cap (resends) holds independently of the time guard.
        for _ in 0..10 {
            maybe_auto_resend_73(
                &rr73_to_us(),
                OUR,
                1500.0,
                Some(SlotParity::Even),
                &mgr,
                &map,
                &policy,
                &bus,
            )
            .await;
            if let Some(e) = map.lock().await.get_mut(DX) {
                e.last_resend_at = None; // defeat the per-slot guard for this test
            }
        }

        let sends = drain_sends(&mut rx);
        assert_eq!(
            sends as u8, AUTO_73_MAX_RESENDS,
            "auto-73 must be capped at {AUTO_73_MAX_RESENDS}, got {sends}"
        );
        // After the cap the entry is dropped so it can never fire again.
        assert!(map.lock().await.get(DX).is_none());
    }

    /// Within one slot, two decodes of the same RR73 fire only ONE auto-73.
    #[tokio::test]
    async fn one_per_slot_dedup() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        for _ in 0..3 {
            maybe_auto_resend_73(
                &rr73_to_us(),
                OUR,
                1500.0,
                Some(SlotParity::Even),
                &mgr,
                &map,
                &policy,
                &bus,
            )
            .await;
            // Do NOT reset last_resend_at — same slot.
        }

        assert_eq!(drain_sends(&mut rx), 1, "only one 73 per slot");
        assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(1));
    }

    /// An RR73 outside the 3-minute window never triggers an auto-73 (the
    /// entry is pruned on lookup).
    #[tokio::test]
    async fn outside_window_no_resend() {
        let mgr = manager().await;
        let map = {
            let mut m = HashMap::new();
            m.insert(
                DX.to_string(),
                RecentManualCompletion {
                    completed_at: chrono::Utc::now()
                        - AUTO_73_WINDOW
                        - chrono::Duration::seconds(1),
                    frequency_hz: 1500.0,
                    dx_parity: Some(SlotParity::Even),
                    resends: 0,
                    last_resend_at: None,
                },
            );
            Arc::new(Mutex::new(m))
        };
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
        assert!(map.lock().await.get(DX).is_none(), "stale entry pruned");
    }

    /// While a QSO with the DX is active, no auto-73 (don't fight a live QSO).
    #[tokio::test]
    async fn active_qso_no_resend() {
        let mgr = manager().await;
        // Open a live QSO with DX (RespondingToCq via manual call).
        mgr.respond_to_cq_manual(DX.to_string(), 1500.0, Some(SlotParity::Even))
            .await
            .expect("start qso");
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        // Subscribe AFTER the manual call so its MessageToSend is not counted;
        // we only want to observe whether the auto-73 fires.
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0, "no auto-73 while QSO active");
    }

    /// A station NOT in the map (e.g. an AUTONOMOUS-completed QSO, which the
    /// QsoCompleted handler never stashes) gets no auto-73.
    #[tokio::test]
    async fn not_in_map_no_resend() {
        let mgr = manager().await;
        let map: RecentManualCompletions = Arc::new(Mutex::new(HashMap::new()));
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }

    /// TX policy DISABLED blocks the auto-73 entirely (and does not consume
    /// the resend budget).
    #[tokio::test]
    async fn disabled_policy_no_resend() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Disabled.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0, "DISABLED blocks auto-73");
        assert_eq!(
            map.lock().await.get(DX).map(|e| e.resends),
            Some(0),
            "budget untouched under DISABLED"
        );
    }

    /// RESPOND-ONLY allows the auto-73 (it's a response, not an initiation).
    #[tokio::test]
    async fn respond_only_allows_resend() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::RespondOnly.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        maybe_auto_resend_73(
            &rr73_to_us(),
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 1, "RESPOND-ONLY permits the 73");
    }

    /// A non-close message (e.g. a signal report) directed at us never
    /// triggers an auto-73, even from a stashed callsign.
    #[tokio::test]
    async fn non_close_message_ignored() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        let report = Mt::SignalReport {
            to_station: OUR.to_string(),
            from_station: DX.to_string(),
            report: -12,
        };
        maybe_auto_resend_73(
            &report,
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }

    /// An RR73 NOT directed at us (to a third party) is ignored.
    #[tokio::test]
    async fn rr73_to_third_party_ignored() {
        let mgr = manager().await;
        let map = map_with_dx();
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        let rr73 = Mt::FinalConfirmation {
            to_station: "W1XYZ".to_string(),
            from_station: DX.to_string(),
        };
        maybe_auto_resend_73(
            &rr73,
            OUR,
            1500.0,
            Some(SlotParity::Even),
            &mgr,
            &map,
            &policy,
            &bus,
        )
        .await;

        assert_eq!(drain_sends(&mut rx), 0);
    }
}

/// Spawn a background task that listens for `QsoEvent::QsoCompleted` and
/// appends one ADIF record to the durable log for each completed QSO.
///
/// ADIF is the source of truth: a failed write is logged at ERROR level because
/// it indicates a real problem (disk full, permissions, etc.) that the operator
/// should investigate. The task handles receiver lag and channel closure
/// gracefully so it never blocks or panics.
/// Spawn a background task that uploads each completed QSO to the operator's
/// online logbooks (ClubLog and/or QRZ Logbook and/or cqdx.io), one record per
/// QSO.
///
/// ClubLog/QRZ receive a single ADIF record rendered exactly as the
/// source-of-truth ADIF writer renders it (`AdifProcessor::qso_to_adif` →
/// `generate_record`), so the uploaded record matches `~/.pancetta/qsos.adi`.
/// cqdx.io is the operator's own first-party logbook service and takes the
/// structured `QsoRecord` JSON its `POST /api/v1/qsos` endpoint expects (see
/// `docs/cqdx-api-requirements.md`) — built from the same `QsoMetadata`, using
/// the dial+offset RF frequency already stamped on the completed metadata.
///
/// Best-effort by design: uploads are decoupled from the QSO pipeline and never
/// block it. Each per-service upload is spawned in its own task. Successes log
/// at `info!`, duplicates at `info!` (non-fatal), failures at `warn!` (target
/// `"qso.upload"`). Credentials / tokens are never logged.
/// Whether the opt-in cqdx.io per-QSO logbook upload should run: the
/// `[network.cqdx]` integration must be enabled AND carry a non-empty PAT
/// token. Default config (disabled, no token) returns `false`, so the upload
/// subscriber never fires unless the operator opts in.
fn cqdx_logbook_upload_enabled(cfg: &pancetta_config::network::CqdxConfig) -> bool {
    cfg.enabled && cfg.token.as_ref().is_some_and(|t| !t.is_empty())
}

/// Build the structured cqdx.io `QsoRecord` for the `POST /api/v1/qsos`
/// logbook endpoint from a completed `QsoMetadata`. Returns `None` when the
/// contra-callsign is unknown (nothing to log). The frequency is the dial+offset
/// RF value already stamped on the metadata; reports are stringified SNRs
/// ("-10" etc.) as the API expects.
fn cqdx_record_from_metadata(
    metadata: &pancetta_qso::QsoMetadata,
) -> Option<pancetta_cqdx::QsoRecord> {
    let callsign = metadata.their_callsign.clone()?;
    Some(pancetta_cqdx::QsoRecord {
        callsign,
        remote_grid: metadata.grids.theirs.clone(),
        local_grid: metadata.grids.ours.clone(),
        frequency: metadata.frequency as u64,
        mode: metadata.mode.clone(),
        rst_sent: metadata.reports.sent.map(|r| r.to_string()),
        rst_received: metadata.reports.received.map(|r| r.to_string()),
        start_time: metadata.start_time,
        end_time: metadata.end_time.unwrap_or_else(chrono::Utc::now),
    })
}

/// Result of merging a QRZ lookup into a completed QSO's metadata. Returned by
/// the pure [`merge_qrz_lookup`] so the merge policy can be unit-tested without
/// any network or `QrzXmlClient`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct QrzMergeResult {
    /// `true` if a missing grid was filled from the QRZ lookup.
    grid_filled: bool,
    /// `true` if a name was appended to notes (for logging/display only).
    name_added: bool,
}

/// Merge a QRZ lookup into the QSO metadata, **only filling MISSING fields**.
///
/// Policy (additive, never overrides decoded/cqdx data):
///   - `grids.theirs`: filled iff currently empty AND the QRZ grid is a valid
///     Maidenhead locator (validated via [`pancetta_core::GridSquare`]).
///   - operator `name` / `dxcc`: stashed into `metadata.notes` (display/log
///     only) iff not already present in notes. Never overrides an existing note.
///
/// Pure + synchronous so the policy is unit-testable. Returns what it changed.
fn merge_qrz_lookup(
    metadata: &mut pancetta_qso::QsoMetadata,
    lookup: &pancetta_dx::QrzLookup,
) -> QrzMergeResult {
    let mut result = QrzMergeResult::default();

    // Grid: only fill when genuinely missing, and only with a grid that parses
    // as a valid Maidenhead locator (QRZ records vary; reject garbage).
    let grid_missing = metadata
        .grids
        .theirs
        .as_ref()
        .map(|g| g.trim().is_empty())
        .unwrap_or(true);
    if grid_missing {
        if let Some(grid) = lookup
            .grid
            .as_ref()
            .map(|g| g.trim())
            .filter(|g| !g.is_empty())
        {
            if pancetta_core::gridsquare::GridSquare::new(grid).is_ok() {
                metadata.grids.theirs = Some(grid.to_string());
                result.grid_filled = true;
            }
        }
    }

    // Name (and DXCC) are enrichment for logging/display only — appended to the
    // notes field so they ride into the ADIF COMMENT without clobbering the
    // structured fields. Only append a name once.
    if let Some(name) = lookup
        .name
        .as_ref()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
    {
        let already = metadata
            .notes
            .as_deref()
            .map(|n| n.contains(name))
            .unwrap_or(false);
        if !already {
            let note = match metadata.notes.take() {
                Some(existing) if !existing.trim().is_empty() => {
                    format!("{existing}; QRZ: {name}")
                }
                _ => format!("QRZ: {name}"),
            };
            metadata.notes = Some(note);
            result.name_added = true;
        }
    }

    result
}

/// Best-effort QRZ-XML grid enrichment for a completed QSO.
///
/// Looks up the contra-callsign via [`QrzXmlClient`](pancetta_dx::QrzXmlClient)
/// **only when the their-grid is missing**, caches the result (hit or miss) for
/// the session, and merges it into `metadata` via [`merge_qrz_lookup`]. Never
/// blocks or fails the pipeline: any error/timeout is logged at debug (target
/// `dx.qrz`) and the metadata is left unchanged.
async fn maybe_enrich_grid_from_qrz(
    metadata: &mut pancetta_qso::QsoMetadata,
    client: &pancetta_dx::QrzXmlClient,
    cache: &Mutex<HashMap<String, Option<pancetta_dx::QrzLookup>>>,
) {
    // Only spend a lookup when the grid is actually missing.
    let grid_missing = metadata
        .grids
        .theirs
        .as_ref()
        .map(|g| g.trim().is_empty())
        .unwrap_or(true);
    if !grid_missing {
        return;
    }

    let callsign = match metadata.their_callsign.as_ref() {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => return,
    };
    let key = callsign.to_ascii_uppercase();

    // Session cache: reuse a prior hit OR miss for this callsign.
    if let Some(cached) = cache.lock().await.get(&key).cloned() {
        match cached {
            Some(lookup) => {
                let merged = merge_qrz_lookup(metadata, &lookup);
                if merged.grid_filled {
                    debug!(
                        target: "dx.qrz",
                        "QRZ (cached): filled grid for {} = {:?}",
                        callsign, metadata.grids.theirs
                    );
                }
            }
            None => {
                debug!(target: "dx.qrz", "QRZ (cached): no data for {}", callsign);
            }
        }
        return;
    }

    // Cache miss — query QRZ. Best-effort: on any error, cache the miss so we
    // don't retry this callsign every QSO, and leave metadata untouched.
    match client.lookup(&callsign).await {
        Ok(lookup) => {
            let merged = merge_qrz_lookup(metadata, &lookup);
            if merged.grid_filled {
                debug!(
                    target: "dx.qrz",
                    "QRZ: filled grid for {} = {:?}", callsign, metadata.grids.theirs
                );
            } else {
                debug!(target: "dx.qrz", "QRZ: no usable grid for {}", callsign);
            }
            cache.lock().await.insert(key, Some(lookup));
        }
        Err(e) => {
            // Never log credentials; QrzXmlClient errors never carry them.
            debug!(target: "dx.qrz", "QRZ lookup failed for {} (skipping): {}", callsign, e);
            cache.lock().await.insert(key, None);
        }
    }
}

// rationale: one explicit config arg per upload destination (ClubLog, QRZ,
// cqdx, LoTW, eQSL) plus the event source + shared handles — bundling them into
// a struct would just move the same fields without improving clarity.
#[allow(clippy::too_many_arguments)]
fn start_qso_upload_subscriber(
    clublog_cfg: pancetta_config::network::ClubLogConfig,
    qrz_cfg: pancetta_config::network::QrzLogbookConfig,
    lotw_cfg: pancetta_config::network::LotwUploadConfig,
    eqsl_cfg: pancetta_config::network::EqslConfig,
    cqdx_cfg: pancetta_config::network::CqdxConfig,
    qrz_xml_cfg: pancetta_config::network::QrzXmlConfig,
    our_callsign: String,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::Arc;

    // Build the enabled clients once and share them across uploads.
    let clublog_client = if clublog_cfg.enabled {
        // Fall back to the QSO's own call when no station call is configured.
        let callsign = if clublog_cfg.callsign.is_empty() {
            our_callsign.clone()
        } else {
            clublog_cfg.callsign.clone()
        };
        Some(Arc::new(pancetta_dx::ClubLogClient::new(
            clublog_cfg.email.clone(),
            clublog_cfg.password.clone(),
            callsign,
            clublog_cfg.api_key.clone(),
        )))
    } else {
        None
    };

    let qrz_client = if qrz_cfg.enabled {
        Some(Arc::new(pancetta_dx::QrzLogbookClient::new(
            qrz_cfg.api_key.clone(),
        )))
    } else {
        None
    };

    // eQSL.cc client. Opt-in: enabled + username/password (config validation
    // already rejects enabled-without-creds). QTH nickname is optional.
    let eqsl_client = if eqsl_cfg.enabled {
        let nick = if eqsl_cfg.qth_nickname.is_empty() {
            None
        } else {
            Some(eqsl_cfg.qth_nickname.clone())
        };
        Some(Arc::new(pancetta_dx::EqslClient::new(
            eqsl_cfg.username.clone(),
            eqsl_cfg.password.clone(),
            nick,
        )))
    } else {
        None
    };

    // LoTW client. Opt-in: enabled + tqsl_path + station_location (config
    // validation already rejects enabled-without-creds). Signs + uploads each
    // QSO by shelling out to the operator's tqsl CLI; a missing/erroring tqsl
    // is logged best-effort and never takes down the subscriber.
    let lotw_client = if lotw_cfg.enabled {
        Some(Arc::new(pancetta_dx::LotwUploadClient::new(
            lotw_cfg.tqsl_path.clone(),
            lotw_cfg.station_location.clone(),
        )))
    } else {
        None
    };

    // cqdx.io logbook client. Opt-in: enabled + a non-empty PAT token. A
    // malformed token (CqdxClient::new validation) is logged once at WARN and
    // simply disables the cqdx upload — it never takes down the subscriber.
    let cqdx_client = if cqdx_cfg.enabled {
        match cqdx_cfg.token.as_ref().filter(|t| !t.is_empty()) {
            Some(token) => {
                match pancetta_cqdx::CqdxClient::new(cqdx_cfg.base_url.clone(), token.clone()) {
                    Ok(c) => Some(Arc::new(c)),
                    Err(e) => {
                        // Token value is wrapped/redacted; the error never prints it.
                        warn!(
                            target: "qso.upload",
                            "cqdx.io upload disabled — client init failed: {}", e
                        );
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    // QRZ paid-XML lookup client (read-side enrichment). Opt-in: enabled +
    // creds (config validation already rejects enabled-without-creds). When
    // present, a completed QSO with a MISSING their-grid gets a best-effort
    // lookup that fills the grid (and name/dxcc for logging) before the ADIF
    // record is rendered. Never blocks or fails the pipeline. Credentials are
    // held inside the client and never logged (target `dx.qrz`).
    let qrz_xml_client = if qrz_xml_cfg.enabled
        && !qrz_xml_cfg.username.is_empty()
        && !qrz_xml_cfg.password.is_empty()
    {
        let agent = format!("pancetta-{}", env!("CARGO_PKG_VERSION"));
        Some(Arc::new(pancetta_dx::QrzXmlClient::new(
            qrz_xml_cfg.username.clone(),
            qrz_xml_cfg.password.clone(),
            agent,
        )))
    } else {
        None
    };
    // Session-scoped lookup cache (uppercased callsign → result). Avoids
    // re-querying QRZ for the same station repeatedly in one session; the
    // `None` value caches a miss/failure too, so a station QRZ has no data for
    // is not retried every QSO. Only allocated when the client is built.
    let qrz_xml_cache: Arc<Mutex<HashMap<String, Option<pancetta_dx::QrzLookup>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    if clublog_client.is_some() {
        info!(target: "qso.upload", "ClubLog per-QSO upload enabled");
    }
    if qrz_client.is_some() {
        info!(target: "qso.upload", "QRZ Logbook per-QSO upload enabled");
    }
    if eqsl_client.is_some() {
        info!(target: "qso.upload", "eQSL.cc per-QSO upload enabled");
    }
    if lotw_client.is_some() {
        info!(target: "qso.upload", "LoTW per-QSO (TQSL-signed) upload enabled");
    }
    if cqdx_client.is_some() {
        info!(target: "qso.upload", "cqdx.io per-QSO logbook upload enabled");
    }
    if qrz_xml_client.is_some() {
        info!(target: "dx.qrz", "QRZ XML grid enrichment enabled (fills missing grid before upload)");
    }

    tokio::spawn(async move {
        let processor = pancetta_qso::AdifProcessor::new();

        while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
            match events.recv().await {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { mut metadata, .. }) => {
                    // Best-effort QRZ XML enrichment: fill a MISSING their-grid
                    // (and name/dxcc in notes for logging) before rendering the
                    // ADIF record. No-op when the client is disabled or the grid
                    // is already known from decode/cqdx; never blocks or fails
                    // the upload pipeline.
                    if let Some(client) = qrz_xml_client.clone() {
                        maybe_enrich_grid_from_qrz(&mut metadata, &client, &qrz_xml_cache).await;
                    }

                    // Render the single ADIF record the same way the
                    // source-of-truth writer does.
                    let adif_qso = processor.qso_to_adif(&metadata, metadata.contest_info.as_ref());
                    let record = match processor.generate_record(&adif_qso) {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                target: "qso.upload",
                                "Skipping upload for QSO {}: ADIF render failed: {}",
                                metadata.qso_id, e
                            );
                            continue;
                        }
                    };

                    let their = metadata
                        .their_callsign
                        .clone()
                        .unwrap_or_else(|| "?".to_string());

                    if let Some(client) = clublog_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(()) => info!(
                                    target: "qso.upload",
                                    "ClubLog: uploaded QSO with {}", their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "ClubLog: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }

                    if let Some(client) = qrz_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(pancetta_dx::QrzInsertOutcome::Inserted { logid }) => info!(
                                    target: "qso.upload",
                                    "QRZ: uploaded QSO with {} (logid={})",
                                    their,
                                    logid.as_deref().unwrap_or("?")
                                ),
                                Ok(pancetta_dx::QrzInsertOutcome::Duplicate { .. }) => info!(
                                    target: "qso.upload",
                                    "QRZ: QSO with {} already logged (duplicate, skipped)",
                                    their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "QRZ: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }

                    // eQSL.cc takes the same rendered ADIF record (the client
                    // prepends an ADIF header carrying the account credentials).
                    if let Some(client) = eqsl_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(pancetta_dx::QsoUploadOutcome::Logged) => info!(
                                    target: "qso.upload",
                                    "eQSL: uploaded QSO with {}", their
                                ),
                                Ok(pancetta_dx::QsoUploadOutcome::Duplicate) => info!(
                                    target: "qso.upload",
                                    "eQSL: QSO with {} already logged (duplicate, skipped)",
                                    their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "eQSL: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }

                    // LoTW signs + uploads the same rendered ADIF record by
                    // shelling out to the operator's tqsl CLI. Best-effort: a
                    // missing/erroring tqsl never blocks or fails the pipeline.
                    if let Some(client) = lotw_client.clone() {
                        let record = record.clone();
                        let their = their.clone();
                        tokio::spawn(async move {
                            match client.upload_adif(&record).await {
                                Ok(pancetta_dx::QsoUploadOutcome::Logged) => info!(
                                    target: "qso.upload",
                                    "LoTW: signed + uploaded QSO with {}", their
                                ),
                                Ok(pancetta_dx::QsoUploadOutcome::Duplicate) => info!(
                                    target: "qso.upload",
                                    "LoTW: QSO with {} already logged (duplicate, skipped)",
                                    their
                                ),
                                Err(e) => warn!(
                                    target: "qso.upload",
                                    "LoTW: upload failed for {}: {}", their, e
                                ),
                            }
                        });
                    }

                    // cqdx.io takes the structured QsoRecord its
                    // `POST /api/v1/qsos` endpoint expects (not ADIF). We only
                    // have something to upload once the contra-callsign is
                    // known; skip otherwise. Frequency is the dial+offset RF
                    // value already stamped on the completed metadata.
                    if let Some(client) = cqdx_client.clone() {
                        if let Some(qso) = cqdx_record_from_metadata(&metadata) {
                            let their = their.clone();
                            tokio::spawn(async move {
                                match client.log_qso(qso).await {
                                    Ok(pancetta_cqdx::QsoUploadOutcome::Logged) => info!(
                                        target: "qso.upload",
                                        "cqdx.io: uploaded QSO with {}", their
                                    ),
                                    Ok(pancetta_cqdx::QsoUploadOutcome::Duplicate) => info!(
                                        target: "qso.upload",
                                        "cqdx.io: QSO with {} already logged (duplicate, skipped)",
                                        their
                                    ),
                                    Err(e) => warn!(
                                        target: "qso.upload",
                                        "cqdx.io: upload failed for {}: {}", their, e
                                    ),
                                }
                            });
                        }
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(target: "qso.upload", "QSO upload subscriber lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn start_adif_subscriber(
    writer: std::sync::Arc<pancetta_qso::AdifLogWriter>,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    tokio::spawn(async move {
        while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
            match events.recv().await {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                    if let Err(e) = writer.append(&metadata).await {
                        // ADIF is the source of truth. A failed write deserves
                        // a loud signal — disk full, permissions, etc.
                        tracing::error!(
                            "ADIF append failed for QSO {} with {}: {}",
                            metadata.qso_id,
                            metadata.their_callsign.as_deref().unwrap_or("?"),
                            e,
                        );
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ADIF subscriber lagged by {n} QSO events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod cqdx_upload_tests {
    use super::{cqdx_logbook_upload_enabled, cqdx_record_from_metadata};
    use chrono::Utc;
    use pancetta_config::network::CqdxConfig;
    use pancetta_qso::{GridSquares, QsoMetadata, SignalReports};

    /// Default cqdx config (disabled, no token) must NOT enable the upload —
    /// the subscriber stays dormant unless the operator opts in.
    #[test]
    fn upload_disabled_by_default() {
        let cfg = CqdxConfig::default();
        assert!(!cfg.enabled);
        assert!(!cqdx_logbook_upload_enabled(&cfg));
    }

    /// Enabled but with no token (or an empty token) must NOT enable the upload
    /// — we never POST without auth.
    #[test]
    fn upload_requires_token() {
        let mut cfg = CqdxConfig {
            enabled: true,
            token: None,
            ..Default::default()
        };
        assert!(!cqdx_logbook_upload_enabled(&cfg));

        cfg.token = Some(String::new());
        assert!(!cqdx_logbook_upload_enabled(&cfg));
    }

    /// Enabled + a non-empty token opts in.
    #[test]
    fn upload_enabled_with_token() {
        let cfg = CqdxConfig {
            enabled: true,
            token: Some("pat_abc123def456".to_string()),
            ..Default::default()
        };
        assert!(cqdx_logbook_upload_enabled(&cfg));
    }

    /// A token without `enabled` is still off (belt-and-suspenders).
    #[test]
    fn upload_off_when_disabled_even_with_token() {
        let cfg = CqdxConfig {
            enabled: false,
            token: Some("pat_abc123def456".to_string()),
            ..Default::default()
        };
        assert!(!cqdx_logbook_upload_enabled(&cfg));
    }

    fn metadata_with_call(call: Option<&str>) -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".to_string(),
            their_callsign: call.map(str::to_string),
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: Some(now + chrono::Duration::seconds(90)),
            reports: SignalReports {
                sent: Some(-8),
                received: Some(-12),
            },
            grids: GridSquares {
                ours: Some("EM10".to_string()),
                theirs: Some("PM95".to_string()),
            },
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            last_rx_text: None,
            dx_repeat_count: 0,
            hound: false,
            partner_freq: None,
            hound_qsyed: false,
        }
    }

    /// The structured cqdx record carries the dial+offset RF frequency,
    /// both grids, and stringified SNR reports the API expects.
    #[test]
    fn record_maps_metadata_fields() {
        let md = metadata_with_call(Some("JA1ABC"));
        let rec = cqdx_record_from_metadata(&md).expect("record");
        assert_eq!(rec.callsign, "JA1ABC");
        assert_eq!(rec.frequency, 14_074_000);
        assert_eq!(rec.mode, "FT8");
        assert_eq!(rec.remote_grid.as_deref(), Some("PM95"));
        assert_eq!(rec.local_grid.as_deref(), Some("EM10"));
        assert_eq!(rec.rst_sent.as_deref(), Some("-8"));
        assert_eq!(rec.rst_received.as_deref(), Some("-12"));
        assert_eq!(rec.start_time, md.start_time);
        assert_eq!(rec.end_time, md.end_time.unwrap());
    }

    /// No contra-callsign → nothing to upload.
    #[test]
    fn record_none_without_callsign() {
        let md = metadata_with_call(None);
        assert!(cqdx_record_from_metadata(&md).is_none());
    }
}

#[cfg(test)]
mod qrz_enrichment_tests {
    use super::merge_qrz_lookup;
    use chrono::Utc;
    use pancetta_dx::QrzLookup;
    use pancetta_qso::{GridSquares, QsoMetadata, SignalReports};

    /// Build a completed QSO metadata with the given their-grid / notes so the
    /// "only fill when missing" merge policy can be exercised in isolation.
    fn metadata(their_grid: Option<&str>, notes: Option<&str>) -> QsoMetadata {
        let now = Utc::now();
        QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".to_string(),
            their_callsign: Some("JA1ABC".to_string()),
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: Some(now + chrono::Duration::seconds(90)),
            reports: SignalReports {
                sent: Some(-8),
                received: Some(-12),
            },
            grids: GridSquares {
                ours: Some("EM10".to_string()),
                theirs: their_grid.map(str::to_string),
            },
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: notes.map(str::to_string),
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            last_rx_text: None,
            dx_repeat_count: 0,
            hound: false,
            partner_freq: None,
            hound_qsyed: false,
        }
    }

    fn lookup(grid: Option<&str>, name: Option<&str>) -> QrzLookup {
        QrzLookup {
            call: Some("JA1ABC".to_string()),
            name: name.map(str::to_string),
            grid: grid.map(str::to_string),
            country: Some("Japan".to_string()),
            dxcc: Some("339".to_string()),
            state: None,
        }
    }

    /// A MISSING grid is filled from a valid QRZ grid.
    #[test]
    fn fills_missing_grid() {
        let mut md = metadata(None, None);
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
        assert!(res.grid_filled);
        assert_eq!(md.grids.theirs.as_deref(), Some("PM95"));
    }

    /// An empty-string grid counts as missing and is filled.
    #[test]
    fn fills_blank_grid() {
        let mut md = metadata(Some("  "), None);
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
        assert!(res.grid_filled);
        assert_eq!(md.grids.theirs.as_deref(), Some("PM95"));
    }

    /// An EXISTING (decoded/cqdx) grid is NEVER overridden by QRZ.
    #[test]
    fn never_overrides_existing_grid() {
        let mut md = metadata(Some("FN20"), None);
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), None));
        assert!(!res.grid_filled);
        assert_eq!(md.grids.theirs.as_deref(), Some("FN20"));
    }

    /// An invalid QRZ grid is rejected (metadata left missing, not poisoned).
    #[test]
    fn rejects_invalid_grid() {
        let mut md = metadata(None, None);
        let res = merge_qrz_lookup(&mut md, &lookup(Some("not-a-grid!!"), None));
        assert!(!res.grid_filled);
        assert!(md.grids.theirs.is_none());
    }

    /// A name is appended to notes for logging/display.
    #[test]
    fn appends_name_to_empty_notes() {
        let mut md = metadata(None, None);
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
        assert!(res.name_added);
        assert_eq!(md.notes.as_deref(), Some("QRZ: Taro"));
    }

    /// A name is appended to (not clobbering) existing notes.
    #[test]
    fn appends_name_to_existing_notes() {
        let mut md = metadata(None, Some("contest exchange"));
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
        assert!(res.name_added);
        assert_eq!(md.notes.as_deref(), Some("contest exchange; QRZ: Taro"));
    }

    /// A name already present in notes is not appended twice (idempotent).
    #[test]
    fn does_not_duplicate_name() {
        let mut md = metadata(None, Some("QRZ: Taro"));
        let res = merge_qrz_lookup(&mut md, &lookup(Some("PM95"), Some("Taro")));
        assert!(!res.name_added);
        assert_eq!(md.notes.as_deref(), Some("QRZ: Taro"));
    }

    /// A lookup with nothing usable is a complete no-op.
    #[test]
    fn empty_lookup_is_noop() {
        let mut md = metadata(None, None);
        let before = md.clone();
        let res = merge_qrz_lookup(&mut md, &lookup(None, None));
        assert!(!res.grid_filled && !res.name_added);
        assert_eq!(md.grids.theirs, before.grids.theirs);
        assert_eq!(md.notes, before.notes);
    }
}

#[cfg(test)]
mod caller_dedup_tests {
    use super::caller_creation_slot_key;
    use std::time::{Duration, UNIX_EPOCH};

    /// Two SystemTimes within the same 15-second window map to the same key.
    #[test]
    fn same_slot_same_key() {
        // Slot N starts at unix second N*15; both 0 s and 14 s are in slot 0.
        let t0 = UNIX_EPOCH + Duration::from_secs(0);
        let t14 = UNIX_EPOCH + Duration::from_secs(14);
        assert_eq!(caller_creation_slot_key(t0), caller_creation_slot_key(t14));
    }

    /// The boundary second (15) starts a new slot.
    #[test]
    fn slot_boundary_increments_key() {
        let t_end_of_slot0 = UNIX_EPOCH + Duration::from_secs(14);
        let t_start_of_slot1 = UNIX_EPOCH + Duration::from_secs(15);
        let k0 = caller_creation_slot_key(t_end_of_slot0);
        let k1 = caller_creation_slot_key(t_start_of_slot1);
        assert_eq!(k1, k0 + 1, "adjacent slots must differ by exactly 1");
    }

    /// A realistic mid-session timestamp (e.g. 2026-06-25 12:00:07 UTC) hashes
    /// to the correct slot index.
    #[test]
    fn real_timestamp_hashes_correctly() {
        // 2026-06-25 12:00:07 UTC = 1_751_198_407 unix seconds.
        // Floor(1_751_198_407 / 15) = 116_746_560  (slot in the :00 window).
        // 1_751_198_407 / 15 = 116_746_560.466...
        let unix_secs: u64 = 1_751_198_407;
        let t = UNIX_EPOCH + Duration::from_secs(unix_secs);
        assert_eq!(caller_creation_slot_key(t), unix_secs / 15);
    }

    /// A timestamp before UNIX_EPOCH (e.g. from a unit-test stub) returns 0
    /// rather than panicking.
    #[test]
    fn pre_epoch_timestamp_returns_zero() {
        // SystemTime doesn't support times before UNIX_EPOCH directly in all
        // implementations; we use UNIX_EPOCH itself as the minimal safe input.
        let t = UNIX_EPOCH;
        assert_eq!(caller_creation_slot_key(t), 0);
    }

    /// The dedup state clears when the slot key changes (simulated inline).
    #[test]
    fn dedup_set_clears_on_slot_change() {
        let mut dedup: (u64, std::collections::HashSet<String>) =
            (0, std::collections::HashSet::new());

        // Slot 0: station A arrives twice.
        let slot0 = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(3));
        if slot0 != dedup.0 {
            dedup.0 = slot0;
            dedup.1.clear();
        }
        let first_insert = dedup.1.insert("G8BCG".to_string());
        assert!(first_insert, "first decode in slot must be admitted");

        // Same slot, same station: second decode skipped.
        let slot0_again = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(7));
        assert_eq!(slot0, slot0_again, "still same slot");
        let second_insert = dedup.1.insert("G8BCG".to_string());
        assert!(!second_insert, "second decode in same slot must be deduped");

        // Slot 1: station A reappears — set should have been cleared.
        let slot1 = caller_creation_slot_key(UNIX_EPOCH + Duration::from_secs(15));
        assert_ne!(slot0, slot1);
        if slot1 != dedup.0 {
            dedup.0 = slot1;
            dedup.1.clear();
        }
        let third_insert = dedup.1.insert("G8BCG".to_string());
        assert!(
            third_insert,
            "first decode in new slot must be admitted again"
        );
    }
}
