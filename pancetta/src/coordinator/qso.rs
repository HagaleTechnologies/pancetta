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

use super::ActiveQsoFreq;
use anyhow::Result;
use jiff::SignedDuration;
use pancetta_core::slot_clock::ClockStamp;
use pancetta_core::time::SignedDurationExt as _;
use pancetta_core::time::UtcTimestamp;
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
///
/// The resend re-keys the completed QSO by id
/// (`QsoManager::resend_completed_close`, PAN-275); it never opens a QSO, so
/// the contact is logged once however late in the window the repeat comes.
const AUTO_73_WINDOW: SignedDuration = SignedDuration::from_mins(3);
/// Maximum number of auto re-sends of our 73 per completed manual QSO.
const AUTO_73_MAX_RESENDS: u8 = 3;
/// Minimum spacing between auto re-sends (one FT8 slot is 15 s; we use a
/// slightly-under-slot guard so we fire at most once per slot even if the
/// DX's RR73 is decoded a hair early/late).
const AUTO_73_MIN_SPACING: SignedDuration = SignedDuration::from_secs(14);
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
const AUTO_73_FIRST_RESEND_MIN_DELAY: SignedDuration = SignedDuration::from_secs(32);

/// One recently-completed **manual** QSO, tracked so we can auto-re-send our
/// 73 if the DX keeps sending RR73/RRR. Keyed (in the map) by uppercased
/// callsign.
#[derive(Debug, Clone)]
struct RecentManualCompletion {
    /// When the QSO completed (window + pruning are measured from here).
    completed_at: UtcTimestamp,
    /// How many auto re-sends we have already done (bounded by
    /// [`AUTO_73_MAX_RESENDS`]).
    resends: u8,
    /// When we last auto-re-sent (one-per-slot guard). `None` = never yet.
    last_resend_at: Option<UtcTimestamp>,
    /// SECURITY: `true` iff the completed QSO was remote-initiated. The auto-73
    /// resend is itself a TX, so it must inherit the origin — a remote QSO's
    /// auto-73 is `TxOrigin::Remote` and armed-TX gated (fail-closed: if the
    /// arm lapsed after completion, the resend is dropped, never keyed as Local).
    remote_origin: bool,
    /// The completed QSO's `remote_client_key_id`, inherited for the same
    /// reason as `remote_origin` (PAN-91): the auto-73's arm-gate check must
    /// bind to the client that originally requested the QSO, not just "some
    /// client is currently armed".
    remote_client_key_id: Option<String>,
    /// The completed QSO's id. The auto-73 re-keys THIS QSO by id through
    /// `QsoManager::resend_completed_close` (PAN-275), so a repeat RR73 past
    /// the 45 s rework grace re-sends its 73 instead of opening (and logging)
    /// a second Completed QSO. It also reads the QSO's CURRENT identity
    /// (round-4 review, Codex P2): a repeated close action
    /// (operator-triggered, via `respond_to_caller`'s completed-QSO rework
    /// path) can rebind the LIVE `QsoManager` object's identity to a new
    /// controller, but this cache entry was only ever populated once, at
    /// `QsoCompleted` time — it never sees that later rebind. Trusting this
    /// snapshot would rebind the QSO back to the ORIGINAL client on every
    /// auto-resent 73.
    qso_id: pancetta_qso::QsoId,
    /// PAN-280: the auto-73 resends this entry charged whose TX outcome has
    /// not arrived yet, by attempt id. A `TxFrameNotSent` for one of them
    /// refunds its charge ([`refund_auto_73_attempt`]); a `TxFrameSent`
    /// keeps it ([`settle_auto_73_attempt`]). A list, because two resends can
    /// be in flight one slot apart.
    pending_resends: Vec<PendingAuto73Resend>,
}

/// One auto-73 resend charged to a [`RecentManualCompletion`] and still
/// waiting for its TX outcome (PAN-280).
#[derive(Debug, Clone, PartialEq)]
struct PendingAuto73Resend {
    /// The emitted resend frame's attempt id, echoed on its TX report.
    attempt_id: pancetta_qso::TxAttemptId,
    /// The `last_resend_at` stamp this resend set.
    committed_at: UtcTimestamp,
    /// `last_resend_at` before this resend, restored on a refund while the
    /// field still holds this resend's own stamp.
    prev_last_resend_at: Option<UtcTimestamp>,
}

/// item-2-auto-73 (PAN-275): record a manual completion for the auto-73.
/// Prunes entries older than [`AUTO_73_WINDOW`]. A repeated completion for
/// the SAME `qso_id` leaves the entry, its window and its counters as they
/// are; a completion with a different `qso_id` for the callsign is a new
/// contact and replaces the entry with a fresh budget.
fn stash_auto_73_completion(
    map: &mut HashMap<String, RecentManualCompletion>,
    callsign: &str,
    fresh: RecentManualCompletion,
    now: UtcTimestamp,
) {
    map.retain(|_, e| now.duration_since(e.completed_at) < AUTO_73_WINDOW);
    match map.entry(callsign.to_uppercase()) {
        std::collections::hash_map::Entry::Occupied(o) if o.get().qso_id == fresh.qso_id => {}
        std::collections::hash_map::Entry::Occupied(mut o) => {
            o.insert(fresh);
        }
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(fresh);
        }
    }
}

/// Drop-stale-TX liveness for a completed QSO's trailing frame. `keep_live`
/// makes the QSO's key live in `active_tx_qsos` (and its offset in
/// `active_tx_offsets`) until `grace` after the newest call for that key. A
/// purge task only removes a key whose generation it still owns, so an
/// earlier purge never cuts a later registration short (PAN-275).
///
/// Why a grace at all: a normally completing QSO emits its FINAL 73 right at
/// completion, so it must NOT be purged from the active set immediately —
/// that would race the 73 out of existence. The grace MUST outlast the
/// worst-case wait for the 73 to actually key PTT. Our TX slots are
/// SAME-PARITY only, which are **30 s** apart (e.g. Odd = :15/:45), not
/// 15 s. When the 73 is emitted just too late to key the immediately-next
/// same-parity slot, the scheduler defers it a full 30 s to the slot after
/// that. The old 16 s grace expired before that deferred slot, so the
/// stale-TX gate silently DROPPED the 73 — the operator saw us stop at
/// R-report and had to send 73 by hand (observed on-air with G8KHF,
/// 2026-06-23).
///
/// 45 s comfortably covers a full 30 s same-parity deferral plus the ≤8 s
/// tx-late window and margin, while still purging any leftover backlog
/// shortly after. Only the single 73 is pending post-completion (the
/// coalescer keeps newest-per-QSO), so a longer grace cannot leak stale
/// report frames. The component's grace shares its value with
/// `pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE` (the grace window
/// `respond_to_caller`'s FIX-B idempotent-close dispatch uses to find a
/// just-completed manual QSO to re-key instead of spawning a duplicate) so
/// the two windows can never drift apart.
///
/// The auto-73 re-registers the completed QSO it re-keys, because a re-key
/// past the first grace would otherwise find the key already purged and the
/// TX worker would drop the 73 (PAN-275).
#[derive(Clone)]
struct CompletedTxGrace {
    active_tx_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    active_tx_offsets: Arc<std::sync::RwLock<HashMap<String, f64>>>,
    latest_tx_intent: Arc<std::sync::RwLock<HashMap<String, super::LatestTxIntent>>>,
    fox_admitted_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Newest registration per key. Written and read only under this lock,
    /// which also covers the map inserts/removes, so a registration and a
    /// purge never interleave.
    generations: Arc<std::sync::Mutex<HashMap<String, u64>>>,
    /// Source of generation numbers: never reused, so a purge task that
    /// outlives its key's removal can never match a later registration.
    next_generation: Arc<std::sync::atomic::AtomicU64>,
    grace: Duration,
    // #40: queued-call promotion after the purge, same inputs as before.
    qso_manager: pancetta_qso::QsoManager,
    pending: PendingManualCalls,
    message_bus: MessageBus,
    limits: QsoCapacityLimits,
    /// PAN-274: shutdown ends a pending grace early; nothing is purged or
    /// promoted after it.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// PAN-358: the coordinator's slot clock; queued-call ages read it.
    clock: pancetta_core::SlotClock,
}

impl CompletedTxGrace {
    #[allow(clippy::too_many_arguments)]
    fn new(
        active_tx_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
        active_tx_offsets: Arc<std::sync::RwLock<HashMap<String, f64>>>,
        latest_tx_intent: Arc<std::sync::RwLock<HashMap<String, super::LatestTxIntent>>>,
        fox_admitted_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
        grace: Duration,
        qso_manager: pancetta_qso::QsoManager,
        pending: PendingManualCalls,
        message_bus: MessageBus,
        limits: QsoCapacityLimits,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        clock: pancetta_core::SlotClock,
    ) -> Self {
        Self {
            active_tx_qsos,
            active_tx_offsets,
            latest_tx_intent,
            fox_admitted_qsos,
            generations: Arc::new(std::sync::Mutex::new(HashMap::new())),
            next_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            grace,
            qso_manager,
            pending,
            message_bus,
            limits,
            shutdown,
            clock,
        }
    }

    /// A registrar over fresh maps, an empty pending-call queue, a private
    /// bus, capacity limits of 1 and a shutdown flag that never flips, for
    /// tests.
    #[cfg(test)]
    fn for_test(qso_manager: &pancetta_qso::QsoManager, grace: Duration) -> Self {
        let fox_admitted_qsos = Arc::new(std::sync::RwLock::new(
            std::collections::HashSet::<String>::new(),
        ));
        Self::new(
            Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            Arc::new(std::sync::RwLock::new(HashMap::new())),
            Arc::new(std::sync::RwLock::new(HashMap::new())),
            fox_admitted_qsos.clone(),
            grace,
            qso_manager.clone(),
            Arc::new(Mutex::new(std::collections::VecDeque::new())),
            MessageBus::new(1000).expect("bus"),
            QsoCapacityLimits {
                max_concurrent: 1,
                fox_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fox_max_streams: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
                fox_admitted_qsos,
                admission: Arc::new(tokio::sync::Mutex::new(())),
            },
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            test_queue_clock(),
        )
    }

    /// Make `qso_id`'s TX key live (with `offset_hz` in the offset registry)
    /// for a fresh `grace`, then purge it unless a later `keep_live` for the
    /// same key superseded this one. Returns this registration's generation.
    fn keep_live(&self, qso_id: pancetta_qso::QsoId, offset_hz: f64) -> u64 {
        let key = super::active_tx_qso_key(&qso_id.to_string());
        let generation = {
            let mut gens = self
                .generations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Ensure the key is present for the grace window's duration.
            // Normally a prior active StateChanged already inserted it
            // (idempotent here), but a QSO that OPENS directly at the close
            // (respond_to_caller SeventyThree → Completed) never passed
            // through an active state, and an auto-73 re-key comes after the
            // first grace purged it, so without this insert the QSO's 73
            // TransmitRequest would be dropped by the Step 4b gate.
            if let Ok(mut s) = self.active_tx_qsos.write() {
                s.insert(key.clone());
            }
            // FQ-F3: same idempotent insert for the offset registry.
            if let Ok(mut offsets) = self.active_tx_offsets.write() {
                offsets.insert(key.clone(), offset_hz);
            }
            let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
            gens.insert(key.clone(), generation);
            generation
        };
        let this = self.clone();
        tokio::spawn(async move {
            // PAN-274: shutdown ends the grace early; nothing is purged or
            // promoted.
            if !wait_out_completed_tx_grace(this.grace, &this.shutdown).await {
                debug!(
                    target: "tx.policy",
                    "QSO {} completed; shutdown began before its grace elapsed, nothing promoted",
                    qso_id
                );
                return;
            }
            if this.purge_if_current(&key, generation) {
                info!(
                    target: "tx.policy",
                    "QSO {} completed — grace elapsed, purging its TX from the active set",
                    qso_id
                );
                // #40: promote any operator-deferred cross-parity manual call
                // once THIS QSO's trailing 73 has cleared (grace elapsed) —
                // only then is the window truly free, so we never end up
                // TXing the 73 and a new opposite-window call in sequential
                // slots.
                promote_pending_manual_calls(
                    &this.qso_manager,
                    &this.pending,
                    &this.message_bus,
                    &this.limits,
                    &this.shutdown,
                    &this.clock,
                )
                .await;
            }
        });
        generation
    }

    /// PAN-280: forget `key`'s newest registration so every purge already
    /// pending for it stands down. Called when the key reopens from
    /// `Completed` (its 73 was not sent) or fails (purged at once). A later
    /// `keep_live` takes a fresh, never-reused generation.
    fn stand_down(&self, key: &str) {
        self.generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    /// Remove `key` from the TX maps when `generation` is still its newest
    /// registration. Returns whether it purged.
    fn purge_if_current(&self, key: &str, generation: u64) -> bool {
        let mut gens = self
            .generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gens.get(key) != Some(&generation) {
            return false;
        }
        gens.remove(key);
        if let Ok(mut s) = self.active_tx_qsos.write() {
            s.remove(key);
        }
        // PAN-76: a completed Hound keeps its Fox-admitted slot through the
        // grace, so its final RR73/73 still airs.
        if let Ok(mut s) = self.fox_admitted_qsos.write() {
            s.remove(key);
        }
        if let Ok(mut m) = self.latest_tx_intent.write() {
            m.remove(key);
        }
        if let Ok(mut offsets) = self.active_tx_offsets.write() {
            offsets.remove(key);
        }
        true
    }
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
    /// When the call was parked in the queue, on both the monotonic and the
    /// wall clock (PAN-289). Its age (see [`ClockStamp::age_at`]) drives the
    /// TTL watchdog and the expiry check at promotion, which retire calls that
    /// have waited without a free window for too long, sleep included.
    queued_at: ClockStamp,
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
    /// The station-agent peer (client keyId) that requested this, iff
    /// `remote_origin`. Carried across the deferral so the promoted QSO is
    /// bound to THIS specific client (PAN-91) — see
    /// `QsoMetadata::remote_client_key_id`.
    remote_client_key_id: Option<String>,
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

/// One slot clock for every test in this module, so `ClockStamp`s taken by
/// different tests and helpers share one monotonic origin.
#[cfg(test)]
fn test_queue_clock() -> pancetta_core::SlotClock {
    static CLOCK: std::sync::OnceLock<pancetta_core::SlotClock> = std::sync::OnceLock::new();
    CLOCK
        .get_or_init(|| {
            // A day of monotonic headroom, so a test can stamp a call hours
            // in the past without underflowing.
            let origin = std::time::Instant::now();
            pancetta_core::SlotClock::new(
                Arc::new(UtcTimestamp::now),
                Arc::new(move || origin.elapsed() + std::time::Duration::from_secs(86_400)),
                Arc::new(std::sync::atomic::AtomicI64::new(
                    pancetta_core::slot::SLOT_NS,
                )),
            )
        })
        .clone()
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
    at: UtcTimestamp,
}

/// Shared map (uppercased callsign → latest [`DxActivity`]) updated by the
/// decode loop for every decoded frame and read when building the active-QSO
/// snapshot. Bounded by [`DX_ACTIVITY_MAX`] + age pruning.
type DxActivityMap = Arc<std::sync::RwLock<HashMap<String, DxActivity>>>;

/// Cap on tracked callsigns; oldest are pruned past this.
const DX_ACTIVITY_MAX: usize = 256;

/// Entries older than this are treated as stale (DX has gone quiet) and not
/// surfaced; also the pruning horizon.
const DX_ACTIVITY_TTL: SignedDuration = SignedDuration::from_secs(150);

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
/// the allocator-default operator range for a nonzero Hold, or
/// `[TX_OFFSET_MIN_HZ, TX_OFFSET_MAX_HZ]` for Auto.
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
    let (lo, hi) = if hold_mode && held_hz != 0 {
        pancetta_qso::frequency::FrequencyAllocatorConfig::default().range
    } else {
        (
            pancetta_qso::TX_OFFSET_MIN_HZ,
            pancetta_qso::TX_OFFSET_MAX_HZ,
        )
    };
    let tx_off = pancetta_qso::deconflict_offset(
        candidate,
        active_offsets,
        pancetta_qso::MIN_TX_SEPARATION_HZ,
        lo,
        hi,
    );
    // Set partner_freq only when we actually diverge from the DX's RX freq.
    // The 1.0 Hz tolerance guards against float-rounding noise on the exact
    // Tx=Rx path.
    let partner_freq = ((tx_off - dx_freq).abs() > 1.0).then_some(dx_freq);
    (tx_off, partner_freq)
}

/// PAN-287: TX audio offset for a network-spot call whose DX offset is
/// unknown (`StartQso { dx_offset_unknown: true }`). `allocated_hz` is the
/// frequency allocator's pick, de-conflicted from the active QSOs exactly
/// like [`compute_manual_tx_offset`]. `partner_freq` is a placeholder equal
/// to our own TX offset: the existing two-sighting drift confirmation then
/// relatches `partner_freq` to wherever the DX answers, and our TX stays put.
pub fn compute_spot_call_tx_offset(
    allocated_hz: f64,
    active_offsets: &[f64],
) -> (f64, Option<f64>) {
    let tx_off = pancetta_qso::deconflict_offset(
        allocated_hz,
        active_offsets,
        pancetta_qso::MIN_TX_SEPARATION_HZ,
        pancetta_qso::TX_OFFSET_MIN_HZ,
        pancetta_qso::TX_OFFSET_MAX_HZ,
    );
    (tx_off, Some(tx_off))
}

/// PAN-287: a band change empties the cross-parity call queue. A call parked
/// on the old band must never promote and transmit on the new one. Returns
/// the dropped callsigns in queue order, so each can be reported.
fn drain_parked_calls_on_band_change(
    queue: &mut std::collections::VecDeque<PendingManualCall>,
) -> Vec<String> {
    queue.drain(..).map(|p| p.callsign).collect()
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
        Mt::ContestReply {
            to_station,
            from_station,
            ..
        } => (
            from_station.clone(),
            format!("→ {} (contest reply)", tgt(to_station)),
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
    now: UtcTimestamp,
) {
    let Some((from, summary)) = dx_activity_summary(msg, our_call) else {
        return;
    };
    if let Ok(mut m) = map.write() {
        m.insert(from.to_uppercase(), DxActivity { summary, at: now });
        // Cheap bound: prune by age, and if still over the cap drop the oldest.
        if m.len() > DX_ACTIVITY_MAX {
            m.retain(|_, a| now.duration_since(a.at) < DX_ACTIVITY_TTL);
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
fn lookup_dx_activity(map: &DxActivityMap, callsign: &str, now: UtcTimestamp) -> Option<String> {
    let m = map.read().ok()?;
    // Exact (uppercased) hit first; fall back to a compound-call match.
    let entry = m.get(&callsign.to_uppercase()).or_else(|| {
        m.iter()
            .find(|(k, _)| pancetta_qso::exchange::callsigns_match(k, callsign))
            .map(|(_, a)| a)
    })?;
    if now.duration_since(entry.at) < DX_ACTIVITY_TTL {
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
/// otherwise we may only add to the side already in flight. Parity decides
/// which calls may start; capacity decides how many (PAN-78): calls matching
/// that side (and any parity-agnostic call) start, oldest first, while
/// `free_slots` lasts. Cross-parity calls, and matching calls left over once
/// the free slots are used up, stay queued in their original order.
fn partition_pending_calls(
    queue: std::collections::VecDeque<PendingManualCall>,
    current_side: Option<pancetta_core::slot::SlotParity>,
    mut free_slots: usize,
) -> (
    Vec<PendingManualCall>,
    std::collections::VecDeque<PendingManualCall>,
) {
    let adopt =
        current_side.or_else(|| queue.front().and_then(PendingManualCall::desired_tx_parity));
    let mut start = Vec::new();
    let mut keep = std::collections::VecDeque::new();
    for p in queue {
        let matches_side = match (p.desired_tx_parity(), adopt) {
            // Rides any side, or matches the side we're committing to.
            (None, _) => true,
            (Some(want), Some(side)) => want == side,
            // Cross-parity: hold until this side clears too.
            _ => false,
        };
        if matches_side && free_slots > 0 {
            free_slots -= 1;
            start.push(p);
        } else {
            keep.push_back(p);
        }
    }
    (start, keep)
}

/// PAN-274: wait out the completed-TX grace (#40). `false` when shutdown
/// begins first; the caller then purges and promotes nothing.
async fn wait_out_completed_tx_grace(
    grace: Duration,
    shutdown: &std::sync::atomic::AtomicBool,
) -> bool {
    tokio::select! {
        biased;
        () = shutdown_requested(shutdown) => false,
        () = tokio::time::sleep(grace) => !shutdown.load(Ordering::Acquire),
    }
}

/// Promote operator-deferred manual calls (#40) once the TX window is free to
/// accept them.
///
/// Called from the QSO-event task after any QSO goes terminal. Determines the
/// side we may now commit to — the side our remaining active QSOs hold, or (if
/// idle) the parity the oldest pending call wants — and starts pending calls
/// that match it (concurrent, same window), oldest first, up to the free
/// capacity (PAN-78: `limits`' cap minus the active count, the same rule as
/// the manual open gates). Parity decides which calls may start; capacity
/// decides how many. Cross-parity calls, and matching calls with no free slot,
/// stay queued in order. Never preempts a live QSO.
///
/// Each promoted entry consumes one slot even if it turns out to resolve to an
/// existing QSO — conservative: it can delay a queued call by one terminal
/// event but never exceed the cap. An entry that fails to open frees its slot
/// again: the next pass starts the following matching entries, and the failed
/// one is re-queued at the front.
///
/// Once the shutdown flag is set it starts no call, so no deferred call is
/// keyed while pancetta quits; the calls it skipped stay queued (PAN-274).
async fn promote_pending_manual_calls(
    qso_manager: &pancetta_qso::QsoManager,
    pending: &PendingManualCalls,
    message_bus: &MessageBus,
    limits: &QsoCapacityLimits,
    shutdown: &std::sync::atomic::AtomicBool,
    clock: &pancetta_core::SlotClock,
) {
    // Capacity and parity are checked and the calls started under the
    // admission lock (see `QsoCapacityLimits::admission`).
    let _admission = limits.admission.lock().await;
    // Collect calls that fail to start so we can re-queue them at the front
    // rather than silently dropping them.
    let mut failed: Vec<PendingManualCall> = Vec::new();
    let mut stopped_for_shutdown = false;
    // PAN-289: queued calls found past their TTL here are dropped, never
    // started, and reported once after the passes.
    let mut expired_calls: Vec<String> = Vec::new();

    // A pass starts calls up to the free slots; a call that fails to start
    // frees its slot again, so passes repeat (the failed call held aside)
    // until a pass has no failures or nothing more can start. Nothing else
    // would retry the calls behind a failed one until the next terminal
    // event.
    loop {
        let mut pass_failed = false;
        // The parity our still-active QSOs are committed to (None ⇒ idle).
        let current_side = qso_manager.current_tx_side().await;
        // PAN-78: how many more QSOs may start (saturating: a count already above
        // the cap, e.g. Fox QSOs left over after Fox mode disengaged, frees none).
        let snap = limits.snapshot(qso_manager).await;
        let free_slots = snap.cap.saturating_sub(snap.active);

        let (to_start, keep_depth, cross_parity): (Vec<PendingManualCall>, usize, usize) = {
            let mut q = pending.lock().await;
            if q.is_empty() {
                break;
            }
            let queue = std::mem::take(&mut *q);
            // PAN-289: drop expired calls before partitioning, so an expired
            // head can neither start nor pick the parity an idle station
            // adopts. Promotion can run on wake before the 15 s watchdog does.
            let (queue, expired) = partition_expired(queue, clock.stamp(), QUEUED_CALL_TTL);
            info!(
                target: "qso",
                "promote_pending_manual_calls: current_side={:?}, queue_depth={}, expired={}",
                current_side,
                queue.len(),
                expired.len()
            );
            expired_calls.extend(expired);
            if queue.is_empty() {
                *q = queue;
                break;
            }
            // The side the partition commits to (for the log line below only).
            let adopt = current_side
                .or_else(|| queue.front().and_then(PendingManualCall::desired_tx_parity));
            let (start, keep) = partition_pending_calls(queue, current_side, free_slots);
            let keep_depth = keep.len();
            let cross_parity = keep
                .iter()
                .filter(|p| match (p.desired_tx_parity(), adopt) {
                    (None, _) => false,
                    (Some(want), Some(side)) => want != side,
                    (Some(_), None) => true,
                })
                .count();
            *q = keep;
            (start, keep_depth, cross_parity)
        };

        if keep_depth > cross_parity {
            // PAN-78: some calls matched the window but found no free slot.
            info!(
                target: "qso",
                "Promote: {} starting, {} still queued (capacity {}/{}, cross-parity {}); current_side={:?}",
                to_start.len(),
                keep_depth,
                snap.active,
                snap.cap,
                cross_parity,
                current_side
            );
        } else if keep_depth > 0 {
            info!(
                target: "qso",
                "Promote: {} call(s) starting, {} still queued (cross-parity); current_side={:?}",
                to_start.len(),
                keep_depth,
                current_side
            );
        }

        for p in to_start {
            // PAN-274: start nothing once shutdown has begun. The call goes
            // back at the front of the queue with any failed ones.
            if shutdown.load(Ordering::Acquire) {
                stopped_for_shutdown = true;
                failed.push(p);
                continue;
            }
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
                        p.remote_client_key_id.clone(),
                    )
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(()) => {
                    emit_status(
                        message_bus,
                        format!("Now calling {} (was queued)", p.callsign),
                        pancetta_core::DiagnosticLevel::Info,
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
                    pass_failed = true;
                }
            }
        }
        if stopped_for_shutdown || !pass_failed {
            break;
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
    if stopped_for_shutdown {
        let left = pending.lock().await.len();
        info!(target: "qso", "Shutdown: {left} deferred call(s) not started");
    }
    // Report outside the admission lock: the bus sends must not hold up a
    // manual open waiting on it.
    drop(_admission);
    report_expired_calls(message_bus, expired_calls).await;
}

/// Pure TTL partition: split the queue into entries that have NOT yet exceeded
/// the TTL (retained) and entries that have expired (returned as a `Vec` of
/// callsigns to report). An entry's age is [`ClockStamp::age_at`], so time the
/// machine spent asleep counts (PAN-289). `now` and `ttl` are injected so this
/// is deterministically unit-testable without sleeping.
fn partition_expired(
    queue: std::collections::VecDeque<PendingManualCall>,
    now: ClockStamp,
    ttl: std::time::Duration,
) -> (std::collections::VecDeque<PendingManualCall>, Vec<String>) {
    let mut kept = std::collections::VecDeque::new();
    let mut expired = Vec::new();
    for p in queue {
        if p.queued_at.age_at(now) >= ttl {
            expired.push(p.callsign.clone());
        } else {
            kept.push_back(p);
        }
    }
    (kept, expired)
}

/// The queued-call TTL task: an expiry pass every 15 s and on every event
/// the slot-clock driver publishes (PAN-358), so a call queued before a
/// sleep or a forward clock step expires as soon as the step is observed.
/// Exits on the first wake after `shutdown` is set.
fn spawn_queued_call_ttl(
    pending: PendingManualCalls,
    bus: MessageBus,
    clock: pancetta_core::SlotClock,
    mut events: tokio::sync::watch::Receiver<Option<pancetta_core::slot_clock::ClockEvent>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut events_open = true;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                changed = events.changed(), if events_open => {
                    if changed.is_err() {
                        // The coordinator dropped the sender: keep the tick.
                        events_open = false;
                        continue;
                    }
                }
            }
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            expire_stale_queued_calls(&pending, &bus, &clock).await;
        }
    })
}

/// TTL watchdog: remove entries from the cross-parity queue that have waited
/// longer than [`QUEUED_CALL_TTL`] without getting a free window to start.
/// Emits an operator status line + warn-level log for each retired call.
/// Called from a dedicated interval task (every 15 s).
async fn expire_stale_queued_calls(
    pending: &PendingManualCalls,
    message_bus: &MessageBus,
    clock: &pancetta_core::SlotClock,
) {
    let expired = {
        let mut q = pending.lock().await;
        if q.is_empty() {
            return;
        }
        let queue = std::mem::take(&mut *q);
        let (kept, expired) = partition_expired(queue, clock.stamp(), QUEUED_CALL_TTL);
        *q = kept;
        expired
    };
    report_expired_calls(message_bus, expired).await;
}

/// Tell the operator about queued calls retired for exceeding
/// [`QUEUED_CALL_TTL`]: a warn log, a status line and a diagnostic per call.
/// Shared by the TTL watchdog and the expiry check at promotion (PAN-289), so
/// both report identically.
async fn report_expired_calls(message_bus: &MessageBus, expired: Vec<String>) {
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
            pancetta_core::DiagnosticLevel::Warn,
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
async fn emit_status(
    message_bus: &MessageBus,
    text: impl Into<String>,
    level: pancetta_core::DiagnosticLevel,
) {
    let msg = ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Tui,
        MessageType::StatusUpdate {
            text: text.into(),
            level,
        },
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
/// On success it re-keys the stashed completed QSO by its id through
/// `QsoManager::resend_completed_close` (PAN-275): a 73 to the QSO's station
/// goes out under the QSO's id, and no QSO is opened, no state changes and no
/// `QsoCompleted` is emitted, however long ago the QSO completed — so
/// `qsos.adi` keeps one record and this entry's counters keep counting.
/// Right before the engine call it re-registers the QSO with `tx_grace`, so
/// the TX worker's drop-stale-TX gate keeps the QSO live for a fresh
/// `COMPLETED_QSO_REWORK_GRACE` and keys the 73. After the cap/window the
/// entry is dropped. A late permission denial, a QSO that is gone, no longer
/// a completed manual QSO, or on another band emits no frame and restores the
/// resend budget and its pacing timestamp.
#[allow(clippy::too_many_arguments)]
async fn maybe_auto_resend_73(
    msg_type: &pancetta_qso::states::MessageType,
    our_callsign: &str,
    frequency_hz: f64,
    qso_manager: &pancetta_qso::QsoManager,
    completions: &RecentManualCompletions,
    tx_policy: &std::sync::atomic::AtomicU8,
    message_bus: &MessageBus,
    remote_tx_arm: &std::sync::Arc<std::sync::Mutex<pancetta_agent::arm::ArmState>>,
    tx_grace: &CompletedTxGrace,
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

    let now = UtcTimestamp::now();

    // Decide under the map lock: is this a stashed manual completion still in
    // window and under the cap, with the per-slot guard satisfied? We mutate
    // the entry (resends/last_resend_at) here so the bound holds even if RR73
    // arrives every slot. We do NOT call into the QSO manager while holding
    // the lock.
    let entry_remote_origin;
    let entry_remote_client_key_id;
    let entry_qso_id;
    let prev_resends;
    let prev_last_resend_at;
    {
        let mut map = completions.lock().await;
        // Prune expired entries every time we look.
        map.retain(|_, e| now.duration_since(e.completed_at) < AUTO_73_WINDOW);

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
            && now.duration_since(entry.completed_at) < AUTO_73_FIRST_RESEND_MIN_DELAY
        {
            return;
        }
        if let Some(last) = entry.last_resend_at {
            if now.duration_since(last) < AUTO_73_MIN_SPACING {
                // Already re-sent this slot — ignore the duplicate decode.
                return;
            }
        }
        // Commit the send: increment + stamp BEFORE we drop the lock so two
        // decodes racing in the same slot can't both pass the per-slot guard.
        // Round-14 review (Codex P1): `prev_resends`/`prev_last_resend_at`
        // captured so this commit can be rolled back exactly (see below)
        // if the identity-bound permission check that follows — which
        // needs the live QSO identity, only knowable after this lock is
        // released — finds the bound client isn't currently TX-permitted.
        prev_resends = entry.resends;
        prev_last_resend_at = entry.last_resend_at;
        entry.resends += 1;
        entry.last_resend_at = Some(now);
        // SECURITY: the auto-73 inherits the completed QSO's origin so a remote
        // QSO's resend stays `TxOrigin::Remote` and armed-TX gated.
        entry_remote_origin = entry.remote_origin;
        entry_remote_client_key_id = entry.remote_client_key_id.clone();
        entry_qso_id = entry.qso_id;
    }

    // Round-4 review (Codex P2): prefer the LIVE QSO's identity over this
    // cache's snapshot — a repeated close action can rebind the QSO's
    // `remote_client_key_id` to a new controller after this entry was
    // stashed (see `RecentManualCompletion::qso_id`'s doc), and passing the
    // stale snapshot to `resend_completed_close` below would rebind the QSO
    // back to the ORIGINAL client on every auto-resent 73. Falls back to the
    // cached values only if the QSO has since aged out of the manager's
    // 1-hour retention (comfortably longer than `AUTO_73_WINDOW`, so this
    // should be rare in practice; the engine then refuses the re-key).
    let live = qso_manager.get_qso(entry_qso_id).await.ok();
    let (entry_remote_origin, entry_remote_client_key_id) = match &live {
        Some(progress) => (
            progress.metadata.remote_origin,
            progress.metadata.remote_client_key_id.clone(),
        ),
        None => (entry_remote_origin, entry_remote_client_key_id),
    };

    // Round 14 / PAN-138: this early check is a fast path for an already
    // denied client. The engine's re-check under its QSO write guard is
    // authoritative; its ManualCallOutcome also triggers rollback below.
    if entry_remote_origin
        && !crate::coordinator::tx::remote_tx_permitted_for(
            remote_tx_arm,
            pancetta_agent::arm::ArmTime::now,
            entry_remote_client_key_id.as_deref(),
        )
    {
        roll_back_auto_73_commit(
            completions,
            &key,
            entry_qso_id,
            now,
            prev_resends,
            prev_last_resend_at,
        )
        .await;
        return;
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

    // PAN-275: register the QSO's TX liveness BEFORE the engine emits its
    // MessageToSend (registration precedes the first frame, as on
    // completion). Past the first 45 s grace the key was already purged, so
    // without this the TX worker would drop the re-keyed 73. If the engine
    // then emits nothing, this registration simply expires after the grace.
    // It uses the QSO's stored offset — the one the re-keyed frame carries.
    // A re-key the engine is certain to refuse (QSO gone, not a completed
    // manual QSO, another band) registers nothing, so a station repeating
    // RR73 after a band change cannot keep the QSO live for minutes through
    // the budget rollback below.
    if let Some(progress) = &live {
        if qso_manager
            .completed_close_rekeyable_now(entry_qso_id, frequency_hz)
            .await
        {
            tx_grace.keep_live(entry_qso_id, progress.metadata.frequency);
        }
    }

    // PAN-275: re-key the EXISTING completed QSO by its stashed id — never
    // open a second QSO (which would emit a second `QsoCompleted` and log a
    // duplicate contact). PAN-138: the engine re-checks permission under its
    // own `qsos` guard and reports whether a frame went out. Anything else (a
    // late denial, a gone/ineligible QSO, another band) un-charges this
    // resend.
    let outcome = qso_manager
        .resend_completed_close(
            entry_qso_id,
            frequency_hz,
            entry_remote_origin, // inherit the completed QSO's origin
            entry_remote_client_key_id,
        )
        .await;
    match outcome {
        o if o.emitted_frame() => {
            // PAN-280: remember which frame this charge paid for, so a
            // key-time denial of it (`TxFrameNotSent`) refunds the charge.
            if let Some(attempt_id) = o.attempt_id() {
                record_pending_auto_73_resend(
                    completions,
                    &key,
                    entry_qso_id,
                    PendingAuto73Resend {
                        attempt_id,
                        committed_at: now,
                        prev_last_resend_at,
                    },
                )
                .await;
            }
            emit_status(
                message_bus,
                format!(
                    "Re-sending 73 to {} ({}/{}) — they repeated RR73",
                    from_station, resend_n, AUTO_73_MAX_RESENDS
                ),
                pancetta_core::DiagnosticLevel::Info,
            )
            .await;
        }
        outcome => {
            info!(
                target: "qso",
                "auto-73 re-send to {} not sent ({:?}); budget restored", from_station, outcome
            );
            roll_back_auto_73_commit(
                completions,
                &key,
                entry_qso_id,
                now,
                prev_resends,
                prev_last_resend_at,
            )
            .await;
        }
    }
}

/// Whether a completed QSO is stashed for `maybe_auto_resend_73`. Only manual
/// completions are; a Hound completion never is, because in the Fox/Hound
/// procedure the Fox's RR73 ends the contact and a Hound sends no 73 (PAN-259).
/// A Fox-role completion never is either: the Fox's RR73 already closed it
/// and a WSJT-X Hound sends no 73 back (PAN-386).
fn stashes_auto_73(metadata: &pancetta_qso::states::QsoMetadata) -> bool {
    metadata.initiated_by == pancetta_qso::CallInitiation::Manual
        && !metadata.hound
        && !metadata.fox
}

/// PAN-359: the operator's own activation reference from `[station]`
/// (`my_sig` / `my_sig_info`), trimmed, with blank values dropped, for
/// `QsoManagerConfig::activation`.
pub(crate) fn activation_from_station(
    station: &pancetta_config::StationConfig,
) -> pancetta_qso::ActivationReference {
    pancetta_qso::ActivationReference::new(
        station.my_sig.as_deref(),
        station.my_sig_info.as_deref(),
    )
}

#[cfg(test)]
#[path = "qso/tests/queued_call_ttl_clock_tests.rs"]
mod queued_call_ttl_clock_tests;

/// PAN-358 test aid for the cross-consumer slot-clock scenario
/// (`coordinator/tests/slot_clock_consumers_tests.rs`): run the production
/// TTL partition over calls queued at `stamps`, at `now`. Returns the kept
/// and the expired callsigns.
#[cfg(test)]
pub(super) fn queued_call_ttl_partition_for_test(
    stamps: &[(&str, ClockStamp)],
    now: ClockStamp,
) -> (Vec<String>, Vec<String>) {
    let queue = stamps
        .iter()
        .map(|(callsign, queued_at)| PendingManualCall {
            callsign: (*callsign).to_string(),
            frequency_hz: 1500.0,
            dx_parity: Some(pancetta_core::slot::SlotParity::Even),
            queued_at: *queued_at,
            hound: false,
            fox_freq_hz: None,
            fox_grid: None,
            held_hz: 0,
            hold_mode: false,
            remote_origin: false,
            remote_client_key_id: None,
            step: pancetta_core::ResponseStep::Grid,
            our_snr_of_them: None,
            their_report: None,
        })
        .collect();
    let (kept, expired) = partition_expired(queue, now, QUEUED_CALL_TTL);
    (kept.into_iter().map(|p| p.callsign).collect(), expired)
}

#[cfg(test)]
#[path = "qso/tests/fox_slot_tests.rs"]
mod fox_slot_tests;

#[cfg(test)]
#[path = "qso/tests/activation_from_station_tests.rs"]
mod activation_from_station_tests;

#[cfg(test)]
#[path = "qso/tests/upload_subscriber_startup_tests.rs"]
mod upload_subscriber_startup_tests;

/// Undo `maybe_auto_resend_73`'s budget commit for a resend that emitted no
/// frame. Only touches the entry this call committed (same completed QSO,
/// `last_resend_at` still its own stamp), so a newer completion stashed for
/// the same callsign meanwhile is left alone.
async fn roll_back_auto_73_commit(
    completions: &RecentManualCompletions,
    key: &str,
    qso_id: pancetta_qso::QsoId,
    committed_at: UtcTimestamp,
    prev_resends: u8,
    prev_last_resend_at: Option<UtcTimestamp>,
) {
    let mut map = completions.lock().await;
    if let Some(entry) = map.get_mut(key) {
        if entry.qso_id == qso_id && entry.last_resend_at == Some(committed_at) {
            entry.resends = prev_resends;
            entry.last_resend_at = prev_last_resend_at;
        }
    }
}

/// PAN-280: record an emitted auto-73 resend's attempt on the entry that
/// charged it, if that entry is still the same completed QSO.
async fn record_pending_auto_73_resend(
    completions: &RecentManualCompletions,
    key: &str,
    qso_id: pancetta_qso::QsoId,
    pending: PendingAuto73Resend,
) {
    let mut map = completions.lock().await;
    if let Some(entry) = map.get_mut(key) {
        if entry.qso_id == qso_id {
            entry.pending_resends.push(pending);
        }
    }
}

/// PAN-280 (ticket scenario 2): a completed QSO's auto-73 resend was denied
/// or dropped at key time (`TxFrameNotSent` for its attempt), so the resend
/// it was charged for never aired. Take one unit off the entry's `resends`,
/// and restore `last_resend_at` only while it still holds this resend's own
/// stamp (a later resend's pacing stays). Returns whether a charge was
/// refunded; an attempt no entry charged (an operator re-key, any other
/// frame, or `None`) refunds nothing.
async fn refund_auto_73_attempt(
    completions: &RecentManualCompletions,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> bool {
    let Some(attempt_id) = attempt_id else {
        return false;
    };
    let mut map = completions.lock().await;
    for entry in map.values_mut() {
        let Some(pos) = entry
            .pending_resends
            .iter()
            .position(|p| p.attempt_id == attempt_id)
        else {
            continue;
        };
        let pending = entry.pending_resends.remove(pos);
        entry.resends = entry.resends.saturating_sub(1);
        if entry.last_resend_at == Some(pending.committed_at) {
            entry.last_resend_at = pending.prev_last_resend_at;
        }
        // A later resend still in flight that recorded this one's stamp as
        // its predecessor now restores to this one's predecessor instead.
        for later in &mut entry.pending_resends {
            if later.prev_last_resend_at == Some(pending.committed_at) {
                later.prev_last_resend_at = pending.prev_last_resend_at;
            }
        }
        return true;
    }
    false
}

/// PAN-280: an auto-73 resend aired (`TxFrameSent` for its attempt), so its
/// charge stands. Drops the pending marker; returns whether one was held.
async fn settle_auto_73_attempt(
    completions: &RecentManualCompletions,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> bool {
    let Some(attempt_id) = attempt_id else {
        return false;
    };
    let mut map = completions.lock().await;
    for entry in map.values_mut() {
        if let Some(pos) = entry
            .pending_resends
            .iter()
            .position(|p| p.attempt_id == attempt_id)
        {
            entry.pending_resends.remove(pos);
            return true;
        }
    }
    false
}

/// PAN-280: the Qso task's `TxFrameNotSent` handling. First reverts a
/// completion still waiting on this frame (ticket scenario 1), so a reopened
/// QSO is active again and a re-key's PAN-75 ledger charge is reversible;
/// then rolls back the frame's ledger attempt as before, then refunds an
/// auto-73 resend charge that frame carried (ticket scenario 2).
async fn handle_tx_frame_not_sent(
    qso_manager: &pancetta_qso::QsoManager,
    completions: &RecentManualCompletions,
    qso_id: &str,
    message_text: &str,
    replaced: Option<&pancetta_qso::UnsentAttemptPayload>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) {
    match qso_id.parse::<pancetta_qso::QsoId>() {
        Ok(id) => {
            let completion = qso_manager.resolve_unsent_completion(id, attempt_id).await;
            let outcome = qso_manager
                .roll_back_unsent_attempt(id, message_text, replaced, attempt_id)
                .await;
            let refunded = refund_auto_73_attempt(completions, attempt_id).await;
            debug!(
                target: "qso",
                "TX frame not sent for QSO {} ('{}', attempt {:?}): completion {:?}; {:?}; auto-73 charge refunded: {}",
                qso_id, message_text, attempt_id, completion, outcome, refunded
            );
        }
        Err(e) => debug!(target: "qso", "TxFrameNotSent: bad QSO id '{}': {}", qso_id, e),
    }
}

/// PAN-280: the Qso task's `TxFrameSent` handling. Releases a PAN-182 held
/// stall action as before, logs a completion that was waiting on this frame
/// (its held `QsoCompleted`), then settles an auto-73 resend that aired.
async fn handle_tx_frame_sent(
    qso_manager: &pancetta_qso::QsoManager,
    completions: &RecentManualCompletions,
    qso_id: &str,
    message_text: &str,
    aired: &pancetta_qso::UnsentAttemptPayload,
    aired_at: UtcTimestamp,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) {
    match qso_id.parse::<pancetta_qso::QsoId>() {
        Ok(id) => {
            let released = qso_manager
                .confirm_stall_trigger_aired(id, message_text, aired, aired_at)
                .await;
            let completion = qso_manager
                .confirm_completion_aired(id, attempt_id, message_text, aired, aired_at)
                .await;
            let settled = settle_auto_73_attempt(completions, attempt_id).await;
            debug!(
                target: "qso",
                "TX frame aired for QSO {} ('{}', attempt {:?}): released held stall action: {}; completion {:?}; auto-73 charge settled: {}",
                qso_id, message_text, attempt_id, released, completion, settled
            );
        }
        Err(e) => debug!(target: "qso", "TxFrameSent: bad QSO id '{}': {}", qso_id, e),
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

/// PAN-259: every logical QSO contact one decoded frame carries.
///
/// An ordinary frame carries one, classified by [`ft8_message_to_qso_type`].
/// A DXpedition (type 0.1) Fox frame carries two: RR73 to its first Hound and
/// a signal report to its second, both from the Fox whose call travels only
/// as a 10-bit hash. When that hash did not resolve to exactly one known call
/// the frame yields no contact at all, so neither Hound's QSO can be advanced
/// by a sender nobody can name, and nothing falls back to text parsing.
///
/// The frame stays ONE physical decode for the TUI, relays and metrics; only
/// the QSO engine sees it as two contacts.
pub fn ft8_message_to_qso_types(
    message: &pancetta_ft8::Ft8Message,
    rendered_text: &str,
    our_callsign: &str,
) -> Vec<pancetta_qso::states::MessageType> {
    use pancetta_qso::states::MessageType as Mt;

    let Some(contacts) = message.dxpedition.as_ref() else {
        return vec![ft8_message_to_qso_type(
            message,
            rendered_text,
            our_callsign,
        )];
    };
    let Some(fox) = contacts.fox_call.as_ref() else {
        return Vec::new();
    };
    vec![
        Mt::FinalConfirmation {
            to_station: contacts.rr73_to.clone(),
            from_station: fox.clone(),
        },
        Mt::SignalReport {
            to_station: contacts.report_to.clone(),
            from_station: fox.clone(),
            report: contacts.report_db,
        },
    ]
}

/// Classify a decoded FT8 message directly from its already-parsed
/// `Ft8Message.standard_type`, instead of re-deriving classification
/// from rendered text via `parse_message`/`QSO_PATTERNS` (PAN-51).
///
/// For every `Some(..)` variant this is the whole PAN-51 win: the shape
/// decision was made against the raw 77-bit payload at decode time,
/// strictly more information than a regex on rendered text can ever
/// recover. This is the structural fix for PAN-49: a `ReplyWithR` decode
/// used to fall through to `NonStandard` because no regex in
/// `QSO_PATTERNS` matched an "R+grid" shape; here it's handled directly
/// (see `ContestReply`'s own doc comment, which already described
/// exactly this shape).
///
/// **`None` does NOT mean "unrecognized shape."** `standard_type` is only
/// ever assigned by `parse_type1_standard`
/// (`pancetta-ft8/src/message.rs`), which handles i3=1/2
/// `MessageType::Standard` payloads and nothing else. An i3=4
/// `MessageType::NonStdCall` payload -- a compound / DXpedition callsign
/// such as `YS/WE9G`, where one call is an exact 58-bit base-38 pack and
/// the other a 12-bit hash render (`<K5ARH>` resolved, `<...>`
/// unresolved) -- goes through `parse_nonstd_call`, which sets
/// `to_callsign`/`from_callsign`/`contest_exchange` and NEVER touches
/// `standard_type`. Those frames are perfectly ordinary CQs, replies, and
/// RR73/RRR/73 closes that a compound-callsign station is working us
/// with, and `is_plausible` (`message.rs`) explicitly accepts them via
/// its own `NonStdCall` shape check (PAN-17).
///
/// So `None` here means "not an i3=1/2 Standard payload" -- in production
/// exactly the i3=4 NonStdCall case, because `is_plausible` gates every
/// decode-acceptance point and rejects `FreeText`/`Telemetry`/`Contest`/
/// `FieldDay`/`RTTYRoundup`/`DXpedition`/`Unknown`, and `Extended` is
/// never produced. Those frames ARE classifiable from their rendered
/// text, which is precisely what `CALL_TOKEN` and
/// `normalize_callsign_token` (`pancetta-qso/src/exchange.rs`) were
/// widened/added for in PAN-17. Classifying them `NonStandard` would
/// silently revert that shipped fix and make every compound-callsign
/// QSO invisible to the QSO engine again, so the `None` arm falls back to
/// the existing, already-correct text parser -- including its bracket
/// stripping, so a resolved hash render latches as `K5ARH`, not
/// `<K5ARH>`.
///
/// `rendered_text` is the ALREADY-COMPUTED `DecodedMessage.text`, passed
/// in rather than re-derived via `Ft8Message`'s `Display` impl -- one
/// render, not two, and the `NonStandard` fallback carries exactly what
/// was actually logged for this decode (the text parser uppercases its
/// input, so its own `NonStandard` result is deliberately discarded in
/// favor of the verbatim `rendered_text`).
///
/// `our_callsign` is threaded through solely to construct the
/// `MessageExchange` the text parser hangs off; the parse path
/// (`parse_message` → `parse_cq_message`/`parse_qso_message`) never reads
/// it -- only the `generate_*` half of `MessageExchange` does.
pub fn ft8_message_to_qso_type(
    message: &pancetta_ft8::Ft8Message,
    rendered_text: &str,
    our_callsign: &str,
) -> pancetta_qso::states::MessageType {
    use pancetta_ft8::message::StandardMessageType;
    use pancetta_qso::states::MessageType as Mt;

    let from = || message.from_callsign.clone().unwrap_or_default();
    let to = || message.to_callsign.clone().unwrap_or_default();
    let grid = || message.grid_square.clone();
    let report = || message.signal_report.unwrap_or_default();

    // PR #344 round-2 Codex P2: an UNRESOLVED 12-bit hash callsign renders
    // as "<...N>" (unpack28 / CallsignField::Hash::to_callsign, literal
    // dots -- distinct from a RESOLVED hash render like "<K5ARH>", which
    // has no dots and is a real, routable callsign). is_plausible()
    // deliberately accepts both shapes (any "<"-prefixed string "looks
    // like a callsign" for decode-acceptance purposes), so an unresolved
    // hash can legitimately reach here with `standard_type: Some(..)` from
    // the native bit-unpacked path -- copying it verbatim into a typed
    // MessageType's callsign field would let maybe_answer_caller try to
    // open a QSO with a station we cannot actually address. The old text
    // parser's validate_callsign rejected this shape outright; route the
    // same way here rather than trusting it.
    let is_unresolved_hash =
        |s: &Option<String>| s.as_deref().is_some_and(|c| c.starts_with("<..."));
    if is_unresolved_hash(&message.from_callsign) || is_unresolved_hash(&message.to_callsign) {
        return match pancetta_qso::utils::parse_ft8_message(rendered_text, our_callsign) {
            Ok(Mt::NonStandard { .. }) | Err(_) => Mt::NonStandard {
                text: rendered_text.to_string(),
            },
            Ok(classified) => classified,
        };
    }

    match message.standard_type {
        Some(StandardMessageType::Cq) => Mt::Cq {
            callsign: from(),
            grid: grid(),
        },
        Some(StandardMessageType::Reply) => Mt::CqResponse {
            calling_station: to(),
            responding_station: from(),
            grid: grid(),
        },
        Some(StandardMessageType::ReplyWithR) => Mt::ContestReply {
            to_station: to(),
            from_station: from(),
            grid: grid().unwrap_or_default(),
            is_ack: true,
        },
        Some(StandardMessageType::Report) => Mt::SignalReport {
            to_station: to(),
            from_station: from(),
            report: report(),
        },
        Some(StandardMessageType::ReportWithR) => Mt::ReportAck {
            to_station: to(),
            from_station: from(),
            report: report(),
        },
        Some(StandardMessageType::Rrr) | Some(StandardMessageType::RR73) => Mt::FinalConfirmation {
            to_station: to(),
            from_station: from(),
        },
        Some(StandardMessageType::Final73) => Mt::SeventyThree {
            to_station: to(),
            from_station: from(),
        },
        // i3=4 NonStdCall (compound/DXpedition callsigns) -- see the doc
        // comment above. Fall back to the existing text parser, which is
        // exactly the path PAN-17 hardened for these frames. Its own
        // `NonStandard` result (and any parse error) collapses to the
        // verbatim `rendered_text` so the logged text stays byte-exact.
        None => match pancetta_qso::utils::parse_ft8_message(rendered_text, our_callsign) {
            Ok(Mt::NonStandard { .. }) | Err(_) => Mt::NonStandard {
                text: rendered_text.to_string(),
            },
            Ok(classified) => classified,
        },
    }
}

/// Pure classifier for automatic direct-caller admission (#39, PAN-120).
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

// Automatic direct-caller replies (#39 + #43 part 2, PAN-120) run independently
// of the autonomous toggle. New callers require console presence unless
// unattended_policy is RespondOnly. Existing exchanges are processed first
// and continue without this new-admission gate.
//
// 1. **Already in QSO** — if `process_message` (run first) is already driving
//    an exchange with this station, or one completed within
//    [`answer_caller_recent_window`] (120 s, or the whole [`AUTO_73_WINDOW`]
//    for an RR73/RRR — PAN-275), skip (no duplicate). This is not a new
//    caller, so it logs no caller outcome.
// 2. **TX policy and presence**: Disabled blocks entirely; otherwise require
//    operator presence or the configured direct-caller exception.
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

/// PAN-78: the ONE concurrent-QSO capacity rule for every QSO the QSO
/// component opens (auto-answer, manual opens, queued-call promotion).
///
/// Fox mode counts caller-answer QSOs against `fox_max_streams` (the Fox's own
/// CQ is a separate stream); otherwise every active QSO counts against
/// `max_concurrent_qsos`. Counts `QsoManager` state (non-terminal QSOs), NOT
/// `active_tx_qsos` (which keeps completed QSOs for the 45 s TX grace), so the
/// operator can call the next station the moment a QSO completes.
///
/// PAN-76: in Fox mode, Fox-admitted Hounds in their 45 s completed grace
/// still hold a TX stream for their final RR73/73, so they count against the
/// Fox cap too (see [`fox_grace_hound_count`]); otherwise a full fresh set of
/// Hounds on top of them could reach the coalescer's hard clamp and cut those
/// closing frames. Only the Fox's own CQ (Fox-admitted) is a free stream: any
/// other active `CallingCq` QSO (a manual `StartCq` while Fox mode is on)
/// counts too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacitySnapshot {
    pub active: usize,
    pub cap: usize,
    pub fox_mode: bool,
}

/// Active `CallingCq` QSOs that Fox mode did not open — CQs started by hand
/// (`StartCq`) while Fox mode is on. `active_caller_qso_count` leaves every
/// CQ out so the Fox's own CQ does not eat a Hound slot; these extra CQs are
/// ordinary streams and count. A poisoned Fox set counts every CQ.
async fn non_fox_cq_count(
    qso_manager: &pancetta_qso::QsoManager,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> usize {
    let active = qso_manager.get_active_qsos().await;
    let fox = fox_admitted_qsos.read().ok();
    active
        .iter()
        .filter(|(id, progress)| {
            matches!(progress.state, pancetta_qso::QsoState::CallingCq { .. })
                && !fox
                    .as_ref()
                    .is_some_and(|f| f.contains(&super::active_tx_qso_key(&id.to_string())))
        })
        .count()
}

/// Read the current [`CapacitySnapshot`] (see its doc for the rule).
pub(crate) async fn capacity_snapshot(
    qso_manager: &pancetta_qso::QsoManager,
    max_concurrent: usize,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> CapacitySnapshot {
    let is_fox = fox_mode.load(std::sync::atomic::Ordering::Relaxed);
    let effective_cap = if is_fox {
        fox_max_streams.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        max_concurrent
    };
    let active_count = if is_fox {
        qso_manager.active_caller_qso_count().await
            + fox_grace_hound_count(qso_manager, fox_admitted_qsos).await
            + non_fox_cq_count(qso_manager, fox_admitted_qsos).await
    } else {
        qso_manager.active_qso_count().await
    };
    CapacitySnapshot {
        active: active_count,
        cap: effective_cap,
        fox_mode: is_fox,
    }
}

/// The inputs of [`capacity_snapshot`], bundled so they can be cloned into
/// the QSO event task (queued-call promotion, PAN-78).
#[derive(Clone)]
pub(crate) struct QsoCapacityLimits {
    pub max_concurrent: usize,
    pub fox_mode: Arc<std::sync::atomic::AtomicBool>,
    pub fox_max_streams: Arc<std::sync::atomic::AtomicUsize>,
    pub fox_admitted_qsos: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// Held across every capacity/parity check and the QSO open it admits
    /// (manual opens, auto-answer, queued-call promotion), so two paths can
    /// never both take the same free slot.
    pub admission: Arc<tokio::sync::Mutex<()>>,
}

impl QsoCapacityLimits {
    pub(crate) async fn snapshot(
        &self,
        qso_manager: &pancetta_qso::QsoManager,
    ) -> CapacitySnapshot {
        capacity_snapshot(
            qso_manager,
            self.max_concurrent,
            &self.fox_mode,
            &self.fox_max_streams,
            &self.fox_admitted_qsos,
        )
        .await
    }
}

/// Which operator action a capacity refusal is for (PAN-78).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualOpen {
    /// `StartQso` — Space/Enter on a station, remote `callStation`.
    Call,
    /// `RespondToCaller` — reply from the Callers panel, remote `answerCaller`.
    Reply,
    /// `EngageHound` — call a Fox.
    Hound,
    /// `StartCq` — the `c` key, remote `startCq`.
    Cq,
}

fn capacity_limit_label(fox_mode: bool) -> &'static str {
    if fox_mode {
        "Fox mode"
    } else {
        "max_concurrent_qsos"
    }
}

/// "call to K2BBB" / "reply to K2BBB" / "Hound call to K2BBB" / "CQ" — the
/// object of a capacity-refusal diagnostic or remote reason.
fn manual_open_object(open: ManualOpen, callsign: Option<&str>) -> String {
    let cs = callsign.unwrap_or("?");
    match open {
        ManualOpen::Call => format!("call to {cs}"),
        ManualOpen::Reply => format!("reply to {cs}"),
        ManualOpen::Hound => format!("Hound call to {cs}"),
        ManualOpen::Cq => "CQ".to_string(),
    }
}

/// Operator status-bar line for a manual open refused at capacity (PAN-78).
fn manual_capacity_status(
    open: ManualOpen,
    callsign: Option<&str>,
    snap: CapacitySnapshot,
) -> String {
    let cs = callsign.unwrap_or("?");
    let verb = match open {
        ManualOpen::Call => format!("calling {cs}"),
        ManualOpen::Reply => format!("replying to {cs}"),
        ManualOpen::Hound => format!("calling Fox {cs}"),
        ManualOpen::Cq => "calling CQ".to_string(),
    };
    format!(
        "Not {verb} — {}/{} QSOs active ({}); finish or abort one first",
        snap.active,
        snap.cap,
        capacity_limit_label(snap.fox_mode)
    )
}

/// Reason relayed to remote clients (as `TxDenied`) for a manual open refused
/// at capacity (PAN-78).
fn manual_capacity_remote_reason(
    open: ManualOpen,
    callsign: Option<&str>,
    snap: CapacitySnapshot,
) -> String {
    format!(
        "{} refused — at capacity {}/{} ({})",
        manual_open_object(open, callsign),
        snap.active,
        snap.cap,
        capacity_limit_label(snap.fox_mode)
    )
}

/// PAN-78: is there room for a manual open that would ADD an active QSO?
/// Returns the snapshot to refuse with when there is not.
async fn manual_open_at_capacity(
    qso_manager: &pancetta_qso::QsoManager,
    max_concurrent: usize,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> Option<CapacitySnapshot> {
    let snap = capacity_snapshot(
        qso_manager,
        max_concurrent,
        fox_mode,
        fox_max_streams,
        fox_admitted_qsos,
    )
    .await;
    match pancetta_qso::qso_manager::admit_within_capacity(snap.active, snap.cap) {
        pancetta_qso::qso_manager::CapacityAdmission::Admit => None,
        pancetta_qso::qso_manager::CapacityAdmission::AtCapacity { .. } => Some(snap),
    }
}

/// PAN-78: refuse (never queue) a manual open at capacity — log it, add a
/// Warn Diagnostics entry, put the reason on the status bar and, for a remote
/// request, relay a `TxDenied` to the gateway (additive-only: gated on
/// `display_feed_enabled`, like every other gateway send).
async fn refuse_manual_open_at_capacity(
    message_bus: &MessageBus,
    display_feed_enabled: &std::sync::atomic::AtomicBool,
    open: ManualOpen,
    callsign: Option<&str>,
    snap: CapacitySnapshot,
    remote_origin: bool,
) {
    info!(
        target: "qso",
        "Refusing manual {:?} for {} — at capacity {}/{} (fox_mode={}, remote={})",
        open,
        callsign.unwrap_or("CQ"),
        snap.active,
        snap.cap,
        snap.fox_mode,
        remote_origin
    );
    emit_skip_diagnostic(
        message_bus,
        SkipSite::ManualOpenAtCapacity {
            open,
            callsign: callsign.map(str::to_string),
            active: snap.active,
            cap: snap.cap,
            fox_mode: snap.fox_mode,
        },
    )
    .await;
    emit_status(
        message_bus,
        manual_capacity_status(open, callsign, snap),
        pancetta_core::DiagnosticLevel::Warn,
    )
    .await;
    if remote_origin {
        super::remote_gateway::relay_to_gateway(
            message_bus,
            display_feed_enabled,
            ComponentId::Qso,
            MessageType::TxDenied {
                reason: manual_capacity_remote_reason(open, callsign, snap),
                qso_id: None,
            },
        )
        .await;
    }
}

/// PAN-76: remember that Fox mode opened `qso_id`, so the TX worker keeps its
/// stream after Fox mode is turned off. Called right after the open returns,
/// before any `.await`.
fn record_fox_admitted(
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
    qso_id: &pancetta_qso::QsoId,
) {
    if let Ok(mut set) = fox_admitted_qsos.write() {
        set.insert(super::active_tx_qso_key(&qso_id.to_string()));
    }
}

/// PAN-76: completed Fox-admitted Hounds still inside their 45 s completed
/// grace. They have left the active set, but keep their Fox-admitted TX
/// stream until the grace elapses so their final RR73/73 can air. Read from
/// the QSO manager's own record (completed QSOs stay in its map with their
/// `completed_at`; cancelled and failed ones leave it), so a Hound opened
/// already complete counts the moment `respond_to_caller` returns, with no
/// wait for the event forwarder, and stale Fox-set keys (never pruned) are
/// inert. A poisoned Fox set reads as 0, i.e. no tighter than without it.
async fn fox_grace_hound_count(
    qso_manager: &pancetta_qso::QsoManager,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> usize {
    let keys: Vec<String> = match fox_admitted_qsos.read() {
        Ok(set) => set.iter().cloned().collect(),
        Err(_) => return 0,
    };
    let now = UtcTimestamp::now();
    let mut count = 0usize;
    for key in keys {
        let Ok(id) = uuid::Uuid::parse_str(&key) else {
            continue;
        };
        if let Ok(progress) = qso_manager.get_qso(id).await {
            if let pancetta_qso::QsoState::Completed { completed_at, .. } = progress.state {
                if now - completed_at <= pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE {
                    count += 1;
                }
            }
        }
    }
    count
}

/// PAN-76: whether a Fox-admitted CQ is still calling, and how many
/// Fox-admitted Hounds still hold a TX stream: the active (non-terminal)
/// ones plus those in their completed grace ([`fox_grace_hound_count`]).
/// A poisoned Fox set reads as empty, like the TX worker's reads of it.
async fn fox_admitted_split(
    qso_manager: &pancetta_qso::QsoManager,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) -> (bool, usize) {
    let active = qso_manager.get_active_qsos().await;
    let (cq_calling, active_hounds) = {
        let Ok(fox) = fox_admitted_qsos.read() else {
            return (false, 0);
        };
        let mut cq_calling = false;
        let mut hounds = 0usize;
        for (id, progress) in &active {
            if !fox.contains(&super::active_tx_qso_key(&id.to_string())) {
                continue;
            }
            if matches!(progress.state, pancetta_qso::QsoState::CallingCq { .. }) {
                cq_calling = true;
            } else {
                hounds += 1;
            }
        }
        (cq_calling, hounds)
    };
    (
        cq_calling,
        active_hounds + fox_grace_hound_count(qso_manager, fox_admitted_qsos).await,
    )
}

/// How long after a completion `maybe_answer_caller` treats a direct call
/// from that station as part of the finished contact.
const ANSWER_CALLER_RECENT_QSO_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// PAN-275: an RR73/RRR to us (step `SeventyThree`) belongs to the auto-73 for
/// the whole of [`AUTO_73_WINDOW`], so the answer-caller path must not open a
/// second QSO for it while the auto-73 still owns the contact. Every other
/// step is a fresh exchange and keeps [`ANSWER_CALLER_RECENT_QSO_WINDOW`].
fn answer_caller_recent_window(step: pancetta_core::ResponseStep) -> std::time::Duration {
    let base = ANSWER_CALLER_RECENT_QSO_WINDOW;
    if step == pancetta_core::ResponseStep::SeventyThree {
        AUTO_73_WINDOW
            .to_std()
            .expect("AUTO_73_WINDOW is positive")
            .max(base)
    } else {
        base
    }
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
    unattended_policy: pancetta_config::UnattendedPolicy,
    operator_present: bool,
    max_concurrent: usize,
    message_bus: &MessageBus,
    fox_mode: &std::sync::atomic::AtomicBool,
    fox_max_streams: &std::sync::atomic::AtomicUsize,
    fox_admitted_qsos: &std::sync::RwLock<std::collections::HashSet<String>>,
) {
    // Classify before reporting a caller outcome; CQ and unrelated frames
    // must never receive a direct-call presence exemption.
    let Some(answer) = classify_caller_answer(msg_type, our_callsign) else {
        return;
    };

    // 1. Don't open a duplicate — process_message already drives any active QSO
    //    with this station (it ran before us this cycle). Also suppress for a
    //    recently-completed QSO (`answer_caller_recent_window`: 120 s, and the
    //    whole 3 min `AUTO_73_WINDOW` for an RR73/RRR, PAN-275): if the DX
    //    sends another RR73 after we already exchanged 73s, the bounded
    //    auto-resend-73 path (`maybe_auto_resend_73`) handles it — opening a
    //    brand-new QSO here would produce spurious duplicate 73s and log the
    //    contact twice. Explicit operator re-work via
    //    StartQso / Space does NOT come through this function, so it is
    //    unaffected by this gate. This runs before the TX-policy and presence
    //    refusals below: such a decode is not a new caller attempt, so it must
    //    not be logged as a suppressed direct-call reply (PAN-120).
    if qso_manager
        .has_active_or_recent_qso_with(
            &answer.their_call,
            frequency_hz,
            answer_caller_recent_window(answer.step),
        )
        .await
    {
        debug!(target: "qso", "Not auto-answering {} — active or recently-completed QSO exists", answer.their_call);
        return;
    }

    // 2. New caller: runtime TX policy, then operator presence. TX policy
    //    wins attribution when both conditions refuse.
    let policy =
        pancetta_core::TxPolicy::from_u8(tx_policy.load(std::sync::atomic::Ordering::Relaxed));
    if !policy.allows_any_tx() {
        info!(
            target: "tx.policy",
            callsign = %answer.their_call,
            reply_step = ?answer.step,
            %unattended_policy,
            tx_policy = policy.label(),
            policy_dropped = 1,
            presence_dropped = 0,
            respond_only_admitted = 0,
            "Direct-call reply suppressed by TX policy"
        );
        return;
    }
    if !operator_present && unattended_policy == pancetta_config::UnattendedPolicy::Off {
        info!(
            target: "tx.policy",
            callsign = %answer.their_call,
            reply_step = ?answer.step,
            %unattended_policy,
            operator_present,
            policy_dropped = 0,
            presence_dropped = 1,
            respond_only_admitted = 0,
            "Direct-call reply suppressed by operator-presence gate"
        );
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
    //
    //    PAN-78: the rule itself now lives in `capacity_snapshot`, shared with
    //    the manual open gates and queued-call promotion.
    let snap = capacity_snapshot(
        qso_manager,
        max_concurrent,
        fox_mode,
        fox_max_streams,
        fox_admitted_qsos,
    )
    .await;
    if let pancetta_qso::qso_manager::CapacityAdmission::AtCapacity { active, cap } =
        pancetta_qso::qso_manager::admit_within_capacity(snap.active, snap.cap)
    {
        debug!(target: "qso", "Not auto-answering {} — at capacity {}/{}", answer.their_call, active, cap);
        emit_skip_diagnostic(
            message_bus,
            SkipSite::AutoAnswerAtCapacity {
                callsign: answer.their_call.clone(),
                active,
                cap,
                fox_mode: snap.fox_mode,
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
    //
    // PAN-386: an FT8 Fox answers on its own lowest free 60 Hz slot in
    // 300-900 Hz instead (WSJT-X `foxgen.f90`), and hears the Hound at its
    // calling frequency (`partner_freq`). FT4/FT2 Fox mode keeps Tx=Rx.
    let fox_slots =
        snap.fox_mode && pancetta_qso::fox_slots_apply(&qso_manager.config().active_mode);
    let (tx_off, partner_freq) = if fox_slots {
        let (fox_offsets, other_offsets) = qso_manager.active_tx_offsets_by_fox_role().await;
        let Some(slot) = pancetta_qso::fox_slot_offset(&fox_offsets, &other_offsets) else {
            info!(
                target: "qso",
                "Not auto-answering {}: no free Fox slot in 300-900 Hz", answer.their_call
            );
            return;
        };
        info!(
            target: "qso",
            "Fox answer slot: caller_freq={:.0} → tx_off={:.0} Hz ({} Fox stream(s))",
            frequency_hz, slot, fox_offsets.len()
        );
        (slot, Some(frequency_hz))
    } else {
        let active = qso_manager.active_tx_offsets().await;
        let (tx_off, partner_freq) = compute_manual_tx_offset(frequency_hz, false, 0, &active);
        if tx_off != frequency_hz {
            info!(
                target: "qso",
                "Auto-answer TX offset de-conflicted: caller_freq={:.0} → tx_off={:.0} Hz ({} active)",
                frequency_hz, tx_off, active.len()
            );
        }
        (tx_off, partner_freq)
    };

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
            None,
        )
        .await
    {
        Ok(qso_id) => {
            if !operator_present {
                info!(
                    target: "tx.policy",
                    callsign = %answer.their_call,
                    reply_step = ?answer.step,
                    %unattended_policy,
                    operator_present,
                    %qso_id,
                    policy_dropped = 0,
                    presence_dropped = 0,
                    respond_only_admitted = 1,
                    "Direct-call reply admitted without console presence"
                );
            }
            // PAN-76: `snap.fox_mode` is the snapshot the capacity check used, so a
            // QSO admitted under Fox's raised cap is recorded even if Fox
            // mode was turned off during the awaits since.
            if snap.fox_mode {
                record_fox_admitted(fox_admitted_qsos, &qso_id);
                // PAN-386: an FT8 Fox answer is a Fox-role stream, pinned to
                // its slot and completed by the Hound's R-report.
                if fox_slots {
                    qso_manager.mark_fox_role(qso_id).await;
                }
            }
            emit_status(
                message_bus,
                format!("Answering {} (caller)", answer.their_call),
                pancetta_core::DiagnosticLevel::Info,
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
        R::ControlTransferred => "remote control passed to another client".to_string(),
        // PAN-280: a QSO opened at 73 whose 73 never aired; nothing logged.
        R::CloseNotAired => "closing 73 not sent, not logged".to_string(),
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
            UtcTimestamp::now() - started >= SignedDuration::from_mins(watchdog_minutes as i64)
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
    /// PAN-78: a manual open refused because it would exceed the cap.
    ManualOpenAtCapacity {
        open: ManualOpen,
        callsign: Option<String>,
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
        SkipSite::ManualOpenAtCapacity { open, callsign, active, cap, fox_mode } => {
            let what = manual_open_object(open, callsign.as_deref());
            let limit = capacity_limit_label(fox_mode);
            let callsign = if open == ManualOpen::Cq { None } else { callsign };
            SkipDiagnostic { target: "qso", level: L::Warn, text: format!("Refused {what} — at capacity {active}/{cap} ({limit})"), callsign }
        }
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

/// PAN-38 round 1 (Codex): tell the autonomous operator a self-CQ attempt
/// never actually transmitted, so it rolls back the streak/offset it
/// mutated speculatively for this attempt — same notification used for a
/// downstream `start_cq` failure, but here for a self-CQ that never even
/// reached `start_cq` (deferred by the cross-parity admission gate before
/// dispatch). A pounce (`callsign: Some(_)`) has no such state to roll back,
/// so `cq_attempt_id` is only ever `Some` for a genuine self-CQ.
async fn notify_autonomous_cq_dispatch_failed_if_self_cq(
    message_bus: &MessageBus,
    callsign: &Option<String>,
    cq_attempt_id: Option<u64>,
    pending_dispatch_failures: &std::sync::Mutex<Vec<u64>>,
) {
    let (None, Some(attempt_id)) = (callsign, cq_attempt_id) else {
        return;
    };
    // PAN-43/45 follow-up (Codex round 1 on PR #342): this notification is
    // the ONLY way a self-CQ's speculative streak/offset mutation ever
    // gets rolled back after a failed open -- including when the failure
    // itself was the Autonomous channel rejecting `AutonomousCqOpened`
    // for being full (PAN-45). A single unchecked `send_message` attempt
    // right after that same channel just rejected a delivery is exactly
    // the condition most likely to drop this one too, silently leaving
    // the mutation uncorrected forever. Retry with a short backoff
    // instead of one best-effort attempt -- bounded so a genuinely
    // wedged/disconnected Autonomous component doesn't stall this task
    // indefinitely.
    const RETRY_DELAYS_MS: [u64; 3] = [5, 20, 50];
    for attempt in 0..=RETRY_DELAYS_MS.len() {
        let fail_msg = ComponentMessage::new(
            ComponentId::Qso,
            ComponentId::Autonomous,
            MessageType::QsoMessage(crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
                attempt_id,
            }),
            Instant::now(),
        );
        match message_bus.send_message_checked(fail_msg).await {
            Ok(true) => return,
            Ok(false) => match RETRY_DELAYS_MS.get(attempt) {
                Some(&delay_ms) => tokio::time::sleep(Duration::from_millis(delay_ms)).await,
                None => {
                    // PAN-43/45 follow-up (Codex round 2 on PR #342): the
                    // message-bus retries above are still bounded by a
                    // ~75ms budget -- sustained congestion past that
                    // window (the Autonomous task itself stuck/backlogged,
                    // not just a momentary blip) would otherwise still
                    // drop this permanently. Fall back to shared state the
                    // Autonomous task drains directly every
                    // `slot_interval` tick, independent of its own
                    // message-bus channel entirely -- same pattern as
                    // `hamlib_pending_frequency` (PAN-19 round 10) for the
                    // identical class of problem.
                    warn!(
                        "AutonomousCqDispatchFailed dropped for attempt {} after {} \
                         message-bus attempts -- falling back to the queue-independent \
                         pending-failures list",
                        attempt_id,
                        RETRY_DELAYS_MS.len() + 1
                    );
                    pending_dispatch_failures
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(attempt_id);
                }
            },
            Err(e) => {
                warn!("Failed to send AutonomousCqDispatchFailed: {}", e);
                return;
            }
        }
    }
}

/// Register the qso_id<->cq_attempt_id association (`AutonomousCqOpened`)
/// for a self-CQ dispatch, returning whether it's safe to proceed with
/// `start_cq_with_id`. A CQ with no `cq_attempt_id` (nothing to register)
/// always returns `true`.
///
/// PAN-45 (Codex round 6 on PR #301): `send_message` swallows a dropped
/// (bounded-queue-full or disconnected) delivery into `Ok(())`,
/// indistinguishable from success -- the same class of bug round 5 already
/// fixed for the sibling `StartAutonomousQso` dispatch via
/// `send_message_checked`. If this registration is silently dropped and
/// the caller dispatches anyway, the QSO opens for real but the operator
/// never learns qso_id<->attempt_id, so a later failed `TransmitComplete`
/// for it finds no `pending_self_cq_qsos` entry and is silently ignored --
/// leaving the failed CQ's streak/offset mutation permanently uncorrected.
/// Returning `false` here lets the caller decline to dispatch at all
/// instead; the existing generic `Err(e)` handling around the dispatch
/// call already rolls back the streak via
/// `notify_autonomous_cq_dispatch_failed_if_self_cq`, exactly like any
/// other downstream open failure.
async fn register_autonomous_cq_open_or_decline(
    message_bus: &MessageBus,
    pre_generated_qso_id: pancetta_qso::QsoId,
    cq_attempt_id: Option<u64>,
) -> bool {
    let Some(attempt_id) = cq_attempt_id else {
        return true;
    };
    let opened_msg = ComponentMessage::new(
        ComponentId::Qso,
        ComponentId::Autonomous,
        MessageType::QsoMessage(crate::message_bus::QsoMessage::AutonomousCqOpened {
            qso_id: pre_generated_qso_id.to_string(),
            attempt_id,
        }),
        Instant::now(),
    );
    match message_bus.send_message_checked(opened_msg).await {
        Ok(true) => true,
        Ok(false) => {
            warn!(
                "AutonomousCqOpened dropped (channel full or disconnected) for attempt {} -- \
                 declining to dispatch the self-CQ",
                attempt_id
            );
            false
        }
        Err(e) => {
            warn!("Failed to send AutonomousCqOpened: {}", e);
            false
        }
    }
}

#[cfg(test)]
#[path = "qso/tests/autonomous_cq_open_registration_tests.rs"]
mod autonomous_cq_open_registration_tests;

#[cfg(test)]
#[path = "qso/tests/skip_diagnostic_tests.rs"]
mod skip_diagnostic_tests;

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
        metadata.contact_started_at().strftime("%H:%M:%S")
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
        qso_uuid: metadata.qso_id.to_string(),
        outcome,
        last_state,
        freq_hz: metadata.frequency as u32,
        ts: UtcTimestamp::now(),
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
    decode_window: &std::sync::Arc<tokio::sync::RwLock<super::DecodeWindowConfig>>,
    ap_state: &std::sync::Arc<std::sync::RwLock<Vec<pancetta_ft8::QsoAp>>>,
) {
    let active = qso_manager.get_active_qsos().await;
    let candidates: Vec<QsoApCandidate> = active
        .iter()
        .filter_map(|(_, progress)| {
            qso_ap_candidate_from_progress(&progress.state, &progress.metadata, my_call)
        })
        .collect();
    let max_ap_qsos = decode_window.read().await.ft8().max_ap_qsos;
    let ranked = rank_active_qsos_for_ap(&candidates, scorer, max_ap_qsos);
    if let Ok(mut guard) = ap_state.write() {
        *guard = ranked;
    }
}

#[cfg(test)]
#[path = "qso/tests/ap_ranking_tests.rs"]
mod ap_ranking_tests;

impl super::ApplicationCoordinator {
    /// Start QSO management component
    ///
    /// Wires decoded FT8 messages into the QSO manager for state tracking,
    /// auto-logging to SQLite at `~/.pancetta/qso.db`, and duplicate detection.
    pub(crate) async fn start_qso_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_qso");
        let _enter = span.enter();

        info!("Starting QSO component");

        // PAN-185: read through the durable inbox so TX outcome reports
        // parked behind a full channel still arrive.
        let qso_inbox = self.message_bus.durable_inbox(ComponentId::Qso).await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();
        let pending_autonomous_cq_dispatch_failures =
            self.pending_autonomous_cq_dispatch_failures.clone();
        let pending_qso_offset_requests = self.pending_qso_offset_requests.clone();

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
        // PAN-359: the operator's own `[station] my_sig` / `my_sig_info`,
        // stamped as ADIF MY_SIG / MY_SIG_INFO on every logged QSO.
        let activation = activation_from_station(&config.station);
        if !activation.is_empty() {
            info!(
                "PAN-359: logging MY_SIG={:?} MY_SIG_INFO={:?} on every QSO",
                activation.my_sig.as_deref().unwrap_or(""),
                activation.my_sig_info.as_deref().unwrap_or("")
            );
        }
        let attribution_comment = config.logging.attribution_comment;
        // PAN-382: snapshot `[logging]` for the N1MM contactinfo gate below.
        let logging_cfg = config.logging.clone();
        // Snapshot the opt-in QSO-upload settings. Only when at least one is
        // enabled do we build clients + spawn the upload subscriber.
        let clublog_cfg = config.network.clublog.clone();
        let qrz_cfg = config.network.qrz_logbook.clone();
        let lotw_cfg = config.network.lotw.clone();
        let eqsl_cfg = config.network.eqsl.clone();
        let cqdx_cfg = config.network.cqdx.clone();
        // PAN-57: the quick-retry schedule for the pending-upload queue.
        let retry_cfg = config.network.reliability.retry.clone();
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
        let unattended_policy = config.autonomous.unattended_policy;
        let last_operator_input_ms = self.last_operator_input_ms.clone();
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
        let database_settings = config.database.clone();
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
        // PAN-72 (Task 7): operator-configured stall-switch threshold,
        // threaded into pancetta_qso::TimeoutConfig below so the TOML
        // setting (Task 6) actually reaches the QSO engine's Rust default
        // (Task 2) instead of being silently overridden by it.
        let qso_stall_switch_after = config.autonomous.qso_stall_switch_after;
        // PAN-128 PR review: the stall ledger's current-slot acceptance must
        // mirror the TX scheduler's own late-start window, so the operator's
        // `[station] tx_late_max_ms` reaches `keep_call_tx_slot` — the same
        // value the TX worker snapshots for `schedule_tx`.
        let station_tx_late_max_ms = config.station.tx_late_max_ms;

        // PAN-50: convert the validated `[contest]` section into the
        // contest-scoring context here, where both pancetta-config and the
        // pancetta-qso rule catalog are visible (serialized config stays
        // independent of pancetta-qso). An unknown rules_id, a profile that
        // does not match the selected rules, or an invalid operator_region
        // refuses startup with the operator-facing message rather than
        // silently disabling scoring; structural validation (required
        // fields, period shape) already ran in Config::validate.
        // Computed before `drop(config)` below so the read guard can be
        // released on schedule.
        let contest_scoring = match &config.contest {
            Some(contest) if contest.enabled => {
                let periods = contest
                    .periods
                    .iter()
                    .map(|window| pancetta_qso::contest::scoring::ContestPeriod {
                        start: window.start,
                        end: window.end,
                    })
                    .collect();
                Some(pancetta_qso::contest::scoring::ContestScoringContext::new(
                    contest.instance_id.as_deref().unwrap_or(""),
                    contest.profile_id.as_deref().unwrap_or(""),
                    contest.rules_id.as_deref().unwrap_or(""),
                    contest.operator_region.as_deref().unwrap_or(""),
                    periods,
                ))
            }
            _ => None,
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
        // PAN-382: N1MM contactinfo UDP destination, `None` when the key is
        // unset and always under `--replay` (replayed QSOs are not real
        // contacts, so no socket is ever bound).
        let n1mm_destination = super::n1mm_udp::n1mm_udp_destination(&logging_cfg, replay_mode);
        if replay_mode && logging_cfg.n1mm_udp_destination().is_some() {
            info!("Replay mode: N1MM contactinfo UDP suppressed -- replayed QSOs are not real contacts");
        }
        if let Some(dest) = n1mm_destination {
            info!("N1MM contactinfo UDP enabled (destination: {dest})");
        }
        // PAN-41 round 3: test-only override for where the replay-history-seed
        // read (below) and the non-replay writers look for `~/.pancetta` --
        // always `None` in production (see the field's own doc comment).
        let pancetta_home_override = self.pancetta_home_override.clone();
        let qrz_xml_base_url = self.qrz_xml_base_url_override.clone();
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

        // PAN-57: ClubLog, QRZ Logbook, eQSL and cqdx.io uploads go through
        // the one process-wide pending-upload worker. It is started on the
        // first Qso start that has a destination and reused on every restart
        // (a crash-restart can briefly leave the old subscriber alive; two
        // writers on the queue file must never happen).
        let outbox_dests = super::upload_outbox::outbox_destinations(
            clublog_cfg.enabled,
            qrz_cfg.enabled,
            eqsl_cfg.enabled,
            cqdx_upload_enabled,
            replay_mode,
            lotw_cfg.enabled,
        );
        if upload_enabled && !outbox_dests.is_empty() && self.upload_outbox.is_none() {
            let clients = super::upload_outbox::build_logbook_clients(
                &clublog_cfg,
                &qrz_cfg,
                &eqsl_cfg,
                &cqdx_cfg,
                &upload_our_callsign,
                &lotw_cfg,
            );
            // PAN-376: the outbox lives in the data directory.
            let path = super::data_dir_for(pancetta_home_override.as_deref())
                .unwrap_or_else(pancetta_core::data_dir::current)
                .join(pancetta_dx::upload_outbox::OUTBOX_FILE_NAME);
            let schedule = pancetta_dx::upload_outbox::RetrySchedule::new(
                retry_cfg.max_attempts,
                retry_cfg.base_delay_ms,
                retry_cfg.max_delay_ms,
                retry_cfg.backoff_multiplier,
                retry_cfg.jitter_factor,
                super::upload_outbox::OutboxTiming::PRODUCTION.slow_retry,
            );
            let (handle, task) = super::upload_outbox::start_upload_outbox(
                path,
                std::sync::Arc::new(clients),
                schedule,
                super::upload_outbox::OutboxTiming::PRODUCTION,
                self.upload_health.clone(),
                Some(self.message_bus.clone()),
                self.shutdown_signal.clone(),
                Some(self.qso_upload_statuses.clone()),
            );
            self.upload_outbox = Some(handle);
            self.upload_outbox_task = Some(task);
        }
        let upload_outbox = if outbox_dests.is_empty() {
            None
        } else {
            self.upload_outbox.clone()
        };
        let active_qso_ap = self.active_qso_ap.clone();
        // Task 5: `max_ap_qsos` is hot-reloadable, so the AP-context write
        // site below re-reads it fresh from this shared handle on every
        // QSO event rather than snapshotting it once at startup.
        let decode_window_for_ap = self.decode_window.clone();
        let active_qso_freq_hz = self.active_qso_freq_hz.clone();
        // PAN-72 round-8 redesign (fix 2): the same live `active_slot_ns`
        // atomic Fix 1 uses inside `pancetta-qso`, so the decoder hint's
        // secondary-window grace agrees with the QSO engine's own crediting
        // grace — one source of truth.
        let qso_freq_slot_ns = self.active_slot_ns();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let split_tx_frequency_hz = self.split_tx_frequency_hz.clone();
        let tx_freq_mode = self.tx_freq_mode.clone();
        // PAN-140: share the coordinator's real PTT-active flag so
        // `apply_tx_offset_switch`'s pre-commit recheck sees genuine TX-worker
        // state, not its private `false` default.
        let ptt_active = self.ptt_active.clone();
        // PAN-143: share the coordinator's PTT/offset-switch synchronization
        // gate so the recheck above is genuinely atomic against the TX
        // worker's/TUI-relay task's writes, not just narrowly raced against
        // them.
        let ptt_sync_gate = self.ptt_sync_gate.clone();
        let relocation_epochs = self.relocation_epochs.clone();
        // T3 will read this to apply the operator's held TX audio offset when
        // starting a manual QSO (Hold mode). Captured here so both atomics
        // are in scope in the StartQso/RespondToCaller handlers below.
        let tx_offset_hold_hz = self.tx_offset_hold_hz.clone();
        // Shared with the TX worker — drives the "drop TX for ended QSOs"
        // gate. The QSO component keeps it in sync from the QsoEvent stream
        // below.
        let active_tx_qsos = self.active_tx_qsos.clone();
        // PAN-76: QSOs Fox mode opened. Written by the Fox engage arm and
        // `maybe_answer_caller`; drained by the event forwarder below at the
        // same points as `active_tx_qsos`.
        let fox_admitted_qsos = self.fox_admitted_qsos.clone();
        // PAN-78: the admission lock (see `QsoCapacityLimits::admission`):
        // the message loop's opens and the event forwarder's queued-call
        // promotion run in different tasks and must not both take one slot.
        let qso_admission = Arc::new(tokio::sync::Mutex::new(()));
        #[cfg(test)]
        {
            self.qso_admission_for_test = Some(qso_admission.clone());
        }
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
        // PAN-50: surface the contest-context conversion error (unknown
        // rules_id / invalid operator_region) with the operator-facing
        // message; see the extraction above `drop(config)`.
        let contest_scoring = contest_scoring
            .transpose()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;

        let qso_config = {
            use pancetta_qso::{
                DuplicateCheckConfig, HoundRegions, QsoManagerConfig, TimeoutConfig,
            };

            QsoManagerConfig {
                our_callsign: our_callsign.clone(),
                our_grid: our_grid.clone(),
                contest_scoring,
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
                // PAN-72 (Task 7): thread the operator's TOML
                // autonomous.qso_stall_switch_after through; every other
                // TimeoutConfig field keeps the qso-side default.
                timeouts: TimeoutConfig {
                    qso_stall_switch_after,
                    ..Default::default()
                },
                activation,
                ..Default::default()
            }
        };

        let mut qso_manager = pancetta_qso::QsoManager::new(qso_config);
        qso_manager.require_persistent_history();
<<<<<<< HEAD
        // PAN-359: share the coordinator's worked-station activation book,
        // which the cqdx bridge's spot poller fills from cqdx.io, so a QSO
        // with a listed activator logs SIG / SIG_INFO. Set before the
        // supervisor clone below so that handle reads the same book.
        qso_manager.set_worked_reference_source(self.worked_references.clone());
=======
        // PAN-358: the watchdog reads the coordinator's one slot clock (over
        // the same `active_slot_ns` atomic) and takes its PAN-296 budget
        // resets from that clock's events, not a detector of its own. Set
        // before the supervisor clone below, so that handle reads the same
        // clock.
        qso_manager.set_slot_clock(self.slot_clock());
>>>>>>> b0966d387d6d752e826e925de3f45b2d6fe12343
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
        // Share the operator's Hold/Auto TX-frequency mode so the adaptive
        // stall detector only emits `TxOffsetActionNeeded` in Auto (Hold keeps
        // the offset sticky).
        qso_manager.set_tx_freq_mode_source(tx_freq_mode.clone());
        // PAN-140 (Codex P1 on PR #369): share the real PTT-active flag so
        // `apply_tx_offset_switch`'s pre-commit recheck (the last-chance guard
        // against a stall/nudge action landing mid-transmission) reflects
        // genuine TX-worker state.
        qso_manager.set_ptt_active_source(ptt_active.clone());
        // PAN-143: share the synchronization gate too — see the field's doc
        // comment in qso_manager.rs.
        qso_manager.set_ptt_sync_gate_source(ptt_sync_gate.clone());
        qso_manager.set_relocation_epoch_source(relocation_epochs);
        // PAN-72 (Codex round 2 on PR #350, finding 5): share the global TX
        // policy too, so the same stall detector does not count a rearm cycle
        // whose re-send the `TxPolicy::Disabled` hard mute blocked outright —
        // the DX cannot answer a frame that never went on the air.
        qso_manager.set_tx_policy_source(tx_policy.clone());
        // PAN-72 (PR #350 round-7 re-review): share the LIVE active-slot-length
        // atomic so the keep-calling rearm gate and the `stall_cycles` counter
        // it drives run on the active protocol's transmit slot. This must be
        // the shared `Arc`, not `qso_config.active_mode`: `QsoManager::start()`
        // runs that loop on a `clone()`, `Clone` copies `config` by value, and
        // the `SetOperatingMode` handler below only mutates this task's own
        // binding — so a config-derived cadence never saw a Shift+M switch and
        // an FT4 → FT8 switch left the loop on a 7.5 s cadence for what was by
        // then a genuine FT8 QSO. `try_switch_operating_mode` rewrites this
        // atomic on every switch, so the running loop tracks it in both
        // directions.
        qso_manager.set_active_slot_ns_source(self.active_slot_ns());
        // PAN-128 PR review: the stall ledger's slot attribution uses the
        // scheduler's current-slot acceptance window — inject the operator's
        // real `tx_late_max_ms` so `keep_call_tx_slot` and `schedule_tx`
        // decide against the same cap. Share reload publication with the
        // TX worker and the manager's internal timeout clones.
        qso_manager.set_tx_late_max_ms(station_tx_late_max_ms);
        qso_manager.set_tx_late_max_ms_source(self.tx_late_max_ms_live.clone());
        // PAN-72 (Codex round 7 on PR #350, finding 3): the policy above is
        // only ONE of the two gates a rearmed frame must clear. A QSO with
        // `remote_origin` emits every frame as `TxOrigin::Remote`, and the TX
        // worker independently checks the station-agent arm immediately before
        // PTT — an arm expiry or explicit disarm drops those frames while
        // `TxPolicy` still reads `Full`, and the stall detector was counting
        // the resulting silence as "the DX ignored us".
        //
        // Read-only evidence, NOT a TX gate: the fail-CLOSED gate stays in the
        // TX worker (AGENTS.md invariant). `remote_tx_permitted` itself denies
        // on a poisoned lock, so a poisoned arm makes the stall detector stop
        // counting too — the conservative direction in both places.
        {
            let arm_for_stall_evidence = self.remote_tx_arm();
            qso_manager.set_remote_tx_permitted_source(std::sync::Arc::new(
                move |remote_client_key_id: Option<&str>| {
                    crate::coordinator::tx::remote_tx_permitted_for(
                        &arm_for_stall_evidence,
                        pancetta_agent::arm::ArmTime::now,
                        remote_client_key_id,
                    )
                },
            ));
        }
        // PAN-72 Fix E (Codex round 10, thread on `qso_manager.rs:6335`): the
        // stall detector's combined "frame reached the air" check above
        // accounts for the `TxPolicy` mute and the remote-arm gate, but not
        // the TX worker's own pre-PTT Hamlib-specific hard-mute reasons (a
        // Hamlib restart, the command loop not yet ready, an undelivered
        // pending frequency/split command, or an in-flight Hamlib command —
        // `tx_hard_mute_reason`, `coordinator/tx.rs`). During any of these a
        // "rearmed" frame never reaches the air, so it must not count as
        // stall evidence either — reuses the EXACT SAME atomics/handles that
        // function's own call sites already read for the TX worker's pre-PTT
        // check; no new state.
        {
            let tx_policy_for_hard_mute = self.tx_policy.clone();
            let tx_restart_inhibit_for_hard_mute = self.tx_restart_inhibit.clone();
            let hamlib_command_loop_ready_for_hard_mute = self.hamlib_command_loop_ready.clone();
            let hamlib_pending_frequency_for_hard_mute = self.hamlib_pending_frequency.clone();
            let hamlib_pending_split_for_hard_mute = self.hamlib_pending_split.clone();
            let hamlib_command_in_flight_for_hard_mute = self.hamlib_command_in_flight.clone();
            // PAN-115: a lost rig CAT link / dead audio output also keeps a
            // frame off the air.
            let rig_conn_state_for_hard_mute = self.rig_conn_state.clone();
            let audio_output_alive_for_hard_mute = self.audio_output_alive.clone();
            let receive_gain_for_hard_mute = self.message_bus.receive_gain.clone();
            let data_mode_for_hard_mute = self.message_bus.data_mode.clone();
            qso_manager.set_hamlib_hard_muted_source(std::sync::Arc::new(move || {
                crate::coordinator::tx::tx_hard_mute_reason(
                    &tx_policy_for_hard_mute,
                    &tx_restart_inhibit_for_hard_mute,
                    &hamlib_command_loop_ready_for_hard_mute,
                    &hamlib_pending_frequency_for_hard_mute,
                    &hamlib_pending_split_for_hard_mute,
                    &hamlib_command_in_flight_for_hard_mute,
                    &rig_conn_state_for_hard_mute,
                    &audio_output_alive_for_hard_mute,
                    &receive_gain_for_hard_mute,
                    &data_mode_for_hard_mute,
                )
                .is_some()
            }));
        }

        // The local logbook and upload receivers are taken before the manager
        // is published below. Both consumers start inside the spawned task,
        // the upload subscriber only after the startup index rebuild and
        // logger open. A QSO completed through the published manager in that
        // window waits in these receivers instead of missing the logbook of
        // record or the upload queue. Replay opens no logbook, so it takes no
        // ADIF receiver.
        let adif_events = (!replay_mode).then(|| qso_manager.subscribe());
        let upload_events = upload_enabled.then(|| qso_manager.subscribe());

        // PAN-382: queue completions before publishing the manager. The
        // emitter starts after local log setup, which can await disk I/O.
        let n1mm_subscription =
            n1mm_destination.map(|destination| (destination, qso_manager.subscribe()));

        // PAN-72 critical-finding fix: also publish the fresh handle onto
        // the watch channel so the (not respawned on a Qso-only restart)
        // Autonomous task's drain can pick up this brand-new manager
        // instead of staying pinned to whatever instance existed when it
        // spawned.
        //
        // This MUST come after every `set_*_source` call above, not before:
        // each setter does `self.field = new_arc`, reassigning a field on
        // `qso_manager` to one of the coordinator's own live shared `Arc`s.
        // `QsoManager::clone()` copies each field's `Arc` handle as it stood
        // at clone time -- cloning before the setters run publishes a
        // snapshot still holding whatever private default `Arc`s
        // `QsoManager::new()` created (e.g. `tx_freq_mode` defaults to
        // `Hold`), permanently disconnected from the coordinator's real
        // shared atomics, because reassigning the field on the original
        // `qso_manager` afterwards has no effect on a clone already taken.
        // Publishing here, after every source injection, is what makes the
        // clone the Autonomous task actually consumes carry the live state.
        //
        // `send_replace`, NOT `send`: tokio's `watch::Sender::send` bails out
        // with `Err` *without storing the value* whenever `receiver_count()`
        // is 0, and this store MUST land unconditionally:
        //   * On first startup `run()` awaits `start_qso_component` BEFORE
        //     `start_autonomous_component` (the only `.subscribe()` site), so
        //     there are genuinely zero receivers here -- a `send` would drop
        //     the very first handle on the floor and every later
        //     `subscribe()` would be seeded with `None`, leaving the whole
        //     mid-QSO offset-switch feature inert until the first Qso-component
        //     crash+restart.
        //   * The same applies to a Qso restart that lands in the window
        //     between the Autonomous task dying and its replacement
        //     re-subscribing.
        // `send_replace` has no receiver-count precondition: it always stores
        // (and notifies whatever receivers exist), which is exactly the
        // "latest handle is always readable via `subscribe()`" contract the
        // field's doc comment in mod.rs promises.
        let _ = self
            .qso_manager_watch
            .send_replace(Some(qso_manager.clone()));

        // Task 5 (QSOLogged/LoggedADIF): subscribe synchronously, before
        // `qso_manager` is moved into the spawned task below, so the WSJT-X
        // UDP component can pick this receiver straight off the coordinator
        // field with no channel or timeout involved.
        self.wsjtx_qso_events_rx = Some(qso_manager.subscribe());

        let qso_handle = {
            let shutdown = self.shutdown_signal.clone();
            // PAN-274: every Qso start (including a supervisor restart)
            // registers its ADIF subscriber here, so shutdown can wait for it.
            let adif_subscribers = self.adif_subscribers.clone();
            // Round-14 review (Codex P1): `maybe_auto_resend_73` needs this
            // to gate its own budget commit on identity-bound TX permission
            // — see the call site and that function's doc for why.
            let remote_tx_arm_for_auto73 = self.remote_tx_arm();

            // PAN-42: released immediately before the receiver loop below,
            // and by `Drop` on every earlier exit (startup error, panic,
            // abort), so the `--replay` feeder never starts while this task
            // is still seeding history -- and never waits forever either.
            let consumer_ready =
                super::replay::QsoConsumerReadyGuard::new(self.qso_consumer_ready.clone());
            // PAN-344: the per-QSO upload-status registry the upload
            // subscriber registers completions into.
            let qso_upload_statuses_for_subscriber = self.qso_upload_statuses.clone();

            let autonomous_cq_attempts = self.autonomous_cq_attempts.clone();
            let autonomous_config_enabled = self.autonomous_config_enabled.clone();
            // PAN-358: the queued-call TTL task wakes on slot-clock events.
            let ttl_clock_events = self.slot_clock_events();
            // PAN-358: queued manual calls are stamped and aged on the
            // coordinator's slot clock (PAN-289 age rule, `ClockStamp`).
            let queue_clock = self.slot_clock();
            tokio::spawn(async move {
                if let Err(e) = qso_manager.start().await {
                    error!("Failed to start QSO manager: {}", e);
                    return Err(anyhow::anyhow!("QSO manager startup failed"));
                }

                // `pancetta_home_override` is always `None` in production (see its
                // own doc comment) -- tests point it at an isolated temp dir so
                // the replay-history-seed read below (and, outside replay, the
                // real writers) never touch the operator's actual `~/.pancetta`.
                // PAN-376: otherwise the logbook lives in the data directory
                // (`--data-dir`, else `~/.pancetta`).
                let data_dir = super::data_dir_for(pancetta_home_override.as_deref())
                    .unwrap_or_else(pancetta_core::data_dir::current);

                // Rebuildable SQLite index at <data-dir>/qso.db.
                let db_path = data_dir.join("qso.db");

                // ADIF source of truth at <data-dir>/qsos.adi.
                let adif_path = data_dir.join("qsos.adi");

                // Both LOCAL source-of-truth writers (ADIF appender + SQLite
                // index) live behind one helper so the `--replay` suppression
                // is a single gate, testable without a whole coordinator. On a
                // live run it also brings the index in line with the ADIF log
                // before either writer opens a file, and hands back that index
                // for the worked-station seed below (PAN-127). Because the
                // rebuild runs inside this call before the logger opens
                // `qso.db`, the logger never holds a pooled connection on a
                // file the rebuild replaced (#453). Nothing can complete a QSO
                // before this point: the decode/command loops below have not
                // started yet.
                qso_lookup.reset_display_history();
                let (_adif_writer, _async_logger, live_seed_db) = start_local_qso_log_writers_with(
                    adif_events,
                    replay_mode,
                    &adif_path,
                    &db_path,
                    station_power_watts,
                    attribution_comment,
                    &database_settings,
                    &qso_manager,
                    &adif_subscribers,
                )
                .await;

                // Per-QSO log-upload subscriber (ClubLog + QRZ Logbook + cqdx.io
                // + eQSL + LoTW), with optional QRZ-XML grid enrichment applied
                // first. Opt-in: only spawned when at least one upload target OR
                // QRZ XML enrichment is enabled, and never under `--replay`
                // (see `upload_enabled`). Fully decoupled from the QSO
                // pipeline: ClubLog/QRZ/eQSL/cqdx.io uploads are handed to the
                // durable pending-upload queue (PAN-57), which retries them
                // and survives restarts, including LoTW. At shutdown it cancels
                // a QRZ lookup in flight
                // and keeps queuing completions until `stop_upload_outbox`
                // asks it to finish (PAN-232).
                if let Some(upload_events) = upload_events {
                    start_qso_upload_subscriber(
                        upload_outbox.clone(),
                        qrz_xml_cfg.clone(),
                        station_power_watts,
                        attribution_comment,
                        upload_events,
                        shutdown.clone(),
                        qso_lookup.clone(),
                        qrz_xml_base_url.clone(),
                        qso_upload_statuses_for_subscriber.clone(),
                    );
                }

                // PAN-382: N1MM Logger+ contactinfo UDP, one datagram per
                // `QsoCompleted`, built from the same ADIF record as
                // `qsos.adi`. `n1mm_destination` is `None` under `--replay`.
                // The receiver also holds completions from startup. The task
                // ends when this manager's event channel closes, never on the shutdown flag
                // (the PAN-232/PAN-274 rule), so a QSO that completes after
                // the flag is set is still broadcast. Shutdown does not wait
                // for it: a UDP send has nothing to flush.
                if let Some((destination, events)) = n1mm_subscription {
                    // Detached: the task ends on its own when the channel closes.
                    drop(super::n1mm_udp::start_n1mm_udp_emitter(
                        destination,
                        station_power_watts,
                        attribution_comment,
                        events,
                    ));
                }

                // History is initialized before decode/command loops or readiness.
                // Replay opens only an in-memory index and reads the canonical ADIF.
                let history_db = if replay_mode {
                    replay_history_db(&adif_path).await
                } else {
                    match live_seed_db {
                        LocalIndex::Ready(db) => Ok(Arc::new(db)),
                        LocalIndex::Unavailable { reason }
                        | LocalIndex::MigrationRefused { reason } => Err(
                            pancetta_qso::async_database::AsyncDatabaseError::IndexUnavailable {
                                reason,
                            },
                        ),
                    }
                };
                // PAN-268: the contest worker scores replay from the prepared
                // in-memory seed, never the real station index. Selected before
                // `history_db` is consumed by QSO-admission history below.
                let contest_history = super::contest::select_history_source(
                    replay_mode,
                    &history_db,
                    db_path.clone(),
                    adif_path.clone(),
                );
                // One sequential reader per QSO component; abort it on supervisor teardown.
                let _contest_worker = super::contest::start(
                    qso_manager.clone(),
                    message_bus.clone(),
                    contest_history,
                    shutdown.clone(),
                );
                let history_result = match history_db {
                    Ok(db) => initialize_qso_history(&mut qso_manager, &qso_lookup, db).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = history_result {
                    warn!(%error, "QSO history unavailable");
                    crate::coordinator::tx::emit_diagnostic_full(
                        &message_bus,
                        ComponentId::Qso,
                        "qso",
                        pancetta_core::DiagnosticLevel::Warn,
                        format!("QSO history unavailable: {error}"),
                        None,
                        None,
                    )
                    .await;
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
                // PAN-358: it also runs a pass as soon as the slot-clock
                // driver publishes a clock event, so a call queued before a
                // sleep or a forward step expires on wake, not up to 15 s later.
                spawn_queued_call_ttl(
                    pending_manual_calls.clone(),
                    message_bus.clone(),
                    queue_clock.clone(),
                    ttl_clock_events,
                    shutdown.clone(),
                );

                // PAN-78: the manual open handlers (message loop below) relay
                // capacity refusals to remote clients through the same gate;
                // cloned here because the event task shadows the original.
                let display_feed_for_admission = display_feed_enabled.clone();

                // PAN-238: only a registered opening has no accepted predecessor
                // that might still own the Autonomous outcome. Consume its marker
                // on the first MessageToSend, whether enqueue succeeds or fails.
                let opening_self_cqs =
                    Arc::new(std::sync::Mutex::new(std::collections::HashSet::<
                        pancetta_qso::QsoId,
                    >::new()));
                let opening_self_cqs_for_events = opening_self_cqs.clone();
                let autonomous_cq_attempts_for_events = autonomous_cq_attempts.clone();

                // Spawn a task to forward QSO auto-sequence TX requests to the transmitter
                // and update AP decoding state for the FT8 decoder thread.
                let mut qso_events = qso_manager.subscribe();
                let tx_bus = message_bus.clone();
                let tx_shutdown = shutdown.clone();
                let tx_callsign = our_callsign.clone();
                let ap_state = active_qso_ap;
                let qso_freq_state = active_qso_freq_hz;
                let qso_freq_slot_ns = qso_freq_slot_ns.clone();
                let active_tx_qsos = active_tx_qsos.clone();
                let fox_admitted_for_events = fox_admitted_qsos.clone();
                let active_tx_offsets = active_tx_offsets.clone();
                let latest_tx_intent = latest_tx_intent.clone();
                let snapshot_qso_manager = qso_manager.clone();
                let snapshot_bus = tx_bus.clone();
                let completions_for_events = recent_manual_completions.clone();
                let pending_for_events = pending_manual_calls.clone();
                let events_queue_clock = queue_clock.clone();
                let dx_activity_for_events = dx_activity.clone();
                let display_feed_enabled = display_feed_enabled.clone();
                let pending_qso_offset_requests = pending_qso_offset_requests.clone();
                // PAN-78: the capacity rule queued-call promotion applies
                // (same inputs as the manual open gates / auto-answer).
                let capacity_for_events = QsoCapacityLimits {
                    max_concurrent: auto_answer_max_concurrent,
                    fox_mode: fox_mode.clone(),
                    fox_max_streams: fox_max_streams.clone(),
                    fox_admitted_qsos: fox_admitted_qsos.clone(),
                    admission: qso_admission.clone(),
                };
                // PAN-275: one completed-QSO TX grace registrar, shared by the
                // `Completed` state change (PAN-280) and the decode loop's
                // auto-73 re-key. PAN-280: a key that reopens from
                // `Completed` or fails stands its pending purge down.
                let completed_tx_grace = CompletedTxGrace::new(
                    active_tx_qsos.clone(),
                    active_tx_offsets.clone(),
                    latest_tx_intent.clone(),
                    fox_admitted_qsos.clone(),
                    pancetta_qso::qso_manager::COMPLETED_QSO_REWORK_GRACE
                        .to_std()
                        .expect("COMPLETED_QSO_REWORK_GRACE is a positive literal duration"),
                    snapshot_qso_manager.clone(),
                    pending_for_events.clone(),
                    snapshot_bus.clone(),
                    capacity_for_events.clone(),
                    tx_shutdown.clone(),
                    queue_clock.clone(),
                );
                let tx_grace_for_events = completed_tx_grace.clone();
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
                                // immediately. A QSO entering `Completed` stays
                                // live for a grace window so its closing 73
                                // still goes out (PAN-280: here, not on
                                // `QsoCompleted`, which now waits for that 73 to
                                // air).
                                {
                                    let key = super::active_tx_qso_key(&qso_id.to_string());
                                    if new_state.is_active() {
                                        if matches!(
                                            old_state,
                                            pancetta_qso::QsoState::Completed { .. }
                                        ) {
                                            // PAN-280: the closing 73 was not sent and
                                            // the engine reopened the QSO. Stop the
                                            // completion's grace purge from removing
                                            // the key the reopened QSO needs.
                                            tx_grace_for_events.stand_down(&key);
                                            let call = new_state
                                                .their_callsign()
                                                .unwrap_or("the DX")
                                                .to_string();
                                            let text = format!(
                                                "73 to {call} was not sent; QSO not logged, still open"
                                            );
                                            emit_status(
                                                &snapshot_bus,
                                                text.clone(),
                                                pancetta_core::DiagnosticLevel::Warn,
                                            )
                                            .await;
                                            let _ = snapshot_bus
                                                .send_message(ComponentMessage::new(
                                                    ComponentId::Qso,
                                                    ComponentId::Tui,
                                                    MessageType::DiagnosticEvent {
                                                        target: "qso",
                                                        level: pancetta_core::DiagnosticLevel::Warn,
                                                        text,
                                                        qso_id: Some(qso_id.to_string()),
                                                        callsign: Some(call),
                                                    },
                                                    Instant::now(),
                                                ))
                                                .await;
                                        }
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
                                    } else if let pancetta_qso::QsoState::Completed {
                                        frequency,
                                        ..
                                    } = &new_state
                                    {
                                        // Keep the closing 73 live for the grace
                                        // window, then purge it and promote queued
                                        // calls (see `CompletedTxGrace` for why the
                                        // grace is 45 s). `keep_live` inserts the
                                        // key: a QSO opened directly at the close
                                        // never passed through an active state, so
                                        // without it its 73 would be dropped by the
                                        // Step 4b gate and never key PTT.
                                        tx_grace_for_events.keep_live(qso_id, *frequency);
                                    } else if let pancetta_qso::QsoState::Failed {
                                        reason, ..
                                    } = &new_state
                                    {
                                        autonomous_cq_attempts_for_events
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .retain(|(id, _)| id != &qso_id.to_string());
                                        // PAN-280: a pending grace purge must not act
                                        // on a key this immediate purge clears.
                                        tx_grace_for_events.stand_down(&key);
                                        opening_self_cqs_for_events
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .remove(&qso_id);
                                        if let Ok(mut set) = active_tx_qsos.write() {
                                            set.remove(&key);
                                        }
                                        // PAN-76: same removal point as `active_tx_qsos`.
                                        if let Ok(mut set) = fox_admitted_for_events.write() {
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
                                                &capacity_for_events,
                                                &tx_shutdown,
                                                &events_queue_clock,
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
                                    &decode_window_for_ap,
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
                                    // PAN-72 round-8 redesign (fix 2): the
                                    // primary hint keeps its byte-identical
                                    // derivation (partner_freq, else our own
                                    // TX frequency). The secondary hint
                                    // (`decoder_hint.secondary`) is the
                                    // still-in-grace vacated offset of an
                                    // unanswered `CallingCq` relocation — see
                                    // `secondary_decoder_hint_freq_for`.
                                    //
                                    // PAN-72 round-9 fix (finding 1): the
                                    // secondary now also carries its own
                                    // `expires_at` so the FT8 decode loop can
                                    // re-check freshness on every READ
                                    // instead of trusting a value latched
                                    // once at this write.
                                    let decoder_hint: Option<ActiveQsoFreq> = if new_state
                                        .is_active()
                                    {
                                        // Fetch the QSO's current progress
                                        // once. This is a cheap read-lock
                                        // on the already-updated QSO map;
                                        // it fires once per state-change
                                        // (not per decode window). On
                                        // error (QSO vanished between the
                                        // event and the lookup — extremely
                                        // rare) we fall back to the
                                        // state's own TX frequency and no
                                        // secondary.
                                        let progress =
                                            snapshot_qso_manager.get_qso(qso_id).await.ok();
                                        let partner =
                                            progress.as_ref().and_then(|p| p.metadata.partner_freq);
                                        let primary = partner.or_else(|| new_state.frequency());
                                        let secondary = progress.as_ref().and_then(|p| {
                                            secondary_decoder_hint_freq_for(
                                                p,
                                                qso_freq_slot_ns.load(Ordering::Relaxed),
                                            )
                                        });
                                        primary.map(|primary_hz| ActiveQsoFreq {
                                            primary_hz,
                                            secondary,
                                        })
                                    } else {
                                        None
                                    };
                                    // Acquire the guard synchronously (no await
                                    // in this scope) and write.
                                    if let Ok(mut guard) = qso_freq_state.write() {
                                        *guard = decoder_hint;
                                    }
                                }

                                // Push an updated snapshot of in-progress
                                // QSOs to the TUI banner. The QSO state
                                // machine is the source of truth; the TUI
                                // replaces its list each push.
                                push_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                    &display_feed_enabled,
                                    &snapshot_bus,
                                    &events_queue_clock,
                                )
                                .await;

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
                                        emit_status(
                                            &snapshot_bus,
                                            text,
                                            pancetta_core::DiagnosticLevel::Warn,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::MessageToSend {
                                attempt_id,
                                qso_id,
                                message,
                                frequency,
                                tx_parity,
                                remote_origin,
                                remote_client_key_id,
                            }) => {
                                let is_opening_self_cq = opening_self_cqs_for_events
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .remove(&qso_id);
                                match pancetta_qso::utils::generate_ft8_message(
                                    &message,
                                    &tx_callsign,
                                ) {
                                    Ok(text) => {
                                        info!(
                                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={}, tx_parity={:?})",
                                            text, frequency, qso_id, tx_parity
                                        );
                                        // SECURITY: a remote-initiated QSO's TX MUST be
                                        // `TxOrigin::Remote` so the armed-TX gate applies;
                                        // a local QSO stays `Local` (byte-identical).
                                        let tx_origin = if remote_origin {
                                            crate::message_bus::TxOrigin::Remote
                                        } else {
                                            crate::message_bus::TxOrigin::Local
                                        };
                                        // Record this as the newest intent for the QSO so the
                                        // TX worker can pivot to it at key-time if it arrives
                                        // while an earlier frame for the same QSO is still in
                                        // the worker's pre-PTT wait. Round-4 review (Codex
                                        // P1): `origin`/`remote_client_key_id` travel WITH the
                                        // payload here — see `LatestTxIntent`'s doc — computed
                                        // from the SAME `remote_origin`/`remote_client_key_id`
                                        // this exact event carries, so the intent can never
                                        // disagree with the `TransmitRequest` sent below.
                                        //
                                        // PAN-185 (Codex 4244602501, the publish-before-send
                                        // window): publish the intent as not queued, send the
                                        // request checked, then settle: mark it queued once
                                        // the bus accepted the request, withdraw it if not.
                                        // Pivots ignore unqueued intents, so a pivot never
                                        // targets a request the TX worker will never see.
                                        let key = super::active_tx_qso_key(&qso_id.to_string());
                                        let published = super::LatestTxIntent {
                                            attempt_id,
                                            message_text: text.clone(),
                                            frequency_offset: frequency,
                                            tx_parity,
                                            origin: tx_origin,
                                            remote_client_key_id: remote_client_key_id.clone(),
                                            request_queued: false,
                                        };
                                        if let Ok(mut m) = latest_tx_intent.write() {
                                            m.insert(key.clone(), published.clone());
                                        }
                                        if is_opening_self_cq {
                                            autonomous_cq_attempts_for_events
                                                .lock()
                                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                                .insert((qso_id.to_string(), attempt_id));
                                        }
                                        let tx_msg = ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                attempt_id,
                                                message_text: text.clone(),
                                                frequency_offset: frequency,
                                                qso_id: Some(qso_id.to_string()),
                                                tx_parity,
                                                origin: tx_origin,
                                                remote_client_key_id,
                                            },
                                            Instant::now(),
                                        );
                                        let queued = matches!(
                                            tx_bus.send_message_checked(tx_msg).await,
                                            Ok(true)
                                        );
                                        super::settle_tx_intent(
                                            &latest_tx_intent,
                                            &key,
                                            &published,
                                            queued,
                                        );
                                        if !queued {
                                            autonomous_cq_attempts_for_events
                                                .lock()
                                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                                .remove(&(qso_id.to_string(), attempt_id));
                                            warn!(
                                                "auto-sequence TX for QSO {qso_id} never reached the TX worker; withdrawing its intent and reporting it unsent"
                                            );
                                            // Self-addressed: the Qso loop's own handler rolls
                                            // back this attempt by id (a no-op for an opening
                                            // or advance, PAN-183). No worker received an
                                            // opening self-CQ, so no worker outcome can settle
                                            // its registration: Autonomous hears it here. A
                                            // rejected resend sends Autonomous nothing
                                            // (PAN-185 D8, PAN-238). One settlement path
                                            // (PAN-279).
                                            super::tx_outcome::settle_unaired_frame(
                                                &tx_bus,
                                                ComponentId::Qso,
                                                super::tx_outcome::TxFrame {
                                                    qso_id: Some(qso_id.to_string()),
                                                    message_text: text,
                                                    attempt_id,
                                                },
                                                None,
                                                if is_opening_self_cq {
                                                    super::tx_outcome::AutonomousOutcome::Report
                                                } else {
                                                    super::tx_outcome::AutonomousOutcome::Withheld
                                                },
                                            )
                                            .await;
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
                                if !upload_enabled {
                                    qso_upload_statuses_for_subscriber
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .register(
                                            &qso_id.to_string(),
                                            metadata.their_callsign.as_deref().unwrap_or("?"),
                                            vec![],
                                        );
                                }
                                if let Some(callsign) = &metadata.their_callsign {
                                    let _ = snapshot_bus
                                        .send_message_durable(ComponentMessage::new(
                                            ComponentId::Qso,
                                            ComponentId::Autonomous,
                                            MessageType::WatchlistEdit {
                                                callsign: callsign.clone(),
                                                action: crate::message_bus::WatchlistAction::Unpin,
                                            },
                                            Instant::now(),
                                        ))
                                        .await;
                                }
                                opening_self_cqs_for_events
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .remove(&qso_id);
                                // PAN-280: the closing 73's TX liveness and its
                                // grace purge (`CompletedTxGrace::keep_live`)
                                // moved to the `Completed` state change: this
                                // event now fires only after that 73 aired.
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
                                    &decode_window_for_ap,
                                    &ap_state,
                                )
                                .await;
                                // hb-091: also clear the partner freq.
                                if let Ok(mut guard) = qso_freq_state.write() {
                                    *guard = None;
                                }
                                // Push fresh snapshot so the banner drops
                                // the just-completed QSO from the active list.
                                push_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                    &display_feed_enabled,
                                    &snapshot_bus,
                                    &events_queue_clock,
                                )
                                .await;
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
                                            completed_at: UtcTimestamp::now(),
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
                                    // The typed completion toast owns the visual slot.
                                    // Keep the diagnostic below for counters and history.
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

                                    // PAN-94: record before the outcome below, so
                                    // the TUI relay's rescore on it sees this contact.
                                    qso_lookup.record_completed_contact(&metadata);

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

                                    // item-2-auto-73: stash MANUAL completions so
                                    // that if this DX keeps re-sending RR73/RRR (they
                                    // didn't copy our 73) we can auto-re-send our 73,
                                    // bounded, from the decode-processing loop below.
                                    // Autonomous completions are deliberately NOT
                                    // stashed — that path has its own dx-busy /
                                    // duplicate gates and shouldn't keep TXing 73s.
                                    // PAN-259: neither are Hound completions — a
                                    // Hound never sends 73 after the Fox's RR73.
                                    if stashes_auto_73(&metadata) {
                                        let now = UtcTimestamp::now();
                                        let entry = RecentManualCompletion {
                                            completed_at: now,
                                            resends: 0,
                                            last_resend_at: None,
                                            remote_origin: metadata.remote_origin,
                                            remote_client_key_id: metadata
                                                .remote_client_key_id
                                                .clone(),
                                            qso_id,
                                            pending_resends: Vec::new(),
                                        };
                                        let mut map = completions_for_events.lock().await;
                                        // Prunes stale entries while we hold the lock
                                        // so the map never grows unbounded, and keeps
                                        // the counters of a repeated completion for
                                        // the same QSO (PAN-275).
                                        stash_auto_73_completion(&mut map, their_call, entry, now);
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
                                opening_self_cqs_for_events
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .remove(&qso_id);
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
                                    // PAN-76: same removal point as `active_tx_qsos`.
                                    if let Ok(mut set) = fox_admitted_for_events.write() {
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
                                        &capacity_for_events,
                                        &tx_shutdown,
                                        &events_queue_clock,
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
                                    &decode_window_for_ap,
                                    &ap_state,
                                )
                                .await;
                                // Push fresh snapshot so the banner drops
                                // the failed QSO.
                                push_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                    &display_feed_enabled,
                                    &snapshot_bus,
                                    &events_queue_clock,
                                )
                                .await;
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
                                            completed_at: UtcTimestamp::now(),
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

                                // PAN-94: record before the outcome below, so
                                // the TUI relay's rescore on it sees this failure.
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!(
                                        "QSO failed with {}: {} (adding backoff)",
                                        their_call, reason_text
                                    );
                                    qso_lookup.record_failure(their_call);
                                } else {
                                    info!("QSO failed: {}", reason_text);
                                }

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
                            Ok(pancetta_qso::QsoEvent::TxOffsetActionNeeded {
                                qso_id,
                                action,
                                raised_at_generation,
                            }) => {
                                if let Ok(mut pending) = pending_qso_offset_requests.lock() {
                                    pending.push(
                                        pancetta_qso::qso_manager::OffsetActionRequest::stall_detected(
                                            qso_id,
                                            action,
                                            raised_at_generation,
                                        ),
                                    );
                                }
                            }
                            Ok(pancetta_qso::QsoEvent::TxOffsetApplied { qso_id, offset_hz }) => {
                                // PAN-72 (Codex round 1 on PR #350, finding 7):
                                // an applied offset move is not a state
                                // transition, so nothing else in this task
                                // rebuilds the `ActiveQsosSnapshot` the TUI
                                // banner and the TX-placement stream marker
                                // render from. A stalled exchange is exactly
                                // where no further transition arrives, so
                                // without this push the UI keeps showing the
                                // pre-switch offset through several
                                // switch/revert cycles. The coordinator-side
                                // `active_tx_offsets` mirror is written by the
                                // Autonomous drain itself (it needs the value
                                // in time for the same tick's
                                // `set_own_frequencies`); this is the UI half.
                                debug!(
                                    target: "tx.freq",
                                    qso_id = %qso_id,
                                    offset_hz,
                                    "PAN-72: refreshing the active-QSO snapshot after an offset move"
                                );
                                // PAN-72 (Codex round 7 on PR #350, finding 6):
                                // re-centre the DECODER too, not just the UI.
                                // With the QSO filter on (the default) the main
                                // decode is collapsed to ±60 Hz around
                                // `active_qso_freq_hz`, and a relocation is at
                                // least `min_separation_hz` (75 Hz) away — so an
                                // unanswered CQ that switched offset could no
                                // longer hear an answer on its NEW frequency,
                                // and with no decode there was no StateChanged
                                // to refresh the hint either. Self-sustaining
                                // deafness, for the life of the QSO.
                                //
                                // Resolve the value BEFORE taking the std
                                // RwLock guard so no non-Send guard is held
                                // across an await, exactly as the StateChanged
                                // arm does. A QSO that has gone terminal in the
                                // meantime yields `None` and the hint is left
                                // ALONE — the terminal arms own that clear, and
                                // a co-active QSO's hint must not be wiped by a
                                // late event for a dead one.
                                {
                                    // PAN-72 round-8 redesign (fix 2): pair
                                    // the primary hint (unchanged derivation)
                                    // with the still-in-grace secondary — the
                                    // vacated offset of an unanswered
                                    // `CallingCq` relocation.
                                    let progress = snapshot_qso_manager.get_qso(qso_id).await.ok();
                                    let hint = progress.as_ref().and_then(decoder_hint_freq_for);
                                    if let Some(hint_hz) = hint {
                                        let secondary = progress.as_ref().and_then(|p| {
                                            secondary_decoder_hint_freq_for(
                                                p,
                                                qso_freq_slot_ns.load(Ordering::Relaxed),
                                            )
                                        });
                                        if let Ok(mut guard) = qso_freq_state.write() {
                                            *guard = Some(ActiveQsoFreq {
                                                primary_hz: hint_hz,
                                                secondary,
                                            });
                                        }
                                    }
                                }
                                push_active_qso_snapshot(
                                    &snapshot_qso_manager,
                                    &dx_activity_for_events,
                                    &pending_for_events,
                                    &display_feed_enabled,
                                    &snapshot_bus,
                                    &events_queue_clock,
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

                // PAN-42: the receiver loop starts here, so the replay
                // feeder may start feeding now (the history seed above has
                // fully completed by this point).
                consumer_ready.release();

                while !shutdown.load(Ordering::Acquire) {
                    match qso_inbox.try_recv() {
                        Ok(message) => {
                            match message.message_type {
                                // Decoded FT8 messages forwarded from the decoder
                                MessageType::DecodedMessage(ref decoded_msg) => {
                                    // PAN-69: a decode whose window closed on a
                                    // since-left band must not create or advance
                                    // a QSO — it would be answered on the rig's
                                    // CURRENT band, against a station never
                                    // heard there. Drop before any of the arm
                                    // body (auto-73, process_message, dedup,
                                    // maybe_answer_caller, dx-activity record).
                                    if super::is_stale_band_decode(
                                        decoded_msg.captured_dial_hz,
                                        operating_frequency_hz.load(Ordering::Relaxed),
                                    ) {
                                        debug!(
                                            target: "qso",
                                            "Dropping decode captured on a prior band (band switch happened mid-decode)"
                                        );
                                    } else {
                                        let raw_text = decoded_msg.text.clone();
                                        let frequency = decoded_msg.frequency_offset;
                                        let snr = decoded_msg.snr_db;

                                        // PAN-51: classify directly from the decoder's own
                                        // typed Ft8Message.standard_type instead of
                                        // re-parsing the rendered text — see
                                        // ft8_message_to_qso_type's doc comment.
                                        //
                                        // PAN-259: a DXpedition (type 0.1) frame
                                        // carries two contacts; each runs through
                                        // the same per-contact processing below.
                                        let mut msg_types = ft8_message_to_qso_types(
                                            &decoded_msg.message,
                                            &raw_text,
                                            &our_callsign,
                                        );
                                        // Process the contact addressed to us
                                        // last, so `record_dx_activity` leaves
                                        // the Fox shown as working us.
                                        msg_types.sort_by_key(|m| {
                                            m.addressee_callsign().is_some_and(|to| {
                                                pancetta_qso::exchange::callsigns_match(
                                                    to,
                                                    &our_callsign,
                                                )
                                            })
                                        });
                                        let is_dxpedition_frame =
                                            decoded_msg.message.dxpedition.is_some();
                                        for msg_type in msg_types {
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
                                                &qso_manager,
                                                &recent_manual_completions,
                                                &tx_policy,
                                                &message_bus,
                                                &remote_tx_arm_for_auto73,
                                                &completed_tx_grace,
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
                                            // policy / presence / parity / capacity.
                                            //
                                            // Skip if we already attempted creation
                                            // for this station in this slot (Part B
                                            // of duplicate-QSO fix).
                                            //
                                            // PAN-259: a Fox's DXpedition frame never
                                            // opens a caller QSO; it only advances
                                            // a Hound QSO that already exists.
                                            let skip_creation = is_dxpedition_frame
                                                || caller_base.as_deref().is_some_and(|base| {
                                                    caller_dedup.1.contains(base)
                                                });

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
                                                let _admission = qso_admission.lock().await;
                                                maybe_answer_caller(
                                                    &msg_type,
                                                    &our_callsign,
                                                    frequency,
                                                    decoded_msg.slot_parity,
                                                    snr,
                                                    &qso_manager,
                                                    &tx_policy,
                                                    unattended_policy,
                                                    super::operator_present_now(
                                                        &last_operator_input_ms,
                                                    ),
                                                    auto_answer_max_concurrent,
                                                    &message_bus,
                                                    &fox_mode,
                                                    &fox_max_streams,
                                                    &fox_admitted_qsos,
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
                                                UtcTimestamp::now(),
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
                                            remote_client_key_id,
                                            dx_offset_unknown,
                                        } => {
                                            // PAN-287: a network-spot call carries no parity,
                                            // so the parity gate always admits it (never queued).
                                            debug_assert!(!dx_offset_unknown || dx_parity.is_none());
                                            // PAN-78: capacity/parity checks and the open run
                                            // under the admission lock.
                                            let _admission = qso_admission.lock().await;
                                            // PAN-23 round-2 (Codex review of #283): reject the
                                            // unresolved-hash placeholder "<...>" at ADMISSION
                                            // time, before it can ever be queued. Same bug class
                                            // as the RespondToCaller fix just above (and the
                                            // same fix shape): a cross-parity StartQso — e.g. a
                                            // remote `callStation` request, whose callsign and
                                            // parity are accepted without validation upstream —
                                            // queues into `pending_manual_calls` FIRST (below)
                                            // and only reaches the `qso_manager` guard later, at
                                            // promotion time via `promote_pending_manual_calls`.
                                            // Since the InvalidCallsign rejection is
                                            // deterministic, letting it reach that generic
                                            // queue-on-failure path would re-queue it at the
                                            // front on every promotion attempt, repeatedly
                                            // holding the parity slot against legitimate
                                            // opposite-parity calls for up to the full
                                            // 10-minute queue TTL. Reject it here instead,
                                            // before it ever occupies a slot.
                                            if callsign == "<...>" {
                                                warn!(
                                                    target: "qso.security",
                                                    "Refusing StartQso for the unresolved hash \
                                                     placeholder \"<...>\"",
                                                );
                                                crate::coordinator::tx::emit_diagnostic_full(
                                                    &message_bus,
                                                    ComponentId::Qso,
                                                    "qso.security",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    "Refusing StartQso for the unresolved hash \
                                                     placeholder \"<...>\" — it carries no \
                                                     identity information and can never be \
                                                     transmitted"
                                                        .to_string(),
                                                    None,
                                                    Some(&callsign),
                                                )
                                                .await;
                                                continue;
                                            }
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
                                                let dup_pos = q.iter().position(|p| {
                                                    p.callsign.eq_ignore_ascii_case(&callsign)
                                                });
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
                                                match dup_pos {
                                                    // Round-4 review (Codex P2): a repeat
                                                    // call for the same callsign while it's
                                                    // still deferred must REPLACE the stored
                                                    // request — including origin/identity —
                                                    // not be silently dropped. Before this
                                                    // fix, if client A's call was deferred
                                                    // and client B later repeated it, the
                                                    // dup check discarded B's request
                                                    // outright: promotion still admitted the
                                                    // stale A-bound entry, which B's own
                                                    // valid arm then rejected.
                                                    Some(pos) => {
                                                        q[pos] = PendingManualCall {
                                                            callsign: callsign.clone(),
                                                            frequency_hz: frequency as f64,
                                                            dx_parity,
                                                            queued_at: queue_clock.stamp(),
                                                            hound: false,
                                                            fox_freq_hz: None,
                                                            fox_grid: None,
                                                            held_hz: queued_held,
                                                            hold_mode: queued_hold_mode,
                                                            remote_origin,
                                                            remote_client_key_id,
                                                            step: pancetta_core::ResponseStep::Grid,
                                                            our_snr_of_them: None,
                                                            their_report: None,
                                                        };
                                                    }
                                                    None => {
                                                        if q.len() >= MAX_PENDING_MANUAL_CALLS {
                                                            q.pop_front();
                                                        }
                                                        q.push_back(PendingManualCall {
                                                            callsign: callsign.clone(),
                                                            frequency_hz: frequency as f64,
                                                            dx_parity,
                                                            queued_at: queue_clock.stamp(),
                                                            hound: false,
                                                            fox_freq_hz: None,
                                                            fox_grid: None,
                                                            held_hz: queued_held,
                                                            hold_mode: queued_hold_mode,
                                                            remote_origin,
                                                            remote_client_key_id,
                                                            step: pancetta_core::ResponseStep::Grid,
                                                            our_snr_of_them: None,
                                                            their_report: None,
                                                        });
                                                    }
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
                                                    ), pancetta_core::DiagnosticLevel::Info)
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
                                            // PAN-287: a network-spot call with no known DX
                                            // offset opens at the allocator's TX offset with a
                                            // placeholder partner_freq; every other call is
                                            // unchanged.
                                            let (tx_off, partner) = if dx_offset_unknown {
                                                compute_spot_call_tx_offset(dx_freq, &active)
                                            } else {
                                                compute_manual_tx_offset(
                                                    dx_freq, hold_mode, held, &active,
                                                )
                                            };
                                            if tx_off != dx_freq {
                                                info!(
                                                    target: "qso",
                                                    "TX offset: held={} hold_mode={} dx_freq={:.0} \
                                                     → tx_off={:.0} Hz (de-conflicted from {} active)",
                                                    held, hold_mode, dx_freq, tx_off, active.len()
                                                );
                                            }
                                            // PAN-78: capacity gate. Refuse (never queue) a manual call
                                            // that would ADD an active QSO while `max_concurrent_qsos`
                                            // are already active — the TX cap only sees requests drained
                                            // in the same cycle, so an idle active QSO is invisible to
                                            // it. Re-calls / same-call supersedes add nothing and pass.
                                            // Runs after the parity gate, so a cross-parity call still
                                            // queues; promotion applies the cap when it starts.
                                            if qso_manager
                                                .manual_open_adds_active_qso(
                                                    &callsign,
                                                    tx_off,
                                                    pancetta_core::ResponseStep::Grid,
                                                )
                                                .await
                                            {
                                                if let Some(snap) = manual_open_at_capacity(
                                                    &qso_manager,
                                                    auto_answer_max_concurrent,
                                                    &fox_mode,
                                                    &fox_max_streams,
                                                    &fox_admitted_qsos,
                                                )
                                                .await
                                                {
                                                    refuse_manual_open_at_capacity(
                                                        &message_bus,
                                                        &display_feed_for_admission,
                                                        ManualOpen::Call,
                                                        Some(&callsign),
                                                        snap,
                                                        remote_origin,
                                                    )
                                                    .await;
                                                    continue;
                                                }
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
                                                    remote_client_key_id,
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
                                                        ), pancetta_core::DiagnosticLevel::Info)
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!(
                                                        "Failed to start QSO with {}: {}",
                                                        callsign, e
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Call {} failed: {}", callsign, e), pancetta_core::DiagnosticLevel::Warn)
                                                    .await;
                                                }
                                            }
                                        }
                                        crate::message_bus::QsoMessage::StartAutonomousQso {
                                            callsign,
                                            frequency,
                                            parity,
                                            cq_attempt_id,
                                        } => {
                                            // Recheck a queued opening after any reload while it waited.
                                            if !autonomous_config_enabled.load(Ordering::Acquire) {
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus, &callsign, cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
                                                ).await;
                                                continue;
                                            }
                                            // PAN-78: the autonomous task gated on its own view of
                                            // the active count before sending this; a manual open
                                            // may have filled the slot since. Re-check capacity and
                                            // parity and open under the shared admission lock.
                                            let _admission = qso_admission.lock().await;
                                            if !autonomous_config_enabled.load(Ordering::Acquire) {
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus, &callsign, cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
                                                ).await;
                                                continue;
                                            }
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
                                                // PAN-38 round 1: this self-CQ
                                                // never reached `start_cq` at
                                                // all (deferred here, before
                                                // dispatch) -- no QSO or
                                                // transmission was created,
                                                // but the streak/offset
                                                // `decide_at` mutated
                                                // speculatively for this
                                                // attempt is still live and
                                                // must be rolled back the
                                                // same way a downstream
                                                // failure is.
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus,
                                                    &callsign,
                                                    cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
                                                )
                                                .await;
                                                continue;
                                            }
                                            if let Some(snap) = manual_open_at_capacity(
                                                &qso_manager,
                                                auto_answer_max_concurrent,
                                                &fox_mode,
                                                &fox_max_streams,
                                                &fox_admitted_qsos,
                                            )
                                            .await
                                            {
                                                info!(
                                                    target: "qso.autonomous",
                                                    "Skipping autonomous QSO ({:?}) — at capacity {}/{}",
                                                    callsign, snap.active, snap.cap
                                                );
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus,
                                                    &callsign,
                                                    cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
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
                                            if !autonomous_config_enabled.load(Ordering::Acquire) {
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus, &callsign, cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
                                                ).await;
                                                continue;
                                            }
                                                    qso_manager
                                                        .respond_to_cq_with(
                                                            dx.clone(),
                                                            tx_off,
                                                            parity,
                                                            pancetta_qso::CallInitiation::Auto,
                                                            partner,
                                                            false,
                                                            None,
                                                        )
                                                        .await
                                                }
                                                None => {
                                                    // Calling CQ ourselves: `parity` is our TX
                                                    // parity (not a DX parity). Autonomous CQ is
                                                    // a LOCAL initiation, never remote.
                                                    //
                                                    // PAN-38 round 2 (Codex): pre-generate the
                                                    // qso_id and register the
                                                    // qso_id<->cq_attempt_id association
                                                    // (AutonomousCqOpened) BEFORE dispatching
                                                    // start_cq_with_id, not after it returns --
                                                    // start_cq's own MessageToSend becomes
                                                    // visible to the independently-scheduled
                                                    // event-forwarding task as soon as it's
                                                    // emitted, and a same-instant downstream
                                                    // failure's TransmitComplete could otherwise
                                                    // reach the autonomous task before this
                                                    // association did.
                                                    let pre_generated_qso_id =
                                                        pancetta_qso::QsoId::new_v4();
                                                    // PAN-45 (Codex round 6 on PR #301): see
                                                    // `register_autonomous_cq_open_or_decline`'s
                                                    // doc comment -- a dropped registration
                                                    // means we must decline to dispatch
                                                    // rather than proceed with an
                                                    // unassociated QSO.
                                                    if register_autonomous_cq_open_or_decline(
                                                        &message_bus,
                                                        pre_generated_qso_id,
                                                        cq_attempt_id,
                                                    )
                                                    .await
                                                    {
                                            if !autonomous_config_enabled.load(Ordering::Acquire) {
                                                notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                    &message_bus, &callsign, cq_attempt_id,
                                                    &pending_autonomous_cq_dispatch_failures,
                                                ).await;
                                                continue;
                                            }
                                                        if cq_attempt_id.is_some() {
                                                            opening_self_cqs
                                                                .lock()
                                                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                                                .insert(pre_generated_qso_id);
                                                        }
                                                        let started = qso_manager
                                                            .start_cq_with_id(
                                                                pre_generated_qso_id,
                                                                frequency,
                                                                parity,
                                                                false,
                                                                None,
                                                            )
                                                            .await;
                                                        if started.is_err() {
                                                            opening_self_cqs
                                                                .lock()
                                                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                                                .remove(&pre_generated_qso_id);
                                                        }
                                                        started
                                                    } else {
                                                        Err(pancetta_qso::QsoManagerError::Internal {
                                                            message: format!(
                                                                "AutonomousCqOpened registration for attempt {:?} \
                                                                 was not delivered; declining to dispatch the \
                                                                 self-CQ",
                                                                cq_attempt_id
                                                            ),
                                                        })
                                                    }
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
                                                    None => {
                                                        // PAN-38 round 2: the
                                                        // AutonomousCqOpened
                                                        // registration now
                                                        // happens BEFORE
                                                        // dispatch, above --
                                                        // see the comment
                                                        // there.
                                                        info!(
                                                            target: "qso.autonomous",
                                                            "Autonomous CQ QSO opened on {:.0} Hz: {}",
                                                            frequency, qso_id
                                                        );
                                                    }
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
                                                    // PAN-38: a self-CQ
                                                    // (`callsign: None`) that
                                                    // failed downstream never
                                                    // actually transmitted —
                                                    // tell the autonomous
                                                    // operator so it rolls
                                                    // back the streak/offset
                                                    // it mutated speculatively
                                                    // for this attempt.
                                                    notify_autonomous_cq_dispatch_failed_if_self_cq(
                                                        &message_bus,
                                                        &callsign,
                                                        cq_attempt_id,
                                                        &pending_autonomous_cq_dispatch_failures,
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
                                            // PAN-78: capacity/parity checks and the open run
                                            // under the admission lock.
                                            let _admission = qso_admission.lock().await;
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
                                                let dup_pos = q.iter().position(|p| {
                                                    p.callsign.eq_ignore_ascii_case(&callsign)
                                                });
                                                if let Some(pos) = dup_pos {
                                                    // PAN-289: a repeat engage restarts the
                                                    // wait, as a repeat StartQso does by
                                                    // replacing its entry. Otherwise a
                                                    // re-engage after wake would keep the
                                                    // pre-sleep stamp and be expired.
                                                    q[pos].queued_at = queue_clock.stamp();
                                                } else {
                                                    if q.len() >= MAX_PENDING_MANUAL_CALLS {
                                                        q.pop_front();
                                                    }
                                                    q.push_back(PendingManualCall {
                                                        callsign: callsign.clone(),
                                                        frequency_hz: fox_freq as f64,
                                                        dx_parity,
                                                        queued_at: queue_clock.stamp(),
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
                                                        remote_client_key_id: None,
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
                                                    ), pancetta_core::DiagnosticLevel::Info)
                                                .await;
                                                continue;
                                            }
                                            // PAN-78: capacity gate. Only an engage that would ADD an
                                            // active QSO is refused: `engage_hound` is a manual
                                            // `respond_to_cq_with`, so pressing Shift+H again on a Fox
                                            // already being worked continues or replaces that QSO.
                                            // EngageHound carries no remote origin.
                                            if qso_manager.engage_hound_adds_active_qso(&callsign).await {
                                                if let Some(snap) = manual_open_at_capacity(
                                                    &qso_manager,
                                                    auto_answer_max_concurrent,
                                                    &fox_mode,
                                                    &fox_max_streams,
                                                    &fox_admitted_qsos,
                                                )
                                                .await
                                                {
                                                    refuse_manual_open_at_capacity(
                                                        &message_bus,
                                                        &display_feed_for_admission,
                                                        ManualOpen::Hound,
                                                        Some(&callsign),
                                                        snap,
                                                        false,
                                                    )
                                                    .await;
                                                    continue;
                                                }
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
                                                         (calling region, keep-calling under watchdog)",
                                                        callsign, qso_id
                                                    );
                                                    emit_status(
                                                        &message_bus,
                                                        format!(
                                                            "Hound: calling Fox {} — TX \
                                                             queued ({} Hz RX)",
                                                            callsign, fox_freq
                                                        ), pancetta_core::DiagnosticLevel::Info)
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
                                                        ), pancetta_core::DiagnosticLevel::Warn)
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
                                            remote_client_key_id,
                                        } => {
                                            // PAN-78: capacity/parity checks and the open run
                                            // under the admission lock.
                                            let _admission = qso_admission.lock().await;
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
                                                let dup_pos = q.iter().position(|p| {
                                                    p.callsign.eq_ignore_ascii_case(&callsign)
                                                });
                                                let queued_held =
                                                    tx_offset_hold_hz.load(Ordering::Relaxed);
                                                let queued_hold_mode =
                                                    pancetta_core::TxFreqMode::from_u8(
                                                        tx_freq_mode.load(Ordering::Relaxed),
                                                    ) == pancetta_core::TxFreqMode::Hold;
                                                // Round-4 review (Codex P2): same fix as the
                                                // StartQso deferred queue above — a repeat
                                                // caller-response for the same callsign must
                                                // REPLACE the stored request (origin/identity
                                                // included), not be silently dropped.
                                                match dup_pos {
                                                    Some(pos) => {
                                                        q[pos] = PendingManualCall {
                                                            callsign: callsign.clone(),
                                                            frequency_hz: frequency as f64,
                                                            dx_parity,
                                                            queued_at: queue_clock.stamp(),
                                                            hound: false,
                                                            fox_freq_hz: None,
                                                            fox_grid: None,
                                                            held_hz: queued_held,
                                                            hold_mode: queued_hold_mode,
                                                            remote_origin,
                                                            remote_client_key_id,
                                                            step,
                                                            our_snr_of_them: snr,
                                                            their_report: None,
                                                        };
                                                    }
                                                    None => {
                                                        if q.len() >= MAX_PENDING_MANUAL_CALLS {
                                                            q.pop_front();
                                                        }
                                                        q.push_back(PendingManualCall {
                                                            callsign: callsign.clone(),
                                                            frequency_hz: frequency as f64,
                                                            dx_parity,
                                                            queued_at: queue_clock.stamp(),
                                                            hound: false,
                                                            fox_freq_hz: None,
                                                            fox_grid: None,
                                                            held_hz: queued_held,
                                                            hold_mode: queued_hold_mode,
                                                            remote_origin,
                                                            remote_client_key_id,
                                                            step,
                                                            our_snr_of_them: snr,
                                                            // The immediate path below always
                                                            // passes `None` (the engine defaults
                                                            // it); match that here too.
                                                            their_report: None,
                                                        });
                                                    }
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
                                                    ), pancetta_core::DiagnosticLevel::Info)
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
                                            // PAN-78: capacity gate — same rule as StartQso. A reply
                                            // that continues/re-keys an existing QSO (or `SeventyThree`,
                                            // which opens at Completed) adds nothing and always passes.
                                            if qso_manager
                                                .manual_open_adds_active_qso(&callsign, tx_off, step)
                                                .await
                                            {
                                                if let Some(snap) = manual_open_at_capacity(
                                                    &qso_manager,
                                                    auto_answer_max_concurrent,
                                                    &fox_mode,
                                                    &fox_max_streams,
                                                    &fox_admitted_qsos,
                                                )
                                                .await
                                                {
                                                    refuse_manual_open_at_capacity(
                                                        &message_bus,
                                                        &display_feed_for_admission,
                                                        ManualOpen::Reply,
                                                        Some(&callsign),
                                                        snap,
                                                        remote_origin,
                                                    )
                                                    .await;
                                                    continue;
                                                }
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
                                                    remote_client_key_id,
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
                                                        ), pancetta_core::DiagnosticLevel::Info)
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
                                                        ), pancetta_core::DiagnosticLevel::Warn)
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
                                        crate::message_bus::QsoMessage::TxFrameNotSent { qso_id, message_text, replaced, attempt_id } => {
                                            autonomous_cq_attempts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                                                .remove(&(qso_id.clone(), attempt_id));
                                            handle_tx_frame_not_sent(&qso_manager, &recent_manual_completions, &qso_id, &message_text, replaced.as_ref(), attempt_id).await;
                                        }
                                        crate::message_bus::QsoMessage::TxFrameScheduled { qso_id, attempt_id, target_slot } => {
                                            match qso_id.parse::<pancetta_qso::QsoId>() {
                                                Ok(id) => {
                                                    let outcome = qso_manager.record_scheduled_tx_slot(id, attempt_id, target_slot).await;
                                                    debug!(target: "qso", "TX frame scheduled for QSO {} (attempt {:?}) into {}: {:?}", qso_id, attempt_id, target_slot, outcome);
                                                }
                                                Err(e) => debug!(target: "qso", "TxFrameScheduled: bad QSO id '{}': {}", qso_id, e),
                                            }
                                        }
                                        crate::message_bus::QsoMessage::TxFrameSent { qso_id, message_text, aired, aired_at, attempt_id } => {
                                            autonomous_cq_attempts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                                                .remove(&(qso_id.clone(), attempt_id));
                                            handle_tx_frame_sent(&qso_manager, &recent_manual_completions, &qso_id, &message_text, &aired, aired_at, attempt_id).await;
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
                                                    ), pancetta_core::DiagnosticLevel::Warn)
                                                .await;
                                            }
                                            // PAN-287: a call parked in the cross-parity queue on
                                            // the old band must not promote and transmit on the
                                            // new band. Drop and report each one.
                                            let dropped = {
                                                let mut queue = pending_manual_calls.lock().await;
                                                drain_parked_calls_on_band_change(&mut queue)
                                            };
                                            for call in dropped {
                                                warn!(
                                                    target: "qso",
                                                    "Band change: dropping queued call to {}", call
                                                );
                                                emit_status(
                                                    &message_bus,
                                                    format!("Queued call to {call} dropped — band changed"),
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                )
                                                .await;
                                                crate::coordinator::tx::emit_diagnostic_full(
                                                    &message_bus,
                                                    ComponentId::Qso,
                                                    "qso",
                                                    pancetta_core::DiagnosticLevel::Warn,
                                                    format!("Dropped queued call to {call} — band changed"),
                                                    None,
                                                    Some(&call),
                                                )
                                                .await;
                                            }
                                        }
                                        // PAN-137: purge displaced clients' queued calls before
                                        // retirement can promote them into fresh stale QSOs.
                                        // Admission and event-driven promotion share this lock.
                                        crate::message_bus::QsoMessage::RemoteControllerChanged {
                                            controller_key_id,
                                        } => {
                                            let _admission = qso_admission.lock().await;
                                            let dropped = {
                                                let mut queue = pending_manual_calls.lock().await;
                                                let before = queue.len();
                                                queue.retain(|p| {
                                                    !pancetta_qso::qso_manager::bound_to_other_remote_client(
                                                        p.remote_origin,
                                                        p.remote_client_key_id.as_deref(),
                                                        &controller_key_id,
                                                    )
                                                });
                                                before - queue.len()
                                            };
                                            let retired = qso_manager
                                                .retire_qsos_bound_to_other_remote_clients(&controller_key_id)
                                                .await;
                                            if !retired.is_empty() || dropped > 0 {
                                                info!(target: "qso",
                                                    "Remote control now held by {}: ended {} QSO(s) and dropped {} queued call(s) of other clients",
                                                    controller_key_id, retired.len(), dropped);
                                            }
                                            // A queue-only change emits no QSO event to refresh
                                            // the TUI and remote display, so push it here.
                                            if dropped > 0 {
                                                push_active_qso_snapshot(
                                                    &qso_manager,
                                                    &dx_activity,
                                                    &pending_manual_calls,
                                                    &display_feed_for_admission,
                                                    &message_bus,
                                                    &queue_clock,
                                                ).await;
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
                                            remote_client_key_id,
                                        } => {
                                            // PAN-78: capacity/parity checks and the open run
                                            // under the admission lock.
                                            let _admission = qso_admission.lock().await;
                                            // PAN-78: capacity gate. A manual CQ always opens a new
                                            // `CallingCq` QSO, a stream the TX cap counts. (Fox engage
                                            // starts its CQ via `start_cq_manual` directly, not here.)
                                            if let Some(snap) = manual_open_at_capacity(
                                                &qso_manager,
                                                auto_answer_max_concurrent,
                                                &fox_mode,
                                                &fox_max_streams,
                                                &fox_admitted_qsos,
                                            )
                                            .await
                                            {
                                                refuse_manual_open_at_capacity(
                                                    &message_bus,
                                                    &display_feed_for_admission,
                                                    ManualOpen::Cq,
                                                    None,
                                                    snap,
                                                    remote_origin,
                                                )
                                                .await;
                                                continue;
                                            }
                                            match qso_manager
                                                .start_cq_manual(
                                                    frequency as f64,
                                                    tx_parity,
                                                    remote_origin,
                                                    remote_client_key_id,
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
                                                        ), pancetta_core::DiagnosticLevel::Info)
                                                    .await;
                                                }
                                                Err(e) => {
                                                    warn!("Failed to start manual CQ: {}", e);
                                                    emit_status(
                                                        &message_bus,
                                                        format!("Start CQ failed: {}", e), pancetta_core::DiagnosticLevel::Warn)
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
                                        //   3. Start a repeating CQ (same path as StartCq / `c`),
                                        //      unless a Fox CQ is still calling or Hounds an
                                        //      earlier engage admitted fill fox_max_streams
                                        //      (PAN-76).
                                        //   4. Raise the caller-answer cap to fox_max_streams
                                        //      (read dynamically in maybe_answer_caller).
                                        // On disengage:
                                        //   1. Clear fox_mode.
                                        //   2. Cancel any active un-answered CallingCq QSO (StopCq path).
                                        //   3. Normal cap restored for *new* admissions; QSOs Fox
                                        //      mode already opened keep their TX slots until they
                                        //      finish (PAN-76, `fox_admitted_qsos`).
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
                                                        ), pancetta_core::DiagnosticLevel::Warn)
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
                                                // manual watchdog until a Hound answers;
                                                // tx_parity = None (Fox picks its own slot via the
                                                // self-parity fallback). PAN-386: in FT8 the CQ
                                                // takes the lowest free 60 Hz Fox slot in
                                                // 300-900 Hz (WSJT-X `foxgen.f90`) and is a
                                                // Fox-role stream; FT4/FT2 keep 1500 Hz (passband
                                                // centre), the legacy geometry.
                                                const LEGACY_FOX_CQ_OFFSET_HZ: f64 = 1500.0;
                                                // PAN-76: a Hound that converts the Fox CQ
                                                // skips maybe_answer_caller's cap. Start no
                                                // CQ while a Fox CQ is still calling, or while
                                                // Hounds an earlier engage admitted still fill
                                                // fox_max_streams, counting completed Hounds
                                                // whose final RR73/73 is still inside its 45 s
                                                // grace. That holds TX-live Fox-admitted QSOs
                                                // to fox_max_streams + 1, as for a single
                                                // engage. fox_mode stays on.
                                                let n = fox_max_streams.load(
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                                let (fox_cq_calling, finishing) =
                                                    fox_admitted_split(
                                                        &qso_manager,
                                                        &fox_admitted_qsos,
                                                    )
                                                    .await;
                                                if fox_cq_calling || finishing >= n {
                                                    info!(
                                                        "Fox mode ON — no new CQ (Fox CQ \
                                                         calling: {}, {} Fox-admitted QSO(s) \
                                                         still finishing, up to {} streams)",
                                                        fox_cq_calling, finishing, n
                                                    );
                                                    let status = if fox_cq_calling {
                                                        "Fox mode ON — Fox CQ already calling, \
                                                         no new CQ"
                                                            .to_string()
                                                    } else {
                                                        format!(
                                                            "Fox mode ON — no new CQ: {} Hound \
                                                             QSO(s) still finishing",
                                                            finishing
                                                        )
                                                    };
                                                    emit_status(&message_bus, status, pancetta_core::DiagnosticLevel::Warn).await;
                                                } else {
                                                    let fox_slots = pancetta_qso::fox_slots_apply(
                                                        &qso_manager.config().active_mode,
                                                    );
                                                    let cq_offset: Result<f64, String> = if fox_slots {
                                                        let (fox_offsets, other_offsets) = qso_manager
                                                            .active_tx_offsets_by_fox_role()
                                                            .await;
                                                        pancetta_qso::fox_slot_offset(
                                                            &fox_offsets,
                                                            &other_offsets,
                                                        )
                                                        .ok_or_else(|| {
                                                            "no free Fox slot in 300-900 Hz".to_string()
                                                        })
                                                    } else {
                                                        Ok(LEGACY_FOX_CQ_OFFSET_HZ)
                                                    };
                                                    let started = match cq_offset {
                                                        Ok(cq_offset) => qso_manager
                                                            .start_cq_manual(cq_offset, None, false, None)
                                                            .await
                                                            .map(|qso_id| (qso_id, cq_offset))
                                                            .map_err(|e| e.to_string()),
                                                        Err(e) => Err(e),
                                                    };
                                                    match started {
                                                        Ok((qso_id, cq_offset)) => {
                                                            // PAN-76: a Hound answering within 15 Hz
                                                            // of the CQ converts this QSO into its
                                                            // exchange, so it counts as Fox-admitted.
                                                            record_fox_admitted(
                                                                &fox_admitted_qsos,
                                                                &qso_id,
                                                            );
                                                            if fox_slots {
                                                                qso_manager.mark_fox_role(qso_id).await;
                                                            }
                                                            info!(
                                                                "Fox mode ON — CQ started: {} \
                                                                 ({:.0} Hz, up to {} streams)",
                                                                qso_id, cq_offset, n
                                                            );
                                                            emit_status(
                                                                &message_bus,
                                                                format!(
                                                                    "Fox mode ON — CQ + up to {} \
                                                                     streams",
                                                                    n
                                                                ), pancetta_core::DiagnosticLevel::Info)
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
                                                                ), pancetta_core::DiagnosticLevel::Warn)
                                                            .await;
                                                        }
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
                                                // Disengage: clear flag first so the admission
                                                // cap and the base TX ceiling drop immediately;
                                                // then cancel CQ.
                                                fox_mode.store(
                                                    false,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );

                                                let active = qso_manager.get_active_qsos().await;
                                                let mut cancelled = 0usize;
                                                // PAN-76: Fox-admitted exchanges left to
                                                // finish on the air (log only).
                                                let mut finishing = 0usize;
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
                                                            // PAN-76: drop it from the Fox
                                                            // set now, not when the event
                                                            // forwarder sees the cancel
                                                            // (it repeats this
                                                            // idempotently). Until then an
                                                            // immediate re-engage would
                                                            // read the cancelled CQ as a
                                                            // Hound in its completed grace
                                                            // and could start no CQ.
                                                            if let Ok(mut set) =
                                                                fox_admitted_qsos.write()
                                                            {
                                                                set.remove(
                                                                    &super::active_tx_qso_key(
                                                                        &id.to_string(),
                                                                    ),
                                                                );
                                                            }
                                                        }
                                                    } else {
                                                        let key = super::active_tx_qso_key(
                                                            &id.to_string(),
                                                        );
                                                        if fox_admitted_qsos
                                                            .read()
                                                            .is_ok_and(|s| s.contains(&key))
                                                        {
                                                            finishing += 1;
                                                        }
                                                    }
                                                }
                                                info!(
                                                    "Fox mode OFF — cancelled {} CQ QSO(s); {} \
                                                     Fox-admitted QSO(s) left to finish",
                                                    cancelled, finishing
                                                );
                                                emit_status(
                                                    &message_bus,
                                                    "Fox mode OFF".to_string(), pancetta_core::DiagnosticLevel::Info)
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
                                        // PAN-38: this component is the SENDER of
                                        // AutonomousCqDispatchFailed (see the
                                        // StartAutonomousQso Err arm above) — it is
                                        // addressed to ComponentId::Autonomous and
                                        // never routed back to this inbound loop.
                                        // Present only for match exhaustiveness.
                                        crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
                                            ..
                                        } => {}
                                        // PAN-38 round 1: same reasoning —
                                        // this component SENDS AutonomousCqOpened
                                        // (see the StartAutonomousQso Ok arm
                                        // above), addressed to
                                        // ComponentId::Autonomous, never routed
                                        // back here. Present only for match
                                        // exhaustiveness.
                                        crate::message_bus::QsoMessage::AutonomousCqOpened {
                                            ..
                                        } => {}
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

/// The decoder's narrow-band hint (`active_qso_freq_hz`) for a QSO, computed
/// from its CURRENT state and metadata.
///
/// PAN-72 (Codex round 7 on PR #350, finding 6). This is the same rule the
/// `StateChanged` arm applies inline — `partner_freq` when the DX's RX offset
/// is known, otherwise the QSO's own state frequency — factored out so the
/// `TxOffsetApplied` arm can apply it too, from the POST-switch QSO.
///
/// Why an offset switch must recompute it: the FT8 decoder's QSO filter
/// (`coordinator::qso_filter`, on by default) collapses the main decode to
/// ±[`DEFAULT_HALF_WINDOW_HZ`](crate::coordinator::qso_filter::DEFAULT_HALF_WINDOW_HZ)
/// (60 Hz) around this value. A relocation is guaranteed to be at least
/// `min_separation_hz` (75 Hz by default) away, so a hint left on the
/// abandoned offset puts the QSO's own traffic OUTSIDE the decode window.
///
/// The two cases differ in which field carries the answer, and only one of
/// them is dangerous:
///
/// - **Established QSO** (`partner_freq` latched, which
///   `apply_tx_offset_switch` does for exactly this reason): the DX still
///   transmits on the offset we vacated, so the hint must stay there. The
///   switch changed only our TX side. Recomputing yields `partner_freq` and
///   the hint effectively does not move — correct, and previously correct by
///   accident, since it also did not move.
/// - **Unanswered `CallingCq`**: no DX exists yet and `partner_freq` stays
///   `None` on purpose, so the state frequency IS the hint — and the switch
///   just changed it. Leaving the hint on the abandoned CQ offset meant an
///   answer to the NEW CQ fell outside the decode search entirely. No decode
///   ⇒ no `StateChanged` ⇒ nothing ever refreshed the hint: the station was
///   left calling CQ on a frequency it could not hear replies on, for the life
///   of the QSO. That is the failure PAN-72's whole mechanism exists to avoid.
///
/// Returns `None` for a QSO that is no longer active — the caller uses that to
/// LEAVE the shared hint alone rather than clearing it, since the terminal
/// paths (`StateChanged`'s inactive branch, `QsoCompleted`, `QsoFailed`)
/// already own that transition and a co-active QSO's hint must not be wiped by
/// a late event for a dead one.
fn decoder_hint_freq_for(progress: &pancetta_qso::QsoProgress) -> Option<f64> {
    if !progress.state.is_active() {
        return None;
    }
    progress
        .metadata
        .partner_freq
        .or_else(|| progress.state.frequency())
}

/// The SECONDARY decoder hint (PAN-72 round-8 redesign, fix 2): the offset a
/// stalled, unanswered `CallingCq` relocated AWAY from, for as long as a
/// reply to the last frame transmitted there is still credible.
///
/// The bug this closes (Codex round 8, finding 1): `QsoMetadata::pre_switch_offset`
/// deliberately keeps the vacated offset "eligible" for CREDITING purposes
/// (`live_pre_switch_offset_at` in `pancetta-qso`), but the decoder's actual
/// band-collapse filter (`coordinator::qso_filter`, ±60 Hz around a SINGLE
/// [`decoder_hint_freq_for`] value) got recentred onto the NEW offset by the
/// round-7 fix, dropping the old offset outside the decode window — a caller
/// answering the old-offset CQ was filtered out and never decoded.
///
/// `Some` only when ALL of:
/// - the QSO is `CallingCq` (an established QSO already has a stable hint
///   via `partner_freq` — the DX transmits there regardless of our TX side,
///   so it needs no second window; touching that path is explicitly out of
///   scope for this fix);
/// - `metadata.pre_switch_offset` is `Some(pre)`;
/// - `pre.left_at` is still within
///   [`pancetta_qso::qso_manager::pre_switch_offset_grace_from_slot_ns`] of
///   `slot_ns` — the coordinator's live `active_slot_ns` atomic, the SAME
///   value `pancetta-qso` derives Fix 1's grace window from (one source of
///   truth, two call sites).
///
/// Every other case (established QSO, no pre-switch offset, or grace
/// expired) yields `None`.
///
/// PAN-72 round-9 fix (Codex round 9, finding 1): returns the pair
/// `(offset_hz, expires_at)` rather than just the frequency. The old
/// single-`f64` return let the shared `active_qso_freq_hz` state carry a
/// secondary hint with NO way for a later reader to tell whether it was
/// still within grace — the expiry check only ever ran HERE, at write
/// time (when a `StateChanged`/`TxOffsetApplied` event happened to fire),
/// so a secondary that expired with no further event kept widening the
/// decoder's filter forever. Carrying `expires_at` through lets
/// `coordinator::qso_filter::compute_narrow_filter_bins_dual_with_expiry`
/// re-derive freshness at every decode-loop READ instead.
fn secondary_decoder_hint_freq_for(
    progress: &pancetta_qso::QsoProgress,
    slot_ns: i64,
) -> Option<(f64, UtcTimestamp)> {
    if !matches!(
        progress.state,
        pancetta_qso::states::QsoState::CallingCq { .. }
    ) {
        return None;
    }
    let pre = progress.metadata.pre_switch_offset?;
    let grace = pancetta_qso::qso_manager::pre_switch_offset_grace_from_slot_ns(slot_ns);
    let expires_at = pre.left_at + grace;
    (expires_at > UtcTimestamp::now()).then_some((pre.offset_hz, expires_at))
}

#[cfg(test)]
#[path = "qso/tests/secondary_decoder_hint_tests.rs"]
mod secondary_decoder_hint_tests;

/// Build a fresh active-QSO snapshot and push it to the TUI (and, when the
/// read-only display feed is enabled, to the remote gateway).
///
/// Extracted from the three copies that already existed inline in the QSO
/// event-forwarding task (`StateChanged`, `QsoCompleted`, `QsoFailed`) when
/// PAN-72 added a fourth caller: `QsoEvent::TxOffsetApplied`. A mid-QSO offset
/// switch changes what the banner and the TX-placement stream marker must
/// show, but it is not a state transition — during the stalled exchange that
/// triggered the switch there may be no further transition at all, so without
/// this push the operator would watch multiple switch/revert cycles while the
/// UI kept displaying the pre-switch offset (Codex round 1 on PR #350,
/// finding 7).
async fn push_active_qso_snapshot(
    qso_manager: &pancetta_qso::QsoManager,
    dx_activity: &DxActivityMap,
    pending_manual_calls: &PendingManualCalls,
    display_feed_enabled: &std::sync::atomic::AtomicBool,
    message_bus: &MessageBus,
    clock: &pancetta_core::SlotClock,
) {
    let (snapshot, pending_snap) =
        build_active_qso_snapshot(qso_manager, dx_activity, pending_manual_calls, clock).await;
    // Additive: clone for the read-only gateway BEFORE the snapshot is moved
    // into the ->Tui send, and only when the gateway is enabled.
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
    if let Err(e) = message_bus.send_message(snap_msg).await {
        debug!("Failed to push active-QSOs snapshot: {}", e);
    }
    if let Some(m) = gw_snap {
        super::remote_gateway::relay_to_gateway(
            message_bus,
            display_feed_enabled,
            ComponentId::Qso,
            m,
        )
        .await;
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
    clock: &pancetta_core::SlotClock,
) -> (
    Vec<crate::message_bus::ActiveQsoSnapshotItem>,
    Vec<crate::message_bus::PendingCallSnapshotItem>,
) {
    let active = qso_manager.get_active_qsos().await;
    let now = UtcTimestamp::now();
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
        // PAN-289: the displayed wait uses the same age rule as expiry.
        let stamp_now = clock.stamp();
        let guard = pending_manual_calls.lock().await;
        guard
            .iter()
            .map(|p| crate::message_bus::PendingCallSnapshotItem {
                callsign: p.callsign.clone(),
                dx_parity: p.dx_parity,
                waited_secs: p.queued_at.age_at(stamp_now).as_secs(),
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
            .map(|t| t + SignedDuration::from_mins(watchdog_minutes as i64));
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
        pending_freq_drift_hz: progress.metadata.pending_freq_drift.map(|(hz, _)| hz),
        pending_freq_drift_since: progress.metadata.pending_freq_drift.map(|(_, t)| t),
    })
}

#[cfg(test)]
#[path = "qso/tests/pending_manual_tests.rs"]
mod pending_manual_tests;

#[cfg(test)]
#[path = "qso/tests/dx_activity_tests.rs"]
mod dx_activity_tests;

#[cfg(test)]
#[path = "qso/tests/caller_answer_tests.rs"]
mod caller_answer_tests;

#[cfg(test)]
#[path = "qso/tests/ft8_message_classification_tests.rs"]
mod ft8_message_classification_tests;

#[cfg(test)]
#[path = "qso/tests/snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "qso/tests/auto_73_tests.rs"]
mod auto_73_tests;

#[cfg(test)]
#[path = "qso/tests/completed_tx_grace_tests.rs"]
mod completed_tx_grace_tests;

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
/// Decoupled from the QSO pipeline and never blocks it. ClubLog, QRZ
/// Logbook, eQSL and cqdx.io uploads are handed to the durable pending-upload
/// queue (`upload_outbox`, PAN-57), which retries them and survives restarts;
/// LoTW uses this same queue. Successes log at
/// `info!`, duplicates at `info!` (non-fatal), failures at `warn!` (target
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
        start_time: metadata.contact_started_at(),
        end_time: metadata.end_time.unwrap_or_else(UtcTimestamp::now),
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
    /// `true` if a missing state was filled from the QRZ lookup (PAN-85).
    state_filled: bool,
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

    // PAN-85: QRZ's <state> is the one live source of state for a station we
    // have just worked. Fill only when missing; never override a value that
    // came from the operator's own log.
    //
    // The same US-related + `normalize_us_state` gate the display applies is
    // enforced HERE, before the value is stored: `their_state` is rendered
    // into the uploaded ADIF `STATE` field (an enumerated field), so a
    // free-text QRZ value for a non-US station -- a Japanese prefecture, a
    // Canadian province -- must never reach a logbook. Only the canonical
    // two-letter code is stored.
    let state_missing = metadata
        .their_state
        .as_ref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    let us_related = metadata
        .their_callsign
        .as_deref()
        .and_then(pancetta_core::dxcc::entity_for_callsign)
        .is_some_and(pancetta_core::dxcc::is_us_related_entity);
    if state_missing && us_related {
        if let Some(code) = lookup
            .state
            .as_deref()
            .and_then(pancetta_core::dxcc::normalize_us_state)
        {
            metadata.their_state = Some(code.to_string());
            result.state_filled = true;
        }
    }

    result
}

/// Does this completed QSO still need a QRZ lookup? Either the their-grid
/// is missing (pre-PAN-85 behaviour), or the station is a US-related DXCC
/// entity whose state we do not know yet and which the DX Hunter would
/// display (PAN-85 / D7). Non-US entities are never looked up for state,
/// so the added QRZ traffic is exactly proportional to the feature.
fn needs_qrz_enrichment(metadata: &pancetta_qso::QsoMetadata) -> bool {
    let grid_missing = metadata
        .grids
        .theirs
        .as_ref()
        .map(|g| g.trim().is_empty())
        .unwrap_or(true);
    if grid_missing {
        return true;
    }
    let state_missing = metadata
        .their_state
        .as_ref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    state_missing
        && metadata
            .their_callsign
            .as_deref()
            .and_then(pancetta_core::dxcc::entity_for_callsign)
            .is_some_and(pancetta_core::dxcc::is_us_related_entity)
}

/// Best-effort QRZ-XML grid/state enrichment for a completed QSO.
///
/// Looks up the contra-callsign via [`QrzXmlClient`](pancetta_dx::QrzXmlClient)
/// only when [`needs_qrz_enrichment`] says something is still missing, caches
/// the result (hit or miss) for the session, and merges it into `metadata`
/// via [`merge_qrz_lookup`]. Never blocks or fails the pipeline: any
/// error/timeout is logged at debug (target `dx.qrz`) and the metadata is
/// left unchanged. Coordinator shutdown cancels it at its next `.await`
/// (PAN-232).
async fn maybe_enrich_from_qrz(
    metadata: &mut pancetta_qso::QsoMetadata,
    client: &pancetta_dx::QrzXmlClient,
    cache: &Mutex<HashMap<String, Option<pancetta_dx::QrzLookup>>>,
) {
    if !needs_qrz_enrichment(metadata) {
        return;
    }

    let raw = match metadata.their_callsign.as_ref() {
        Some(c) if !c.trim().is_empty() => c.trim().to_ascii_uppercase(),
        _ => return,
    };
    // A resolved FT8 hash render ("<W5ABC>") is looked up and cached as the
    // plain call it represents; the unresolved "<...>" placeholder names no
    // station, so it is never sent to QRZ.
    let Some(callsign) = pancetta_core::callsign::resolve_hash_render(&raw) else {
        return;
    };
    let callsign = callsign.to_string();
    let key = callsign.clone();

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
                if merged.state_filled {
                    debug!(
                        target: "dx.qrz",
                        "QRZ (cached): filled state for {} = {:?}",
                        callsign, metadata.their_state
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
            if merged.state_filled {
                debug!(
                    target: "dx.qrz",
                    "QRZ: filled state for {} = {:?}", callsign, metadata.their_state
                );
            }
            cache.lock().await.insert(key, Some(lookup));
        }
        Err(e) => {
            // Safe to log: pancetta-dx network errors never carry the request URL,
            // which holds the QRZ password or session key (see pancetta_dx::NetworkError).
            debug!(target: "dx.qrz", "QRZ lookup failed for {} (skipping): {}", callsign, e);
            cache.lock().await.insert(key, None);
        }
    }
}

/// How often the QSO upload subscriber re-checks the shutdown flag while a
/// QRZ lookup is in flight. The flag is a plain `AtomicBool`; 50 ms is the TX
/// worker's `interruptible_sleep` cadence.
const UPLOAD_SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Resolves once the shutdown flag is set.
async fn shutdown_requested(shutdown: &std::sync::atomic::AtomicBool) {
    while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
        tokio::time::sleep(UPLOAD_SHUTDOWN_POLL).await;
    }
}

/// Resolves when coordinator shutdown asks the upload subscriber to finish.
/// Without a queue nothing asks: the subscriber runs until the runtime ends.
async fn upload_subscriber_finish(outbox: Option<&super::upload_outbox::UploadOutboxHandle>) {
    match outbox {
        Some(outbox) => outbox.stop_requested().await,
        None => std::future::pending().await,
    }
}

// rationale: the queue handle, one explicit config arg per remaining direct
// destination (QRZ XML) plus the event source + shared handles and the
// QRZ XML test endpoint — bundling them into a struct would just move the
// same fields without improving clarity.
#[allow(clippy::too_many_arguments)]
/// PAN-344: the enabled upload targets captured for one completion — the
/// outbox's runtime-enabled destinations, including LoTW,
/// each starting Pending ("queued" means upload work has been scheduled,
/// not accepted). Later config changes never rewrite a contact's captured
/// set; an empty set means "uploads disabled".
fn completion_upload_targets(
    destinations: &[pancetta_dx::upload_outbox::UploadDestination],
) -> Vec<(
    pancetta_tui::upload_status::UploadTarget,
    pancetta_tui::upload_status::TargetUploadStatus,
)> {
    use pancetta_tui::upload_status::{sort_targets, TargetUploadStatus};
    let mut targets: Vec<_> = destinations
        .iter()
        .map(|d| {
            (
                super::qso_upload_status::upload_target_for_destination(*d),
                TargetUploadStatus::pending("queued"),
            )
        })
        .collect();
    sort_targets(&mut targets);
    targets
}

#[allow(clippy::too_many_arguments)] // Keep upload configuration, event stream and shared lifecycle state explicit at subscriber startup.
fn start_qso_upload_subscriber(
    outbox: Option<super::upload_outbox::UploadOutboxHandle>,
    qrz_xml_cfg: pancetta_config::network::QrzXmlConfig,
    station_power_watts: u32,
    attribution_comment: bool,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    station_lookup: std::sync::Arc<crate::priority_evaluator::CachedStationLookup>,
    qrz_xml_base_url: Option<String>,
    upload_statuses: super::qso_upload_status::SharedUploadStatusRegistry,
) {
    use std::sync::Arc;

    // ClubLog, QRZ Logbook, eQSL and cqdx.io clients live in the
    // pending-upload worker (`upload_outbox::build_logbook_clients`).

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
        let client = pancetta_dx::QrzXmlClient::new(
            qrz_xml_cfg.username.clone(),
            qrz_xml_cfg.password.clone(),
            agent,
        );
        // The override is used only by local tests in normal operation.
        #[cfg(test)]
        let client = match qrz_xml_base_url {
            Some(url) => client.with_mock_base_url(url),
            None => Ok(client),
        };
        #[cfg(not(test))]
        let client = match qrz_xml_base_url {
            Some(url) => client.with_base_url(url),
            None => Ok(client),
        };
        match client {
            Ok(client) => Some(Arc::new(client)),
            Err(err) => {
                tracing::warn!(target: "dx.qrz", "{err}");
                None
            }
        }
    } else {
        None
    };
    // Session-scoped lookup cache (uppercased callsign → result). Avoids
    // re-querying QRZ for the same station repeatedly in one session; the
    // `None` value caches a miss/failure too, so a station QRZ has no data for
    // is not retried every QSO. Only allocated when the client is built.
    let qrz_xml_cache: Arc<Mutex<HashMap<String, Option<pancetta_dx::QrzLookup>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    if qrz_xml_client.is_some() {
        info!(
            target: "dx.qrz",
            "QRZ XML enrichment enabled (fills missing grid, and state for US-related \
             entities, before upload — PAN-85)"
        );
    }

    tokio::spawn(async move {
        let processor = pancetta_qso::AdifProcessor::new()
            .with_station_power_watts(station_power_watts)
            .with_attribution_comment(attribution_comment);

        // PAN-232: with a queue, the subscriber runs until coordinator
        // shutdown asks it to finish (`stop_upload_outbox`, after every QSO
        // component has stopped), not just until the shutdown flag: the QSO
        // component can still complete a QSO after the flag flips. It then
        // queues the completions already in its channel and drops its handle,
        // which lets the worker close its queue at once.
        let mut draining = false;
        loop {
            let event = if draining {
                match events.try_recv() {
                    Ok(event) => Ok(event),
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n))
                    }
                    // Empty or Closed: nothing left to queue.
                    Err(_) => break,
                }
            } else {
                tokio::select! {
                    biased;
                    event = events.recv() => event,
                    () = upload_subscriber_finish(outbox.as_ref()) => {
                        draining = true;
                        continue;
                    }
                }
            };
            match event {
                Ok(pancetta_qso::QsoEvent::QsoCompleted { mut metadata, .. }) => {
                    // PAN-344: register the COMPLETE enabled target set at
                    // completion processing, BEFORE QRZ enrichment or ADIF
                    // rendering, so later config changes never rewrite this
                    // contact's denominator and local preparation failures
                    // have targets to mark. All captured targets start
                    // Pending ("queued" = scheduled, not accepted).
                    {
                        let their_for_registry = metadata
                            .their_callsign
                            .clone()
                            .unwrap_or_else(|| "?".to_string());
                        let destinations: &[pancetta_dx::upload_outbox::UploadDestination] =
                            outbox.as_ref().map(|o| o.destinations()).unwrap_or(&[]);
                        let targets = completion_upload_targets(destinations);
                        // Registered even when EMPTY: a confirmed empty
                        // target set is "uploads disabled", never unknown.
                        upload_statuses
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .register(&metadata.qso_id.to_string(), &their_for_registry, targets);
                    }
                    // Best-effort QRZ XML enrichment: fill a MISSING their-grid
                    // (and name/dxcc in notes for logging) before rendering the
                    // ADIF record. No-op when the client is disabled or the grid
                    // is already known from decode/cqdx; never blocks or fails
                    // the upload pipeline. PAN-232: never hold a completed QSO's
                    // uploads for it once shutdown has started. A lookup can
                    // take its 30 s client timeout (twice with a login), far
                    // past the worker's shutdown window, so the shutdown flag
                    // cancels it. Biased toward the lookup, so a cached answer
                    // still lands at shutdown.
                    if let Some(client) = qrz_xml_client.clone() {
                        let call = metadata
                            .their_callsign
                            .clone()
                            .unwrap_or_else(|| "?".to_string());
                        tokio::select! {
                            biased;
                            () = maybe_enrich_from_qrz(&mut metadata, &client, &qrz_xml_cache) => {}
                            () = shutdown_requested(&shutdown) => info!(
                                target: "qso.upload",
                                "Shutdown: QRZ lookup for {call} cancelled — its uploads are queued without QRZ data"
                            ),
                        }
                    }

                    // PAN-85: session-scoped only — the ADIF writer and the
                    // SQLite logger already consumed their own copies of this
                    // event, so this value is not persisted (see D6).
                    station_lookup.record_completed_state(&metadata);

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
                            // PAN-344: a local preparation failure marks every
                            // intended target Failed — the contact keeps its
                            // denominator and cannot become all-accepted.
                            upload_statuses
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .record_all(
                                    &metadata.qso_id.to_string(),
                                    pancetta_tui::upload_status::UploadState::Failed,
                                    "upload skipped: ADIF render failed",
                                );
                            continue;
                        }
                    };

                    let their = metadata
                        .their_callsign
                        .clone()
                        .unwrap_or_else(|| "?".to_string());

                    // PAN-57: ClubLog, QRZ Logbook, eQSL and cqdx.io go
                    // through the durable pending-upload queue. ClubLog/QRZ/
                    // eQSL take the rendered ADIF record (eQSL's client adds
                    // its credential header at send time, so the queue never
                    // stores a secret). cqdx.io takes the structured QsoRecord
                    // its `POST /api/v1/qsos` endpoint expects, built once
                    // here so every retry carries the same startTime/endTime
                    // and hits the same idempotency key; it is skipped when
                    // the contra-callsign is unknown. Frequency is the
                    // dial+offset RF value already stamped on the metadata.
                    if let Some(outbox) = &outbox {
                        let cqdx_enabled = outbox
                            .destinations()
                            .contains(&pancetta_dx::upload_outbox::UploadDestination::Cqdx);
                        let cqdx_payload = if cqdx_enabled {
                            let payload = cqdx_record_from_metadata(&metadata)
                                .and_then(|qso| serde_json::to_string(&qso).ok());
                            // PAN-344: an enabled cqdx.io target with no
                            // constructible payload is a local failure for
                            // that target, never an acceptance.
                            if payload.is_none() {
                                upload_statuses
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .record(
                                        &metadata.qso_id.to_string(),
                                        pancetta_tui::upload_status::UploadTarget::Cqdx,
                                        pancetta_tui::upload_status::UploadState::Failed,
                                        "cqdx record unavailable",
                                    );
                            }
                            payload
                        } else {
                            None
                        };
                        let queued = outbox
                            .enqueue(super::upload_outbox::outbox_batch(
                                metadata.qso_id,
                                &their,
                                &record,
                                cqdx_payload,
                                outbox.destinations(),
                            ))
                            .await;
                        // PAN-344: a closed worker channel is a local refusal
                        // for every targeted destination — never silence.
                        if !queued {
                            let mut registry = upload_statuses
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            for destination in outbox.destinations() {
                                registry.record(
                                    &metadata.qso_id.to_string(),
                                    super::qso_upload_status::upload_target_for_destination(
                                        *destination,
                                    ),
                                    pancetta_tui::upload_status::UploadState::Failed,
                                    "upload queue is closed",
                                );
                            }
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
// Startup settings and subscriber handles are passed together.
#[allow(clippy::too_many_arguments)]
async fn start_local_qso_log_writers(
    replay: bool,
    adif_path: &std::path::Path,
    db_path: &std::path::Path,
    station_power_watts: u32,
    attribution_comment: bool,
    database_settings: &pancetta_config::DatabaseConfig,
    qso_manager: &pancetta_qso::QsoManager,
    adif_subscribers: &AdifSubscribers,
) -> (
    Option<std::sync::Arc<pancetta_qso::AdifLogWriter>>,
    Option<std::sync::Arc<pancetta_qso::async_logger::QsoLogger>>,
    LocalIndex,
) {
    start_local_qso_log_writers_with(
        None,
        replay,
        adif_path,
        db_path,
        station_power_watts,
        attribution_comment,
        database_settings,
        qso_manager,
        adif_subscribers,
    )
    .await
}

/// [`start_local_qso_log_writers`] with the ADIF writer's receiver already
/// taken (`Some`), so it holds every completion since the caller subscribed;
/// `None` subscribes here, before the index work.
// Startup settings and subscriber handles are passed together.
#[allow(clippy::too_many_arguments)]
async fn start_local_qso_log_writers_with(
    adif_events: Option<tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>>,
    replay: bool,
    adif_path: &std::path::Path,
    db_path: &std::path::Path,
    station_power_watts: u32,
    attribution_comment: bool,
    database_settings: &pancetta_config::DatabaseConfig,
    qso_manager: &pancetta_qso::QsoManager,
    adif_subscribers: &AdifSubscribers,
) -> (
    Option<std::sync::Arc<pancetta_qso::AdifLogWriter>>,
    Option<std::sync::Arc<pancetta_qso::async_logger::QsoLogger>>,
    LocalIndex,
) {
    if replay {
        info!(
            "--replay: local QSO log writes suppressed — neither {} nor {} is \
             opened, so a QSO 'completed' off replayed traffic leaves no record",
            adif_path.display(),
            db_path.display(),
        );
        return (
            None,
            None,
            LocalIndex::Unavailable {
                reason: "--replay: the local index is not opened".into(),
            },
        );
    }

    // Subscribe before the index work below: a QSO that completes while a
    // large log replays waits in this receiver and is appended once the
    // writer opens, instead of missing the logbook of record.
    let adif_events = adif_events.unwrap_or_else(|| qso_manager.subscribe());

    // Before either writer below creates a file: opening the writers first
    // made a legacy-only `qso.db` look stale (wiped by the rebuild) and a
    // missing index look current (never rebuilt) — PAN-127.
    let seed_db = prepare_local_qso_index(adif_path, db_path).await;

    if !matches!(&seed_db, LocalIndex::MigrationRefused { .. }) {
        if let Err(e) = super::adif_backup::startup(adif_path, database_settings).await {
            warn!(
                "Startup ADIF backup failed at {}: {}; continuing",
                adif_path.display(),
                e
            );
        }
    }

    // ADIF source-of-truth writer. Subscribes to QsoEvent::QsoCompleted and
    // appends one ADIF record per completed QSO. Fail-soft: if open fails, we
    // log but proceed with DB-only — every operator should at least get
    // duplicate detection from the DB.
    let adif_writer = if matches!(&seed_db, LocalIndex::MigrationRefused { .. }) {
        warn!("ADIF log not opened: the legacy index was not migrated; QSOs this session are logged to {} only", db_path.display());
        drop(adif_events);
        None
    } else {
        match pancetta_qso::AdifLogWriter::open(adif_path).await {
            Ok(mut w) => {
                info!("ADIF log open at {}", adif_path.display());
                w.set_station_power_watts(station_power_watts);
                w.set_attribution_comment(attribution_comment);
                let w = std::sync::Arc::new(w);
                adif_subscribers.track(start_adif_subscriber(
                    w.clone(),
                    adif_events,
                    adif_subscribers,
                ));
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
        }
    };

    // Async QSO logger — subscribes independently to QsoEvent::QsoCompleted
    // and inserts into the rebuildable SQLite index.
    let mut logger_config = pancetta_qso::LoggerConfig {
        database_path: db_path.to_path_buf(),
        persist_qso_timeline: database_settings.persist_qso_timeline,
        ..Default::default()
    };
    // `~/.pancetta/backups`, never relative to the process cwd.
    if let Some(dir) = db_path.parent() {
        logger_config.backup.backup_directory = dir.join("backups");
    }

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

    (adif_writer, async_logger, seed_db)
}

/// The history seed and whether a refused migration must hold back ADIF creation.
#[doc(hidden)]
pub enum LocalIndex {
    Ready(pancetta_qso::async_database::QsoDatabase),
    Unavailable { reason: String },
    MigrationRefused { reason: String },
}
impl LocalIndex {
    #[cfg(test)]
    fn expect(self, message: &str) -> pancetta_qso::async_database::QsoDatabase {
        match self {
            Self::Ready(db) => db,
            _ => panic!("{message}"),
        }
    }
    #[cfg(test)]
    fn is_none(&self) -> bool {
        !matches!(self, Self::Ready(_))
    }
}

/// Reconcile contacts before either writer opens a file. An unchanged ADIF
/// needs no rebuild regardless of timestamps. A migration refusal must never
/// create a header-only ADIF that would erase legacy contacts on the next start.
#[doc(hidden)]
pub async fn prepare_local_qso_index(
    adif_path: &std::path::Path,
    db_path: &std::path::Path,
) -> LocalIndex {
    use pancetta_qso::async_database::{AdifSnapshot, PreviousIndex, QsoDatabase};
    let unavailable = |reason: String| LocalIndex::Unavailable { reason };
    let adif_exists = match adif_path.try_exists() {
        Ok(v) => v,
        Err(e) => return unavailable(format!("cannot inspect ADIF: {e}")),
    };
    let db_exists = match db_path.try_exists() {
        Ok(v) => v,
        Err(e) => return unavailable(format!("cannot inspect index: {e}")),
    };
    let migrated = if !adif_exists && db_exists {
        match QsoDatabase::migrate_legacy_to_adif(db_path, adif_path).await {
            Ok((db, m)) => {
                info!("DB→ADIF migration succeeded: exported {} QSOs to {}; legacy index copied to {}", m.exported, adif_path.display(),m.backup.display());
                debug!(
                    backup_existed = m.backup_existed,
                    "Legacy migration backup retained"
                );
                Some(db)
            }
            Err(e) => {
                return LocalIndex::MigrationRefused {
                    reason: format!(
                        "{e}. QSOs this session are logged to {} only, not {}.",
                        db_path.display(),
                        adif_path.display()
                    ),
                }
            }
        }
    } else {
        None
    };
    if !adif_exists && !db_exists {
        return match QsoDatabase::open(db_path).await {
            Ok(db) => LocalIndex::Ready(db),
            Err(e) => unavailable(e.to_string()),
        };
    }
    let marker = QsoDatabase::rebuild_marker(db_path);
    let marked = match marker.try_exists() {
        Ok(v) => v,
        Err(e) => return unavailable(format!("cannot inspect rebuild marker: {e}")),
    };
    let snapshot = match AdifSnapshot::load(adif_path).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tokio::fs::write(&marker, b"").await;
            return unavailable(format!(
                "cannot read {} to check the index: {e}",
                adif_path.display()
            ));
        }
    };
    let mut previous = PreviousIndex::Replace;
    let mut missing_from_adif = None;
    if db_exists {
        let opened = match migrated {
            Some(db) => Ok(db),
            None => QsoDatabase::open_for_reconciliation(db_path).await,
        };
        match opened {
            Ok(db) => {
                match db.drift_from(&snapshot).await {
                    Ok(drift) if drift.is_current() && !marked => return LocalIndex::Ready(db),
                    Ok(drift) => {
                        if drift.missing_from_adif > 0 {
                            previous = PreviousIndex::KeepCopy;
                            missing_from_adif = Some(drift.missing_from_adif);
                        }
                        if marked {
                            info!("Index at {} is missing contacts an interrupted import added to {} — replaying",db_path.display(),adif_path.display());
                        } else {
                            info!("Index at {} differs from ADIF at {}: {} contacts missing from the index, {} not in the ADIF; rebuilding",db_path.display(),adif_path.display(),drift.missing_from_index,drift.missing_from_adif);
                        }
                    }
                    Err(e) => {
                        previous = PreviousIndex::KeepCopy;
                        info!(
                            "Index at {} cannot be read ({e}); rebuilding from ADIF",
                            db_path.display()
                        );
                    }
                }
                db.close().await;
            }
            Err(e) => {
                previous = PreviousIndex::KeepCopy;
                info!(
                    "Index at {} cannot be read ({e}); rebuilding from ADIF",
                    db_path.display()
                );
            }
        }
    } else {
        info!(
            "Index missing at {} — replaying from ADIF",
            db_path.display()
        );
    }
    match QsoDatabase::rebuild_from(db_path, snapshot, previous).await {
        Ok(rebuilt) => {
            if let Some(kept) = rebuilt.kept {
                if let Some(count) = missing_from_adif {
                    warn!(
                        "Kept the previous index as {}: it held {} contacts that {} does not",
                        kept.display(),
                        count,
                        adif_path.display()
                    );
                } else {
                    warn!("Kept the unreadable index as {}", kept.display());
                }
            }
            let _ = tokio::fs::remove_file(&marker).await;
            info!(
                "QSO index ready: {} records ({} skipped)",
                rebuilt.inserted, rebuilt.skipped
            );
            LocalIndex::Ready(rebuilt.db)
        }
        Err(e) => {
            let _ = tokio::fs::write(&marker, b"").await;
            unavailable(format!("ADIF replay failed: {e}"))
        }
    }
}

/// PAN-274: how long coordinator shutdown waits for the ADIF subscribers to
/// write what they still hold. A drain normally takes milliseconds; the bound
/// matters only while another process holds `qsos.adi.lock`, where one append
/// can wait up to `LIVE_LOCK_WAIT` (60 s). It runs alongside the upload
/// worker's own wait, so it does not lengthen shutdown.
pub(crate) const ADIF_SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// PAN-274: coordinator shutdown's hold on every ADIF subscriber this process
/// started. Each Qso start adds one; after a supervisor restart the old one can
/// still be running. Shutdown asks them all to finish once every QSO component
/// has stopped, then waits for them.
#[derive(Clone)]
pub(crate) struct AdifSubscribers {
    /// Set once, by [`AdifSubscribers::stop`].
    stop: Arc<tokio::sync::watch::Sender<bool>>,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl AdifSubscribers {
    pub(crate) fn new() -> Self {
        let (stop, _) = tokio::sync::watch::channel(false);
        Self {
            stop: Arc::new(stop),
            tasks: Arc::default(),
        }
    }

    /// Keep `task` for shutdown to wait on, forgetting subscribers that ended.
    fn track(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }

    /// How many subscribers are tracked (test accessor).
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Ask every subscriber to write what is left in its channel and exit, then
    /// wait for them, at most `limit` in total. The shutdown flag must already
    /// be set and every QSO component stopped.
    pub(crate) async fn stop(&self, limit: Duration) {
        self.stop.send_replace(true);
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = tokio::time::Instant::now() + limit;
        for task in tasks {
            match tokio::time::timeout_at(deadline, task).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("ADIF subscriber ended abnormally: {e}"),
                Err(_) => {
                    error!(
                        "qsos.adi writer still busy after {limit:?}; shutdown continues, \
                         and a QSO it had not written yet is not in qsos.adi"
                    );
                    break;
                }
            }
        }
    }
}

/// Resolves once [`AdifSubscribers::stop`] has been called.
async fn adif_subscriber_finish(stop: &tokio::sync::watch::Sender<bool>) {
    let mut stop = stop.subscribe();
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Append every `QsoCompleted` to `qsos.adi`. Runs until coordinator shutdown
/// asks it to finish (`AdifSubscribers::stop`, after every QSO component has
/// stopped), then writes every completion still in its channel; or until the
/// channel closes. Never stops on the shutdown flag: the Qso task can complete
/// a QSO after the flag flips (PAN-274). A broadcast lag is reported at ERROR
/// and never ends the loop: the completions still in the channel are written.
fn start_adif_subscriber(
    writer: std::sync::Arc<pancetta_qso::AdifLogWriter>,
    mut events: tokio::sync::broadcast::Receiver<pancetta_qso::QsoEvent>,
    subscribers: &AdifSubscribers,
) -> tokio::task::JoinHandle<()> {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    let stop = subscribers.stop.clone();
    tokio::spawn(async move {
        let mut draining = false;
        loop {
            let event = if draining {
                match events.try_recv() {
                    Ok(event) => Ok(event),
                    Err(TryRecvError::Lagged(n)) => Err(RecvError::Lagged(n)),
                    // Empty or Closed: every completion in the channel is written.
                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                }
            } else {
                tokio::select! {
                    biased;
                    event = events.recv() => event,
                    () = adif_subscriber_finish(&stop) => {
                        draining = true;
                        continue;
                    }
                }
            };
            match event {
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
                Err(RecvError::Lagged(n)) => error!(
                    "ADIF subscriber lagged by {n} QSO events; any QSO completed \
                     among them is not in qsos.adi"
                ),
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
#[path = "qso/tests/adif_subscriber_tests.rs"]
mod adif_subscriber_tests;

#[cfg(test)]
#[path = "qso/tests/cqdx_upload_tests.rs"]
mod cqdx_upload_tests;

#[cfg(test)]
#[path = "qso/tests/qrz_enrichment_tests.rs"]
mod qrz_enrichment_tests;

#[cfg(test)]
#[path = "qso/tests/caller_dedup_tests.rs"]
mod caller_dedup_tests;

#[cfg(test)]
#[path = "qso/tests/replay_local_log_tests.rs"]
mod replay_local_log_tests;

/// PAN-41 round 4: coverage for the actual replay-history-seed branch in
/// `start_qso_component` (introduced round 2, revised rounds 3-4) —
/// `replay_local_log_tests` above only proves the LOCAL WRITERS stay off
/// under `--replay`; these tests prove the READ side actually seeds
/// duplicate-detection state, from an isolated directory rather than the
/// operator's real `~/.pancetta`.
#[cfg(test)]
#[path = "qso/tests/replay_history_seed_tests.rs"]
mod replay_history_seed_tests;

#[cfg(test)]
#[path = "qso/tests/respond_to_caller_admission_tests.rs"]
mod respond_to_caller_admission_tests;

#[cfg(test)]
#[path = "qso/tests/fox_admitted_tracking_tests.rs"]
mod fox_admitted_tracking_tests;

#[cfg(test)]
#[path = "qso/tests/manual_capacity_admission_tests.rs"]
mod manual_capacity_admission_tests;

#[cfg(test)]
#[path = "qso/tests/remote_controller_change_tests.rs"]
mod remote_controller_change_tests;

// ------------------------------------------------------------------
// PAN-69: wiring pins for the DecodedMessage handler's stale-band
// gate. These exercise the shared predicate with the exact fixture
// shape the handler receives — a directed decode whose captured dial
// no longer matches the live band must be dropped before
// maybe_answer_caller can create a QSO on the new band.
// ------------------------------------------------------------------
#[cfg(test)]
#[path = "qso/tests/stale_band_tests.rs"]
mod stale_band_tests;

#[cfg(test)]
#[path = "qso/tests/new_caller_policy_tests.rs"]
mod new_caller_policy_tests;

/// Attach history only after its dated all-band read succeeds. Keep the manager's broadcaster.
async fn initialize_qso_history(
    manager: &mut pancetta_qso::QsoManager,
    lookup: &crate::priority_evaluator::CachedStationLookup,
    db: Arc<pancetta_qso::async_database::QsoDatabase>,
) -> Result<(), pancetta_qso::async_database::AsyncDatabaseError> {
    lookup.seed_worked_history(&db).await?;
    lookup.seed_display_history(&db).await;
    manager.attach_database(db);
    Ok(())
}

/// Replay's prepared history seed: the canonical ADIF replayed into an
/// in-memory index, an empty in-memory index when ADIF is absent, or the
/// failure that must keep every replay consumer (QSO admission and the
/// contest worker alike) off the operator's real files. Live startup never
/// calls this (PAN-268).
pub(super) async fn replay_history_db(
    adif_path: &std::path::Path,
) -> Result<
    Arc<pancetta_qso::async_database::QsoDatabase>,
    pancetta_qso::async_database::AsyncDatabaseError,
> {
    match tokio::fs::try_exists(adif_path).await {
        Ok(false) => pancetta_qso::async_database::QsoDatabase::new_in_memory()
            .await
            .map(Arc::new),
        Ok(true) => {
            pancetta_qso::async_database::QsoDatabase::replay_from_adif(":memory:", adif_path)
                .await
                .map(Arc::new)
        }
        Err(source) => Err(pancetta_qso::async_database::AsyncDatabaseError::Io {
            path: adif_path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
#[path = "qso/tests/restart_history_recall_tests.rs"]
mod restart_history_recall_tests;

#[cfg(test)]
#[path = "qso/tests/replay_startup_seed_tests.rs"]
mod replay_startup_seed_tests;

#[cfg(test)]
#[path = "qso/tests/dxpedition_receive_integration_tests.rs"]
mod dxpedition_receive_integration_tests;

#[cfg(test)]
#[path = "qso/tests/live_contest_completion_tests.rs"]
mod live_contest_completion_tests;

#[cfg(test)]
#[path = "qso/tests/completed_tx_purge_tests.rs"]
mod completed_tx_purge_tests;

/// Integration adapter over the same outcome handlers used by the QSO task.
#[cfg(feature = "test-harness")]
#[derive(Default)]
pub(super) struct HarnessOutcomes {
    completions: RecentManualCompletions,
}

#[cfg(feature = "test-harness")]
impl HarnessOutcomes {
    pub(super) async fn apply(
        &self,
        manager: &pancetta_qso::QsoManager,
        outcome: &crate::message_bus::QsoMessage,
    ) {
        use crate::message_bus::QsoMessage;
        match outcome {
            QsoMessage::TxFrameNotSent {
                qso_id,
                message_text,
                replaced,
                attempt_id,
            } => {
                handle_tx_frame_not_sent(
                    manager,
                    &self.completions,
                    qso_id,
                    message_text,
                    replaced.as_ref(),
                    *attempt_id,
                )
                .await
            }
            QsoMessage::TxFrameSent {
                qso_id,
                message_text,
                aired,
                aired_at,
                attempt_id,
            } => {
                handle_tx_frame_sent(
                    manager,
                    &self.completions,
                    qso_id,
                    message_text,
                    aired,
                    *aired_at,
                    *attempt_id,
                )
                .await
            }
            QsoMessage::TxFrameScheduled {
                qso_id,
                attempt_id,
                target_slot,
            } => {
                if let Ok(id) = qso_id.parse() {
                    manager
                        .record_scheduled_tx_slot(id, *attempt_id, *target_slot)
                        .await;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "qso/tests/n1mm_udp_wiring_tests.rs"]
mod n1mm_udp_wiring_tests;
