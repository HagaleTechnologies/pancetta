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
///   - never when a live QSO with that station is already active,
///   - the FIRST auto-resend never fires before [`AUTO_73_FIRST_RESEND_MIN_DELAY`]
///     has elapsed since completion (SM-F3/TX-F10 — see that constant's doc).
const AUTO_73_WINDOW: chrono::Duration = chrono::Duration::minutes(3);
/// Maximum number of auto re-sends of our 73 per completed manual QSO.
const AUTO_73_MAX_RESENDS: u8 = 3;
/// Minimum spacing between auto re-sends (one FT8 slot is 15 s; we use a
/// slightly-under-slot guard so we fire at most once per slot even if the
/// DX's RR73 is decoded a hair early/late).
const AUTO_73_MIN_SPACING: chrono::Duration = chrono::Duration::seconds(14);
/// SM-F3/TX-F10: minimum time that must elapse since `completed_at` before
/// the FIRST auto-resend (`resends` 0 → 1) is allowed to fire.
///
/// Guards against a real race two independent review tracks flagged: FT8
/// decoders routinely emit 2-4 copies of one frame, and `maybe_auto_resend_73`
/// runs per decode copy, *before* `process_message` gets to it. The QSO's own
/// closing `MessageToSend(73)` — emitted by the very same `QsoCompleted` that
/// stashes this entry — can still be waiting out the TX scheduler's defer
/// logic (`schedule_tx`: late-in-slot pushes to the next slot of the required
/// parity, up to ~30s away) when a duplicate decode of the DX's RR73 arrives.
/// Without this guard, that duplicate decode reads `resends == 0` and fires a
/// SECOND 73 for a QSO whose FIRST 73 hasn't even keyed yet — a double-PTT
/// bug in the making. 30s (one worst-case defer window) plus a small margin
/// comfortably covers it while staying tiny next to [`AUTO_73_WINDOW`] (3
/// min), so a truly later, genuine repeat of RR73 (the DX really didn't copy
/// our first 73) is unaffected.
const AUTO_73_FIRST_RESEND_MIN_DELAY: chrono::Duration = chrono::Duration::seconds(32);

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
    /// SECURITY: `true` iff the completed QSO was remote-initiated. The auto-73
    /// resend is itself a TX, so it must inherit the origin — a remote QSO's
    /// auto-73 is `TxOrigin::Remote` and armed-TX gated (fail-closed: if the
    /// arm lapsed after completion, the resend is dropped, never keyed as Local).
    remote_origin: bool,
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
    /// SECURITY: `true` iff this call was initiated by a REMOTE operator
    /// (station agent). Carried across the cross-parity deferral so the
    /// promoted QSO is opened with `remote_origin = true` — its TX stays
    /// `TxOrigin::Remote` and armed-TX-gated. `false` for every local/TUI call.
    remote_origin: bool,
    /// Which rung of the response ladder to open at on promotion — replayed
    /// via `respond_to_caller` (see [`promote_pending_manual_calls`]).
    /// `StartQso`-originated entries always use [`pancetta_core::ResponseStep::Grid`],
    /// which `respond_to_caller` degrades to exactly the `respond_to_cq_with`
    /// call this queue used before `RespondToCaller` entries existed — byte-
    /// identical for the pre-existing StartQso path.
    step: pancetta_core::ResponseStep,
    /// Our measurement of the caller's signal (drives the report WE send).
    /// Only meaningful for `RespondToCaller`-originated entries opening at
    /// `Report`/`ReportAck`/`Rr73`/`SeventyThree`. `None` for `StartQso`
    /// entries (a `Grid` open has no report to send yet).
    our_snr_of_them: Option<f32>,
    /// Their report of us, if known. Currently always `None` at both call
    /// sites (the engine defaults it) — carried for forward completeness.
    their_report: Option<i8>,
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
/// Divergence can come from an operator-held offset, a collision nudge, or the
/// passband floor/ceiling clamp. All three retain two-strike drift protection
/// on `partner_freq`; confirmation moves where we listen for the DX but never
/// moves the deliberately selected `tx_off`.
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
            // Replay via `respond_to_caller`, which degrades a `Grid` step to
            // exactly the `respond_to_cq_with` call this promotion path used
            // before `RespondToCaller` entries could be queued here — so a
            // `StartQso`-originated entry (always `step == Grid`) is
            // byte-identical to the pre-existing behavior. A
            // `RespondToCaller`-originated entry opens at its own step
            // (Report/ReportAck/Rr73/SeventyThree) instead.
            qso_manager
                .respond_to_caller(
                    p.callsign.clone(),
                    tx_off,
                    p.dx_parity,
                    p.step,
                    p.our_snr_of_them,
                    p.their_report,
                    partner,
                    p.remote_origin,
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
        crate::coordinator::tx::emit_diagnostic_full(
            message_bus,
            ComponentId::Qso,
            "qso",
            pancetta_core::DiagnosticLevel::Warn,
            format!(
                "Retired queued call to {call} — no free TX window in {}m",
                QUEUED_CALL_TTL.as_secs() / 60
            ),
            None,
            Some(&call),
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
    let entry_remote_origin;
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
        // SM-F3/TX-F10: don't let a duplicate/near-immediate decode of the
        // closing RR73 fire our FIRST auto-resend before the original 73
        // (emitted by the same QsoCompleted that stashed this entry) has had
        // time to clear the TX scheduler's defer window. Only gates the
        // 0 -> 1 transition — once a genuine first resend has gone out, later
        // resends are governed by AUTO_73_MIN_SPACING as before. We do NOT
        // consume the budget here: this is a no-op skip, not a burned
        // attempt, so a genuine later RR73 (past this guard) still gets its
        // full allotment.
        if entry.resends == 0
            && now.signed_duration_since(entry.completed_at) < AUTO_73_FIRST_RESEND_MIN_DELAY
        {
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
        // SECURITY: the auto-73 inherits the completed QSO's origin so a remote
        // QSO's resend stays `TxOrigin::Remote` and armed-TX gated.
        entry_remote_origin = entry.remote_origin;
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
            None,                // auto-73: always Tx=Rx, no partner offset
            entry_remote_origin, // inherit the completed QSO's origin
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
        emit_skip_diagnostic(
            message_bus,
            SkipSite::AutoAnswerCrossParity {
                callsign: answer.their_call.clone(),
                active_side: current_side,
            },
        )
        .await;
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
        debug!(target: "qso", "Not auto-answering {} — at capacity {}/{}", answer.their_call, active_count, effective_cap);
        emit_skip_diagnostic(
            message_bus,
            SkipSite::AutoAnswerAtCapacity {
                callsign: answer.their_call.clone(),
                active: active_count,
                cap: effective_cap,
                fox_mode: is_fox,
            },
        )
        .await;
        return;
    }

    info!(
        target: "qso",
        "Auto-answering {} at {:?} on {:.0} Hz (caller — autonomous-independent)",
        answer.their_call, answer.step, frequency_hz
    );

    // FQ-F4/TX-F6: de-conflict the raw Tx=Rx candidate (the caller's own
    // decoded frequency) against our OTHER active streams before latching
    // it — exactly mirroring `compute_manual_tx_offset`'s pattern (the
    // manual-call path already does this). Without this, two concurrent
    // QSOs can collide within MIN_TX_SEPARATION_HZ, which can make
    // `modulate_multi_tx`'s pairwise-separation check fail the ENTIRE
    // multi-TX bundle when the coalescer folds them together. A genuine
    // no-op (byte-identical) when nothing is within MIN_TX_SEPARATION_HZ —
    // `deconflict_offset` returns the input unchanged in that case.
    let active = qso_manager.active_tx_offsets().await;
    let (tx_off, partner_freq) = compute_manual_tx_offset(frequency_hz, false, 0, &active);
    if tx_off != frequency_hz {
        info!(
            target: "qso",
            "Auto-answer TX offset de-conflicted: caller_freq={:.0} → tx_off={:.0} Hz ({} active)",
            frequency_hz, tx_off, active.len()
        );
    }

    match qso_manager
        .respond_to_caller(
            answer.their_call.clone(),
            tx_off,
            their_parity,
            answer.step,
            Some(snr),
            answer.their_report,
            partner_freq,
            false, // local decode-loop auto-answer, never remote
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
        R::SupervisorRestart => "dropped by an internal restart".to_string(),
        R::ComponentCrash(component) => format!("{component} crashed mid-QSO"),
        // PAN-17: distinct from "watchdog timeout" (the DX never answered) —
        // this QSO's outgoing message could never be transmitted at all, so
        // the watchdog retired it fast instead of waiting on the DX.
        R::MessageUnencodable(detail) => format!("cannot transmit this message: {detail}"),
    }
}

fn timeout_detail(
    last_state: Option<&pancetta_qso::QsoState>,
    metadata: &pancetta_qso::QsoMetadata,
    max_calls: u32,
    watchdog_minutes: u64,
) -> String {
    use pancetta_qso::{CallInitiation, QsoState};
    let initial_call = matches!(
        last_state,
        Some(QsoState::CallingCq { .. } | QsoState::RespondingToCq { .. })
    );
    if metadata.initiated_by == CallInitiation::Manual {
        if initial_call && metadata.call_count >= max_calls {
            return format!(
                "watchdog timeout — no reply after {} calls",
                metadata.call_count
            );
        }
        if matches!(
            last_state,
            Some(QsoState::SendingReport { .. } | QsoState::WaitingForReport { .. })
        ) {
            return "watchdog timeout — no reply after we sent the report".to_string();
        }
        if metadata.first_call_at.is_some_and(|started| {
            chrono::Utc::now() - started >= chrono::Duration::minutes(watchdog_minutes as i64)
        }) {
            return format!("watchdog timeout — no reply in {watchdog_minutes} min");
        }
    } else if initial_call {
        return "watchdog timeout — autonomous pounce never answered".to_string();
    }
    "watchdog timeout".to_string()
}

pub(crate) enum SkipSite {
    CrossParityDeferral {
        callsign: Option<String>,
        active_side: Option<pancetta_core::slot::SlotParity>,
        wanted: Option<pancetta_core::slot::SlotParity>,
    },
    AutonomousOpenFailed {
        callsign: Option<String>,
        error: String,
    },
    AutoAnswerCrossParity {
        callsign: String,
        active_side: Option<pancetta_core::slot::SlotParity>,
    },
    AutoAnswerAtCapacity {
        callsign: String,
        active: usize,
        cap: usize,
        fox_mode: bool,
    },
}

pub(crate) struct SkipDiagnostic {
    pub target: &'static str,
    pub level: pancetta_core::DiagnosticLevel,
    pub text: String,
    pub callsign: Option<String>,
}

pub(crate) fn skip_diagnostic(site: SkipSite) -> SkipDiagnostic {
    use pancetta_core::DiagnosticLevel as L;
    match site {
        SkipSite::CrossParityDeferral { callsign, active_side, wanted } => {
            let call = callsign.as_deref().unwrap_or("CQ");
            SkipDiagnostic { target: "qso.autonomous", level: L::Info, text: format!("Deferred autonomous QSO with {call} — cross-parity (active side {active_side:?}, wanted {wanted:?})"), callsign }
        }
        SkipSite::AutonomousOpenFailed { callsign, error } => {
            let call = callsign.as_deref().unwrap_or("CQ");
            SkipDiagnostic { target: "qso.autonomous", level: L::Warn, text: format!("Autonomous QSO with {call} not opened — {error}"), callsign }
        }
        SkipSite::AutoAnswerCrossParity { callsign, active_side } => SkipDiagnostic { target: "qso", level: L::Info, text: format!("Skipped auto-answer to {callsign} — cross-parity (active side {active_side:?}); queue manually to override"), callsign: Some(callsign) },
        SkipSite::AutoAnswerAtCapacity { callsign, active, cap, fox_mode } => SkipDiagnostic { target: "qso", level: L::Info, text: format!("Skipped auto-answer to {callsign} — at capacity {active}/{cap}{}", if fox_mode { " (Fox mode)" } else { "" }), callsign: Some(callsign) },
    }
}

pub(crate) async fn emit_skip_diagnostic(message_bus: &MessageBus, site: SkipSite) {
    let d = skip_diagnostic(site);
    crate::coordinator::tx::emit_diagnostic_full(
        message_bus,
        ComponentId::Qso,
        d.target,
        d.level,
        d.text,
        None,
        d.callsign.as_deref(),
    )
    .await;
}

#[cfg(test)]
mod pan6_diagnostic_tests {
    use super::*;
    use pancetta_core::slot::SlotParity;
    use pancetta_qso::{CallInitiation, QsoMetadata, QsoState};

    fn metadata(initiated_by: CallInitiation, call_count: u32, age_minutes: i64) -> QsoMetadata {
        let now = chrono::Utc::now();
        QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".into(),
            their_callsign: Some("W1AW".into()),
            frequency: 14_074_000.0,
            mode: "FT8".into(),
            start_time: now,
            end_time: None,
            reports: Default::default(),
            grids: Default::default(),
            contest_info: None,
            tags: Default::default(),
            notes: None,
            tx_parity: None,
            tx_parity_provisional: false,
            initiated_by,
            role: Default::default(),
            call_count,
            first_call_at: Some(now - chrono::Duration::minutes(age_minutes)),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            last_rx_text: None,
            dx_repeat_count: 0,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
        }
    }

    fn calling() -> QsoState {
        QsoState::CallingCq {
            frequency: 14_074_000.0,
            started_at: chrono::Utc::now(),
            call_count: 1,
        }
    }

    #[test]
    fn call_cap_wording_excludes_time_bound() {
        let detail = timeout_detail(
            Some(&calling()),
            &metadata(CallInitiation::Manual, 25, 10),
            25,
            5,
        );
        assert!(detail.contains("25 calls"));
        assert!(!detail.contains("5 min"));
    }

    #[test]
    fn time_bound_wording_excludes_call_count() {
        let detail = timeout_detail(
            Some(&calling()),
            &metadata(CallInitiation::Manual, 2, 6),
            25,
            5,
        );
        assert!(detail.contains("5 min"));
        assert!(!detail.contains("2 calls"));
    }

    #[test]
    fn engaged_qso_names_report_stage_before_elapsed_bound() {
        let state = QsoState::WaitingForReport {
            their_callsign: "W1AW".into(),
            frequency: 14_074_000.0,
            started_at: chrono::Utc::now(),
            their_grid: None,
            our_report: -10,
        };
        assert_eq!(
            timeout_detail(Some(&state), &metadata(CallInitiation::Manual, 2, 6), 25, 5),
            "watchdog timeout — no reply after we sent the report"
        );
    }

    #[test]
    fn auto_pounce_and_ambiguous_fallback_are_pinned() {
        assert_eq!(
            timeout_detail(
                Some(&calling()),
                &metadata(CallInitiation::Auto, 1, 0),
                25,
                5
            ),
            "watchdog timeout — autonomous pounce never answered"
        );
        assert_eq!(
            timeout_detail(None, &metadata(CallInitiation::Auto, 1, 0), 25, 5),
            "watchdog timeout"
        );
        assert!(format!(
            "QSO failed: {}",
            timeout_detail(None, &metadata(CallInitiation::Auto, 1, 0), 25, 5)
        )
        .starts_with("QSO failed: "));
    }

    #[test]
    fn all_skip_sites_have_operator_facing_text_without_counter_prefix_collisions() {
        let diagnostics = [
            skip_diagnostic(SkipSite::CrossParityDeferral {
                callsign: Some("W1AW".into()),
                active_side: Some(SlotParity::Even),
                wanted: Some(SlotParity::Odd),
            }),
            skip_diagnostic(SkipSite::AutonomousOpenFailed {
                callsign: None,
                error: "busy".into(),
            }),
            skip_diagnostic(SkipSite::AutoAnswerCrossParity {
                callsign: "W1AW".into(),
                active_side: Some(SlotParity::Even),
            }),
            skip_diagnostic(SkipSite::AutoAnswerAtCapacity {
                callsign: "W1AW".into(),
                active: 2,
                cap: 2,
                fox_mode: true,
            }),
        ];
        assert!(diagnostics.iter().all(|d| !d.text.is_empty()));
        assert!(diagnostics.iter().all(|d| !d.text.starts_with("QSO with")
            && !d.text.starts_with("QSO failed:")
            && !d.text.starts_with("dropping stale")));
    }

    #[tokio::test]
    async fn a_skip_diagnostic_reaches_the_tui_channel() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();
        emit_skip_diagnostic(
            &bus,
            SkipSite::AutoAnswerAtCapacity {
                callsign: "W1AW".into(),
                active: 2,
                cap: 2,
                fox_mode: false,
            },
        )
        .await;
        let message = receiver.try_recv().expect("skip diagnostic");
        assert_eq!(message.source, ComponentId::Qso);
        match message.message_type {
            MessageType::DiagnosticEvent { callsign, text, .. } => {
                assert_eq!(callsign.as_deref(), Some("W1AW"));
                assert!(text.contains("at capacity 2/2"));
            }
            other => panic!("expected DiagnosticEvent, got {other:?}"),
        }
    }
}

fn rejection_reason_text(reason: &pancetta_qso::RejectionReason) -> &'static str {
    use pancetta_qso::RejectionReason as R;
    match reason {
        R::SenderNotPartner => "sender is not our QSO partner",
        R::AddresseeNotUs => "not addressed to us",
        R::SenderAndAddresseeMismatch => "wrong sender and wrong addressee",
        R::UnsafeCompoundUpgrade => "unsafe compound-callsign upgrade",
    }
}

fn drop_should_promote(reason: &pancetta_qso::QsoFailureReason) -> bool {
    !matches!(reason, pancetta_qso::QsoFailureReason::ComponentCrash(_))
}

/// Build a `RecentQsoOutcome` (observability-diagnostics-plan.md Layer 2 —
/// the Recent-QSOs panel) from a terminal QSO's already-in-scope
/// `QsoMetadata`. Pure and side-effect-free so it can be unit tested without
/// standing up the full coordinator/bus plumbing; both the `QsoCompleted`
/// and `QsoFailed` handlers call this to construct their sibling
/// `MessageType::RecentQsoOutcome` emission.
///
/// `brief_timeline` is intentionally short (start time, exchanged reports,
/// terminal line) — it is NOT the full per-message `state_history` (that
/// timeline isn't available at this call site; persisting it is a separate,
/// larger change).
fn recent_qso_outcome(
    their_call: &str,
    outcome: crate::message_bus::QsoOutcome,
    metadata: &pancetta_qso::QsoMetadata,
) -> crate::message_bus::RecentQsoOutcome {
    use crate::message_bus::QsoOutcome;

    let mut brief_timeline = vec![format!(
        "{} started at {}",
        their_call,
        metadata.start_time.format("%H:%M:%S")
    )];
    if let Some(sent) = metadata.reports.sent {
        brief_timeline.push(format!("Report sent: {sent:+}"));
    }
    if let Some(received) = metadata.reports.received {
        brief_timeline.push(format!("Report received: {received:+}"));
    }
    let (last_state, tail) = match &outcome {
        QsoOutcome::Completed => ("Completed".to_string(), "Completed".to_string()),
        QsoOutcome::Failed(reason) => (
            "Failed".to_string(),
            format!("Failed: {}", failure_reason_text(reason)),
        ),
    };
    brief_timeline.push(tail);

    crate::message_bus::RecentQsoOutcome {
        callsign: their_call.to_string(),
        outcome,
        last_state,
        freq_hz: metadata.frequency as u32,
        ts: chrono::Utc::now(),
        brief_timeline,
    }
}

// =============================================================================
// Task 5 (gap 2 of 4): coordinator priority-ranking wiring for the AP context.
// docs/superpowers/plans/2026-07-25-ap-content-decoding.md / the "Multi-QSO +
// priority ranking" rule in docs/ap-decoding-design.md §1.
// =============================================================================

/// One active QSO's data needed to rank it for the AP context and build its
/// [`pancetta_ft8::QsoAp`]. Kept separate from `QsoAp` itself so
/// [`rank_active_qsos_for_ap`] stays pure and unit-testable without a live
/// `QsoManager`.
#[derive(Debug, Clone, PartialEq)]
struct QsoApCandidate {
    callsign: String,
    grid: Option<String>,
    snr: i8,
    freq_hz: f64,
    progress: pancetta_ft8::QsoApProgress,
    /// WSJT-X Improved-style a8 expected-next-message templates — same
    /// `enumerate_a8_expected_texts` call the old single-QSO write site made
    /// (see `qso_ap_candidate_from_progress`).
    expected_texts: Vec<String>,
}

/// Maps one active QSO's live `state`/`metadata` into a [`QsoApCandidate`],
/// or `None` for states the AP context doesn't represent — `Idle` and
/// `CallingCq` (no confirmed contra-callsign yet), terminal
/// `Completed`/`Failed`, and `Contest`. Mirrors the progress mapping the
/// single-QSO `active_qso_ap` write site used before Task 5.
///
/// Scoring inputs are pulled preferentially from the *live QSO state*
/// (freshest — e.g. `WaitingForReport::our_report` is the report we JUST
/// computed from decoding this station) and fall back to `metadata` (stamped
/// at various transitions, notably QSO completion) when the current state
/// variant doesn't carry the field — e.g. `RespondingToCq` precedes any
/// report being computed.
fn qso_ap_candidate_from_progress(
    state: &pancetta_qso::QsoState,
    metadata: &pancetta_qso::QsoMetadata,
    my_call: &str,
) -> Option<QsoApCandidate> {
    use pancetta_qso::QsoState;

    let (callsign, progress) = match state {
        QsoState::RespondingToCq {
            target_callsign, ..
        }
        | QsoState::WaitingForReport {
            their_callsign: target_callsign,
            ..
        }
        | QsoState::SendingReport {
            their_callsign: target_callsign,
            ..
        } => (
            target_callsign.clone(),
            pancetta_ft8::QsoApProgress::WaitingForReport,
        ),
        QsoState::WaitingForConfirmation { their_callsign, .. }
        | QsoState::SendingConfirmation { their_callsign, .. } => (
            their_callsign.clone(),
            pancetta_ft8::QsoApProgress::WaitingForConfirmation,
        ),
        // Idle / CallingCq / Completed / Failed / Contest: not represented
        // in the AP context.
        _ => return None,
    };

    let freq_hz = state.frequency().unwrap_or(metadata.frequency);

    // `pancetta_qso::states::GridSquare` is a plain `String` alias (not
    // `pancetta_core::gridsquare::GridSquare`) — no conversion needed.
    let grid = match state {
        QsoState::WaitingForReport { their_grid, .. } => their_grid.clone(),
        QsoState::WaitingForConfirmation { grid_square, .. }
        | QsoState::SendingConfirmation { grid_square, .. } => grid_square.clone(),
        _ => None,
    }
    .or_else(|| metadata.grids.theirs.clone());

    let snr = match state {
        QsoState::WaitingForReport { our_report, .. }
        | QsoState::SendingReport { our_report, .. }
        | QsoState::WaitingForConfirmation { our_report, .. }
        | QsoState::SendingConfirmation { our_report, .. } => *our_report,
        _ => metadata.reports.sent.unwrap_or(0),
    };

    let expected_texts =
        pancetta_ft8::ap::enumerate_a8_expected_texts(my_call, &callsign, progress);

    Some(QsoApCandidate {
        callsign,
        grid,
        snr,
        freq_hz,
        progress,
        expected_texts,
    })
}

/// Ranks active-QSO candidates by [`pancetta_qso::PriorityScorer::evaluate_cq`]
/// (highest first) and caps to `max_qsos` — the multi-QSO anti-brute-force
/// rule from docs/ap-decoding-design.md §1 ("Rank concurrent QSOs ... write
/// the top-`MAX_AP_QSOS` QSOs into the context each slot"). Pure and
/// independent of the live coordinator/`QsoManager`, so it's directly
/// unit-testable (see the `ap_ranking_tests` module below).
fn rank_active_qsos_for_ap(
    candidates: &[QsoApCandidate],
    scorer: &pancetta_qso::PriorityScorer,
    max_qsos: usize,
) -> Vec<pancetta_ft8::QsoAp> {
    use pancetta_qso::DxEvaluator;

    let mut scored: Vec<(f64, pancetta_ft8::QsoAp)> = candidates
        .iter()
        .filter_map(|c| {
            let qso = pancetta_ft8::QsoAp::new(&c.callsign, c.progress)?
                .with_expected_texts(c.expected_texts.clone());
            let score = scorer.evaluate_cq(&c.callsign, c.grid.as_deref(), c.snr, c.freq_hz);
            Some((score, qso))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(max_qsos);
    scored.into_iter().map(|(_, qso)| qso).collect()
}

/// Recomputes the ranked, capped `active_qso_ap` state from ALL currently
/// -active QSOs and writes it into `ap_state`. Called after every QSO state
/// transition — including completion/failure, so a QSO going terminal simply
/// drops out of `get_active_qsos()` on its own and the write naturally
/// reflects whichever QSOs remain active (or an empty list if none do), with
/// no separate "clear" branch needed.
async fn refresh_active_qso_ap(
    qso_manager: &pancetta_qso::QsoManager,
    my_call: &str,
    scorer: &pancetta_qso::PriorityScorer,
    ft8_config: &std::sync::Arc<tokio::sync::RwLock<pancetta_ft8::Ft8Config>>,
    ap_state: &std::sync::Arc<std::sync::RwLock<Vec<pancetta_ft8::QsoAp>>>,
) {
    let active = qso_manager.get_active_qsos().await;
    let candidates: Vec<QsoApCandidate> = active
        .iter()
        .filter_map(|(_, progress)| {
            qso_ap_candidate_from_progress(&progress.state, &progress.metadata, my_call)
        })
        .collect();
    let max_ap_qsos = ft8_config.read().await.max_ap_qsos;
    let ranked = rank_active_qsos_for_ap(&candidates, scorer, max_ap_qsos);
    if let Ok(mut guard) = ap_state.write() {
        *guard = ranked;
    }
}

#[cfg(test)]
mod ap_ranking_tests {
    use super::*;
    use std::collections::HashSet;

    /// Minimal `WorkedStationLookup` fixture — mirrors the one in
    /// `pancetta-qso/src/priority.rs`'s own tests and `tui_relay.rs`'s
    /// `priority_score_bucket_boundaries_are_exact` test, giving real
    /// differentiation between an ATNO/needed-DXCC candidate and a plain
    /// worked-before one.
    struct TestLookup {
        needed_dxcc: HashSet<String>,
        atno: HashSet<String>,
        duplicates: HashSet<String>,
    }

    impl pancetta_qso::priority::WorkedStationLookup for TestLookup {
        fn is_duplicate(&self, callsign: &str, _freq_hz: f64) -> bool {
            self.duplicates.contains(&callsign.to_uppercase())
        }
        fn is_recent_failure(&self, _callsign: &str) -> bool {
            false
        }
        fn is_needed_dxcc(&self, callsign: &str) -> bool {
            self.needed_dxcc.contains(&callsign.to_uppercase())
        }
        fn is_needed_grid(&self, _grid: &str) -> bool {
            false
        }
        fn is_dxcc_needed_on_band(&self, callsign: &str, _freq_hz: f64) -> bool {
            self.needed_dxcc.contains(&callsign.to_uppercase())
        }
        fn is_grid_needed_on_band(&self, _grid: &str, _freq_hz: f64) -> bool {
            false
        }
        fn is_atno(&self, callsign: &str) -> bool {
            self.atno.contains(&callsign.to_uppercase())
        }
        fn is_notable(&self, _callsign: &str) -> bool {
            false
        }
        fn rarity(&self, _callsign: &str) -> f64 {
            0.0
        }
        fn network_last_seen(&self, _callsign: &str) -> Option<i64> {
            None
        }
        fn network_snr(&self, _callsign: &str) -> Option<(u32, i32)> {
            None
        }
    }

    fn scorer_with(
        needed_dxcc: &[&str],
        atno: &[&str],
        duplicates: &[&str],
    ) -> pancetta_qso::PriorityScorer {
        let lookup = TestLookup {
            needed_dxcc: needed_dxcc.iter().map(|s| s.to_uppercase()).collect(),
            atno: atno.iter().map(|s| s.to_uppercase()).collect(),
            duplicates: duplicates.iter().map(|s| s.to_uppercase()).collect(),
        };
        pancetta_qso::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(lookup),
        )
    }

    fn candidate(callsign: &str, snr: i8) -> QsoApCandidate {
        QsoApCandidate {
            callsign: callsign.to_string(),
            grid: None,
            snr,
            freq_hz: 14_074_000.0,
            progress: pancetta_ft8::QsoApProgress::WaitingForReport,
            expected_texts: vec![],
        }
    }

    // RED (pre-implementation): this test fails against the OLD single-QSO
    // `Option<QsoAp>` design because there was no ranking function at all —
    // `rank_active_qsos_for_ap` didn't exist. GREEN once implemented above.
    #[test]
    fn ranks_atno_above_worked_before_regardless_of_snr_order() {
        // K5AAA is a plain, already-worked station with a strong signal;
        // JA1ABC is an ATNO (needed + never-worked-anywhere) with a much
        // weaker signal. Priority ranking must still put the ATNO first —
        // a raw-SNR sort would get this backwards.
        let scorer = scorer_with(&["JA1ABC"], &["JA1ABC"], &["K5AAA"]);
        let candidates = vec![candidate("K5AAA", -5), candidate("JA1ABC", -20)];

        let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 4);

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].their_call, "JA1ABC", "ATNO must rank first");
        assert_eq!(ranked[1].their_call, "K5AAA");
    }

    #[test]
    fn caps_to_max_qsos_keeping_the_highest_scored() {
        let scorer = scorer_with(&["JA1ABC"], &["JA1ABC"], &[]);
        let candidates = vec![
            candidate("W1AAA", -10),
            candidate("W1BBB", -10),
            candidate("JA1ABC", -25),
            candidate("W1CCC", -10),
        ];

        let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 2);

        assert_eq!(ranked.len(), 2);
        assert_eq!(
            ranked[0].their_call, "JA1ABC",
            "ATNO still wins despite weak SNR"
        );
    }

    #[test]
    fn empty_candidates_yields_empty_ranking() {
        let scorer = scorer_with(&[], &[], &[]);
        assert!(rank_active_qsos_for_ap(&[], &scorer, 4).is_empty());
    }

    #[test]
    fn each_ranked_qso_carries_its_a8_expected_texts() {
        let scorer = scorer_with(&[], &[], &[]);
        let candidates = vec![candidate("W1AW", -10)];

        let ranked = rank_active_qsos_for_ap(&candidates, &scorer, 4);

        assert_eq!(ranked.len(), 1);
        // candidate() defaults to WaitingForReport with no pre-populated
        // expected_texts in the fixture — this test's contract is really
        // about qso_ap_candidate_from_progress populating them from real
        // state, exercised below.
        assert_eq!(
            ranked[0].progress,
            pancetta_ft8::QsoApProgress::WaitingForReport
        );
    }

    fn waiting_for_report_state(their_callsign: &str) -> pancetta_qso::QsoState {
        pancetta_qso::QsoState::WaitingForReport {
            their_callsign: their_callsign.to_string(),
            frequency: 14_074_000.0,
            started_at: chrono::Utc::now(),
            their_grid: Some("FN31".to_string()),
            our_report: -12,
        }
    }

    fn empty_metadata(
        qso_id: pancetta_qso::QsoId,
        our_callsign: &str,
    ) -> pancetta_qso::QsoMetadata {
        let now = chrono::Utc::now();
        pancetta_qso::QsoMetadata {
            qso_id,
            our_callsign: our_callsign.to_string(),
            their_callsign: None,
            frequency: 14_074_000.0,
            mode: "FT8".to_string(),
            start_time: now,
            end_time: None,
            reports: pancetta_qso::states::SignalReports::default(),
            grids: pancetta_qso::states::GridSquares::default(),
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            tx_parity_provisional: false,
            initiated_by: pancetta_qso::states::CallInitiation::Auto,
            role: pancetta_qso::states::QsoRole::Caller,
            call_count: 1,
            first_call_at: Some(now),
            last_call_at: Some(now),
            progressed_this_cycle: false,
            last_rx_text: None,
            dx_repeat_count: 0,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
        }
    }

    #[test]
    fn qso_ap_candidate_from_progress_extracts_grid_snr_freq_from_live_state() {
        let state = waiting_for_report_state("W1AW");
        let metadata = empty_metadata(pancetta_qso::QsoId::new_v4(), "K5ARH");

        let candidate = qso_ap_candidate_from_progress(&state, &metadata, "K5ARH")
            .expect("WaitingForReport must produce a candidate");

        assert_eq!(candidate.callsign, "W1AW");
        assert_eq!(candidate.grid.as_deref(), Some("FN31"));
        assert_eq!(candidate.snr, -12);
        assert_eq!(candidate.freq_hz, 14_074_000.0);
        assert_eq!(
            candidate.progress,
            pancetta_ft8::QsoApProgress::WaitingForReport
        );
        assert!(
            !candidate.expected_texts.is_empty(),
            "a8 expected-texts must be populated, same as the pre-Task-5 single-QSO write site"
        );
    }

    #[test]
    fn qso_ap_candidate_from_progress_returns_none_for_calling_cq_and_idle() {
        let calling_cq = pancetta_qso::QsoState::CallingCq {
            frequency: 14_074_000.0,
            started_at: chrono::Utc::now(),
            call_count: 1,
        };
        let metadata = empty_metadata(pancetta_qso::QsoId::new_v4(), "K5ARH");
        assert!(qso_ap_candidate_from_progress(&calling_cq, &metadata, "K5ARH").is_none());
        assert!(
            qso_ap_candidate_from_progress(&pancetta_qso::QsoState::Idle, &metadata, "K5ARH")
                .is_none()
        );
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

        let (_qso_tx, qso_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::Qso)
            .await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();

        // Read station config for callsign/grid
        let config = self.config.read().await;
        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };
        // Stamped into every rendered ADIF record's TX_PWR field (source-of-truth
        // qsos.adi and every per-QSO logbook upload). 0 = unconfigured, omit TX_PWR.
        let station_power_watts = config.station.power_watts;
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
        // Operator-configured duplicate-QSO checking. Copied field-by-field
        // into pancetta_qso::DuplicateCheckConfig (same pattern as HoundRegions
        // above) to avoid a pancetta-qso → pancetta-config dependency. The
        // config-side defaults equal the qso-side defaults (guard test:
        // config_duplicate_defaults_match_qso_manager_defaults), so a config
        // without the section behaves exactly like the pre-wiring binary.
        let dup_cfg = config.duplicate_checking.clone();
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
        // Layer 2 timeline persistence gate — see docs/CONFIG.md `[database]`.
        let persist_qso_timeline = config.database.persist_qso_timeline;
        // Task 5 (gap 2/4, docs/ap-decoding-design.md §1): priority weights
        // for ranking ALL currently-active QSOs by `PriorityScorer::
        // evaluate_cq` for the AP context — read once here before `drop`,
        // same field set the autonomous operator's own scorer uses
        // (`autonomous.rs`'s `priority_weights`).
        let ap_priority_weights = {
            let p = &config.autonomous.priorities;
            pancetta_qso::priority::PriorityWeights {
                needed_dxcc: p.needed_dxcc,
                needed_grid: p.needed_grid,
                pota_sota: p.pota_sota,
                rarity: p.rarity,
                signal_strength: p.signal_strength,
                duplicate_penalty: p.duplicate_penalty,
                recent_failure_penalty: p.recent_failure_penalty,
                atno_bonus: p.atno_bonus,
            }
        };
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
        // The one predicate every "does this run touch the outside world — or
        // the operator's real log?" gate consults. Read once here; both the
        // upload subscriber and the LOCAL ADIF/DB writers below take it.
        let replay_mode = self.replay_mode();
        let upload_enabled = logbook_upload_enabled(
            clublog_cfg.enabled,
            qrz_cfg.enabled,
            lotw_cfg.enabled,
            eqsl_cfg.enabled,
            cqdx_upload_enabled,
            qrz_xml_enabled,
            replay_mode,
        );

        let qso_lookup = self.cached_lookup.clone();
        let upload_our_callsign = our_callsign.clone();
        let active_qso_ap = self.active_qso_ap.clone();
        // Task 5: `max_ap_qsos` is hot-reloadable, so the AP-context write
        // site below re-reads it fresh from this shared handle on every
        // QSO event rather than snapshotting it once at startup.
        let ft8_config_for_ap = self.ft8_config.clone();
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
        // FQ-F3: active QSO id -> TX offset (Hz), mirroring `active_tx_qsos`'s
        // exact insert/remove points (including the 45s completed-grace
        // window) so the autonomous frequency allocator's own-frequency
        // registry can be kept in sync each tick.
        let active_tx_offsets = self.active_tx_offsets.clone();
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

        // `QsoManager::new` and its three `set_*_source` setters are plain
        // synchronous, non-blocking calls — nothing about them needs the
        // spawned task's async context. Constructing the manager here (instead
        // of inside `tokio::spawn` below) lets us call `.subscribe()`
        // synchronously and populate `self.wsjtx_qso_events_rx` before this
        // method returns. `run()` `.await`s `start_qso_component` before
        // `start_wsjtx_udp_component`, so the field is always already
        // populated by the time the latter reads it — no channel, handoff, or
        // timeout needed.
        let qso_config = {
            use pancetta_qso::{DuplicateCheckConfig, HoundRegions, QsoManagerConfig};

            QsoManagerConfig {
                our_callsign: our_callsign.clone(),
                our_grid: our_grid.clone(),
                hound: HoundRegions {
                    call_min_hz: hound_cfg.call_min_hz,
                    call_max_hz: hound_cfg.call_max_hz,
                    response_min_hz: hound_cfg.response_min_hz,
                    response_max_hz: hound_cfg.response_max_hz,
                },
                active_mode: active_mode.clone(),
                duplicate_checking: DuplicateCheckConfig {
                    enabled: dup_cfg.enabled,
                    time_window_hours: dup_cfg.time_window_hours,
                    check_frequency: dup_cfg.check_frequency,
                    // check_band is defined but unread in pancetta-qso;
                    // keep the qso-side default rather than exposing a
                    // dead knob in the config schema.
                    ..Default::default()
                },
                ..Default::default()
            }
        };

        let mut qso_manager = pancetta_qso::QsoManager::new(qso_config);
        // Task 6 (task-supervision): store a cheap Arc-based handle clone so
        // the task supervisor (health.rs) can still enumerate and fail
        // in-flight QSOs after this component's task panics and dies. See
        // the field's doc-comment in mod.rs for why the clone stays valid.
        self.qso_manager_for_supervisor = Some(qso_manager.clone());
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

        // Task 5 (QSOLogged/LoggedADIF): subscribe synchronously, before
        // `qso_manager` is moved into the spawned task below, so the WSJT-X
        // UDP component can pick this receiver straight off the coordinator
        // field with no channel or timeout involved.
        self.wsjtx_qso_events_rx = Some(qso_manager.subscribe());

        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }

                // Rebuildable SQLite index at ~/.pancetta/qso.db.
                let db_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qso.db");

                // ADIF source of truth at ~/.pancetta/qsos.adi.
                let adif_path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".pancetta")
                    .join("qsos.adi");

                // Both LOCAL source-of-truth writers (ADIF appender + SQLite
                // index) live behind one helper so the `--replay` suppression
                // is a single gate, testable without a whole coordinator.
                let (_adif_writer, _async_logger) = start_local_qso_log_writers(
                    replay_mode,
                    &adif_path,
                    &db_path,
                    station_power_watts,
                    persist_qso_timeline,
                    &qso_manager,
                    shutdown.clone(),
                )
                .await;

                // Per-QSO log-upload subscriber (ClubLog + QRZ Logbook + cqdx.io
                // + eQSL + LoTW), with optional QRZ-XML grid enrichment applied
                // first. Opt-in: only spawned when at least one upload target OR
                // QRZ XML enrichment is enabled, and never under `--replay`
                // (see `upload_enabled`). Best-effort and fully decoupled
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
                        station_power_watts,
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

                        // DX Hunter per-band-needed (2026-07-18): unlike the
                        // duplicate-filter seed above (current band only),
                        // this pulls every band in one query so DX Hunter
                        // rows on OTHER bands can be evaluated too.
                        let band_callsign_pairs = db.get_worked_bands_and_callsigns().await;
                        if !band_callsign_pairs.is_empty() {
                            qso_lookup.seed_worked_dxcc_from_list(band_callsign_pairs);
                        }

                        let band_grid_pairs = db.get_worked_bands_and_grids().await;
                        qso_lookup.seed_worked_grids_from_list(band_grid_pairs);
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
                let active_tx_offsets = active_tx_offsets.clone();
                let latest_tx_intent = latest_tx_intent.clone();
                let snapshot_qso_manager = qso_manager.clone();
                let snapshot_bus = tx_bus.clone();
                let completions_for_events = recent_manual_completions.clone();
                let pending_for_events = pending_manual_calls.clone();
                let dx_activity_for_events = dx_activity.clone();
                let display_feed_enabled = display_feed_enabled.clone();
                tokio::spawn(async move {
                    // Task 5 (gap 2/4): built once for this task's lifetime,
                    // mirroring the autonomous operator's own scorer
                    // (`autonomous.rs`) — same weights source, and a fresh
                    // clone of the SAME `Arc`-shared `CachedStationLookup`
                    // (`qso_lookup`) so this always sees the latest
                    // needed/rarity/worked data.
                    let ap_priority_scorer = pancetta_qso::PriorityScorer::new(
                        ap_priority_weights,
                        Box::new((*qso_lookup).clone()),
                    );
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
                                            set.insert(key.clone());
                                        }
                                        // FQ-F3: keep the own-frequency
                                        // registry's source map in sync
                                        // alongside `active_tx_qsos`.
                                        if let Some(freq) = new_state.frequency() {
                                            if let Ok(mut offsets) = active_tx_offsets.write() {
                                                offsets.insert(key, freq);
                                            }
                                        }
                                    } else if let pancetta_qso::QsoState::Failed {
                                        reason, ..
                                    } = &new_state
                                    {
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.remove(&key);
                                        }
                                        if let Ok(mut m) = latest_tx_intent.write() {
                                            m.remove(&key);
                                        }
                                        if let Ok(mut offsets) = active_tx_offsets.write() {
                                            offsets.remove(&key);
                                        }
                                        info!(
                                            target: "tx.policy",
                                            "QSO {} went terminal-Failed — purging its TX from the active set",
                                            qso_id
                                        );
                                        // #40: a failed QSO frees its window
                                        // immediately (no trailing TX) — promote
                                        // any deferred cross-parity manual call.
                                        if drop_should_promote(reason) {
                                            promote_pending_manual_calls(
                                                &snapshot_qso_manager,
                                                &pending_for_events,
                                                &snapshot_bus,
                                            )
                                            .await;
                                        }
                                    }
                                }

                                // Map QSO state to AP context for AP3/AP4 decoding.
                                //
                                // Task 5 (gap 2/4, docs/ap-decoding-design.md
                                // §1): rank ALL currently-active QSOs by
                                // `PriorityScorer::evaluate_cq` and write the
                                // top-`max_ap_qsos` into the shared AP state,
                                // highest priority first. Replaces the old
                                // single-QSO (whichever just changed state)
                                // write — see `refresh_active_qso_ap` for the
                                // WSJT-X Improved-style a8 wiring this
                                // subsumed (each candidate still gets its
                                // `enumerate_a8_expected_texts` templates).
                                refresh_active_qso_ap(
                                    &snapshot_qso_manager,
                                    &tx_callsign,
                                    &ap_priority_scorer,
                                    &ft8_config_for_ap,
                                    &ap_state,
                                )
                                .await;

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
                                let gw_snap = if display_feed_enabled.load(Ordering::Relaxed) {
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
                                        &display_feed_enabled,
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
                                remote_origin,
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
                                                // SECURITY: a remote-initiated QSO's TX MUST be
                                                // `TxOrigin::Remote` so the armed-TX gate applies;
                                                // a local QSO stays `Local` (byte-identical).
                                                origin: if remote_origin {
                                                    crate::message_bus::TxOrigin::Remote
                                                } else {
                                                    crate::message_bus::TxOrigin::Local
                                                },
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
                                // Shares its value with
                                // `pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE`
                                // (the grace window `respond_to_caller`'s FIX-B
                                // idempotent-close dispatch uses to find a
                                // just-completed manual QSO to re-key instead of
                                // spawning a duplicate) so the two windows can
                                // never drift apart. A positive 45s chrono
                                // literal never overflows `to_std`.
                                let completed_tx_grace: Duration =
                                    pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE
                                        .to_std()
                                        .expect(
                                            "COMPLETED_QSO_REWORK_GRACE is a positive literal duration",
                                        );
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
                                    // FQ-F3: same idempotent insert for the
                                    // offset registry, mirroring the
                                    // active_tx_qsos insert immediately above.
                                    if let Ok(mut offsets) = active_tx_offsets.write() {
                                        offsets.insert(key.clone(), metadata.frequency);
                                    }
                                    let set = active_tx_qsos.clone();
                                    let intent_map = latest_tx_intent.clone();
                                    let offsets_map = active_tx_offsets.clone();
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
                                        tokio::time::sleep(completed_tx_grace).await;
                                        if let Ok(mut s) = set.write() {
                                            s.remove(&key);
                                        }
                                        if let Ok(mut m) = intent_map.write() {
                                            m.remove(&key);
                                        }
                                        if let Ok(mut offsets) = offsets_map.write() {
                                            offsets.remove(&key);
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
                                // Task 5: recompute the ranked active-QSO AP
                                // state now this QSO has gone terminal — it
                                // naturally drops out of `get_active_qsos()`,
                                // so other still-active QSOs' AP context is
                                // preserved (the old code unconditionally
                                // cleared to `None`/empty here, which would
                                // wipe a co-active QSO's AP context too).
                                refresh_active_qso_ap(
                                    &snapshot_qso_manager,
                                    &tx_callsign,
                                    &ap_priority_scorer,
                                    &ft8_config_for_ap,
                                    &ap_state,
                                )
                                .await;
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
                                let gw_snap = if display_feed_enabled.load(Ordering::Relaxed) {
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
                                        &display_feed_enabled,
                                        ComponentId::Qso,
                                        m,
                                    )
                                    .await;
                                }
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO completed with {}, marking as worked", their_call);

                                    let history_band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    let history_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::QsoHistoryEntry {
                                            call_sign: their_call.clone(),
                                            band: history_band,
                                            success: true,
                                            reason: None,
                                            completed_at: chrono::Utc::now(),
                                        },
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(history_msg).await;

                                    // Batch 2 #4: completed QSOs are filtered out
                                    // of the active snapshot, so the operator never
                                    // saw success. Surface a one-line confirmation
                                    // with the reports exchanged (RST sent/received).
                                    let rst = |r: Option<i8>| {
                                        r.map(|v| format!("{v:+}"))
                                            .unwrap_or_else(|| "--".to_string())
                                    };
                                    let completed_text = format!(
                                        "QSO with {} logged (RST {}/{})",
                                        their_call,
                                        rst(metadata.reports.sent),
                                        rst(metadata.reports.received),
                                    );
                                    emit_status(&snapshot_bus, completed_text.clone()).await;
                                    // observability-diagnostics-plan.md Layer 2:
                                    // also retain this outcome in the diagnostic
                                    // history (the status line above is overwritten
                                    // by the next status update within a frame).
                                    let diag_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::DiagnosticEvent {
                                            target: "qso",
                                            level: pancetta_core::DiagnosticLevel::Info,
                                            text: completed_text,
                                            qso_id: Some(qso_id.to_string()),
                                            callsign: Some(their_call.clone()),
                                        },
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(diag_msg).await;

                                    // observability-diagnostics-plan.md Layer 2
                                    // (2026-07-25 plan, Task 1): sibling
                                    // structured Recent-QSOs outcome, additive
                                    // to the DiagnosticEvent above.
                                    let recent_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::RecentQsoOutcome(recent_qso_outcome(
                                            their_call,
                                            crate::message_bus::QsoOutcome::Completed,
                                            &metadata,
                                        )),
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(recent_msg).await;

                                    let band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    qso_lookup.record_worked(their_call, &band);
                                    if let Some(grid) = metadata.grids.theirs.as_deref() {
                                        qso_lookup.record_worked_grid(grid, &band);
                                    }

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
                                            remote_origin: metadata.remote_origin,
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
                                qso_id,
                                reason,
                                last_state,
                                metadata,
                                ..
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
                                    if let Ok(mut offsets) = active_tx_offsets.write() {
                                        offsets.remove(&key);
                                    }
                                }
                                // #40: window freed — promote a deferred call.
                                if drop_should_promote(&reason) {
                                    promote_pending_manual_calls(
                                        &snapshot_qso_manager,
                                        &pending_for_events,
                                        &snapshot_bus,
                                    )
                                    .await;
                                }
                                // Task 5: recompute (see the QsoCompleted arm
                                // above for why this replaced an
                                // unconditional clear-to-empty).
                                refresh_active_qso_ap(
                                    &snapshot_qso_manager,
                                    &tx_callsign,
                                    &ap_priority_scorer,
                                    &ft8_config_for_ap,
                                    &ap_state,
                                )
                                .await;
                                // Push fresh snapshot so the banner drops
                                // the failed QSO.
                                let (snapshot, pending_snap) = build_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                )
                                .await;
                                let gw_snap = if display_feed_enabled.load(Ordering::Relaxed) {
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
                                        &display_feed_enabled,
                                        ComponentId::Qso,
                                        m,
                                    )
                                    .await;
                                }
                                // observability-diagnostics-plan.md Layer 2: surface
                                // WHY the QSO failed instead of letting it silently
                                // vanish from the banner — `reason` was previously
                                // destructured away (`..`) and never read anywhere.
                                if let Some(ref their_call) = metadata.their_callsign {
                                    let history_band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    let history_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::QsoHistoryEntry {
                                            call_sign: their_call.clone(),
                                            band: history_band,
                                            success: false,
                                            reason: Some(failure_reason_text(&reason)),
                                            completed_at: chrono::Utc::now(),
                                        },
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(history_msg).await;
                                }
                                let reason_text =
                                    if reason == pancetta_qso::QsoFailureReason::Timeout {
                                        let timeouts = &snapshot_qso_manager.config().timeouts;
                                        timeout_detail(
                                            Some(&last_state),
                                            &metadata,
                                            timeouts.manual_call_max_calls,
                                            timeouts.manual_call_watchdog_minutes,
                                        )
                                    } else {
                                        failure_reason_text(&reason)
                                    };
                                let diag_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::DiagnosticEvent {
                                        target: "qso",
                                        level: pancetta_core::DiagnosticLevel::Warn,
                                        text: format!("QSO failed: {reason_text}"),
                                        qso_id: Some(qso_id.to_string()),
                                        callsign: metadata.their_callsign.clone(),
                                    },
                                    Instant::now(),
                                );
                                let _ = snapshot_bus.send_message(diag_msg).await;

                                // observability-diagnostics-plan.md Layer 2
                                // (2026-07-25 plan, Task 1): sibling
                                // structured Recent-QSOs outcome, additive
                                // to the DiagnosticEvent above. Gated on a
                                // known callsign, same as the QsoHistoryEntry
                                // push above.
                                if let Some(ref their_call) = metadata.their_callsign {
                                    let recent_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::RecentQsoOutcome(recent_qso_outcome(
                                            their_call,
                                            crate::message_bus::QsoOutcome::Failed(reason.clone()),
                                            &metadata,
                                        )),
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(recent_msg).await;
                                }

                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!(
                                        "QSO failed with {}: {} (adding backoff)",
                                        their_call, reason_text
                                    );
                                    qso_lookup.record_failure(their_call);
                                } else {
                                    info!("QSO failed: {}", reason_text);
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::MessageRejected {
                                qso_id,
                                reason,
                                from_callsign,
                                to_callsign,
                            }) => {
                                let who = from_callsign.as_deref().unwrap_or("?");
                                let whom = to_callsign.as_deref().unwrap_or("us");
                                crate::coordinator::tx::emit_diagnostic_full(
                                    &snapshot_bus,
                                    ComponentId::Qso,
                                    "qso.security",
                                    pancetta_core::DiagnosticLevel::Warn,
                                    format!(
                                        "Rejected frame from {who} to {whom} — {}",
                                        rejection_reason_text(&reason)
                                    ),
                                    Some(&qso_id.to_string()),
                                    from_callsign.as_deref(),
                                )
                                .await;
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
                                            // Use the parity-carrying entry point: this
                                            // is a live decode, so `slot_parity` is a
                                            // real observation, letting a
                                            // provisionally-latched QSO (e.g. answered
                                            // from a DX-cluster/DX-Hunter spot before any
                                            // live decode existed) refine its `tx_parity`
                                            // to the true opposite-of-DX value on first
                                            // contact — see
                                            // `QsoMetadata::tx_parity_provisional`.
                                            if let Err(e) = qso_manager
                                                .process_message_with_parity(
                                                    msg_type.clone(),
                                                    raw_text.clone(),
                                                    frequency,
                                                    Some(snr),
                                                    decoded_msg.slot_parity,
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
                                            remote_origin,
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
                                                crate::coordinator::tx::emit_diagnostic_full(
                                                    &message_bus,
                                                    ComponentId::Qso,
                                                    "qso.security",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    format!("Refusing StartQso for our own callsign {callsign}"),
                                                    None,
                                                    Some(&callsign),
                                                )
                                                .await;
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
                                                        remote_origin,
                                                        step: pancetta_core::ResponseStep::Grid,
                                                        our_snr_of_them: None,
                                                        their_report: None,
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
                                                    remote_origin,
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
                                                emit_skip_diagnostic(
                                                    &message_bus,
                                                    SkipSite::CrossParityDeferral {
                                                        callsign: callsign.clone(),
                                                        active_side: current_side,
                                                        wanted: desired_tx_parity,
                                                    },
                                                )
                                                .await;
                                                continue;
                                            }
                                            let result = match &callsign {
                                                Some(dx) => {
                                                    // FQ-F4/TX-F6: de-conflict the
                                                    // raw Tx=Rx candidate (the DX's
                                                    // own decoded frequency) against
                                                    // our OTHER active streams before
                                                    // latching it, mirroring the
                                                    // manual-call path's
                                                    // `compute_manual_tx_offset`
                                                    // pattern. Byte-identical no-op
                                                    // when nothing collides.
                                                    let active =
                                                        qso_manager.active_tx_offsets().await;
                                                    let (tx_off, partner) =
                                                        compute_manual_tx_offset(
                                                            frequency, false, 0, &active,
                                                        );
                                                    if tx_off != frequency {
                                                        info!(
                                                            target: "qso.autonomous",
                                                            "Autonomous pounce TX offset \
                                                             de-conflicted: dx_freq={:.0} \
                                                             → tx_off={:.0} Hz ({} active)",
                                                            frequency, tx_off, active.len()
                                                        );
                                                    }
                                                    qso_manager
                                                        .respond_to_cq_with(
                                                            dx.clone(),
                                                            tx_off,
                                                            parity,
                                                            pancetta_qso::CallInitiation::Auto,
                                                            partner,
                                                            false,
                                                        )
                                                        .await
                                                }
                                                None => {
                                                    // Calling CQ ourselves: `parity` is our TX
                                                    // parity (not a DX parity). Autonomous CQ is
                                                    // a LOCAL initiation, never remote.
                                                    qso_manager
                                                        .start_cq(frequency, parity, false)
                                                        .await
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
                                                    emit_skip_diagnostic(
                                                        &message_bus,
                                                        SkipSite::AutonomousOpenFailed {
                                                            callsign: callsign.clone(),
                                                            error: e.to_string(),
                                                        },
                                                    )
                                                    .await;
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
                                                crate::coordinator::tx::emit_diagnostic_full(
                                                    &message_bus,
                                                    ComponentId::Qso,
                                                    "qso.security",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    format!("Refusing EngageHound for our own callsign {callsign}"),
                                                    None,
                                                    Some(&callsign),
                                                )
                                                .await;
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
                                                        // Hound (Shift+H) is a local operator action.
                                                        remote_origin: false,
                                                        // Hound entries promote via `engage_hound`,
                                                        // not `respond_to_caller` — these fields are
                                                        // unused when `hound == true`.
                                                        step: pancetta_core::ResponseStep::Grid,
                                                        our_snr_of_them: None,
                                                        their_report: None,
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
                                            remote_origin,
                                        } => {
                                            // PAN-23 round-2 (Codex review of #283): reject the
                                            // unresolved-hash placeholder "<...>" at ADMISSION
                                            // time, before it can ever be queued. The guard in
                                            // `qso_manager::respond_to_caller`/
                                            // `respond_to_cq_with` only fires when a call is
                                            // actually PROCESSED — but when both TX parities are
                                            // occupied by active QSOs this arm queues the request
                                            // first (below) and defers processing to
                                            // `promote_pending_manual_calls`. Since the
                                            // InvalidCallsign rejection is deterministic (it can
                                            // never succeed, no matter how many times it's
                                            // retried), letting it reach that generic
                                            // queue-on-failure path would re-queue it at the
                                            // front on every promotion attempt, repeatedly
                                            // holding the parity slot against legitimate
                                            // opposite-parity calls for up to the full 10-minute
                                            // queue TTL. Reject it here instead, before it ever
                                            // occupies a slot.
                                            if callsign == "<...>" {
                                                warn!(
                                                    target: "qso.security",
                                                    "Refusing RespondToCaller for the unresolved \
                                                     hash placeholder \"<...>\"",
                                                );
                                                crate::coordinator::tx::emit_diagnostic_full(
                                                    &message_bus,
                                                    ComponentId::Qso,
                                                    "qso.security",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    "Refusing RespondToCaller for the unresolved \
                                                     hash placeholder \"<...>\" — it carries no \
                                                     identity information and can never be \
                                                     transmitted"
                                                        .to_string(),
                                                    None,
                                                    Some(&callsign),
                                                )
                                                .await;
                                                continue;
                                            }
                                            info!(
                                                "Responding to caller {} on {} Hz at step {:?} \
                                                 (manual)",
                                                callsign, frequency, step
                                            );
                                            // #40 half-duplex parity gate (same admission check
                                            // as StartQso, applied here for consistency — a
                                            // RespondToCaller that would TX in the *opposite*
                                            // window from the one our active QSOs hold is
                                            // DEFERRED, not started immediately, to keep the
                                            // opposite window free to hear responses (no
                                            // sequential-window TX). Promoted automatically once
                                            // the current side's QSOs finish
                                            // (promote_pending_manual_calls), replayed via
                                            // `respond_to_caller` at its own `step`.
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
                                                        remote_origin,
                                                        step,
                                                        our_snr_of_them: snr,
                                                        // The immediate path below always passes
                                                        // `None` (the engine defaults it); match
                                                        // that here for the deferred replay too.
                                                        their_report: None,
                                                    });
                                                }
                                                let queue_depth = q.len();
                                                drop(q);
                                                info!(
                                                    target: "qso",
                                                    "Queued caller-response to {} ({:?}) — \
                                                     opposite window (active side {:?}); queue \
                                                     now {} pending",
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
                                                    remote_origin,
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
                                            remote_origin,
                                        } => {
                                            match qso_manager
                                                .start_cq_manual(
                                                    frequency as f64,
                                                    tx_parity,
                                                    remote_origin,
                                                )
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
                                                    .start_cq_manual(FOX_CQ_OFFSET_HZ, None, false)
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
                                        crate::message_bus::QsoMessage::SetOperatingMode {
                                            mode,
                                        } => {
                                            qso_manager.set_active_mode(mode.clone());
                                            info!("QSO manager active mode set to {}", mode);
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
    use super::{compute_manual_tx_offset, partition_pending_calls, PendingManualCall};
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
            remote_origin: false,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
        }
    }

    fn names(v: &[PendingManualCall]) -> Vec<String> {
        v.iter().map(|p| p.callsign.clone()).collect()
    }

    #[test]
    fn dx_above_ceiling_clamps_tx_and_sets_partner_freq() {
        let (tx_off, partner) = compute_manual_tx_offset(2931.0, false, 0, &[]);
        assert_eq!(tx_off, pancetta_qso::TX_OFFSET_MAX_HZ);
        assert_eq!(partner, Some(2931.0));
    }

    #[test]
    fn dx_below_floor_clamps_tx_and_sets_partner_freq() {
        let (tx_off, partner) = compute_manual_tx_offset(180.0, false, 0, &[]);
        assert_eq!(tx_off, pancetta_qso::TX_OFFSET_MIN_HZ);
        assert_eq!(partner, Some(180.0));
    }

    // Build a RespondToCaller-shaped pending call (non-Grid step) whose DX is
    // on `dx`, so its desired TX parity is the opposite. Used to verify a
    // `RespondToCaller` entry queued by the admission gate participates in
    // exactly the SAME cross-parity partition logic as a `StartQso` entry
    // (`call` above), not a separate/duplicated path.
    fn caller_call(
        name: &str,
        dx: SlotParity,
        step: pancetta_core::ResponseStep,
    ) -> PendingManualCall {
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
            remote_origin: false,
            step,
            our_snr_of_them: Some(-8.0),
            their_report: None,
        }
    }

    /// A `RespondToCaller`-shaped entry (step = Report, i.e. NOT the
    /// `StartQso`-equivalent `Grid` step) that wants the opposite parity from
    /// our committed side stays queued — it is gated by the SAME half-duplex
    /// admission logic as a `StartQso` entry, not bypassed.
    #[test]
    fn respond_to_caller_shaped_entry_stays_queued_on_opposite_side() {
        let q: VecDeque<_> = [caller_call(
            "DX1",
            SlotParity::Even, // wants Odd
            pancetta_core::ResponseStep::Report,
        )]
        .into();
        // Committed to Even already: the Odd-wanting caller-response stays queued.
        let (start, keep) = partition_pending_calls(q, Some(SlotParity::Even));
        assert!(
            start.is_empty(),
            "opposite-parity caller-response must not start"
        );
        assert_eq!(names(&keep.into_iter().collect::<Vec<_>>()), vec!["DX1"]);
    }

    /// The same `RespondToCaller`-shaped entry starts once the committed side
    /// flips to match it (mirrors what promotion after a QSO going terminal
    /// looks like from the partition step's point of view).
    #[test]
    fn respond_to_caller_shaped_entry_starts_when_side_matches() {
        let q: VecDeque<_> = [caller_call(
            "DX1",
            SlotParity::Even, // wants Odd
            pancetta_core::ResponseStep::Report,
        )]
        .into();
        let (start, keep) = partition_pending_calls(q, Some(SlotParity::Odd));
        assert_eq!(names(&start), vec!["DX1"]);
        assert!(keep.is_empty());
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
            remote_origin: false,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
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
            remote_origin: false,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
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
            remote_origin: false,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
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
            remote_origin: false,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
        };
        let active: Vec<f64> = vec![];
        let (tx_off, partner) =
            compute_manual_tx_offset(p.frequency_hz, p.hold_mode, p.held_hz, &active);
        assert_eq!(tx_off, 1750.0, "Auto + no collision → Tx=Rx");
        assert_eq!(partner, None, "Tx=Rx → partner_freq is None");
    }

    // ── RespondToCaller admission-gate promotion (Batch 2, part 3) ───────────
    //
    // These exercise the REAL `promote_pending_manual_calls` (not just the
    // pure partition step) against a REAL `QsoManager`, proving a
    // `RespondToCaller`-shaped queued entry replays via `respond_to_caller`
    // at its OWN step on promotion — not the old hardcoded
    // `respond_to_cq_with` (which would always reopen at `RespondingToCq`
    // regardless of what step the operator actually chose).

    use super::{promote_pending_manual_calls, PendingManualCalls};
    use crate::message_bus::MessageBus;
    use pancetta_qso::{QsoManager, QsoManagerConfig, QsoState};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    async fn test_manager(our_callsign: &str) -> QsoManager {
        let m = QsoManager::new(QsoManagerConfig {
            our_callsign: our_callsign.to_string(),
            our_grid: Some("EM10".to_string()),
            ..Default::default()
        });
        m.start().await.expect("manager start");
        m
    }

    /// A `RespondToCaller` (step = Report) queued because it wanted the
    /// opposite parity from our (idle) committed side promotes — via
    /// `promote_pending_manual_calls` — into a REAL QSO opened at
    /// `SendingReport` (the Report step's initial state), not `RespondingToCq`
    /// (the Grid/StartQso shape). This is the admission-gate + promotion
    /// round-trip for Part 3: queued-not-admitted-immediately, then promoted
    /// once a window is free.
    #[tokio::test]
    async fn respond_to_caller_shaped_queued_call_promotes_at_its_own_step() {
        let mgr = test_manager("K5ARH").await;
        let bus = MessageBus::new(1000).expect("bus");
        let pending: PendingManualCalls = Arc::new(TokioMutex::new(VecDeque::from([caller_call(
            "JA1ABC",
            SlotParity::Even,
            pancetta_core::ResponseStep::Report,
        )])));

        // Idle (no active QSOs) — `promote_pending_manual_calls` will adopt
        // the queue's own desired parity (Odd, opposite of Even) as the side
        // to commit to, and start it.
        promote_pending_manual_calls(&mgr, &pending, &bus).await;

        assert!(
            pending.lock().await.is_empty(),
            "the queued caller-response must be promoted (queue drained), not left waiting"
        );

        let active = mgr.get_active_qsos().await;
        assert_eq!(active.len(), 1, "exactly one QSO should have started");
        let (_, progress) = &active[0];
        assert_eq!(progress.metadata.their_callsign.as_deref(), Some("JA1ABC"));
        assert!(
            matches!(progress.state, QsoState::SendingReport { .. }),
            "step=Report must open at SendingReport (not RespondingToCq/Grid), got {:?}",
            progress.state
        );
        assert_eq!(
            progress.metadata.tx_parity,
            Some(SlotParity::Odd),
            "tx_parity must be opposite of the caller's Even slot"
        );
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
                pending_freq_drift: None,
                hound_qsyed: false,
                remote_origin: false,
                tx_parity_provisional: false,
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

    #[test]
    fn rejection_reason_text_covers_every_variant_and_fits_overlay() {
        use pancetta_qso::RejectionReason as R;
        for reason in [
            R::SenderNotPartner,
            R::AddresseeNotUs,
            R::SenderAndAddresseeMismatch,
            R::UnsafeCompoundUpgrade,
        ] {
            assert!(super::rejection_reason_text(&reason).len() <= 40);
        }
        assert_eq!(
            super::rejection_reason_text(&R::SenderNotPartner),
            "sender is not our QSO partner"
        );
    }

    /// observability-diagnostics-plan.md Layer 2 (Task 1): the `Completed`
    /// path of `recent_qso_outcome` — correct callsign, outcome, frequency,
    /// and a brief timeline that reflects the exchanged reports.
    #[test]
    fn recent_qso_outcome_completed_carries_callsign_and_reports() {
        let progress = fixture_progress();
        let metadata = progress.metadata;
        let their_call = metadata.their_callsign.clone().expect("fixture has a call");

        let outcome = super::recent_qso_outcome(
            &their_call,
            crate::message_bus::QsoOutcome::Completed,
            &metadata,
        );

        assert_eq!(outcome.callsign, "JA1ABC");
        assert!(matches!(
            outcome.outcome,
            crate::message_bus::QsoOutcome::Completed
        ));
        assert_eq!(outcome.last_state, "Completed");
        assert_eq!(outcome.freq_hz, 1500);
        assert!(
            outcome
                .brief_timeline
                .iter()
                .any(|l| l.contains("Report sent: -8")),
            "brief_timeline should mention the sent report: {:?}",
            outcome.brief_timeline
        );
        assert!(
            outcome
                .brief_timeline
                .iter()
                .any(|l| l.contains("Report received: -12")),
            "brief_timeline should mention the received report: {:?}",
            outcome.brief_timeline
        );
        assert_eq!(
            outcome.brief_timeline.last().map(String::as_str),
            Some("Completed")
        );
    }

    /// observability-diagnostics-plan.md Layer 2 (Task 1): the `Failed`
    /// path of `recent_qso_outcome` — correct callsign + reason, and the
    /// reason surfaces in the brief timeline's final line.
    #[test]
    fn recent_qso_outcome_failed_timeout_carries_reason() {
        let progress = fixture_progress();
        let metadata = progress.metadata;
        let their_call = metadata.their_callsign.clone().expect("fixture has a call");

        let outcome = super::recent_qso_outcome(
            &their_call,
            crate::message_bus::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout),
            &metadata,
        );

        assert_eq!(outcome.callsign, "JA1ABC");
        assert!(matches!(
            outcome.outcome,
            crate::message_bus::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout)
        ));
        assert_eq!(outcome.last_state, "Failed");
        assert_eq!(
            outcome.brief_timeline.last().map(String::as_str),
            Some("Failed: watchdog timeout")
        );
    }
}

#[cfg(test)]
mod auto_73_tests {
    use super::{
        maybe_auto_resend_73, RecentManualCompletion, RecentManualCompletions,
        AUTO_73_FIRST_RESEND_MIN_DELAY, AUTO_73_MAX_RESENDS, AUTO_73_WINDOW,
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

    /// A completions map containing a single manual completion for `DX`,
    /// completed far enough in the past to clear the SM-F3/TX-F10
    /// first-resend guard (`AUTO_73_FIRST_RESEND_MIN_DELAY`) but still well
    /// within `AUTO_73_WINDOW` — i.e. a stashed completion whose original 73
    /// has certainly gone out by now, matching how these tests were written
    /// before that guard existed.
    fn map_with_dx() -> RecentManualCompletions {
        let mut map = HashMap::new();
        map.insert(
            DX.to_string(),
            RecentManualCompletion {
                completed_at: chrono::Utc::now() - chrono::Duration::seconds(40),
                frequency_hz: 1500.0,
                dx_parity: Some(SlotParity::Even),
                resends: 0,
                last_resend_at: None,
                remote_origin: false,
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
                    remote_origin: false,
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

    /// SM-F3/TX-F10: two decodes of the SAME closing RR73, arriving
    /// back-to-back immediately after `QsoCompleted` (i.e. within the
    /// original 73's TX-scheduler defer window), must NOT fire a second 73 —
    /// the first one may not have keyed yet. Neither call may consume the
    /// resend budget.
    #[tokio::test]
    async fn duplicate_decode_within_guard_window_no_resend() {
        let mgr = manager().await;
        // Completion JUST happened — the original 73 may still be deferred.
        let map = {
            let mut m = HashMap::new();
            m.insert(
                DX.to_string(),
                RecentManualCompletion {
                    completed_at: chrono::Utc::now(),
                    frequency_hz: 1500.0,
                    dx_parity: Some(SlotParity::Even),
                    resends: 0,
                    last_resend_at: None,
                    remote_origin: false,
                },
            );
            Arc::new(Mutex::new(m))
        };
        let policy = AtomicU8::new(TxPolicy::Full.as_u8());
        let bus = bus();
        let mut rx = mgr.subscribe();

        // Two "copies" of the same decode (as FT8 decoders routinely emit),
        // arriving in immediate succession.
        for _ in 0..2 {
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
        }

        assert_eq!(
            drain_sends(&mut rx),
            0,
            "duplicate decodes within the first-resend guard window must not \
             fire a 2nd 73 before the 1st one's scheduled defer could complete"
        );
        assert_eq!(
            map.lock().await.get(DX).map(|e| e.resends),
            Some(0),
            "guarded attempts must not consume the resend budget"
        );
    }

    /// A GENUINE later resend — the DX truly didn't copy our first 73 and
    /// repeats RR73 well after the first-resend guard window elapses — must
    /// still fire normally. This is the feature `maybe_auto_resend_73` exists
    /// for; the SM-F3/TX-F10 guard must not regress it.
    #[tokio::test]
    async fn genuine_later_resend_past_guard_window_fires() {
        let mgr = manager().await;
        let map = {
            let mut m = HashMap::new();
            m.insert(
                DX.to_string(),
                RecentManualCompletion {
                    completed_at: chrono::Utc::now()
                        - AUTO_73_FIRST_RESEND_MIN_DELAY
                        - chrono::Duration::seconds(5),
                    frequency_hz: 1500.0,
                    dx_parity: Some(SlotParity::Even),
                    resends: 0,
                    last_resend_at: None,
                    remote_origin: false,
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

        assert_eq!(
            drain_sends(&mut rx),
            1,
            "a genuine later resend past the guard window must still fire"
        );
        assert_eq!(map.lock().await.get(DX).map(|e| e.resends), Some(1));
    }

    /// Guard for the [duplicate_checking] wiring: the pancetta-config defaults
    /// must equal pancetta-qso's hard-coded DuplicateCheckConfig::default(),
    /// so a config file WITHOUT the section produces byte-identical behavior
    /// to the pre-wiring binary. If either side changes, this fails.
    #[test]
    fn config_duplicate_defaults_match_qso_manager_defaults() {
        let c = pancetta_config::Config::default().duplicate_checking;
        let q = pancetta_qso::DuplicateCheckConfig::default();
        assert_eq!(c.enabled, q.enabled);
        assert_eq!(c.time_window_hours, q.time_window_hours);
        assert_eq!(c.check_frequency, q.check_frequency);
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

/// Whether the per-QSO logbook upload subscriber should run at all: at least
/// one target must be enabled, AND `--replay` must not be active.
///
/// `--replay` never uploads. A QSO the engine "completes" off replayed
/// (historical) traffic would be POSTed to ClubLog/QRZ/LoTW/eQSL/cqdx.io as a
/// brand-new contact stamped with today's date — permanent, externally-visible
/// bad log data. Same gate as the reception/spot paths; see
/// `ApplicationCoordinator::replay_mode`.
fn logbook_upload_enabled(
    clublog_enabled: bool,
    qrz_enabled: bool,
    lotw_enabled: bool,
    eqsl_enabled: bool,
    cqdx_upload_enabled: bool,
    qrz_xml_enabled: bool,
    replay: bool,
) -> bool {
    !replay
        && (clublog_enabled
            || qrz_enabled
            || lotw_enabled
            || eqsl_enabled
            || cqdx_upload_enabled
            || qrz_xml_enabled)
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
    station_power_watts: u32,
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
        let processor =
            pancetta_qso::AdifProcessor::new().with_station_power_watts(station_power_watts);

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

/// Start the LOCAL source-of-truth QSO log writers and hand back the handles
/// the QSO component keeps alive for the process lifetime: the ADIF appender
/// (`~/.pancetta/qsos.adi`) and the rebuildable SQLite index
/// (`~/.pancetta/qso.db`).
///
/// **Suppressed entirely under `--replay`** (see
/// [`crate::coordinator::ApplicationCoordinator::replay_mode`]). A QSO the
/// engine "completes" off replayed (historical) traffic is not a contact that
/// ever happened, and it would be written to the operator's real log stamped
/// with today's date — the same fabricated-live-data failure the outbound
/// gates exist to stop, just aimed at the local logbook instead of the
/// network. Tagging it instead of dropping it is not an option: ADIF has no
/// standard field for "this is not a real contact". The only mechanism the
/// specification offers is `APP_<PROGRAMID>_<FIELDNAME>`, which is
/// private-by-convention to the originating program — no other logger, no
/// TQSL, no upload tool would recognise or honour it, so a tagged record would
/// still propagate as a genuine QSO the moment the file left this process.
/// Writing nothing is the only portable answer, so replay returns
/// `(None, None)` and the caller's keep-alive bindings stay the same shape
/// either way.
///
/// No drain is needed for the suppressed path (unlike the bus-consumer
/// components): each writer owns a `QsoManager::subscribe()` receiver that is
/// simply never created, and a `broadcast` channel does not back up on
/// account of a subscriber that does not exist.
///
/// On the live path the order matters and matches what it always was: ADIF
/// first, SQLite second, so a crash between the two is recoverable by the
/// startup ADIF→index replay (ADIF is the source of truth; the DB is a
/// cache). Both are fail-soft — a failure to open either is logged and the
/// other still runs.
async fn start_local_qso_log_writers(
    replay: bool,
    adif_path: &std::path::Path,
    db_path: &std::path::Path,
    station_power_watts: u32,
    persist_qso_timeline: bool,
    qso_manager: &pancetta_qso::QsoManager,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (
    Option<std::sync::Arc<pancetta_qso::AdifLogWriter>>,
    Option<std::sync::Arc<pancetta_qso::async_logger::QsoLogger>>,
) {
    if replay {
        info!(
            "--replay: local QSO log writes suppressed — neither {} nor {} is \
             opened, so a QSO 'completed' off replayed traffic leaves no record",
            adif_path.display(),
            db_path.display(),
        );
        return (None, None);
    }

    // ADIF source-of-truth writer. Subscribes to QsoEvent::QsoCompleted and
    // appends one ADIF record per completed QSO. Fail-soft: if open fails, we
    // log but proceed with DB-only — every operator should at least get
    // duplicate detection from the DB.
    let adif_writer = match pancetta_qso::AdifLogWriter::open(adif_path).await {
        Ok(mut w) => {
            info!("ADIF log open at {}", adif_path.display());
            w.set_station_power_watts(station_power_watts);
            let w = std::sync::Arc::new(w);
            start_adif_subscriber(w.clone(), qso_manager.subscribe(), shutdown);
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
    // and inserts into the rebuildable SQLite index.
    let logger_config = pancetta_qso::LoggerConfig {
        database_path: db_path.to_path_buf(),
        persist_qso_timeline,
        ..Default::default()
    };

    let async_logger = match pancetta_qso::async_logger::QsoLogger::new(
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

    (adif_writer, async_logger)
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
    use super::{cqdx_logbook_upload_enabled, cqdx_record_from_metadata, logbook_upload_enabled};
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
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
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

    /// `--replay` forces the logbook-upload subscriber off even when every
    /// target is enabled — a replayed "completed" QSO must never be filed as
    /// a real contact.
    #[test]
    fn replay_disables_upload_even_with_every_target_enabled() {
        assert!(!logbook_upload_enabled(
            true, true, true, true, true, true, true
        ));
    }

    /// With replay off, at least one enabled target is sufficient.
    #[test]
    fn any_single_target_enables_upload_when_not_replaying() {
        assert!(logbook_upload_enabled(
            true, false, false, false, false, false, false
        ));
        assert!(logbook_upload_enabled(
            false, true, false, false, false, false, false
        ));
        assert!(logbook_upload_enabled(
            false, false, true, false, false, false, false
        ));
        assert!(logbook_upload_enabled(
            false, false, false, true, false, false, false
        ));
        assert!(logbook_upload_enabled(
            false, false, false, false, true, false, false
        ));
        assert!(logbook_upload_enabled(
            false, false, false, false, false, true, false
        ));
    }

    /// No target enabled → no upload, replay or not.
    #[test]
    fn no_targets_enabled_means_no_upload() {
        assert!(!logbook_upload_enabled(
            false, false, false, false, false, false, false
        ));
        assert!(!logbook_upload_enabled(
            false, false, false, false, false, false, true
        ));
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
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
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

#[cfg(test)]
mod replay_local_log_tests {
    use super::start_local_qso_log_writers;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Minimal manager config: a real callsign (the placeholder guard in
    /// `start_cq` rejects NOCALL/N0CALL) and a grid, everything else default.
    fn manager() -> pancetta_qso::QsoManager {
        pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            ..Default::default()
        })
    }

    /// Drive one full CQ→grid→report→73 exchange, which is what makes the
    /// manager emit `QsoEvent::QsoCompleted` — the single event both local
    /// writers subscribe to.
    async fn complete_one_qso(manager: &pancetta_qso::QsoManager) {
        let freq = 14_074_000.0;
        manager
            .start_cq(freq, None, false)
            .await
            .expect("start_cq should succeed");

        manager
            .process_message(
                pancetta_qso::MessageType::CqResponse {
                    calling_station: "W1ABC".to_string(),
                    responding_station: "K1DEF".to_string(),
                    grid: Some("FN31".to_string()),
                },
                "W1ABC K1DEF FN31".to_string(),
                freq,
                Some(-10.0),
            )
            .await
            .expect("CqResponse should be accepted");
        manager
            .process_message(
                pancetta_qso::MessageType::ReportAck {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                    report: -12,
                },
                "W1ABC K1DEF R-12".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .expect("ReportAck should be accepted");
        manager
            .process_message(
                pancetta_qso::MessageType::SeventyThree {
                    to_station: "W1ABC".to_string(),
                    from_station: "K1DEF".to_string(),
                },
                "W1ABC K1DEF 73".to_string(),
                freq,
                Some(-11.0),
            )
            .await
            .expect("73 should be accepted");
    }

    /// Number of complete ADIF records in `path` (`<eor>` terminates one).
    /// A missing file counts as zero — the file only exists once something
    /// opened it for writing.
    fn adif_record_count(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .matches("<eor>")
            .count()
    }

    /// Poll for up to ~5s: the ADIF append happens in the subscriber task, so
    /// the LIVE control has to wait for it rather than race it.
    async fn wait_for_adif_record(path: &std::path::Path) -> usize {
        for _ in 0..100 {
            let n = adif_record_count(path);
            if n > 0 {
                return n;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        adif_record_count(path)
    }

    /// CONTROL (makes the replay assertion below non-vacuous): on a live run
    /// the very same exchange DOES reach both local writers — the ADIF file
    /// gains a record and the SQLite index is created.
    #[tokio::test]
    async fn live_run_writes_the_completed_qso_to_the_local_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adif_path = dir.path().join("qsos.adi");
        let db_path = dir.path().join("qso.db");
        let manager = manager();
        let shutdown = Arc::new(AtomicBool::new(false));

        let (adif_writer, async_logger) = start_local_qso_log_writers(
            false, // NOT replay
            &adif_path,
            &db_path,
            100,
            false,
            &manager,
            shutdown.clone(),
        )
        .await;
        assert!(
            adif_writer.is_some(),
            "precondition: a live run must open the ADIF source of truth"
        );
        assert!(
            async_logger.is_some(),
            "precondition: a live run must construct the SQLite QSO logger"
        );

        complete_one_qso(&manager).await;

        assert_eq!(
            wait_for_adif_record(&adif_path).await,
            1,
            "precondition: a completed QSO must be appended to the ADIF log on a \
             live run — otherwise the --replay assertions prove nothing"
        );
        assert!(
            db_path.exists(),
            "precondition: a live run must create the SQLite index at {}",
            db_path.display()
        );

        shutdown.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The gate itself: under `--replay` a QSO the engine "completes" off
    /// replayed (historical) traffic must leave ZERO trace in the operator's
    /// local log. Not a tagged record — no record, and no file at all, because
    /// ADIF has no portable way to mark a record as synthetic.
    #[tokio::test]
    async fn replay_run_writes_nothing_to_the_local_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adif_path = dir.path().join("qsos.adi");
        let db_path = dir.path().join("qso.db");
        let manager = manager();
        let shutdown = Arc::new(AtomicBool::new(false));

        let (adif_writer, async_logger) = start_local_qso_log_writers(
            true, // --replay
            &adif_path,
            &db_path,
            100,
            false,
            &manager,
            shutdown.clone(),
        )
        .await;
        assert!(
            adif_writer.is_none(),
            "the ADIF source of truth must not be opened under --replay"
        );
        assert!(
            async_logger.is_none(),
            "the SQLite QSO logger must not be constructed under --replay"
        );

        complete_one_qso(&manager).await;

        // Give any (wrongly) spawned subscriber the same window the live
        // control needed to land its record, so this is a real wait, not an
        // instant pass on a race.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_eq!(
            adif_record_count(&adif_path),
            0,
            "a replayed QSO must not appear in the ADIF log"
        );
        assert!(
            !adif_path.exists(),
            "--replay must not even create {} — zero trace",
            adif_path.display()
        );
        assert!(
            !db_path.exists(),
            "--replay must not even create the SQLite index at {} — zero trace",
            db_path.display()
        );

        shutdown.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod respond_to_caller_admission_tests {
    //! PAN-23 round-2 (Codex review of PR #283): the backend guards added to
    //! `qso_manager::respond_to_caller`/`respond_to_cq_with` only fire when
    //! a call is actually PROCESSED. But `RespondToCaller`'s admission path
    //! (the `if matches!(admit_new_qso(...), TxAdmission::Queue)` block
    //! above, guarding `pending_manual_calls`) can queue a request FIRST —
    //! whenever both TX parities are contested (an active QSO occupies the
    //! side opposite the one this caller wants) — and only run the
    //! `qso_manager` guard later, at promotion time
    //! (`promote_pending_manual_calls`). Since the unresolved-hash
    //! placeholder `"<...>"` is deterministically un-transmittable no
    //! matter how many times it's retried, letting it reach that point
    //! would re-queue it at the front of `pending_manual_calls` on every
    //! failed promotion attempt — repeatedly holding the parity slot
    //! against legitimate opposite-parity calls for up to the full
    //! `QUEUED_CALL_TTL` (10 minutes).
    //!
    //! The fix (this module proves it): reject the placeholder at
    //! ADMISSION time, in the `RespondToCaller` match arm itself, BEFORE
    //! the opposite-parity queueing check ever runs — so it can never
    //! occupy a slot in the first place. This exercises the REAL
    //! `start_qso_component` message loop end to end (real coordinator,
    //! real message bus, real admission logic), not a synthetic
    //! reproduction — a lower-level unit test of `qso_manager`'s guards
    //! alone (see `pancetta-qso/src/qso_manager.rs`) cannot see this gap,
    //! since it never goes through the queueing path at all.
    use super::super::ApplicationCoordinator;
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType, QsoMessage};
    use pancetta_config::Config;
    use pancetta_core::slot::SlotParity;
    use pancetta_core::ResponseStep;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Instant;

    /// Local copy of the `test_coordinator` pattern used elsewhere in this
    /// crate (`coordinator::health`, `coordinator::hamlib`) — no shared
    /// helper exists yet.
    async fn test_coordinator() -> ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// Poll the Tui channel (bounded crossbeam queue, not broadcast — a
    /// message sent before the consuming loop starts polling simply waits
    /// in the queue, so no readiness handshake is needed here) until `pred`
    /// matches, or give up after `max_tries` (10ms apart).
    async fn poll_until(
        rx: &crossbeam_channel::Receiver<ComponentMessage>,
        max_tries: u32,
        mut pred: impl FnMut(&ComponentMessage) -> bool,
    ) -> Option<ComponentMessage> {
        for _ in 0..max_tries {
            if let Ok(msg) = rx.try_recv() {
                if pred(&msg) {
                    return Some(msg);
                }
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    #[tokio::test]
    async fn respond_to_caller_placeholder_rejected_at_admission_not_queued() {
        let mut coordinator = test_coordinator().await;
        coordinator.config.write().await.station.callsign = "K1TEST".to_string();

        let (_tui_tx, tui_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .unwrap();

        coordinator.start_qso_component().await.unwrap();
        let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();

        // Pin current_tx_side to Odd: DX CQs on Even, so we answer on the
        // opposite side (Odd) — mirrors
        // `current_tx_side_none_when_idle_then_pins_after_admit`
        // (pancetta-qso/src/qso_manager.rs).
        let priming_id = manager
            .respond_to_cq("K9RDY".to_string(), 1500.0, Some(SlotParity::Even))
            .await
            .expect("seeding the parity-pinning QSO");
        assert_eq!(
            manager.current_tx_side().await,
            Some(SlotParity::Odd),
            "priming QSO must pin current_tx_side to Odd"
        );

        // A RespondToCaller whose caller transmits on Odd wants us to reply
        // on the OPPOSITE side (Even) — conflicting with the pinned Odd
        // side. Pre-fix, this exact shape is what got queued into
        // `pending_manual_calls` instead of rejected.
        let msg = ComponentMessage::new(
            ComponentId::Tui,
            ComponentId::Qso,
            MessageType::QsoMessage(QsoMessage::RespondToCaller {
                callsign: "<...>".to_string(),
                frequency: 2000,
                dx_parity: Some(SlotParity::Odd),
                step: ResponseStep::ReportAck,
                snr: Some(-8.0),
                remote_origin: false,
            }),
            Instant::now(),
        );
        coordinator.message_bus.send_message(msg).await.unwrap();

        // The admission-time rejection diagnostic must land on the Tui
        // channel — proves the guard fired before any queueing decision.
        let rejection = poll_until(&tui_rx, 200, |m| {
            matches!(
                &m.message_type,
                MessageType::DiagnosticEvent { target, callsign, .. }
                    if *target == "qso.security" && callsign.as_deref() == Some("<...>")
            )
        })
        .await;
        assert!(
            rejection.is_some(),
            "expected a qso.security DiagnosticEvent refusing the unresolved \
             hash placeholder at admission time"
        );

        // And it must never have been queued: no "Queued <callsign> ..."
        // status update naming the placeholder may have been emitted.
        let mut saw_queued = false;
        while let Ok(m) = tui_rx.try_recv() {
            if let MessageType::StatusUpdate(text) = &m.message_type {
                if text.contains("Queued") && text.contains("<...>") {
                    saw_queued = true;
                }
            }
        }
        assert!(
            !saw_queued,
            "the unresolved hash placeholder must never be queued — even \
             when the opposite-parity gate would otherwise defer the call — \
             or it would repeatedly hold the parity slot against legitimate \
             opposite-parity calls for up to the full queue TTL"
        );

        // Cleanup (best-effort, not asserted): retire the priming QSO.
        let _ = manager
            .fail_qso(priming_id, pancetta_qso::QsoFailureReason::UserCancelled)
            .await;
    }
}
